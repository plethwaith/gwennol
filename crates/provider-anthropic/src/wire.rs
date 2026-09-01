//! Translation between the `LLM_CHAT` contract and the Anthropic
//! Messages API — pure with respect to the guest ABI, so every rule
//! here is unit-tested on the host target and the wasm entry points
//! only wire it to streams and steps.
//!
//! Three directions:
//!
//! - **request**: [`build_request`] turns the contract's `chat` input
//!   plus the plugin's `$config` into a Messages API request body. The
//!   contract's block shapes were chosen to be the vendor's, so
//!   `messages` and `tools` pass through verbatim; what needs code is
//!   the defaults and the knobs the contract deliberately leaves to
//!   config (`docs/SPI.md`, "Known exclusions").
//! - **buffered answer**: [`buffered_output`] turns an HTTP status and
//!   body into the contract's buffered output — the message, or the
//!   `Failure` the contract's 0.2.0 buffered form carries when the
//!   vendor answered and said no.
//! - **stream**: [`StreamTranslator`] is the state machine that folds
//!   the Messages API's server-sent events into contract events: text
//!   deltas relayed as they arrive, tool calls buffered and emitted
//!   whole, `end` with the mapped stop reason and merged usage, or an
//!   `error` event that ends the stream.
//!
//! # What is fail-closed and what is ignored
//!
//! The contract's enumerations are fail-closed for *consumers*: a stop
//! reason outside the enum fails the turn. So this translator maps
//! every vendor stop reason it knows and turns any other — `pause_turn`
//! included, which the provider cannot resume — into a `Failure`, never
//! into a guess. The vendor's *own* forward-compatibility rule runs the
//! other way: unknown SSE event types, delta types and content-block
//! types are documented as safe to ignore, and are. Thinking blocks are
//! the one known kind ignored on purpose: the contract carries no block
//! for them (a known exclusion), so a turn that produced them keeps its
//! text and tool calls and nothing else. The request sends no
//! `thinking` field unless `$config.thinking` supplies one — absence is
//! the setting every current model accepts, where an explicit
//! `disabled` is refused by the models that cannot turn thinking off —
//! which means thinking is *on* by the vendor's default on current
//! models, and a later tool-use turn replays a history without the
//! blocks the vendor produced. Whether the vendor accepts that history
//! is exactly what the live smoke test the roadmap calls for must
//! establish; if it refuses, the contract's exclusion is the thing to
//! reopen, not this default.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

/// The model used when `$config.model` is absent.
pub const DEFAULT_MODEL: &str = "claude-opus-5";

/// The Messages API version the manifest's `anthropic-version` header
/// names. Declared here so the integration suite can pin the manifest
/// to it; the guest itself sends no headers.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// `max_tokens` for a buffered turn when neither the input nor
/// `$config.max_tokens` says: high enough not to cut an answer
/// mid-thought, low enough that the buffered request finishes inside
/// its HTTP timeout.
pub const DEFAULT_MAX_TOKENS_BUFFERED: u64 = 16_384;

/// `max_tokens` for a streamed turn when nothing says otherwise. A
/// stream is not bound by a response timeout, so the model gets room.
pub const DEFAULT_MAX_TOKENS_STREAMED: u64 = 65_536;

/// A request body ready for `host_http.post`, and whether the turn is
/// the contract's streamed form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// The Messages API request body.
    pub body: Value,
    /// `true` for the contract's `stream: true` form.
    pub stream: bool,
}

/// The API origin used when `$config.base_url` is absent.
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// The Messages API endpoint for a `$config`: `base_url` (default
/// [`DEFAULT_BASE_URL`]), trailing slashes dropped, plus
/// `/v1/messages`. Built here rather than templated in the manifest so
/// a `base_url` with a trailing slash — or a path prefix, for a proxy —
/// does not yield `//v1/messages`; the host the request reaches is
/// still pinned by the manifest's `network:egress` grant, not by this.
pub fn messages_url(config: &Value) -> Result<String, String> {
    let base = match config.get("base_url") {
        None | Some(Value::Null) => DEFAULT_BASE_URL,
        Some(Value::String(s)) => s.as_str(),
        Some(other) => return Err(format!("config.base_url must be a string, got {other}")),
    };
    let base = base.trim_end_matches('/');
    if !(base.starts_with("https://") || base.starts_with("http://")) {
        return Err(format!(
            "config.base_url must be an http(s) origin, got {base:?}"
        ));
    }
    Ok(format!("{base}/v1/messages"))
}

