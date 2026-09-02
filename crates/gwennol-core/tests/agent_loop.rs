//! Milestone 5, end to end: the agent loop against a scriptable
//! provider and fixture tools on a real kernel.
//!
//! The roadmap's done-when, each pinned here: a multi-turn conversation
//! with tool calls runs against a stubbed provider; a failing tool is
//! reported to the model rather than ending the turn; cancelling
//! mid-stream tears the turn down cleanly. Around those, the consumer
//! rules the loop is the first implementation of: the assistant message
//! rebuilt from a stream and replayed verbatim, `opaque` in place;
//! results rendered through the shared truncation convention; fail-
//! closed reading of everything the provider says; the retry policy;
//! what the transcript holds after a turn that did not complete.
//!
//! Two fixture providers, so the loop's provider selection is exercised
//! too: a streamed one that POSTs the whole `chat` input to a stub and
//! relays the stub's NDJSON answer as its stream, letting each test
//! script the model's side by route; and a buffered one that answers
//! from canned responses in its `$config`.

use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use gwead::kernel::Kernel;
use gwead::serde_json::{Value, json};
use gwead::tokio_util::sync::CancellationToken;
use gwennol_core::{
    Access, ApprovalRequest, Decision, Event, Operator, Session, SessionConfig, SessionError,
    StopReason, ToolCall, Turn, TurnError, spi,
};

mod common;
use common::{assert_conforms, contracts};

// ------------------------------------------------------------ operator

tokio::task_local! {
    /// Where the harness records the events of the session driven by
    /// the current task. Tests in this binary share one operator (the
    /// host is a process singleton), so events are kept per task and
    /// a test reads back only what its own session emitted.
    static EVENTS: Arc<Mutex<Vec<Event>>>;
}

/// Run `fut` with an event sink, returning its output and the events
/// emitted while it ran.
async fn with_events<T>(fut: impl Future<Output = T>) -> (T, Vec<Event>) {
    let sink = Arc::new(Mutex::new(Vec::new()));
    let out = EVENTS.scope(sink.clone(), fut).await;
    let events = sink.lock().unwrap().clone();
    (out, events)
}

/// Approves everything except plugins named `denied*`; holds approvals
/// for plugins named `gated*` until the test releases them; records
/// every request; feeds `Session::run` from a queue.
#[derive(Default)]
struct Harness {
    requests: Mutex<Vec<ApprovalRequest>>,
    gates: common::Gates,
    inputs: Mutex<VecDeque<String>>,
}

impl Harness {
    fn requests_for(&self, plugin: &str) -> Vec<ApprovalRequest> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.plugin == plugin)
            .cloned()
            .collect()
    }
}

#[async_trait::async_trait]
impl Operator for Harness {
    async fn approve(&self, request: ApprovalRequest) -> Decision {
        let plugin = request.plugin.clone();
        self.requests.lock().unwrap().push(request);
        if plugin.starts_with("denied") {
            return Decision::Deny;
        }
        if plugin.starts_with("gated") {
            self.gates.gate(&plugin).hold().await;
        }
        Decision::Allow
    }
    async fn secret(&self, _: &str, _: &str) -> Option<String> {
        None
    }
    fn emit(&self, event: Event) {
        // A session driven outside `with_events` (none here) would
        // simply not be recorded.
        let _ = EVENTS.try_with(|sink| sink.lock().unwrap().push(event));
    }
    async fn input(&self) -> Option<Turn> {
        self.inputs
            .lock()
            .unwrap()
            .pop_front()
            .map(|text| Turn { text })
    }
}

// ------------------------------------------------------------- fixture

const STREAM_LLM: &str = "fixture_stream_llm";
const CANNED_LLM: &str = "fixture_canned_llm";

struct Fixture {
    kernel: Arc<Kernel>,
    harness: Arc<Harness>,
    workspace: std::path::PathBuf,
    stub: &'static Stub,
}

fn fixture() -> &'static Fixture {
    static F: OnceLock<Fixture> = OnceLock::new();
    F.get_or_init(|| {
        let workspace = tempfile::tempdir().unwrap().keep().canonicalize().unwrap();
        std::fs::write(workspace.join("hello.txt"), "hello from the workspace\n").unwrap();
        let harness = Arc::new(Harness::default());
        let mut kernel = gwennol_core::boot(harness.clone(), workspace.clone()).unwrap();
        for plugin in fixture_plugins() {
            kernel
                .register_plugin_from_json(&plugin.to_string())
                .unwrap_or_else(|e| panic!("{}: {e}", plugin["name"]));
        }
        Fixture {
            kernel: kernel.into_arc(),
            harness,
            workspace,
            stub: stub(),
        }
    })
}

/// The config of a streamed session against one stub route.
fn config(route: &str) -> SessionConfig {
    let f = fixture();
    let mut configs = BTreeMap::new();
    configs.insert(
        STREAM_LLM.to_string(),
        json!({"url": format!("http://{}{route}", f.stub.addr)}),
    );
    SessionConfig {
        provider: Some(STREAM_LLM.into()),
        system: Some("be brief".into()),
        max_tokens: Some(64),
        plugin_configs: configs,
        retry: gwennol_core::RetryPolicy {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(4),
        },
        ..SessionConfig::default()
    }
}

/// A streamed session against one stub route.
fn session(route: &str) -> Session {
    Session::new(fixture().kernel.clone(), config(route)).unwrap()
}

/// A buffered session answering from canned responses.
fn canned(answers: Vec<Value>) -> Session {
    let f = fixture();
    let mut configs = BTreeMap::new();
    configs.insert(CANNED_LLM.to_string(), json!({"answers": answers}));
    Session::new(
        f.kernel.clone(),
        SessionConfig {
            provider: Some(CANNED_LLM.into()),
            stream: false,
            plugin_configs: configs,
            ..SessionConfig::default()
        },
    )
    .unwrap()
}

