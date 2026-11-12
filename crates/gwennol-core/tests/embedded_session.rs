//! A session on a kernel this crate did not build: the embedder's path
//! through the loop.
//!
//! This crate has no host to boot, and this binary links none of the host's
//! native step bodies. Its kernels carry
//! kernel intrinsics (`return`, `ifs`), a buffered provider, and fixture tools;
//! the provider and each tool declare a secret so the kernel consults the
//! resolver on every invocation (`gwead::kernel::secrets`: a non-empty declaration is what
//! makes the kernel call it). The recording resolver is how a test
//! observes the execution context each invocation ran under.

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use gwead::kernel::native_impls::NativeStepImplTable;
use gwead::kernel::secrets::{SecretError, SecretRequest, SecretResolver};
use gwead::kernel::{Kernel, KernelConfig};
use gwead::serde_json::{Map, Value, json};
use gwead::tokio_util::sync::CancellationToken;
use gwennol_core::spi::HarvestError;
use gwennol_core::{Event, EventSink, Session, SessionConfig, SessionError, ToolCall, spi};

const PROVIDER: &str = "embed_llm";

/// `(subject, exec_ctx)` of every secret request, in order.
type Seen = Arc<Mutex<Vec<(String, Value)>>>;

struct Recording(Seen);

impl SecretResolver for Recording {
    fn resolve<'a>(
        &'a self,
        request: SecretRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<Value, SecretError>> + Send + 'a>> {
        self.0.lock().unwrap().push((
            request.subject.to_string(),
            request.exec_ctx.as_value().clone(),
        ));
        Box::pin(async { Ok(json!({})) })
    }
}

#[derive(Default)]
struct Recorder(Mutex<Vec<Event>>);

impl EventSink for Recorder {
    fn emit(&self, event: Event) {
        self.0.lock().unwrap().push(event);
    }
}

fn tool(plugin: &str, name: &str) -> Value {
    json!({
        "name": plugin, "version": "0.0.0",
        "description": format!("test fixture tool {name}"),
        "roles": [spi::tool::ROLE],
        "usesSecrets": ["k"],
        "actions": {"call": {
            "tool": {
                "name": name,
                "description": format!("The {name} fixture."),
                "parameters": {"type": "object", "additionalProperties": false, "properties": {}}
            },
            "steps": [{"id": "out", "type": "return", "params": {"value": {
                "content": "ok", "is_error": false}}}]
        }}
    })
}

/// A `tool` block on a `call` action of a plugin that never claimed the
/// `TOOL` role: the raw engine list has it, the harvest must not.
fn decoy() -> Value {
    let mut plugin = tool("decoy_call", "sneaky");
    plugin.as_object_mut().unwrap().remove("roles");
    plugin
}

fn provider() -> Value {
    json!({
        "name": PROVIDER, "version": "0.0.0",
        "description": "test fixture: buffered answers from $config.answers by round",
        "roles": [spi::llm_chat::ROLE],
        "usesSecrets": ["k"],
        "actions": {"chat": {"steps": [
            {"id": "pick", "type": "ifs", "params": {"ifs": [
                {"test": "$input.messages[1].role == 'assistant'",
                 "then": [{"id": "r1", "type": "return", "params": {"value": "{{$config.answers[1]}}"}}]},
                {"then": [{"id": "r0", "type": "return", "params": {"value": "{{$config.answers[0]}}"}}]}
            ]}}
        ]}}
    })
}

/// A kernel with the contracts, the buffered provider, and `tools`.
fn kernel(tools: Vec<Value>) -> (Arc<Kernel>, Seen) {
    let seen = Seen::default();
    let config = KernelConfig::default()
        .with_native_step_impls(NativeStepImplTable::discover().unwrap())
        .with_secret_resolver(Arc::new(Recording(seen.clone())));
    let mut kernel = Kernel::boot(config).unwrap();
    spi::register(&mut kernel).unwrap();
    for plugin in std::iter::once(provider()).chain(tools) {
        kernel
            .register_plugin_from_json(&plugin.to_string())
            .unwrap_or_else(|e| panic!("{}: {e}", plugin["name"]));
    }
    (kernel.into_arc(), seen)
}

