//! Reading a streamed turn: contract NDJSON events off a Gwead stream
//! handle, one at a time, cancellable, bounded.

use gwead::kernel::streams::{
    STREAM_EOF, SharedStreamRegistry, StreamId, lock_shared, read_async_shared,
};
use gwead::serde_json::Value;
use gwead::tokio_util::sync::CancellationToken;

/// Why [`EventReader::next`] could not produce an event.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ReadError {
    /// The turn was cancelled while waiting for bytes.
    #[error("cancelled")]
    Cancelled,
    /// One line grew past the cap without ending. Events can be
    /// arbitrarily long by contract, so the cap is the consumer's
    /// bound on buffering, not a contract limit.
    #[error("a stream event exceeded the {cap}-byte cap without ending")]
    TooLong { cap: usize },
    /// A line was not a JSON document.
    #[error("a stream line is not JSON: {0}")]
    NotJson(String),
    /// The read failed with a stream error code other than end-of-stream.
    #[error("stream read failed with code {0}")]
    Io(i32),
}

/// One-event-at-a-time reader over a readable handle in `streams`.
///
/// Owns the consumer's end for the duration: dropping the reader closes
/// the handle, which is how the producer learns the reader is gone —
/// the relay's benign-hangup wind-down — whichever way the read loop
/// exits.
pub(crate) struct EventReader {
    streams: SharedStreamRegistry,
    id: StreamId,
    buf: Vec<u8>,
    /// Bytes before this offset have been returned as lines.
    consumed: usize,
    /// Bytes before this offset hold no newline: the scan for the next
    /// line resumes here, so a long event costs one pass, not one per
    /// chunk it spans.
    scanned: usize,
    cap: usize,
    eof: bool,
}

/// Bytes pulled per read. A line longer than this simply takes several
/// reads; the cap is checked between them, so the buffer can overshoot
/// the cap by at most one chunk.
const CHUNK: usize = 8192;

impl EventReader {
    pub(crate) fn new(streams: SharedStreamRegistry, id: StreamId, cap: usize) -> Self {
        Self {
            streams,
            id,
            buf: Vec::new(),
            consumed: 0,
            scanned: 0,
            cap,
            eof: false,
        }
    }

    /// The next event, `None` at end-of-stream. An incomplete final line
    /// — bytes after the last newline when the stream ends — is not an
    /// event: the contract frames every event as a whole line, so a
    /// torn one is the failed-turn shape, and the caller's
    /// no-`end`-event rule reports it.
    pub(crate) async fn next(
        &mut self,
        cancel: &CancellationToken,
    ) -> Result<Option<Value>, ReadError> {
        loop {
            let from = self.scanned.max(self.consumed);
            if let Some(offset) = self.buf[from..].iter().position(|b| *b == b'\n') {
                let newline = from + offset;
                let line = &self.buf[self.consumed..newline];
                let parsed = gwead::serde_json::from_slice::<Value>(line);
                self.consumed = newline + 1;
                self.scanned = self.consumed;
                if self.consumed * 2 >= self.buf.len() {
                    self.buf.drain(..self.consumed);
                    self.consumed = 0;
                    self.scanned = 0;
                }
                return parsed
                    .map(Some)
                    .map_err(|e| ReadError::NotJson(e.to_string()));
            }
            self.scanned = self.buf.len();
            if self.eof {
                return Ok(None);
            }
            if self.buf.len() - self.consumed > self.cap {
                return Err(ReadError::TooLong { cap: self.cap });
            }
            let mut chunk = [0u8; CHUNK];
            let n = tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(ReadError::Cancelled),
                n = read_async_shared(&self.streams, self.id, &mut chunk) => n,
            };
            match n {
                n if n > 0 => self.buf.extend_from_slice(&chunk[..n as usize]),
                STREAM_EOF => self.eof = true,
                code => return Err(ReadError::Io(code)),
            }
        }
    }
}

