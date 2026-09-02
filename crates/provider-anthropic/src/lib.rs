//! The bundled Anthropic provider: `LLM_CHAT` over the Messages API.
//!
//! Two halves of one plugin. The manifest,
//! `plugins/providers/anthropic.json`, owns everything that reaches
//! outside the sandbox: the one `host_http.post` step per turn shape,
//! the URL, the `x-api-key` header templated from the plugin's secret,
//! the egress grant. This crate owns everything that needs code — and
//! it never sees the key: the guest builds the request *body*, and the
//! declarative steps put the credential on the wire.
//!
//! Two entry points, matching the streaming-provider composition
//! `docs/SUBSTRATE.md` records:
//!
//! - [`ENTRY_CHAT`] runs in the plain `chat` action. It translates the
//!   contract's input into a Messages API request, then either
//!   dispatches the [`STREAM_ACTION`] dataflow action and returns its
//!   handle as `{"stream": n}`, or dispatches [`FETCH_ACTION`] and
//!   translates the buffered answer into the contract's message — or
//!   its `Failure`, when the vendor said no.
//! - [`ENTRY_RELAY`] runs as the `long_running` step of the dataflow
//!   action: it reads the Messages API's server-sent events off the
//!   fetch step's stream handle and writes contract NDJSON events to
//!   its pre-provisioned output.
//!
//! The translation itself — request shaping, the stream state machine,
//! the buffered-response and failure mapping — is the pure [`wire`]
//! module, unit-tested on the host target. This file is the glue.

use gwennol_guest::sse::SseParser;
use gwennol_guest::{
    Args, Delivery, Level, Stream, Target, cancelled, entrypoints, invoke, invoke_streaming, log,
};
use serde_json::{Value, json};

pub mod wire;

use wire::{Emitted, StreamTranslator};

/// The plugin this module ships inside — its manifest `name`, the
/// `language` selector of its script steps, and how the guest addresses
/// its own sibling actions.
pub const PLUGIN_NAME: &str = "provider-anthropic";

/// The `dataflow: true` action that fetches a streamed turn and relays
/// it; dispatched by `chat` with `invoke_streaming`.
pub const STREAM_ACTION: &str = "stream_turn";

/// The plain action that fetches a buffered turn; dispatched by `chat`
/// with `invoke`. Returns `{status, body, truncated}` from its
/// `host_http.post` step.
pub const FETCH_ACTION: &str = "fetch_turn";

/// Entry-point name the manifest's `chat` action selects via `source`.
pub const ENTRY_CHAT: &str = "chat";

/// Entry-point name the dataflow action's `long_running` step selects.
pub const ENTRY_RELAY: &str = "relay_sse";

/// The step id of the `host_http.post` step in both fetching actions —
/// what the relay reads its handle and status from.
pub const FETCH_STEP: &str = "fetch";

/// How much of a non-2xx response body is read into the failure's
/// message. Vendor error bodies are small JSON documents; the cap only
/// keeps a broken endpoint from ballooning the message.
const ERROR_BODY_CAP: usize = 4096;

/// `chat` — the plain action's entry point.
fn chat(args: Args) -> Result<Value, String> {
    let request = wire::build_request(args.raw(), args.config())?;
    // Anything left out of the request is logged, so a vendor refusal
    // of the replayed history has a breadcrumb in the host's tracing.
    for dropped in &request.dropped {
        log(Level::Warn, dropped);
    }
    // The endpoint is computed here (trailing slashes and proxy path
    // prefixes normalised) and handed to the fetching action as input;
    // which host it may reach is the manifest's egress grant, not this.
    let url = wire::messages_url(args.config())?;
    if request.stream {
        let stream = invoke_streaming(
            Target::Plugin(PLUGIN_NAME),
            STREAM_ACTION,
            &json!({ "url": url, "request": request.body }),
        )?;
        // The handle was minted in this invocation's stream table, so it
        // is exactly what the contract's streamed output form carries.
        return Ok(json!({ "stream": stream.handle() }));
    }
    let answer = invoke(
        Target::Plugin(PLUGIN_NAME),
        FETCH_ACTION,
        &json!({ "url": url, "request": request.body }),
    )?;
    let status = answer
        .get("status")
        .and_then(Value::as_i64)
        .ok_or("fetch_turn returned no HTTP status")?;
    let body = answer
        .get("body")
        .and_then(Value::as_str)
        .ok_or("fetch_turn returned no body text")?;
    let truncated = answer
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let translated = wire::buffered_output(status, body, truncated);
    for note in &translated.notes {
        log(Level::Warn, note);
    }
    Ok(translated.output)
}