/// Build the Messages API request for one `chat` input.
///
/// `input` is the step's resolution context (the `chat` input fields
/// flattened at its root); `config` is the plugin's `$config`. Config
/// keys: `model`, `max_tokens`, `thinking` (a Messages API `thinking`
/// object, sent verbatim; absent means the field is not sent and the
/// model's own default applies — see the module docs for what that
/// costs the contract), and `extra` — an object shallow-merged into the
/// request for any key the provider did not itself set, the contract's
/// sanctioned place for sampling knobs and vendor features it
/// deliberately does not carry.
pub fn build_request(input: &Value, config: &Value) -> Result<Request, String> {
    let messages = match input.get("messages") {
        Some(Value::Array(m)) if !m.is_empty() => Value::Array(m.clone()),
        Some(Value::Array(_)) => return Err("chat input carries no messages".into()),
        Some(other) => {
            return Err(format!(
                "chat input `messages` must be an array, got {other}"
            ));
        }
        None => return Err("chat input carries no messages".into()),
    };
    let stream = input
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let model = match config.get("model") {
        None | Some(Value::Null) => DEFAULT_MODEL.to_string(),
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        Some(other) => {
            return Err(format!(
                "config.model must be a non-empty string, got {other}"
            ));
        }
    };
    let max_tokens = match input.get("max_tokens").or_else(|| config.get("max_tokens")) {
        None | Some(Value::Null) => {
            if stream {
                DEFAULT_MAX_TOKENS_STREAMED
            } else {
                DEFAULT_MAX_TOKENS_BUFFERED
            }
        }
        Some(v) => match v.as_u64() {
            Some(n) if n >= 1 => n,
            _ => return Err(format!("max_tokens must be a positive integer, got {v}")),
        },
    };
    let mut body = Map::new();
    body.insert("model".into(), Value::String(model));
    body.insert("max_tokens".into(), json!(max_tokens));
    body.insert("stream".into(), Value::Bool(stream));
    // `thinking` is sent only when config says: the field's absence is
    // the one setting every current model accepts (adaptive where it
    // is on by default, off where it is off), whereas an explicit
    // `disabled` is a 400 on the models that cannot turn it off.
    match config.get("thinking") {
        None | Some(Value::Null) => {}
        Some(v @ Value::Object(_)) => {
            body.insert("thinking".into(), v.clone());
        }
        Some(other) => return Err(format!("config.thinking must be an object, got {other}")),
    }
    body.insert("messages".into(), messages);
    if let Some(system) = input.get("system") {
        match system {
            Value::String(_) => {
                body.insert("system".into(), system.clone());
            }
            Value::Null => {}
            other => return Err(format!("chat input `system` must be a string, got {other}")),
        }
    }
    if let Some(tools) = input.get("tools") {
        match tools {
            // The contract's Tool is the vendor's tool definition —
            // name, description, input_schema — so it travels verbatim.
            // An empty list is the same as none.
            Value::Array(t) if !t.is_empty() => {
                body.insert("tools".into(), tools.clone());
            }
            Value::Array(_) | Value::Null => {}
            other => return Err(format!("chat input `tools` must be an array, got {other}")),
        }
    }
    match config.get("extra") {
        None | Some(Value::Null) => {}
        Some(Value::Object(extra)) => {
            for (key, value) in extra {
                // The provider's own keys win: `extra` cannot override
                // what the contract or the config already decided.
                body.entry(key.clone()).or_insert_with(|| value.clone());
            }
        }
        Some(other) => return Err(format!("config.extra must be an object, got {other}")),
    }
    Ok(Request {
        body: Value::Object(body),
        stream,
    })
}

/// Why a turn failed, with the vendor's answer in hand — the contract's
/// `Failure`, carried either as the buffered `{"error": …}` output or as
/// the stream's `error` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// Human-readable, informational.
    pub message: String,
    /// Worth repeating unchanged? `None` when the provider cannot say.
    pub retryable: Option<bool>,
    /// The vendor's own class identifier (an `error.type`), or a
    /// provider-assigned one for failures the vendor did not classify.
    pub kind: Option<String>,
}

impl Failure {
    fn new(
        message: impl Into<String>,
        retryable: Option<bool>,
        kind: impl Into<String>,
    ) -> Failure {
        Failure {
            message: message.into(),
            retryable,
            kind: Some(kind.into()),
        }
    }

    fn fields(&self) -> Map<String, Value> {
        let mut fields = Map::new();
        fields.insert("message".into(), Value::String(self.message.clone()));
        if let Some(retryable) = self.retryable {
            fields.insert("retryable".into(), Value::Bool(retryable));
        }
        if let Some(kind) = &self.kind {
            fields.insert("kind".into(), Value::String(kind.clone()));
        }
        fields
    }

    /// The contract's stream `error` event.
    pub fn into_stream_event(self) -> Value {
        let mut fields = self.fields();
        fields.insert("type".into(), Value::String("error".into()));
        Value::Object(fields)
    }

    /// The contract's buffered failed form.
    pub fn into_buffered_output(self) -> Value {
        json!({ "error": Value::Object(self.fields()) })
    }
}

use gwennol_guest::retryable_http_status as retryable_status;

/// Whether a Messages API `error.type` is worth repeating unchanged,
/// where the documented types say; `None` for one this crate does not
/// know.
fn retryable_kind(kind: &str) -> Option<bool> {
    match kind {
        "overloaded_error" | "rate_limit_error" | "api_error" | "timeout_error" => Some(true),
        "authentication_error"
        | "permission_error"
        | "invalid_request_error"
        | "not_found_error"
        | "request_too_large"
        | "billing_error" => Some(false),
        _ => None,
    }
}

/// The `{"type": "error", "error": {"type", "message"}}` document the
/// Messages API uses for both non-2xx bodies and stream error events.
fn vendor_error(document: &Value) -> Option<(String, String)> {
    let error = document.get("error")?;
    let kind = error.get("type")?.as_str()?.to_string();
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Some((kind, message))
}

