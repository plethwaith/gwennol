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

/// A canned `LLM_CHAT` implementation. First call: asks for the first
/// offered tool with fixed arguments. Follow-up call (three or more
/// messages, the third a tool result): closes the turn with text quoting
/// the result. Purely declarative — intrinsics only, no permissions.
fn fixture_provider() -> Value {
    json!({
        "name": "fixture_llm", "version": "0.0.0",
        "description": "Canned LLM_CHAT fixture: one tool call, then a closing turn.",
        "roles": [gwennol_core::spi::llm_chat::ROLE],
        "actions": {
            "chat": {
                "steps": [
                    {"id": "branch", "type": "ifs", "params": {"ifs": [
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
