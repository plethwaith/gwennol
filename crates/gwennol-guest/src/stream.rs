//! Typed byte-stream I/O over Gwead stream handles.

use crate::sys;

/// A stream-handle failure, decoded from the ABI's negative return
/// codes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamError {
    /// The handle is not in this invocation's stream registry.
    InvalidHandle,
    /// Read on a writable handle, or write on a readable one.
    DirectionMismatch,
    /// The handle was closed. On a read, this is what
    /// [`Stream::read`] itself reports. On a write,
    /// [`Stream::write_all`] never reaches this variant: it decodes
    /// `STREAM_CLOSED` itself, into [`Delivery::ReaderGone`] — the
    /// normal way a producer learns its reader stopped listening —
    /// before falling through to `from_code`. A producer winds that
    /// down; it never sees `Closed` for its own write.
    Closed,
    /// The readable's underlying source reported an I/O error, with
    /// the vendor's own text when the host recorded one
    /// (`stream_last_error`) — `None` when it recorded nothing.
    Io { detail: Option<String> },
    /// [`Stream::read`] was handed an empty buffer, whose 0-byte read
    /// would be indistinguishable from end-of-stream. Guest-side; the
    /// host was never asked.
    EmptyBuffer,
    /// The host committed zero bytes on a non-empty write — not an ABI
    /// error code, but not the documented success shape either.
    ZeroCommit,
    /// A code this crate does not know; carries the raw value. Seeing
    /// one means the kernel speaks a newer ABI revision than this
    /// crate was built for.
    Other(i32),
}

impl StreamError {
    fn from_code(handle: i32, code: i32) -> StreamError {
        match code {
            sys::STREAM_INVALID_HANDLE => StreamError::InvalidHandle,
            sys::STREAM_DIRECTION_MISMATCH => StreamError::DirectionMismatch,
            sys::STREAM_CLOSED => StreamError::Closed,
            sys::STREAM_IO_ERROR => StreamError::Io {
                detail: last_error_text(|buf| sys::stream_last_error(handle, buf)),
            },
            // STREAM_CANCELLED never reaches here: `Stream::read` and
            // `Stream::write_all` decode it themselves, into
            // `Received::Cancelled` / `Delivery::Cancelled`, before
            // falling through to this function — the text is retained
            // on the handle, not drained, so a binding must key on the
            // code it just got rather than on whether text is present.
            // STREAM_OOB can't arise from these wrappers — the buffer
            // is a real slice in our own linear memory — so it lands
            // in Other alongside genuinely unknown codes.
            other => StreamError::Other(other),
        }
    }
}

/// Decode the text a `stream_last_error`-shaped call hands back:
/// `fetch` is given a fixed, `MAX_LAST_ERROR_BYTES`-sized buffer and
/// returns the ABI's return value (the text's full length, `0` for
/// nothing recorded, or a negative `STREAM_*` failure for the probe
/// itself). A non-positive return is "no text"; a length past the
/// buffer — impossible under today's kernel cap, but the ABI documents
/// the return as the text's full length, not the copied length — is
/// clamped to what was actually copied and decoded lossily, since a
/// truncated copy may end mid-codepoint.
fn last_error_text(fetch: impl FnOnce(&mut [u8]) -> i32) -> Option<String> {
    let mut buf = [0u8; sys::MAX_LAST_ERROR_BYTES];
    let n = fetch(&mut buf);
    if n <= 0 {
        return None;
    }
    let n = (n as usize).min(buf.len());
    Some(String::from_utf8_lossy(&buf[..n]).into_owned())
}

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamError::InvalidHandle => write!(f, "invalid stream handle"),
            StreamError::DirectionMismatch => write!(f, "stream direction mismatch"),
            StreamError::Closed => write!(f, "stream closed"),
            StreamError::Io { detail: Some(d) } => write!(f, "stream source I/O error: {d}"),
            StreamError::Io { detail: None } => write!(f, "stream source I/O error"),
            StreamError::EmptyBuffer => {
                write!(f, "zero-length read buffer is ambiguous with end-of-stream")
            }
            StreamError::ZeroCommit => write!(f, "host committed zero bytes on a stream write"),
            StreamError::Other(code) => write!(f, "unknown stream error code {code}"),
        }
    }
}

