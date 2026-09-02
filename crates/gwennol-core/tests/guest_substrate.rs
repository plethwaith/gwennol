//! End-to-end proof of the milestone-3 substrate decision: the example
//! guest (`crates/sse-guest`, Rust compiled to wasm32-unknown-unknown)
//! occupies the script-runtime slot and runs the full streaming-provider
//! composition through a real kernel — a plain `chat` action whose guest
//! entry builds the vendor request and calls `io.invoke_streaming` on a
//! sibling `dataflow: true` action, whose single `long_running` guest
//! step parses a chunked server-sent-events body off `host_http.post`
//! and relays contract NDJSON to its pre-provisioned output.
//!
//! The wasm module is **built from source by this suite** — the
//! documented command is
//!
//! ```text
//! cargo build -p sse-guest --target wasm32-unknown-unknown --release
//! ```
//!
//! run via the same `cargo` that runs the tests, into its own target
//! directory so the two builds' locks never meet. No compiled blob is
//! committed anywhere; CI compiles the guest like everything else.

use std::io::Write;
use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use gwead::kernel::streams::StreamRegistry;
use gwead::kernel::{Kernel, KernelConfig};
use gwead::serde_json::{Value, json};
use gwennol_core::{HostConfig, ProcessEnv, spi};
// The guest's own exported names: the manifest and these tests name the
// plugin, its actions, and its entry points from the one declaration
// the guest crate carries, instead of retyping string literals that
// could drift from it.
use sse_guest::{ENTRY_CHAT, ENTRY_RELAY_SSE, PLUGIN_NAME as PLUGIN, STREAM_ACTION};

mod common;
use common::{Permissive, assert_conforms, contracts, drain_stream_events};

// ----------------------------------------------- building the guest

/// Compile the example guest to wasm and return the module bytes,
/// base64-encoded for the manifest. Built once per test process,
/// through the same bundler `cargo xtask bundle` and the bundled-plugin
/// suite use — the example is held to the build path real plugins get.
fn guest_wasm_base64() -> &'static str {
    static WASM: OnceLock<String> = OnceLock::new();
    WASM.get_or_init(|| {
        use base64::Engine as _;
        let crate_dir = Path::new("crates").join(PLUGIN);
        let bytes = xtask::build_guest(&xtask::workspace_root(), &crate_dir)
            .unwrap_or_else(|e| panic!("building the example guest failed: {e}"));
        base64::engine::general_purpose::STANDARD.encode(bytes)
    })
}

// ----------------------------------------------------- the manifest

/// The example plugin: guest module inline, `(script, "sse-guest")`
/// claimed, `LLM_CHAT` fulfilled, and only the reach it uses declared.
fn sse_guest_manifest() -> Value {
    json!({
        "name": PLUGIN, "version": "0.0.0",
        "description": "Milestone-3 example: a guest-backed streaming provider fixture.",
        "roles": [spi::llm_chat::ROLE],
        "permissions": [
            // Half of the two-key script-runtime authorization; the
            // other half is the embedder's trusted list at boot.
            format!("provide:step_type:script:{PLUGIN}"),
            "step_type:host_http.post",
            "network:egress:127.0.0.1"
        ],
        "wasmModules": {
            "guest": {"base64": guest_wasm_base64()}
        },
        "stepTypeImpls": [
            {"stepType": "script", "matches": PLUGIN, "wasmModule": "guest"}
        ],
        "actions": {
            (spi::llm_chat::CHAT): {
                "steps": [
                    {"id": "run", "type": "script", "params": {
                        "language": PLUGIN, "source": ENTRY_CHAT}}
                ]
            },
            (STREAM_ACTION): {
                "dataflow": true,
                "steps": [
                    {"id": "fetch", "type": "host_http.post", "params": {
                        "url": "{{$input.url}}",
                        "body": "{{$input.request}}",
                        "stream": true}},
                    {"id": "relay", "type": "script", "longRunning": true,
                     "dependsOn": ["fetch"],
                     "params": {"language": PLUGIN, "source": ENTRY_RELAY_SSE}}
                ]
            }
        }
    })
}

