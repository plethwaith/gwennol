//! Milestone 4, end to end: the committed manifests under `plugins/`
//! — the Anthropic provider and the four tools — bundled by the same
//! code `cargo xtask bundle` runs, registered on a real kernel, and
//! driven against a stub that speaks the Messages API.
//!
//! The roadmap's done-when, each pinned here: the provider streams a
//! response against a stub HTTP server (and answers a buffered one);
//! every tool manifest declares only the host step types it actually
//! uses; and a model-issued tool call executes end to end against the
//! stubbed provider — opening turn, tool dispatch by harvested
//! descriptor, result rendered by the shared convention and carried
//! back, closing turn.
//!
//! The provider's egress grant names `api.anthropic.com`; the suite
//! adds `127.0.0.1` at registration so the stub is reachable, after
//! pinning that the committed manifest declares exactly what it ships
//! with and nothing more.

use std::io::Write;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use gwead::kernel::Kernel;
use gwead::kernel::streams::StreamRegistry;
use gwead::serde_json::{Value, json};
use gwennol_core::{ApprovalRequest, Decision, Event, HostConfig, Operator, ProcessEnv, Turn, spi};
use provider_anthropic::wire::ANTHROPIC_VERSION;
use provider_anthropic::{
    ENTRY_CHAT, ENTRY_RELAY, FETCH_ACTION, PLUGIN_NAME as PROVIDER, STREAM_ACTION,
};

mod common;
use common::{assert_conforms, contracts, drain_stream_events};

// ------------------------------------------------------------ operator

/// The key the provider's `api_key` secret resolves to.
const API_KEY: &str = "sk-ant-test-fixture";

/// Allows everything; knows exactly one secret, for exactly one plugin.
struct Keyed;

#[async_trait::async_trait]
impl Operator for Keyed {
    async fn approve(&self, _: ApprovalRequest) -> Decision {
        Decision::Allow
    }
    async fn secret(&self, plugin: &str, name: &str) -> Option<String> {
        (plugin == PROVIDER && name == "api_key").then(|| API_KEY.to_string())
    }
    fn emit(&self, _: Event) {}
    async fn input(&self) -> Option<Turn> {
        None
    }
}

// ------------------------------------------------------------- fixture

struct Fixture {
    kernel: Arc<Kernel>,
    workspace: PathBuf,
    stub: &'static Stub,
    bundled: Vec<xtask::BundledPlugin>,
}

fn fixture() -> &'static Fixture {
    static F: OnceLock<Fixture> = OnceLock::new();
    F.get_or_init(|| {
        let workspace = tempfile::tempdir().unwrap().keep().canonicalize().unwrap();
        std::fs::write(workspace.join("hello.txt"), "hello from the workspace\n").unwrap();
        let bundled = xtask::bundle(&xtask::workspace_root())
            .unwrap_or_else(|e| panic!("bundling plugins/ failed: {e}"));
        let mut kernel = gwennol_core::boot_with(HostConfig {
            operator: Arc::new(Keyed),
            workspace_root: workspace.clone(),
            process_env: ProcessEnv::default(),
            trusted_step_type_providers: vec![PROVIDER.to_string()],
        })
        .unwrap();
        for plugin in &bundled {
            let mut manifest = plugin.manifest.clone();
            if plugin.name() == PROVIDER {
                // The stub is local; the shipped grant is not. Added
                // here, after the shipped set is pinned below.
                manifest["permissions"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!("network:egress:127.0.0.1"));
            }
            kernel
                .register_plugin_from_json(&manifest.to_string())
                .unwrap_or_else(|e| panic!("{}: {e}", plugin.relative_path.display()));
        }
        Fixture {
            kernel: kernel.into_arc(),
            workspace,
            stub: stub(),
            bundled,
        }
    })
}

impl Fixture {
    /// The provider `$config` for a stub route.
    fn config(&self, route: &str) -> Value {
        json!({
            "model": "claude-fixture",
            "base_url": format!("http://{}{route}", self.stub.addr),
        })
    }

    /// The bundled tools as the model sees them.
    fn tools(&self) -> Value {
        Value::Array(
            spi::harvest_tools(&self.kernel)
                .unwrap()
                .into_iter()
                .map(|d| {
                    json!({"name": d.tool_name, "description": d.description,
                           "input_schema": d.parameters})
                })
                .collect(),
        )
    }

