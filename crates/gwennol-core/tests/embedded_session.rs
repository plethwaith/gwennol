//! A session on a kernel `boot` did not build, in a process where no
//! host is installed: the embedder's path through the loop.
//!
//! This binary never calls `boot` or `boot_with`. Its kernels carry only
//! kernel intrinsics (`return`), a buffered provider, and fixture tools,
//! each declaring a secret so the kernel consults the resolver on every
//! invocation (`gwead::kernel::secrets`: a non-empty declaration is what
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
async fn a_sink_session_runs_without_an_installed_host() {
    let (kernel, _) = kernel(vec![tool("tool_a", "alpha")]);
    assert!(matches!(
        Session::new(kernel.clone(), config("alpha")),
        Err(SessionError::NotBooted)
    ));
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

#[tokio::test]
async fn run_on_a_sink_session_returns_without_a_turn() {
    let (kernel, seen) = kernel(vec![tool("tool_a", "alpha")]);
    let mut s =
        Session::with_events(kernel, config("alpha"), Arc::new(Recorder::default())).unwrap();
    s.run(&CancellationToken::new()).await.unwrap();
    assert!(seen.lock().unwrap().is_empty(), "no provider call");
    assert!(s.transcript().is_empty());
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
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .filter(|(subject, _)| subject == PROVIDER)
            .all(|(_, ctx)| ctx.is_null())
    );
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
    let (kernel, _) = kernel(vec![
        tool("tool_c", "gamma"),
        tool("tool_a", "alpha"),
        tool("tool_b", "beta"),
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
        offered(&session(Some(names(&["tool_b", "tool_a"]))).unwrap()),
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
