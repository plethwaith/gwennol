//! End-to-end: the bundled SPI contracts are registered at boot, plugins
//! are checked against them, and fixture implementations of each role are
//! dispatched *by role* through a real Gwead kernel. The tool-call wire
//! shapes exercised here are the ones `docs/SPI.md` documents.
//!
//! The kernel never validates payloads against the contract schemas, so
//! this suite is where conformance is enforced: every fixture payload —
//! chat inputs, buffered and streamed outputs, each stream event, tool
//! results — is validated against the schema it claims to satisfy.
//!
//! The host is a process singleton, so this binary boots one kernel with
//! every fixture plugin registered up front and shares it across tests.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex, OnceLock};

use boon::{Compiler, SchemaIndex, Schemas};
use gwead::kernel::Kernel;
use gwead::kernel::streams::{STREAM_EOF, StreamRegistry, read_async_shared};
use gwead::serde_json::{Value, json};
use gwennol_core::{ApprovalRequest, Decision, Event, Operator, Turn, spi};

/// Allows everything, knows no secrets: contract dispatch needs no policy.
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

struct Fixture {
    kernel: Arc<Kernel>,
}

fn fixture() -> &'static Fixture {
    static F: OnceLock<Fixture> = OnceLock::new();
    F.get_or_init(|| {
        // Canonicalised like the M1 fixture: macOS tempdirs sit behind
        // the /var symlink, and future host_fs tests here would compare
        // approval paths against workspace joins.
        let workspace = tempfile::tempdir().unwrap().keep().canonicalize().unwrap();
        let mut kernel = gwennol_core::boot(Arc::new(Permissive), workspace).unwrap();
        for plugin in [fixture_provider(), fixture_tool(), decoy_tool()] {
            kernel
                .register_plugin_from_json(&plugin.to_string())
                .unwrap();
        }
        Fixture {
            kernel: kernel.into_arc(),
        }
    })
}

// ------------------------------------------------ contract validation

/// The contract schemas, compiled for instance validation. boon compiles
/// JSON-pointer fragments of a registered document directly, and
/// `#/$defs/…` references resolve against the document root, so each
/// subschema is addressed in place — no synthetic copies.
struct Contracts {
    schemas: Schemas,
    chat_input: SchemaIndex,
    chat_output: SchemaIndex,
    stream_event: SchemaIndex,
    call_output: SchemaIndex,
}

fn contracts() -> &'static Contracts {
    static C: OnceLock<Contracts> = OnceLock::new();
    C.get_or_init(|| {
        const LLM: &str = "http://gwennol.dev/spi/llm_chat.json";
        const TOOL: &str = "http://gwennol.dev/spi/tool.json";
        let mut compiler = Compiler::new();
        let mut schemas = Schemas::new();
        for (url, document) in [
            (LLM, spi::llm_chat::DEFINITION),
            (TOOL, spi::tool::DEFINITION),
        ] {
            compiler
                .add_resource(url, gwead::serde_json::from_str(document).unwrap())
                .unwrap();
        }
        let [chat_input, chat_output, stream_event, call_output] = [
            format!("{LLM}#/actions/chat/input"),
            format!("{LLM}#/actions/chat/output"),
            format!("{LLM}#/streamEventShape"),
            format!("{TOOL}#/actions/call/output"),
        ]
        .map(|url| {
            compiler
                .compile(&url, &mut schemas)
                .unwrap_or_else(|e| panic!("{url} is not a compilable schema: {e}"))
        });
        Contracts {
            schemas,
            chat_input,
            chat_output,
            stream_event,
            call_output,
        }
    })
}

#[track_caller]
fn assert_conforms(schema: SchemaIndex, instance: &Value) {
    if let Err(e) = contracts().schemas.validate(instance, schema) {
        panic!("payload violates the contract schema: {e:#}\npayload: {instance}");
    }
}

// -------------------------------------------------------------- fixtures