/// `relay_sse` — the `long_running` dataflow step's entry point.
fn relay_sse(args: Args) -> Result<Value, String> {
    let upstream = args
        .step_result(FETCH_STEP)
        .and_then(|r| r.get("body"))
        .and_then(Value::as_i64)
        .and_then(|h| i32::try_from(h).ok())
        .and_then(Stream::from_handle)
        .ok_or("the fetch step produced no streamed body handle")?;
    let output = Stream::output()
        .ok_or("relay_sse must run as the long_running step of a dataflow action")?;
    let status = args
        .step_meta(FETCH_STEP, "status")
        .and_then(Value::as_i64)
        .ok_or("the fetch step recorded no HTTP status")?;

    // A non-2xx answer is not an event stream: its body is the vendor's
    // reason, and it becomes the contract's error event — the same
    // failure the buffered path reports as data.
    if !(200..300).contains(&status) {
        let body = upstream.read_excerpt(ERROR_BODY_CAP);
        let event = wire::http_failure(status, &body).into_stream_event();
        let _ = emit(&output, &event)?; // reader-gone changes nothing here
        upstream.close();
        output.close();
        return Ok(Value::Null);
    }

    let mut parser = SseParser::new();
    let mut translator = StreamTranslator::new();
    let mut buf = [0u8; 4096];
    loop {
        if cancelled() {
            // Wind down without an `end` event: the consumer reads the
            // early end-of-stream as a failed turn, which a cancelled
            // turn is. Polling here catches cancellation between chunks;
            // a read blocked on a stalled vendor ends when the fetch's
            // streaming idle timeout ends the body.
            upstream.close();
            output.close();
            return Ok(Value::Null);
        }
        let n = upstream
            .read(&mut buf)
            .map_err(|e| format!("upstream read failed: {e}"))?;
        if n == 0 {
            // Vendor end-of-stream before `message_stop`: the turn
            // failed with the cause lost, and the contract's shape for
            // that is end-of-stream without `end` — so just close.
            break;
        }
        // A byte stream that is not an event stream at all (the parser's
        // line and event caps) fails the step; the consumer sees early
        // end-of-stream, as for any relay failure.
        for event in parser.feed(&buf[..n])? {
            let emitted = translator.accept(&event.event, &event.data);
            for note in translator.take_notes() {
                log(Level::Warn, &note);
            }
            for emitted in emitted {
                let (value, terminal) = match emitted {
                    Emitted::Event(v) => (v, false),
                    Emitted::Terminal(v) => (v, true),
                };
                match emit(&output, &value)? {
                    Delivery::Delivered if !terminal => {}
                    // The contract makes `end` and `error` each the last
                    // event of its kind of turn, so the relay enforces
                    // it: emit, stop reading, close. A vendor straggler
                    // after either is never relayed. And the consumer
                    // closing its handle is a benign hangup: the same
                    // wind-down, not a failed step.
                    Delivery::Delivered | Delivery::ReaderGone => {
                        upstream.close();
                        output.close();
                        return Ok(Value::Null);
                    }
                }
            }
        }
    }
    output.close();
    Ok(Value::Null)
}

/// Write one JSON value as one NDJSON line; the consumer's hangup is
/// [`Delivery::ReaderGone`], not an error.
fn emit(output: &Stream, value: &Value) -> Result<Delivery, String> {
    output
        .write_json_line(value)
        .map_err(|e| format!("output write failed: {e}"))
}

entrypoints! {
    ENTRY_CHAT => chat,
    ENTRY_RELAY => relay_sse,
}
