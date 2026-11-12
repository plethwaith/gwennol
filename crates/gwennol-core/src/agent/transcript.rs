//! The conversation the loop replays, and the shape rules it enforces on
//! what goes into it.
//!
//! Messages are stored exactly as the contract shapes them
//! (`docs/SPI.md`), as JSON, because the provider gets them back
//! verbatim — an assistant message in particular is replayed as the
//! provider produced it, `opaque` blocks in place. The loop never
//! reshapes what it stores; it only decides *when* something is stored.

use gwead::serde_json::{Value, json};

use crate::content::ImageFormat;

/// The messages of one session, oldest first.
///
/// A turn's pieces land here only when they are whole: the user's
/// content at the start of the turn, an assistant message once the provider has
/// finished producing it, the tool results once every call in that
/// message has been answered. Nothing partial is ever stored, so a turn
/// that fails or is cancelled once its content is stored leaves the
/// transcript ending in a user message — the last thing that completed —
/// and the next turn's content joins that message rather than following
/// it, keeping the roles alternating and every tool call answered in the
/// message right after it.
#[derive(Debug, Default)]
pub(crate) struct Transcript {
    messages: Vec<Value>,
}

impl Transcript {
    pub(crate) fn messages(&self) -> &[Value] {
        &self.messages
    }

    /// A transcript holding `messages`, if they pass the checks here and
    /// those of [`check_user_message`] and [`check_assistant_message`].
    /// Whatever [`Self::messages`] returned passes. On refusal, the
    /// offending message's position (the list's length for a trailing
    /// unanswered call) and the rule it breaks.
    ///
    /// What this does not check is listed on `Session::restore`.
    pub(crate) fn from_messages(messages: Vec<Value>) -> Result<Self, (usize, String)> {
        // The calls the previous assistant message made, awaiting answers.
        let mut owed: Vec<ToolUse> = Vec::new();
        let mut previous: Option<&str> = None;
        for (index, message) in messages.iter().enumerate() {
            let fail = |error: String| (index, error);
            let role = match message.get("role").and_then(Value::as_str) {
                Some(role @ ("user" | "assistant")) => role,
                _ => return Err(fail("message role is not `user` or `assistant`".into())),
            };
            if previous.is_none() && role != "user" {
                return Err(fail("the first message is not a user message".into()));
            }
            if previous == Some(role) {
                return Err(fail(format!("two {role} messages in a row")));
            }
            match role {
                "assistant" => {
                    let content_is_empty = message["content"].as_array().is_some_and(Vec::is_empty);
                    if content_is_empty {
                        return Err(fail("message has an empty `content` array".into()));
                    }
                    owed = check_assistant_message(message).map_err(fail)?;
                }
                _ => {
                    let answered = check_user_message(message).map_err(fail)?;
                    if answered.len() != owed.len() {
                        return Err(fail(format!(
                            "the message answers {} tool calls but the assistant message before it made {}",
                            answered.len(),
                            owed.len()
                        )));
                    }
                    for (answer, call) in answered.iter().zip(&owed) {
                        if *answer != call.id {
                            return Err(fail(format!(
                                "tool_result answers {answer:?} where the call made was {:?}",
                                call.id
                            )));
                        }
                    }
                    owed.clear();
                }
            }
            previous = Some(role);
        }
        if !owed.is_empty() {
            return Err((
                messages.len(),
                format!(
                    "the last assistant message's {} tool calls are unanswered",
                    owed.len()
                ),
            ));
        }
        Ok(Self { messages })
    }