/// A canned `LLM_CHAT` implementation. Streamed call: relays NDJSON
/// events fetched from the stub server (`$config.stream_url`) as its
/// stream handle — the same composition a real provider uses, minus the
/// protocol translation. First buffered call: text plus two parallel
/// calls to the first offered tool. Follow-up call (three or more
/// messages, the third leading with a tool result): closes the turn with
/// text quoting the first result.
fn fixture_provider() -> Value {
    json!({
        "name": "fixture_llm", "version": "0.0.0",
        "description": "Canned LLM_CHAT fixture: two tool calls, then a closing turn.",
        "roles": [gwennol_core::spi::llm_chat::ROLE],
        "permissions": ["step_type:host_http.get", "network:egress:127.0.0.1"],
        "actions": {
            "chat": {
                "steps": [
                    {"id": "branch", "type": "ifs", "params": {"ifs": [
                        {
                            "test": "$input.stream == true",
                            "then": [
                                {"id": "fetch", "type": "host_http.get", "params": {
                                    "url": "{{$config.stream_url}}", "stream": true}},
                                {"id": "streamed", "type": "return", "params": {"value": {
                                    "stream": "{{$steps.fetch.result.body}}"
                                }}}
                            ]
                        },
                        {
                            "test": "$input.messages[2].content[0].type == 'tool_result'",
                            "then": [{"id": "closing", "type": "return", "params": {"value": {
                                "message": {"role": "assistant", "content": [
                                    {"type": "text", "text": "the tool said: {{$input.messages[2].content[0].content}}"}
                                ]},
                                "stop_reason": "end_turn",
                                "usage": {"input_tokens": 3, "output_tokens": 4}
                            }}}]
                        },
                        {
                            "then": [{"id": "opening", "type": "return", "params": {"value": {
                                "message": {"role": "assistant", "content": [
                                    {"type": "text", "text": "calling the tool twice"},
                                    {"type": "tool_use", "id": "call_1",
                                     "name": "{{$input.tools[0].name}}",
                                     "input": {"message": "hello from the model"}},
                                    {"type": "tool_use", "id": "call_2",
                                     "name": "{{$input.tools[0].name}}",
                                     "input": {"message": "and again"}}
                                ]},
                                "stop_reason": "tool_use",
                                "usage": {"input_tokens": 1, "output_tokens": 2}
                            }}}]
                        }
                    ]}}
                ]
            }
        }
    })
}

/// A canned `TOOL` implementation that echoes its argument. The `tool`
/// block is the single declaration of its schema — what the descriptor
/// harvest and the `tools` wire shape are tested against.
fn fixture_tool() -> Value {
    json!({
        "name": "fixture_echo", "version": "0.0.0",
        "description": "Canned TOOL fixture: echoes its message argument.",
        "roles": [gwennol_core::spi::tool::ROLE],
        "actions": {
            "call": {
                "tool": {
                    "name": "fixture_echo",
                    "description": "Echo a message back.",
                    "parameters": {
                        "type": "object",
                        "required": ["message"],
                        "additionalProperties": false,
                        "properties": {"message": {"type": "string"}}
                    }
                },
                "steps": [
                    {"id": "out", "type": "return", "params": {"value": {
                        "content": "echo: {{$input.message}}",
                        "is_error": false
                    }}}
                ]
            }
        }
    })
}

/// A plugin carrying a `tool` block that never claimed the `TOOL` role:
/// Gwead's raw descriptor list includes it anyway, and the docs/SPI.md
/// harvest rules must screen it out before anything reaches the model.
fn decoy_tool() -> Value {
    json!({
        "name": "decoy", "version": "0.0.0",
        "description": "test fixture: a tool block outside the TOOL role",
        "actions": {
            "tempt": {
                "tool": {
                    "name": "sneaky_tool",
                    "description": "Must never be offered to the model.",
                    "parameters": {"type": "object"}
                },
                "steps": [
                    {"id": "out", "type": "return", "params": {"value": {"content": "gotcha"}}}
                ]
            }
        }
    })
}

/// The `tools` input for `chat`, mapped from [`spi::harvest_tools`] —
/// the core's one implementation of the docs/SPI.md harvest rules, never
/// the raw descriptor list.
fn wire_tools(kernel: &Kernel) -> Value {
    Value::Array(
        spi::harvest_tools(kernel)
            .unwrap()
            .into_iter()
            .map(|d| {
                json!({
                    "name": d.tool_name,
                    "description": d.description,
                    "input_schema": d.parameters
                })
            })
            .collect(),
    )
}