/// Run one turn bounded in time, so a stalled loop fails with a name
/// rather than hanging the suite.
async fn turn(session: &mut Session, text: &str) -> Result<gwennol_core::TurnOutcome, TurnError> {
    let cancel = CancellationToken::new();
    tokio::time::timeout(Duration::from_secs(30), session.turn(text, &cancel))
        .await
        .expect("the turn finishes within 30s")
}

fn tool_plugin(plugin: &str, tool: &str, permissions: &[&str], steps: Value) -> Value {
    json!({
        "name": plugin, "version": "0.0.0",
        "description": format!("test fixture tool {tool}"),
        "roles": [spi::tool::ROLE],
        "permissions": permissions,
        "actions": {"call": {
            "tool": {
                "name": tool,
                "description": format!("The {tool} fixture."),
                "parameters": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "message": {"type": "string"},
                        "path": {"type": "string"}
                    }
                }
            },
            "steps": steps
        }}
    })
}

fn fixture_plugins() -> Vec<Value> {
    let ret = |value: Value| json!([{"id": "out", "type": "return", "params": {"value": value}}]);
    vec![
        // The scriptable streamed provider: the whole chat input goes
        // to the stub, whose NDJSON body is the stream.
        json!({
            "name": STREAM_LLM, "version": "0.0.0",
            "description": "test fixture: relays the stub's NDJSON as the turn's stream",
            "roles": [spi::llm_chat::ROLE],
            "permissions": ["step_type:host_http.post", "network:egress:127.0.0.1"],
            "actions": {"chat": {"steps": [
                {"id": "fetch", "type": "host_http.post", "params": {
                    "url": "{{$config.url}}",
                    // Field by field: `$input` alone resolves to the
                    // whole template namespace. Every session in this
                    // suite sets system and max_tokens, and the tool
                    // inventory is never empty, so nothing renders as
                    // a missing value here.
                    "body": {
                        "system": "{{$input.system}}",
                        "messages": "{{$input.messages}}",
                        "tools": "{{$input.tools}}",
                        "max_tokens": "{{$input.max_tokens}}",
                        "stream": true
                    },
                    "stream": true}},
                {"id": "out", "type": "return", "params": {"value": {
                    "stream": "{{$steps.fetch.result.body}}"}}}
            ]}}
        }),
        // The canned buffered provider: answers[0] for the opening
        // round, [1] after one assistant message, [2] after two.
        json!({
            "name": CANNED_LLM, "version": "0.0.0",
            "description": "test fixture: buffered answers from $config.answers by round",
            "roles": [spi::llm_chat::ROLE],
            "actions": {"chat": {"steps": [
                {"id": "pick", "type": "ifs", "params": {"ifs": [
                    {"test": "$input.messages[3].role == 'assistant'",
                     "then": [{"id": "r2", "type": "return", "params": {"value": "{{$config.answers[2]}}"}}]},
                    {"test": "$input.messages[1].role == 'assistant'",
                     "then": [{"id": "r1", "type": "return", "params": {"value": "{{$config.answers[1]}}"}}]},
                    {"then": [{"id": "r0", "type": "return", "params": {"value": "{{$config.answers[0]}}"}}]}
                ]}}
            ]}}
        }),
        tool_plugin(
            "fixture_echo",
            "echo",
            &[],
            ret(json!({"content": "echo: {{$input.message}}", "is_error": false})),
        ),
        tool_plugin(
            "fixture_fail",
            "fail",
            &[],
            ret(json!({"content": "boom", "is_error": true})),
        ),
        tool_plugin(
            "fixture_cut",
            "cut",
            &[],
            ret(json!({"content": "abc", "is_error": false, "truncated": true})),
        ),
        tool_plugin("fixture_broken", "broken", &[], ret(json!({"nope": 1}))),
        tool_plugin(
            "fixture_slow",
            "slow",
            &["step_type:host_process.run"],
            json!([
                {"id": "p", "type": "host_process.run", "params": {
                    "argv": ["sleep", "30"], "timeout_ms": 60000, "max_output_bytes": 1024}},
                {"id": "out", "type": "return", "params": {"value": {"content": "slept", "is_error": false}}}
            ]),
        ),
        // Reads through a host step: the approval carries the cause.
        tool_plugin(
            "fixture_read",
            "read_file",
            &["step_type:host_fs.read"],
            read_steps(),
        ),
        // Same tool, held at its approval / refused at its approval.
        tool_plugin(
            "gated_tool",
            "gated",
            &["step_type:host_fs.read"],
            read_steps(),
        ),
        tool_plugin(
            "denied_tool",
            "denied",
            &["step_type:host_fs.read"],
            read_steps(),
        ),
    ]
}

fn read_steps() -> Value {
    json!([
        {"id": "r", "type": "host_fs.read", "params": {"path": "{{$input.path}}", "max_bytes": 1024}},
        {"id": "out", "type": "return", "params": {"value": {
            "content": "{{$steps.r.result.content ?? $steps.r.result.message}}",
            "is_error": false}}}
    ])
}

// ---------------------------------------------------------------- stub

/// The model's side, scripted by route. Records every request; answers
/// with NDJSON events chosen by the route, the round (assistant messages
/// in the request so far) and, for the retry route, how many times the
/// route has been asked.
struct Stub {
    addr: std::net::SocketAddr,
    /// `(path, chat input)` in arrival order.
    requests: Mutex<Vec<(String, Value)>>,
    /// Paths whose response the client hung up on mid-way.
    hangups: Mutex<Vec<String>>,
}

