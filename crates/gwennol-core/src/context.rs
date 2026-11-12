//! What the loop records about *why* an action is running.
//!
//! Gwead threads an opaque [`ExecutionContext`] through every dispatch site
//! and clones it into child invocations, never introspecting it: the
//! embedder owns the schema. Gwennol writes one key, [`CONTEXT_KEY`], beside
//! whatever entries the session's `SessionConfig::context` carries. Its key
//! names the model's tool call an action serves, so that a step reached
//! through two `dispatch_role` hops can still name the call it serves.
//!
//! Threading it this way rather than through a task-local is deliberate:
//! the kernel may run step bodies on tasks the host never sees, and the
//! context follows the invocation tree instead of the task tree.
//!
//! The blob is namespaced under a single [`CONTEXT_KEY`] so later additions
//! do not collide, and plugins cannot read or forge it — Gwead exposes no
//! `$context` template root.

use gwead::kernel::ExecutionContext;
use gwead::serde_json::{Map, Value, json};

use crate::events::ToolCall;

/// Top-level key Gwennol's slice of the execution context lives under.
pub const CONTEXT_KEY: &str = "gwennol";

/// Build the execution context for an action run on behalf of `call`.
///
/// This is [`exec_context_with`] with an empty base; the agent loop passes
/// that function's result to `Kernel::execute(…).with_exec_ctx(…)`; a step
/// body underneath reads the call back with [`tool_call`].
pub fn exec_context(call: &ToolCall) -> ExecutionContext {
    exec_context_with(&Map::new(), call)
}

/// Build the execution context for an action run on behalf of `call`, for a
/// session whose embedder supplied `base`: the context's top-level entries
/// are `base`'s, plus [`CONTEXT_KEY`] naming the call.
///
/// `base` must not itself contain [`CONTEXT_KEY`]: the entry naming the call
/// would replace it. `Session` refuses such a map when it is built.
/// With an empty `base` the result is [`exec_context`]'s.
pub fn exec_context_with(base: &Map<String, Value>, call: &ToolCall) -> ExecutionContext {
    let mut tc = Map::new();
    if let Some(id) = &call.id {
        tc.insert("id".to_string(), Value::String(id.clone()));
    }
    tc.insert("name".to_string(), Value::String(call.name.clone()));
    tc.insert(
        "arguments".to_string(),
        Value::String(call.arguments.clone()),
    );
    let mut entries = base.clone();
    entries.insert(
        CONTEXT_KEY.to_string(),
        json!({"toolCall": Value::Object(tc)}),
    );
    ExecutionContext::new(Value::Object(entries))
}

/// Read back what [`exec_context`] or [`exec_context_with`] wrote.
///
/// `None` for a context Gwennol did not write — an empty one, or an
/// embedder's own blob. A malformed entry reads as `None` rather than
/// failing the reader: the cause is explanatory, and losing it must never
/// turn into losing what it explains.
pub fn tool_call(ctx: &ExecutionContext) -> Option<ToolCall> {
    let tc = ctx.as_value().get(CONTEXT_KEY)?.get("toolCall")?;
    Some(ToolCall {
        id: tc.get("id").and_then(Value::as_str).map(str::to_string),
        name: tc.get("name").and_then(Value::as_str)?.to_string(),
        arguments: tc
            .get("arguments")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call() -> ToolCall {
        ToolCall {
            id: Some("call_01".into()),
            name: "bash".into(),
            arguments: r#"{"command":"ls"}"#.into(),
        }
    }

    #[test]
    fn round_trips() {
        assert_eq!(tool_call(&exec_context(&call())), Some(call()));
    }

    #[test]
    fn round_trips_without_an_id() {
        let c = ToolCall { id: None, ..call() };
        assert_eq!(tool_call(&exec_context(&c)), Some(c));
    }

    #[test]
    fn a_base_context_is_kept_beside_the_tool_call() {
        let base = json!({"principal": "p1"}).as_object().unwrap().clone();
        let ctx = exec_context_with(&base, &call());
        assert_eq!(ctx.as_value()["principal"], "p1");
        assert_eq!(tool_call(&ctx), Some(call()));

        assert_eq!(
            exec_context_with(&Map::new(), &call()).as_value(),
            exec_context(&call()).as_value()
        );
    }

    #[test]
    fn absent_or_foreign_context_reads_as_none() {
        assert_eq!(tool_call(&ExecutionContext::empty()), None);
        let theirs = ExecutionContext::new(json!({"tenantId": "t1"}));
        assert_eq!(tool_call(&theirs), None);
        let malformed = ExecutionContext::new(json!({CONTEXT_KEY: {"toolCall": {"id": "x"}}}));
        assert_eq!(tool_call(&malformed), None);
    }
}