/// Run one tool call the milestone-5 way: the model named a tool, the
/// *harvested* descriptor (filtered and checked, not the raw list) says
/// what to execute — by plugin and action, not by role, since many
/// `TOOL` plugins register at once.
async fn call_by_descriptor(f: &Fixture, call: &Value) -> Value {
    let name = call["name"].as_str().unwrap();
    let descriptors = spi::harvest_tools(&f.kernel).unwrap();
    let d = descriptors
        .iter()
        .find(|d| d.tool_name == name)
        .unwrap_or_else(|| panic!("no descriptor for tool {name}"));
    let out = f
        .kernel
        .execute(&d.plugin_key, &d.action_name, call["input"].clone())
        .with_config(&json!({}))
        .run()
        .await
        .unwrap()
        .output;
    assert_conforms(contracts().call_output, &out);
    out
}

// ------------------------------------------------------- stream stub

/// One canned model turn as `streamEventShape` NDJSON: text deltas, a
/// whole tool call, and the final `end` event.
const STREAM_BODY: &str = concat!(
    r#"{"type":"text","text":"hel"}"#,
    "\n",
    r#"{"type":"text","text":"lo"}"#,
    "\n",
    r#"{"type":"tool_use","id":"call_s1","name":"fixture_echo","input":{"message":"streamed"}}"#,
    "\n",
    r#"{"type":"end","stop_reason":"end_turn","usage":{"input_tokens":5,"output_tokens":2}}"#,
    "\n",
);

/// [`STREAM_BODY`] cut off before the `end` event: the upstream died
/// mid-turn and the relay saw a clean close. What the contract calls a
/// failed turn.
const TRUNCATED_BODY: &str = concat!(
    r#"{"type":"text","text":"hel"}"#,
    "\n",
    r#"{"type":"text","text":"lo"}"#,
    "\n",
);

/// Minimal HTTP/1.1 server: `/truncated` gets [`TRUNCATED_BODY`], any
/// other path [`STREAM_BODY`], each written in two flushed halves so the
/// client genuinely streams. One process-wide instance — the path
/// routing exists so a single listener serves every streaming test —
/// and a failed `accept` (possible under parallel CI load) skips that
/// connection rather than killing the accept thread. Returns the base
/// URL.
fn stream_stub() -> &'static str {
    static URL: OnceLock<String> = OnceLock::new();
    URL.get_or_init(|| {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                std::thread::spawn(move || {
                    // Read until the blank line ending the request head;
                    // the fixture's GET carries no body.
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 1024];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        let n = s.read(&mut tmp).unwrap();
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                    }
                    let head = String::from_utf8_lossy(&buf);
                    let path = head.split_whitespace().nth(1).unwrap_or("/");
                    let body: &[u8] = if path.ends_with("/truncated") {
                        TRUNCATED_BODY.as_bytes()
                    } else {
                        STREAM_BODY.as_bytes()
                    };
                    write!(
                        s,
                        "HTTP/1.1 200 OK\r\ncontent-type: application/x-ndjson\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    )
                    .unwrap();
                    // Split on bytes: a UTF-8 boundary mid-body must not
                    // matter to the halving.
                    let (a, b) = body.split_at(body.len() / 2);
                    s.write_all(a).unwrap();
                    s.flush().unwrap();
                    s.write_all(b).unwrap();
                });
            }
        });
        format!("http://127.0.0.1:{port}")
    })
}

