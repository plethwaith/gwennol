//! An incremental server-sent-events parser.
//!
//! Pure with respect to the guest ABI — bytes in, events out — so the
//! host-target unit tests below cover it directly; the wasm entry point
//! only wires it to streams. Implements the subset of the SSE wire
//! format a model provider's stream actually uses:
//!
//! - a line ending is `\n`, with an optional preceding `\r` stripped;
//! - an event is terminated by an empty line, and is dispatched only if
//!   it accumulated data;
//! - `event:` names the event type (default `message`), `data:` lines
//!   accumulate and join with `\n`, one leading space after the colon
//!   is stripped;
//! - `:` comment lines and unknown fields (`id`, `retry`) are ignored.
//!
//! Chunk boundaries carry no meaning: bytes may arrive split anywhere,
//! including mid-UTF-8-sequence, because the parser buffers raw bytes
//! and only interprets complete lines.

/// One dispatched SSE event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// The `event:` field's value, or `message` when the event carried
    /// none — the wire format's default.
    pub event: String,
    /// The `data:` lines, joined with `\n`.
    pub data: String,
}

/// The longest line the parser will buffer. Generous — a provider's
/// whole tool-call payload can ride on one `data:` line — but bounded:
/// without it, a misbehaving endpoint answering with a large
/// newline-free body (binary, one giant line) grows the buffer until
/// the wasm memory cap traps opaquely, where this limit fails the step
/// with a readable message like every other malformed-input path.
pub const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

/// Incremental parser state. Feed it chunks as they arrive; it returns
/// each event exactly once, as soon as the terminating empty line is
/// complete.
#[derive(Debug, Default)]
pub struct SseParser {
    /// Bytes of the current, not-yet-complete line.
    line: Vec<u8>,
    /// The in-progress event's `event:` field, if seen.
    event_type: Option<String>,
    /// The in-progress event's accumulated `data:` lines.
    data: Vec<String>,
}

impl SseParser {
    pub fn new() -> SseParser {
        SseParser::default()
    }

    /// Consume one chunk, returning every event it completed, or an
    /// error once any single line exceeds [`MAX_LINE_BYTES`] — input
    /// that long without a newline is not an event stream. After an
    /// error the parser is spent; the caller aborts the stream.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>, String> {
        let mut out = Vec::new();
        for &b in bytes {
            if b != b'\n' {
                if self.line.len() >= MAX_LINE_BYTES {
                    return Err(format!(
                        "SSE line exceeded {MAX_LINE_BYTES} bytes without a newline — \
                         this is not an event stream"
                    ));
                }
                self.line.push(b);
                continue;
            }
            if self.line.last() == Some(&b'\r') {
                self.line.pop();
            }
            let line = std::mem::take(&mut self.line);
            // Field names are ASCII; data payloads may be any UTF-8 —
            // lossy decoding keeps a torn byte from killing the stream
            // while never touching well-formed input.
            let line = String::from_utf8_lossy(&line).into_owned();
            if let Some(event) = self.take_line(&line) {
                out.push(event);
            }
        }
        Ok(out)
    }

    /// Handle one complete line; returns an event when the line
    /// terminated one.
    fn take_line(&mut self, line: &str) -> Option<SseEvent> {
        if line.is_empty() {
            let data = std::mem::take(&mut self.data);
            let event_type = self.event_type.take();
            // An event with no data is not dispatched — that's the
            // format's rule, and it is what makes a lone keepalive
            // comment plus blank line invisible.
            if data.is_empty() {
                return None;
            }
            return Some(SseEvent {
                event: event_type.unwrap_or_else(|| "message".to_string()),
                data: data.join("\n"),
            });
        }
        if line.starts_with(':') {
            return None; // comment
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            // A field name with no colon has an empty value.
            None => (line, ""),
        };
        match field {
            "event" => self.event_type = Some(value.to_string()),
            "data" => self.data.push(value.to_string()),
            _ => {} // id, retry, anything newer: ignored
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(parser: &mut SseParser, chunks: &[&str]) -> Vec<SseEvent> {
        chunks
            .iter()
            .flat_map(|c| parser.feed(c.as_bytes()).expect("within the line cap"))
            .collect()
    }

    #[test]
    fn one_event_per_blank_line() {
        let mut p = SseParser::new();
        let events = feed_all(
            &mut p,
            &["event: text\ndata: {\"a\":1}\n\nevent: end\ndata: {}\n\n"],
        );
        assert_eq!(
            events,
            vec![
                SseEvent {
                    event: "text".into(),
                    data: "{\"a\":1}".into()
                },
                SseEvent {
                    event: "end".into(),
                    data: "{}".into()
                },
            ]
        );
    }

    #[test]
    fn chunk_boundaries_are_meaningless() {
        // Split mid-field-name, mid-value, and between \r and \n.
        let mut p = SseParser::new();
        let events = feed_all(
            &mut p,
            &[
                "ev", "ent: te", "xt\r", "\nda", "ta: pay", "load\r\n", "\r\n",
            ],
        );
        assert_eq!(
            events,
            vec![SseEvent {
                event: "text".into(),
                data: "payload".into()
            }]
        );
    }

    #[test]
    fn chunk_boundaries_may_tear_multi_byte_utf8() {
        // "é" is 0xC3 0xA9; split between the two bytes. Buffering is
        // byte-level and decoding happens per complete line, so the
        // torn sequence reassembles losslessly.
        let mut p = SseParser::new();
        let payload = "data: caf\u{e9} {\"ok\":true}\n\n".as_bytes();
        let split = payload.iter().position(|&b| b == 0xC3).unwrap() + 1;
        let mut events = p.feed(&payload[..split]).unwrap();
        events.extend(p.feed(&payload[split..]).unwrap());
        assert_eq!(
            events,
            vec![SseEvent {
                event: "message".into(),
                data: "caf\u{e9} {\"ok\":true}".into()
            }]
        );
    }

    #[test]
    fn multi_line_data_joins_with_newline() {
        let mut p = SseParser::new();
        let events = feed_all(&mut p, &["data: {\"x\":\ndata:  1}\n\n"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "message", "default event type");
        assert_eq!(
            events[0].data, "{\"x\":\n 1}",
            "one space stripped, rest kept"
        );
    }

    #[test]
    fn comments_and_dataless_events_do_not_dispatch() {
        let mut p = SseParser::new();
        let events = feed_all(&mut p, &[": keepalive\n\nevent: ping\n\ndata: x\n\n"]);
        assert_eq!(
            events,
            vec![SseEvent {
                event: "message".into(),
                data: "x".into()
            }],
            "only the event that accumulated data dispatches"
        );
    }

    #[test]
    fn incomplete_trailing_event_is_never_dispatched() {
        let mut p = SseParser::new();
        assert!(p.feed(b"data: never terminated\n").unwrap().is_empty());
        // No blank line arrives; the event stays undelivered, which is
        // what makes a truncated stream detectable downstream.
    }

    #[test]
    fn a_newline_free_flood_fails_instead_of_growing_forever() {
        let mut p = SseParser::new();
        // One byte past the cap, delivered in two chunks so the check
        // provably spans feeds.
        let flood = vec![b'z'; MAX_LINE_BYTES];
        assert!(p.feed(&flood).is_ok(), "the cap itself still fits");
        let err = p.feed(b"z").expect_err("the byte after the cap fails");
        assert!(err.contains("not an event stream"), "{err}");
    }
}
