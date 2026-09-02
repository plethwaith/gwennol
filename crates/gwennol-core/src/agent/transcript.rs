//! The conversation the loop replays, and the shape rules it enforces on
//! what goes into it.
//!
//! Messages are stored exactly as the contract shapes them
//! (`docs/SPI.md`), as JSON, because the provider gets them back
//! verbatim — an assistant message in particular is replayed as the
//! provider produced it, `opaque` blocks in place. The loop never
//! reshapes what it stores; it only decides *when* something is stored.

use gwead::serde_json::{Value, json};

/// The messages of one session, oldest first.
///
/// A turn's pieces land here only when they are whole: the user's text
/// at the start of the turn, an assistant message once the provider has
/// finished producing it, the tool results once every call in that
/// message has been answered. Nothing partial is ever stored, so a turn
/// that fails or is cancelled leaves the transcript ending in a user
/// message — the last thing that completed — and the next turn's text
/// joins that message rather than following it, keeping the roles
/// alternating and every tool call answered in the message right after
/// it.
#[derive(Debug, Default)]
pub(crate) struct Transcript {
    messages: Vec<Value>,
}

impl Transcript {
    pub(crate) fn messages(&self) -> &[Value] {
        &self.messages
    }

    /// Add the user's text. When the transcript already ends in a user
    /// message — the previous turn did not complete — the text becomes
    /// another block of that message, after any tool results it holds:
    /// the protocol wants results before text, and a vendor rejects two
    /// user messages in a row.
    pub(crate) fn push_user_text(&mut self, text: &str) {
        let block = json!({"type": "text", "text": text});
        if let Some(last) = self.messages.last_mut()
            && last["role"] == "user"
            && let Some(content) = last["content"].as_array_mut()
        {
            content.push(block);
            return;
        }
        self.messages
            .push(json!({"role": "user", "content": [block]}));
    }

    /// Add an assistant message, exactly as the provider produced it.
    pub(crate) fn push_assistant(&mut self, message: Value) {
        self.messages.push(message);
    }

    /// Add the user message answering the previous assistant message's
    /// tool calls: one `tool_result` block per call, in the calls'
    /// order, and nothing else.
    pub(crate) fn push_tool_results(&mut self, results: Vec<Value>) {
        self.messages
            .push(json!({"role": "user", "content": results}));
    }
}

/// An assistant message rebuilt from a stream, per the contract's rule:
/// the events in order, adjacent `text` events coalesced into one text
/// block, `tool_use` and `opaque` blocks whole and in place.
#[derive(Debug, Default)]
pub(crate) struct MessageBuilder {
    content: Vec<Value>,
}

impl MessageBuilder {
    /// Append streamed text, extending the block before it when that
    /// block is text.
    pub(crate) fn text(&mut self, text: &str) {
        if let Some(last) = self.content.last_mut()
            && last["type"] == "text"
            && let Some(Value::String(existing)) = last.get_mut("text")
        {
            existing.push_str(text);
            return;
        }
        self.content.push(json!({"type": "text", "text": text}));
    }

    /// Append a whole block — a `tool_use` or `opaque` event is already
    /// shaped as the block it becomes.
    pub(crate) fn block(&mut self, block: Value) {
        self.content.push(block);
    }

    pub(crate) fn finish(self) -> Value {
        json!({"role": "assistant", "content": self.content})
    }
}

/// A `tool_use` block: the model asking for a tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolUse {
    pub id: String,
    pub name: String,
    pub input: Value,
}

/// Check one assistant content block — or the stream event of the same
/// shape — against the contract, and return the tool call it is when
/// it is one.
///
/// Fail-closed, as the contract asks: an unknown block type, a missing
/// or mistyped field, or a field the closed schema does not allow all
/// refuse the block. A block is replayed verbatim later, so admitting
/// an extra field here would mean sending the provider something the
/// contract says cannot exist.
pub(crate) fn check_block(block: &Value) -> Result<Option<ToolUse>, String> {
    let Some(fields) = block.as_object() else {
        return Err(format!("content block is not an object: {block}"));
    };
    let kind = match fields.get("type") {
        Some(Value::String(kind)) => kind.as_str(),
        _ => return Err(format!("content block has no string `type`: {block}")),
    };
    let expect = |allowed: &[&str]| -> Result<(), String> {
        for key in fields.keys() {
            if !allowed.contains(&key.as_str()) {
                return Err(format!(
                    "{kind} block carries a field the contract does not allow: {key:?}"
                ));
            }
        }
        Ok(())
    };
    match kind {
        "text" => {
            expect(&["type", "text"])?;
            match fields.get("text") {
                Some(Value::String(_)) => Ok(None),
                _ => Err("text block has no string `text`".into()),
            }
        }
        "tool_use" => {
            expect(&["type", "id", "name", "input"])?;
            let id = match fields.get("id") {
                Some(Value::String(id)) => id.clone(),
                _ => return Err("tool_use block has no string `id`".into()),
            };
            let name = match fields.get("name") {
                Some(Value::String(name)) => name.clone(),
                _ => return Err("tool_use block has no string `name`".into()),
            };
            let input = match fields.get("input") {
                Some(input @ Value::Object(_)) => input.clone(),
                _ => return Err(format!("tool_use block {id:?} has no object `input`")),
            };
            Ok(Some(ToolUse { id, name, input }))
        }
        "opaque" => {
            expect(&["type", "provider", "data"])?;
            if !matches!(fields.get("provider"), Some(Value::String(_))) {
                return Err("opaque block has no string `provider`".into());
            }
            if !fields.contains_key("data") {
                return Err("opaque block has no `data`".into());
            }
            Ok(None)
        }
        other => Err(format!("unknown assistant content block type {other:?}")),
    }
}