impl Stub {
    fn requests_for(&self, path: &str) -> Vec<Value> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| p == path)
            .map(|(_, body)| body.clone())
            .collect()
    }
}

fn text(t: &str) -> Value {
    json!({"type": "text", "text": t})
}
fn call(id: &str, name: &str, input: Value) -> Value {
    json!({"type": "tool_use", "id": id, "name": name, "input": input})
}
fn end(stop: &str) -> Value {
    json!({"type": "end", "stop_reason": stop, "usage": {"input_tokens": 3, "output_tokens": 5, "cache_read_input_tokens": 0}})
}

/// The closing round quotes what it was given: the last message's
/// first block, so the round trip is provable from the model's side.
fn quote(body: &Value) -> Vec<Value> {
    let last = body["messages"].as_array().unwrap().last().unwrap();
    let block = &last["content"][0];
    let quoted = format!(
        "result: {} (error: {})",
        block["content"].as_str().unwrap_or("<no content>"),
        block["is_error"]
    );
    vec![text(&quoted), end("end_turn")]
}

/// Routes whose events are off-contract on purpose; the stub does not
/// hold them to the schema it holds every other event to.
const OFF_CONTRACT: &[&str] = &[
    "/bogus-event",
    "/bad-stop",
    "/bad-block",
    "/extra-field",
    "/no-usage",
];

/// Which round of the current turn a request is: the assistant
/// messages since the last user message that carried text.
fn round_of(body: &Value) -> usize {
    let mut round = 0;
    for message in body["messages"].as_array().unwrap().iter().rev() {
        if message["role"] == "assistant" {
            round += 1;
        } else if message["content"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|b| b["type"] == "text")
        {
            break;
        }
    }
    round
}

fn script(route: &str, body: &Value, asked: usize) -> Vec<Value> {
    let round = round_of(body);
    let one_call = |name: &str, input: Value| -> Vec<Value> {
        if round == 0 {
            vec![call("c1", name, input), end("tool_use")]
        } else {
            quote(body)
        }
    };
    match route {
        "/text" => vec![text("Hello"), text(" there"), end("end_turn")],
        "/tools" if round == 0 => vec![
            text("Looking."),
            call("c1", "echo", json!({"message": "hi"})),
            call("c2", "echo", json!({"message": "again"})),
            end("tool_use"),
        ],
        "/tools" => quote(body),
        "/opaque" if round == 0 => vec![
            json!({"type": "opaque", "provider": STREAM_LLM, "data": {"thinking": "hmm", "signature": "s"}}),
            text("Let "),
            text("me."),
            call("c1", "echo", json!({"message": "x"})),
            text("tail"),
            end("tool_use"),
        ],
        "/opaque" => vec![text("ok"), end("end_turn")],
        "/fail-tool" => one_call("fail", json!({})),
        "/unknown-tool" => one_call("nope", json!({})),
        "/bad-args" => one_call("echo", json!({"message": 5})),
        "/broken-tool" => one_call("broken", json!({})),
        "/cut-tool" => one_call("cut", json!({})),
        "/read-tool" => one_call("read_file", json!({"path": "hello.txt"})),
        "/denied-tool" => one_call("denied", json!({"path": "hello.txt"})),
        // Keyed on the request count, not the round: the turn after the
        // cancelled one carries on from the interrupted results.
        "/slow-tool" if asked == 1 => vec![
            call("c1", "slow", json!({})),
            call("c2", "echo", json!({"message": "never"})),
            end("tool_use"),
        ],
        "/slow-tool" => quote(body),
        "/gated-tool" => one_call("gated", json!({"path": "hello.txt"})),
        "/empty" => vec![end("max_tokens")],
        "/retry" | "/retry-once" | "/retry-clamped" if asked == 1 => vec![
            text("partial"),
            json!({"type": "error", "message": "overloaded", "retryable": true, "kind": "overloaded_error"}),
        ],
        "/retry" | "/retry-once" | "/retry-clamped" => vec![text("recovered"), end("end_turn")],
        "/always-retryable" | "/always-retryable-saturating" => {
            vec![json!({"type": "error", "message": "still overloaded", "retryable": true})]
        }
        "/fatal" => vec![
            json!({"type": "error", "message": "bad key", "retryable": false, "kind": "authentication_error"}),
        ],
        "/unknown-retryable" => vec![json!({"type": "error", "message": "who knows"})],
        "/truncated" => vec![text("hel")],
        "/bogus-event" => vec![json!({"type": "bogus"})],
        "/bad-stop" => vec![end("pause_turn")],
        "/bad-block" => vec![json!({"type": "tool_use", "id": "c", "name": "echo", "input": []})],
        "/extra-field" => vec![json!({"type": "text", "text": "a", "extra": 1})],
        "/no-usage" => vec![json!({"type": "end", "stop_reason": "end_turn"})],
        "/tool-use-without-call" => vec![text("hm"), end("tool_use")],
        "/refusal" => vec![text("no"), end("refusal")],
        "/refusal-with-call" => vec![call("c1", "echo", json!({"message": "x"})), end("refusal")],
        "/max-tokens" => vec![text("cut off"), end("max_tokens")],
        "/endless-tools" => vec![
            call(&format!("c{asked}"), "echo", json!({"message": "more"})),
            end("tool_use"),
        ],
        "/big-event" => vec![text(&"x".repeat(100_000)), end("end_turn")],
        other => panic!("no script for route {other}"),
    }
}

