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

use std::io::{Read, Write};
use std::net::TcpListener;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use gwead::kernel::streams::{STREAM_EOF, StreamRegistry, read_async_shared};
use gwead::kernel::{Kernel, KernelConfig};
use gwead::serde_json::{Value, json};
use gwennol_core::{ApprovalRequest, Decision, Event, HostConfig, Operator, ProcessEnv, Turn, spi};

/// The example plugin's name — also its `language` selector, under the
/// substrate convention that a guest-backed plugin's module registers
/// under the plugin's own name.
const PLUGIN: &str = "sse-guest";

/// Allows everything, knows no secrets.
struct Permissive;

#[async_trait::async_trait]
impl Operator for Permissive {
    async fn approve(&self, _: ApprovalRequest) -> Decision {
        Decision::Allow
    }
    async fn secret(&self, _: &str, _: &str) -> Option<String> {
        None
    }
    fn emit(&self, _: Event) {}
    async fn input(&self) -> Option<Turn> {
        None
    }
}

// ----------------------------------------------- building the guest

/// Compile the example guest to wasm and return the module bytes,
/// base64-encoded for the manifest. Built once per test process.
fn guest_wasm_base64() -> &'static str {
    static WASM: OnceLock<String> = OnceLock::new();
    WASM.get_or_init(|| {
        use base64::Engine as _;
        let workspace = workspace_root();
        // A separate target dir: the outer `cargo test` holds the lock
        // on `target/`, and this dir has its own.
        let target_dir = workspace.join("target/wasm-guest");
        let output = std::process::Command::new(env!("CARGO"))
            .current_dir(&workspace)
            .args([
                "build",
                "-p",
                PLUGIN,
                "--target",
                "wasm32-unknown-unknown",
                "--release",
                "--locked",
                "--target-dir",
            ])
            .arg(&target_dir)
            .output()
            .expect("cargo is runnable");
        assert!(
            output.status.success(),
            "building the example guest failed (is the target installed? \
             `rustup target add wasm32-unknown-unknown`):\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let artifact = target_dir.join("wasm32-unknown-unknown/release/sse_guest.wasm");
        let bytes = std::fs::read(&artifact)
            .unwrap_or_else(|e| panic!("guest artifact missing at {}: {e}", artifact.display()));
        base64::engine::general_purpose::STANDARD.encode(bytes)
    })
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root resolves")
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
            "chat": {
                "steps": [
                    {"id": "run", "type": "script", "params": {
                        "language": PLUGIN, "source": "chat"}}
                ]
            },
            "stream_turn": {
                "dataflow": true,
                "steps": [
                    {"id": "fetch", "type": "host_http.post", "params": {
                        "url": "{{$input.url}}",
                        "body": "{{$input.request}}",
                        "stream": true}},
                    {"id": "relay", "type": "script", "longRunning": true,
                     "dependsOn": ["fetch"],
                     "params": {"language": PLUGIN, "source": "relay_sse"}}
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
    /// Request bodies received, in arrival order.
    requests: Mutex<Vec<Value>>,
}

/// The happy-path SSE body. One keepalive comment, a `ping` event the
/// relay must drop, a text event, a text event whose JSON spans two
/// `data:` lines, a tool call, and the terminating `end`.
const HAPPY_SSE: &str = concat!(
    ": keepalive, carrying nothing\n\n",
    "event: ping\ndata: {}\n\n",
    "event: text\ndata: {\"type\":\"text\",\"text\":\"Hel\"}\n\n",
    "event: text\ndata: {\"type\":\"text\",\ndata:  \"text\":\"lo\"}\n\n",
    "event: tool_use\n",
    "data: {\"type\":\"tool_use\",\"id\":\"call_1\",\"name\":\"echo\",\"input\":{\"message\":\"hi\"}}\n\n",
    "event: end\n",
    "data: {\"type\":\"end\",\"stop_reason\":\"end_turn\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}\n\n",
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

fn sse_stub() -> &'static SseStub {
    static S: OnceLock<&'static SseStub> = OnceLock::new();
    S.get_or_init(|| {
        let listener = TcpListener::bind("127.0.0.1:0").expect("stub binds");
        let stub: &'static SseStub = Box::leak(Box::new(SseStub {
            addr: listener.local_addr().unwrap(),
            requests: Mutex::new(Vec::new()),
        }));
        std::thread::spawn(move || {
            let mut consecutive_failures = 0u32;
            loop {
                let mut socket = match listener.accept() {
                    Ok((s, _)) => {
                        consecutive_failures = 0;
                        s
                    }
                    Err(e) => {
                        eprintln!("sse stub accept failed: {e}");
                        consecutive_failures += 1;
                        if consecutive_failures >= 16 {
                            return;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(50));
                        continue;
                    }
                };
                std::thread::spawn(move || {
                    let Some((path, body)) = read_request(&mut socket) else {
                        return;
                    };
                    if let Ok(parsed) = gwead::serde_json::from_slice::<Value>(&body) {
                        stub.requests.lock().unwrap().push(parsed);
                    }
                    let sse = match path.as_str() {
                        "/error-turn" => ERROR_SSE,
                        _ => HAPPY_SSE,
                    };
                    let _ = socket.write_all(
                        b"HTTP/1.1 200 OK\r\n\
                          Content-Type: text/event-stream\r\n\
                          Connection: close\r\n\r\n",
                    );
                    let _ = socket.flush();
                    // Byte-level splits that respect nothing — not
                    // lines, not fields, not UTF-8 for the parser to
                    // reassemble (its unit tests pin the strict
                    // guarantee; this exercises it through the whole
                    // kernel pipe). Flush + pause coaxes each slice
                    // into its own TCP segment.
                    for chunk in sse.as_bytes().chunks(23) {
                        if socket.write_all(chunk).is_err() {
                            // The relay hung up mid-stream (the error
                            // turn does this by design).
                            return;
                        }
                        let _ = socket.flush();
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                });
            }
        });
        stub
    })
}

