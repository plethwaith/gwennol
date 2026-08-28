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

    let mut parser = SseParser::new();
    let mut buf = [0u8; 4096];
    loop {
        if cancelled() {
            // Wind down without an `end` event: the consumer reads the
            // early end-of-stream as a failed turn, which a cancelled
            // turn is.
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
        for event in parser.feed(&buf[..n]) {
            match event.event.as_str() {
                // Vendor keepalive — real streams carry these and the
                // contract does not, so the relay is where they die.
                "ping" => continue,
                // The vendor reported the turn failed. The contract
                // error event is always last: emit it, stop reading,
                // and end the stream without an `end` event.
                "error" => {
                    write_ndjson_line(&output, &event.data)?;
                    upstream.close();
                    output.close();
                    return Ok(Value::Null);
                }
                _ => write_ndjson_line(&output, &event.data)?,
            }
        }
    }
    // Close explicitly so the consumer sees EOF the moment relaying is
    // done, not when the invocation's stream registry drains.
    output.close();
    Ok(Value::Null)
}

/// Re-serialise one SSE data payload as one NDJSON line.
///
/// The parse is load-bearing twice over: multi-line `data:` joins with
/// a raw newline, which NDJSON cannot carry, so compact re-serialisation
/// is what restores one-event-one-line — and a payload that is not JSON
/// at all fails the step here, surfacing to the consumer as an early
/// end-of-stream rather than as a garbage line it would have to parse to
/// distrust.
fn write_ndjson_line(output: &Stream, data: &str) -> Result<(), String> {
    let value: Value = serde_json::from_str(data)
        .map_err(|e| format!("vendor event payload is not JSON: {e}; payload: {data:?}"))?;
    let mut line = serde_json::to_string(&value).map_err(|e| e.to_string())?;
    line.push('\n');
    output
        .write_all(line.as_bytes())
        .map_err(|e| format!("output write failed: {e}"))
}

entrypoints! {
    "chat" => chat,
    "relay_sse" => relay_sse,
}
