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
//! - **empty `data:` values before the first non-empty one buffer
//!   nothing** — a deliberate departure from the WHATWG algorithm,
//!   which would buffer a separator for each (and so would dispatch an
//!   all-empty event where this parser drops it, the way it drops
//!   keepalives). Interior and trailing empty values join byte-exactly;
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

/// The most an in-progress event's data buffer may hold before its
/// terminating blank line — the other shape of the flood
/// [`MAX_LINE_BYTES`] bounds: endless *short* `data:` lines with the
/// blank line never coming pass the line cap on every byte. The bound
/// is on the buffer's **actual bytes** (`\n` separators included), not
/// on a payload tally that a parallel structure could outgrow — the
/// accounting cannot diverge from the memory, so the parser's whole
/// buffering is bounded by these two caps plus the `event:` field's
/// value, itself bounded by the line cap.
pub const MAX_EVENT_BYTES: usize = 8 * 1024 * 1024;

/// Incremental parser state. Feed it chunks as they arrive; it returns
/// each event exactly once, as soon as the terminating empty line is
/// complete.
#[derive(Debug, Default)]
pub struct SseParser {
    /// Bytes of the current, not-yet-complete line.
    line: Vec<u8>,
    /// The in-progress event's `event:` field, if seen.
    event_type: Option<String>,
    /// The in-progress event's data buffer: `data:` values joined with
    /// `\n` as they arrive. One `String`, so `self.data.len()` *is* the
    /// memory the event holds — no per-line container overhead to
    /// escape [`MAX_EVENT_BYTES`], and empty `data:` values buffer at
    /// most their separator. (An event whose values were all empty
    /// leaves the buffer empty and is not dispatched — this parser's
    /// deliberate stricter reading, not the WHATWG algorithm's, which
    /// would buffer a separator per empty value; see the module docs.)
    data: String,
    /// Set once [`Self::feed`] has returned an error; every later call
    /// fails too, making "the parser is spent" a property the type
    /// enforces rather than a doc-comment plea.
    spent: bool,
}

impl SseParser {
    pub fn new() -> SseParser {
        SseParser::default()
    }

