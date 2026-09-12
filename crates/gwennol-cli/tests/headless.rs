//! End to end: the built `gwennol` binary runs a task
//! headlessly against the bundled plugins and a stub Messages API, with
//! every approval decided by a rule and traced to it, and no prompt
//! anywhere.
//!
//! The roadmap's "6. Non-interactive CLI" done-when, each pinned
//! here: a real task — read a
//! file, answer from it — runs with no interaction; each decision's
//! trace names the flag or file rule that made it, or says none did;
//! a request no rule matches is denied and the model is told, not the
//! user re-prompted; secrets arrive by the documented sources and never
//! from nowhere; Ctrl-C cancels the turn and the process says so; and
//! a usage error is a usage error. The stub answers the opening turn
//! with a thinking block before its tool call, so the transcript pins
//! that the thinking is replayed whole on the follow-up — the shape
//! the first live smoke test must reproduce.

mod common;
use common::{API_KEY, Stub, stub, thinking_block};

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use provider_anthropic::PLUGIN_NAME as PROVIDER;
use serde_json::{Value, json};

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

/// The binary with stderr merged into stdout, through a shell, so the
/// order between the two streams is observable: both are flushed per
/// line or per round, and one pipe keeps the sequence.
fn merged(cmd: &mut Command) -> Command {
    let mut sh = Command::new("sh");
    sh.arg("-c")
        .arg("exec \"$0\" \"$@\" 2>&1")
        .arg(cmd.get_program());
    sh.args(cmd.get_args());
    sh.env_clear();
    for (k, v) in cmd.get_envs().filter_map(|(k, v)| v.map(|v| (k, v))) {
        sh.env(k, v);
    }
    if let Some(dir) = cmd.get_current_dir() {
        sh.current_dir(dir);
    }
    sh.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    sh
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

    // The transcript file is the conversation as the provider saw it:
    // the whole chat input — system prompt, tools, settings — with the
    // thinking carried as the contract's opaque block, not the
    // messages alone.
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(&transcript).unwrap()).unwrap();
    assert_eq!(saved["system"], follow_up["system"]);
    assert_eq!(saved["tools"], follow_up["tools"]);
    assert_eq!(saved["stream"], true);
    let tools: Vec<&str> = saved["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(tools, ["bash", "grep", "read", "write"]);
    let messages = saved["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[1]["content"][0]["type"], "opaque");
    assert_eq!(messages[1]["content"][0]["data"], thinking_block());
    assert_eq!(messages[3]["role"], "assistant");
    // Everything but the final answer is the request the stub received
    // on the last round — in contract form: the one difference is the
    // thinking block, which the file holds as `opaque` and the provider
    // unwrapped to the vendor's own block on the wire.
    let wire = follow_up["messages"].as_array().unwrap();
    assert_eq!(messages[0], wire[0]);
    assert_eq!(messages[2], wire[2]);
    assert_eq!(messages[1]["role"], wire[1]["role"]);
    assert_eq!(
        messages[1]["content"].as_array().unwrap()[1..],
        wire[1]["content"].as_array().unwrap()[1..]
    );
    assert_eq!(wire[1]["content"][0], thinking_block());
}

#[test]
fn a_transcript_that_cannot_be_written_comes_after_the_outcome() {
    let f = fixture();
    let config = f.config("transcript", "", "");
    let r = run(f
        .gwennol()
        .env(KEY_VAR, API_KEY)
        .arg("--config")
        .arg(&config)
        .args(["--allow", &f.allow_stub(), "--allow", "read:**"])
        .args(["--transcript", "/nonexistent/dir/t.json"])
        .arg("What does hello.txt say?"));
    // The turn completed and said so first; the write failed after,
    // and a completed turn with no transcript is exit 2.
    assert_eq!(r.status.code(), Some(2), "{:?}", r.status);
    let done = r
        .stderr
        .find("gwennol: done (EndTurn)")
        .expect("outcome line");
    let failed = r
        .stderr
        .find("gwennol: transcript /nonexistent/dir/t.json: ")
        .expect("transcript failure line");
    assert!(done < failed, "{}", r.stderr);
    assert_eq!(
        r.stdout,
        "Let me read it.\nIt says: hello from the workspace\n"
    );

    // A failed turn keeps its own status: without a key the vendor
    // refuses, and the unwritable transcript is reported after that
    // without turning the 1 into a 2.
    let r = run(f
        .gwennol()
        .arg("--config")
        .arg(&config)
        .args(["--allow", &f.allow_stub()])
        .args(["--transcript", "/nonexistent/dir/t.json"])
        .arg("What does hello.txt say?"));
    assert_eq!(r.status.code(), Some(1), "{:?}", r.status);
    let failed_turn = r
        .stderr
        .find("gwennol: turn failed: ")
        .expect("outcome line");
    let failed_write = r
        .stderr
        .find("gwennol: transcript /nonexistent/dir/t.json: ")
        .expect("transcript failure line");
    assert!(failed_turn < failed_write, "{}", r.stderr);
}

#[test]
fn a_round_the_model_ends_in_a_refusal_still_reaches_stdout_first() {
    let f = fixture();
    let config = f.config("refusal", "/refusal", "");
    let r = run(&mut merged(
        f.gwennol()
            .env(KEY_VAR, API_KEY)
            .arg("--config")
            .arg(&config)
            .args(["--allow", &f.allow_stub(), "--allow", "read:**"])
            .arg("What does hello.txt say?"),
    ));
    assert!(r.status.success(), "{:?}", r.status);
    // The call was never dispatched — no `->` line — and the round it
    // belongs to is in the transcript, so its text is owed to stdout
    // before the call's failure is reported.
    assert!(!r.stdout.contains("gwennol: -> read"), "{}", r.stdout);
    let text = r
        .stdout
        .find("I would rather not.\n")
        .expect("the round's text");
    let failed = r
        .stdout
        .find("gwennol: !! read toolu_r1: ")
        .expect("the undispatched call's failure");
    assert!(text < failed, "{}", r.stdout);
    assert!(r.stdout.contains("gwennol: done (Refusal)"), "{}", r.stdout);
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
    let failed = r
        .stderr
        .lines()
        .find(|line| line.starts_with("gwennol: turn failed: "))
        .expect("outcome line");
    // The kernel names the actual invoked-streaming action that
    // failed, provider-anthropic's own sibling `stream_turn` action
    // (`provider-anthropic/src/lib.rs`'s `STREAM_ACTION`), not the
    // public `chat` entry point that spawned it.
    // Both needles are matched within the one outcome line on purpose:
    // two `stderr_has` calls would not pin that they share it. A
    // recorded text carrying a newline would split the line and fail
    // here for an unrelated reason; none of the texts this test can
    // see carries one.
    assert!(
        failed.contains(
            "the stream failed before the turn did: provider-anthropic.stream_turn failed:"
        ),
        "{failed}"
    );
    assert!(failed.contains("operator denied"), "{failed}");

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
        // A cancelled turn keeps 130 even when the transcript cannot
        // be written afterwards.
        .args(["--transcript", "/nonexistent/dir/t.json"])
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
    // The whole line, not a prefix: a plain cancellation carries no
    // text, and a detail leaking into a suffix must fail this, not
    // slip past a `find` that only checks the start.
    let lines: Vec<&str> = stderr.lines().collect();
    let cancelled = lines
        .iter()
        .position(|&line| line == "gwennol: cancelled")
        .expect("outcome line, exactly, carrying no leaked text");
    let failed_write = lines
        .iter()
        .position(|&line| line.starts_with("gwennol: transcript /nonexistent/dir/t.json: "))
        .expect("transcript failure line");
    assert!(cancelled < failed_write, "{stderr}");
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

/// The workspace is settled before the task is read or any file is
/// loaded, so a run that has both a workspace that does not exist and
/// an empty task reports the workspace.
#[test]
fn a_missing_workspace_is_reported_before_the_task_is_read() {
    let f = fixture();
    let r = run(f.gwennol().args(["-C", "/nonexistent/ws"]).arg("  "));
    assert_eq!(r.status.code(), Some(2), "{:?}", r.status);
    r.stderr_has("gwennol: workspace /nonexistent/ws: ");
    assert!(!r.stderr.contains("the task is empty"), "{}", r.stderr);
}