    /// Add the user's content blocks, in the order given.
    /// When the transcript already ends in a user message, the blocks become
    /// the end of that message, after any tool results it holds: the
    /// protocol wants results before text and images, and a vendor rejects
    /// two user messages in a row.
    pub(crate) fn push_user_content(&mut self, blocks: Vec<Value>) {
        if let Some(last) = self.messages.last_mut()
            && last["role"] == "user"
            && let Some(content) = last["content"].as_array_mut()
        {
            content.extend(blocks);
            return;
        }
        self.messages
            .push(json!({"role": "user", "content": blocks}));
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

/// Check a whole user message and return the `tool_use_id`s of its leading
/// `tool_result` blocks, in order.
///
/// Fail-closed like [`check_block`]: only the block types the loop
/// stores (`text`, `image`, `tool_result`), with no field it does not
/// store, and results before any text or image.
pub(crate) fn check_user_message(message: &Value) -> Result<Vec<&str>, String> {
    let Some(fields) = message.as_object() else {
        return Err(format!("user message is not an object: {message}"));
    };
    for key in fields.keys() {
        if key != "role" && key != "content" {
            return Err(format!(
                "user message carries a field the contract does not allow: {key:?}"
            ));
        }
    }
    if fields.get("role").and_then(Value::as_str) != Some("user") {
        return Err("message role is not `user`".into());
    }
    let Some(content) = fields.get("content").and_then(Value::as_array) else {
        return Err("user message has no `content` array".into());
    };
    if content.is_empty() {
        return Err("message has an empty `content` array".into());
    }
    let mut ids = Vec::new();
    // The kind of the first `text` or `image` block seen, if any.
    let mut seen_content: Option<&str> = None;
    for block in content {
        let Some(block_fields) = block.as_object() else {
            return Err(format!("content block is not an object: {block}"));
        };
        let allowed: &[&str] = match block_fields.get("type").and_then(Value::as_str) {
            Some("text") => {
                if !matches!(block_fields.get("text"), Some(Value::String(_))) {
                    return Err("text block has no string `text`".into());
                }
                seen_content.get_or_insert("a text");
                &["type", "text"]
            }
            Some("image") => {
                check_image_source(block_fields.get("source"))?;
                seen_content.get_or_insert("an image");
                &["type", "source"]
            }
            Some("tool_result") => {
                if let Some(kind) = seen_content {
                    return Err(format!("tool_result block comes after {kind} block"));
                }
                let Some(Value::String(id)) = block_fields.get("tool_use_id") else {
                    return Err("tool_result block has no string `tool_use_id`".into());
                };
                if !matches!(block_fields.get("content"), Some(Value::String(_))) {
                    return Err(format!("tool_result block {id:?} has no string `content`"));
                }
                if block_fields
                    .get("is_error")
                    .is_some_and(|flag| !flag.is_boolean())
                {
                    return Err(format!(
                        "tool_result block {id:?} has a non-boolean `is_error`"
                    ));
                }
                ids.push(id.as_str());
                &["type", "tool_use_id", "content", "is_error"]
            }
            Some(other) => return Err(format!("unknown user content block type {other:?}")),
            None => return Err(format!("content block has no string `type`: {block}")),
        };
        for key in block_fields.keys() {
            if !allowed.contains(&key.as_str()) {
                return Err(format!(
                    "user content block carries a field the contract does not allow: {key:?}"
                ));
            }
        }
    }
    Ok(ids)
}

/// Check an `image` block's `source`: closed, and either inline base64 or
/// a reference, each of a media type the contract names. The field set is
/// chosen by `source.type`. Not checked: that `data` is base64, that it
/// decodes to the format `media_type` names, or that a reference names
/// anything.
fn check_image_source(source: Option<&Value>) -> Result<(), String> {
    let Some(source) = source.and_then(Value::as_object) else {
        return Err("image block has no object `source`".into());
    };
    let (allowed, payload): (&[&str], &str) = match source.get("type").and_then(Value::as_str) {
        Some("base64") => (&["type", "media_type", "data"], "data"),
        Some("reference") => (&["type", "media_type", "reference"], "reference"),
        _ => return Err("image source type is not `base64` or `reference`".into()),
    };
    for key in source.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(format!(
                "image source carries a field the contract does not allow: {key:?}"
            ));
        }
    }
    match source.get("media_type").and_then(Value::as_str) {
        Some(media_type)
            if ImageFormat::ALL
                .iter()
                .any(|f| f.media_type() == media_type) => {}
        other => {
            return Err(format!(
                "image block has a media type the contract does not name: {}",
                other.map_or_else(|| "(none)".to_string(), str::to_string)
            ));
        }
    }
    if !matches!(source.get(payload), Some(Value::String(text)) if !text.is_empty()) {
        return Err(format!("image block has no non-empty string `{payload}`"));
    }
    Ok(())
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
        t.push_user_content(vec![txt("first")]);
        assert_eq!(t.messages().len(), 1);
        // The previous turn failed: the transcript still ends in the
        // user's text, and the retry joins it as a second block.
        t.push_user_content(vec![txt("again")]);
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
        t.push_user_content(vec![txt("carry on")]);
        assert_eq!(t.messages().len(), 3);
        let last = &t.messages()[2]["content"];
        assert_eq!(last[0]["type"], "tool_result");
        assert_eq!(last[1], json!({"type": "text", "text": "carry on"}));
    }

    #[test]
    fn content_joins_a_trailing_user_message_in_the_order_given() {
        let mut t = Transcript::default();
        t.push_assistant(json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": "c1", "name": "t", "input": {}}
        ]}));
        t.push_tool_results(vec![
            json!({"type": "tool_result", "tool_use_id": "c1", "content": "r", "is_error": false}),
        ]);
        let image = json!({"type": "image", "source": {
            "type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo="
        }});
        t.push_user_content(vec![
            json!({"type": "text", "text": "one"}),
            image.clone(),
            json!({"type": "text", "text": "two"}),
        ]);
        let content = t.messages().last().unwrap()["content"].as_array().unwrap();
        assert_eq!(content.len(), 4);
        assert_eq!(content[0]["type"], "tool_result");
        assert_eq!(content[1], json!({"type": "text", "text": "one"}));
        assert_eq!(content[2], image);
        assert_eq!(content[3], json!({"type": "text", "text": "two"}));
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

    fn tr(id: &str, content: &str) -> Value {
        json!({"type": "tool_result", "tool_use_id": id, "content": content, "is_error": false})
    }
    fn user(blocks: Vec<Value>) -> Value {
        json!({"role": "user", "content": blocks})
    }
    fn txt(t: &str) -> Value {
        json!({"type": "text", "text": t})
    }
    fn img_ref(media_type: &str, reference: &str) -> Value {
        json!({"type": "image", "source": {"type": "reference", "media_type": media_type, "reference": reference}})
    }

    fn img(media_type: &str, data: &str) -> Value {
        json!({"type": "image", "source": {"type": "base64", "media_type": media_type, "data": data}})
    }
    fn asst(blocks: Vec<Value>) -> Value {
        json!({"role": "assistant", "content": blocks})
    }
    fn tool_use(id: &str, name: &str) -> Value {
        json!({"type": "tool_use", "id": id, "name": name, "input": {}})
    }

    #[test]
    fn restore_accepts_what_the_loop_stores_and_refuses_what_breaks_a_rule() {
        let exchange = || {
            vec![
                user(vec![txt("go")]),
                asst(vec![txt("ok"), tool_use("c1", "a"), tool_use("c2", "b")]),
                user(vec![tr("c1", "r1"), tr("c2", "r2")]),
            ]
        };
        let with = |tail: Vec<Value>| {
            let mut m = exchange();
            m.extend(tail);
            m
        };
        /// A case's name, its messages, and where and why it is refused.
        type Case<'a> = (&'a str, Vec<Value>, Option<(usize, &'a str)>);
        let table: Vec<Case> = vec![
            ("empty", vec![], None),
            ("one user text", vec![user(vec![txt("hi")])], None),
            (
                "user then assistant text",
                vec![user(vec![txt("hi")]), asst(vec![txt("yo")])],
                None,
            ),
            (
                "a full tool exchange",
                with(vec![asst(vec![txt("done")])]),
                None,
            ),
            (
                "text joined after results",
                {
                    let mut m = exchange();
                    m[2] = user(vec![tr("c1", "r1"), tr("c2", "r2"), txt("again")]);
                    m
                },
                None,
            ),
            (
                "text then an image",
                vec![user(vec![txt("look"), img("image/png", "iVBORw0KGgo=")])],
                None,
            ),
            (
                "an image alone",
                vec![user(vec![img("image/webp", "AAAA")])],
                None,
            ),
            (
                "an image reference alone",
                vec![user(vec![img_ref("image/png", "shots/a.png")])],
                None,
            ),
            (
                "text, an image reference and an inline image",
                vec![user(vec![
                    txt("look"),
                    img_ref("image/jpeg", "k"),
                    img("image/png", "AA"),
                ])],
                None,
            ),
            (
                "an image and text joined after results",
                {
                    let mut m = exchange();
                    m[2] = user(vec![
                        tr("c1", "r1"),
                        tr("c2", "r2"),
                        img("image/gif", "R0lGODlh"),
                        txt("again"),
                    ]);
                    m
                },
                None,
            ),
            (
                "an answered call to a tool nobody offers",
                vec![
                    user(vec![txt("go")]),
                    asst(vec![tool_use("c1", "no-such-tool")]),
                    user(vec![tr("c1", "x")]),
                ],
                None,
            ),
            (
                "ids repeat across exchanges",
                vec![
                    user(vec![txt("a")]),
                    asst(vec![tool_use("c1", "a")]),
                    user(vec![tr("c1", "x")]),
                    asst(vec![tool_use("c1", "a")]),
                    user(vec![tr("c1", "y")]),
                ],
                None,
            ),
            (
                "results without is_error",
                vec![
                    user(vec![txt("go")]),
                    asst(vec![tool_use("c1", "a")]),
                    user(vec![
                        json!({"type": "tool_result", "tool_use_id": "c1", "content": "x"}),
                    ]),
                ],
                None,
            ),
            (
                "extra message field",
                vec![json!({"role": "user", "content": [txt("hi")], "id": 1})],
                Some((0, "does not allow")),
            ),
            (
                "unknown role",
                vec![json!({"role": "system", "content": [txt("hi")]})],
                Some((0, "`user` or `assistant`")),
            ),
            (
                "empty content",
                vec![user(vec![])],
                Some((0, "empty `content`")),
            ),
            (
                "empty assistant content",
                vec![user(vec![txt("a")]), asst(vec![])],
                Some((1, "empty `content`")),
            ),
            (
                "first message is the assistant's",
                vec![asst(vec![txt("hi")])],
                Some((0, "first message")),
            ),
            (
                "two user messages in a row",
                vec![user(vec![txt("a")]), user(vec![txt("b")])],
                Some((1, "in a row")),
            ),
            (
                "an off-contract assistant block",
                vec![
                    user(vec![txt("a")]),
                    asst(vec![json!({"type": "thinking", "thinking": "x"})]),
                ],
                Some((1, "unknown assistant content block type")),
            ),
            (
                "a user block of unknown type",
                vec![user(vec![json!({"type": "audio"})])],
                Some((0, "unknown user content block type")),
            ),
            (
                "a text block with non-string text",
                vec![user(vec![json!({"type": "text", "text": 5})])],
                Some((0, "no string `text`")),
            ),
            (
                "a user block with no type",
                vec![user(vec![json!({"text": "a"})])],
                Some((0, "no string `type`")),
            ),
            (
                "a user message with no content array",
                vec![json!({"role": "user"})],
                Some((0, "no `content` array")),
            ),
            (
                "a tool_result with a non-string tool_use_id",
                with(vec![])
                    .into_iter()
                    .take(2)
                    .chain([user(vec![
                        json!({"type": "tool_result", "tool_use_id": 7, "content": "x"}),
                        tr("c2", "r2"),
                    ])])
                    .collect(),
                Some((2, "no string `tool_use_id`")),
            ),
            (
                "a tool_result with a non-boolean is_error",
                with(vec![])
                    .into_iter()
                    .take(2)
                    .chain([user(vec![
                        json!({"type": "tool_result", "tool_use_id": "c1", "content": "x", "is_error": "no"}),
                        tr("c2", "r2"),
                    ])])
                    .collect(),
                Some((2, "non-boolean `is_error`")),
            ),
            (
                "a user block with an extra field",
                vec![user(vec![json!({"type": "text", "text": "a", "x": 1})])],
                Some((0, "does not allow")),
            ),
            (
                "a tool_result with non-string content",
                with(vec![])
                    .into_iter()
                    .take(2)
                    .chain([user(vec![
                        json!({"type": "tool_result", "tool_use_id": "c1", "content": 5}),
                        tr("c2", "r2"),
                    ])])
                    .collect(),
                Some((2, "no string `content`")),
            ),
            (
                "text before a tool_result",
                {
                    let mut m = exchange();
                    m[2] = user(vec![txt("early"), tr("c1", "r1"), tr("c2", "r2")]);
                    m
                },
                Some((2, "after a text block")),
            ),
            (
                "an image block with no source",
                vec![user(vec![json!({"type": "image"})])],
                Some((0, "no object `source`")),
            ),
            (
                "an image block whose source is not an object",
                vec![user(vec![json!({"type": "image", "source": "x"})])],
                Some((0, "no object `source`")),
            ),
            (
                "an image block with an extra field",
                vec![user(vec![{ let mut b = img("image/png", "AA"); b["extra"] = json!(1); b }])],
                Some((0, "user content block carries a field")),
            ),
            (
                "an image source with an extra field",
                vec![user(vec![{ let mut b = img("image/png", "AA"); b["source"]["extra"] = json!(1); b }])],
                Some((0, "image source carries a field")),
            ),
            (
                "an image source of type url",
                vec![user(vec![{ let mut b = img("image/png", "AA"); b["source"]["type"] = json!("url"); b }])],
                Some((0, "image source type is not `base64` or `reference`")),
            ),
            (
                "an image source with no type",
                vec![user(vec![{
                    let mut b = img("image/png", "AA");
                    b["source"].as_object_mut().unwrap().remove("type");
                    b
                }])],
                Some((0, "type is not `base64`")),
            ),
            (
                "a reference source carrying data",
                vec![user(vec![{ let mut b = img_ref("image/png", "k"); b["source"]["data"] = json!("AA"); b }])],
                Some((0, "image source carries a field the contract does not allow: \"data\"")),
            ),
            (
                "a base64 source carrying a reference",
                vec![user(vec![{ let mut b = img("image/png", "AA"); b["source"]["reference"] = json!("k"); b }])],
                Some((0, "image source carries a field the contract does not allow: \"reference\"")),
            ),
            (
                "an empty reference",
                vec![user(vec![img_ref("image/png", "")])],
                Some((0, "no non-empty string `reference`")),
            ),
            (
                "a numeric reference",
                vec![user(vec![{ let mut b = img_ref("image/png", "k"); b["source"]["reference"] = json!(5); b }])],
                Some((0, "no non-empty string `reference`")),
            ),
            (
                "a reference source with no reference",
                vec![user(vec![{
                    let mut b = img_ref("image/png", "k");
                    b["source"].as_object_mut().unwrap().remove("reference");
                    b
                }])],
                Some((0, "no non-empty string `reference`")),
            ),
            (
                "a reference of a media type the contract does not name",
                vec![user(vec![img_ref("image/bmp", "k")])],
                Some((0, "media type the contract does not name: image/bmp")),
            ),
            (
                "a tool_result after an image reference",
                {
                    let mut m = exchange();
                    m[2] = user(vec![img_ref("image/png", "k"), tr("c1", "r1"), tr("c2", "r2")]);
                    m
                },
                Some((2, "after an image block")),
            ),
            (
                "an image source with no media type",
                vec![user(vec![{
                    let mut b = img("image/png", "AA");
                    b["source"].as_object_mut().unwrap().remove("media_type");
                    b
                }])],
                Some((0, "media type the contract does not name: (none)")),
            ),
            (
                "an image source with no data",
                vec![user(vec![{
                    let mut b = img("image/png", "AA");
                    b["source"].as_object_mut().unwrap().remove("data");
                    b
                }])],
                Some((0, "no non-empty string `data`")),
            ),
            (
                "an image of a media type the contract does not name",
                vec![user(vec![img("image/bmp", "AA")])],
                Some((0, "media type the contract does not name")),
            ),
            (
                "an image with empty data",
                vec![user(vec![img("image/png", "")])],
                Some((0, "non-empty string `data`")),
            ),
            (
                "an image with numeric data",
                vec![user(vec![{ let mut b = img("image/png", "AA"); b["source"]["data"] = json!(5); b }])],
                Some((0, "non-empty string `data`")),
            ),
            (
                "a tool_result after an image block",
                {
                    let mut m = exchange();
                    m[2] = user(vec![img("image/png", "AA"), tr("c1", "r1"), tr("c2", "r2")]);
                    m
                },
                Some((2, "after an image block")),
            ),
            (
                "an image block in an assistant message",
                vec![user(vec![txt("a")]), asst(vec![img("image/png", "AA")])],
                Some((1, "unknown assistant content block type")),
            ),
            (
                "a missing result",
                {
                    let mut m = exchange();
                    m[2] = user(vec![tr("c1", "r1")]);
                    m
                },
                Some((2, "answers 1 tool calls")),
            ),
            (
                "an extra result",
                {
                    let mut m = exchange();
                    m[2] = user(vec![tr("c1", "r1"), tr("c2", "r2"), tr("c3", "r3")]);
                    m
                },
                Some((2, "answers 3 tool calls")),
            ),
            (
                "results out of order",
                {
                    let mut m = exchange();
                    m[2] = user(vec![tr("c2", "r2"), tr("c1", "r1")]);
                    m
                },
                Some((2, "answers \"c2\"")),
            ),
            (
                "a result with nothing to answer",
                vec![
                    user(vec![txt("a")]),
                    asst(vec![txt("b")]),
                    user(vec![tr("c1", "r1")]),
                ],
                Some((2, "answers 1 tool calls")),
            ),
            (
                "a trailing unanswered call",
                vec![user(vec![txt("a")]), asst(vec![tool_use("c1", "a")])],
                Some((2, "unanswered")),
            ),
        ];
        for (name, messages, expect) in table {
            let got = Transcript::from_messages(messages.clone());
            match (expect, got) {
                (None, Ok(t)) => assert_eq!(t.messages(), &messages[..], "{name}"),
                (Some((index, why)), Err((at, error))) => {
                    assert_eq!(at, index, "{name}: {error}");
                    assert!(error.contains(why), "{name}: {error}");
                }
                (None, Err(e)) => panic!("{name}: refused: {e:?}"),
                (Some(_), Ok(_)) => panic!("{name}: accepted"),
            }
        }
    }
}