/// The failure a non-2xx answer amounts to. The body is the vendor's
/// error document when it parses as one — `kind` is then its
/// `error.type` — and an opaque excerpt otherwise.
pub fn http_failure(status: i64, body: &str) -> Failure {
    let by_status = retryable_status(status);
    if let Some((kind, message)) = serde_json::from_str::<Value>(body)
        .ok()
        .as_ref()
        .and_then(vendor_error)
    {
        let text = if message.is_empty() {
            format!("vendor answered HTTP {status}: {kind}")
        } else {
            format!("vendor answered HTTP {status}: {kind}: {message}")
        };
        // Either signal suffices: an overload is retryable whether it
        // arrived as a 529 or as its error type.
        let retryable = by_status || retryable_kind(&kind).unwrap_or(false);
        return Failure::new(text, Some(retryable), kind);
    }
    let body = body.trim();
    let text = if body.is_empty() {
        format!("vendor answered HTTP {status}")
    } else {
        format!("vendor answered HTTP {status}: {body}")
    };
    Failure::new(text, Some(by_status), format!("http_{status}"))
}

/// The failure a vendor error *event* (or an error document on a 2xx)
/// amounts to.
fn event_failure(document: &Value) -> Failure {
    match vendor_error(document) {
        Some((kind, message)) => {
            let text = if message.is_empty() {
                format!("vendor error: {kind}")
            } else {
                format!("vendor error: {kind}: {message}")
            };
            let retryable = retryable_kind(&kind);
            Failure::new(text, retryable, kind)
        }
        None => Failure::new(
            format!("vendor error event without an error document: {document}"),
            None,
            "malformed_error",
        ),
    }
}

/// The contract stop reason for a vendor one — or the failure a stop
/// reason the contract cannot express amounts to. Fail-closed: a value
/// this crate does not know is a failed turn, never a guess.
fn map_stop_reason(raw: Option<&str>) -> Result<&'static str, Failure> {
    match raw {
        Some("end_turn") | Some("stop_sequence") => Ok("end_turn"),
        Some("tool_use") => Ok("tool_use"),
        Some("max_tokens") => Ok("max_tokens"),
        Some("refusal") => Ok("refusal"),
        Some("pause_turn") => Err(Failure::new(
            "the vendor paused the turn (pause_turn); this provider does not resume paused turns",
            Some(false),
            "pause_turn",
        )),
        Some(other) => Err(Failure::new(
            format!("the vendor reported a stop reason this provider does not know: {other}"),
            Some(false),
            "unknown_stop_reason",
        )),
        None => Err(Failure::new(
            "the vendor reported no stop reason",
            None,
            "missing_stop_reason",
        )),
    }
}

/// The contract's `usage` for a vendor one: passed through whole (the
/// contract's object is open, so cache counters ride along) once the
/// two required counters are present. Their absence is a failed turn
/// rather than a fabricated zero.
fn contract_usage(usage: &Map<String, Value>) -> Result<Value, Failure> {
    for key in ["input_tokens", "output_tokens"] {
        if !usage.get(key).is_some_and(Value::is_u64) {
            return Err(Failure::new(
                format!("the vendor reported no {key} in usage"),
                None,
                "missing_usage",
            ));
        }
    }
    Ok(Value::Object(usage.clone()))
}

/// One vendor content block as a contract block, `None` for the kinds
/// the contract does not carry (thinking) or this crate does not know.
fn contract_block(block: &Value) -> Result<Option<Value>, Failure> {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => {
            let text = block.get("text").and_then(Value::as_str).unwrap_or("");
            Ok(Some(json!({ "type": "text", "text": text })))
        }
        Some("tool_use") => {
            let (Some(id), Some(name)) = (
                block.get("id").and_then(Value::as_str),
                block.get("name").and_then(Value::as_str),
            ) else {
                return Err(Failure::new(
                    format!("tool_use block without id and name: {block}"),
                    Some(false),
                    "malformed_tool_use",
                ));
            };
            let Some(input @ Value::Object(_)) = block.get("input") else {
                return Err(Failure::new(
                    format!("tool_use block {id} carries no object input"),
                    Some(false),
                    "malformed_tool_input",
                ));
            };
            Ok(Some(json!({
                "type": "tool_use", "id": id, "name": name, "input": input
            })))
        }
        _ => Ok(None),
    }
}

/// The contract's buffered output for a Messages API answer.
///
/// `truncated` is the fetch step's own signal that the body was cut at
/// the host's cap: a cut JSON document is not a message, and the
/// failure says so rather than reporting a parse error.
pub fn buffered_output(status: i64, body: &str, truncated: bool) -> Value {
    if truncated {
        return Failure::new(
            "the vendor's response exceeded the buffered body cap",
            Some(false),
            "response_too_large",
        )
        .into_buffered_output();
    }
    if !(200..300).contains(&status) {
        return http_failure(status, body).into_buffered_output();
    }
    match translate_message(body) {
        Ok(output) => output,
        Err(failure) => failure.into_buffered_output(),
    }
}

