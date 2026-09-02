//! Scaffolding shared by the integration suites.
//!
//! One copy of the pieces every suite otherwise re-grows and lets
//! drift: the permissive test operator, the contract-schema validator,
//! the stub HTTP plumbing, and the NDJSON stream drain. `host_steps.rs`
//! still carries its own richer echo server (method/header echo plus
//! the hang/dribble/endless timing routes M1's pins depend on); folding
//! it in is worthwhile but belongs to its own change, not a fix round.

// Each test binary compiles this module afresh and uses its own subset,
// so per-binary dead-code analysis would flag whatever that binary
// happens not to touch.
#![allow(dead_code)]

use std::collections::HashMap;
use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex, OnceLock};

use boon::{Compiler, SchemaIndex, Schemas};
use gwead::kernel::streams::{STREAM_EOF, StreamRegistry, read_async_shared};
use gwead::serde_json::Value;
use gwennol_core::{ApprovalRequest, Decision, Event, Operator, Turn, spi};

/// Allows everything, knows no secrets: contract and substrate dispatch
/// need no policy.
pub struct Permissive;

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

// ------------------------------------------------- the cancel harness

/// An approval the test holds open: `arrived` fires when the operator
/// is asked, `release` lets the answer through — and in the
/// cancellation pins it never does, because the point is what happens
/// to a prompt nobody answers.
pub struct Gate {
    pub arrived: tokio::sync::Notify,
    pub release: tokio::sync::Semaphore,
}

impl Default for Gate {
    fn default() -> Self {
        Self {
            arrived: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
        }
    }
}

impl Gate {
    /// The operator's side: announce the prompt, then wait to be let
    /// through. Cancel-safe — a dropped wait takes no permit.
    pub async fn hold(&self) {
        self.arrived.notify_one();
        let permit = self.release.acquire().await.expect("gate never closes");
        permit.forget();
    }
}

/// One gate per plugin name, created on first use by either side.
#[derive(Default)]
pub struct Gates(Mutex<HashMap<String, Arc<Gate>>>);

impl Gates {
    pub fn gate(&self, plugin: &str) -> Arc<Gate> {
        self.0
            .lock()
            .unwrap()
            .entry(plugin.to_string())
            .or_default()
            .clone()
    }
}

// ------------------------------------------------ contract validation

/// The contract schemas, compiled for instance validation. boon compiles
/// JSON-pointer fragments of a registered document directly, and
/// `#/$defs/…` references resolve against the document root, so each
/// subschema is addressed in place — no synthetic copies.
pub struct Contracts {
    pub schemas: Schemas,
    pub chat_input: SchemaIndex,
    pub chat_output: SchemaIndex,
    pub stream_event: SchemaIndex,
    pub call_output: SchemaIndex,
}

pub fn contracts() -> &'static Contracts {
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
pub fn assert_conforms(schema: SchemaIndex, instance: &Value) {
    if let Err(e) = contracts().schemas.validate(instance, schema) {
        panic!("payload violates the contract schema: {e:#}\npayload: {instance}");
    }
}

// ------------------------------------------------- stub HTTP plumbing

/// Run a stub server's accept loop on its own thread, one handler
/// thread per connection. A failed `accept` (possible under parallel CI
/// load) skips that connection with a stderr note rather than killing
/// the loop — but bails after 16 consecutive failures so a persistent
/// fault surfaces as connection-refused in the tests instead of a
/// silent spin.
pub fn serve<F>(listener: TcpListener, handler: F)
where
    F: Fn(TcpStream) + Send + Sync + 'static,
{
    let handler = Arc::new(handler);
    std::thread::spawn(move || {
        let mut consecutive_failures = 0u32;
        loop {
            match listener.accept() {
                Ok((socket, _)) => {
                    consecutive_failures = 0;
                    let handler = Arc::clone(&handler);
                    std::thread::spawn(move || handler(socket));
                }
                Err(e) => {
                    consecutive_failures += 1;
                    eprintln!("stub server: accept failed ({e}), {consecutive_failures} in a row");
                    if consecutive_failures >= 16 {
                        eprintln!("stub server: persistent accept failure, giving up");
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            }
        }
    });
}

/// Minimal HTTP/1.1 request reader: the head, then a Content-Length
/// body (absent means empty — a GET). Returns the request path and
/// body; `None` drops the connection, which reaches the kernel-side
/// HTTP client as a loud transport error — the error signal, not a
/// swallow. Callers that fear a wedged client should set a read
/// timeout on the socket first.
pub fn read_http_request(socket: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    let (path, _, body) = read_http_request_with_headers(socket)?;
    Some((path, body))
}

/// [`read_http_request`], also returning the headers as a JSON object
/// keyed by lowercased name — for a stub that pins what a plugin sent.
pub fn read_http_request_with_headers(socket: &mut TcpStream) -> Option<(String, Value, Vec<u8>)> {
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
    let mut headers = gwead::serde_json::Map::new();
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

// --------------------------------------------------- stream draining

/// Take a `chat` action's streamed output, validate it against the
/// contract's output schema, drain the handle from `streams`, and
/// return the NDJSON events — each validated against the contract's
/// `streamEventShape`. The kernel deliberately validates no payloads
/// at dispatch, so this drain is where streamed conformance is
/// enforced for every suite that uses it.
pub async fn drain_stream_events(streams: &Arc<Mutex<StreamRegistry>>, out: &Value) -> Vec<Value> {
    assert_conforms(contracts().chat_output, out);
    let handle = out["stream"]
        .as_u64()
        .unwrap_or_else(|| panic!("chat output is the streamed form: {out}"));
    let id = NonZeroU32::new(u32::try_from(handle).unwrap()).unwrap();
    let mut collected = Vec::new();
    let mut buf = [0u8; 7]; // small, so the body takes many reads
    loop {
        let n = read_async_shared(streams, id, &mut buf).await;
        if n == STREAM_EOF {
            break;
        }
        assert!(n > 0, "stream read returned {n}");
        collected.extend_from_slice(&buf[..n as usize]);
    }
    String::from_utf8(collected)
        .expect("NDJSON is UTF-8")
        .lines()
        .map(|l| {
            let event: Value = gwead::serde_json::from_str(l).expect("one JSON document per line");
            assert_conforms(contracts().stream_event, &event);
            event
        })
        .collect()
}