// ------------------------------------------------------ the fixture

struct Fixture {
    kernel: Arc<Kernel>,
    stub: &'static SseStub,
}

fn fixture() -> &'static Fixture {
    static F: OnceLock<Fixture> = OnceLock::new();
    F.get_or_init(|| {
        let workspace = tempfile::tempdir().unwrap().keep().canonicalize().unwrap();
        let mut kernel = gwennol_core::boot_with(HostConfig {
            operator: Arc::new(Permissive),
            workspace_root: workspace,
            process_env: ProcessEnv::default(),
            // The embedder half of the authorization the manifest's
            // provide: grant asks for.
            trusted_step_type_providers: vec![PLUGIN.to_string()],
            action_timeout: gwennol_core::DEFAULT_ACTION_TIMEOUT,
        })
        .unwrap();
        kernel
            .register_plugin_from_json(&sse_guest_manifest().to_string())
            .unwrap();
        Fixture {
            kernel: kernel.into_arc(),
            stub: sse_stub(),
        }
    })
}

// ---------------------------------------------------- the SSE stub

/// A vendor-side stand-in: accepts the provider's POST, records its
/// body, and answers with a server-sent-events stream written in
/// deliberately awkward splits.
struct SseStub {
    addr: std::net::SocketAddr,
    /// `(path, body)` for every request received, in arrival order.
    /// Tests run concurrently against one shared stub, so a test that
    /// asserts on a recorded request must select by a path only it
    /// uses. An unparseable body is recorded as a JSON string rather
    /// than dropped, so a broken request shows up in an assertion diff
    /// instead of vanishing.
    requests: Mutex<Vec<(String, Value)>>,
    /// Paths whose response the relay hung up on mid-way.
    hangups: Mutex<Vec<String>>,
}

/// The happy-path SSE body. One keepalive comment, a `ping` event the
/// relay must drop, a text event, a text event whose JSON spans two
/// `data:` lines and carries a multi-byte character, a tool call, the
/// terminating `end` — and a straggler event after `end` that must
/// never be relayed, because the contract makes `end` the last event of
/// every successful stream.
const HAPPY_SSE: &str = concat!(
    ": keepalive, carrying nothing\n\n",
    "event: ping\ndata: {}\n\n",
    "event: text\ndata: {\"type\":\"text\",\"text\":\"Hel\"}\n\n",
    "event: text\ndata: {\"type\":\"text\",\ndata:  \"text\":\"l\u{f4}\"}\n\n",
    "event: tool_use\n",
    "data: {\"type\":\"tool_use\",\"id\":\"call_1\",\"name\":\"echo\",\"input\":{\"message\":\"hi\"}}\n\n",
    "event: end\n",
    "data: {\"type\":\"end\",\"stop_reason\":\"end_turn\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}\n\n",
    "event: text\ndata: {\"type\":\"text\",\"text\":\"straggler after end\"}\n\n",
);

/// A stream the vendor aborts: the error event must be the last thing
/// the relay emits — the text event and `end` after it must never
/// reach the consumer.
const ERROR_SSE: &str = concat!(
    "event: text\ndata: {\"type\":\"text\",\"text\":\"so far so\"}\n\n",
    "event: error\ndata: {\"type\":\"error\",\"message\":\"vendor melted\",\"retryable\":true}\n\n",
    "event: text\ndata: {\"type\":\"text\",\"text\":\"never seen\"}\n\n",
    "event: end\ndata: {\"type\":\"end\",\"stop_reason\":\"end_turn\",\"usage\":{}}\n\n",
);

