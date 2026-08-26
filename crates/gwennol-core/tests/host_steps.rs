//! End-to-end: a plugin action runs a `host_*` step through a real Gwead
//! kernel, and both gates — manifest (kernel-enforced) and operator
//! (host-asked) — behave as documented.
//!
//! The host is a process singleton, so this binary boots one kernel with
//! every fixture plugin registered up front and shares it across tests.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use gwead::kernel::streams::{STREAM_EOF, STREAM_IO_ERROR, StreamRegistry, read_async_shared};
use gwead::kernel::{Kernel, KernelError};
use gwead::serde_json::{Value, json};
use gwennol_core::{Access, ApprovalRequest, Decision, Event, Operator, ToolCall, Turn};

/// Records every approval request; denies anything the `denied` plugin
/// asks for; answers `token` for the `secretive` plugin.
#[derive(Default)]
struct Recorder {
    requests: Mutex<Vec<ApprovalRequest>>,
}

#[async_trait::async_trait]
impl Operator for Recorder {
    async fn approve(&self, request: ApprovalRequest) -> Decision {
        let deny = request.plugin == "denied";
        self.requests.lock().unwrap().push(request);
        if deny {
            Decision::Deny
        } else {
            Decision::Allow
        }
    }
    async fn secret(&self, plugin: &str, name: &str) -> Option<String> {
        (plugin == "secretive" && name == "token").then(|| "s3cr3t".to_string())
    }
    fn emit(&self, _: Event) {}
    async fn input(&self) -> Option<Turn> {
        None
    }
}

struct Fixture {
    kernel: Arc<Kernel>,
    operator: Arc<Recorder>,
    workspace: PathBuf,
    /// `http://127.0.0.1:<port>` of the echo server.
    echo: String,
    /// A second echo server: same host, different port, so a redirect
    /// between them crosses an origin without leaving the egress grant.
    echo2: String,
}

impl Fixture {
    fn requests_for(&self, plugin: &str) -> Vec<Access> {
        self.operator
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.plugin == plugin)
            .map(|r| r.access.clone())
            .collect()
    }

    /// What the operator was told each request for `plugin` was for.
    fn causes_for(&self, plugin: &str) -> Vec<Option<ToolCall>> {
        self.operator
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.plugin == plugin)
            .map(|r| r.cause.clone())
            .collect()
    }

    /// Every URL the operator was shown for `plugin`.
    fn urls_for(&self, plugin: &str) -> Vec<String> {
        self.requests_for(plugin)
            .into_iter()
            .filter_map(|a| match a {
                Access::Http { url, .. } => Some(url),
                _ => None,
            })
            .collect()
    }
}

fn fixture() -> &'static Fixture {
    static F: OnceLock<Fixture> = OnceLock::new();
    F.get_or_init(|| {
        // Canonicalised so the canonical paths in approvals compare equal
        // to workspace joins (macOS tempdirs sit behind the /var symlink).
        let workspace = tempfile::tempdir().unwrap().keep().canonicalize().unwrap();
        let operator = Arc::new(Recorder::default());
        let mut kernel = gwennol_core::boot(operator.clone(), workspace.clone()).unwrap();
        for m in fixture_plugins() {
            kernel.register_plugin_from_json(&m.to_string()).unwrap();
        }
        Fixture {
            kernel: kernel.into_arc(),
            operator,
            workspace,
            echo: spawn_echo_server(),
            echo2: spawn_echo_server(),
        }
    })
}

fn plugin(name: &str, permissions: &[&str], steps: Value) -> Value {
    json!({
        "name": name, "version": "0.0.0", "description": "test fixture",
        "permissions": permissions,
        "actions": {"go": {"steps": steps}}
    })
}

