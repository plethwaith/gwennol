//! End-to-end: the bundled SPI contracts are registered at boot, plugins
//! are checked against them, and fixture implementations of each role are
//! dispatched *by role* through a real Gwead kernel. The tool-call wire
//! shapes exercised here are the ones `docs/SPI.md` documents.
//!
//! The host is a process singleton, so this binary boots one kernel with
//! every fixture plugin registered up front and shares it across tests.

use std::sync::{Arc, OnceLock};

use gwead::kernel::Kernel;
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
        let workspace = tempfile::tempdir().unwrap().keep();
        let mut kernel = gwennol_core::boot(Arc::new(Permissive), workspace).unwrap();
        for plugin in [fixture_provider(), fixture_tool()] {
            kernel
                .register_plugin_from_json(&plugin.to_string())
                .unwrap();
        }
        Fixture {
            kernel: kernel.into_arc(),
        }
    })
}

/// A canned `LLM_CHAT` implementation. Streamed call: relays NDJSON
/// events fetched from the stub server (`$config.stream_url`) as its
/// stream handle — the same composition a real provider uses, minus the
/// protocol translation. First buffered call: asks for the first offered
/// tool with fixed arguments. Follow-up call (three or more messages,
/// the third a tool result): closes the turn with text quoting the
/// result.
fn fixture_provider() -> Value {
    json!({
        "name": "fixture_llm", "version": "0.0.0",
        "description": "Canned LLM_CHAT fixture: one tool call, then a closing turn.",
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
                                    {"type": "tool_use", "id": "call_1",
                                     "name": "{{$input.tools[0].name}}",
                                     "input": {"message": "hello from the model"}}
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

/// The `tools` input for `chat`, built the way the agent loop will build
/// it: from the kernel's harvested tool descriptors, never by hand.
fn wire_tools(kernel: &Kernel) -> Value {
    let tools: Vec<Value> = kernel
        .registry()
        .get_tool_descriptors()
        .into_iter()
        .map(|d| {
            json!({
                "name": d.tool_name,
                "description": d.description,
                "input_schema": d.parameters
            })
        })
        .collect();
    Value::Array(tools)
}

/// One canned model turn as `streamEventShape` NDJSON: two text deltas
/// and the final `end` event.
const STREAM_BODY: &str = concat!(
    r#"{"type":"text","text":"hel"}"#,
    "\n",
    r#"{"type":"text","text":"lo"}"#,
    "\n",
    r#"{"type":"end","stop_reason":"end_turn","usage":{"input_tokens":5,"output_tokens":2}}"#,
    "\n",
);

/// Minimal HTTP/1.1 server answering every request with [`STREAM_BODY`],
/// written in two flushed halves so the client genuinely streams.
fn spawn_stream_stub() -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut s = stream.unwrap();
            std::thread::spawn(move || {
                // Read until the blank line ending the request head; the
                // fixture's GET carries no body.
                let mut buf = Vec::new();
                let mut tmp = [0u8; 1024];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = s.read(&mut tmp).unwrap();
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                write!(
                    s,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/x-ndjson\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    STREAM_BODY.len()
                )
                .unwrap();
                let (a, b) = STREAM_BODY.split_at(STREAM_BODY.len() / 2);
                s.write_all(a.as_bytes()).unwrap();
                s.flush().unwrap();
                s.write_all(b.as_bytes()).unwrap();
            });
        }
    });
    format!("http://127.0.0.1:{port}/chat")
}

// ------------------------------------------------------- registration

#[test]
fn bundled_contracts_are_spi_definitions() {
    for (_, definition) in spi::SPI_DEFINITIONS {
        assert!(matches!(
            Kernel::manifest_kind(definition).unwrap(),
            gwead::kernel::ManifestKind::SpiDef
        ));
    }
}

#[tokio::test]
async fn boot_registers_both_roles() {
    let f = fixture();
    let roles = f.kernel.spi_registry().roles();
    assert!(roles.contains(&spi::llm_chat::ROLE), "{roles:?}");
    assert!(roles.contains(&spi::tool::ROLE), "{roles:?}");
}

