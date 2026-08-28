//! The milestone-3 example guest: a chat-shaped plugin whose
//! non-declarative work — building a JSON request, parsing a chunked
//! server-sent-events body — runs as Rust compiled to wasm32, occupying
//! the script-runtime slot (see `docs/SUBSTRATE.md`).
//!
//! Two entry points, matching the two halves of the streaming-provider
//! composition the roadmap's milestone-3 constraint records:
//!
//! - `chat` runs in the plain `chat` action: it translates the
//!   `LLM_CHAT` input into a vendor-shaped JSON request, dispatches the
//!   sibling dataflow action with `invoke_streaming`, and returns the
//!   resulting handle as the contract's `{"stream": n}` output.
//! - `relay_sse` runs as the single `long_running` step of the
//!   `stream_turn` dataflow action: it reads the SSE bytes a
//!   `host_http.post` step fetched, and writes one contract NDJSON
//!   event per SSE event to its pre-provisioned output.
//!
//! The "vendor" wire here is deliberately thin — its SSE `data:`
//! payloads are already contract-shaped events — because the fixture's
//! job is to prove the substrate, not to translate a real provider's
//! protocol (that is milestone 4). The parsing is real: framing,
//! chunk-boundary reassembly, multi-line `data:` joining, keepalive
//! filtering, and NDJSON-safe re-serialisation.

use gwennol_guest::{Args, Stream, Target, cancelled, entrypoints, invoke_streaming};
use serde_json::{Value, json};

pub mod sse;

use sse::SseParser;

/// The plugin this module ships inside. Guest code and manifest are two
/// halves of one artifact, so the name is a constant, not
/// configuration: it is how `chat` addresses its sibling action
/// (self-invocation needs no grant) and it doubles as the manifest's
/// `language` selector under the substrate's naming convention.
pub const PLUGIN_NAME: &str = "sse-guest";

/// The sibling `dataflow: true` action `chat` dispatches.
pub const STREAM_ACTION: &str = "stream_turn";

/// Entry-point name the manifest's `chat` action selects via `source`.
pub const ENTRY_CHAT: &str = "chat";

/// Entry-point name the dataflow action's `long_running` step selects.
pub const ENTRY_RELAY_SSE: &str = "relay_sse";

/// `chat` — the plain action's entry point.
fn chat(args: Args) -> Result<Value, String> {
    if args.field("stream").and_then(Value::as_bool) != Some(true) {
        return Err(
            "the sse-guest example implements only the streamed form; pass \"stream\": true"
                .to_string(),
        );
    }
    let messages = args
        .field("messages")
        .cloned()
        .ok_or("chat input carries no messages")?;

    // The vendor-shaped request body. Building it here — rather than in
    // a declarative template — is the point: this is the work that
    // needs code.
    let mut request = json!({
        "model": args.config().get("model").cloned().unwrap_or(json!("fixture-model")),
        "stream": true,
        "messages": messages,
    });
    for passthrough in ["system", "tools", "max_tokens"] {
        if let Some(v) = args.field(passthrough) {
            request[passthrough] = v.clone();
        }
    }

    let url = args
        .config()
        .get("stream_url")
        .and_then(Value::as_str)
        .ok_or("config.stream_url must name the vendor SSE endpoint")?;

    let stream = invoke_streaming(
        Target::Plugin(PLUGIN_NAME),
        STREAM_ACTION,
        &json!({ "url": url, "request": request }),
    )?;
    // The handle was minted in this invocation's stream table, so it is
    // exactly what the contract's streamed output form carries.
    Ok(json!({ "stream": stream.handle() }))
}

/// How much of a non-2xx response body is read into the error event's
/// message. Vendor error bodies are small JSON documents; the cap only
/// keeps a broken endpoint from ballooning the message.
const ERROR_BODY_CAP: usize = 4096;

