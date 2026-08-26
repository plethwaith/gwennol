//! End-to-end: a plugin action runs a `host_*` step through a real Gwead
//! kernel, and both gates — manifest (kernel-enforced) and operator
//! (host-asked) — behave as documented.
//!
//! The host is a process singleton, so this binary boots one kernel with
//! every fixture plugin registered up front and shares it across tests.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use gwead::kernel::{Kernel, KernelError};
use gwead::serde_json::{Value, json};
use gwennol_core::{Access, ApprovalRequest, Decision, Event, Operator, ToolCall, Turn};

/// Records every approval request; denies anything the `denied` plugin
/// asks for.
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
    async fn secret(&self, _plugin: &str, _name: &str) -> Option<String> {
        None
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
}

fn fixture() -> &'static Fixture {
    static F: OnceLock<Fixture> = OnceLock::new();
    F.get_or_init(|| {
        let workspace = tempfile::tempdir().unwrap().keep();
        let operator = Arc::new(Recorder::default());
        let mut kernel = gwennol_core::boot(operator.clone(), workspace.clone()).unwrap();
        for m in fixture_plugins() {
            kernel.register_plugin_from_json(&m.to_string()).unwrap();
        }
        Fixture {
            kernel: kernel.into_arc(),
            operator,
            workspace,
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
    ]
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
    let asked = f.requests_for("writer");
    assert!(asked.contains(&Access::WriteFile(f.workspace.join("out/nested/a.txt"))));
    assert!(asked.contains(&Access::ListDir(f.workspace.join("out/nested"))));
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
    assert_eq!(names, ["host_fs.read", "host_fs.write", "host_fs.list"]);
}