/// A stream whose second event's payload is not JSON: the relay's step
/// fails there, and the consumer must see the events so far and then
/// end-of-stream — no `end`, no `error` event, no garbage line. This is
/// the pin on the kernel's post-invocation drain closing the
/// pre-provisioned output when the long-running step errors instead of
/// closing it itself.
const GARBAGE_SSE: &str = concat!(
    "event: text\ndata: {\"type\":\"text\",\"text\":\"before the garbage\"}\n\n",
    "event: text\ndata: this is not JSON\n\n",
    "event: end\ndata: {\"type\":\"end\",\"stop_reason\":\"end_turn\",\"usage\":{}}\n\n",
);

fn sse_stub() -> &'static SseStub {
    static S: OnceLock<&'static SseStub> = OnceLock::new();
    S.get_or_init(|| {
        let listener = TcpListener::bind("127.0.0.1:0").expect("stub binds");
        let stub: &'static SseStub = Box::leak(Box::new(SseStub {
            addr: listener.local_addr().unwrap(),
            requests: Mutex::new(Vec::new()),
            hangups: Mutex::new(Vec::new()),
        }));
        common::serve(listener, move |mut socket| {
            // A wedged connection (a client that never finishes its
            // request) fails this handler loudly via a read timeout
            // instead of parking the thread forever.
            let _ = socket.set_read_timeout(Some(std::time::Duration::from_secs(10)));
            let Some((path, body)) = common::read_http_request(&mut socket) else {
                // Socket drop sends FIN/RST, so the kernel-side fetch
                // step errors loudly — the early return is the error
                // signal, not a swallow.
                return;
            };
            let parsed = gwead::serde_json::from_slice::<Value>(&body)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&body).into()));
            stub.requests.lock().unwrap().push((path.clone(), parsed));
            // The vendor said no: a non-2xx JSON answer, not an event
            // stream. Three flavours: a retryable rate limit, a
            // don't-retry client error, and a rate limit whose body
            // overflows the relay's excerpt cap (the sentinel sits past
            // the cap and must never surface).
            let http_error: Option<(&str, String)> = match path.as_str() {
                "/http-error-turn" => Some((
                    "429 Too Many Requests",
                    r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#.to_string(),
                )),
                "/http-notfound-turn" => Some((
                    "404 Not Found",
                    r#"{"error":{"type":"not_found_error","message":"no such model"}}"#.to_string(),
                )),
                "/http-conflict-turn" => Some((
                    "409 Conflict",
                    r#"{"error":{"type":"api_error","message":"try again"}}"#.to_string(),
                )),
                // One sentinel inside the excerpt's overshoot window
                // (bytes the relay reads but must trim away) and one
                // past everything it reads — a dropped truncate would
                // leak the first, a dropped cap the second.
                "/http-bloated-error-turn" => Some((
                    "429 Too Many Requests",
                    format!(
                        "{}WINDOW_SENTINEL{}TAIL_SENTINEL",
                        "B".repeat(4400),
                        "B".repeat(3600)
                    ),
                )),
                _ => None,
            };
            if let Some((status, reason)) = http_error {
                let _ = write!(
                    socket,
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    reason.len()
                );
                let _ = socket.write_all(reason.as_bytes());
                return;
            }
            let _ = socket.write_all(
                b"HTTP/1.1 200 OK\r\n\
                  Content-Type: text/event-stream\r\n\
                  Connection: close\r\n\r\n",
            );
            let _ = socket.flush();
            if path == "/slow-turn" {
                // A long, slow turn: text events at a steady pace, so a
                // consumer that hangs up does so with plenty still to
                // come. Ends when the relay closes the connection.
                for i in 0..200 {
                    let event = format!(
                        "event: text\ndata: {{\"type\":\"text\",\"text\":\"tick {i} \"}}\n\n"
                    );
                    if socket.write_all(event.as_bytes()).is_err() || socket.flush().is_err() {
                        stub.hangups.lock().unwrap().push(path.clone());
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                return;
            }
            let sse = match path.as_str() {
                "/error-turn" => ERROR_SSE,
                "/garbage-turn" => GARBAGE_SSE,
                _ => HAPPY_SSE,
            };
            // Byte-level splits that respect neither lines nor fields,
            // exercised through the whole kernel pipe; the fixture's
            // multi-byte character rides along. (Whether a split lands
            // mid-sequence depends on byte offsets — the parser's unit
            // tests pin the torn-UTF-8 guarantee deterministically.)
            // Flush + pause coaxes each slice into its own TCP segment.
            for chunk in sse.as_bytes().chunks(23) {
                if socket.write_all(chunk).is_err() {
                    // The relay hung up mid-stream (the error, garbage
                    // and happy-path-straggler turns do this by design).
                    return;
                }
                let _ = socket.flush();
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        });
        stub
    })
}