/// Minimal HTTP request reader: headers, then a Content-Length body.
fn read_request(socket: &mut std::net::TcpStream) -> Option<(String, Vec<u8>)> {
    let mut raw = Vec::new();
    let mut buf = [0u8; 1024];
    let header_end = loop {
        if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        let n = socket.read(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        raw.extend_from_slice(&buf[..n]);
    };
    let head = String::from_utf8_lossy(&raw[..header_end]).into_owned();
    let path = head.split_whitespace().nth(1)?.to_string();
    let content_length: usize = head
        .lines()
        .find_map(|l| {
            let (name, value) = l.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0);
    let mut body = raw[header_end..].to_vec();
    while body.len() < content_length {
        let n = socket.read(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        body.extend_from_slice(&buf[..n]);
    }
    body.truncate(content_length);
    Some((path, body))
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
async fn stream_turn_events(f: &Fixture, path: &str) -> Vec<Value> {
    let provider = f
        .kernel
        .role_candidates(None, spi::llm_chat::ROLE)
        .into_iter()
        .next()
        .expect("an LLM_CHAT fulfiller");
    assert_eq!(provider, PLUGIN);

    let config = json!({
        "model": "m3-fixture",
        "stream_url": format!("http://{}{path}", f.stub.addr)
    });
    let streams = Arc::new(Mutex::new(StreamRegistry::new()));
    let out = f
        .kernel
        .execute(&provider, spi::llm_chat::CHAT, chat_input())
        .with_config(&config)
        .with_streams(streams.clone())
        .run()
        .await
        .expect("streamed chat dispatch succeeds")
        .output;

    let handle = out["stream"]
        .as_u64()
        .unwrap_or_else(|| panic!("chat output is the streamed form: {out}"));
    let id = NonZeroU32::new(u32::try_from(handle).unwrap()).unwrap();
    let mut collected = Vec::new();
    let mut buf = [0u8; 64];
    loop {
        let n = read_async_shared(&streams, id, &mut buf).await;
        if n == STREAM_EOF {
            break;
        }
        assert!(n > 0, "stream read returned {n}");
        collected.extend_from_slice(&buf[..n as usize]);
    }
    String::from_utf8(collected)
        .expect("NDJSON is UTF-8")
        .lines()
        .map(|l| gwead::serde_json::from_str(l).expect("one JSON document per line"))
        .collect()
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
            json!({"type": "text", "text": "lo"}),
            json!({"type": "tool_use", "id": "call_1", "name": "echo",
                   "input": {"message": "hi"}}),
            json!({"type": "end", "stop_reason": "end_turn",
                   "usage": {"input_tokens": 1, "output_tokens": 2}}),
        ],
        "keepalives and pings dropped, split events reassembled, \
         multi-line data re-serialised to one line, end last"
    );
}

/// The request the vendor received is the one the guest built: the
/// `LLM_CHAT` input translated, not templated through.
#[tokio::test(flavor = "multi_thread")]
async fn the_guest_builds_the_vendor_request() {
    let f = fixture();
    let events = stream_turn_events(f, "/turn").await;
    assert_eq!(events.last().unwrap()["type"], json!("end"));

    let requests = f.stub.requests.lock().unwrap();
    let request = requests.last().expect("the stub saw the POST").clone();
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

/// The two-key authorization holds: the manifest's `provide:` grant
/// alone does not let a plugin supply a script runtime — a kernel whose
/// embedder did not trust the plugin refuses it at registration.
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
