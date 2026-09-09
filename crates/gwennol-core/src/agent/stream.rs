//! Reading a streamed turn: contract NDJSON events off a Gwead stream
//! handle, one at a time, cancellable, bounded.

use gwead::kernel::streams::{
    STREAM_CANCELLED, STREAM_EOF, SharedStreamRegistry, StreamId, lock_shared, read_async_shared,
};
use gwead::serde_json::Value;
use gwead::tokio_util::sync::CancellationToken;

/// Why [`EventReader::next`] could not produce an event.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ReadError {
    /// The turn was cancelled: either the reader declined to start a
    /// new read because the token had already fired, or a read parked
    /// on a quiet source was released by it.
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
    ///
    /// Two guards keep a cancelled turn from reading past its token,
    /// and neither replaces the other. The token is passed into the
    /// read itself, which releases a read parked on a source that has
    /// *not yet* yielded — the kernel polls the source before the
    /// token, so bytes, an error, or EOF already available still win.
    /// A source that is instead *always immediately ready with data*
    /// would outrun a fired token indefinitely under that alone, so
    /// `next` also checks the token immediately before issuing each
    /// read and declines to start one once it has fired. Both checks
    /// sit after the buffer scan, so an event already whole in the
    /// buffer is still returned before either stops the read.
    ///
    /// Neither guard bounds a source that is always immediately ready
    /// with **empty** chunks: gwead's own `read_async` skips those
    /// inside one call rather than returning to ask again, so a fired
    /// token cannot win the race until the source finally yields
    /// something or ends. Unreachable from any of this crate's own
    /// guests — `Stream::write_all` never commits a zero-length
    /// write — so this is a residual bound on a third-party `LLM_CHAT`
    /// guest, not a gap in these two checks; tracked upstream as
    /// gwead#22.
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
            // No new read once the turn is cancelled. Only a read that has to
            // *wait* is released by the token below; the kernel polls the
            // source before the token by contract, so a source that is
            // always immediately ready would otherwise outrun a fired
            // token indefinitely. The scan above has already run, so an
            // event that was fully buffered is still returned before this
            // stops asking for more.
            if cancel.is_cancelled() {
                return Err(ReadError::Cancelled);
            }
            let mut chunk = [0u8; CHUNK];
            let n = read_async_shared(&self.streams, self.id, &mut chunk, cancel).await;
            match n {
                n if n > 0 => self.buf.extend_from_slice(&chunk[..n as usize]),
                STREAM_EOF => self.eof = true,
                STREAM_CANCELLED => return Err(ReadError::Cancelled),
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use gwead::bytes::Bytes;
    use gwead::futures::StreamExt as _;
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

    /// A readable that yields `chunk` forever and never returns
    /// `Pending` — the shape that outruns a fired token when nothing
    /// bounds the reader. Counts how many times the source was polled
    /// so a test can assert a bound rather than wait for one.
    fn always_ready(
        chunk: &'static str,
        polls: Arc<AtomicUsize>,
    ) -> (SharedStreamRegistry, StreamId) {
        let source = Box::pin(gwead::futures::stream::repeat_with(move || {
            polls.fetch_add(1, Ordering::SeqCst);
            Ok(Bytes::from_static(chunk.as_bytes()))
        }));
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
    async fn an_event_larger_than_a_chunk_is_assembled_in_one_pass() {
        // A 100 KB event delivered in 3 KB pieces: many reads per line,
        // each resuming the scan where the last stopped.
        let text = "x".repeat(100_000);
        let line = format!("{{\"type\":\"text\",\"text\":\"{text}\"}}\n{{\"type\":\"end\"}}\n");
        let line: &'static str = Box::leak(line.into_boxed_str());
        let chunks: Vec<&'static str> = line
            .as_bytes()
            .chunks(3000)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect();
        let (streams, id) = readable(chunks);
        let cancel = CancellationToken::new();
        let mut reader = EventReader::new(streams, id, 1 << 20);
        let event = reader.next(&cancel).await.unwrap().unwrap();
        assert_eq!(event["text"].as_str().unwrap().len(), 100_000);
        assert_eq!(
            reader.next(&cancel).await.unwrap(),
            Some(json!({"type": "end"}))
        );
        assert_eq!(reader.next(&cancel).await.unwrap(), None);
    }

    #[tokio::test]
    async fn read_failures_carry_the_streams_code() {
        use gwead::kernel::streams::STREAM_IO_ERROR;
        // A source that fails: the I/O code.
        let source = Box::pin(gwead::futures::stream::iter([
            Ok(Bytes::from_static(b"{\"type\":\"text\",\"text\":\"a\"}\n")),
            Err(std::io::Error::other("transport died")),
        ]));
        let mut registry = StreamRegistry::new();
        let id = registry.register_readable("application/x-ndjson", source);
        let streams = Arc::new(Mutex::new(registry));
        let cancel = CancellationToken::new();
        let mut reader = EventReader::new(streams, id, 1 << 20);
        assert!(reader.next(&cancel).await.unwrap().is_some());
        assert_eq!(
            reader.next(&cancel).await.unwrap_err(),
            ReadError::Io(STREAM_IO_ERROR)
        );

        // A handle closed under the reader: the closed code, not EOF.
        let (streams, id) = readable(vec!["{\"type\":\"end\"}\n"]);
        let mut reader = EventReader::new(streams.clone(), id, 1 << 20);
        lock_shared(&streams).close(id);
        assert_eq!(
            reader.next(&cancel).await.unwrap_err(),
            ReadError::Io(STREAM_CLOSED)
        );
    }

    #[tokio::test]
    async fn cancellation_ends_a_parked_read() {
        // One chunk, then a source that genuinely parks — never
        // returning `Pending` forever without ending — so the release
        // below can only come from the kernel unparking a real wait,
        // never from D3's pre-read check.
        let source = Box::pin(
            gwead::futures::stream::iter([Ok(Bytes::from_static(
                b"{\"type\":\"text\",\"text\":\"a\"}\n",
            ))])
            .chain(gwead::futures::stream::pending::<
                Result<Bytes, std::io::Error>,
            >()),
        );
        let mut registry = StreamRegistry::new();
        let id = registry.register_readable("application/x-ndjson", source);
        let streams = Arc::new(Mutex::new(registry));
        let cancel = CancellationToken::new();
        let mut reader = EventReader::new(streams.clone(), id, 1 << 20);
        // Consume the first chunk with a quiet token: the source has
        // now genuinely parked, and the read below has to wait on it.
        assert!(reader.next(&cancel).await.unwrap().is_some());
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
            read_async_shared(&streams, id, &mut buf, &CancellationToken::new()).await,
            STREAM_CLOSED
        );
    }

    #[tokio::test]
    async fn a_fired_token_stops_the_reader_on_an_always_ready_source() {
        // A chunk that is not a complete line — no newline — so the
        // reader can never return an event from it and only D3's
        // pre-read check can stop the loop; under D2 alone, an
        // always-ready source (the kernel polls it before the token)
        // would be consumed forever.
        let polls = Arc::new(AtomicUsize::new(0));
        let (streams, id) = always_ready("{\"type\":\"text\"", polls.clone());
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut reader = EventReader::new(streams, id, 1 << 20);
        // Without D3, this reads forever rather than returning
        // `Cancelled` — but the buffer's 1 MiB cap trips first, after
        // ~128 reads, failing the outcome assertion below in well
        // under a second; the timeout here is a backstop, not the
        // guard that actually catches a reverted D3. The poll counter
        // is: it also catches a weaker fix that reads once and only
        // then checks the token, which the timeout and cap alone
        // would not distinguish from the real one.
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), reader.next(&cancel))
            .await
            .expect("a fired token must stop the reader without a source read");
        assert_eq!(outcome, Err(ReadError::Cancelled));
        assert_eq!(
            polls.load(Ordering::SeqCst),
            0,
            "the source was never polled"
        );
    }

    #[tokio::test]
    async fn a_token_fired_before_the_first_read_polls_no_source() {
        let (streams, id) = readable(vec!["{\"type\":\"end\"}\n"]);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut reader = EventReader::new(streams.clone(), id, 1 << 20);
        assert_eq!(reader.next(&cancel).await, Err(ReadError::Cancelled));
        // The reader never consumed the queued bytes: a direct,
        // quietly-tokened read over the same handle still finds them.
        let mut buf = [0u8; 64];
        let n = read_async_shared(&streams, id, &mut buf, &CancellationToken::new()).await;
        assert!(n > 0, "the queued event is still there: {n}");
    }

    #[tokio::test]
    async fn an_event_already_buffered_is_returned_before_the_reader_stops() {
        // One chunk holding two whole events.
        let (streams, id) = readable(vec![
            "{\"type\":\"text\",\"text\":\"a\"}\n{\"type\":\"end\"}\n",
        ]);
        let cancel = CancellationToken::new();
        let mut reader = EventReader::new(streams, id, 1 << 20);
        assert_eq!(
            reader.next(&cancel).await.unwrap(),
            Some(json!({"type": "text", "text": "a"}))
        );
        cancel.cancel();
        // The second event was already whole in the buffer: it is
        // still returned before either guard stops the read.
        assert_eq!(
            reader.next(&cancel).await.unwrap(),
            Some(json!({"type": "end"}))
        );
        assert_eq!(reader.next(&cancel).await, Err(ReadError::Cancelled));
    }

    /// This makes the reader's EOF-under-cancel path unreachable — once
    /// cancelled, `next` declines to read again, so it never sees the
    /// `STREAM_EOF` that would otherwise arrive next. The turn's own
    /// `Ok(None) if cancel.is_cancelled()` arm in `consume_stream`
    /// (`agent/mod.rs`) is what ends such a turn as cancelled instead.
    #[tokio::test]
    async fn an_end_of_stream_under_a_fired_token_stops_at_the_check() {
        let (streams, id) = readable(vec!["{\"type\":\"end\"}\n"]);
        let cancel = CancellationToken::new();
        let mut reader = EventReader::new(streams, id, 1 << 20);
        assert_eq!(
            reader.next(&cancel).await.unwrap(),
            Some(json!({"type": "end"}))
        );
        cancel.cancel();
        assert_eq!(reader.next(&cancel).await, Err(ReadError::Cancelled));
    }

    #[tokio::test]
    async fn a_release_puts_the_source_back_for_a_later_read() {
        // An mpsc-backed source, not `readable`: keeping `tx` after
        // registration lets the test tell a lost/dropped source (the
        // bug row 3 names — "the swapped-out source goes back") from a
        // merely quiet one instantly. `tx.send` fails the moment its
        // receiver is dropped, so a regression here fails in
        // milliseconds, not on a timer.
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(4);
        let source = Box::pin(gwead::futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        }));
        let mut registry = StreamRegistry::new();
        let id = registry.register_readable("application/x-ndjson", source);
        let streams = Arc::new(Mutex::new(registry));
        let cancel = CancellationToken::new();

        tx.send(Ok(Bytes::from_static(
            b"{\"type\":\"text\",\"text\":\"a\"}\n",
        )))
        .await
        .unwrap();
        let mut reader = EventReader::new(streams, id, 1 << 20);
        // Consume the queued event with a quiet token: the channel is
        // now empty, so the reader's next read genuinely parks on
        // `rx.recv()` rather than returning immediately.
        assert!(reader.next(&cancel).await.unwrap().is_some());
        let waiting = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                let outcome = reader.next(&cancel).await;
                (outcome, reader)
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cancel.cancel();
        let (outcome, mut reader) =
            tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
                .await
                .expect("cancellation ends the parked read")
                .unwrap();
        assert_eq!(outcome, Err(ReadError::Cancelled));

        // The source was put back, not lost: the sender still has a
        // live receiver on the other end.
        tx.send(Ok(Bytes::from_static(b"{\"type\":\"end\"}\n")))
            .await
            .expect("the release must put the source back, not drop it");
        // And the handle is genuinely live, not merely holding a
        // sender with nowhere for its bytes to go: reading it again,
        // reader kept alive throughout, finds the new event.
        let quiet = CancellationToken::new();
        assert_eq!(
            reader.next(&quiet).await.unwrap(),
            Some(json!({"type": "end"}))
        );
    }
}
