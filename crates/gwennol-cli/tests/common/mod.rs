//! The stub Messages API and its SSE fixtures, for the suites in this
//! directory; `headless.rs` uses it today. The opening turn thinks,
//! says something, and asks to `read` hello.txt; the follow-up quotes
//! the tool result it was given, saying whether it was an error. The
//! special routes `/stall` and `/refusal` drive a stalled connection
//! and a model refusal; `/flaky` overloads mid-round on its first
//! request and answers normally after it, so a run retries once and
//! finishes; any other route gets the opening or closing turn
//! depending on what the request carries. A wrong `x-api-key` gets a
//! 401. Every suite compiles its own copy (`mod common;`).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};

/// The key the stub accepts; anything else is a 401.
pub const API_KEY: &str = "sk-ant-headless";

/// A Messages API stand-in, listening at `addr` and recording every
/// request it receives; see the module doc for what it answers.
pub struct Stub {
    /// Where the stub listens.
    pub addr: std::net::SocketAddr,
    /// `(path, headers, body)` in arrival order.
    requests: Mutex<Vec<(String, Value, Value)>>,
}

impl Stub {
    /// Everything recorded so far. Poison-proof: a test that panics
    /// while holding the lock must not take every later test's stub
    /// down with it.
    pub fn requests(&self) -> Vec<(String, Value, Value)> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// The one running stub, started on first use and shared by every
/// test in the process.
pub fn stub() -> &'static Stub {
    static S: OnceLock<&'static Stub> = OnceLock::new();
    S.get_or_init(|| {
        let listener = TcpListener::bind("127.0.0.1:0").expect("stub binds");
        let stub: &'static Stub = Box::leak(Box::new(Stub {
            addr: listener.local_addr().unwrap(),
            requests: Mutex::new(Vec::new()),
        }));
        std::thread::spawn(move || {
            for socket in listener.incoming().flatten() {
                std::thread::spawn(move || handle(stub, socket));
            }
        });
        stub
    })
}

fn handle(stub: &Stub, mut socket: TcpStream) {
    let _ = socket.set_read_timeout(Some(Duration::from_secs(10)));
    let Some((path, headers, body)) = read_request(&mut socket) else {
        return;
    };
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    stub.requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push((path.clone(), headers.clone(), parsed.clone()));
    if headers.get("x-api-key").and_then(Value::as_str) != Some(API_KEY) {
        respond(
            &mut socket,
            "401 Unauthorized",
            r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#,
        );
        return;
    }
    let (route, _) = path
        .split_once("/v1/messages")
        .unwrap_or((path.as_str(), ""));
    match route {
        // Never answers, for the cancellation pin; the read timeout
        // above ends it after the test has moved on.
        "/stall" => {
            let mut sink = [0u8; 1];
            let _ = socket.read(&mut sink);
        }
        // The first request on this route fails part-way through a
        // round the vendor marks retryable; the rest go normally.
        "/flaky"
            if stub
                .requests()
                .iter()
                .filter(|(p, _, _)| p.starts_with("/flaky/"))
                .count()
                == 1 =>
        {
            stream(&mut socket, OVERLOADED_MIDSTREAM_SSE);
        }
        // The model speaks, asks for a tool, and ends in a refusal.
        "/refusal" => stream(&mut socket, REFUSAL_SSE),
        _ => match tool_result_in(&parsed) {
            Some((content, is_error)) => {
                let text = if is_error {
                    format!("The read failed: {content}")
                } else {
                    format!("It says: {content}")
                };
                stream(&mut socket, &closing_sse(&text));
            }
            None => stream(&mut socket, OPENING_SSE),
        },
    }
}

/// The first tool result in a request's last message, if it is a
/// follow-up turn: `(content, is_error)`.
fn tool_result_in(body: &Value) -> Option<(String, bool)> {
    let last = body.get("messages")?.as_array()?.last()?;
    let block = last.get("content")?.as_array()?.first()?;
    if block.get("type")?.as_str()? != "tool_result" {
        return None;
    }
    Some((
        block.get("content")?.as_str()?.to_string(),
        block
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    ))
}

/// The thinking block the opening turn produces before its tool call
/// — what the follow-up must carry back verbatim.
pub fn thinking_block() -> Value {
    json!({"type": "thinking", "thinking": "Read first.", "signature": "sig-01"})
}

const OPENING_SSE: &str = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"model":"claude-fixture","stop_reason":null,"usage":{"input_tokens":12,"output_tokens":1}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Read first."}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-01"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Let me "}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"read it."}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: content_block_start
data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_01","name":"read","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"path\": \"hello.txt\"}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":2}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":17}}

event: message_stop
data: {"type":"message_stop"}

"#;

/// A round that starts speaking and is then cut off by an overload —
/// text the loop will have shown before it retries.
const OVERLOADED_MIDSTREAM_SSE: &str = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_0","type":"message","role":"assistant","content":[],"model":"claude-fixture","stop_reason":null,"usage":{"input_tokens":1,"output_tokens":1}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"so far"}}

event: error
data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}

event: message_stop
data: {"type":"message_stop"}

"#;

/// A round with text and a tool call that the model then ends in a
/// refusal: the loop stores it with the call answered as not run and
/// never dispatches it, so the frontend hears `ToolFailed` with no
/// `ToolCall` before it.
const REFUSAL_SSE: &str = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_r","type":"message","role":"assistant","content":[],"model":"claude-fixture","stop_reason":null,"usage":{"input_tokens":12,"output_tokens":1}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"I would rather not."}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_r1","name":"read","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\": \"hello.txt\"}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"refusal","stop_sequence":null},"usage":{"output_tokens":9}}

event: message_stop
data: {"type":"message_stop"}

"#;

fn closing_sse(text: &str) -> String {
    let text = json!(text);
    format!(
        concat!(
            "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_2\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-fixture\",\"stop_reason\":null,\"usage\":{{\"input_tokens\":40,\"output_tokens\":1}}}}}}\n\n",
            "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n",
            "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":{text}}}}}\n\n",
            "event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n",
            "event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\",\"stop_sequence\":null}},\"usage\":{{\"output_tokens\":9}}}}\n\n",
            "event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n",
        ),
        text = text
    )
}

/// The head, then a Content-Length body. `None` drops the connection.
fn read_request(socket: &mut TcpStream) -> Option<(String, Value, Vec<u8>)> {
    let mut raw = Vec::new();
    let mut buf = [0u8; 1024];
    let header_end = loop {
        if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        let n = socket.read(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        raw.extend_from_slice(&buf[..n]);
    };
    let head = String::from_utf8_lossy(&raw[..header_end]).into_owned();
    let path = head.split_whitespace().nth(1)?.to_string();
    let mut headers = serde_json::Map::new();
    for line in head.lines().skip(1) {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(
                name.trim().to_ascii_lowercase(),
                Value::String(value.trim().to_string()),
            );
        }
    }
    let content_length: usize = headers
        .get("content-length")
        .and_then(Value::as_str)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = raw[header_end..].to_vec();
    while body.len() < content_length {
        let n = socket.read(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        body.extend_from_slice(&buf[..n]);
    }
    body.truncate(content_length);
    Some((path, Value::Object(headers), body))
}

fn respond(socket: &mut TcpStream, status: &str, body: &str) {
    let _ = write!(
        socket,
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    let _ = socket.write_all(body.as_bytes());
}

fn stream(socket: &mut TcpStream, sse: &str) {
    let _ = socket.write_all(
        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
    );
    let _ = socket.write_all(sse.as_bytes());
    let _ = socket.flush();
}