/// What one [`Stream::read`] call produced. Mirrors [`Delivery`]: a
/// small enum naming what became of the call, so the benign outcomes
/// stay out of [`StreamError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Received {
    /// `n` bytes copied into the caller's buffer.
    Bytes(usize),
    /// The source is exhausted. Distinct from [`Received::Cancelled`]:
    /// this source will never yield again, where a cancelled one is
    /// merely untouched.
    End,
    /// The read was parked on a source that had not yet yielded, and
    /// was released by the step's cancellation token. Nothing was
    /// copied and the source is untouched — unlike `End`, a fresh read
    /// on the same handle would still find it live.
    Cancelled,
}

/// Map a `stream_read` return value to [`Stream::read`]'s outcome.
/// Pure and testable off-wasm, unlike `sys::stream_read` itself: any
/// code not classified here is left for the caller to decode through
/// [`StreamError::from_code`], which alone needs the handle — to fetch
/// `stream_last_error`'s text for `STREAM_IO_ERROR` — the seam that
/// keeps that host call out of this table.
fn classify_read(n: i32) -> Result<Received, i32> {
    match n {
        n if n >= 0 => Ok(Received::Bytes(n as usize)),
        sys::STREAM_EOF => Ok(Received::End),
        sys::STREAM_CANCELLED => Ok(Received::Cancelled),
        code => Err(code),
    }
}

/// Map one `stream_write` return value to a step of
/// [`Stream::write_all`]'s loop. Pure and testable off-wasm, unlike
/// `sys::stream_write` itself; built the same way as `classify_read`,
/// with one deliberate difference — `classify_read` maps `0` to
/// `Ok(Received::Bytes(0))`, a real (if never-healthy) outcome to
/// report, where a write's `0` is `ZeroCommit`, an error the caller
/// decodes, since the two ABI calls don't treat it alike.
///
/// `Ok(Delivery::Delivered)` means only that *this call* committed `n`
/// bytes — the loop advances `rest` by `n` and keeps going, or, if
/// `rest` is now empty, that is also the whole write's outcome. It is
/// not itself a promise that the whole buffer is sent; `write_all`'s
/// own `rest.is_empty()` decides that. Any other `Ok` is a terminal
/// [`Delivery`] the loop returns immediately. `0` (`ZeroCommit`, not an
/// ABI error code) and every other unclassified code are left for the
/// caller: `0` needs no handle, and anything else is decoded through
/// [`StreamError::from_code`].
fn classify_write(n: i32) -> Result<Delivery, i32> {
    match n {
        n if n > 0 => Ok(Delivery::Delivered),
        sys::STREAM_CANCELLED => Ok(Delivery::Cancelled),
        sys::STREAM_CLOSED => Ok(Delivery::ReaderGone),
        code => Err(code),
    }
}

/// One Gwead stream handle: an index into the current invocation's
/// stream registry, readable or writable (the registry knows which;
/// calling the wrong direction returns
/// [`StreamError::DirectionMismatch`]).
///
/// Dropping a `Stream` does **not** close the handle. That is
/// deliberate: handles regularly outlive the guest's interest in them —
/// an entry point that obtains a handle from
/// [`invoke_streaming`](crate::invoke_streaming) and returns it as part
/// of its result must leave it open for whoever reads the result. Call
/// [`Stream::close`] when the guest itself is the endpoint and is
/// done — don't rely on the kernel's post-invocation drain instead: it
/// runs only when the embedder that invoked this action left the
/// kernel owning the stream table, and is skipped whenever that
/// embedder supplied its own, which is how a streamed handle reaches a
/// consumer outside the action at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stream {
    handle: i32,
}

impl Stream {
    /// Wrap a handle received as data — a prior step's result, say.
    /// Returns `None` for values that cannot be a handle (handles are
    /// positive by the ABI's construction).
    pub fn from_handle(handle: i32) -> Option<Stream> {
        (handle > 0).then_some(Stream { handle })
    }

    /// The pre-provisioned writable output of this step, when the step
    /// is `long_running` in a `dataflow: true` action; `None` for every
    /// other kind of step.
    pub fn output() -> Option<Stream> {
        Stream::from_handle(sys::stream_output())
    }

    /// The raw handle, for carrying in a result.
    pub fn handle(&self) -> i32 {
        self.handle
    }