    /// Run one tool call the milestone-5 way: by harvested descriptor.
    async fn call_tool(&self, name: &str, input: Value) -> Value {
        let descriptors = spi::harvest_tools(&self.kernel).unwrap();
        let d = descriptors
            .iter()
            .find(|d| d.tool_name == name)
            .unwrap_or_else(|| panic!("no tool named {name}"));
        let out = self
            .kernel
            .execute(&d.plugin_key, &d.action_name, input)
            .with_config(&json!({}))
            .run()
            .await
            .unwrap_or_else(|e| panic!("tool {name} failed as a step: {e}"))
            .output;
        assert_conforms(contracts().call_output, &out);
        out
    }

    /// One buffered provider turn.
    async fn buffered(&self, route: &str, input: Value) -> Value {
        assert_conforms(contracts().chat_input, &input);
        let out = self
            .kernel
            .execute(PROVIDER, spi::llm_chat::CHAT, input)
            .with_config(&self.config(route))
            .run()
            .await
            .expect("a buffered turn the vendor answered is never a step error")
            .output;
        assert_conforms(contracts().chat_output, &out);
        out
    }

    /// One streamed provider turn, drained to its events.
    async fn streamed(&self, route: &str, input: Value) -> Vec<Value> {
        assert_conforms(contracts().chat_input, &input);
        let streams = Arc::new(Mutex::new(StreamRegistry::new()));
        let out = self
            .kernel
            .execute(PROVIDER, spi::llm_chat::CHAT, input)
            .with_config(&self.config(route))
            .with_streams(streams.clone())
            .run()
            .await
            .expect("streamed dispatch succeeds")
            .output;
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            drain_stream_events(&streams, &out),
        )
        .await
        .expect("the streamed turn finishes within 30s")
    }
}

fn opening_input(tools: Value, stream: bool) -> Value {
    json!({
        "system": "You are terse.",
        "messages": [{"role": "user", "content": [{"type": "text", "text": "What does hello.txt say?"}]}],
        "tools": tools,
        "max_tokens": 200,
        "stream": stream
    })
}

// ---------------------------------------------------------------- stub

/// A Messages API stand-in. Routes by the URL prefix `$config.base_url`
/// contributes, then by whether the request is a follow-up carrying a
/// tool result. Records every request for the pins.
struct Stub {
    addr: std::net::SocketAddr,
    /// `(path, headers, body)` in arrival order.
    requests: Mutex<Vec<(String, Value, Value)>>,
}

/// The opening answer: text, then a `read` call on hello.txt whose
/// input JSON arrives in two fragments — the stream's "buffer and emit
/// whole" rule under test.
fn opening_sse() -> String {
    [
        r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"model":"claude-fixture","stop_reason":null,"usage":{"input_tokens":12,"output_tokens":1,"cache_read_input_tokens":0}}}

event: ping
data: {"type": "ping"}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Read "}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"first."}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-01"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Let me "}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"read it."}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: content_block_start
data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_01","name":"read","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"path\": \"hel"}}

event: content_block_delta
data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"lo.txt\"}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":2}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":17}}

event: message_stop
data: {"type":"message_stop"}

"#,
    ]
    .concat()
}

/// The thinking block the vendor produces before its tool call — the
/// thing the vendor requires replayed verbatim on the next turn. The
/// stream builds the same block from its deltas.
const THINKING_BLOCK: &str =
    r#"{"type": "thinking", "thinking": "Read first.", "signature": "sig-01"}"#;

fn thinking_block() -> Value {
    gwead::serde_json::from_str(THINKING_BLOCK).unwrap()
}

fn opaque_block() -> Value {
    json!({"type": "opaque", "provider": PROVIDER, "data": thinking_block()})
}

fn opening_json() -> Value {
    json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "claude-fixture",
        "content": [
            thinking_block(),
            {"type": "text", "text": "Let me read it."},
            {"type": "tool_use", "id": "toolu_01", "name": "read", "input": {"path": "hello.txt"}}
        ],
        "stop_reason": "tool_use", "stop_sequence": null,
        "usage": {"input_tokens": 12, "output_tokens": 17}
    })
}