fn fixture_plugins() -> Vec<Value> {
    vec![
        plugin(
            "reader",
            &["step_type:host_fs.read"],
            json!([{"id": "r", "type": "host_fs.read", "params": {"path": "{{$input.path}}", "max_bytes": "{{$input.max_bytes}}"}}]),
        ),
        plugin(
            "writer",
            &["step_type:host_fs.write", "step_type:host_fs.list"],
            json!([
                {"id": "w", "type": "host_fs.write", "params": {"path": "{{$input.path}}", "content": "{{$input.content}}", "create_dirs": true}},
                {"id": "l", "type": "host_fs.list", "params": {"path": "{{$input.dir}}"}}
            ]),
        ),
        plugin(
            "lister",
            &["step_type:host_fs.list"],
            json!([{"id": "l", "type": "host_fs.list", "params": {"path": "{{$input.dir}}", "max_entries": "{{$input.max}}"}}]),
        ),
        plugin(
            "runner",
            &["step_type:host_process.run"],
            json!([{"id": "p", "type": "host_process.run", "params": {"argv": "{{$input.argv}}", "stdin": "{{$input.stdin}}", "timeout_ms": "{{$input.timeout_ms}}", "max_output_bytes": "{{$input.max_output_bytes}}"}}]),
        ),
        plugin(
            "fetcher",
            &["step_type:host_http.post", "network:egress:127.0.0.1"],
            json!([{"id": "h", "type": "host_http.post", "params": {"url": "{{$input.url}}", "body": "{{$input.body}}", "stream": "{{$input.stream}}", "max_bytes": "{{$input.max_bytes}}"}}]),
        ),
        plugin(
            "getter",
            &["step_type:host_http.get", "network:egress:127.0.0.1"],
            json!([{"id": "h", "type": "host_http.get", "params": {"url": "{{$input.url}}", "stream": "{{$input.stream}}"}}]),
        ),
        plugin(
            "getbody",
            &["step_type:host_http.get", "network:egress:127.0.0.1"],
            json!([{"id": "h", "type": "host_http.get", "params": {"url": "{{$input.url}}", "body": "nope"}}]),
        ),
        plugin(
            "tuned",
            &["step_type:host_http.get", "network:egress:127.0.0.1"],
            json!([{"id": "h", "type": "host_http.get", "params": {
                "url": "{{$input.url}}", "stream": "{{$input.stream}}",
                "timeout_ms": "{{$input.timeout_ms}}", "idle_timeout_ms": "{{$input.idle_timeout_ms}}",
                "max_redirects": "{{$input.max_redirects}}"}}]),
        ),
        plugin(
            "nofetch",
            &["step_type:host_http.get"],
            json!([{"id": "h", "type": "host_http.get", "params": {"url": "{{$input.url}}"}}]),
        ),
        plugin(
            "delegator",
            &["invoke:plugin:reader"],
            json!([{"id": "i", "type": "invoke", "params": {"plugin": "reader", "action": "go", "input": {"path": "{{$input.path}}", "max_bytes": 1024}}}]),
        ),
        plugin(
            "ungranted",
            &[],
            json!([{"id": "r", "type": "host_fs.read", "params": {"path": "{{$input.path}}"}}]),
        ),
        plugin(
            "denied",
            &["step_type:host_fs.read"],
            json!([{"id": "r", "type": "host_fs.read", "params": {"path": "{{$input.path}}"}}]),
        ),
        {
            let mut p = plugin(
                "secretive",
                &["step_type:host_http.post", "network:egress:127.0.0.1"],
                json!([{"id": "h", "type": "host_http.post", "params": {"url": "{{$input.url}}", "headers": {"authorization": "Bearer {{$secrets.token}}", "x-api-key": "{{$secrets.token}}"}}}]),
            );
            p["usesSecrets"] = json!(["token"]);
            p
        },
    ]
}