fn stub() -> &'static Stub {
    static S: OnceLock<&'static Stub> = OnceLock::new();
    S.get_or_init(|| {
        let listener = TcpListener::bind("127.0.0.1:0").expect("stub binds");
        let stub: &'static Stub = Box::leak(Box::new(Stub {
            addr: listener.local_addr().unwrap(),
            requests: Mutex::new(Vec::new()),
            hangups: Mutex::new(Vec::new()),
        }));
        common::serve(listener, move |mut socket| {
            let _ = socket.set_read_timeout(Some(Duration::from_secs(10)));
            let Some((path, body)) = common::read_http_request(&mut socket) else {
                return;
            };
            let body: Value = gwead::serde_json::from_slice(&body)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&body).into()));
            // Every request the provider sends is a conforming chat
            // input, or the loop built it wrong.
            assert_conforms(contracts().chat_input, &body);
            let asked = {
                let mut requests = stub.requests.lock().unwrap();
                requests.push((path.clone(), body.clone()));
                requests.iter().filter(|(p, _)| *p == path).count()
            };
            if path == "/slow" {
                slow_stream(stub, &mut socket, &path);
                return;
            }
            let events = script(&path, &body, asked);
            let _ = socket.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: application/x-ndjson\r\nconnection: close\r\n\r\n",
            );
            for event in events {
                if !OFF_CONTRACT.contains(&path.as_str()) {
                    assert_conforms(contracts().stream_event, &event);
                }
                let _ = socket.write_all(format!("{event}\n").as_bytes());
                let _ = socket.flush();
            }
        });
        stub
    })
}

/// Text events at a steady pace until the client hangs up (recorded)
/// or, after enough of them, `end`.
fn slow_stream(stub: &Stub, socket: &mut TcpStream, path: &str) {
    let _ = socket.write_all(
        b"HTTP/1.1 200 OK\r\ncontent-type: application/x-ndjson\r\nconnection: close\r\n\r\n",
    );
    for i in 0..200 {
        let line = format!("{}\n", text(&format!("tick {i} ")));
        if socket.write_all(line.as_bytes()).is_err() || socket.flush().is_err() {
            stub.hangups.lock().unwrap().push(path.to_string());
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = socket.write_all(format!("{}\n", end("end_turn")).as_bytes());
}

/// Wait for the stub to record a hangup on `path`.
async fn wait_for_hangup(path: &str) {
    let f = fixture();
    for _ in 0..250 {
        if f.stub.hangups.lock().unwrap().iter().any(|p| p == path) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the stub never saw the client hang up on {path}");
}

fn user(text: &str) -> Value {
    json!({"role": "user", "content": [{"type": "text", "text": text}]})
}

fn tool_call(id: &str, name: &str, input: Value) -> ToolCall {
    ToolCall {
        id: Some(id.into()),
        name: name.into(),
        arguments: input.to_string(),
    }
}

// ------------------------------------------------------ construction

#[tokio::test(flavor = "multi_thread")]
async fn a_session_needs_one_named_or_unambiguous_provider() {
    let f = fixture();
    // Two fulfillers are registered: resolving by role refuses rather
    // than picking silently.
    let err = Session::new(f.kernel.clone(), SessionConfig::default()).unwrap_err();
    match err {
        SessionError::AmbiguousProvider(names) => {
            assert_eq!(names, vec![CANNED_LLM.to_string(), STREAM_LLM.to_string()]);
        }
        other => panic!("{other}"),
    }
    let err = Session::new(
        f.kernel.clone(),
        SessionConfig {
            provider: Some("fixture_echo".into()),
            ..SessionConfig::default()
        },
    )
    .unwrap_err();
    assert!(matches!(err, SessionError::NoSuchProvider(name) if name == "fixture_echo"));
    assert_eq!(session("/text").provider(), STREAM_LLM);
}

// --------------------------------------------------------- the loop

/// The done-when, streamed: two turns, the first with two parallel tool
/// calls answered in order in the next message, the second continuing
/// the same transcript; text shown as it streams; the model quoting
/// the result it was given.
#[tokio::test(flavor = "multi_thread")]
async fn a_multi_turn_conversation_with_tool_calls_runs_end_to_end() {
    let f = fixture();
    let mut s = session("/tools");
    let (outcome, events) = with_events(async { turn(&mut s, "call the tool").await }).await;
    let outcome = outcome.unwrap();
    assert_eq!(outcome.stop_reason, StopReason::EndTurn);
    assert_eq!(outcome.rounds, 2);
    assert_eq!(outcome.usage.input_tokens, 6, "summed over both rounds");
    assert_eq!(outcome.usage.output_tokens, 10);

    let hi = tool_call("c1", "echo", json!({"message": "hi"}));
    let again = tool_call("c2", "echo", json!({"message": "again"}));
    assert_eq!(
        events,
        vec![
            Event::Text("Looking.".into()),
            Event::ToolCall(hi.clone()),
            Event::ToolResult {
                call: hi,
                content: "echo: hi".into(),
                is_error: false
            },
            Event::ToolCall(again.clone()),
            Event::ToolResult {
                call: again,
                content: "echo: again".into(),
                is_error: false
            },
            Event::Text("result: echo: hi (error: false)".into()),
            Event::TurnComplete,
        ]
    );
    assert_eq!(
        s.transcript(),
        &[
            user("call the tool"),
            json!({"role": "assistant", "content": [
                text("Looking."),
                call("c1", "echo", json!({"message": "hi"})),
                call("c2", "echo", json!({"message": "again"})),
            ]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "c1", "content": "echo: hi", "is_error": false},
                {"type": "tool_result", "tool_use_id": "c2", "content": "echo: again", "is_error": false},
            ]}),
            json!({"role": "assistant", "content": [text("result: echo: hi (error: false)")]}),
        ]
    );

    // The second turn carries the whole transcript, and the request the
    // provider builds is what the loop was configured with.
    let (second, events) = with_events(async { turn(&mut s, "and once more").await }).await;
    assert_eq!(second.unwrap().rounds, 2);
    assert!(events.contains(&Event::TurnComplete));
    let sent = f.stub.requests_for("/tools");
    assert_eq!(sent.len(), 4, "two rounds per turn");
    assert_eq!(sent[2]["messages"].as_array().unwrap().len(), 5);
    assert_eq!(sent[2]["messages"][4], user("and once more"));
    assert_eq!(sent[2]["system"], "be brief");
    assert_eq!(sent[2]["max_tokens"], 64);
    assert_eq!(sent[2]["stream"], true);
    let tools: Vec<&str> = sent[2]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        tools,
        [
            "broken",
            "cut",
            "denied",
            "echo",
            "fail",
            "gated",
            "read_file",
            "slow"
        ],
        "the harvest, sorted"
    );
    assert_eq!(s.transcript().len(), 8);
}