fn answer(blocks: Vec<Value>, stop: &str) -> Value {
    json!({
        "message": {"role": "assistant", "content": blocks},
        "stop_reason": stop,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
}

/// A config whose provider asks for `tool` once, then says "done".
fn config(tool: &str) -> SessionConfig {
    let answers = vec![
        answer(
            vec![
                json!({"type": "text", "text": "calling"}),
                json!({"type": "tool_use", "id": "c1", "name": tool, "input": {}}),
            ],
            "tool_use",
        ),
        answer(vec![json!({"type": "text", "text": "done"})], "end_turn"),
    ];
    SessionConfig {
        provider: Some(PROVIDER.into()),
        stream: false,
        plugin_configs: [(PROVIDER.to_string(), json!({"answers": answers}))].into(),
        ..SessionConfig::default()
    }
}

fn names(set: &[&str]) -> BTreeSet<String> {
    set.iter().map(ToString::to_string).collect()
}

fn context(entries: Value) -> Map<String, Value> {
    entries.as_object().unwrap().clone()
}

fn one(seen: &Seen, subject: &str) -> Value {
    let seen = seen.lock().unwrap();
    let mut hits = seen.iter().filter(|(s, _)| s == subject);
    let (_, ctx) = hits
        .next()
        .unwrap_or_else(|| panic!("no request for {subject}: {seen:?}"));
    assert!(hits.next().is_none(), "one request for {subject}: {seen:?}");
    ctx.clone()
}

#[tokio::test]
async fn a_sink_session_receives_its_events_in_order() {
    let (kernel, _) = kernel(vec![tool("tool_a", "alpha")]);
    let sink = Arc::new(Recorder::default());
    let mut s = Session::with_events(kernel, config("alpha"), sink.clone()).unwrap();
    s.turn("go", &CancellationToken::new()).await.unwrap();
    let call = ToolCall {
        id: Some("c1".into()),
        name: "alpha".into(),
        arguments: "{}".into(),
    };
    assert_eq!(
        *sink.0.lock().unwrap(),
        vec![
            Event::Text("calling".into()),
            Event::ToolCall(call.clone()),
            Event::ToolResult {
                call,
                content: "ok".into(),
                is_error: false
            },
            Event::Text("done".into()),
            Event::TurnComplete,
        ]
    );
}

/// Gwead submits two entries of its own to this table,
/// `gwead.intrinsics.throw_error` and `gwead.intrinsics.let`; the table
/// holds those two and nothing else, so none of the host's six step bodies
/// is among them. `NativeStepImplTable` exposes `get` and `len` but no way
/// to list its names, so the count is the only way to say "nothing else".
#[test]
fn the_table_holds_only_the_kernels_own_entries() {
    let table = NativeStepImplTable::discover().unwrap();
    for name in ["gwead.intrinsics.throw_error", "gwead.intrinsics.let"] {
        assert!(table.get(name).is_some(), "{name} is the kernel's own");
    }
    assert_eq!(table.len(), 2, "only gwead's own entries");
    for name in [
        "gwennol.host_fs.read",
        "gwennol.host_fs.write",
        "gwennol.host_fs.list",
        "gwennol.host_process.run",
        "gwennol.host_http.get",
        "gwennol.host_http.post",
    ] {
        assert!(table.get(name).is_none(), "{name} is linked");
    }
}

#[tokio::test]
async fn the_embedders_context_reaches_the_provider_and_the_tool() {
    let (kernel, seen) = kernel(vec![tool("tool_a", "alpha")]);
    let config = SessionConfig {
        context: context(json!({"principal": "p1"})),
        ..config("alpha")
    };
    let mut s = Session::with_events(kernel, config, Arc::new(Recorder::default())).unwrap();
    s.turn("go", &CancellationToken::new()).await.unwrap();
    let seen_provider: Vec<Value> = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|(subject, _)| subject == PROVIDER)
        .map(|(_, ctx)| ctx.clone())
        .collect();
    assert_eq!(seen_provider, vec![json!({"principal": "p1"}); 2]);
    let tool_ctx = one(&seen, "tool_a");
    assert_eq!(tool_ctx["principal"], "p1");
    assert_eq!(tool_ctx["gwennol"]["toolCall"]["name"], "alpha");
}

#[tokio::test]
async fn an_empty_context_leaves_the_provider_round_without_one() {
    let (kernel, seen) = kernel(vec![tool("tool_a", "alpha")]);
    let mut s =
        Session::with_events(kernel, config("alpha"), Arc::new(Recorder::default())).unwrap();
    s.turn("go", &CancellationToken::new()).await.unwrap();
    let provider_contexts: Vec<Value> = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|(subject, _)| subject == PROVIDER)
        .map(|(_, ctx)| ctx.clone())
        .collect();
    assert_eq!(provider_contexts, vec![Value::Null; 2]);
    let tool_ctx = one(&seen, "tool_a");
    assert_eq!(tool_ctx["gwennol"]["toolCall"]["name"], "alpha");
    assert_eq!(tool_ctx.as_object().unwrap().len(), 1);
}