    /// Read into `buf`, blocking (via the host) until [`Received::Bytes`]
    /// arrive, the source ends ([`Received::End`]), or the step's own
    /// cancellation releases a parked wait ([`Received::Cancelled`]).
    /// An empty `buf` is refused ([`StreamError::EmptyBuffer`]) — its
    /// 0-byte read would be indistinguishable from EOF, and guests ship
    /// as release builds where a debug assertion would never fire.
    pub fn read(&self, buf: &mut [u8]) -> Result<Received, StreamError> {
        if buf.is_empty() {
            return Err(StreamError::EmptyBuffer);
        }
        classify_read(sys::stream_read(self.handle, buf))
            .map_err(|code| StreamError::from_code(self.handle, code))
    }

    /// Write all of `buf`, blocking (via the host) while the consumer
    /// applies backpressure. A write released by the step's
    /// cancellation token part-way through the loop discards how much
    /// was already committed: the step is stopping, and no caller
    /// retries or reports a byte count for a partial send.
    pub fn write_all(&self, buf: &[u8]) -> Result<Delivery, StreamError> {
        // The ABI commits the whole buffer per successful call; the
        // loop is defensive against a future partial-commit revision.
        let mut rest = buf;
        while !rest.is_empty() {
            let n = sys::stream_write(self.handle, rest);
            match classify_write(n) {
                Ok(Delivery::Delivered) => rest = &rest[(n as usize).min(rest.len())..],
                Ok(terminal) => return Ok(terminal),
                Err(0) => return Err(StreamError::ZeroCommit),
                Err(code) => return Err(StreamError::from_code(self.handle, code)),
            }
        }
        Ok(Delivery::Delivered)
    }

    /// Close the handle. Idempotent; closing early is how a consumer
    /// tells a producer to stop, and how a producer signals EOF before
    /// its step returns.
    pub fn close(&self) {
        let _ = sys::stream_close(self.handle);
    }
}

/// What became of one call to [`Stream::write_all`] (and so, since it
/// forwards the result, one line written with
/// [`Stream::write_json_line`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// The consumer took it.
    Delivered,
    /// The consumer has closed its handle, which is the normal way a
    /// producer learns the reader stopped listening. Wind down; don't
    /// treat it as a failure.
    ReaderGone,
    /// The write was parked on a full channel and released by the
    /// step's cancellation token before it could commit. Bytes an
    /// earlier iteration of `write_all`'s loop already committed are
    /// discarded from the caller's perspective: the step is stopping.
    Cancelled,
}

impl Stream {
    /// Write one JSON value as one newline-terminated line — the
    /// contract's NDJSON event framing.
    ///
    /// Compact serialisation is what keeps one event on one line: a
    /// value carrying raw newlines inside its strings is escaped, never
    /// split.
    pub fn write_json_line(&self, value: &serde_json::Value) -> Result<Delivery, StreamError> {
        let mut line = serde_json::to_string(value).map_err(|e| StreamError::Io {
            detail: Some(e.to_string()),
        })?;
        line.push('\n');
        self.write_all(line.as_bytes())
    }

    /// Read the rest of the stream as lossy UTF-8, trimmed and truncated
    /// to `cap` bytes — an excerpt for an error message, typically a
    /// vendor's non-2xx body. Up to one extra chunk is consumed past the
    /// cap; that overshoot is how truncation is detected, and is trimmed
    /// away. Every degraded state marks itself: a truncated excerpt
    /// says so, and a read error mid-body marks the partial excerpt as
    /// interrupted — except a body that could not be read *at all*,
    /// which says so instead of posing as empty, unless the reason was
    /// the step's own cancellation reaching the very first read (below),
    /// in which case it is indistinguishable from an empty body. A read
    /// released by the step's own cancellation token stops collecting
    /// like end-of-stream and is **not** marked interrupted: the
    /// excerpt exists to explain a non-2xx body to an operator who may
    /// at that moment be cancelling the turn, and reporting their own
    /// stop as a degraded read would misname it.
    pub fn read_excerpt(&self, cap: usize) -> String {
        collect_excerpt(cap, |buf| self.read(buf))
    }
}