/// Stream a chat turn from `url` through the fixture provider and return
/// the parsed events, validating the handle result and every line
/// against the contract on the way.
async fn stream_events_from(f: &Fixture, url: String) -> Vec<Value> {
    // `execute_by_role` cannot carry a streams table, so a streaming
    // caller resolves the role first and executes the winner — exactly
    // what the milestone-5 loop will do.
    let provider = f
        .kernel
        .role_candidates(None, spi::llm_chat::ROLE)
        .into_iter()
        .next()
        .expect("an LLM_CHAT fulfiller");
    assert_eq!(provider, "fixture_llm");

    let input = json!({"messages": [user_text("stream please")], "stream": true});
    assert_conforms(contracts().chat_input, &input);
    let streams = Arc::new(Mutex::new(StreamRegistry::new()));
    let out = f
        .kernel
        .execute(&provider, spi::llm_chat::CHAT, input)
        .with_config(&json!({"stream_url": url}))
        .with_streams(streams.clone())
        .run()
        .await
        .unwrap()
        .output;
    assert_conforms(contracts().chat_output, &out);

    let handle = out["stream"].as_u64().expect("integer stream handle");
    let id = NonZeroU32::new(u32::try_from(handle).unwrap()).unwrap();
    let mut collected = Vec::new();
    let mut buf = [0u8; 7]; // small, so the body takes many reads
    loop {
        let n = read_async_shared(&streams, id, &mut buf).await;
        if n == STREAM_EOF {
            break;
        }
        assert!(n > 0, "stream read returned {n}");
        collected.extend_from_slice(&buf[..n as usize]);
    }

    String::from_utf8(collected)
        .unwrap()
        .lines()
        .map(|l| {
            let event: Value = gwead::serde_json::from_str(l).unwrap();
            assert_conforms(contracts().stream_event, &event);
            event
        })
        .collect()
}

// ------------------------------------------------------- registration

#[test]
fn the_bundled_documents_classify_as_spi_definitions() {
    for (_, definition) in spi::SPI_DEFINITIONS {
        assert!(matches!(
            Kernel::manifest_kind(definition).unwrap(),
            gwead::kernel::ManifestKind::SpiDef
        ));
    }
}

#[test]
fn the_contract_schemas_compile_as_json_schema() {
    // The kernel checks only that these are well-formed documents; that
    // every embedded schema also *compiles* (all $refs resolve) is
    // pinned here, by building the validators the other tests use.
    contracts();
}

#[test]
fn the_conformance_harness_rejects_violating_payloads() {
    // A validator that accepts everything would green every conformance
    // assertion in this file; prove each compiled schema still bites.
    let c = contracts();
    for (schema, bad) in [
        (c.chat_input, json!({"messages": []})),
        (c.chat_output, json!({"stream": 0})),
        // usage is required, on the buffered form and the end event both.
        (
            c.chat_output,
            json!({
                "message": {"role": "assistant", "content": [{"type": "text", "text": "x"}]},
                "stop_reason": "end_turn"
            }),
        ),
        (
            c.stream_event,
            json!({"type": "end", "stop_reason": "end_turn"}),
        ),
        (c.stream_event, json!({"type": "bogus"})),
        (c.call_output, json!({"is_error": true})),
    ] {
        assert!(
            c.schemas.validate(&bad, schema).is_err(),
            "schema accepted a violating payload: {bad}"
        );
    }
}

#[tokio::test]
async fn boot_registers_both_roles() {
    let f = fixture();
    let roles = f.kernel.spi_registry().roles();
    assert!(roles.contains(&spi::llm_chat::ROLE), "{roles:?}");
    assert!(roles.contains(&spi::tool::ROLE), "{roles:?}");
}

/// A claim on `role` without its required action must be refused. Runs
/// on a bare Gwead kernel: registration is `&mut`, and the shared
/// fixture kernel is already behind its `Arc`.
#[track_caller]
fn assert_hollow_claim_rejected(role: &str, required_action: &str) {
    // The reason boot registers the contracts first: with the definition
    // present, an incomplete claim is an error rather than a warning.
    let mut kernel = Kernel::boot(gwead::kernel::KernelConfig::default()).unwrap();
    spi::register(&mut kernel).unwrap();
    let claim = json!({
        "name": "hollow", "version": "0.0.0",
        "description": "claims a role but omits its required action",
        "roles": [role],
        "actions": {"other": {"steps": [
            {"id": "s", "type": "let", "params": {"value": 1}}
        ]}}
    });
    let err = kernel
        .load_manifest(&claim.to_string())
        .register()
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains(required_action), "{msg}");
    assert!(msg.contains(role), "{msg}");
}