// ------------------------------------------------------- exercising

fn chat_input() -> Value {
    json!({
        "system": "be brief",
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "stream please"}]}
        ],
        "max_tokens": 64,
        "stream": true
    })
}

/// Run one streamed `chat` turn against the stub `path` and return the
/// NDJSON events, resolving the provider by role and reading the handle
/// after the action returns — the milestone-5 loop's pattern.
///
/// Bounded: a stub or kernel regression that stalls the stream fails
/// here with a named timeout instead of hanging the suite until the
/// CI job dies with no diagnostics.
async fn stream_turn_events(f: &Fixture, path: &str) -> Vec<Value> {
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        stream_turn_events_unbounded(f, path),
    )
    .await
    .unwrap_or_else(|_| panic!("streamed turn against {path} did not finish within 30s"))
}

async fn stream_turn_events_unbounded(f: &Fixture, path: &str) -> Vec<Value> {
    let provider = f
        .kernel
        .role_candidates(None, spi::llm_chat::ROLE)
        .into_iter()
        .next()
        .expect("an LLM_CHAT fulfiller");
    assert_eq!(provider, PLUGIN);

    let input = chat_input();
    assert_conforms(contracts().chat_input, &input);
    let config = json!({
        "model": "m3-fixture",
        "stream_url": format!("http://{}{path}", f.stub.addr)
    });
    let streams = Arc::new(Mutex::new(StreamRegistry::new()));
    let out = f
        .kernel
        .execute(&provider, spi::llm_chat::CHAT, input)
        .with_config(&config)
        .with_streams(streams.clone())
        .run()
        .await
        .expect("streamed chat dispatch succeeds")
        .output;

    // The shared drain validates the output shape and every event
    // against the contract schemas — the example guest is held to the
    // same wire the declarative fixture is.
    drain_stream_events(&streams, &out).await
}

// ------------------------------------------------------------ tests

/// The whole composition, end to end: guest-built request out,
/// chunk-parsed SSE back, contract NDJSON delivered through a handle
/// that outlives the `chat` invocation.
#[tokio::test(flavor = "multi_thread")]
async fn the_guest_streams_a_turn_end_to_end() {
    let f = fixture();
    let events = stream_turn_events(f, "/turn").await;

    assert_eq!(
        events,
        vec![
            json!({"type": "text", "text": "Hel"}),
            json!({"type": "text", "text": "l\u{f4}"}),
            json!({"type": "tool_use", "id": "call_1", "name": "echo",
                   "input": {"message": "hi"}}),
            json!({"type": "end", "stop_reason": "end_turn",
                   "usage": {"input_tokens": 1, "output_tokens": 2}}),
        ],
        "keepalives and pings dropped, split events reassembled, \
         multi-line data re-serialised to one line, multi-byte text \
         intact, end last"
    );
}