#[tokio::test]
async fn a_provider_missing_the_chat_action_is_rejected() {
    // The reason boot registers the contracts first: with the definition
    // present, an incomplete claim is an error rather than a warning.
    // (On a bare Gwead kernel — registration is `&mut`, and the shared
    // fixture kernel is already behind its `Arc`.)
    let mut kernel = Kernel::boot(gwead::kernel::KernelConfig::default()).unwrap();
    for (role, definition) in spi::SPI_DEFINITIONS {
        kernel.register_spi_from_json(role, definition).unwrap();
    }
    let claim = json!({
        "name": "hollow_provider", "version": "0.0.0",
        "description": "claims LLM_CHAT but provides no chat action",
        "roles": [spi::llm_chat::ROLE],
        "actions": {"other": {"steps": [
            {"id": "s", "type": "let", "params": {"value": 1}}
        ]}}
    });
    let err = kernel
        .load_manifest(&claim.to_string())
        .register()
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("chat"), "{msg}");
    assert!(msg.contains(spi::llm_chat::ROLE), "{msg}");
}

// ---------------------------------------------------- dispatch by role

fn user_text(text: &str) -> Value {
    json!({"role": "user", "content": [{"type": "text", "text": text}]})
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
    let out = f
        .kernel
        .execute_by_role(
            spi::llm_chat::ROLE,
            spi::llm_chat::CHAT,
            json!({
                "system": "You are a test fixture.",
                "messages": [user_text("please call the tool")],
                "tools": wire_tools(&f.kernel),
                "max_tokens": 64
            }),
            &json!({}),
        )
        .await
        .unwrap()
        .output;
    assert_eq!(out["stop_reason"], "tool_use");
    let block = &out["message"]["content"][0];
    assert_eq!(block["type"], "tool_use");
    // The name came from the descriptor round trip, not from the fixture.
    assert_eq!(block["name"], "fixture_echo");
    assert_eq!(block["input"], json!({"message": "hello from the model"}));
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
    assert_eq!(out, json!({"content": "echo: by role", "is_error": false}));
}

#[tokio::test]
async fn a_tool_call_round_trips_with_no_agent_loop() {
    // Provider output → tool input → tool result → provider, each hop
    // dispatched by role, wired by hand: the loop is milestone 5.
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
    let call = &first["message"]["content"][0];
    assert_eq!(first["stop_reason"], "tool_use");

    let result = f
        .kernel
        .execute_by_role(
            spi::tool::ROLE,
            spi::tool::CALL,
            call["input"].clone(),
            &json!({}),
        )
        .await
        .unwrap()
        .output;
    assert_eq!(result["is_error"], false);

    let mut messages = opening["messages"].as_array().unwrap().clone();
    messages.push(first["message"].clone());
    messages.push(json!({"role": "user", "content": [{
        "type": "tool_result",
        "tool_use_id": call["id"],
        "content": result["content"],
        "is_error": result["is_error"]
    }]}));
    let second = f
        .kernel
        .execute_by_role(
            spi::llm_chat::ROLE,
            spi::llm_chat::CHAT,
            json!({"messages": messages, "tools": opening["tools"]}),
            &json!({}),
        )
        .await
        .unwrap()
        .output;
    assert_eq!(second["stop_reason"], "end_turn");
    assert_eq!(
        second["message"]["content"][0]["text"],
        "the tool said: echo: hello from the model"
    );
}

#[tokio::test]
async fn a_streamed_response_arrives_through_a_stream_handle() {
    use gwead::kernel::streams::{STREAM_EOF, StreamRegistry, read_async_shared};

    let f = fixture();
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

    let streams = Arc::new(std::sync::Mutex::new(StreamRegistry::new()));
    let out = f
        .kernel
        .execute(
            &provider,
            spi::llm_chat::CHAT,
            json!({"messages": [user_text("stream please")], "stream": true}),
        )
        .with_config(&json!({"stream_url": spawn_stream_stub()}))
        .with_streams(streams.clone())
        .run()
        .await
        .unwrap()
        .output;

    let handle = out["stream"].as_u64().expect("integer stream handle");
    let id = std::num::NonZeroU32::new(u32::try_from(handle).unwrap()).unwrap();
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

    // The bytes are contract NDJSON: text deltas to concatenate, then
    // the end event, then end-of-stream.
    let events: Vec<Value> = String::from_utf8(collected)
        .unwrap()
        .lines()
        .map(|l| gwead::serde_json::from_str(l).unwrap())
        .collect();
    let text: String = events
        .iter()
        .filter(|e| e["type"] == "text")
        .map(|e| e["text"].as_str().unwrap())
        .collect();
    assert_eq!(text, "hello");
    let end = events.last().unwrap();
    assert_eq!(end["type"], "end");
    assert_eq!(end["stop_reason"], "end_turn");
    assert_eq!(end["usage"]["output_tokens"], 2);
}