/// The closing answer quotes the tool result it was given, so the
/// round trip is provable from the model's side.
fn closing_json(quoted: &str) -> Value {
    json!({
        "id": "msg_2", "type": "message", "role": "assistant", "model": "claude-fixture",
        "content": [{"type": "text", "text": format!("It says: {quoted}")}],
        "stop_reason": "end_turn", "stop_sequence": null,
        "usage": {"input_tokens": 40, "output_tokens": 9}
    })
}

/// The first tool result in a request's last message, if it is a
/// follow-up turn.
fn tool_result_in(body: &Value) -> Option<String> {
    let last = body.get("messages")?.as_array()?.last()?;
    let block = last.get("content")?.as_array()?.first()?;
    (block.get("type")?.as_str()? == "tool_result")
        .then(|| block.get("content")?.as_str().map(str::to_string))
        .flatten()
}

fn stub() -> &'static Stub {
    static S: OnceLock<&'static Stub> = OnceLock::new();
    S.get_or_init(|| {
        let listener = TcpListener::bind("127.0.0.1:0").expect("stub binds");
        let stub: &'static Stub = Box::leak(Box::new(Stub {
            addr: listener.local_addr().unwrap(),
            requests: Mutex::new(Vec::new()),
        }));
        common::serve(listener, move |mut socket| {
            let _ = socket.set_read_timeout(Some(std::time::Duration::from_secs(10)));
            let Some((path, headers, body)) = common::read_http_request_with_headers(&mut socket)
            else {
                return;
            };
            let parsed: Value = gwead::serde_json::from_slice(&body)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&body).into()));
            stub.requests
                .lock()
                .unwrap()
                .push((path.clone(), headers.clone(), parsed.clone()));
            let streaming = parsed.get("stream").and_then(Value::as_bool) == Some(true);
            let (route, _) = path.split_once("/v1/messages").unwrap_or((path.as_str(), ""));
            match route {
                "/rate-limited" => respond(
                    &mut socket,
                    "429 Too Many Requests",
                    "application/json",
                    r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#,
                ),
                "/bad-key" => respond(
                    &mut socket,
                    "401 Unauthorized",
                    "application/json",
                    r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#,
                ),
                "/overloaded-midstream" => stream(
                    &mut socket,
                    concat!(
                        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
                        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"so far\"}}\n\n",
                        "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
                        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
                    ),
                ),
                _ => match (tool_result_in(&parsed), streaming) {
                    (Some(quoted), false) => respond(
                        &mut socket,
                        "200 OK",
                        "application/json",
                        &closing_json(&quoted).to_string(),
                    ),
                    (None, false) => respond(
                        &mut socket,
                        "200 OK",
                        "application/json",
                        &opening_json().to_string(),
                    ),
                    (_, true) => stream(&mut socket, &opening_sse()),
                },
            }
        });
        stub
    })
}