    /// Consume one chunk, returning every event it completed.
    ///
    /// Fails once any single line exceeds [`MAX_LINE_BYTES`], or once
    /// one event accumulates more than [`MAX_EVENT_BYTES`] of `data:`
    /// payload without its terminating blank line — either way the
    /// input is not a plausible event stream. After an error the
    /// parser is spent: every further call fails, so a caller cannot
    /// accidentally resume mid-flood.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>, String> {
        if self.spent {
            return Err("SSE parser already failed; the stream should have been aborted".into());
        }
        let mut out = Vec::new();
        for &b in bytes {
            if b != b'\n' {
                if self.line.len() >= MAX_LINE_BYTES {
                    self.spent = true;
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
            if let Some(event) = self.take_line(&line)? {
                out.push(event);
            }
        }
        Ok(out)
    }

    /// Handle one complete line; returns an event when the line
    /// terminated one, or an error when it pushed the in-progress
    /// event past [`MAX_EVENT_BYTES`].
    fn take_line(&mut self, line: &str) -> Result<Option<SseEvent>, String> {
        if line.is_empty() {
            let data = std::mem::take(&mut self.data);
            let event_type = self.event_type.take();
            // An event with an empty data buffer is not dispatched —
            // the parser's deliberate stricter reading (the module docs
            // own the departure from WHATWG), and what makes a lone
            // keepalive comment plus blank line invisible.
            if data.is_empty() {
                return Ok(None);
            }
            return Ok(Some(SseEvent {
                event: event_type.unwrap_or_else(|| "message".to_string()),
                data,
            }));
        }
        if line.starts_with(':') {
            return Ok(None); // comment
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            // A field name with no colon has an empty value.
            None => (line, ""),
        };
        match field {
            "event" => self.event_type = Some(value.to_string()),
            "data" => {
                // Charge what will actually be stored: the value plus
                // its separator. A parallel byte tally once bounded
                // only payloads while every accepted line also bought
                // container overhead — bounding the buffer itself is
                // what makes the cap mean memory.
                if self
                    .data
                    .len()
                    .saturating_add(value.len())
                    .saturating_add(1)
                    > MAX_EVENT_BYTES
                {
                    self.spent = true;
                    return Err(format!(
                        "one SSE event accumulated more than {MAX_EVENT_BYTES} bytes of \
                         data without its terminating blank line — this is not an \
                         event stream"
                    ));
                }
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(value);
            }
            _ => {} // id, retry, anything newer: ignored
        }
        Ok(None)
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

    #[test]
    fn an_endless_stream_of_short_data_lines_fails_too() {
        // The other shape of the flood: every line passes the line cap,
        // the blank line never comes, and the per-event accumulator is
        // what has to say no.
        let mut p = SseParser::new();
        let line = b"data: xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\n";
        let payload_per_line = line.len() - "data: ".len() - 1;
        let lines_to_cap = MAX_EVENT_BYTES / payload_per_line + 2;
        let mut err = None;
        for _ in 0..lines_to_cap {
            match p.feed(line) {
                Ok(events) => assert!(events.is_empty(), "no blank line ever arrives"),
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        let err = err.expect("the event cap fires before the loop ends");
        assert!(err.contains("not an event stream"), "{err}");
    }

    #[test]
    fn an_empty_value_flood_buys_no_memory_and_no_event() {
        // `data:\n` forever: six wire bytes must not buy any buffered
        // bytes. Under the old per-payload tally each such line cost a
        // container slot the accounting never saw; now an empty value
        // before any content stores nothing at all. The proof is
        // behavioral: if the flood had buffered anything, the eventual
        // real value would arrive with a million separators in front.
        let mut p = SseParser::new();
        let flood = "data:\n".repeat(1_000_000);
        assert!(p.feed(flood.as_bytes()).unwrap().is_empty());
        let events = p.feed(b"data: x\n\n").unwrap();
        assert_eq!(
            events,
            vec![SseEvent {
                event: "message".into(),
                data: "x".into()
            }],
            "the empty-value flood left no trace in the buffer"
        );

        // And an event whose values were ALL empty is not dispatched —
        // the parser's deliberate stricter-than-WHATWG reading.
        let mut q = SseParser::new();
        assert!(q.feed(b"data:\ndata:\n\n").unwrap().is_empty());
    }

    #[test]
    fn a_separator_only_flood_after_content_is_capped() {
        // Once the buffer holds anything, every further empty value
        // costs its separator — so the same endless-`data:` stream that
        // buffers nothing up front is capped the moment it grows the
        // event. Charged as actual bytes, so the cap fires at
        // MAX_EVENT_BYTES of real memory, not at some tally the
        // representation can outgrow.
        let mut p = SseParser::new();
        p.feed(b"data: a\n").unwrap();
        let chunk = "data:\n".repeat(64 * 1024);
        let mut err = None;
        for _ in 0..(MAX_EVENT_BYTES / (64 * 1024) + 2) {
            match p.feed(chunk.as_bytes()) {
                Ok(events) => assert!(events.is_empty(), "no blank line ever arrives"),
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        let err = err.expect("the event cap fires before the loop ends");
        assert!(err.contains("not an event stream"), "{err}");
    }

    #[test]
    fn a_spent_parser_refuses_to_resume() {
        let mut p = SseParser::new();
        let flood = vec![b'z'; MAX_LINE_BYTES + 1];
        p.feed(&flood).expect_err("over the line cap");
        // Without the spent flag, this newline would complete the flood
        // as an ordinary line and parsing would silently resume.
        let err = p
            .feed(b"\ndata: x\n\n")
            .expect_err("the parser stays failed");
        assert!(err.contains("already failed"), "{err}");
    }
}