/// The request the vendor received is the one the guest built: the
/// `LLM_CHAT` input translated, not templated through. Selected from
/// the shared stub's log by a path only this test uses, so a dropped or
/// mangled request cannot be papered over by another test's identical
/// POST.
#[tokio::test(flavor = "multi_thread")]
async fn the_guest_builds_the_vendor_request() {
    let f = fixture();
    let events = stream_turn_events(f, "/turn-request-pin").await;
    assert_eq!(events.last().unwrap()["type"], json!("end"));

    // Everything — the expects included — happens outside the guard: a
    // panic while holding the lock would poison the mutex, and the
    // stub's handler threads would then take the whole suite down with
    // them instead of surfacing the one-line failure.
    let mine: Vec<Value> = {
        let requests = f.stub.requests.lock().unwrap();
        requests
            .iter()
            .filter(|(path, _)| path == "/turn-request-pin")
            .map(|(_, body)| body.clone())
            .collect()
    };
    assert_eq!(mine.len(), 1, "one turn sends exactly one POST: {mine:?}");
    let request = &mine[0];
    assert_eq!(request["model"], json!("m3-fixture"), "from config");
    assert_eq!(request["stream"], json!(true));
    assert_eq!(request["system"], json!("be brief"), "passed through");
    assert_eq!(request["max_tokens"], json!(64), "passed through");
    assert_eq!(
        request["messages"][0]["content"][0]["text"],
        json!("stream please")
    );
}

/// A vendor error event is relayed as the contract error event and is
/// the last event: nothing after it — not even the vendor's own
/// spurious `end` — reaches the consumer, and the stream ends without
/// an `end` event, which is the contract's failed-turn shape.
#[tokio::test(flavor = "multi_thread")]
async fn a_vendor_error_ends_the_stream_with_the_error_event_last() {
    let f = fixture();
    let events = stream_turn_events(f, "/error-turn").await;

    assert_eq!(
        events,
        vec![
            json!({"type": "text", "text": "so far so"}),
            json!({"type": "error", "message": "vendor melted", "retryable": true}),
        ]
    );
}

/// A relay step that *fails* (a non-JSON vendor payload) reaches the
/// consumer as the contract's failed-turn shape: the events so far,
/// then end-of-stream with no `end` and no `error` event. The relay
/// does not close its output on this path — this pins the kernel's
/// post-invocation drain doing it, which everything in
/// `docs/SUBSTRATE.md` about `Err` surfacing as early EOF relies on.
#[tokio::test(flavor = "multi_thread")]
async fn a_failing_relay_step_surfaces_as_early_end_of_stream() {
    let f = fixture();
    let events = stream_turn_events(f, "/garbage-turn").await;

    assert_eq!(
        events,
        vec![json!({"type": "text", "text": "before the garbage"})],
        "the garbage line is not relayed, and the vendor's later end \
         event never reaches the consumer"
    );
}

/// A non-2xx vendor answer is not swallowed into an unexplained empty
/// turn: the relay reads the error body and emits it as the contract's
/// error event — status named, vendor reason carried, rate limits
/// marked retryable — and nothing else.
#[tokio::test(flavor = "multi_thread")]
async fn an_http_error_becomes_the_contract_error_event() {
    let f = fixture();
    let events = stream_turn_events(f, "/http-error-turn").await;

    assert_eq!(
        events.len(),
        1,
        "the error event is the whole stream: {events:?}"
    );
    let event = &events[0];
    assert_eq!(event["type"], json!("error"));
    assert_eq!(event["retryable"], json!(true), "429 is worth repeating");
    let message = event["message"].as_str().unwrap();
    assert!(message.contains("429"), "names the status: {message}");
    assert!(
        message.contains("slow down"),
        "carries the vendor's reason: {message}"
    );
}

/// A plain client error is not marked worth repeating: only the
/// `retryable` classifier's true branch was pinned before, so a
/// regression to always-retryable would have sent consumers retrying
/// 401s and 404s forever without failing the suite.
#[tokio::test(flavor = "multi_thread")]
async fn a_client_error_is_not_marked_retryable() {
    let f = fixture();
    let events = stream_turn_events(f, "/http-notfound-turn").await;

    assert_eq!(
        events.len(),
        1,
        "the error event is the whole stream: {events:?}"
    );
    assert_eq!(events[0]["type"], json!("error"));
    assert_eq!(
        events[0]["retryable"],
        json!(false),
        "a 404 will fail again"
    );
    let message = events[0]["message"].as_str().unwrap();
    assert!(
        message.contains("404") && message.contains("no such model"),
        "{message}"
    );
}