/// The same conversation on the buffered form, with text emitted per
/// block after the round.
#[tokio::test(flavor = "multi_thread")]
async fn the_buffered_form_runs_the_same_loop() {
    let mut s = canned(vec![
        json!({
            "message": {"role": "assistant", "content": [
                text("Calling."), call("c1", "echo", json!({"message": "buffered"}))
            ]},
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 1, "output_tokens": 2}
        }),
        json!({
            "message": {"role": "assistant", "content": [text("done")]},
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 3, "output_tokens": 4}
        }),
    ]);
    let (outcome, events) = with_events(async { turn(&mut s, "go").await }).await;
    let outcome = outcome.unwrap();
    assert_eq!(outcome.rounds, 2);
    assert_eq!(outcome.usage.input_tokens, 4);
    let c1 = tool_call("c1", "echo", json!({"message": "buffered"}));
    assert_eq!(
        events,
        vec![
            Event::Text("Calling.".into()),
            Event::ToolCall(c1.clone()),
            Event::ToolResult {
                call: c1,
                content: "echo: buffered".into(),
                is_error: false
            },
            Event::Text("done".into()),
            Event::TurnComplete,
        ]
    );
    assert_eq!(s.transcript().len(), 4);
    assert_eq!(s.transcript()[2]["content"][0]["content"], "echo: buffered");
}

/// The buffered failed form is the same `Failure` the stream's error
/// event carries; anything else off-contract is refused fail-closed.
#[tokio::test(flavor = "multi_thread")]
async fn the_buffered_failure_and_contract_violations_are_read_fail_closed() {
    let mut s = canned(vec![
        json!({"error": {"message": "nope", "retryable": false, "kind": "k"}}),
    ]);
    match turn(&mut s, "go").await.unwrap_err() {
        TurnError::Provider(failure) => {
            assert_eq!(failure.message, "nope");
            assert_eq!(failure.retryable, Some(false));
            assert_eq!(failure.kind.as_deref(), Some("k"));
        }
        other => panic!("{other}"),
    }
    for (bad, why) in [
        (
            json!({"message": {"role": "assistant", "content": []}, "stop_reason": "end_turn"}),
            "usage",
        ),
        (
            json!({"message": {"role": "assistant", "content": [{"type": "thinking", "thinking": "x"}]},
                   "stop_reason": "end_turn", "usage": {"input_tokens": 0, "output_tokens": 0}}),
            "unknown assistant content block",
        ),
        (json!({"stream": 1}), "streamed form"),
    ] {
        let mut s = canned(vec![bad]);
        match turn(&mut s, "go").await.unwrap_err() {
            TurnError::Contract(msg) => assert!(msg.contains(why), "{msg}"),
            other => panic!("{other}"),
        }
        assert_eq!(s.transcript(), &[user("go")], "nothing partial is kept");
    }
}

/// The streamed message the loop replays is the events in order:
/// adjacent text coalesced, the `opaque` block and the tool call whole
/// and in place — and the provider gets exactly that back.
#[tokio::test(flavor = "multi_thread")]
async fn the_assistant_message_is_rebuilt_from_the_stream_and_replayed_verbatim() {
    let f = fixture();
    let mut s = session("/opaque");
    let (outcome, events) = with_events(async { turn(&mut s, "think then call").await }).await;
    assert_eq!(outcome.unwrap().rounds, 2);
    // Text is shown as it streamed, never the opaque block.
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::Text(_)))
            .cloned()
            .collect::<Vec<_>>(),
        vec![
            Event::Text("Let ".into()),
            Event::Text("me.".into()),
            Event::Text("tail".into()),
            Event::Text("ok".into()),
        ]
    );
    let rebuilt = json!({"role": "assistant", "content": [
        {"type": "opaque", "provider": STREAM_LLM, "data": {"thinking": "hmm", "signature": "s"}},
        text("Let me."),
        call("c1", "echo", json!({"message": "x"})),
        text("tail"),
    ]});
    assert_eq!(s.transcript()[1], rebuilt);
    let sent = f.stub.requests_for("/opaque");
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1]["messages"][1], rebuilt, "replayed verbatim");
    assert_eq!(
        sent[1]["messages"][2]["content"][0]["tool_use_id"], "c1",
        "answered in the immediately following message"
    );
}

/// `Session::run` drives turns from the operator until it has none.
#[tokio::test(flavor = "multi_thread")]
async fn run_takes_turns_from_the_operator_until_input_ends() {
    let f = fixture();
    let mut s = session("/text");
    {
        let mut inputs = f.harness.inputs.lock().unwrap();
        inputs.push_back("one".into());
        inputs.push_back("two".into());
    }
    let cancel = CancellationToken::new();
    let (outcome, events) = with_events(async {
        tokio::time::timeout(Duration::from_secs(30), s.run(&cancel))
            .await
            .unwrap()
    })
    .await;
    outcome.unwrap();
    assert_eq!(
        events.iter().filter(|e| **e == Event::TurnComplete).count(),
        2
    );
    assert_eq!(s.transcript().len(), 4);
    assert_eq!(s.transcript()[3]["content"][0]["text"], "Hello there");
}