#[test]
fn a_provider_missing_the_chat_action_is_rejected() {
    assert_hollow_claim_rejected(spi::llm_chat::ROLE, spi::llm_chat::CHAT);
}

#[test]
fn a_tool_missing_the_call_action_is_rejected() {
    assert_hollow_claim_rejected(spi::tool::ROLE, spi::tool::CALL);
}

#[test]
fn the_harvest_refuses_duplicate_tool_names() {
    // Two TOOL plugins advertising the same tool.name make selection by
    // descriptor ambiguous; spi::harvest_tools must refuse, not pick.
    let mut kernel = Kernel::boot(gwead::kernel::KernelConfig::default()).unwrap();
    spi::register(&mut kernel).unwrap();
    for plugin_name in ["clash_one", "clash_two"] {
        let mut manifest = fixture_tool();
        manifest["name"] = json!(plugin_name);
        kernel
            .register_plugin_from_json(&manifest.to_string())
            .unwrap();
    }
    let err = spi::harvest_tools(&kernel).unwrap_err();
    assert!(
        matches!(&err, spi::HarvestError::DuplicateToolName(name) if name == "fixture_echo"),
        "{err}"
    );
}

// ---------------------------------------------------- dispatch by role

fn user_text(text: &str) -> Value {
    json!({"role": "user", "content": [{"type": "text", "text": text}]})
}

#[tokio::test]
async fn the_harvest_screens_out_tool_blocks_outside_the_role() {
    let f = fixture();
    // Gwead's raw descriptor list contains the decoy…
    assert!(
        f.kernel
            .registry()
            .get_tool_descriptors()
            .iter()
            .any(|d| d.tool_name == "sneaky_tool"),
        "the decoy fixture should be harvestable by the raw engine list"
    );
    // …the docs/SPI.md harvest does not.
    let names: Vec<String> = spi::harvest_tools(&f.kernel)
        .unwrap()
        .into_iter()
        .map(|d| d.tool_name)
        .collect();
    assert_eq!(names, ["fixture_echo"]);
}

#[tokio::test]
async fn the_tool_descriptor_is_harvested_from_the_manifest() {
    // The settled answer to how a tool's schema reaches the model: it is
    // declared once, in the manifest's `tool` block, and harvested.
    let f = fixture();
    let descriptors = f.kernel.registry().get_tool_descriptors();
    let echo = descriptors
        .iter()
        .find(|d| d.tool_name == "fixture_echo")
        .expect("fixture_echo descriptor");
    assert_eq!(echo.plugin_key, "fixture_echo");
    assert_eq!(echo.action_name, "call");
    assert_eq!(echo.parameters["required"], json!(["message"]));
    assert_eq!(
        echo.parameters["properties"]["message"]["type"],
        json!("string")
    );
}

#[tokio::test]
async fn the_provider_is_dispatched_by_role() {
    let f = fixture();
    let input = json!({
        "system": "You are a test fixture.",
        "messages": [user_text("please call the tool")],
        "tools": wire_tools(&f.kernel),
        "max_tokens": 64
    });
    assert_conforms(contracts().chat_input, &input);
    let out = f
        .kernel
        .execute_by_role(spi::llm_chat::ROLE, spi::llm_chat::CHAT, input, &json!({}))
        .await
        .unwrap()
        .output;
    assert_conforms(contracts().chat_output, &out);
    assert_eq!(out["stop_reason"], "tool_use");
    // Text and parallel tool calls mix in one assistant message.
    let content = out["message"]["content"].as_array().unwrap();
    assert_eq!(content.len(), 3);
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[1]["id"], "call_1");
    assert_eq!(content[2]["id"], "call_2");
    for call in &content[1..] {
        assert_eq!(call["type"], "tool_use");
        // The name came from the descriptor round trip, not the fixture.
        assert_eq!(call["name"], "fixture_echo");
    }
}