/// A 409 is contention, worth repeating: the fixture answers through
/// the shared `retryable_http_status`, so this pins that the two
/// LLM_CHAT guests in the repository classify it alike — a fixture
/// that grew its own inline rule again would fail here.
#[tokio::test(flavor = "multi_thread")]
async fn a_conflict_is_marked_retryable_like_the_real_provider() {
    let f = fixture();
    let events = stream_turn_events(f, "/http-conflict-turn").await;
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["type"], json!("error"));
    assert_eq!(
        events[0]["retryable"],
        json!(true),
        "409 is worth repeating"
    );
    assert!(
        gwennol_guest::retryable_http_status(409),
        "the shared classifier agrees"
    );
}

/// An oversized error body is cut at the excerpt cap and says so — and
/// neither the bytes the relay read past the cap (the overshoot window
/// that detects truncation) nor the bytes it never read reach the
/// message, whose length is itself bounded near the cap.
#[tokio::test(flavor = "multi_thread")]
async fn an_oversized_error_body_is_truncated_with_a_marker() {
    let f = fixture();
    let events = stream_turn_events(f, "/http-bloated-error-turn").await;

    assert_eq!(
        events.len(),
        1,
        "the error event is the whole stream: {events:?}"
    );
    let message = events[0]["message"].as_str().unwrap();
    assert!(message.contains("…(truncated)"), "marks the cut: {message}");
    assert!(
        !message.contains("WINDOW_SENTINEL"),
        "read-but-past-the-cap bytes are trimmed, not kept: {message}"
    );
    assert!(
        !message.contains("TAIL_SENTINEL"),
        "nothing past what the relay reads leaks through: {message}"
    );
    // The exact ceiling: prefix + the 4 KiB excerpt cap + the marker.
    // Byte-based, and valid because the fixture body is ASCII —
    // `from_utf8_lossy` can expand a *binary* body up to 3× (each bad
    // byte becomes a 3-byte U+FFFD), so a non-ASCII fixture would need
    // this bound rethought, not just raised.
    let ceiling = "vendor answered HTTP 429: ".len() + 4096 + " …(truncated)".len();
    assert!(
        message.len() <= ceiling,
        "the message stays within the cap's ceiling ({} > {ceiling} bytes)",
        message.len()
    );
}

/// Owed by milestone 3, pinned here where the outcome is observable: a
/// consumer that hangs up mid-turn is a benign stop for the relay, not
/// a failed step. The dataflow action is driven directly so its result
/// can be seen — through `chat`'s `io.invoke_streaming` the callee's
/// outcome is only logged — with the reader dropped after the first
/// chunk and the vendor still streaming. The relay must then close
/// its upstream (the stub sees the hangup) and finish `Ok`; a relay
/// that treated the closed output as an error would resolve `Err`.
#[tokio::test(flavor = "multi_thread")]
async fn a_consumer_hanging_up_is_a_graceful_stop_for_the_relay() {
    use gwead::futures::StreamExt as _;
    let f = fixture();
    let input = json!({
        "url": format!("http://{}/slow-turn", f.stub.addr),
        "request": {"model": "m3-fixture", "stream": true, "messages": []}
    });
    let mut handle = f
        .kernel
        .execute(PLUGIN, STREAM_ACTION, input)
        .with_config(&json!({}))
        .into_dataflow_streaming_handle()
        .expect("the dataflow action streams");
    let first = tokio::time::timeout(std::time::Duration::from_secs(10), handle.output.next())
        .await
        .expect("the relay produces within 10s")
        .expect("not EOF")
        .expect("not an I/O error");
    assert!(
        String::from_utf8_lossy(&first).contains("tick"),
        "{first:?}"
    );
    // Hang up: the readable end goes away while the vendor is still
    // sending — the relay's next write finds no reader.
    drop(std::mem::replace(
        &mut handle.output,
        Box::pin(gwead::futures::stream::empty()),
    ));
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), &mut handle.result)
        .await
        .expect("the relay winds down within 10s")
        .expect("the pipeline reports");
    assert!(
        outcome.is_ok(),
        "reader-gone is a graceful stop, not a failed step: {outcome:?}"
    );
    for _ in 0..250 {
        if f.stub
            .hangups
            .lock()
            .unwrap()
            .iter()
            .any(|p| p == "/slow-turn")
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("the relay never closed its upstream: the vendor kept streaming");
}