fn translate_message(body: &str) -> Result<Value, Failure> {
    let document: Value = serde_json::from_str(body).map_err(|e| {
        Failure::new(
            format!("the vendor's response is not JSON: {e}"),
            None,
            "malformed_response",
        )
    })?;
    if document.get("type").and_then(Value::as_str) == Some("error") {
        return Err(event_failure(&document));
    }
    let mut content = Vec::new();
    if let Some(blocks) = document.get("content").and_then(Value::as_array) {
        for block in blocks {
            if let Some(mapped) = contract_block(block)? {
                content.push(mapped);
            }
        }
    }
    let stop_reason = map_stop_reason(document.get("stop_reason").and_then(Value::as_str))?;
    let usage = match document.get("usage") {
        Some(Value::Object(usage)) => contract_usage(usage)?,
        _ => contract_usage(&Map::new())?,
    };
    Ok(json!({
        "message": { "role": "assistant", "content": content },
        "stop_reason": stop_reason,
        "usage": usage,
    }))
}

/// What the stream translator produced for one vendor event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Emitted {
    /// A contract event; more may follow.
    Event(Value),
    /// The stream's last event — `end` or `error`. Emit it, then stop
    /// reading: nothing the vendor sends afterwards is relayed.
    Terminal(Value),
}

/// A content block in flight.
#[derive(Debug)]
enum Block {
    Text,
    ToolUse {
        id: String,
        name: String,
        partial_json: String,
    },
    /// Thinking, or a kind this crate does not know: its deltas are
    /// consumed and dropped.
    Ignored,
}

/// The Messages API stream as contract events.
///
/// Feed it each server-sent event (its `event:` name and `data:`
/// payload) in order; it returns what to emit. After a
/// [`Emitted::Terminal`] it returns nothing more.
#[derive(Debug, Default)]
pub struct StreamTranslator {
    blocks: HashMap<u64, Block>,
    usage: Map<String, Value>,
    stop_reason: Option<String>,
    /// A tool call whose buffered input never parsed: decided at
    /// `message_stop`, because the contract drops such a call silently
    /// when `max_tokens` cut it and fails the turn otherwise.
    broken_call: Option<String>,
    finished: bool,
}

impl StreamTranslator {
    pub fn new() -> StreamTranslator {
        StreamTranslator::default()
    }