/// Check a whole assistant message and return its tool calls in order.
pub(crate) fn check_assistant_message(message: &Value) -> Result<Vec<ToolUse>, String> {
    let Some(fields) = message.as_object() else {
        return Err(format!("assistant message is not an object: {message}"));
    };
    for key in fields.keys() {
        if key != "role" && key != "content" {
            return Err(format!(
                "assistant message carries a field the contract does not allow: {key:?}"
            ));
        }
    }
    if fields.get("role").and_then(Value::as_str) != Some("assistant") {
        return Err("message role is not `assistant`".into());
    }
    let Some(content) = fields.get("content").and_then(Value::as_array) else {
        return Err("assistant message has no `content` array".into());
    };
    let mut calls = Vec::new();
    for block in content {
        if let Some(call) = check_block(block)? {
            calls.push(call);
        }
    }
    Ok(calls)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_turn_joins_a_trailing_user_message_after_its_results() {
        let mut t = Transcript::default();
        t.push_user_text("first");
        assert_eq!(t.messages().len(), 1);
        // The previous turn failed: the transcript still ends in the
        // user's text, and the retry joins it as a second block.
        t.push_user_text("again");
        assert_eq!(t.messages().len(), 1);
        assert_eq!(
            t.messages()[0]["content"],
            json!([{"type": "text", "text": "first"}, {"type": "text", "text": "again"}])
        );

        t.push_assistant(json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": "c1", "name": "t", "input": {}}
        ]}));
        t.push_tool_results(vec![
            json!({"type": "tool_result", "tool_use_id": "c1", "content": "r", "is_error": false}),
        ]);
        // Failed after the results landed: the next text goes after
        // them, in the same message — results before text.
        t.push_user_text("carry on");
        assert_eq!(t.messages().len(), 3);
        let last = &t.messages()[2]["content"];
        assert_eq!(last[0]["type"], "tool_result");
        assert_eq!(last[1], json!({"type": "text", "text": "carry on"}));
    }

    #[test]
    fn the_builder_coalesces_adjacent_text_and_keeps_other_blocks_in_place() {
        let mut b = MessageBuilder::default();
        b.block(json!({"type": "opaque", "provider": "p", "data": 1}));
        b.text("Let ");
        b.text("me ");
        b.text("read.");
        b.block(json!({"type": "tool_use", "id": "c", "name": "read", "input": {}}));
        b.text("after");
        b.text("wards");
        assert_eq!(
            b.finish(),
            json!({"role": "assistant", "content": [
                {"type": "opaque", "provider": "p", "data": 1},
                {"type": "text", "text": "Let me read."},
                {"type": "tool_use", "id": "c", "name": "read", "input": {}},
                {"type": "text", "text": "afterwards"}
            ]})
        );
        assert_eq!(
            MessageBuilder::default().finish(),
            json!({"role": "assistant", "content": []}),
            "a turn may complete no block at all"
        );
    }

    #[test]
    fn blocks_are_checked_fail_closed() {
        let ok = json!({"role": "assistant", "content": [
            {"type": "opaque", "provider": "p", "data": null},
            {"type": "text", "text": "t"},
            {"type": "tool_use", "id": "c1", "name": "n", "input": {"a": 1}}
        ]});
        let calls = check_assistant_message(&ok).unwrap();
        assert_eq!(
            calls,
            vec![ToolUse {
                id: "c1".into(),
                name: "n".into(),
                input: json!({"a": 1})
            }]
        );

        for (bad, why) in [
            (json!({"type": "thinking", "thinking": "x"}), "unknown"),
            (json!({"type": "text"}), "no string `text`"),
            (
                json!({"type": "text", "text": "t", "extra": 1}),
                "does not allow",
            ),
            (
                json!({"type": "tool_use", "id": "c", "name": "n", "input": []}),
                "object `input`",
            ),
            (
                json!({"type": "tool_use", "id": 1, "name": "n", "input": {}}),
                "string `id`",
            ),
            (json!({"type": "opaque", "provider": "p"}), "no `data`"),
            (json!({"type": "opaque", "data": 1}), "string `provider`"),
            (json!("text"), "not an object"),
        ] {
            let err = check_block(&bad).unwrap_err();
            assert!(err.contains(why), "{bad}: {err}");
        }
        for bad in [
            json!({"role": "user", "content": []}),
            json!({"role": "assistant", "content": [], "id": "m1"}),
            json!({"role": "assistant"}),
            json!([]),
        ] {
            check_assistant_message(&bad).unwrap_err();
        }
    }
}