/// Minimal HTTP/1.1 server: answers every request with a JSON echo of the
/// method, path, headers and body. Streaming-friendly: the body is sent in
/// two writes with a flush between.
fn spawn_echo_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut s = stream.unwrap();
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let (head_end, mut body_len) = loop {
                    let n = s.read(&mut tmp).unwrap();
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..i]).to_string();
                        let len = head
                            .lines()
                            .find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                k.trim()
                                    .eq_ignore_ascii_case("content-length")
                                    .then(|| v.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                        break (i + 4, len);
                    }
                };
                while buf.len() < head_end + body_len {
                    let n = s.read(&mut tmp).unwrap();
                    if n == 0 {
                        body_len = buf.len() - head_end;
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let mut lines = head.lines();
                let req_line = lines.next().unwrap_or("");
                let mut parts = req_line.split_whitespace();
                let method = parts.next().unwrap_or("");
                let path = parts.next().unwrap_or("");
                let headers: gwead::serde_json::Map<String, Value> = lines
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| {
                        (
                            k.trim().to_ascii_lowercase(),
                            Value::String(v.trim().to_string()),
                        )
                    })
                    .collect();
                let (route, query) = path.split_once('?').unwrap_or((path, ""));
                let location = query.strip_prefix("to=").unwrap_or("");
                match route {
                    // `/redirect?to=<url>` sends the client onward; the
                    // 307 form is the one that must keep method and body.
                    "/redirect" | "/redirect307" | "/loop" => {
                        let (code, to) = match route {
                            "/redirect307" => ("307 Temporary Redirect", location),
                            "/loop" => ("302 Found", "/loop"),
                            _ => ("302 Found", location),
                        };
                        write!(s, "HTTP/1.1 {code}\r\nlocation: {to}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").unwrap();
                        return;
                    }
                    // Reads the request and never answers.
                    "/hang" => {
                        std::thread::sleep(std::time::Duration::from_secs(30));
                        return;
                    }
                    // Answers, sends half a body, then goes quiet.
                    "/dribble" => {
                        write!(s, "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 32\r\nconnection: close\r\n\r\n").unwrap();
                        s.write_all(b"data: half").unwrap();
                        s.flush().unwrap();
                        std::thread::sleep(std::time::Duration::from_secs(30));
                        return;
                    }
                    _ => {}
                }
                let body = String::from_utf8_lossy(&buf[head_end..head_end + body_len]).to_string();
                let echo =
                    json!({"method": method, "path": path, "headers": headers, "body": body})
                        .to_string();
                let (a, b) = echo.split_at(echo.len() / 2);
                write!(s, "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nx-echo: yes\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", echo.len()).unwrap();
                s.write_all(a.as_bytes()).unwrap();
                s.flush().unwrap();
                s.write_all(b.as_bytes()).unwrap();
            });
        }
    });
    format!("http://127.0.0.1:{port}")
}

async fn run(plugin: &str, input: Value) -> Result<Value, KernelError> {
    let f = fixture();
    f.kernel
        .execute(plugin, "go", input)
        .with_config(&json!({}))
        .run()
        .await
        .map(|r| Value::Object(r.step_results.into_iter().collect()))
}

// ---------------------------------------------------------------- fs

#[tokio::test]
async fn fs_read_asks_operator_with_absolute_path_and_returns_content() {
    let f = fixture();
    std::fs::write(f.workspace.join("hello.txt"), "hello, weaver\n").unwrap();
    let out = run("reader", json!({"path": "hello.txt", "max_bytes": 1 << 20}))
        .await
        .unwrap();
    assert_eq!(out["r"]["content"], "hello, weaver\n");
    assert_eq!(out["r"]["truncated"], false);
    assert_eq!(out["r"]["size"], 14);
    assert!(
        f.requests_for("reader")
            .contains(&Access::ReadFile(f.workspace.join("hello.txt")))
    );
}

#[tokio::test]
async fn fs_read_truncates_on_utf8_boundary() {
    let f = fixture();
    std::fs::write(f.workspace.join("utf8.txt"), "héllo").unwrap(); // 'é' is 2 bytes
    let out = run("reader", json!({"path": "utf8.txt", "max_bytes": 2}))
        .await
        .unwrap();
    assert_eq!(out["r"]["content"], "h");
    assert_eq!(out["r"]["truncated"], true);
    assert_eq!(out["r"]["size"], 6);
}

#[tokio::test]
async fn fs_read_normalises_dotdot_before_asking() {
    let f = fixture();
    std::fs::create_dir_all(f.workspace.join("sub")).unwrap();
    std::fs::write(f.workspace.join("top.txt"), "top").unwrap();
    run(
        "reader",
        json!({"path": "sub/../top.txt", "max_bytes": 100}),
    )
    .await
    .unwrap();
    let asked = f.requests_for("reader");
    assert!(
        asked.contains(&Access::ReadFile(f.workspace.join("top.txt"))),
        "{asked:?}"
    );
    assert!(
        !asked.iter().any(|a| format!("{a:?}").contains("..")),
        "operator saw an unnormalised path: {asked:?}"
    );
}

