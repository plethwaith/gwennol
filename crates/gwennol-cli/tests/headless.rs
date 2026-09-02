//! Milestone 6, end to end: the built `gwennol` binary runs a task
//! headlessly against the bundled plugins and a stub Messages API, with
//! every approval decided by a rule and traced to it, and no prompt
//! anywhere.
//!
//! The roadmap's done-when, each pinned here: a real task — read a
//! file, answer from it — runs with no interaction; each decision's
//! trace names the flag or file rule that made it, or says none did;
//! a request no rule matches is denied and the model is told, not the
//! user re-prompted; secrets arrive by the documented sources and never
//! from nowhere; Ctrl-C cancels the turn and the process says so; and
//! a usage error is a usage error. The stub answers the opening turn
//! with a thinking block before its tool call, so the transcript pins
//! that the thinking is replayed whole on the follow-up — the shape
//! the first live smoke test must reproduce.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use provider_anthropic::PLUGIN_NAME as PROVIDER;
use serde_json::{Value, json};

/// The key the stub accepts; anything else is a 401.
const API_KEY: &str = "sk-ant-headless";
/// The convention variable for the provider's key.
const KEY_VAR: &str = "GWENNOL_SECRET_PROVIDER_ANTHROPIC_API_KEY";

// ------------------------------------------------------------- fixture

struct Fixture {
    /// Bundled manifests, provider egress widened to the stub.
    plugins: PathBuf,
    /// A workspace with `hello.txt` in it, canonical.
    workspace: PathBuf,
    /// Where per-run config files go, and `$XDG_CONFIG_HOME`.
    scratch: PathBuf,
    stub: &'static Stub,
}

fn fixture() -> &'static Fixture {
    static F: OnceLock<Fixture> = OnceLock::new();
    F.get_or_init(|| {
        let root = tempfile::tempdir().unwrap().keep().canonicalize().unwrap();
        let workspace = root.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("hello.txt"), "hello from the workspace\n").unwrap();
        let scratch = root.join("scratch");
        std::fs::create_dir(&scratch).unwrap();

        let mut bundled = xtask::bundle(&xtask::workspace_root())
            .unwrap_or_else(|e| panic!("bundling plugins/ failed: {e}"));
        for plugin in &mut bundled {
            if plugin.name() == PROVIDER {
                plugin.manifest["permissions"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!("network:egress:127.0.0.1"));
            }
        }
        let bundle_root = root.join("bundle");
        xtask::write_bundle(&bundled, &bundle_root).unwrap();
        Fixture {
            plugins: bundle_root.join(xtask::PLUGINS_DIR),
            workspace,
            scratch,
            stub: stub(),
        }
    })
}

impl Fixture {
    /// A config file pointing the provider at a stub route.
    fn config(&self, name: &str, route: &str, extra: &str) -> PathBuf {
        let path = self.scratch.join(format!("{name}.toml"));
        std::fs::write(
            &path,
            format!(
                "[plugins]\ndir = {plugins:?}\ntrust_runtimes = [{provider:?}]\n\n\
                 [plugin_config.{provider}]\nmodel = \"claude-fixture\"\nbase_url = \"http://{addr}{route}\"\n\n{extra}",
                plugins = self.plugins.display().to_string(),
                provider = PROVIDER,
                addr = self.stub.addr,
            ),
        )
        .unwrap();
        path
    }

    /// The rule that lets the provider reach the stub.
    fn allow_stub(&self) -> String {
        format!("http:POST http://{}/*", self.stub.addr)
    }