impl Drop for EventReader {
    fn drop(&mut self) {
        lock_shared(&self.streams).close(self.id);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use gwead::bytes::Bytes;
    use gwead::kernel::streams::{STREAM_CLOSED, StreamRegistry};
    use gwead::serde_json::json;

    use super::*;

    /// A readable handle yielding `chunks` in order.
    fn readable(chunks: Vec<&'static str>) -> (SharedStreamRegistry, StreamId) {
        let source = Box::pin(gwead::futures::stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok(Bytes::from_static(c.as_bytes()))),
        ));
        let mut registry = StreamRegistry::new();
        let id = registry.register_readable("application/x-ndjson", source);
        (Arc::new(Mutex::new(registry)), id)
    }

    #[tokio::test]
    async fn events_are_reassembled_across_chunk_boundaries() {
        // Splits inside a line, a chunk holding two lines, and a line
        // spanning three chunks.
        let (streams, id) = readable(vec![
            "{\"type\":\"te",
            "xt\",\"text\":\"a\"}\n{\"type\":\"text\",\"text\":\"b\"}\n{\"ty",
            "pe\":\"e",
            "nd\"}\n",
        ]);
        let cancel = CancellationToken::new();
        let mut reader = EventReader::new(streams, id, 1 << 20);
        assert_eq!(
            reader.next(&cancel).await.unwrap(),
            Some(json!({"type": "text", "text": "a"}))
        );
        assert_eq!(
            reader.next(&cancel).await.unwrap(),
            Some(json!({"type": "text", "text": "b"}))
        );
        assert_eq!(
            reader.next(&cancel).await.unwrap(),
            Some(json!({"type": "end"}))
        );
        assert_eq!(reader.next(&cancel).await.unwrap(), None);
        assert_eq!(reader.next(&cancel).await.unwrap(), None, "EOF sticks");
    }

    #[tokio::test]
    async fn a_torn_final_line_is_not_an_event() {
        let (streams, id) = readable(vec![
            "{\"type\":\"text\",\"text\":\"a\"}\n{\"type\":\"end\"",
        ]);
        let cancel = CancellationToken::new();
        let mut reader = EventReader::new(streams, id, 1 << 20);
        assert!(reader.next(&cancel).await.unwrap().is_some());
        assert_eq!(reader.next(&cancel).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_line_past_the_cap_and_a_non_json_line_are_refused() {
        // The cap is checked between reads: a line still open after a
        // chunk that put the buffer past the cap is refused before the
        // next read, whatever it would have turned out to be.
        let (streams, id) = readable(vec!["0123456789", "abcdefghij\n"]);
        let cancel = CancellationToken::new();
        let mut reader = EventReader::new(streams, id, 8);
        assert_eq!(
            reader.next(&cancel).await.unwrap_err(),
            ReadError::TooLong { cap: 8 }
        );

        let (streams, id) = readable(vec!["not json\n"]);
        let mut reader = EventReader::new(streams, id, 1 << 20);
        assert!(matches!(
            reader.next(&cancel).await.unwrap_err(),
            ReadError::NotJson(_)
        ));
    }

    #[tokio::test]
    async fn cancellation_wins_and_dropping_the_reader_closes_the_handle() {
        // A source that never yields: the read parks, and only the
        // token can end it.
        let source = Box::pin(gwead::futures::stream::pending::<
            Result<Bytes, std::io::Error>,
        >());
        let mut registry = StreamRegistry::new();
        let id = registry.register_readable("application/x-ndjson", source);
        let streams = Arc::new(Mutex::new(registry));
        let cancel = CancellationToken::new();
        let mut reader = EventReader::new(streams.clone(), id, 1 << 20);
        let waiting = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                let outcome = reader.next(&cancel).await;
                drop(reader);
                outcome
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cancel.cancel();
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
            .await
            .expect("cancellation ends the read")
            .unwrap();
        assert_eq!(outcome.unwrap_err(), ReadError::Cancelled);
        // The handle is closed behind the dropped reader: a later read
        // reports closed, not a parked wait.
        let mut buf = [0u8; 8];
        assert_eq!(
            read_async_shared(&streams, id, &mut buf).await,
            STREAM_CLOSED
        );
    }
}