#[tokio::test]
async fn fs_write_then_list() {
    let f = fixture();
    let out = run(
        "writer",
        json!({"path": "out/nested/a.txt", "content": "abc", "dir": "out/nested"}),
    )
    .await
    .unwrap();
    assert_eq!(out["w"]["bytes_written"], 3);
    assert_eq!(
        std::fs::read_to_string(f.workspace.join("out/nested/a.txt")).unwrap(),
        "abc"
    );
    assert_eq!(
        out["l"]["entries"],
        json!([{"name": "a.txt", "kind": "file", "size": 3}])
    );
    assert_eq!(out["l"]["truncated"], false);
    let asked = f.requests_for("writer");
    assert!(asked.contains(&Access::WriteFile(f.workspace.join("out/nested/a.txt"))));
    assert!(asked.contains(&Access::ListDir(f.workspace.join("out/nested"))));
}

#[cfg(unix)]
#[tokio::test]
async fn fs_read_bounds_what_it_reads_even_on_endless_files() {
    // /dev/zero never ends; an unbounded read would never return.
    let out = run("reader", json!({"path": "/dev/zero", "max_bytes": 1024}))
        .await
        .unwrap();
    assert_eq!(out["r"]["truncated"], true);
    assert_eq!(out["r"]["content"].as_str().unwrap().len(), 1024);
}