    /// The binary, in the workspace, with an empty environment apart
    /// from what a process needs and what the test sets.
    fn gwennol(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_gwennol"));
        cmd.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            // No user config can leak in: the default location is
            // under here, and nothing is there.
            .env("XDG_CONFIG_HOME", &self.scratch)
            .current_dir(&self.workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    }
}

struct Run {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

fn run(cmd: &mut Command) -> Run {
    let out = cmd.output().expect("gwennol runs");
    let run = Run {
        status: out.status,
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    };
    eprintln!(
        "--- stdout ---\n{}--- stderr ---\n{}---",
        run.stdout, run.stderr
    );
    run
}

impl Run {
    fn stderr_has(&self, needle: &str) {
        assert!(
            self.stderr.contains(needle),
            "stderr lacks {needle:?}:\n{}",
            self.stderr
        );
    }
}

// ---------------------------------------------------------------- stub

/// A Messages API stand-in: the opening turn thinks, says something,
/// and asks to `read` hello.txt; the follow-up quotes the tool result
/// it was given, saying whether it was an error. Records every
/// request.
struct Stub {
    addr: std::net::SocketAddr,
    /// `(path, headers, body)` in arrival order.
    requests: Mutex<Vec<(String, Value, Value)>>,
}

impl Stub {
    /// Everything recorded so far. Poison-proof: a test that panics
    /// while holding the lock must not take every later test's stub
    /// down with it.
    fn requests(&self) -> Vec<(String, Value, Value)> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

fn stub() -> &'static Stub {
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
fn thinking_block() -> Value {
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

// --------------------------------------------------------------- pins

#[test]
fn a_task_runs_headlessly_with_every_decision_traced() {
    let f = fixture();
    let config = f.config("traced", "/traced", "");
    let transcript = f.scratch.join("traced.transcript.json");
    let r = run(f
        .gwennol()
        .env(KEY_VAR, API_KEY)
        .args(["--config"])
        .arg(&config)
        .args(["--allow", &f.allow_stub(), "--allow", "read:**"])
        .arg("--transcript")
        .arg(&transcript)
        .arg("What does hello.txt say?"));
    assert!(r.status.success(), "{:?}", r.status);

    // Model text and nothing else on stdout: the opening text, its
    // line ended when the tool call interrupted it, then the answer.
    assert_eq!(
        r.stdout,
        "Let me read it.\nIt says: hello from the workspace\n"
    );

    // Every decision, with the rule that made it.
    r.stderr_has(&format!(
        "gwennol: POST http://{}/traced/v1/messages from {PROVIDER}: allowed by --allow \"http:POST http://{}/*\"",
        f.stub.addr, f.stub.addr
    ));
    r.stderr_has(&format!(
        "gwennol: read {} from tool-read (call read toolu_01): allowed by --allow \"read:**\"",
        f.workspace.join("hello.txt").display()
    ));
    // The call and its result, as the model saw them.
    r.stderr_has("gwennol: -> read toolu_01: {\"path\":\"hello.txt\"}");
    r.stderr_has("gwennol: <- read toolu_01: ok, 25 bytes");
    r.stderr_has("gwennol: done (EndTurn): 2 rounds, 52 tokens in, 26 out");
    // No prompt exists: nothing on stderr asks anything.
    assert!(!r.stderr.contains('?'), "{}", r.stderr);

    // The secret reached the vendor by the convention variable and by
    // no other route: the key is not in the trace.
    let requests = f.stub.requests();
    let mine: Vec<_> = requests
        .iter()
        .filter(|(path, _, _)| path.starts_with("/traced/"))
        .collect();
    assert_eq!(mine.len(), 2);
    for (_, headers, _) in &mine {
        assert_eq!(headers["x-api-key"], API_KEY);
    }
    assert!(!r.stderr.contains(API_KEY));

    // The follow-up replayed the opening message whole — thinking
    // block first, verbatim — then answered the call.
    let follow_up = &mine[1].2;
    let messages = follow_up["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["content"][0], thinking_block());
    assert_eq!(messages[1]["content"][2]["type"], "tool_use");
    assert_eq!(messages[2]["content"][0]["tool_use_id"], "toolu_01");
    assert_eq!(messages[2]["content"][0]["is_error"], false);
    // The system prompt names the workspace, so the model knows where
    // relative paths go.
    assert!(
        follow_up["system"]
            .as_str()
            .unwrap()
            .contains(&f.workspace.display().to_string())
    );

    // The transcript file is the conversation as the provider saw it,
    // with the thinking carried as the contract's opaque block.
    let saved: Vec<Value> =
        serde_json::from_str(&std::fs::read_to_string(&transcript).unwrap()).unwrap();
    assert_eq!(saved.len(), 4);
    assert_eq!(saved[1]["content"][0]["type"], "opaque");
    assert_eq!(saved[1]["content"][0]["data"], thinking_block());
    assert_eq!(saved[3]["role"], "assistant");
}

#[test]
fn a_retried_round_is_not_written_twice() {
    let f = fixture();
    let config = f.config("flaky", "/flaky", "");
    let r = run(f
        .gwennol()
        .env(KEY_VAR, API_KEY)
        .arg("--config")
        .arg(&config)
        .args(["--allow", &f.allow_stub(), "--allow", "read:**"])
        .arg("What does hello.txt say?"));
    assert!(r.status.success(), "{:?}", r.status);
    r.stderr_has("gwennol: provider failure, retrying (2/3): ");
    // The failed round's text was shown to the operator, then the
    // round was retried from the start: stdout carries the accepted
    // rounds only, and nothing twice.
    assert_eq!(
        r.stdout,
        "Let me read it.\nIt says: hello from the workspace\n"
    );
    assert!(!r.stdout.contains("so far"), "{}", r.stdout);
}

#[test]
fn a_request_no_rule_matches_is_denied_and_the_model_is_told() {
    let f = fixture();
    let config = f.config("denied", "", "");
    let r = run(f
        .gwennol()
        .env(KEY_VAR, API_KEY)
        .arg("--config")
        .arg(&config)
        .args(["--allow", &f.allow_stub()])
        .arg("What does hello.txt say?"));
    // The turn completed: a denial is routed around, not fatal.
    assert!(r.status.success(), "{:?}", r.status);
    r.stderr_has(&format!(
        "gwennol: read {} from tool-read (call read toolu_01): denied: no rule matched",
        f.workspace.join("hello.txt").display()
    ));
    r.stderr_has("gwennol: !! read toolu_01: ");
    r.stderr_has("operator denied read of ");
    // The model was told and answered accordingly.
    assert!(
        r.stdout.contains("The read failed: ") && r.stdout.contains("operator denied read of"),
        "{}",
        r.stdout
    );
}

#[test]
fn rules_are_tried_in_order_and_file_rules_name_their_file() {
    let f = fixture();
    // A deny in the policy file comes before the config's allow.
    let policy = f.scratch.join("policy.toml");
    std::fs::write(&policy, "[[rules]]\ndeny = \"read:hello.txt\"\n").unwrap();
    let config = f.config(
        "ordered",
        "",
        &format!(
            "[[rules]]\nallow = {:?}\nplugin = {PROVIDER:?}\n\n[[rules]]\nallow = \"read:**\"\n",
            f.allow_stub()
        ),
    );
    let r = run(f
        .gwennol()
        .env(KEY_VAR, API_KEY)
        .arg("--config")
        .arg(&config)
        .arg("--policy")
        .arg(&policy)
        .arg("What does hello.txt say?"));
    assert!(r.status.success(), "{:?}", r.status);
    r.stderr_has(&format!(
        "denied by deny \"read:hello.txt\" ({} rule 1)",
        policy.display()
    ));
    r.stderr_has(&format!(
        "allowed by allow {:?} for plugin {PROVIDER} ({} rule 1)",
        f.allow_stub(),
        config.display()
    ));

    // The same deny as a flag, after an allow flag: the allow wins,
    // because flags are tried in the order given.
    let r = run(f
        .gwennol()
        .env(KEY_VAR, API_KEY)
        .arg("--config")
        .arg(&config)
        .args(["--allow", "read:**", "--deny", "read:hello.txt"])
        .arg("What does hello.txt say?"));
    assert!(r.status.success(), "{:?}", r.status);
    r.stderr_has("allowed by --allow \"read:**\"");
}

#[test]
fn secrets_come_from_a_named_source_and_a_missing_one_is_said_so() {
    let f = fixture();
    let config = f.config("secrets", "", "");

    // --secret names another variable; the convention one is unset.
    let r = run(f
        .gwennol()
        .env("MY_KEY", API_KEY)
        .arg("--config")
        .arg(&config)
        .args([
            "--secret",
            &format!("{PROVIDER}:api_key=env:MY_KEY"),
            "--allow",
            &f.allow_stub(),
            "--allow",
            "read:**",
        ])
        .arg("What does hello.txt say?"));
    assert!(r.status.success(), "{:?}", r.status);

    // A file source, from the config.
    let key_file = f.scratch.join("anthropic.key");
    std::fs::write(&key_file, format!("{API_KEY}\n")).unwrap();
    let with_file = f.config(
        "secrets-file",
        "",
        &format!(
            "[[secrets]]\nplugin = {PROVIDER:?}\nname = \"api_key\"\nfile = \"anthropic.key\"\n"
        ),
    );
    let r = run(f
        .gwennol()
        .arg("--config")
        .arg(&with_file)
        .args(["--allow", &f.allow_stub(), "--allow", "read:**"])
        .arg("What does hello.txt say?"));
    assert!(r.status.success(), "{:?}", r.status);

    // No source at all: warned at startup, and the vendor's refusal
    // ends the turn — the key was never invented.
    let r = run(f
        .gwennol()
        .arg("--config")
        .arg(&config)
        .args(["--allow", &f.allow_stub(), "--allow", "read:**"])
        .arg("What does hello.txt say?"));
    assert_eq!(r.status.code(), Some(1), "{:?}", r.status);
    r.stderr_has(&format!(
        "plugin {PROVIDER} declares secret \"api_key\" but no source has it: set environment variable {KEY_VAR}"
    ));
    r.stderr_has("gwennol: turn failed: provider refused the turn");
}

#[test]
fn plugins_and_trust_come_from_flags_too() {
    let f = fixture();
    // No config file at all: everything by flag.
    let r = run(f
        .gwennol()
        .env(KEY_VAR, API_KEY)
        .arg("--plugins")
        .arg(&f.plugins)
        .args(["--trust-runtime", PROVIDER, "--model", "claude-fixture"])
        .args(["--allow", &f.allow_stub()])
        .arg("Say hi."));
    // The provider's default base_url is the real API, which the stub
    // rule does not cover: denied, so the provider step fails — but
    // the startup path (plugins, trust, --model) all held.
    assert_eq!(r.status.code(), Some(1), "{:?}", r.status);
    r.stderr_has(
        "gwennol: POST https://api.anthropic.com/v1/messages from provider-anthropic: denied: no rule matched",
    );
    r.stderr_has("gwennol: turn failed: the stream ended before the turn did");

    // Without trust, the provider cannot register; the error names the
    // file.
    let r = run(f.gwennol().arg("--plugins").arg(&f.plugins).arg("Say hi."));
    assert_eq!(r.status.code(), Some(2), "{:?}", r.status);
    r.stderr_has("anthropic.json: ");
}

#[cfg(unix)]
#[test]
fn ctrl_c_cancels_the_turn() {
    use std::io::BufRead;

    let f = fixture();
    let config = f.config("stall", "/stall", "");
    let mut child = f
        .gwennol()
        .env(KEY_VAR, API_KEY)
        .arg("--config")
        .arg(&config)
        .args(["--allow", &f.allow_stub()])
        .arg("Say hi.")
        .spawn()
        .unwrap();
    // Wait for the request to be approved — the turn is now parked on
    // the stub — then interrupt.
    let stderr = child.stderr.take().unwrap();
    let mut lines = std::io::BufReader::new(stderr).lines();
    let mut seen = String::new();
    loop {
        let line = lines
            .next()
            .expect("stderr ends before the approval")
            .unwrap();
        seen.push_str(&line);
        seen.push('\n');
        if line.contains("/stall/v1/messages from") {
            break;
        }
    }
    let killed = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let rest: Vec<String> = lines.map_while(Result::ok).collect();
    let status = child.wait().unwrap();
    let stderr = format!("{seen}{}", rest.join("\n"));
    eprintln!("--- stderr ---\n{stderr}\n---");
    assert_eq!(status.code(), Some(130), "{status:?}");
    assert!(stderr.contains("gwennol: interrupted; cancelling the turn"));
    assert!(stderr.contains("gwennol: cancelled"));
}

#[test]
fn usage_errors_are_usage_errors() {
    let f = fixture();
    // A malformed rule, before anything is loaded.
    let r = run(f.gwennol().args(["--allow", "open:**"]).arg("Say hi."));
    assert_eq!(r.status.code(), Some(2), "{:?}", r.status);
    r.stderr_has("gwennol: rule \"open:**\": unknown kind \"open\"");

    // A config file that does not parse names itself.
    let bad = f.scratch.join("bad.toml");
    std::fs::write(&bad, "[sesion]\n").unwrap();
    let r = run(f.gwennol().arg("--config").arg(&bad).arg("Say hi."));
    assert_eq!(r.status.code(), Some(2), "{:?}", r.status);
    r.stderr_has(&format!("gwennol: {}: ", bad.display()));

    // A plugins directory that is not there names itself.
    let r = run(f
        .gwennol()
        .args(["--plugins", "/nonexistent/plugins"])
        .arg("Say hi."));
    assert_eq!(r.status.code(), Some(2), "{:?}", r.status);
    r.stderr_has("gwennol: /nonexistent/plugins: ");

    // An empty task.
    let r = run(f.gwennol().arg("--plugins").arg(&f.plugins).arg("  "));
    assert_eq!(r.status.code(), Some(2), "{:?}", r.status);
    r.stderr_has("gwennol: the task is empty");
}

/// A test that needs no bundle: `--help` documents the rule grammar
/// and the exit statuses a script relies on.
#[test]
fn help_documents_the_policy_surface() {
    let out = Command::new(env!("CARGO_BIN_EXE_gwennol"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(out.status.success());
    let help = String::from_utf8_lossy(&out.stdout);
    for needle in [
        "--allow",
        "--deny",
        "--policy",
        "--secret",
        "--plugins",
        "read, write",
    ] {
        assert!(help.contains(needle), "help lacks {needle:?}:\n{help}");
    }
}