/// The example implements only the streamed form and says so as a step
/// error, rather than returning something shaped like a buffered turn.
#[tokio::test(flavor = "multi_thread")]
async fn the_buffered_form_is_refused_with_a_readable_error() {
    let f = fixture();
    let mut input = chat_input();
    input["stream"] = json!(false);
    let err = f
        .kernel
        .execute(PLUGIN, spi::llm_chat::CHAT, input)
        .with_config(&json!({"model": "m", "stream_url": "http://127.0.0.1:1/"}))
        .run()
        .await
        .expect_err("the buffered form is not implemented");
    let msg = err.to_string();
    assert!(msg.contains("streamed form"), "names the limitation: {msg}");
}

/// gwennol-guest re-declares the six STREAM_* return codes because it
/// deliberately cannot depend on gwead — so nothing but this test keeps
/// the copies equal. A gwead renumbering would otherwise silently
/// misclassify stream errors in every guest (Closed decoded as Io turns
/// "reader hung up, wind down" into a failed step).
#[test]
fn the_guest_stream_codes_match_gwead() {
    use gwead::kernel::streams as host;
    use gwennol_guest::sys as guest;
    assert_eq!(guest::STREAM_EOF, host::STREAM_EOF);
    assert_eq!(guest::STREAM_INVALID_HANDLE, host::STREAM_INVALID_HANDLE);
    assert_eq!(
        guest::STREAM_DIRECTION_MISMATCH,
        host::STREAM_DIRECTION_MISMATCH
    );
    assert_eq!(guest::STREAM_CLOSED, host::STREAM_CLOSED);
    assert_eq!(guest::STREAM_IO_ERROR, host::STREAM_IO_ERROR);
    assert_eq!(guest::STREAM_OOB, host::STREAM_OOB);
}

/// The two-key authorization holds, first key: the manifest's
/// `provide:` grant alone does not let a plugin supply a script
/// runtime — a kernel whose embedder did not trust the plugin refuses
/// it at registration.
#[test]
fn an_untrusted_guest_plugin_is_refused_at_registration() {
    let mut kernel = Kernel::boot(KernelConfig::default()).unwrap();
    let err = kernel
        .register_plugin_from_json(&sse_guest_manifest().to_string())
        .expect_err("registration is refused without embedder trust");
    let msg = err.to_string();
    assert!(
        msg.contains("trust"),
        "the refusal names the missing trust: {msg}"
    );
}

/// …and second key: embedder trust alone is not enough either — a
/// manifest that omits its own `provide:step_type:script:<name>`
/// declaration is refused even by a kernel that trusts the plugin.
#[test]
fn a_guest_plugin_without_the_provide_grant_is_refused() {
    let mut kernel =
        Kernel::boot(KernelConfig::default().trusting_step_type_provider(PLUGIN.to_string()))
            .unwrap();
    let mut manifest = sse_guest_manifest();
    let permissions = manifest["permissions"].as_array_mut().unwrap();
    permissions.retain(|p| {
        !p.as_str()
            .is_some_and(|s| s.starts_with("provide:step_type:script:"))
    });
    let err = kernel
        .register_plugin_from_json(&manifest.to_string())
        .expect_err("registration is refused without the provide grant");
    let msg = err.to_string();
    assert!(
        msg.contains("provide"),
        "the refusal names the missing declaration: {msg}"
    );
}