#[cfg(unix)]
#[tokio::test]
async fn fs_read_shows_the_operator_where_a_symlink_really_leads() {
    let f = fixture();
    let outside = tempfile::tempdir().unwrap().keep().canonicalize().unwrap();
    std::fs::write(outside.join("real.txt"), "elsewhere").unwrap();
    std::os::unix::fs::symlink(outside.join("real.txt"), f.workspace.join("innocent.txt")).unwrap();
    let out = run("reader", json!({"path": "innocent.txt", "max_bytes": 1024}))
        .await
        .unwrap();
    assert_eq!(out["r"]["content"], "elsewhere");
    let asked = f.requests_for("reader");
    assert!(
        asked.contains(&Access::ReadFile(outside.join("real.txt"))),
        "the operator judged the alias, not the target: {asked:?}"
    );
    assert!(
        !asked.contains(&Access::ReadFile(f.workspace.join("innocent.txt"))),
        "the alias reached the operator: {asked:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn fs_write_refuses_a_symlink_destination() {
    let f = fixture();
    let outside = tempfile::tempdir().unwrap().keep();
    std::fs::write(outside.join("target.txt"), "untouched").unwrap();
    std::os::unix::fs::symlink(outside.join("target.txt"), f.workspace.join("sly.txt")).unwrap();
    let err = run(
        "writer",
        json!({"path": "sly.txt", "content": "overwritten", "dir": "."}),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("symlink"), "{err}");
    assert_eq!(
        std::fs::read_to_string(outside.join("target.txt")).unwrap(),
        "untouched",
        "the write escaped through the link"
    );
    assert!(
        !f.requests_for("writer")
            .contains(&Access::WriteFile(f.workspace.join("sly.txt"))),
        "the operator was asked about a write the host must refuse itself"
    );
}

#[tokio::test]
async fn fs_write_replaces_the_file_and_leaves_no_temporary_behind() {
    let f = fixture();
    for content in ["first", "second"] {
        run(
            "writer",
            json!({"path": "atomic/x.txt", "content": content, "dir": "atomic"}),
        )
        .await
        .unwrap();
    }
    assert_eq!(
        std::fs::read_to_string(f.workspace.join("atomic/x.txt")).unwrap(),
        "second"
    );
    let leftovers: Vec<_> = std::fs::read_dir(f.workspace.join("atomic"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("gwennol-tmp"))
        .collect();
    assert_eq!(leftovers, Vec::<String>::new());
}

#[tokio::test]
async fn fs_list_caps_entries_and_reports_the_cut() {
    let f = fixture();
    std::fs::create_dir_all(f.workspace.join("capped")).unwrap();
    for name in ["a", "b", "c", "d", "e"] {
        std::fs::write(f.workspace.join("capped").join(name), "x").unwrap();
    }
    let out = run("lister", json!({"dir": "capped", "max": 2}))
        .await
        .unwrap();
    assert_eq!(out["l"]["entries"].as_array().unwrap().len(), 2);
    assert_eq!(out["l"]["truncated"], true);

    let out = run("lister", json!({"dir": "capped", "max": 100}))
        .await
        .unwrap();
    let names: Vec<_> = out["l"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, ["a", "b", "c", "d", "e"]);
    assert_eq!(out["l"]["truncated"], false);
}

// ---------------------------------------------------------------- process

#[tokio::test]
async fn process_run_captures_output_and_status_without_a_shell() {
    let f = fixture();
    let out = run("runner", json!({"argv": ["sh", "-c", "cat; echo err >&2; exit 3"], "stdin": "from stdin", "timeout_ms": 10000, "max_output_bytes": 1048576})).await.unwrap();
    assert_eq!(out["p"]["status"], 3);
    assert_eq!(out["p"]["stdout"], "from stdin");
    assert_eq!(out["p"]["stderr"], "err\n");
    let asked = f.requests_for("runner");
    assert!(
        asked.iter().any(
            |a| matches!(a, Access::Spawn { argv, cwd, .. } if argv[0] == "sh" && *cwd == f.workspace)
        ),
        "{asked:?}"
    );
    assert!(
        asked.iter().any(
            |a| matches!(a, Access::Spawn { stdin, .. } if *stdin == Some("from stdin".into()))
        ),
        "the operator was not shown the stdin payload: {asked:?}"
    );
}

#[tokio::test]
async fn process_run_survives_a_child_that_never_reads_a_large_stdin() {
    // Bigger than any pipe buffer; a blocking stdin write before the drains
    // would deadlock here and ignore the timeout entirely.
    let big = "x".repeat(1 << 20);
    let out = run(
        "runner",
        json!({"argv": ["sh", "-c", "exit 0"], "stdin": big, "timeout_ms": 10000, "max_output_bytes": 1048576}),
    )
    .await
    .unwrap();
    assert_eq!(out["p"]["status"], 0);
}

#[tokio::test]
async fn process_run_bounds_captured_output_while_draining_the_rest() {
    let out = run(
        "runner",
        json!({"argv": ["sh", "-c", "head -c 200000 /dev/zero"], "stdin": "", "timeout_ms": 10000, "max_output_bytes": 100}),
    )
    .await
    .unwrap();
    assert_eq!(
        out["p"]["status"], 0,
        "the drained child should still exit cleanly"
    );
    assert_eq!(out["p"]["stdout_truncated"], true);
    assert!(out["p"]["stdout"].as_str().unwrap().len() <= 100);
}

#[cfg(unix)]
#[tokio::test]
async fn a_timed_out_process_group_leaves_no_orphans() {
    let f = fixture();
    // The backgrounded sleep would outlive a kill that reached only the
    // direct child. The shell records its own pid, which — as group
    // leader — is also the pgid.
    let err = run(
        "runner",
        json!({"argv": ["sh", "-c", "echo $$ > pg.pid; sleep 30 & sleep 30"], "stdin": "", "timeout_ms": 500, "max_output_bytes": 1048576}),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("exceeded timeout"), "{err}");
    let pgid: u32 = std::fs::read_to_string(f.workspace.join("pg.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // Poll: a freshly-reparented zombie still answers kill -0 until init
    // reaps it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let alive = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("kill -0 -- -{pgid} 2>/dev/null"))
            .status()
            .unwrap()
            .success();
        if !alive {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "process group {pgid} is still alive after the timeout"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[tokio::test]
async fn process_run_child_sees_only_the_allow_listed_environment() {
    assert!(
        std::env::var_os("CARGO_MANIFEST_DIR").is_some(),
        "this test needs a variable the harness sets and the allow-list omits"
    );
    let out = run(
        "runner",
        json!({"argv": ["sh", "-c", r#"printf '%s|%s' "${PATH:+set}" "${CARGO_MANIFEST_DIR:+leaked}""#], "stdin": "", "timeout_ms": 10000, "max_output_bytes": 1048576}),
    )
    .await
    .unwrap();
    assert_eq!(
        out["p"]["stdout"], "set|",
        "the child should keep PATH and lose everything not allow-listed"
    );
}

#[tokio::test]
async fn process_run_times_out() {
    let err = run(
        "runner",
        json!({"argv": ["sleep", "5"], "stdin": "", "timeout_ms": 100, "max_output_bytes": 1048576}),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("exceeded timeout"), "{err}");
}

// ---------------------------------------------------------------- http

#[tokio::test]
async fn http_post_buffered_with_json_body() {
    let f = fixture();
    let out = run(
        "fetcher",
        json!({"url": format!("{}/chat", f.echo), "body": {"a": 1}, "stream": false, "max_bytes": 1048576}),
    )
    .await
    .unwrap();
    assert_eq!(out["h"]["status"], 200);
    let echoed: Value = gwead::serde_json::from_str(out["h"]["body"].as_str().unwrap()).unwrap();
    assert_eq!(echoed["method"], "POST");
    assert_eq!(echoed["path"], "/chat");
    assert_eq!(echoed["headers"]["content-type"], "application/json");
    assert_eq!(echoed["body"], "{\"a\":1}");
    assert!(f.requests_for("fetcher").iter().any(
        |a| matches!(a, Access::Http { method, url } if method == "POST" && url.ends_with("/chat"))
    ));
}

#[tokio::test]
async fn http_get_streaming_returns_a_readable_handle() {
    let f = fixture();
    let streams = Arc::new(Mutex::new(StreamRegistry::new()));
    let r = f
        .kernel
        .execute(
            "getter",
            "go",
            json!({"url": format!("{}/stream", f.echo), "stream": true}),
        )
        .with_config(&json!({}))
        .with_streams(streams.clone())
        .run()
        .await
        .unwrap();
    let handle = r.step_results["h"]["body"].as_u64().expect("stream handle") as u32;
    let id = std::num::NonZeroU32::new(handle).unwrap();
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
    let echoed: Value = gwead::serde_json::from_slice(&collected).unwrap();
    assert_eq!(echoed["method"], "GET");
    assert_eq!(echoed["path"], "/stream");
}

#[tokio::test]
async fn a_get_with_a_body_is_refused() {
    let f = fixture();
    let err = run("getbody", json!({"url": format!("{}/x", f.echo)}))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("body"), "{err}");
}

#[tokio::test]
async fn http_post_secret_reaches_header_only_for_declaring_plugin() {
    let f = fixture();
    let out = run("secretive", json!({"url": format!("{}/auth", f.echo)}))
        .await
        .unwrap();
    let echoed: Value = gwead::serde_json::from_str(out["h"]["body"].as_str().unwrap()).unwrap();
    assert_eq!(echoed["headers"]["authorization"], "Bearer s3cr3t");
}

// ---------------------------------------------------------------- http redirects

#[tokio::test]
async fn redirect_is_followed_and_every_hop_faces_the_operator() {
    let f = fixture();
    let start = format!("{}/redirect?to=/final", f.echo);
    let out = run("getter", json!({"url": start, "stream": false}))
        .await
        .unwrap();
    let echoed: Value = gwead::serde_json::from_str(out["h"]["body"].as_str().unwrap()).unwrap();
    assert_eq!(echoed["path"], "/final", "the redirect was not followed");
    let urls = f.urls_for("getter");
    assert!(urls.contains(&start), "{urls:?}");
    assert!(
        urls.iter().any(|u| u.ends_with("/final")),
        "the operator never saw the hop: {urls:?}"
    );
}

#[tokio::test]
async fn redirect_to_an_ungranted_host_is_refused() {
    let f = fixture();
    // Same machine, same port — but `localhost` is not the host the
    // manifest declared, and the grant is what decides.
    let onward = f.echo.replace("127.0.0.1", "localhost");
    let err = run(
        "getter",
        json!({"url": format!("{}/redirect?to={onward}/final", f.echo), "stream": false}),
    )
    .await
    .unwrap_err();
    assert!(
        err.to_string().contains("network:egress:localhost"),
        "{err}"
    );
    // Not `contains`: the starting URL names the onward host in its query
    // string. What must never appear is a request *to* it.
    assert!(
        !f.urls_for("getter").iter().any(|u| u.starts_with(&onward)),
        "operator was asked about a host the manifest never declared"
    );
}

#[tokio::test]
async fn redirect_keeps_credentials_within_an_origin_and_drops_them_across_one() {
    let f = fixture();
    let same = run(
        "secretive",
        json!({"url": format!("{}/redirect?to=/after", f.echo)}),
    )
    .await
    .unwrap();
    let echoed: Value = gwead::serde_json::from_str(same["h"]["body"].as_str().unwrap()).unwrap();
    assert_eq!(echoed["path"], "/after");
    assert_eq!(echoed["headers"]["authorization"], "Bearer s3cr3t");
    assert_eq!(echoed["headers"]["x-api-key"], "s3cr3t");

    let across = run(
        "secretive",
        json!({"url": format!("{}/redirect?to={}/after", f.echo, f.echo2)}),
    )
    .await
    .unwrap();
    let echoed: Value = gwead::serde_json::from_str(across["h"]["body"].as_str().unwrap()).unwrap();
    assert_eq!(echoed["path"], "/after");
    assert_eq!(
        echoed["headers"].get("authorization"),
        None,
        "the key followed a redirect off its origin: {echoed}"
    );
    assert_eq!(
        echoed["headers"].get("x-api-key"),
        None,
        "a non-standard credential header followed a redirect off its origin: {echoed}"
    );
}

#[tokio::test]
async fn http_buffered_body_is_bounded_by_max_bytes() {
    let f = fixture();
    // The echo of a 100 KB request body is well past the 500-byte cap.
    let big = "y".repeat(100_000);
    let out = run(
        "fetcher",
        json!({"url": format!("{}/big", f.echo), "body": big, "stream": false, "max_bytes": 500}),
    )
    .await
    .unwrap();
    assert_eq!(out["h"]["truncated"], true);
    assert!(out["h"]["body"].as_str().unwrap().len() <= 500);
}

#[tokio::test]
async fn redirect_chain_is_bounded() {
    let f = fixture();
    let err = run(
        "tuned",
        json!({"url": format!("{}/loop", f.echo), "stream": false, "timeout_ms": 10000, "idle_timeout_ms": 10000, "max_redirects": 2}),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("more than 2 redirects"), "{err}");
}

// ---------------------------------------------------------------- http time

#[tokio::test]
async fn http_times_out_reaching_a_response() {
    let f = fixture();
    let err = run(
        "tuned",
        json!({"url": format!("{}/hang", f.echo), "stream": false, "timeout_ms": 200, "idle_timeout_ms": 10000, "max_redirects": 5}),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("exceeded timeout"), "{err}");
}

#[tokio::test]
async fn stalled_stream_ends_in_an_io_error_rather_than_hanging() {
    let f = fixture();
    let streams = Arc::new(Mutex::new(StreamRegistry::new()));
    let r = f
        .kernel
        .execute(
            "tuned",
            "go",
            json!({"url": format!("{}/dribble", f.echo), "stream": true, "timeout_ms": 10000, "idle_timeout_ms": 200, "max_redirects": 5}),
        )
        .with_config(&json!({}))
        .with_streams(streams.clone())
        .run()
        .await
        .unwrap();
    let handle = r.step_results["h"]["body"].as_u64().expect("stream handle") as u32;
    let id = std::num::NonZeroU32::new(handle).unwrap();
    let mut buf = [0u8; 64];
    let mut seen = Vec::new();
    let code = loop {
        let n = read_async_shared(&streams, id, &mut buf).await;
        if n < 0 {
            break n;
        }
        seen.extend_from_slice(&buf[..n as usize]);
    };
    assert_eq!(seen, b"data: half", "the delivered prefix was lost");
    assert_eq!(
        code, STREAM_IO_ERROR,
        "a peer that stopped talking should surface as an error, not EOF"
    );
}

// ---------------------------------------------------------------- cause

fn a_tool_call() -> ToolCall {
    ToolCall {
        id: Some("call_01".into()),
        name: "read".into(),
        arguments: r#"{"path":"caused.txt"}"#.into(),
    }
}

async fn run_for(plugin: &str, input: Value, call: &ToolCall) -> Result<Value, KernelError> {
    fixture()
        .kernel
        .execute(plugin, "go", input)
        .with_config(&json!({}))
        .with_exec_ctx(gwennol_core::context::exec_context(call))
        .run()
        .await
        .map(|r| Value::Object(r.step_results.into_iter().collect()))
}

#[tokio::test]
async fn approval_names_the_tool_call_that_caused_it() {
    let f = fixture();
    std::fs::write(f.workspace.join("caused.txt"), "why").unwrap();
    run_for(
        "reader",
        json!({"path": "caused.txt", "max_bytes": 1024}),
        &a_tool_call(),
    )
    .await
    .unwrap();
    assert!(
        f.causes_for("reader").contains(&Some(a_tool_call())),
        "the operator could not say what asked for this: {:?}",
        f.causes_for("reader")
    );
}

#[tokio::test]
async fn the_cause_survives_a_dispatch_into_another_plugin() {
    let f = fixture();
    std::fs::write(f.workspace.join("delegated.txt"), "through").unwrap();
    let call = ToolCall {
        name: "delegating-read".into(),
        ..a_tool_call()
    };
    run_for("delegator", json!({"path": "delegated.txt"}), &call)
        .await
        .unwrap();
    // `reader` ran the step, but the context came from the invocation that
    // started two plugins ago.
    assert!(
        f.causes_for("reader").contains(&Some(call)),
        "the cause was lost crossing an invoke: {:?}",
        f.causes_for("reader")
    );
}

#[tokio::test]
async fn an_action_the_frontend_started_itself_has_no_cause() {
    let f = fixture();
    std::fs::write(f.workspace.join("uncaused.txt"), "none").unwrap();
    run("reader", json!({"path": "uncaused.txt", "max_bytes": 1024}))
        .await
        .unwrap();
    assert!(
        f.causes_for("reader").contains(&None),
        "an unattributed approval should say so rather than borrow a cause"
    );
}

// ---------------------------------------------------------------- gates

#[tokio::test]
async fn kernel_refuses_ungranted_step_type_before_operator_is_asked() {
    let f = fixture();
    let err = run("ungranted", json!({"path": "hello.txt"}))
        .await
        .unwrap_err();
    assert!(matches!(err, KernelError::Validation(_)), "{err:?}");
    assert!(err.to_string().contains("step_type:host_fs.read"), "{err}");
    assert!(
        f.requests_for("ungranted").is_empty(),
        "operator was asked despite missing grant"
    );
}

#[tokio::test]
async fn missing_egress_grant_is_refused_before_operator_is_asked() {
    let f = fixture();
    let err = run("nofetch", json!({"url": format!("{}/x", f.echo)}))
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("network:egress:127.0.0.1"),
        "{err}"
    );
    assert!(
        f.requests_for("nofetch").is_empty(),
        "operator was asked despite missing egress grant"
    );
}

#[tokio::test]
async fn operator_denial_fails_the_step() {
    let f = fixture();
    std::fs::write(f.workspace.join("secret.txt"), "no").unwrap();
    let err = run("denied", json!({"path": "secret.txt"}))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("operator denied"), "{err}");
    assert_eq!(
        f.requests_for("denied"),
        vec![Access::ReadFile(f.workspace.join("secret.txt"))]
    );
}

#[test]
fn host_manifests_are_valid_and_nothing_is_freely_usable() {
    let mut names = Vec::new();
    for manifest in gwennol_core::HOST_MANIFESTS {
        let m: Value = gwead::serde_json::from_str(manifest).unwrap();
        let plugin = m["name"].as_str().unwrap();
        for d in m["stepTypeDefs"].as_array().unwrap() {
            assert_ne!(
                d["freelyUsable"], true,
                "{} must require a grant",
                d["name"]
            );
            let name = d["name"].as_str().unwrap();
            assert_eq!(
                name.split('.').next().unwrap(),
                plugin,
                "step type {name} is not under its own plugin's prefix"
            );
            names.push(name.to_string());
        }
    }
    assert_eq!(
        names,
        [
            "host_fs.read",
            "host_fs.write",
            "host_fs.list",
            "host_process.run",
            "host_http.get",
            "host_http.post",
        ]
    );
}