#[tokio::test]
async fn the_tool_is_dispatched_by_role() {
    let f = fixture();
    let out = f
        .kernel
        .execute_by_role(
            spi::tool::ROLE,
            spi::tool::CALL,
            json!({"message": "by role"}),
            &json!({}),
        )
        .await
        .unwrap()
        .output;
    assert_conforms(contracts().call_output, &out);
    assert_eq!(out, json!({"content": "echo: by role", "is_error": false}));
}

#[tokio::test]
async fn a_tool_call_round_trips_with_no_agent_loop() {
    // Provider output → tool inputs → tool results → provider, wired by
    // hand: the loop is milestone 5. The provider is dispatched by role;
    // the tools by harvested descriptor, per docs/SPI.md.
    let f = fixture();
    let opening = json!({
        "messages": [user_text("please call the tool")],
        "tools": wire_tools(&f.kernel)
    });
    let first = f
        .kernel
        .execute_by_role(
            spi::llm_chat::ROLE,
            spi::llm_chat::CHAT,
            opening.clone(),
            &json!({}),
        )
        .await
        .unwrap()
        .output;
    assert_conforms(contracts().chat_output, &first);
    assert_eq!(first["stop_reason"], "tool_use");
    let calls: Vec<&Value> = first["message"]["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|b| b["type"] == "tool_use")
        .collect();
    assert_eq!(calls.len(), 2);

    // Answer every call, all in the immediately following user message,
    // results before text — the tool-call protocol.
    let mut results = Vec::new();
    for call in &calls {
        let result = call_by_descriptor(f, call).await;
        assert_eq!(result["is_error"], false);
        results.push(json!({
            "type": "tool_result",
            "tool_use_id": call["id"],
            "content": result["content"],
            "is_error": result["is_error"]
        }));
    }
    results.push(json!({"type": "text", "text": "carry on"}));

    let mut messages = opening["messages"].as_array().unwrap().clone();
    messages.push(first["message"].clone());
    messages.push(json!({"role": "user", "content": results}));
    let follow_up = json!({"messages": messages, "tools": opening["tools"]});
    assert_conforms(contracts().chat_input, &follow_up);
    let second = f
        .kernel
        .execute_by_role(
            spi::llm_chat::ROLE,
            spi::llm_chat::CHAT,
            follow_up,
            &json!({}),
        )
        .await
        .unwrap()
        .output;
    assert_conforms(contracts().chat_output, &second);
    assert_eq!(second["stop_reason"], "end_turn");
    assert_eq!(
        second["message"]["content"][0]["text"],
        "the tool said: echo: hello from the model"
    );
}

// ------------------------------------------------------------ streaming

#[tokio::test]
async fn a_streamed_response_arrives_through_a_stream_handle() {
    let f = fixture();
    let events = stream_events_from(f, format!("{}/chat", stream_stub())).await;

    // Text deltas concatenate in arrival order; the tool call arrives
    // whole; the end event closes the turn before end-of-stream.
    let text: String = events
        .iter()
        .filter(|e| e["type"] == "text")
        .map(|e| e["text"].as_str().unwrap())
        .collect();
    assert_eq!(text, "hello");
    let call = events
        .iter()
        .find(|e| e["type"] == "tool_use")
        .expect("a whole tool_use event");
    assert_eq!(call["name"], "fixture_echo");
    assert_eq!(call["input"], json!({"message": "streamed"}));
    let end = events.last().unwrap();
    assert_eq!(end["type"], "end");
    assert_eq!(end["stop_reason"], "end_turn");
    assert_eq!(end["usage"]["output_tokens"], 2);
}

#[tokio::test]
async fn a_stream_ending_without_end_is_a_failed_turn() {
    // End-of-stream with no `end` (or `error`) event: the consumer must
    // classify the turn as failed, not treat "hello" as a short answer.
    let f = fixture();
    let events = stream_events_from(f, format!("{}/truncated", stream_stub())).await;

    assert!(!events.is_empty(), "the truncated turn did stream bytes");
    let failed = !events
        .iter()
        .any(|e| e["type"] == "end" || e["type"] == "error");
    assert!(
        failed,
        "no end event may appear in a truncated stream: {events:?}"
    );
}