/// `relay_sse` — the `long_running` dataflow step's entry point.
fn relay_sse(args: Args) -> Result<Value, String> {
    let upstream = args
        .step_result("fetch")
        .and_then(|r| r.get("body"))
        .and_then(Value::as_i64)
        .and_then(|h| i32::try_from(h).ok())
        .and_then(Stream::from_handle)
        .ok_or("the fetch step produced no streamed body handle")?;
    let output = Stream::output()
        .ok_or("relay_sse must run as the long_running step of a dataflow action")?;

    // A non-2xx answer is not an event stream: its body is the vendor's
    // reason (a rate-limit or auth error, typically), and discarding it
    // would make every HTTP-level failure an unexplained empty turn.
    // This is exactly the contract's "error event when the provider can
    // still say why" case, so say why.
    let status = args
        .step_meta("fetch", "status")
        .and_then(Value::as_i64)
        .ok_or("the fetch step recorded no HTTP status")?;
    if !(200..300).contains(&status) {
        let reason = read_capped(&upstream, ERROR_BODY_CAP);
        let event = json!({
            "type": "error",
            "message": format!("vendor answered HTTP {status}: {reason}"),
            // Timeouts, rate limits and server-side failures are worth
            // repeating unchanged; the other 4xx will fail again.
            "retryable": matches!(status, 408 | 429) || (500..=599).contains(&status),
        });
        let _ = emit(&output, &event)?; // reader-gone changes nothing here
        upstream.close();
        output.close();
        return Ok(Value::Null);
    }

    let mut parser = SseParser::new();
    let mut buf = [0u8; 4096];
    loop {
        if cancelled() {
            // Wind down without an `end` event: the consumer reads the
            // early end-of-stream as a failed turn, which a cancelled
            // turn is. Polling here only catches cancellation between
            // chunks — a read blocked on a stalled vendor ends when the
            // fetch's streaming idle timeout ends the body, which ends
            // the read (a dataflow callee carries no wallclock deadline
            // of its own under the kernel's defaults).
            upstream.close();
            output.close();
            return Ok(Value::Null);
        }
        let n = upstream
            .read(&mut buf)
            .map_err(|e| format!("upstream read failed: {e}"))?;
        if n == 0 {
            break; // vendor end-of-stream
        }
        for event in parser.feed(&buf[..n])? {
            match event.event.as_str() {
                // Vendor keepalive — real streams carry these and the
                // contract does not, so the relay is where they die.
                "ping" => continue,
                // Both terminal events: the contract makes `error` and
                // `end` each the last event of its kind of turn, so the
                // relay enforces it — emit, stop reading, close. A
                // vendor straggler after either (trailing diagnostics,
                // a retry artifact) is never relayed.
                "error" | "end" => {
                    let _ = emit_data(&output, &event.data)?;
                    upstream.close();
                    output.close();
                    return Ok(Value::Null);
                }
                _ => match emit_data(&output, &event.data)? {
                    Delivery::Delivered => {}
                    // The consumer closed its handle: a benign hangup,
                    // not a failure — the same wind-down as
                    // cancellation, not a failed step.
                    Delivery::ReaderGone => {
                        upstream.close();
                        output.close();
                        return Ok(Value::Null);
                    }
                },
            }
        }
    }
    // Close explicitly so the consumer sees EOF the moment relaying is
    // done, not when the invocation's stream registry drains.
    output.close();
    Ok(Value::Null)
}

/// What became of one emitted NDJSON line.
enum Delivery {
    /// The consumer took it.
    Delivered,
    /// The consumer has closed its handle —
    /// [`gwennol_guest::StreamError::Closed`], which the stream docs
    /// call "the normal way to learn the reader stopped listening".
    /// Wind down; don't treat it as a failure.
    ReaderGone,
}

/// Re-serialise one SSE data payload as one NDJSON line on `output`.
///
/// The parse is load-bearing twice over: multi-line `data:` joins with
/// a raw newline, which NDJSON cannot carry, so compact re-serialisation
/// is what restores one-event-one-line — and a payload that is not JSON
/// at all fails the step here, surfacing to the consumer as an early
/// end-of-stream rather than as a garbage line it would have to parse to
/// distrust.
fn emit_data(output: &Stream, data: &str) -> Result<Delivery, String> {
    let value: Value = serde_json::from_str(data)
        .map_err(|e| format!("vendor event payload is not JSON: {e}; payload: {data:?}"))?;
    emit(output, &value)
}

/// Write one JSON value as one NDJSON line, folding the consumer's
/// hangup into [`Delivery::ReaderGone`] instead of an error.
fn emit(output: &Stream, value: &Value) -> Result<Delivery, String> {
    let mut line = serde_json::to_string(value).map_err(|e| e.to_string())?;
    line.push('\n');
    match output.write_all(line.as_bytes()) {
        Ok(()) => Ok(Delivery::Delivered),
        Err(gwennol_guest::StreamError::Closed) => Ok(Delivery::ReaderGone),
        Err(e) => Err(format!("output write failed: {e}")),
    }
}

/// Read a stream as lossy UTF-8, trimmed and truncated to `cap` bytes
/// — the error-body excerpt for a non-2xx answer. (Up to one extra
/// chunk is consumed past the cap; that overshoot is how truncation is
/// detected, and is trimmed away.) A truncated excerpt says so, and a
/// body that could not be read at all says that instead of posing as
/// empty: the HTTP status alone is still worth reporting.
fn read_capped(stream: &Stream, cap: usize) -> String {
    let mut collected = Vec::new();
    let mut buf = [0u8; 1024];
    let mut unreadable = false;
    while collected.len() <= cap {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => collected.extend_from_slice(&buf[..n]),
            Err(_) => {
                unreadable = true;
                break;
            }
        }
    }
    let truncated = collected.len() > cap;
    collected.truncate(cap);
    let mut text = String::from_utf8_lossy(&collected).trim().to_string();
    if truncated {
        text.push_str(" …(truncated)");
    }
    if unreadable && text.is_empty() {
        text = "(error body unreadable)".to_string();
    }
    text
}

entrypoints! {
    ENTRY_CHAT => chat,
    ENTRY_RELAY_SSE => relay_sse,
}