// ------------------------------------------------- answering the model

/// The done-when: a tool's failure goes to the model as `is_error` and
/// the turn continues; the shared truncation marker is rendered by the
/// loop, and the frontend sees the tool's verdict as data.
#[tokio::test(flavor = "multi_thread")]
async fn a_failing_tool_and_a_cut_result_are_reported_to_the_model() {
    let mut s = session("/fail-tool");
    let (outcome, events) = with_events(async { turn(&mut s, "fail").await }).await;
    assert_eq!(outcome.unwrap().rounds, 2);
    let c1 = tool_call("c1", "fail", json!({}));
    assert!(events.contains(&Event::ToolResult {
        call: c1,
        content: "boom".into(),
        is_error: true
    }));
    assert_eq!(
        s.transcript()[3]["content"][0]["text"],
        "result: boom (error: true)",
        "the model saw the failure and carried on"
    );

    let mut s = session("/cut-tool");
    let (outcome, events) = with_events(async { turn(&mut s, "cut").await }).await;
    assert_eq!(outcome.unwrap().rounds, 2);
    let rendered = format!("abc\n{}", spi::tool::TRUNCATED_MARKER);
    assert!(events.contains(&Event::ToolResult {
        call: tool_call("c1", "cut", json!({})),
        content: rendered.clone(),
        is_error: false
    }));
    assert_eq!(s.transcript()[2]["content"][0]["content"], rendered);
}

/// A call the tool cannot answer is still answered — unknown tool,
/// refused arguments, malformed result, a step the operator denied —
/// with the reason, and the frontend told it was not the tool's answer.
#[tokio::test(flavor = "multi_thread")]
async fn a_call_that_cannot_run_is_still_answered_with_the_reason() {
    let f = fixture();
    for (route, name, input, expect) in [
        ("/unknown-tool", "nope", json!({}), "no tool named \"nope\""),
        (
            "/bad-args",
            "echo",
            json!({"message": 5}),
            "do not match its schema",
        ),
        ("/broken-tool", "broken", json!({}), "malformed result"),
        (
            "/denied-tool",
            "denied",
            json!({"path": "hello.txt"}),
            "operator denied",
        ),
    ] {
        let mut s = session(route);
        let (outcome, events) = with_events(async { turn(&mut s, route).await }).await;
        assert_eq!(outcome.unwrap().rounds, 2, "{route}: the turn continued");
        let failed = events
            .iter()
            .find_map(|e| match e {
                Event::ToolFailed { call, error } => Some((call.clone(), error.clone())),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{route}: no ToolFailed in {events:?}"));
        assert_eq!(failed.0, tool_call("c1", name, input));
        assert!(failed.1.contains(expect), "{route}: {}", failed.1);
        assert!(
            !events.iter().any(|e| matches!(e, Event::ToolResult { .. })),
            "{route}: exactly one answer event per call"
        );
        let result = &s.transcript()[2]["content"][0];
        assert_eq!(result["is_error"], true, "{route}");
        assert_eq!(
            result["content"], failed.1,
            "{route}: the model is told the same"
        );
        assert!(
            s.transcript()[3]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains(expect),
            "{route}: the model saw it"
        );
    }
    // The denial reached the operator with the tool call named as its
    // cause, through the execution context the loop set.
    let denials = f.harness.requests_for("denied_tool");
    assert_eq!(denials.len(), 1);
    assert_eq!(
        denials[0].cause,
        Some(tool_call("c1", "denied", json!({"path": "hello.txt"})))
    );
    assert_eq!(
        denials[0].access,
        Access::ReadFile(f.workspace.join("hello.txt"))
    );
}

/// A tool that reaches outside the sandbox does so with the model's
/// call named in the approval, and its result is what the file holds.
#[tokio::test(flavor = "multi_thread")]
async fn a_tool_call_names_itself_in_the_approval_it_causes() {
    let f = fixture();
    let mut s = session("/read-tool");
    turn(&mut s, "read").await.unwrap();
    assert_eq!(
        s.transcript()[2]["content"][0]["content"],
        "hello from the workspace\n"
    );
    let asked = f.harness.requests_for("fixture_read");
    assert_eq!(asked.len(), 1);
    assert_eq!(
        asked[0].cause,
        Some(tool_call("c1", "read_file", json!({"path": "hello.txt"})))
    );
}

// ------------------------------------------------ provider failures

/// A `retryable` failure is retried unchanged, with the frontend told;
/// one that is not — or that ran out of attempts, or whose provider
/// could not say — ends the turn as the failure.
#[tokio::test(flavor = "multi_thread")]
async fn retryable_failures_are_retried_and_others_end_the_turn() {
    let f = fixture();
    let mut s = session("/retry");
    let (outcome, events) = with_events(async { turn(&mut s, "flaky").await }).await;
    assert_eq!(outcome.unwrap().rounds, 1, "one round, two attempts");
    assert_eq!(
        events,
        vec![
            Event::Text("partial".into()),
            Event::Retry {
                attempt: 2,
                max_attempts: 3,
                failure: gwennol_core::Failure {
                    message: "overloaded".into(),
                    retryable: Some(true),
                    kind: Some("overloaded_error".into()),
                },
            },
            Event::Text("recovered".into()),
            Event::TurnComplete,
        ]
    );
    let sent = f.stub.requests_for("/retry");
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0], sent[1], "retried unchanged");
    assert_eq!(s.transcript()[1]["content"][0]["text"], "recovered");

    let mut s = session("/always-retryable");
    let (outcome, events) = with_events(async { turn(&mut s, "hopeless").await }).await;
    assert!(matches!(outcome.unwrap_err(), TurnError::Provider(f) if f.retryable == Some(true)));
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::Retry { .. }))
            .count(),
        2,
        "three attempts, two retries"
    );
    assert_eq!(f.stub.requests_for("/always-retryable").len(), 3);

    // The backoff is clamped before the first wait and doubled without
    // overflow: an hour's initial backoff under a millisecond ceiling
    // retries at once, and a maximal one never panics the loop.
    let mut s = Session::new(
        f.kernel.clone(),
        SessionConfig {
            retry: gwennol_core::RetryPolicy {
                max_attempts: 2,
                initial_backoff: Duration::from_secs(3600),
                max_backoff: Duration::from_millis(1),
            },
            ..config("/retry-clamped")
        },
    )
    .unwrap();
    assert_eq!(turn(&mut s, "clamped").await.unwrap().rounds, 1);
    let mut s = Session::new(
        f.kernel.clone(),
        SessionConfig {
            retry: gwennol_core::RetryPolicy {
                max_attempts: 3,
                initial_backoff: Duration::MAX,
                max_backoff: Duration::from_millis(1),
            },
            ..config("/always-retryable-saturating")
        },
    )
    .unwrap();
    assert!(matches!(
        turn(&mut s, "saturating").await.unwrap_err(),
        TurnError::Provider(_)
    ));
    assert_eq!(f.stub.requests_for("/always-retryable-saturating").len(), 3);

    for route in ["/fatal", "/unknown-retryable"] {
        let mut s = session(route);
        let err = turn(&mut s, "no").await.unwrap_err();
        assert!(matches!(err, TurnError::Provider(_)), "{route}: {err}");
        assert_eq!(f.stub.requests_for(route).len(), 1, "{route}: not retried");
        assert_eq!(s.transcript(), &[user("no")]);
    }
}