/// The pure body of [`Stream::read_excerpt`], taking the read as a
/// closure so its collection and marker logic is testable off-wasm:
/// `read` stands in for repeated calls to [`Stream::read`].
fn collect_excerpt(
    cap: usize,
    mut read: impl FnMut(&mut [u8]) -> Result<Received, StreamError>,
) -> String {
    let mut collected = Vec::new();
    let mut buf = [0u8; 1024];
    let mut interrupted = false;
    while collected.len() <= cap {
        match read(&mut buf) {
            Ok(Received::End) | Ok(Received::Cancelled) => break,
            // No progress; a healthy kernel never sends one.
            Ok(Received::Bytes(0)) => break,
            Ok(Received::Bytes(n)) => collected.extend_from_slice(&buf[..n]),
            Err(_) => {
                interrupted = true;
                break;
            }
        }
    }
    let truncated = collected.len() > cap;
    collected.truncate(cap);
    let mut text = String::from_utf8_lossy(&collected).trim().to_string();
    let marker = match (text.is_empty(), truncated, interrupted) {
        (true, false, true) => "(body unreadable)",
        (_, true, _) => "…(truncated)",
        (false, false, true) => "…(read interrupted)",
        _ => return text,
    };
    if !text.is_empty() {
        text.push(' ');
    }
    text.push_str(marker);
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wasm imports panic off-wasm, so these test only the pure
    /// decoding `last_error_text` does, at its documented boundaries.
    #[test]
    fn last_error_text_decodes_what_the_host_returned() {
        assert_eq!(last_error_text(|_buf| 0), None);
        assert_eq!(last_error_text(|_buf| -2), None);

        assert_eq!(
            last_error_text(|buf| {
                buf[..5].copy_from_slice(b"hello");
                5
            }),
            Some("hello".to_string())
        );

        // A full `MAX_LAST_ERROR_BYTES` buffer, exactly at the cap.
        let full = "x".repeat(sys::MAX_LAST_ERROR_BYTES);
        assert_eq!(
            last_error_text(|buf| {
                buf.copy_from_slice(full.as_bytes());
                sys::MAX_LAST_ERROR_BYTES as i32
            }),
            Some(full.clone())
        );

        // A return past the buffer's capacity — impossible under
        // today's kernel cap, but the ABI documents the return as the
        // text's full length, not the copied length: clamp to what
        // fits, never index past the slice.
        assert_eq!(
            last_error_text(|buf| {
                buf.fill(b'x');
                (sys::MAX_LAST_ERROR_BYTES as i32) + 1000
            }),
            Some(full)
        );

        // A copy cut mid-codepoint: "€" (E2 82 AC) truncated to its
        // first two bytes — lossy decoding, not a panic.
        assert_eq!(
            last_error_text(|buf| {
                buf[..2].copy_from_slice(&"€".as_bytes()[..2]);
                2
            }),
            Some("\u{FFFD}".to_string())
        );
    }

    /// `Stream::read`'s actual code-to-outcome mapping, table-tested
    /// through the pure `classify_read` it delegates to. This is the
    /// behaviour a `PartialEq` comparison of two `Received` variants
    /// cannot pin: mutating `read` to map a 0-byte host return to
    /// `Received::End` instead of `Bytes(0)` leaves `Received`'s own
    /// derive untouched and only this table catches it.
    #[test]
    fn classify_read_maps_every_code_to_its_outcome() {
        assert_eq!(classify_read(5), Ok(Received::Bytes(5)));
        assert_eq!(classify_read(0), Ok(Received::Bytes(0)), "not End");
        assert_eq!(classify_read(sys::STREAM_EOF), Ok(Received::End));
        assert_eq!(
            classify_read(sys::STREAM_CANCELLED),
            Ok(Received::Cancelled)
        );
        // Left to the caller to decode via `StreamError::from_code`.
        assert_eq!(classify_read(sys::STREAM_CLOSED), Err(sys::STREAM_CLOSED));
        assert_eq!(
            classify_read(sys::STREAM_INVALID_HANDLE),
            Err(sys::STREAM_INVALID_HANDLE)
        );
    }

    /// `Stream::write_all`'s actual code-to-outcome mapping, table
    /// tested the same way `classify_read` is: this is the seam plan
    /// section 4's "a write released by the token part-way through
    /// `write_all`'s loop" row pins, and the only thing standing
    /// between `Delivery::Cancelled` and a silent regression back to
    /// `Delivery::Delivered`.
    #[test]
    fn classify_write_maps_every_code_to_its_outcome() {
        assert_eq!(classify_write(5), Ok(Delivery::Delivered));
        assert_eq!(classify_write(1), Ok(Delivery::Delivered));
        assert_eq!(
            classify_write(sys::STREAM_CANCELLED),
            Ok(Delivery::Cancelled)
        );
        assert_eq!(classify_write(sys::STREAM_CLOSED), Ok(Delivery::ReaderGone));
        // `ZeroCommit` needs no handle, so the caller decodes it
        // itself rather than through `StreamError::from_code`.
        assert_eq!(classify_write(0), Err(0));
        // Left to the caller to decode via `StreamError::from_code`.
        assert_eq!(
            classify_write(sys::STREAM_INVALID_HANDLE),
            Err(sys::STREAM_INVALID_HANDLE)
        );
    }

    /// The handle-free branches of `from_code`: every code but
    /// `STREAM_IO_ERROR` needs no host call, so these are testable
    /// off-wasm with a handle value that is never touched.
    #[test]
    fn from_code_decodes_every_handle_free_branch() {
        const H: i32 = 1;
        assert_eq!(
            StreamError::from_code(H, sys::STREAM_INVALID_HANDLE),
            StreamError::InvalidHandle
        );
        assert_eq!(
            StreamError::from_code(H, sys::STREAM_DIRECTION_MISMATCH),
            StreamError::DirectionMismatch
        );
        assert_eq!(
            StreamError::from_code(H, sys::STREAM_CLOSED),
            StreamError::Closed
        );
        assert_eq!(StreamError::from_code(H, -99), StreamError::Other(-99));
    }

    /// [`collect_excerpt`] at each of `read_excerpt`'s documented
    /// outcomes, driven by a fake reader instead of the wasm-only
    /// `Stream::read` — this is what "unlocks" these arms for testing.
    #[test]
    fn collect_excerpt_covers_every_documented_outcome() {
        let scripted = |mut items: std::vec::IntoIter<Result<Received, StreamError>>| {
            move |buf: &mut [u8]| match items.next().expect("script exhausted") {
                Ok(Received::Bytes(n)) => {
                    buf[..n].copy_from_slice(&b"xxxxxxxxxxxxxxxxxxxxxxxxxx"[..n]);
                    Ok(Received::Bytes(n))
                }
                other => other,
            }
        };

        // A clean, short body: exact text, no marker.
        assert_eq!(
            collect_excerpt(
                1024,
                scripted(vec![Ok(Received::Bytes(5)), Ok(Received::End)].into_iter())
            ),
            "xxxxx"
        );

        // Cancelled before any byte: indistinguishable from an empty
        // body by design (D12) — the doc above qualifies this.
        assert_eq!(
            collect_excerpt(1024, scripted(vec![Ok(Received::Cancelled)].into_iter())),
            ""
        );

        // Cancelled after some bytes: what was collected, no marker —
        // the operator's own stop is not a degraded read.
        assert_eq!(
            collect_excerpt(
                1024,
                scripted(vec![Ok(Received::Bytes(5)), Ok(Received::Cancelled)].into_iter())
            ),
            "xxxxx"
        );

        // Truncated: collected past `cap`, cut and marked.
        assert_eq!(
            collect_excerpt(
                3,
                scripted(vec![Ok(Received::Bytes(5)), Ok(Received::End)].into_iter())
            ),
            "xxx …(truncated)"
        );

        // Exactly `cap` bytes, then end: not truncated. `collected.len()
        // > cap`, not `>=`, is what this pins — a body that just fits
        // must not be falsely marked.
        assert_eq!(
            collect_excerpt(
                5,
                scripted(vec![Ok(Received::Bytes(5)), Ok(Received::End)].into_iter())
            ),
            "xxxxx"
        );

        // A read error before any byte: unreadable, not empty.
        assert_eq!(
            collect_excerpt(1024, scripted(vec![Err(StreamError::Closed)].into_iter())),
            "(body unreadable)"
        );

        // A read error after some bytes: interrupted, not silently
        // truncated.
        assert_eq!(
            collect_excerpt(
                1024,
                scripted(vec![Ok(Received::Bytes(5)), Err(StreamError::Closed)].into_iter())
            ),
            "xxxxx …(read interrupted)"
        );

        // No progress: a healthy kernel never sends this, but the
        // guard treats it like end-of-stream rather than looping.
        assert_eq!(
            collect_excerpt(1024, scripted(vec![Ok(Received::Bytes(0))].into_iter())),
            ""
        );
    }
}