fn respond(socket: &mut std::net::TcpStream, status: &str, content_type: &str, body: &str) {
    let _ = write!(
        socket,
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    let _ = socket.write_all(body.as_bytes());
}

/// Send an event stream in 23-byte slices with a flush and a pause
/// between, so the relay sees splits that respect neither lines nor
/// fields.
fn stream(socket: &mut std::net::TcpStream, sse: &str) {
    let _ = socket.write_all(
        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
    );
    let _ = socket.flush();
    for chunk in sse.as_bytes().chunks(23) {
        if socket.write_all(chunk).is_err() {
            return;
        }
        let _ = socket.flush();
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

// ---------------------------------------------------- the manifests

/// The committed provider manifest declares exactly the reach it ships
/// with — the guest slot, one host step, one host — and names its
/// guest by crate path, a form the kernel refuses: the file in the
/// repository is honestly not registrable without bundling.
#[test]
fn the_committed_provider_manifest_declares_its_reach_and_needs_bundling() {
    let workspace = xtask::workspace_root();
    let raw: Value = gwead::serde_json::from_str(
        &std::fs::read_to_string(workspace.join("plugins/providers/anthropic.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(raw["name"], PROVIDER);
    assert_eq!(raw["roles"], json!([spi::llm_chat::ROLE]));
    assert_eq!(
        raw["permissions"],
        json!([
            format!("provide:step_type:script:{PROVIDER}"),
            "step_type:host_http.post",
            "network:egress:api.anthropic.com"
        ])
    );
    assert_eq!(raw["usesSecrets"], json!(["api_key"]));
    assert_eq!(
        raw["wasmModules"]["guest"],
        json!({"path": format!("crates/{PROVIDER}")})
    );
    // The names the manifest uses are the ones the guest crate exports.
    assert_eq!(raw["stepTypeImpls"][0]["matches"], PROVIDER);
    assert_eq!(
        raw["actions"][spi::llm_chat::CHAT]["steps"][0]["params"]["source"],
        ENTRY_CHAT
    );
    assert_eq!(
        raw["actions"][STREAM_ACTION]["steps"][1]["params"]["source"],
        ENTRY_RELAY
    );
    assert!(raw["actions"][FETCH_ACTION].is_object());
    for action in [FETCH_ACTION, STREAM_ACTION] {
        let headers = &raw["actions"][action]["steps"][0]["params"]["headers"];
        assert_eq!(headers["anthropic-version"], ANTHROPIC_VERSION, "{action}");
        assert_eq!(headers["x-api-key"], "{{$secrets.api_key}}", "{action}");
    }
    // The key reaches the wire from a declarative step only: no script
    // step admits any secret into the guest.
    for (name, action) in raw["actions"].as_object().unwrap() {
        for step in action["steps"].as_array().unwrap() {
            if step["type"] == "script" {
                assert!(
                    step.get("passSecrets").is_none(),
                    "{name}: the guest sees no secret"
                );
            }
        }
    }

    let mut kernel = gwead::kernel::Kernel::boot(
        gwead::kernel::KernelConfig::default().trusting_step_type_provider(PROVIDER.to_string()),
    )
    .unwrap();
    spi::register(&mut kernel).unwrap();
    let err = kernel
        .register_plugin_from_json(&raw.to_string())
        .expect_err("the committed manifest is not registrable");
    assert!(err.to_string().contains("path-based"), "{err}");
}

/// Every step type used by a step list, gathered through every nesting
/// Gwead's own walker descends: ifs branches, try/catch/finally bodies,
/// loop bodies, parallel branches (each an array of steps).
fn step_types(steps: &Value, into: &mut std::collections::BTreeSet<String>) {
    for step in steps.as_array().into_iter().flatten() {
        if let Some(t) = step["type"].as_str() {
            into.insert(t.to_string());
        }
        for branch in step["params"]["ifs"].as_array().into_iter().flatten() {
            step_types(&branch["then"], into);
        }
        for key in ["try", "catch", "finally", "steps"] {
            step_types(&step["params"][key], into);
        }
        for branch in step["params"]["branches"].as_array().into_iter().flatten() {
            step_types(branch, into);
        }
    }
}

/// The walker the grants pin relies on reaches every nesting — pinned
/// on a synthetic tree, since no bundled manifest yet nests a host
/// step under `parallel`, `try` or a loop, and a walker arm that never
/// runs would otherwise be an untested promise.
#[test]
fn the_step_walker_reaches_every_nesting() {
    let tree = json!([
        {"id": "a", "type": "host_fs.read", "params": {}},
        {"id": "b", "type": "ifs", "params": {"ifs": [
            {"test": "true", "then": [{"id": "c", "type": "host_fs.write", "params": {}}]},
            {"then": [{"id": "d", "type": "return", "params": {}}]}
        ]}},
        {"id": "e", "type": "try", "params": {
            "try": [{"id": "f", "type": "host_process.run", "params": {}}],
            "catch": [{"id": "g", "type": "host_http.get", "params": {}}],
            "finally": [{"id": "h", "type": "host_http.post", "params": {}}]
        }},
        {"id": "i", "type": "for_each", "params": {"steps": [
            {"id": "j", "type": "host_fs.list", "params": {}}
        ]}},
        {"id": "k", "type": "parallel", "params": {"branches": [
            [{"id": "l", "type": "invoke", "params": {}}],
            // A type that appears nowhere else in the tree, so a walker
            // that skipped this branch could not pass on the strength
            // of a twin at top level.
            [{"id": "m", "type": "host_only.under_parallel", "params": {}},
             {"id": "n", "type": "ifs", "params": {"ifs": [
                 {"then": [{"id": "o", "type": "script", "params": {}}]}
             ]}}]
        ]}}
    ]);
    let mut found = std::collections::BTreeSet::new();
    step_types(&tree, &mut found);
    let expected: std::collections::BTreeSet<String> = [
        "host_fs.read",
        "ifs",
        "host_fs.write",
        "return",
        "try",
        "host_process.run",
        "host_http.get",
        "host_http.post",
        "for_each",
        "host_fs.list",
        "parallel",
        "invoke",
        "host_only.under_parallel",
        "script",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(found, expected);
}

/// Every tool manifest declares only the host step types its steps
/// use, and nothing else — no egress, no invoke, no secrets — so the
/// manifest is an accurate statement of what the tool can reach. The
/// step types are gathered from the steps themselves, branches
/// included, and compared to the grants as sets.
#[test]
fn every_tool_manifest_declares_exactly_the_host_steps_it_uses() {
    let f = fixture();
    let tools: Vec<_> = f.bundled.iter().filter(|p| p.group() == "tools").collect();
    assert_eq!(tools.len(), 4, "read, write, grep, bash");
    for plugin in tools {
        let m = &plugin.manifest;
        let name = plugin.name();
        assert_eq!(m["roles"], json!([spi::tool::ROLE]), "{name}");
        assert!(plugin.guests.is_empty(), "{name} is declarative");
        assert_eq!(
            m["actions"].as_object().unwrap().len(),
            1,
            "{name}: only `call`"
        );
        let call = &m["actions"][spi::tool::CALL];
        assert_eq!(
            call["tool"]["parameters"]["additionalProperties"], false,
            "{name}"
        );
        let mut used = std::collections::BTreeSet::new();
        step_types(&call["steps"], &mut used);
        let used_host: std::collections::BTreeSet<String> = used
            .iter()
            .filter(|t| t.starts_with("host_"))
            .map(|t| format!("step_type:{t}"))
            .collect();
        assert!(!used_host.is_empty(), "{name} reaches something");
        let declared: std::collections::BTreeSet<String> = m["permissions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p.as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            declared, used_host,
            "{name}: grants must equal the host steps used"
        );
        assert!(
            !used.iter().any(|t| t == "try"),
            "{name}: outcomes are data; a tool never string-matches a caught error"
        );
        assert!(
            m.get("usesSecrets").is_none(),
            "{name}: a tool holds no secret"
        );
    }
}

// ------------------------------------------------------- the provider

/// The provider streams a turn against the stub: the request is the
/// contract input translated (key on the header, version pinned,
/// harvested tools passed through), and the events are the contract's
/// — text as it came, the split tool call whole, `end` with the merged
/// usage.
#[tokio::test(flavor = "multi_thread")]
async fn the_provider_streams_a_turn() {
    let f = fixture();
    let events = f
        .streamed("/stream-pin", opening_input(f.tools(), true))
        .await;
    assert_eq!(
        events,
        vec![
            opaque_block(),
            json!({"type": "text", "text": "Let me "}),
            json!({"type": "text", "text": "read it."}),
            json!({"type": "tool_use", "id": "toolu_01", "name": "read", "input": {"path": "hello.txt"}}),
            json!({"type": "end", "stop_reason": "tool_use",
                   "usage": {"input_tokens": 12, "output_tokens": 17, "cache_read_input_tokens": 0}}),
        ]
    );
    let (headers, body) = {
        let requests = f.stub.requests.lock().unwrap();
        let mine: Vec<_> = requests
            .iter()
            .filter(|(p, _, _)| p.starts_with("/stream-pin/"))
            .collect();
        assert_eq!(mine.len(), 1, "one turn, one POST: {mine:?}");
        (mine[0].1.clone(), mine[0].2.clone())
    };
    assert_eq!(headers["x-api-key"], API_KEY, "the secret reached the wire");
    assert_eq!(headers["anthropic-version"], ANTHROPIC_VERSION);
    assert_eq!(headers["content-type"], "application/json");
    assert_eq!(body["model"], "claude-fixture");
    assert_eq!(body["stream"], true);
    assert_eq!(body["max_tokens"], 200);
    assert_eq!(body["system"], "You are terse.");
    assert_eq!(body["tools"], f.tools(), "harvested tools, verbatim");
    assert_eq!(
        body["messages"][0]["content"][0]["text"],
        "What does hello.txt say?"
    );
    assert!(
        body.get("thinking").is_none(),
        "no thinking field unless config supplies one"
    );
}

/// A `base_url` with a trailing slash still reaches `/v1/messages`,
/// not `//v1/messages`: the guest builds the endpoint.
#[tokio::test(flavor = "multi_thread")]
async fn a_trailing_slash_on_base_url_does_not_double_the_path() {
    let f = fixture();
    let out = f
        .buffered("/slashed/", opening_input(json!([]), false))
        .await;
    assert_eq!(out["stop_reason"], "tool_use");
    let paths: Vec<String> = {
        let requests = f.stub.requests.lock().unwrap();
        requests
            .iter()
            .filter(|(p, _, _)| p.starts_with("/slashed"))
            .map(|(p, _, _)| p.clone())
            .collect()
    };
    assert_eq!(paths, vec!["/slashed/v1/messages".to_string()]);

    // The streamed turn goes through the other fetching action and
    // must land on the same path.
    let events = f
        .streamed("/slashed/", opening_input(json!([]), true))
        .await;
    assert_eq!(events.last().unwrap()["type"], "end");
    let paths: Vec<String> = {
        let requests = f.stub.requests.lock().unwrap();
        requests
            .iter()
            .filter(|(p, _, _)| p.starts_with("/slashed"))
            .map(|(p, _, _)| p.clone())
            .collect()
    };
    assert_eq!(paths, vec!["/slashed/v1/messages".to_string(); 2]);
}

/// The buffered form: the same turn as a message.
#[tokio::test(flavor = "multi_thread")]
async fn the_provider_answers_a_buffered_turn() {
    let f = fixture();
    let out = f
        .buffered("/buffered-pin", opening_input(f.tools(), false))
        .await;
    assert_eq!(
        out,
        json!({
            "message": {"role": "assistant", "content": [
                opaque_block(),
                {"type": "text", "text": "Let me read it."},
                {"type": "tool_use", "id": "toolu_01", "name": "read", "input": {"path": "hello.txt"}}
            ]},
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 12, "output_tokens": 17}
        })
    );
}

/// The vendor saying no is the contract's Failure on both paths — data
/// with `retryable` filled from the vendor's answer, never a step
/// error the loop would have to read.
#[tokio::test(flavor = "multi_thread")]
async fn a_vendor_rejection_is_the_contract_failure_on_both_paths() {
    let f = fixture();
    let out = f
        .buffered("/rate-limited", opening_input(json!([]), false))
        .await;
    assert_eq!(out["error"]["retryable"], true);
    assert_eq!(out["error"]["kind"], "rate_limit_error");
    assert!(
        out["error"]["message"]
            .as_str()
            .unwrap()
            .contains("slow down")
    );

    let out = f
        .buffered("/bad-key", opening_input(json!([]), false))
        .await;
    assert_eq!(out["error"]["retryable"], false);
    assert_eq!(out["error"]["kind"], "authentication_error");

    let events = f
        .streamed("/rate-limited", opening_input(json!([]), true))
        .await;
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["type"], "error");
    assert_eq!(events[0]["retryable"], true);
    assert_eq!(events[0]["kind"], "rate_limit_error");

    let events = f
        .streamed("/overloaded-midstream", opening_input(json!([]), true))
        .await;
    assert_eq!(
        events,
        vec![
            json!({"type": "text", "text": "so far"}),
            json!({"type": "error", "message": "vendor error: overloaded_error: Overloaded",
                   "retryable": true, "kind": "overloaded_error"}),
        ],
        "the error event is last; the vendor's message_stop after it is never relayed"
    );
}

// ------------------------------------------------------- the round trip

/// The milestone's done-when: a model-issued tool call executes end to
/// end. The stub's opening turn asks for `read` on hello.txt; the call
/// is dispatched by harvested descriptor; the result is rendered by
/// the shared convention and carried back as a tool_result; the stub's
/// closing turn quotes it.
#[tokio::test(flavor = "multi_thread")]
async fn a_model_issued_tool_call_executes_end_to_end() {
    let f = fixture();
    let mut input = opening_input(f.tools(), false);
    let opening = f.buffered("/round-trip", input.clone()).await;
    assert_eq!(opening["stop_reason"], "tool_use");
    let call = opening["message"]["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == "tool_use")
        .expect("the model asked for a tool")
        .clone();
    assert_eq!(call["name"], "read");

    let result = f.call_tool("read", call["input"].clone()).await;
    assert_eq!(result["is_error"], false);
    assert_eq!(result["content"], "hello from the workspace\n");
    let content = spi::tool::render_content(
        result["content"].as_str().unwrap(),
        result["truncated"].as_bool().unwrap_or(false),
    );

    let messages = input["messages"].as_array_mut().unwrap();
    messages.push(opening["message"].clone());
    messages.push(json!({"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": call["id"], "content": content}
    ]}));
    let closing = f.buffered("/round-trip", input).await;
    assert_eq!(closing["stop_reason"], "end_turn");
    assert_eq!(
        closing["message"]["content"][0]["text"],
        "It says: hello from the workspace\n"
    );

    // What the vendor was sent on the closing turn is the whole
    // conversation, the tool result first in the last message.
    let sent = {
        let requests = f.stub.requests.lock().unwrap();
        requests
            .iter()
            .filter(|(p, _, _)| p.starts_with("/round-trip/"))
            .map(|(_, _, b)| b.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1]["messages"].as_array().unwrap().len(), 3);
    assert_eq!(
        sent[1]["messages"][2]["content"][0]["tool_use_id"],
        "toolu_01"
    );
    // The vendor's thinking came back exactly as it was produced, in
    // front of the tool call it led to — the round trip the vendor
    // requires, carried by the caller as an opaque block it never read.
    assert_eq!(opening["message"]["content"][0], opaque_block());
    assert_eq!(
        sent[1]["messages"][1]["content"],
        json!([
            thinking_block(),
            {"type": "text", "text": "Let me read it."},
            {"type": "tool_use", "id": "toolu_01", "name": "read", "input": {"path": "hello.txt"}}
        ])
    );
}

// ---------------------------------------------------------- the tools

/// `read`: a hit is the content; a miss is the host step's message
/// with `is_error`; a cut is `truncated`, and the shared renderer adds
/// the marker.
#[tokio::test(flavor = "multi_thread")]
async fn the_read_tool_reports_hits_misses_and_cuts() {
    let f = fixture();
    let out = f.call_tool("read", json!({"path": "hello.txt"})).await;
    assert_eq!(
        out,
        json!({"content": "hello from the workspace\n", "is_error": false, "truncated": false})
    );

    let out = f
        .call_tool("read", json!({"path": "nope/missing.txt"}))
        .await;
    assert_eq!(out["is_error"], true);
    let msg = out["content"].as_str().unwrap();
    assert!(
        msg.contains("no such file") && msg.contains("missing.txt"),
        "{msg}"
    );

    let out = f
        .call_tool("read", json!({"path": "hello.txt", "max_bytes": 5}))
        .await;
    assert_eq!(
        out,
        json!({"content": "hello", "is_error": false, "truncated": true})
    );
    assert_eq!(
        spi::tool::render_content("hello", true),
        format!("hello\n{}", spi::tool::TRUNCATED_MARKER)
    );

    std::fs::create_dir_all(f.workspace.join("a-dir")).unwrap();
    let out = f.call_tool("read", json!({"path": "a-dir"})).await;
    assert_eq!(out["is_error"], true);
    assert!(out["content"].as_str().unwrap().contains("is a directory"));
}

/// `write` creates the file (parents included) and says what it did;
/// a directory in the way is an error the model sees.
#[tokio::test(flavor = "multi_thread")]
async fn the_write_tool_writes_and_reports() {
    let f = fixture();
    let out = f
        .call_tool(
            "write",
            json!({"path": "made/by/write.txt", "content": "fresh"}),
        )
        .await;
    assert_eq!(
        out,
        json!({"content": "wrote 5 bytes to made/by/write.txt", "is_error": false})
    );
    assert_eq!(
        std::fs::read_to_string(f.workspace.join("made/by/write.txt")).unwrap(),
        "fresh"
    );
    std::fs::create_dir_all(f.workspace.join("occupied")).unwrap();
    let out = f
        .call_tool("write", json!({"path": "occupied", "content": "x"}))
        .await;
    assert_eq!(out["is_error"], true);
    assert!(out["content"].as_str().unwrap().contains("is a directory"));
}

/// `grep`: matches as path:line:text; no matches is an answer, not an
/// error; a bad pattern is.
#[tokio::test(flavor = "multi_thread")]
async fn the_grep_tool_distinguishes_no_matches_from_failure() {
    let f = fixture();
    std::fs::create_dir_all(f.workspace.join("src")).unwrap();
    std::fs::write(
        f.workspace.join("src/lib.rs"),
        "fn alpha() {}\nfn beta() {}\n",
    )
    .unwrap();
    let out = f
        .call_tool("grep", json!({"pattern": "fn (alpha|beta)", "path": "src"}))
        .await;
    assert_eq!(out["is_error"], false);
    assert_eq!(out["truncated"], false);
    assert_eq!(
        out["content"],
        "src/lib.rs:1:fn alpha() {}\nsrc/lib.rs:2:fn beta() {}\n"
    );

    let out = f
        .call_tool("grep", json!({"pattern": "gamma", "path": "src"}))
        .await;
    assert_eq!(out, json!({"content": "no matches", "is_error": false}));

    let out = f
        .call_tool("grep", json!({"pattern": "(unclosed", "path": "src"}))
        .await;
    assert_eq!(out["is_error"], true);
    assert!(
        out["content"]
            .as_str()
            .unwrap()
            .starts_with("grep failed (exit status 2)")
    );
}

/// grep exits 2 for any file it could not read even when lines were
/// selected: the matches it did find are the answer, with the files it
/// could not read noted — not a failure that discards them.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn the_grep_tool_keeps_matches_when_some_files_are_unreadable() {
    use std::os::unix::fs::PermissionsExt as _;
    let f = fixture();
    let dir = f.workspace.join("mixed");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("open.txt"), "needle here\n").unwrap();
    std::fs::write(dir.join("locked.txt"), "needle too\n").unwrap();
    std::fs::set_permissions(
        dir.join("locked.txt"),
        std::fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    if std::fs::read(dir.join("locked.txt")).is_ok() {
        eprintln!("skipping: this user (root?) reads a mode-000 file anyway");
        return;
    }
    let out = f
        .call_tool("grep", json!({"pattern": "needle", "path": "mixed"}))
        .await;
    assert_eq!(out["is_error"], false, "{out}");
    let content = out["content"].as_str().unwrap();
    assert!(
        content.contains("mixed/open.txt:1:needle here"),
        "{content}"
    );
    assert!(
        content.contains("[grep could not read everything]"),
        "{content}"
    );
    assert!(
        content.contains("locked.txt"),
        "names the unreadable file: {content}"
    );
}

/// `write` onto a symlink is the model's error to see, not a fatal
/// step error: the host's refusal arrives as data.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn the_write_tool_reports_a_symlink_destination() {
    let f = fixture();
    std::fs::write(f.workspace.join("real-target.txt"), "keep").unwrap();
    std::os::unix::fs::symlink(
        f.workspace.join("real-target.txt"),
        f.workspace.join("alias.txt"),
    )
    .unwrap();
    let out = f
        .call_tool("write", json!({"path": "alias.txt", "content": "x"}))
        .await;
    assert_eq!(out["is_error"], true);
    assert!(
        out["content"].as_str().unwrap().contains("symlink"),
        "{out}"
    );
    assert_eq!(
        std::fs::read_to_string(f.workspace.join("real-target.txt")).unwrap(),
        "keep"
    );
}

/// `bash`: status, stdout and stderr as data; a nonzero status is the
/// model's error; capped output is `truncated`.
#[tokio::test(flavor = "multi_thread")]
async fn the_bash_tool_returns_status_and_output_as_data() {
    let f = fixture();
    let out = f
        .call_tool("bash", json!({"command": "echo out; echo err >&2; exit 3"}))
        .await;
    assert_eq!(out["is_error"], true);
    assert_eq!(out["truncated"], false);
    assert_eq!(
        out["content"],
        "exit status: 3\n\nstdout:\nout\n\nstderr:\nerr\n"
    );

    let out = f.call_tool("bash", json!({"command": "printf ok"})).await;
    assert_eq!(out["is_error"], false);
    assert_eq!(out["content"], "exit status: 0\n\nstdout:\nok\nstderr:\n");

    let out = f
        .call_tool(
            "bash",
            json!({"command": "head -c 300000 /dev/zero | tr '\\0' x"}),
        )
        .await;
    assert_eq!(out["is_error"], false);
    assert_eq!(out["truncated"], true, "stdout past the tool's cap");
}