/// Everything off-contract from a streamed provider fails the turn
/// rather than being guessed at.
#[tokio::test(flavor = "multi_thread")]
async fn off_contract_streams_fail_the_turn_closed() {
    for (route, why) in [
        ("/bogus-event", "unknown stream event type"),
        ("/bad-stop", "stop_reason"),
        ("/bad-block", "object `input`"),
        ("/extra-field", "does not allow"),
        ("/no-usage", "usage"),
        ("/tool-use-without-call", "no tool_use block"),
    ] {
        let mut s = session(route);
        match turn(&mut s, route).await.unwrap_err() {
            TurnError::Contract(msg) => assert!(msg.contains(why), "{route}: {msg}"),
            other => panic!("{route}: {other}"),
        }
        assert_eq!(s.transcript(), &[user(route)]);
    }
    // A stream that ends without `end` is a failed turn, not a short
    // answer — the "hel" it streamed is shown, never stored.
    let mut s = session("/truncated");
    let (outcome, events) = with_events(async { turn(&mut s, "cut").await }).await;
    assert!(matches!(outcome.unwrap_err(), TurnError::StreamEnded));
    assert_eq!(events, vec![Event::Text("hel".into())]);
    assert_eq!(s.transcript(), &[user("cut")]);

    // The event cap is the loop's bound on buffering.
    let f = fixture();
    let mut s = Session::new(
        f.kernel.clone(),
        SessionConfig {
            max_event_bytes: 4096,
            ..config("/big-event")
        },
    )
    .unwrap();
    match turn(&mut s, "big").await.unwrap_err() {
        TurnError::Contract(msg) => assert!(msg.contains("4096-byte cap"), "{msg}"),
        other => panic!("{other}"),
    }
}

/// Refusal ends the turn unretried; `max_tokens` is a completed turn;
/// a refusal that also asked for tools keeps neither.
#[tokio::test(flavor = "multi_thread")]
async fn refusal_and_max_tokens_end_the_turn() {
    let mut s = session("/refusal");
    let outcome = turn(&mut s, "do it").await.unwrap();
    assert_eq!(outcome.stop_reason, StopReason::Refusal);
    assert_eq!(s.transcript().len(), 2);

    let mut s = session("/max-tokens");
    let outcome = turn(&mut s, "long").await.unwrap();
    assert_eq!(outcome.stop_reason, StopReason::MaxTokens);
    assert_eq!(s.transcript()[1]["content"][0]["text"], "cut off");

    // A refusal that also asked for tools is kept, its calls answered
    // as not run — the model remembers refusing, and the transcript
    // stays replayable.
    let mut s = session("/refusal-with-call");
    let outcome = turn(&mut s, "hm").await.unwrap();
    assert_eq!(outcome.stop_reason, StopReason::Refusal);
    assert_eq!(s.transcript().len(), 3);
    assert_eq!(s.transcript()[1]["content"][0]["type"], "tool_use");
    assert_eq!(
        s.transcript()[2]["content"][0],
        json!({"type": "tool_result", "tool_use_id": "c1",
               "content": "not run: the model's turn ended in a refusal", "is_error": true})
    );

    // An empty message is a completed turn with nothing to replay: not
    // stored, so the next turn's text joins the user message it left.
    let mut s = session("/empty");
    let outcome = turn(&mut s, "cut short").await.unwrap();
    assert_eq!(outcome.stop_reason, StopReason::MaxTokens);
    assert_eq!(s.transcript(), &[user("cut short")]);
}

