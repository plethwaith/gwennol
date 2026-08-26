//! What the host knows about *why* an action is running.
//!
//! Gwead threads an opaque [`ExecutionContext`] through every dispatch site
//! and clones it into child invocations, never introspecting it: the
//! embedder owns the schema. Gwennol uses it for one thing — naming the
//! model's tool call an action serves — so that a `host_*` step reached
//! through two `dispatch_role` hops can still tell the operator that the
//! model asked `bash` to run something.
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

use crate::operator::ToolCall;

/// Top-level key Gwennol's slice of the execution context lives under.
pub const CONTEXT_KEY: &str = "gwennol";

/// Build the execution context for an action run on behalf of `call`.
///
/// The agent loop passes the result to
/// `Kernel::execute(…).with_exec_ctx(…)`; every approval raised underneath
/// then carries the call as [`ApprovalRequest::cause`](crate::ApprovalRequest).
pub fn exec_context(call: &ToolCall) -> ExecutionContext {
    let mut tc = Map::new();
    if let Some(id) = &call.id {
        tc.insert("id".to_string(), Value::String(id.clone()));
    }
    tc.insert("name".to_string(), Value::String(call.name.clone()));
    tc.insert(
        "arguments".to_string(),
        Value::String(call.arguments.clone()),
    );
    ExecutionContext::new(json!({CONTEXT_KEY: {"toolCall": Value::Object(tc)}}))
}

/// Read back what [`exec_context`] wrote.
///
/// `None` for a context Gwennol did not write — an empty one, or an
/// embedder's own blob. A malformed entry reads as `None` rather than
/// failing a step: the cause is explanatory, and losing it must never turn
/// into losing the approval.
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
    fn absent_or_foreign_context_reads_as_none() {
        assert_eq!(tool_call(&ExecutionContext::empty()), None);
        let theirs = ExecutionContext::new(json!({"tenantId": "t1"}));
        assert_eq!(tool_call(&theirs), None);
        let malformed = ExecutionContext::new(json!({CONTEXT_KEY: {"toolCall": {"id": "x"}}}));
        assert_eq!(tool_call(&malformed), None);
    }
}