#[tokio::test]
async fn the_loops_own_context_key_is_refused() {
    let (kernel, _) = kernel(vec![tool("tool_a", "alpha")]);
    let config = SessionConfig {
        context: context(json!({"gwennol": {}})),
        ..config("alpha")
    };
    assert!(matches!(
        Session::with_events(kernel, config, Arc::new(Recorder::default())),
        Err(SessionError::ReservedContextKey)
    ));
}

#[tokio::test]
async fn a_session_offers_the_tool_plugins_it_selected() {
    // Plugin names sort opposite to the tool names they carry, so an
    // ordering by plugin name would show. The sort of a selection is pinned by
    // `a_selection_is_offered_sorted_by_tool_name`.
    let (kernel, _) = kernel(vec![
        tool("tool_x", "gamma"),
        tool("tool_z", "alpha"),
        tool("tool_y", "beta"),
        decoy(),
    ]);
    let session = |selection: Option<BTreeSet<String>>| {
        Session::with_events(
            kernel.clone(),
            SessionConfig {
                tool_plugins: selection,
                ..config("alpha")
            },
            Arc::new(Recorder::default()),
        )
    };
    let offered = |s: &Session| -> Vec<String> {
        s.chat_input()["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(
        offered(&session(Some(names(&["tool_y", "tool_z"]))).unwrap()),
        ["alpha", "beta"]
    );
    assert_eq!(offered(&session(None).unwrap()), ["alpha", "beta", "gamma"]);
    let none = session(Some(names(&[]))).unwrap();
    assert!(none.chat_input().get("tools").is_none());
    for missing in ["nope", PROVIDER, "decoy_call"] {
        match session(Some(names(&[missing]))) {
            Err(SessionError::Harvest(HarvestError::NoSuchToolPlugin(name))) => {
                assert_eq!(name, missing);
            }
            other => panic!("{missing}: {other:?}"),
        }
    }
}

/// The kernel hands the harvest its tool descriptors in an order that varies
/// from process to process, so one pair of tools sorts correctly by chance in
/// about half the runs. Eight tools sort correctly by chance in about one run
/// in 40320 (8!); the plugin names run opposite to the tool names.
#[tokio::test]
async fn a_selection_is_offered_sorted_by_tool_name() {
    let plugins: Vec<(String, String)> = (0..8)
        .map(|i| (format!("plugin_{}", 7 - i), format!("tool_{i}")))
        .collect();
    let (kernel, _) = kernel(plugins.iter().map(|(p, t)| tool(p, t)).collect());
    let selection: BTreeSet<String> = plugins.iter().map(|(p, _)| p.clone()).collect();
    let session = Session::with_events(
        kernel,
        SessionConfig {
            tool_plugins: Some(selection),
            ..config("tool_0")
        },
        Arc::new(Recorder::default()),
    )
    .unwrap();
    let input = session.chat_input();
    let offered: Vec<&str> = input["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    let expected: Vec<String> = (0..8).map(|i| format!("tool_{i}")).collect();
    assert_eq!(offered, expected);
}

#[tokio::test]
async fn a_duplicate_tool_name_is_refused_only_among_the_selected() {
    let (kernel, _) = kernel(vec![
        tool("dup_a", "same"),
        tool("dup_b", "same"),
        tool("tool_a", "alpha"),
    ]);
    let session = |selection: Option<BTreeSet<String>>| {
        Session::with_events(
            kernel.clone(),
            SessionConfig {
                tool_plugins: selection,
                ..config("alpha")
            },
            Arc::new(Recorder::default()),
        )
    };
    session(Some(names(&["dup_a", "tool_a"]))).unwrap();
    session(Some(names(&["dup_b"]))).unwrap();
    for selection in [Some(names(&["dup_a", "dup_b"])), None] {
        assert!(
            matches!(
                session(selection.clone()),
                Err(SessionError::Harvest(HarvestError::DuplicateToolName(n))) if n == "same"
            ),
            "{selection:?}"
        );
    }
}

/// One action of a [`multi`] plugin: its name, the tool name it declares
/// (or none, for a helper), its `parameters`, and what its one `return`
/// step gives.
struct Action {
    action: &'static str,
    tool: Option<&'static str>,
    parameters: Value,
    returns: Value,
}

fn action(action: &'static str, tool: Option<&'static str>, content: &str) -> Action {
    Action {
        action,
        tool,
        parameters: json!({"type": "object", "additionalProperties": false, "properties": {}}),
        returns: json!({"content": content, "is_error": false}),
    }
}

/// A `TOOL` plugin with one action per entry of `actions`.
fn multi(plugin: &str, actions: Vec<Action>) -> Value {
    let actions: Map<String, Value> = actions
        .into_iter()
        .map(|a| {
            let mut body = json!({"steps": [
                {"id": "out", "type": "return", "params": {"value": a.returns}}
            ]});
            if let Some(name) = a.tool {
                body["tool"] = json!({
                    "name": name,
                    "description": format!("The {name} fixture."),
                    "parameters": a.parameters
                });
            }
            (a.action.to_string(), body)
        })
        .collect();
    json!({
        "name": plugin, "version": "0.0.0",
        "description": "test fixture plugin with several actions",
        "roles": [spi::tool::ROLE],
        "usesSecrets": ["k"],
        "actions": actions
    })
}

fn session(
    kernel: &Arc<Kernel>,
    config: SessionConfig,
    sink: Arc<Recorder>,
) -> Result<Session, SessionError> {
    Session::with_events(kernel.clone(), config, sink)
}

fn offered(s: &Session) -> Vec<String> {
    s.chat_input()
        .get("tools")
        .map(|tools| {
            tools
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["name"].as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn with_selection(tool: &str, selection: Option<BTreeSet<String>>) -> SessionConfig {
    SessionConfig {
        tool_plugins: selection,
        ..config(tool)
    }
}

#[tokio::test]
async fn a_plugin_offers_one_tool_for_each_action_that_carries_a_tool_block() {
    let (kernel, _) = kernel(vec![
        multi(
            "multi",
            vec![
                action("call", Some("mu"), "m"),
                action("second", Some("alpha"), "s"),
                action("helper", None, "h"),
            ],
        ),
        tool("tool_a", "kappa"),
    ]);
    let offer = |selection| {
        let s = session(
            &kernel,
            with_selection("alpha", selection),
            Arc::new(Recorder::default()),
        );
        offered(&s.unwrap())
    };
    assert_eq!(offer(None), ["alpha", "kappa", "mu"]);
    assert_eq!(offer(Some(names(&["multi"]))), ["alpha", "mu"]);
    assert_eq!(offer(Some(names(&["tool_a"]))), ["kappa"]);
}

#[tokio::test]
async fn a_tool_runs_the_action_that_declared_it() {
    let (kernel, seen) = kernel(vec![multi(
        "multi",
        vec![
            action("call", Some("mu"), "from call"),
            action("second", Some("alpha"), "from second"),
        ],
    )]);
    let sink = Arc::new(Recorder::default());
    let mut s = session(&kernel, config("alpha"), sink.clone()).unwrap();
    s.turn("go", &CancellationToken::new()).await.unwrap();
    let call = ToolCall {
        id: Some("c1".into()),
        name: "alpha".into(),
        arguments: "{}".into(),
    };
    assert_eq!(
        *sink.0.lock().unwrap(),
        vec![
            Event::Text("calling".into()),
            Event::ToolCall(call.clone()),
            Event::ToolResult {
                call,
                content: "from second".into(),
                is_error: false
            },
            Event::Text("done".into()),
            Event::TurnComplete,
        ]
    );
    assert_eq!(one(&seen, "multi")["gwennol"]["toolCall"]["name"], "alpha");
}

#[tokio::test]
async fn a_tool_plugin_needs_no_call_action() {
    let (kernel, _) = kernel(vec![multi(
        "nocall",
        vec![action("lookup", Some("gamma"), "found")],
    )]);
    let sink = Arc::new(Recorder::default());
    let mut s = session(&kernel, config("gamma"), sink.clone()).unwrap();
    assert_eq!(offered(&s), ["gamma"]);
    s.turn("go", &CancellationToken::new()).await.unwrap();
    assert!(
        sink.0.lock().unwrap().iter().any(|e| matches!(
            e,
            Event::ToolResult { content, is_error: false, .. } if content == "found"
        )),
        "{:?}",
        sink.0.lock().unwrap()
    );
}

#[tokio::test]
async fn a_tool_plugin_with_no_tool_block_offers_nothing_and_cannot_be_selected() {
    let hollow = || multi("hollow", vec![action("only", None, "x")]);
    let (kernel_both, _) = kernel(vec![hollow(), tool("tool_a", "alpha")]);
    let build = |selection| {
        session(
            &kernel_both,
            with_selection("alpha", selection),
            Arc::new(Recorder::default()),
        )
    };
    assert_eq!(offered(&build(None).unwrap()), ["alpha"]);
    for selection in [names(&["hollow"]), names(&["hollow", "tool_a"])] {
        match build(Some(selection.clone())) {
            Err(SessionError::Harvest(HarvestError::NoSuchToolPlugin(name))) => {
                assert_eq!(name, "hollow");
            }
            other => panic!("{selection:?}: {other:?}"),
        }
    }
    let (kernel_alone, _) = kernel(vec![hollow()]);
    let s = session(
        &kernel_alone,
        config("alpha"),
        Arc::new(Recorder::default()),
    )
    .unwrap();
    assert!(s.chat_input().get("tools").is_none());
}

#[tokio::test]
async fn two_actions_of_one_plugin_cannot_share_a_tool_name() {
    let (kernel, _) = kernel(vec![
        multi(
            "twin",
            vec![
                action("a", Some("same"), "a"),
                action("b", Some("same"), "b"),
            ],
        ),
        tool("tool_a", "alpha"),
    ]);
    let build = |selection| {
        session(
            &kernel,
            with_selection("alpha", selection),
            Arc::new(Recorder::default()),
        )
    };
    for selection in [None, Some(names(&["twin"]))] {
        assert!(
            matches!(
                build(selection.clone()),
                Err(SessionError::Harvest(HarvestError::DuplicateToolName(n))) if n == "same"
            ),
            "{selection:?}"
        );
    }
    build(Some(names(&["tool_a"]))).unwrap();
}

#[tokio::test]
async fn every_offered_action_is_held_to_its_own_schema_and_the_result_shape() {
    let strict = Action {
        parameters: json!({
            "type": "object", "required": ["q"],
            "properties": {"q": {"type": "string"}}
        }),
        ..action("strict", Some("strict"), "unreached")
    };
    let broken = Action {
        returns: json!({"content": "x", "extra": 1}),
        ..action("broken", Some("broken"), "")
    };
    let (kernel, _) = kernel(vec![multi("multi", vec![strict, broken])]);
    for (tool, expected) in [
        ("strict", "do not match its schema"),
        ("broken", "returned a malformed result"),
    ] {
        let sink = Arc::new(Recorder::default());
        let mut s = session(&kernel, config(tool), sink.clone()).unwrap();
        let outcome = s.turn("go", &CancellationToken::new()).await.unwrap();
        assert_eq!(outcome.rounds, 2, "{tool}");
        let events = sink.0.lock().unwrap();
        let failures: Vec<&Event> = events
            .iter()
            .filter(|e| matches!(e, Event::ToolFailed { .. }))
            .collect();
        assert_eq!(failures.len(), 1, "{tool}: {events:?}");
        assert!(
            format!("{:?}", failures[0]).contains(expected),
            "{tool}: {failures:?}"
        );
        assert!(
            !events.iter().any(|e| matches!(e, Event::ToolResult { .. })),
            "{tool}: {events:?}"
        );
    }
}