/// The round cap ends a turn whose model never stops asking for tools;
/// the exchanges up to the cap are kept.
#[tokio::test(flavor = "multi_thread")]
async fn the_round_limit_bounds_a_turn() {
    let f = fixture();
    let mut s = Session::new(
        f.kernel.clone(),
        SessionConfig {
            max_rounds: 3,
            ..config("/endless-tools")
        },
    )
    .unwrap();
    assert!(matches!(
        turn(&mut s, "forever").await.unwrap_err(),
        TurnError::RoundLimit(3)
    ));
    assert_eq!(s.transcript().len(), 7, "user + 3 × (assistant, results)");
    assert_eq!(s.transcript()[6]["role"], "user");

    // Zero is read as one, and reported as what ran.
    let mut s = Session::new(
        f.kernel.clone(),
        SessionConfig {
            max_rounds: 0,
            ..config("/endless-tools")
        },
    )
    .unwrap();
    assert!(matches!(
        turn(&mut s, "once").await.unwrap_err(),
        TurnError::RoundLimit(1)
    ));
    assert_eq!(s.transcript().len(), 3);
}

/// After a turn that did not complete, the next turn's text joins the
/// trailing user message, so the provider never sees two user messages
/// in a row and every stored tool call stays answered.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_turn_joins_the_trailing_user_message_after_a_failed_one() {
    let f = fixture();
    // One attempt only: the first turn fails on the stub's first answer.
    let mut s = Session::new(
        f.kernel.clone(),
        SessionConfig {
            retry: gwennol_core::RetryPolicy {
                max_attempts: 1,
                ..Default::default()
            },
            ..config("/retry-once")
        },
    )
    .unwrap();
    let before = f.stub.requests_for("/retry-once").len();
    assert!(matches!(
        turn(&mut s, "first").await.unwrap_err(),
        TurnError::Provider(_)
    ));
    turn(&mut s, "again").await.unwrap();
    let sent = f.stub.requests_for("/retry-once");
    assert_eq!(sent.len(), before + 2);
    assert_eq!(
        sent[before + 1]["messages"],
        json!([{"role": "user", "content": [text("first"), text("again")]}])
    );
}

// ------------------------------------------------------- cancellation

/// The done-when: cancelling mid-stream tears the turn down cleanly —
/// the loop stops promptly, stores nothing partial, closes its end so
/// the vendor sees the hangup, and the next turn is unaffected.
#[tokio::test(flavor = "multi_thread")]
async fn cancelling_mid_stream_tears_the_turn_down() {
    let mut s = session("/slow");
    let cancel = CancellationToken::new();
    let handle = tokio::spawn({
        let cancel = cancel.clone();
        async move {
            let (outcome, events) = with_events(async { s.turn("stream", &cancel).await }).await;
            (s, outcome, events)
        }
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    cancel.cancel();
    let (s, outcome, events) = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("cancellation ends the turn promptly")
        .unwrap();
    assert!(matches!(outcome.unwrap_err(), TurnError::Cancelled));
    assert!(
        events.iter().all(|e| matches!(e, Event::Text(_))) && !events.is_empty(),
        "some ticks were shown, nothing else: {events:?}"
    );
    assert_eq!(s.transcript(), &[user("stream")]);
    wait_for_hangup("/slow").await;
}

/// Cancelling while a tool runs: the running call is cut short, the
/// call after it never starts, both are answered as interrupted, the
/// exchange is kept whole, and the turn ends as cancelled.
#[tokio::test(flavor = "multi_thread")]
async fn cancelling_during_a_tool_call_answers_the_rest_as_interrupted() {
    let mut s = session("/slow-tool");
    let cancel = CancellationToken::new();
    let handle = tokio::spawn({
        let cancel = cancel.clone();
        async move {
            let (outcome, events) = with_events(async { s.turn("sleep", &cancel).await }).await;
            (s, outcome, events)
        }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    cancel.cancel();
    let (s, outcome, events) = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("cancellation ends the tool step promptly")
        .unwrap();
    assert!(matches!(outcome.unwrap_err(), TurnError::Cancelled));
    let slow = tool_call("c1", "slow", json!({}));
    assert_eq!(
        events,
        vec![
            Event::ToolCall(slow.clone()),
            Event::ToolFailed {
                call: slow,
                error: "interrupted: the turn was cancelled while this call was running; it may have run in part or in full".into()
            },
        ],
        "the second call never started"
    );
    let transcript = s.transcript();
    assert_eq!(transcript.len(), 3, "the exchange is kept whole");
    assert_eq!(
        transcript[2]["content"],
        json!([
            {"type": "tool_result", "tool_use_id": "c1", "is_error": true,
             "content": "interrupted: the turn was cancelled while this call was running; it may have run in part or in full"},
            {"type": "tool_result", "tool_use_id": "c2", "is_error": true,
             "content": "interrupted: the turn was cancelled before this call started"},
        ]),
        "the cut-off call and the never-started one are told apart"
    );
    // The next turn continues from there: its text joins the results.
    let mut s = s;
    turn(&mut s, "carry on").await.unwrap();
    assert_eq!(s.transcript()[2]["content"][2], text("carry on"));
}

/// Cancelling while an approval is open withdraws the question: the
/// turn ends without the operator ever answering.
#[tokio::test(flavor = "multi_thread")]
async fn cancelling_at_an_open_approval_withdraws_it() {
    let f = fixture();
    let gate = f.harness.gates.gate("gated_tool");
    let mut s = session("/gated-tool");
    let cancel = CancellationToken::new();
    let handle = tokio::spawn({
        let cancel = cancel.clone();
        async move {
            let outcome = s.turn("ask", &cancel).await;
            (s, outcome)
        }
    });
    tokio::time::timeout(Duration::from_secs(10), gate.arrived.notified())
        .await
        .expect("the tool asked the operator");
    cancel.cancel();
    let (s, outcome) = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("the withdrawn approval does not hold the turn")
        .unwrap();
    assert!(matches!(outcome.unwrap_err(), TurnError::Cancelled));
    assert_eq!(s.transcript()[2]["content"][0]["is_error"], true);
    assert_eq!(
        gate.release.available_permits(),
        0,
        "the operator never answered"
    );
}
