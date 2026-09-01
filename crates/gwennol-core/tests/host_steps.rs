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

/// Records every approval request; denies anything a `denied*` plugin
/// asks for; answers `token` for the `secretive` plugin.
#[derive(Default)]
struct Recorder {
    requests: Mutex<Vec<ApprovalRequest>>,
}

#[async_trait::async_trait]
impl Operator for Recorder {
    async fn approve(&self, request: ApprovalRequest) -> Decision {
        let deny = request.plugin.starts_with("denied");
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
            "plain_writer",
            &["step_type:host_fs.write"],
            json!([{"id": "w", "type": "host_fs.write", "params": {"path": "{{$input.path}}", "content": "{{$input.content}}"}}]),
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
        plugin(
            "denied_prober",
            &["step_type:host_fs.read"],
            json!([{"id": "r", "type": "host_fs.read", "params": {"path": "{{$input.path}}"}}]),
        ),
        plugin(
            "denied_runner",
            &["step_type:host_process.run"],
            json!([{"id": "p", "type": "host_process.run", "params": {"argv": "{{$input.argv}}", "stdin": "{{$input.stdin}}", "timeout_ms": 1000, "max_output_bytes": 1024}}]),
        ),
        plugin(
            "denied_getter",
            &["step_type:host_http.get", "network:egress:127.0.0.1"],
            json!([{"id": "h", "type": "host_http.get", "params": {"url": "{{$input.url}}"}}]),
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
                    // Answers and then streams a body forever.
                    "/endless" => {
                        write!(s, "HTTP/1.1 200 OK\r\ncontent-type: application/octet-stream\r\nconnection: close\r\n\r\n").unwrap();
                        let chunk = [b'z'; 8192];
                        while s.write_all(&chunk).is_ok() {
                            let _ = s.flush();
                        }
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
async fn fs_read_refuses_a_fifo_instead_of_blocking_on_it() {
    let f = fixture();
    let path = f.workspace.join("pipe.fifo");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );
    // Losing the refusal makes this an unwrap_err failure (a nonblocking
    // writerless-FIFO open reads as instant EOF). Losing O_NONBLOCK parks
    // the open; the timeout turns that into a failure, and the writer
    // thread below then releases the parked blocking worker so the suite
    // can exit instead of hanging on it. The refusal must also come
    // before the operator is asked.
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        run("reader", json!({"path": "pipe.fifo", "max_bytes": 10})),
    )
    .await;
    if result.is_err() {
        let unblock = path.clone();
        std::thread::spawn(move || {
            let _ = std::fs::OpenOptions::new().write(true).open(unblock);
        });
    }
    let err = result
        .expect("fs_read blocked on a writerless FIFO")
        .unwrap_err();
    assert!(err.to_string().contains("fifo"), "{err}");
    assert!(
        !f.requests_for("reader").contains(&Access::ReadFile(path)),
        "the operator was asked to approve reading a conduit"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn fs_write_preserves_restrictive_permissions_on_overwrite() {
    use std::os::unix::fs::PermissionsExt as _;
    let f = fixture();
    let path = f.workspace.join("private.txt");
    std::fs::write(&path, "old").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    run(
        "writer",
        json!({"path": "private.txt", "content": "new", "dir": "."}),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "the replacement widened a private file");
}

#[cfg(unix)]
#[tokio::test]
async fn fs_write_does_not_carry_setuid_across_a_replacement() {
    use std::os::unix::fs::PermissionsExt as _;
    let f = fixture();
    let path = f.workspace.join("suid.sh");
    std::fs::write(&path, "old").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o4755)).unwrap();
    run(
        "writer",
        json!({"path": "suid.sh", "content": "new", "dir": "."}),
    )
    .await
    .unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
    assert_eq!(
        mode, 0o755,
        "replacing a setuid file minted a setuid file owned by the agent's user"
    );
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
    let mut inodes = Vec::new();
    for content in ["first", "second"] {
        run(
            "writer",
            json!({"path": "atomic/x.txt", "content": content, "dir": "atomic"}),
        )
        .await
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            inodes.push(
                std::fs::metadata(f.workspace.join("atomic/x.txt"))
                    .unwrap()
                    .ino(),
            );
        }
    }
    assert_eq!(
        std::fs::read_to_string(f.workspace.join("atomic/x.txt")).unwrap(),
        "second"
    );
    #[cfg(unix)]
    assert_ne!(
        inodes[0], inodes[1],
        "the replacement arrived in place — a rename must bring a new inode"
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

// ------------------------------------------------ fs: outcomes as data

/// The filesystem's answers the model must react to arrive as results
/// with an `outcome`, never as step errors — the errors-as-data rule a
/// declarative tool depends on (docs/SPI.md). The happy path says so
/// too, so a tool can branch on one field.
#[tokio::test]
async fn fs_steps_name_their_outcome_on_success() {
    let f = fixture();
    std::fs::write(f.workspace.join("named.txt"), "x").unwrap();
    let out = run("reader", json!({"path": "named.txt", "max_bytes": 10}))
        .await
        .unwrap();
    assert_eq!(out["r"]["outcome"], "ok");
    let out = run(
        "writer",
        json!({"path": "named-out/w.txt", "content": "y", "dir": "named-out"}),
    )
    .await
    .unwrap();
    assert_eq!(out["w"]["outcome"], "ok");
    assert_eq!(out["l"]["outcome"], "ok");
}

/// A read of a missing file is a `not_found` result — and the operator
/// was asked first, about the path canonical up to its deepest existing
/// ancestor, so a plugin cannot learn what exists without every probe
/// crossing the approval surface.
#[tokio::test]
async fn fs_read_reports_a_miss_as_data_after_asking() {
    let f = fixture();
    std::fs::create_dir_all(f.workspace.join("exists")).unwrap();
    let out = run(
        "reader",
        json!({"path": "exists/missing/file.txt", "max_bytes": 10}),
    )
    .await
    .expect("a miss is not a step error");
    assert_eq!(out["r"]["outcome"], "not_found");
    let message = out["r"]["message"].as_str().unwrap();
    assert!(
        message.contains("no such file") && message.contains("file.txt"),
        "{message}"
    );
    assert!(out["r"].get("content").is_none(), "no fabricated content");
    assert!(
        f.requests_for("reader").contains(&Access::ReadFile(
            f.workspace.join("exists/missing/file.txt")
        )),
        "the probe never reached the operator: {:?}",
        f.requests_for("reader")
    );
}

/// The remaining answers: a directory where a file was wanted, a path
/// through a file, and a file the agent's user may not read. Each is
/// data with the outcome named; none is a step error.
#[tokio::test]
async fn fs_read_reports_directories_and_bad_components_as_data() {
    let f = fixture();
    std::fs::create_dir_all(f.workspace.join("a-dir")).unwrap();
    let out = run("reader", json!({"path": "a-dir", "max_bytes": 10}))
        .await
        .unwrap();
    assert_eq!(out["r"]["outcome"], "is_directory");
    assert!(
        f.requests_for("reader")
            .contains(&Access::ReadFile(f.workspace.join("a-dir"))),
        "asked before answering"
    );

    std::fs::write(f.workspace.join("a-file"), "x").unwrap();
    let out = run(
        "reader",
        json!({"path": "a-file/below.txt", "max_bytes": 10}),
    )
    .await
    .unwrap();
    assert_eq!(out["r"]["outcome"], "not_a_directory");
}

#[cfg(unix)]
#[tokio::test]
async fn fs_read_reports_permission_denied_as_data() {
    use std::os::unix::fs::PermissionsExt as _;
    let f = fixture();
    let path = f.workspace.join("locked.txt");
    std::fs::write(&path, "x").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&path).is_ok() {
        eprintln!("skipping: this user (root?) reads a mode-000 file anyway");
        return;
    }
    let out = run("reader", json!({"path": "locked.txt", "max_bytes": 10}))
        .await
        .unwrap();
    assert_eq!(out["r"]["outcome"], "permission_denied");
    assert!(
        f.requests_for("reader").contains(&Access::ReadFile(path)),
        "asked before answering"
    );
}

/// A write whose parent is missing (and `create_dirs` unset), or whose
/// destination is a directory, is the destination's answer: data, with
/// nothing written and no temporary left behind.
#[tokio::test]
async fn fs_write_reports_a_missing_parent_and_a_directory_in_the_way_as_data() {
    let f = fixture();
    let out = run(
        "plain_writer",
        json!({"path": "no-parent/w.txt", "content": "z"}),
    )
    .await
    .expect("not a step error");
    assert_eq!(out["w"]["outcome"], "not_found");
    assert!(
        !f.workspace.join("no-parent").exists(),
        "nothing was created"
    );
    assert!(
        f.requests_for("plain_writer")
            .contains(&Access::WriteFile(f.workspace.join("no-parent/w.txt"))),
        "asked before answering"
    );

    // In its own directory: the leftover scan must not see a sibling
    // test's in-flight temporary in the shared workspace root.
    std::fs::create_dir_all(f.workspace.join("blocked/in-the-way")).unwrap();
    let out = run(
        "plain_writer",
        json!({"path": "blocked/in-the-way", "content": "z"}),
    )
    .await
    .unwrap();
    assert_eq!(out["w"]["outcome"], "is_directory");
    assert!(
        f.workspace.join("blocked/in-the-way").is_dir(),
        "left alone"
    );
    let leftovers: Vec<_> = std::fs::read_dir(f.workspace.join("blocked"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("gwennol-tmp"))
        .collect();
    assert_eq!(leftovers, Vec::<String>::new());
}

/// A listing of a missing directory, or of a file, is data too — and the
/// missing one is approved as a probe like a read miss.
#[tokio::test]
async fn fs_list_reports_missing_and_non_directories_as_data() {
    let f = fixture();
    let out = run("lister", json!({"dir": "no-such-dir", "max": 10}))
        .await
        .expect("not a step error");
    assert_eq!(out["l"]["outcome"], "not_found");
    assert!(
        f.requests_for("lister")
            .contains(&Access::ListDir(f.workspace.join("no-such-dir"))),
        "asked before answering"
    );

    std::fs::write(f.workspace.join("listed-file"), "x").unwrap();
    let out = run("lister", json!({"dir": "listed-file", "max": 10}))
        .await
        .unwrap();
    assert_eq!(out["l"]["outcome"], "not_a_directory");
}

/// The line between data and error: the operator's denial of a probe is
/// still a step error, even when the file does not exist — a denied
/// miss must not become "not found".
#[tokio::test]
async fn a_denied_probe_of_a_missing_file_is_an_error_not_a_miss() {
    let f = fixture();
    let err = run("denied_prober", json!({"path": "never-there.txt"}))
        .await
        .expect_err("denial is a step error");
    assert!(err.to_string().contains("operator denied"), "{err}");
    assert!(
        f.requests_for("denied_prober")
            .contains(&Access::ReadFile(f.workspace.join("never-there.txt")))
    );
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
async fn process_run_survives_a_child_that_floods_stdout_and_never_reads_stdin() {
    // The child fills its stdout pipe while never touching stdin, and the
    // stdin payload is bigger than any pipe buffer — so a sequential
    // "write stdin, then drain output" host deadlocks against it with the
    // timeout out of reach. Concurrent feeding and draining completes it.
    let big = "x".repeat(1 << 20);
    let out = run(
        "runner",
        json!({"argv": ["sh", "-c", "head -c 200000 /dev/zero"], "stdin": big, "timeout_ms": 10000, "max_output_bytes": 1048576}),
    )
    .await
    .unwrap();
    assert_eq!(out["p"]["status"], 0);
    assert_eq!(out["p"]["stdout"].as_str().unwrap().len(), 200_000);
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
    // direct child; its recorded pid is what makes this a real pin — a
    // group-existence check alone reads "dead" vacuously when no group
    // was ever created.
    let err = run(
        "runner",
        json!({"argv": ["sh", "-c", "sleep 30 & echo $! > bg.pid; sleep 30"], "stdin": "", "timeout_ms": 500, "max_output_bytes": 1048576}),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("exceeded timeout"), "{err}");
    let bg: u32 = std::fs::read_to_string(f.workspace.join("bg.pid"))
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
            .arg(format!("kill -0 {bg} 2>/dev/null"))
            .status()
            .unwrap()
            .success();
        if !alive {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "backgrounded child {bg} outlived the timeout — only its parent was killed"
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

#[tokio::test]
async fn cancelling_an_invocation_tears_a_running_step_down() {
    let f = fixture();
    let cancel = gwennol_core::gwead::tokio_util::sync::CancellationToken::new();
    let kernel = f.kernel.clone();
    let token = cancel.clone();
    let handle = tokio::spawn(async move {
        kernel
            .execute(
                "runner",
                "go",
                json!({"argv": ["sleep", "30"], "stdin": "", "timeout_ms": 60000, "max_output_bytes": 1024}),
            )
            .with_config(&json!({}))
            .with_cancel(token)
            .run()
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    cancel.cancel();
    let err = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
        .await
        .expect("cancellation did not tear the step down")
        .unwrap()
        .unwrap_err();
    assert!(err.to_string().contains("cancelled"), "{err}");
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

#[tokio::test]
async fn a_failed_connection_does_not_leak_the_url_credentials() {
    // reqwest's error Display appends `for url (…)` unredacted; the host
    // must scrub the error's own URL before formatting it.
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    }; // dropped: nothing listens here any more
    let err = run(
        "getter",
        json!({"url": format!("http://user:pw-secret@127.0.0.1:{port}/p?token=qs-secret"), "stream": false}),
    )
    .await
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("request to 127.0.0.1"), "{msg}");
    assert!(
        !msg.contains("qs-secret"),
        "a transport error leaked the query string: {msg}"
    );
    // Defensive, not a pin: reqwest moves userinfo into a basic-auth
    // header at request build, so today no send error can carry it. The
    // scrub guards a future reqwest that keeps it in the URL.
    assert!(
        !msg.contains("pw-secret"),
        "a transport error leaked URL userinfo: {msg}"
    );
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
async fn http_stops_reading_an_endless_body_at_the_cap() {
    // A host that buffers the whole body before applying the cap never
    // returns from this endpoint; one that stops past the cap is instant.
    let f = fixture();
    let out = run(
        "fetcher",
        json!({"url": format!("{}/endless", f.echo), "body": "", "stream": false, "max_bytes": 1000}),
    )
    .await
    .unwrap();
    assert_eq!(out["h"]["truncated"], true);
    assert!(out["h"]["body"].as_str().unwrap().len() <= 1000);
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
async fn a_denial_names_the_shape_but_not_the_payload() {
    let f = fixture();
    let err = run(
        "denied_runner",
        json!({"argv": ["cat"], "stdin": "hunter2-super-secret"}),
    )
    .await
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("operator denied"), "{msg}");
    assert!(msg.contains("spawn"), "{msg}");
    assert!(
        !msg.contains("hunter2"),
        "the denial leaked stdin into a plugin-visible error: {msg}"
    );
    // The *operator* still saw the payload — that is the point of the gate.
    assert!(
        f.requests_for("denied_runner")
            .iter()
            .any(|a| matches!(a, Access::Spawn { stdin: Some(s), .. } if s.contains("hunter2")))
    );
}

#[tokio::test]
async fn a_denied_url_loses_its_credentials_but_keeps_its_shape() {
    let f = fixture();
    let port = f.echo.rsplit(':').next().unwrap();
    let err = run(
        "denied_getter",
        json!({"url": format!("http://user:pw-secret@127.0.0.1:{port}/secret-path?token=qs-secret")}),
    )
    .await
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("operator denied"), "{msg}");
    assert!(msg.contains("GET"), "{msg}");
    assert!(msg.contains("/secret-path"), "{msg}");
    assert!(
        !msg.contains("qs-secret"),
        "the denial leaked a query-string credential: {msg}"
    );
    assert!(
        !msg.contains("pw-secret") && !msg.contains("user:"),
        "the denial leaked URL userinfo: {msg}"
    );
    // The operator, as ever, saw the whole thing.
    assert!(
        f.urls_for("denied_getter")
            .iter()
            .any(|u| u.contains("qs-secret"))
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