    /// Fold one vendor event.
    pub fn accept(&mut self, event_name: &str, data: &str) -> Vec<Emitted> {
        if self.finished {
            return Vec::new();
        }
        let document: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(e) => {
                return self.finish(Failure::new(
                    format!("vendor event payload is not JSON: {e}; payload: {data:?}"),
                    None,
                    "malformed_stream",
                ));
            }
        };
        // The payload's own `type` is authoritative; the SSE event name
        // is the fallback for a vendor that sends one without the other.
        let kind = document
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or(event_name)
            .to_string();
        match kind.as_str() {
            "message_start" => {
                if let Some(Value::Object(usage)) = document.pointer("/message/usage") {
                    self.usage.extend(usage.clone());
                }
                Vec::new()
            }
            "content_block_start" => self.block_start(&document),
            "content_block_delta" => self.block_delta(&document),
            "content_block_stop" => self.block_stop(&document),
            "message_delta" => {
                if let Some(reason) = document
                    .pointer("/delta/stop_reason")
                    .and_then(Value::as_str)
                {
                    self.stop_reason = Some(reason.to_string());
                }
                if let Some(Value::Object(usage)) = document.get("usage") {
                    // Later counters win: `message_delta` carries the
                    // final output count, and (on newer API versions)
                    // restates the input side too.
                    self.usage.extend(usage.clone());
                }
                Vec::new()
            }
            "message_stop" => self.message_stop(),
            "error" => self.finish(event_failure(&document)),
            // `ping`, and anything newer: the vendor documents unknown
            // event types as safe to ignore.
            _ => Vec::new(),
        }
    }

    fn index(document: &Value) -> Option<u64> {
        document.get("index").and_then(Value::as_u64)
    }

    fn block_start(&mut self, document: &Value) -> Vec<Emitted> {
        let Some(index) = Self::index(document) else {
            return Vec::new();
        };
        let block = document.get("content_block");
        let started = match block.and_then(|b| b.get("type")).and_then(Value::as_str) {
            Some("text") => Block::Text,
            Some("tool_use") => {
                let (Some(id), Some(name)) = (
                    block.and_then(|b| b.get("id")).and_then(Value::as_str),
                    block.and_then(|b| b.get("name")).and_then(Value::as_str),
                ) else {
                    return self.finish(Failure::new(
                        format!("tool_use block without id and name: {document}"),
                        Some(false),
                        "malformed_tool_use",
                    ));
                };
                Block::ToolUse {
                    id: id.to_string(),
                    name: name.to_string(),
                    partial_json: String::new(),
                }
            }
            _ => Block::Ignored,
        };
        self.blocks.insert(index, started);
        Vec::new()
    }

    fn block_delta(&mut self, document: &Value) -> Vec<Emitted> {
        let Some(index) = Self::index(document) else {
            return Vec::new();
        };
        let delta = document.get("delta");
        let delta_type = delta.and_then(|d| d.get("type")).and_then(Value::as_str);
        match (self.blocks.get_mut(&index), delta_type) {
            (Some(Block::Text), Some("text_delta")) => {
                match delta.and_then(|d| d.get("text")).and_then(Value::as_str) {
                    Some(text) if !text.is_empty() => {
                        vec![Emitted::Event(json!({ "type": "text", "text": text }))]
                    }
                    _ => Vec::new(),
                }
            }
            (Some(Block::ToolUse { partial_json, .. }), Some("input_json_delta")) => {
                if let Some(fragment) = delta
                    .and_then(|d| d.get("partial_json"))
                    .and_then(Value::as_str)
                {
                    partial_json.push_str(fragment);
                }
                Vec::new()
            }
            // A delta for an ignored block, an unknown delta type, or a
            // delta for a block never started: nothing the contract
            // carries.
            _ => Vec::new(),
        }
    }

    fn block_stop(&mut self, document: &Value) -> Vec<Emitted> {
        let Some(index) = Self::index(document) else {
            return Vec::new();
        };
        match self.blocks.remove(&index) {
            Some(Block::ToolUse {
                id,
                name,
                partial_json,
            }) => {
                // An empty input streams as no deltas at all.
                let raw = if partial_json.trim().is_empty() {
                    "{}"
                } else {
                    partial_json.as_str()
                };
                match serde_json::from_str::<Value>(raw) {
                    Ok(input @ Value::Object(_)) => vec![Emitted::Event(json!({
                        "type": "tool_use", "id": id, "name": name, "input": input
                    }))],
                    // Decided at message_stop: `max_tokens` drops it,
                    // anything else is a failed turn. Only the first
                    // broken call is remembered; one is enough to fail.
                    _ => {
                        self.broken_call
                            .get_or_insert_with(|| format!("tool call {id} ({name})"));
                        Vec::new()
                    }
                }
            }
            _ => Vec::new(),
        }
    }

    fn message_stop(&mut self) -> Vec<Emitted> {
        let stop_reason = self.stop_reason.as_deref();
        if let Some(broken) = self.broken_call.take() {
            // The contract: a streamed turn that hits max_tokens while a
            // tool call is still being generated drops the partial call
            // and ends with max_tokens. Any other cut is malformed
            // vendor output.
            if stop_reason != Some("max_tokens") {
                return self.finish(Failure::new(
                    format!("{broken}: the vendor's input JSON never became a complete object"),
                    Some(false),
                    "malformed_tool_input",
                ));
            }
        }
        let stop_reason = match map_stop_reason(stop_reason) {
            Ok(reason) => reason,
            Err(failure) => return self.finish(failure),
        };
        let usage = match contract_usage(&self.usage) {
            Ok(usage) => usage,
            Err(failure) => return self.finish(failure),
        };
        self.finished = true;
        vec![Emitted::Terminal(json!({
            "type": "end", "stop_reason": stop_reason, "usage": usage
        }))]
    }

    fn finish(&mut self, failure: Failure) -> Vec<Emitted> {
        self.finished = true;
        vec![Emitted::Terminal(failure.into_stream_event())]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(stream: bool) -> Value {
        json!({
            "system": "be brief",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
            "tools": [{"name": "read", "description": "Read.", "input_schema": {"type": "object"}}],
            "stream": stream,
            "config": {}, "secrets": {}, "vars": {}, "steps": {}
        })
    }

    // ------------------------------------------------------- request

    #[test]
    fn the_request_carries_the_contract_fields_verbatim_with_defaults() {
        let r = build_request(&input(true), &json!({})).unwrap();
        assert!(r.stream);
        assert_eq!(r.body["model"], DEFAULT_MODEL);
        assert_eq!(r.body["max_tokens"], DEFAULT_MAX_TOKENS_STREAMED);
        assert_eq!(r.body["stream"], true);
        assert!(
            r.body.get("thinking").is_none(),
            "no thinking field unless config says: absent is what every model accepts"
        );
        assert_eq!(r.body["system"], "be brief");
        assert_eq!(r.body["messages"], input(true)["messages"]);
        assert_eq!(r.body["tools"], input(true)["tools"]);

        let r = build_request(&input(false), &json!({})).unwrap();
        assert!(!r.stream);
        assert_eq!(r.body["max_tokens"], DEFAULT_MAX_TOKENS_BUFFERED);
    }

    #[test]
    fn config_sets_model_max_tokens_thinking_and_extras_without_overriding_the_contract() {
        let config = json!({
            "model": "claude-sonnet-5",
            "max_tokens": 300,
            "thinking": {"type": "adaptive"},
            "extra": {"temperature": 0.2, "messages": "never", "stream": "never"}
        });
        let r = build_request(&input(false), &config).unwrap();
        assert_eq!(r.body["model"], "claude-sonnet-5");
        assert_eq!(r.body["max_tokens"], 300);
        assert_eq!(r.body["thinking"], json!({"type": "adaptive"}));
        assert_eq!(
            r.body["temperature"], 0.2,
            "an extra the provider did not set"
        );
        assert_eq!(
            r.body["messages"],
            input(false)["messages"],
            "extras cannot override"
        );
        assert_eq!(r.body["stream"], false);

        // The input's max_tokens beats config's.
        let mut with_max = input(false);
        with_max["max_tokens"] = json!(7);
        let r = build_request(&with_max, &config).unwrap();
        assert_eq!(r.body["max_tokens"], 7);
    }

    #[test]
    fn the_endpoint_tolerates_trailing_slashes_and_path_prefixes() {
        assert_eq!(
            messages_url(&json!({})).unwrap(),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            messages_url(&json!({"base_url": "https://api.anthropic.com/"})).unwrap(),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            messages_url(&json!({"base_url": "http://127.0.0.1:8080/proxy//"})).unwrap(),
            "http://127.0.0.1:8080/proxy/v1/messages"
        );
        let err = messages_url(&json!({"base_url": "api.anthropic.com"})).unwrap_err();
        assert!(err.contains("http(s)"), "{err}");
        let err = messages_url(&json!({"base_url": 7})).unwrap_err();
        assert!(err.contains("config.base_url"), "{err}");
    }

    #[test]
    fn malformed_input_and_config_are_readable_errors() {
        let mut no_messages = input(false);
        no_messages["messages"] = json!([]);
        assert!(
            build_request(&no_messages, &json!({}))
                .unwrap_err()
                .contains("messages")
        );
        let err = build_request(&input(false), &json!({"model": 3})).unwrap_err();
        assert!(err.contains("config.model"), "{err}");
        let err = build_request(&input(false), &json!({"thinking": "yes"})).unwrap_err();
        assert!(err.contains("config.thinking"), "{err}");
        let mut bad_max = input(false);
        bad_max["max_tokens"] = json!(0);
        let err = build_request(&bad_max, &json!({})).unwrap_err();
        assert!(err.contains("max_tokens"), "{err}");
    }

    // ------------------------------------------------------ failures

    #[test]
    fn http_failures_classify_by_status_and_by_vendor_error_type() {
        let f = http_failure(
            429,
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#,
        );
        assert_eq!(f.retryable, Some(true));
        assert_eq!(f.kind.as_deref(), Some("rate_limit_error"));
        assert!(
            f.message.contains("429") && f.message.contains("slow down"),
            "{}",
            f.message
        );

        let f = http_failure(
            401,
            r#"{"type":"error","error":{"type":"authentication_error","message":"bad key"}}"#,
        );
        assert_eq!(f.retryable, Some(false));

        // A 200-class error type on a 4xx still counts the type.
        let f = http_failure(
            400,
            r#"{"type":"error","error":{"type":"overloaded_error"}}"#,
        );
        assert_eq!(f.retryable, Some(true));

        let f = http_failure(502, "<html>bad gateway</html>");
        assert_eq!(f.retryable, Some(true));
        assert_eq!(f.kind.as_deref(), Some("http_502"));
        assert!(f.message.contains("bad gateway"));

        let f = http_failure(404, "");
        assert_eq!(f.retryable, Some(false));
        assert_eq!(f.message, "vendor answered HTTP 404");
    }

    #[test]
    fn a_failure_renders_both_contract_forms() {
        let f = Failure::new("why", Some(true), "k");
        assert_eq!(
            f.clone().into_stream_event(),
            json!({"type": "error", "message": "why", "retryable": true, "kind": "k"})
        );
        assert_eq!(
            f.into_buffered_output(),
            json!({"error": {"message": "why", "retryable": true, "kind": "k"}})
        );
        let unknown = Failure {
            message: "?".into(),
            retryable: None,
            kind: None,
        };
        assert_eq!(
            unknown.into_stream_event(),
            json!({"type": "error", "message": "?"})
        );
    }

    // ------------------------------------------------------ buffered

    const HAPPY_MESSAGE: &str = r#"{
        "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
        "content": [
            {"type": "thinking", "thinking": "", "signature": "sig"},
            {"type": "text", "text": "Reading it.", "citations": null},
            {"type": "tool_use", "id": "toolu_1", "name": "read", "input": {"path": "a.rs"}}
        ],
        "stop_reason": "tool_use", "stop_sequence": null,
        "usage": {"input_tokens": 10, "output_tokens": 20, "cache_read_input_tokens": 3}
    }"#;

    #[test]
    fn a_buffered_message_maps_to_the_contract_dropping_thinking() {
        let out = buffered_output(200, HAPPY_MESSAGE, false);
        assert_eq!(
            out,
            json!({
                "message": {"role": "assistant", "content": [
                    {"type": "text", "text": "Reading it."},
                    {"type": "tool_use", "id": "toolu_1", "name": "read", "input": {"path": "a.rs"}}
                ]},
                "stop_reason": "tool_use",
                "usage": {"input_tokens": 10, "output_tokens": 20, "cache_read_input_tokens": 3}
            })
        );
    }

    #[test]
    fn buffered_stop_reasons_map_or_fail_closed() {
        let with = |reason: &str| {
            format!(
                r#"{{"content": [], "stop_reason": "{reason}", "usage": {{"input_tokens": 1, "output_tokens": 0}}}}"#
            )
        };
        assert_eq!(
            buffered_output(200, &with("stop_sequence"), false)["stop_reason"],
            "end_turn"
        );
        assert_eq!(
            buffered_output(200, &with("refusal"), false)["stop_reason"],
            "refusal"
        );
        assert_eq!(
            buffered_output(200, &with("max_tokens"), false)["message"]["content"],
            json!([])
        );
        let paused = buffered_output(200, &with("pause_turn"), false);
        assert_eq!(paused["error"]["kind"], "pause_turn");
        assert_eq!(paused["error"]["retryable"], false);
        let novel = buffered_output(200, &with("something_new"), false);
        assert_eq!(novel["error"]["kind"], "unknown_stop_reason");
    }

    #[test]
    fn a_buffered_answer_without_usage_or_with_an_error_document_is_a_failure() {
        let out = buffered_output(200, r#"{"content": [], "stop_reason": "end_turn"}"#, false);
        assert_eq!(out["error"]["kind"], "missing_usage");
        let out = buffered_output(
            200,
            r#"{"type":"error","error":{"type":"api_error","message":"hiccup"}}"#,
            false,
        );
        assert_eq!(out["error"]["kind"], "api_error");
        assert_eq!(out["error"]["retryable"], true);
        let out = buffered_output(200, "not json", false);
        assert_eq!(out["error"]["kind"], "malformed_response");
        let out = buffered_output(200, HAPPY_MESSAGE, true);
        assert_eq!(out["error"]["kind"], "response_too_large");
        let out = buffered_output(
            529,
            r#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#,
            false,
        );
        assert_eq!(out["error"]["retryable"], true);
    }

    // -------------------------------------------------------- stream

    fn feed(t: &mut StreamTranslator, events: &[(&str, Value)]) -> Vec<Emitted> {
        events
            .iter()
            .flat_map(|(name, data)| t.accept(name, &data.to_string()))
            .collect()
    }

    fn happy_stream() -> Vec<(&'static str, Value)> {
        vec![
            (
                "message_start",
                json!({"type": "message_start", "message": {"id": "msg_1", "usage": {"input_tokens": 10, "output_tokens": 1, "cache_read_input_tokens": 4}}}),
            ),
            ("ping", json!({"type": "ping"})),
            (
                "content_block_start",
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
            ),
            (
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "hmm"}}),
            ),
            (
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "s"}}),
            ),
            (
                "content_block_stop",
                json!({"type": "content_block_stop", "index": 0}),
            ),
            (
                "content_block_start",
                json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
            ),
            (
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "Hel"}}),
            ),
            (
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "lo"}}),
            ),
            (
                "content_block_stop",
                json!({"type": "content_block_stop", "index": 1}),
            ),
            (
                "content_block_start",
                json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "read", "input": {}}}),
            ),
            (
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"pa"}}),
            ),
            (
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "th\": \"a.rs\"}"}}),
            ),
            (
                "content_block_stop",
                json!({"type": "content_block_stop", "index": 2}),
            ),
            (
                "message_delta",
                json!({"type": "message_delta", "delta": {"stop_reason": "tool_use", "stop_sequence": null}, "usage": {"output_tokens": 25}}),
            ),
            ("message_stop", json!({"type": "message_stop"})),
            (
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "straggler"}}),
            ),
        ]
    }

    #[test]
    fn the_stream_relays_text_buffers_tool_calls_and_ends_with_merged_usage() {
        let mut t = StreamTranslator::new();
        let out = feed(&mut t, &happy_stream());
        assert_eq!(
            out,
            vec![
                Emitted::Event(json!({"type": "text", "text": "Hel"})),
                Emitted::Event(json!({"type": "text", "text": "lo"})),
                Emitted::Event(
                    json!({"type": "tool_use", "id": "toolu_1", "name": "read", "input": {"path": "a.rs"}})
                ),
                Emitted::Terminal(json!({"type": "end", "stop_reason": "tool_use",
                    "usage": {"input_tokens": 10, "output_tokens": 25, "cache_read_input_tokens": 4}})),
            ],
            "thinking dropped, pings dropped, the straggler after message_stop dropped"
        );
    }

    #[test]
    fn a_tool_call_with_no_deltas_has_an_empty_input() {
        let mut t = StreamTranslator::new();
        let out = feed(
            &mut t,
            &[
                (
                    "content_block_start",
                    json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "t", "name": "list", "input": {}}}),
                ),
                (
                    "content_block_stop",
                    json!({"type": "content_block_stop", "index": 0}),
                ),
            ],
        );
        assert_eq!(
            out,
            vec![Emitted::Event(
                json!({"type": "tool_use", "id": "t", "name": "list", "input": {}})
            )]
        );
    }

    #[test]
    fn a_partial_tool_call_is_dropped_under_max_tokens_and_fails_the_turn_otherwise() {
        let cut = |stop: &str| {
            vec![
                (
                    "message_start",
                    json!({"type": "message_start", "message": {"usage": {"input_tokens": 1, "output_tokens": 1}}}),
                ),
                (
                    "content_block_start",
                    json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "t", "name": "read", "input": {}}}),
                ),
                (
                    "content_block_delta",
                    json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"path\": \"a"}}),
                ),
                (
                    "content_block_stop",
                    json!({"type": "content_block_stop", "index": 0}),
                ),
                (
                    "message_delta",
                    json!({"type": "message_delta", "delta": {"stop_reason": stop}, "usage": {"output_tokens": 9}}),
                ),
                ("message_stop", json!({"type": "message_stop"})),
            ]
        };
        let mut t = StreamTranslator::new();
        assert_eq!(
            feed(&mut t, &cut("max_tokens")),
            vec![Emitted::Terminal(
                json!({"type": "end", "stop_reason": "max_tokens", "usage": {"input_tokens": 1, "output_tokens": 9}})
            )],
            "the half-generated call does not exist on the wire"
        );
        let mut t = StreamTranslator::new();
        let out = feed(&mut t, &cut("end_turn"));
        assert_eq!(out.len(), 1);
        let Emitted::Terminal(event) = &out[0] else {
            panic!("{out:?}")
        };
        assert_eq!(event["type"], "error");
        assert_eq!(event["kind"], "malformed_tool_input");
        assert_eq!(event["retryable"], false);
    }

    #[test]
    fn a_vendor_error_event_ends_the_stream_and_nothing_follows() {
        let mut t = StreamTranslator::new();
        let out = feed(
            &mut t,
            &[
                (
                    "content_block_start",
                    json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
                ),
                (
                    "content_block_delta",
                    json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "so far"}}),
                ),
                (
                    "error",
                    json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
                ),
                (
                    "content_block_delta",
                    json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "never"}}),
                ),
                ("message_stop", json!({"type": "message_stop"})),
            ],
        );
        assert_eq!(
            out,
            vec![
                Emitted::Event(json!({"type": "text", "text": "so far"})),
                Emitted::Terminal(
                    json!({"type": "error", "message": "vendor error: overloaded_error: Overloaded", "retryable": true, "kind": "overloaded_error"})
                ),
            ]
        );
    }

    #[test]
    fn stop_reasons_the_contract_cannot_express_fail_the_stream_closed() {
        let ending = |stop: Value| {
            vec![
                (
                    "message_start",
                    json!({"type": "message_start", "message": {"usage": {"input_tokens": 1, "output_tokens": 1}}}),
                ),
                (
                    "message_delta",
                    json!({"type": "message_delta", "delta": {"stop_reason": stop}, "usage": {"output_tokens": 2}}),
                ),
                ("message_stop", json!({"type": "message_stop"})),
            ]
        };
        for (stop, kind) in [
            (json!("pause_turn"), "pause_turn"),
            (json!("brand_new"), "unknown_stop_reason"),
            (Value::Null, "missing_stop_reason"),
        ] {
            let mut t = StreamTranslator::new();
            let out = feed(&mut t, &ending(stop.clone()));
            let Some(Emitted::Terminal(event)) = out.last() else {
                panic!("{out:?}")
            };
            assert_eq!(event["type"], "error", "{stop}");
            assert_eq!(event["kind"], kind);
        }
        let mut t = StreamTranslator::new();
        let out = feed(&mut t, &ending(json!("stop_sequence")));
        assert_eq!(
            out.last().unwrap(),
            &Emitted::Terminal(
                json!({"type": "end", "stop_reason": "end_turn", "usage": {"input_tokens": 1, "output_tokens": 2}})
            )
        );
    }

    #[test]
    fn a_stream_without_usage_fails_rather_than_fabricating_zeros() {
        let mut t = StreamTranslator::new();
        let out = feed(
            &mut t,
            &[
                (
                    "message_delta",
                    json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
                ),
                ("message_stop", json!({"type": "message_stop"})),
            ],
        );
        let Some(Emitted::Terminal(event)) = out.last() else {
            panic!("{out:?}")
        };
        assert_eq!(event["kind"], "missing_usage");
    }

    #[test]
    fn a_payload_that_is_not_json_ends_the_stream_with_an_error_event() {
        let mut t = StreamTranslator::new();
        let out = t.accept("content_block_delta", "this is not JSON");
        let [Emitted::Terminal(event)] = out.as_slice() else {
            panic!("{out:?}")
        };
        assert_eq!(event["kind"], "malformed_stream");
        assert!(t.accept("message_stop", "{}").is_empty(), "spent");
    }

    #[test]
    fn unknown_events_blocks_and_deltas_are_ignored_not_fatal() {
        let mut t = StreamTranslator::new();
        let out = feed(
            &mut t,
            &[
                (
                    "message_start",
                    json!({"type": "message_start", "message": {"usage": {"input_tokens": 1, "output_tokens": 1}}}),
                ),
                ("brand_new_event", json!({"type": "brand_new_event"})),
                (
                    "content_block_start",
                    json!({"type": "content_block_start", "index": 0, "content_block": {"type": "server_tool_use", "id": "x"}}),
                ),
                (
                    "content_block_delta",
                    json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{}"}}),
                ),
                (
                    "content_block_stop",
                    json!({"type": "content_block_stop", "index": 0}),
                ),
                (
                    "content_block_start",
                    json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
                ),
                (
                    "content_block_delta",
                    json!({"type": "content_block_delta", "index": 1, "delta": {"type": "citations_delta", "citation": {}}}),
                ),
                (
                    "content_block_delta",
                    json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "ok"}}),
                ),
                (
                    "content_block_stop",
                    json!({"type": "content_block_stop", "index": 1}),
                ),
                (
                    "message_delta",
                    json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 2}}),
                ),
                ("message_stop", json!({"type": "message_stop"})),
            ],
        );
        assert_eq!(
            out,
            vec![
                Emitted::Event(json!({"type": "text", "text": "ok"})),
                Emitted::Terminal(
                    json!({"type": "end", "stop_reason": "end_turn", "usage": {"input_tokens": 1, "output_tokens": 2}})
                ),
            ]
        );
    }
}
