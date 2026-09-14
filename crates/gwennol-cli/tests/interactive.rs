//! The interactive loop, in-process against the stub: one host boot
//! (`gwennol_core::host`'s `OnceLock`, `kernel.rs`'s
//! `install(host).map_err(|_| BootError::AlreadyInstalled)`) and one
//! session, so `tests/interactive.rs` runs `drive` several times on
//! that one session rather than booting a kernel per scenario — the
//! alternative a per-scenario session would need is the kernel `Arc`
//! `Session` keeps private, a core change out of this slice's scope.
//! The stub's `/scripted` route (`tests/common/mod.rs`) reads the
//! first word of the turn's own text to choose a scenario, so one
//! session can drive every case a real terminal would meet.

mod common;
use common::{API_KEY, stub, thinking_block};

use std::path::PathBuf;
use std::process::{ExitCode, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

use clap::{CommandFactory, FromArgMatches};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use gwennol::tui::drive::drive;
use gwennol::tui::keys::Input;
use gwennol::tui::prompt::TITLE;
use gwennol::tui::screen::{Kitty, Screen};
use gwennol::tui::ui::{Entry, Shared, TurnState, Ui, render};
use gwennol::{Cli, frontend, ordered_rule_flags, tui};
use gwennol_core::Session;
use provider_anthropic::PLUGIN_NAME as PROVIDER;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

// ------------------------------------------------------------- fixture

/// The test-only `tool-sh` manifest: pipes its `script` argument to
/// `sh` on stdin, shaped like `plugins/tools/bash.json`. It exists so
/// the suite can raise a spawn that carries stdin, which no bundled
/// tool does: every real tool runs its command as argv, never through
/// an interpreter fed on stdin.
const TOOL_SH_MANIFEST: &str = r#"{
  "formatVersion": 1,
  "name": "tool-sh",
  "version": "0.1.0",
  "description": "Test-only: pipes its script to sh on stdin, so the approval suite can raise a spawn that carries stdin.",
  "roles": ["TOOL"],
  "permissions": ["step_type:host_process.run"],
  "actions": {
    "call": {
      "tool": {
        "name": "sh",
        "description": "Run a script by piping it to sh on stdin.",
        "parameters": {
          "type": "object",
          "required": ["script"],
          "additionalProperties": false,
          "properties": {
            "script": { "type": "string", "description": "Fed to sh on stdin." }
          }
        }
      },
      "steps": [
        {
          "id": "run",
          "type": "host_process.run",
          "params": {
            "argv": ["sh"],
            "stdin": "{{$input.script}}",
            "timeout_ms": 60000,
            "max_output_bytes": 262144
          }
        },
        {
          "id": "answer",
          "type": "return",
          "params": { "value": {
            "content": "{{$steps.run.result.stdout}}",
            "is_error": "{{$steps.run.result.status != 0}}"
          } }
        }
      ]
    }
  }
}
"#;

struct Fixture {
    /// The fixture's own root: the workspace's parent, and where
    /// `outside.txt` sits — a path outside the workspace an
    /// interactive spawn can be judged at (D2).
    root: PathBuf,
    workspace: PathBuf,
    key_file: PathBuf,
    config: PathBuf,
}

fn fixture() -> &'static Fixture {
    static F: OnceLock<Fixture> = OnceLock::new();
    F.get_or_init(|| {
        let root = tempfile::tempdir().unwrap().keep().canonicalize().unwrap();
        let workspace = root.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("hello.txt"), "hello from the workspace\n").unwrap();
        std::fs::write(root.join("outside.txt"), "outside\n").unwrap();

        let mut bundled = xtask::bundle(&xtask::workspace_root())
            .unwrap_or_else(|e| panic!("bundling plugins/ failed: {e}"));
        for plugin in &mut bundled {
            if plugin.name() == PROVIDER {
                plugin.manifest["permissions"]
                    .as_array_mut()
                    .unwrap()
                    .push(serde_json::json!("network:egress:127.0.0.1"));
            }
        }
        let bundle_root = root.join("bundle");
        xtask::write_bundle(&bundled, &bundle_root).unwrap();
        let plugins = bundle_root.join(xtask::PLUGINS_DIR);
        std::fs::write(plugins.join("tools").join("sh.json"), TOOL_SH_MANIFEST).unwrap();

        let stub = stub();
        let key_file = root.join("anthropic.key");
        std::fs::write(&key_file, API_KEY).unwrap();

        let config = root.join("scripted.toml");
        std::fs::write(
            &config,
            format!(
                "[plugins]\ndir = {plugins:?}\ntrust_runtimes = [{provider:?}]\n\n\
                 [plugin_config.{provider}]\nmodel = \"claude-fixture\"\nbase_url = \"http://{addr}/scripted\"\n\n\
                 [[rules]]\ndeny = \"write:forbidden.txt\"\n",
                plugins = plugins.display().to_string(),
                provider = PROVIDER,
                addr = stub.addr,
            ),
        )
        .unwrap();

        Fixture {
            root,
            workspace,
            key_file,
            config,
        }
    })
}

/// The session `tests/interactive.rs` shares across every scenario:
/// `tui::start` runs exactly once for the whole process. A
/// `tokio::sync::Mutex`, not `std`'s: the one lock this file takes is
/// held across `.await` for the scenario's whole run.
fn session() -> &'static tokio::sync::Mutex<(Session, std::sync::Arc<Shared>)> {
    static S: OnceLock<tokio::sync::Mutex<(Session, std::sync::Arc<Shared>)>> = OnceLock::new();
    S.get_or_init(|| {
        let f = fixture();
        let stub = stub();
        let matches = Cli::command().get_matches_from([
            "gwennol".to_string(),
            "--config".to_string(),
            f.config.display().to_string(),
            "--secret".to_string(),
            format!("{PROVIDER}:api_key=file:{}", f.key_file.display()),
            "--allow".to_string(),
            format!("http:POST http://{}/*", stub.addr),
            "--allow".to_string(),
            "read:**".to_string(),
            "--allow".to_string(),
            "spawn:bash *".to_string(),
            "-C".to_string(),
            f.workspace.display().to_string(),
        ]);
        let cli = Cli::from_arg_matches(&matches).unwrap();
        let flag_rules = ordered_rule_flags(&matches);
        let workspace = frontend::workspace(&cli).unwrap();
        let (session, shared) = tui::start(&cli, workspace, flag_rules).unwrap();
        tokio::sync::Mutex::new((session, shared))
    })
}

// -------------------------------------------------------------- helpers

fn type_line(tx: &UnboundedSender<Input>, text: &str) {
    for c in text.chars() {
        tx.send(Input::Key(KeyEvent::new(
            KeyCode::Char(c),
            KeyModifiers::NONE,
        )))
        .unwrap();
    }
    tx.send(Input::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )))
    .unwrap();
}

fn key(tx: &UnboundedSender<Input>, code: KeyCode, modifiers: KeyModifiers) {
    tx.send(Input::Key(KeyEvent::new(code, modifiers))).unwrap();
}

fn esc(tx: &UnboundedSender<Input>) {
    key(tx, KeyCode::Esc, KeyModifiers::NONE);
}

/// Wait until `check` holds, polling on `shared.changed`; bounded at
/// 10 s per wait and 2_000 changes total, so a stuck loop fails fast
/// rather than hanging the suite.
async fn await_ui(shared: &std::sync::Arc<Shared>, mut check: impl FnMut(&Ui) -> bool, what: &str) {
    let mut changes = shared.changed.subscribe();
    for _ in 0..2000 {
        if check(&shared.lock()) {
            return;
        }
        tokio::time::timeout(Duration::from_secs(10), changes.changed())
            .await
            .unwrap_or_else(|_| panic!("timed out after 10s waiting for: {what}"))
            .expect("the Shared sender outlives every waiter in this test");
    }
    panic!("gave up after 2000 changes waiting for: {what}");
}

/// Render `shared`'s current `Ui` into a fresh 80x24 backend and
/// return its rows as plain strings (trailing padding included).
fn frame(shared: &std::sync::Arc<Shared>) -> Vec<String> {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| render(&shared.lock(), f)).unwrap();
    let buf = terminal.backend().buffer();
    (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect()
        })
        .collect()
}

async fn run_drive(
    session: &mut Session,
    shared: &std::sync::Arc<Shared>,
    rx: &mut UnboundedReceiver<Input>,
    first: Option<String>,
) -> ExitCode {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    drive(session, shared, &mut terminal, rx, first)
        .await
        .unwrap()
}

/// Clear the editor and any notice between runs. Production never
/// needs this — `/exit` ends the process, so a stale editor never
/// meets a later turn — but this suite's runs share one `Ui` across
/// several `drive` calls (D12), and defensively so: a cancelled turn
/// leaves the draft in place rather than clearing it (D9), so an
/// un-cleared editor could in principle carry text into the next run
/// and have it read as one plain turn, even though every run in this
/// suite today happens to leave the editor empty when it ends.
fn reset_editor(shared: &std::sync::Arc<Shared>) {
    shared.update(|ui| {
        ui.editor = gwennol::tui::editor::Editor::default();
        ui.notice = None;
    });
}

/// How many `Outcome` entries `ui.entries[start..]` holds: one per
/// turn that has finished, the sync point a fast, all-local stub makes
/// necessary (`ui.turn` is still `Idle` between the submission and the
/// loop picking it up, so waiting on `TurnState::Idle` alone can match
/// the idle from before the turn).
fn outcomes_since(ui: &Ui, start: usize) -> usize {
    ui.entries[start..]
        .iter()
        .filter(|e| matches!(e, Entry::Outcome(_)))
        .count()
}

/// The `text` field of every `assistant`-role message from `from`
/// onward, concatenated: what the transcript holds for the completed
/// turns since a point, to compare against the pane's own `Assistant`
/// entries over the same range (built inline here, since
/// `Ui::assistant_text_since` has no upper bound).
fn assistant_text_from_transcript(transcript: &[Value], from: usize) -> String {
    transcript[from..]
        .iter()
        .filter(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
        .flat_map(|m| {
            m.get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect()
}

/// Whether an approval prompt is open.
fn prompt_open(ui: &Ui) -> bool {
    !ui.prompts.is_empty()
}

/// `v` pretty-printed, as separate lines with no leading/trailing
/// whitespace: what a prompt box's arguments block wraps, one JSON
/// line at a time, so a frame assertion can look for one key's own
/// row rather than the whole block.
fn pretty_lines(v: &Value) -> Vec<String> {
    serde_json::to_string_pretty(v)
        .unwrap()
        .lines()
        .map(|l| l.trim().to_string())
        .collect()
}

// ------------------------------------------------------------ 6.2.5

/// The panic hook restores the terminal (on stdout) before the
/// previous hook prints the panic. Runs itself as a child process,
/// under an env var, because the real assertion is about byte order
/// in a merged stdout+stderr stream a normal `#[test]` cannot capture
/// from itself. Guards D5. Mutation: call the previous hook before
/// `undo`.
#[test]
#[cfg(unix)]
fn the_panic_hook_restores_the_terminal_before_the_panic_prints() {
    if std::env::var_os("GWENNOL_PANIC_CHILD").is_some() {
        gwennol::tui::screen::install_panic_hook();
        let _screen = Screen::enter(std::io::stdout(), false, Kitty::On).unwrap();
        panic!("boom");
    }

    let exe = std::env::current_exe().unwrap();
    let mut sh = std::process::Command::new("sh");
    sh.arg("-c")
        .arg("exec \"$0\" \"$@\" 2>&1")
        .arg(&exe)
        .args([
            "--exact",
            "the_panic_hook_restores_the_terminal_before_the_panic_prints",
            "--nocapture",
        ])
        .env("GWENNOL_PANIC_CHILD", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = sh.output().unwrap();
    let merged = String::from_utf8_lossy(&out.stdout).into_owned();

    // The undo bytes crossterm's `LeaveAlternateScreen`/`PopKeyboardEnhancementFlags`
    // write, in the same order `screen::undo` writes them.
    let pop_kitty = "\x1b[<1u";
    let leave_screen = "\x1b[?1049l";
    let pop_at = merged
        .find(pop_kitty)
        .unwrap_or_else(|| panic!("no kitty-pop bytes in the child's output:\n{merged}"));
    let leave_at = merged
        .find(leave_screen)
        .unwrap_or_else(|| panic!("no leave-screen bytes in the child's output:\n{merged}"));
    let boom_at = merged
        .find("boom")
        .unwrap_or_else(|| panic!("no panic message in the child's output:\n{merged}"));
    assert!(
        pop_at < boom_at && leave_at < boom_at,
        "the terminal was not restored before the panic printed:\n{merged}"
    );
    // `find` above only locates the first occurrence: a `Drop` that
    // repeats the panic hook's own undo would still pass it. Count
    // instead: the kitty-pop bytes appear exactly once, from the panic
    // hook; `Drop` running afterward on the same guard must not send
    // them again.
    assert_eq!(
        merged.matches(pop_kitty).count(),
        1,
        "the kitty-pop bytes appeared more than once, so something undid them twice:\n{merged}"
    );
}

// -------------------------------------------------------------- 6.3

/// One session, driven several times: streaming, a mid-round retry, a
/// stalled stream cancelled two ways, a cancelled tool call, a failed
/// turn, slash-command edge cases, and a forced double `/exit`.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_streams_cancels_retries_and_exits() {
    tokio::time::timeout(Duration::from_secs(180), scenario())
        .await
        .expect("the whole scenario timed out at 180s");
}

async fn scenario() {
    let guard = session();
    let mut locked = guard.lock().await;
    let (session, shared) = &mut *locked;
    let shared = shared.clone();

    // ---- run A: two turns, then an idle /exit. Each run below gets
    // its own fresh channel, moved whole (never cloned) into its
    // driver: if a driver panics (an `await_ui` timeout, a failed
    // assertion), dropping its one `tx` closes the channel, so
    // `run_drive`'s `keys.next()` sees the source close and the run
    // ends in seconds rather than at the outer 180s timeout.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Input>();
    let start = shared.lock().entries.len();
    let driver = {
        let shared = shared.clone();
        tokio::spawn(async move {
            await_ui(
                &shared,
                |ui| outcomes_since(ui, start) >= 1,
                "run A: first turn outcome",
            )
            .await;
            type_line(&tx, "again please");
            await_ui(
                &shared,
                |ui| outcomes_since(ui, start) >= 2,
                "run A: second turn outcome",
            )
            .await;
            type_line(&tx, "/exit");
        })
    };
    let code = run_drive(
        session,
        &shared,
        &mut rx,
        Some("What does hello.txt say?".to_string()),
    )
    .await;
    driver.await.unwrap();
    assert_eq!(code, ExitCode::SUCCESS, "run A");
    assert!(
        matches!(shared.lock().turn, TurnState::Idle),
        "run A: the status stayed on the second turn's state after drive returned"
    );
    {
        let ui = shared.lock();
        let entries = &ui.entries[start..];
        let trace_at = |i: usize, prefix: &str| match &entries[i] {
            Entry::Trace(t) => assert!(
                t.starts_with(prefix),
                "run A entry {i}: {t:?} does not start with {prefix:?}"
            ),
            other => panic!("run A entry {i}: expected Trace, got {other:?}"),
        };
        assert_eq!(
            entries[0],
            Entry::User("What does hello.txt say?".to_string()),
            "run A entry 0"
        );
        // The approval decision for the round's own POST precedes the
        // model's text: the request is judged before it is sent.
        trace_at(1, "gwennol: POST http://");
        assert_eq!(
            entries[2],
            Entry::Assistant("Let me read it.".to_string()),
            "run A entry 2"
        );
        assert!(
            matches!(&entries[3], Entry::Trace(t) if t.starts_with("gwennol: -> read toolu_01: ") && t.contains("hello.txt")),
            "run A entry 3: {:?}",
            entries[3]
        );
        assert!(
            matches!(&entries[4], Entry::Trace(t) if t.starts_with("gwennol: read ") && t.ends_with("allowed by --allow \"read:**\"")),
            "run A entry 4: {:?}",
            entries[4]
        );
        trace_at(5, "gwennol: <- read toolu_01: ok, ");
        trace_at(6, "gwennol: POST http://");
        assert_eq!(
            entries[7],
            Entry::Assistant("It says: hello from the workspace\n".to_string()),
            "run A entry 7"
        );
        assert!(
            matches!(&entries[8], Entry::Outcome(t) if t.starts_with("gwennol: done (EndTurn): 2 rounds")),
            "run A entry 8: {:?}",
            entries[8]
        );

        // The invariant, bounded to turn 1's own entries and messages
        // (both turns have finished by now, `assistant_text_since`
        // has no upper bound, and `session.transcript()` already
        // holds the messages of the "again please" follow-up too):
        // the pane's `Assistant` entries since the turn's `User` entry
        // concatenate to the transcript's assistant `text` blocks
        // since that turn's user message (`transcript()[..4]`: the
        // helper itself filters on `role == "assistant"`).
        let ui_text: String = entries[1..9]
            .iter()
            .filter_map(|e| match e {
                Entry::Assistant(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        let transcript_text = assistant_text_from_transcript(&session.transcript()[..4], 0);
        assert_eq!(ui_text, transcript_text, "run A invariant (turn 1)");
        assert_eq!(
            ui_text,
            "Let me read it.It says: hello from the workspace\n"
        );
    }
    // The opening round's thinking block is replayed on the follow-up
    // exactly as the headless smoke test pins it, so the interactive
    // path carries the same contract.
    assert_eq!(
        session.transcript()[1]["content"][0]["data"],
        thinking_block()
    );
    assert_eq!(
        session.transcript().len(),
        8,
        "run A transcript length after turn 2"
    );

    reset_editor(&shared);
    // ---- run B: a retried round.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Input>();
    let start = shared.lock().entries.len();
    let driver = {
        let shared = shared.clone();
        tokio::spawn(async move {
            type_line(&tx, "flaky please");
            await_ui(
                &shared,
                |ui| outcomes_since(ui, start) >= 1,
                "run B: outcome",
            )
            .await;
            type_line(&tx, "/exit");
        })
    };
    let code = run_drive(session, &shared, &mut rx, None).await;
    driver.await.unwrap();
    assert_eq!(code, ExitCode::SUCCESS, "run B");
    {
        let ui = shared.lock();
        let entries = &ui.entries[start..];
        assert_eq!(
            entries[0],
            Entry::User("flaky please".to_string()),
            "run B entry 0"
        );
        assert!(
            entries
                .iter()
                .any(|e| matches!(e, Entry::Trace(t) if t.starts_with("gwennol: provider failure, retrying (2/3): "))),
            "run B: missing the retry trace: {entries:?}"
        );
        assert!(
            entries.iter().all(|e| match e {
                Entry::Assistant(t) | Entry::Trace(t) => !t.contains("so far"),
                _ => true,
            }),
            "run B: a retracted round's text leaked: {entries:?}"
        );
        assert!(
            entries
                .iter()
                .any(|e| matches!(e, Entry::Assistant(t) if t == "It went fine.")),
            "run B: missing the retried answer: {entries:?}"
        );
        assert!(
            entries.iter().any(|e| matches!(e, Entry::Outcome(t) if t.starts_with("gwennol: done (EndTurn): 1 round"))),
            "run B outcome: {entries:?}"
        );
    }

    reset_editor(&shared);
    // ---- run C: two stalls, cancelled two ways. The second leaves an
    // uncommitted "draft" in the editor (D9: a cancel never touches
    // it), so the driver clears it with Backspace before typing
    // `/exit` — typed straight onto "draft" it would read as one
    // word, `draft/exit`, and land as a plain turn instead of the exit
    // command, exactly as a real, un-cleared editor would.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Input>();
    let start = shared.lock().entries.len();
    let editor_had_draft = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let driver = {
        let shared = shared.clone();
        let editor_had_draft = editor_had_draft.clone();
        tokio::spawn(async move {
            type_line(&tx, "stall");
            await_ui(
                &shared,
                |ui| matches!(ui.turn, TurnState::Working { .. }),
                "run C: stall working",
            )
            .await;
            assert!(
                frame(&shared).iter().any(|r| r.starts_with("working ")),
                "run C: the status line does not show \"working\" while a turn is running"
            );
            esc(&tx);
            await_ui(
                &shared,
                |ui| outcomes_since(ui, start) >= 1,
                "run C: stall outcome",
            )
            .await;

            let mid = shared.lock().entries.len();
            type_line(&tx, "stream-stall");
            await_ui(
                &shared,
                |ui| {
                    ui.entries[mid..]
                        .iter()
                        .any(|e| matches!(e, Entry::Assistant(t) if t.contains("so far")))
                },
                "run C: stream-stall text shown",
            )
            .await;
            {
                let rows = frame(&shared);
                assert!(
                    rows.iter().any(|r| r.contains("so far")),
                    "run C: the pane does not show the stream-stall's partial text: {rows:?}"
                );
                assert!(
                    rows.iter().any(|r| r.starts_with("streaming ")),
                    "run C: the status line does not show \"streaming\" once text has arrived: {rows:?}"
                );
            }
            for c in "draft".chars() {
                key(&tx, KeyCode::Char(c), KeyModifiers::NONE);
            }
            esc(&tx);
            await_ui(
                &shared,
                |ui| outcomes_since(ui, mid) >= 1,
                "run C: stream-stall outcome",
            )
            .await;
            editor_had_draft.store(
                shared.lock().editor.text() == "draft",
                std::sync::atomic::Ordering::SeqCst,
            );
            for _ in 0.."draft".chars().count() {
                key(&tx, KeyCode::Backspace, KeyModifiers::NONE);
            }
            await_ui(
                &shared,
                |ui| ui.editor.text().is_empty(),
                "run C: draft cleared",
            )
            .await;
            type_line(&tx, "/exit");
        })
    };
    let code = run_drive(session, &shared, &mut rx, None).await;
    driver.await.unwrap();
    assert_eq!(code, ExitCode::SUCCESS, "run C");
    assert!(
        editor_had_draft.load(std::sync::atomic::Ordering::SeqCst),
        "run C: the editor did not keep the cancelled turn's draft text"
    );
    {
        let ui = shared.lock();
        let entries = &ui.entries[start..];
        let first_stall_outcome = entries
            .iter()
            .position(|e| matches!(e, Entry::Outcome(t) if t == "gwennol: cancelled"))
            .expect("run C: first stall's cancelled outcome");
        assert!(
            entries[..first_stall_outcome]
                .iter()
                .all(|e| !matches!(e, Entry::Assistant(_))),
            "run C: the plain stall showed text: {:?}",
            &entries[..first_stall_outcome]
        );
        let second_outcome = entries[first_stall_outcome + 1..]
            .iter()
            .position(|e| matches!(e, Entry::Outcome(t) if t == "gwennol: cancelled"))
            .expect("run C: second stall's cancelled outcome");
        assert!(
            entries[first_stall_outcome + 1..][..second_outcome]
                .iter()
                .any(|e| matches!(e, Entry::Assistant(t) if t.contains("so far"))),
            "run C: the stream-stall's partial text is missing from the pane"
        );
    }
    // Neither cancelled round left an assistant reply in the
    // transcript: the last message is still a user message, and it
    // carries both turns' own text — "stall" and "stream-stall" are
    // one message, since a turn's text is appended to an existing
    // trailing user message rather than opening a new one.
    let last = session.transcript().last().cloned().unwrap();
    assert_eq!(last["role"], "user");
    let texts: Vec<&str> = last["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect();
    assert_eq!(
        texts,
        vec!["stall", "stream-stall"],
        "run C transcript: {last:?}"
    );

    reset_editor(&shared);
    // ---- run D: cancel during a tool call.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Input>();
    let start = shared.lock().entries.len();
    let driver = {
        let shared = shared.clone();
        tokio::spawn(async move {
            type_line(&tx, "sleep");
            await_ui(
                &shared,
                |ui| ui.entries[start..].iter().any(|e| matches!(e, Entry::Trace(t) if t.starts_with("gwennol: -> bash toolu_s1: "))),
                "run D: bash tool call traced",
            )
            .await;
            esc(&tx);
            await_ui(
                &shared,
                |ui| outcomes_since(ui, start) >= 1,
                "run D: outcome",
            )
            .await;
            type_line(&tx, "/exit");
        })
    };
    let began = std::time::Instant::now();
    let code = run_drive(session, &shared, &mut rx, None).await;
    driver.await.unwrap();
    assert!(
        began.elapsed() < Duration::from_secs(10),
        "run D: cancel took too long"
    );
    assert_eq!(code, ExitCode::SUCCESS, "run D");
    {
        let ui = shared.lock();
        let entries = &ui.entries[start..];
        assert!(
            entries.iter().any(|e| matches!(e, Entry::Trace(t) if t.starts_with("gwennol: !! bash toolu_s1: interrupted: the turn was cancelled"))),
            "run D: missing the interrupted trace: {entries:?}"
        );
        assert!(
            entries
                .iter()
                .any(|e| matches!(e, Entry::Outcome(t) if t == "gwennol: cancelled")),
            "run D outcome: {entries:?}"
        );
    }
    let last = session.transcript().last().cloned().unwrap();
    let content = last["content"].as_array().cloned().unwrap_or_default();
    assert!(
        content.iter().any(|b| b
            .get("content")
            .and_then(Value::as_str)
            .is_some_and(|c| c.starts_with("interrupted: the turn was cancelled"))),
        "run D: the transcript's tool_result does not carry the interruption: {last:?}"
    );

    reset_editor(&shared);
    // ---- run E: a failed turn.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Input>();
    let start = shared.lock().entries.len();
    let driver = {
        let shared = shared.clone();
        tokio::spawn(async move {
            type_line(&tx, "fail");
            await_ui(
                &shared,
                |ui| outcomes_since(ui, start) >= 1,
                "run E: outcome",
            )
            .await;
            type_line(&tx, "/exit");
        })
    };
    let code = run_drive(session, &shared, &mut rx, None).await;
    driver.await.unwrap();
    assert_eq!(
        code,
        ExitCode::from(1),
        "run E: /exit after a failed turn is status 1"
    );
    assert!(
        matches!(shared.lock().turn, TurnState::Idle),
        "run E: the status stayed on the failed turn's state after drive returned"
    );
    {
        let ui = shared.lock();
        let entries = &ui.entries[start..];
        assert!(
            entries.iter().any(|e| matches!(e, Entry::Outcome(t) if t.starts_with("gwennol: turn failed: provider refused the turn"))),
            "run E outcome: {entries:?}"
        );
    }

    // ---- run F: slash-command edge cases touch no turn, ending by
    // dropping the sole `tx` (never cloned) to close the channel
    // itself, on purpose this time, rather than only as a
    // panic-recovery side effect.
    reset_editor(&shared);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Input>();
    let requests_before = stub().requests().len();
    let start = shared.lock().entries.len();
    let driver = {
        let shared = shared.clone();
        tokio::spawn(async move {
            type_line(&tx, "/nope x");
            await_ui(
                &shared,
                |ui| ui.notice.as_deref() == Some("unknown command: /nope; /help lists them"),
                "run F: unknown command notice",
            )
            .await;
            assert_eq!(shared.lock().editor.text(), "/nope x");

            tx.send(Input::Paste("/exit\n".to_string())).unwrap();
            await_ui(
                &shared,
                |ui| ui.editor.text() == "/nope x/exit ",
                "run F: paste landed",
            )
            .await;

            key(&tx, KeyCode::End, KeyModifiers::NONE);
            for _ in 0.."/nope x/exit ".chars().count() {
                key(&tx, KeyCode::Backspace, KeyModifiers::NONE);
            }
            await_ui(
                &shared,
                |ui| ui.editor.text().is_empty(),
                "run F: editor cleared",
            )
            .await;

            type_line(&tx, "/help");
            await_ui(
                &shared,
                |ui| ui.entries.len() >= start + 3,
                "run F: help printed",
            )
            .await;
            // Guards the `/help` `commit`, otherwise pinned only by the
            // 180s outer timeout. Mutation: drop the `commit` in the
            // `Command::Help` arm — the editor keeps "/help" and this
            // fails fast instead of three runs downstream, at the 180s
            // timeout.
            assert!(
                shared.lock().editor.text().is_empty(),
                "run F: /help did not clear the editor"
            );
            drop(tx);
        })
    };
    let code = run_drive(session, &shared, &mut rx, None).await;
    driver.await.unwrap();
    assert_eq!(
        code,
        ExitCode::SUCCESS,
        "run F: a closed key source ends the session"
    );
    assert_eq!(
        stub().requests().len(),
        requests_before,
        "run F: no turn reached the stub"
    );
    {
        let ui = shared.lock();
        let help: Vec<&str> = ui.entries[ui.entries.len() - gwennol::tui::ui::HELP.len()..]
            .iter()
            .map(|e| match e {
                Entry::Trace(t) => t.as_str(),
                other => panic!("expected /help's Trace entries, got {other:?}"),
            })
            .collect();
        assert_eq!(help, gwennol::tui::ui::HELP);
    }

    // ---- run G: the key source closing while a turn is running is
    // treated as a running `/exit`: cancelled, then unwound. Distinct
    // from run F (closes the source while idle) and run H below
    // (forces the session out through a second `/exit` before its
    // channel closing would ever matter). A fresh channel, moved (not
    // cloned) into the driver, so it is this run's only sender and
    // closes the moment the driver task ends.
    let start = shared.lock().entries.len();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Input>();
    let driver = tokio::spawn(async move {
        type_line(&tx, "sleep");
        // `tx` drops here, its only owner: the channel closes while
        // the turn `drive` is about to run is still in flight.
    });
    let began = std::time::Instant::now();
    let code = run_drive(session, &shared, &mut rx, None).await;
    driver.await.unwrap();
    assert!(
        began.elapsed() < Duration::from_secs(10),
        "run G: closing the key source mid-turn took too long to unwind"
    );
    assert_eq!(code, ExitCode::SUCCESS, "run G");
    {
        let ui = shared.lock();
        let entries = &ui.entries[start..];
        assert!(
            entries
                .iter()
                .any(|e| matches!(e, Entry::Outcome(t) if t == "gwennol: cancelled")),
            "run G outcome: {entries:?}"
        );
    }

    reset_editor(&shared);
    // Runs F and G each closed their own key source; the last one
    // stays closed, so run H needs a fresh channel.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Input>();

    // ---- run H: a second /exit forces the session out at once. This
    // touches the running loop's `biased;` too, but does not pin it: with
    // `biased;` removed the key and the cancelled turn's own completion
    // race, and the mutant survives most runs (17/20 and 23/25 in two
    // local runs). The forced-exit behaviour itself is pinned
    // deterministically by
    // `a_second_exit_forces_the_session_out_while_the_first_is_pending`;
    // no test kills the running loop's `biased;` mutant reliably — this
    // run catches it only in the minority the figures above leave over
    // (3/20 and 2/25).
    let driver = tokio::spawn(async move {
        type_line(&tx, "sleep");
        type_line(&tx, "/exit");
        type_line(&tx, "/exit");
    });
    let began = std::time::Instant::now();
    let code = run_drive(session, &shared, &mut rx, None).await;
    driver.await.unwrap();
    assert!(
        began.elapsed() < Duration::from_secs(10),
        "run H: forced exit took too long"
    );
    assert_eq!(
        code,
        ExitCode::from(130),
        "run H: a second /exit forces status 130"
    );

    let f = fixture();

    reset_editor(&shared);
    // ---- run I: a write no rule matches opens a prompt; `y` allows
    // it once.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Input>();
    let start = shared.lock().entries.len();
    let driver = {
        let shared = shared.clone();
        tokio::spawn(async move {
            type_line(&tx, "write out.txt");
            await_ui(&shared, prompt_open, "run I: prompt open").await;
            let rows = frame(&shared);
            // The access line's own path is a temp workspace, long
            // enough to wrap (even hard-break) across rows, so the
            // needles below look for "write" and the path's own tail
            // separately rather than one continuous "write {path}"
            // string.
            for needle in [
                TITLE,
                "write",
                "out.txt",
                "asked by tool-write",
                "the model asked write (toolu_s1)",
            ] {
                assert!(
                    rows.iter().any(|r| r.contains(needle)),
                    "run I: missing {needle:?} in {rows:?}"
                );
            }
            for line in pretty_lines(&json!({"path": "out.txt", "content": "hello"})) {
                assert!(
                    rows.iter().any(|r| r.contains(&line)),
                    "run I: missing argument row {line:?} in {rows:?}"
                );
            }
            key(&tx, KeyCode::Char('y'), KeyModifiers::NONE);
            await_ui(
                &shared,
                |ui| outcomes_since(ui, start) >= 1,
                "run I: outcome",
            )
            .await;
            type_line(&tx, "/exit");
        })
    };
    let code = run_drive(session, &shared, &mut rx, None).await;
    driver.await.unwrap();
    assert_eq!(code, ExitCode::SUCCESS, "run I");
    {
        let ui = shared.lock();
        let entries = &ui.entries[start..];
        assert!(
            entries
                .iter()
                .any(|e| matches!(e, Entry::Trace(t) if t.ends_with("allowed at the prompt"))),
            "run I: {entries:?}"
        );
        assert!(
            entries.iter().any(|e| matches!(e, Entry::Trace(t) if t.starts_with("gwennol: <- write toolu_s1: ok, "))),
            "run I: {entries:?}"
        );
        assert!(
            entries.iter().any(
                |e| matches!(e, Entry::Assistant(t) if t.starts_with("It says: wrote 5 bytes"))
            ),
            "run I: {entries:?}"
        );
        assert!(
            entries.iter().any(|e| matches!(e, Entry::Outcome(t) if t.starts_with("gwennol: done (EndTurn): 2 rounds"))),
            "run I: {entries:?}"
        );
        assert!(ui.prompts.is_empty(), "run I: a prompt is still open");
    }
    assert_eq!(
        std::fs::read_to_string(f.workspace.join("out.txt")).unwrap(),
        "hello",
        "run I: out.txt"
    );

    reset_editor(&shared);
    // ---- run J: a write no rule matches, denied once.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Input>();
    let start = shared.lock().entries.len();
    let driver = {
        let shared = shared.clone();
        tokio::spawn(async move {
            type_line(&tx, "write denied.txt");
            await_ui(&shared, prompt_open, "run J: prompt open").await;
            key(&tx, KeyCode::Char('n'), KeyModifiers::NONE);
            await_ui(
                &shared,
                |ui| outcomes_since(ui, start) >= 1,
                "run J: outcome",
            )
            .await;
            type_line(&tx, "/exit");
        })
    };
    let code = run_drive(session, &shared, &mut rx, None).await;
    driver.await.unwrap();
    assert_eq!(code, ExitCode::SUCCESS, "run J");
    {
        let ui = shared.lock();
        let entries = &ui.entries[start..];
        assert!(
            entries
                .iter()
                .any(|e| matches!(e, Entry::Trace(t) if t.ends_with("denied at the prompt"))),
            "run J: {entries:?}"
        );
        assert!(
            entries.iter().any(|e| matches!(e, Entry::Trace(t)
                if t.starts_with("gwennol: !! write toolu_s1: ") && t.contains("operator denied write to"))),
            "run J: {entries:?}"
        );
        assert!(
            entries
                .iter()
                .any(|e| matches!(e, Entry::Assistant(t) if t.starts_with("The read failed: "))),
            "run J: {entries:?}"
        );
    }
    {
        let last_req = stub().requests().last().cloned().unwrap();
        let last_msg = last_req.2["messages"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        let first_block = last_msg["content"][0].clone();
        assert_eq!(first_block["type"], "tool_result", "run J: {first_block:?}");
        assert_eq!(first_block["is_error"], true, "run J: {first_block:?}");
        assert!(
            first_block["content"]
                .as_str()
                .unwrap()
                .contains("operator denied write to"),
            "run J: {first_block:?}"
        );
    }
    assert!(
        !f.workspace.join("denied.txt").exists(),
        "run J: denied.txt was written"
    );

    reset_editor(&shared);
    // ---- run K: a session rule for a spawnless request (read), made
    // with `a`; the same request afterward decides by that rule, with
    // no prompt.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Input>();
    let start = shared.lock().entries.len();
    let outside = f.root.join("outside.txt");
    let driver = {
        let shared = shared.clone();
        let outside = outside.clone();
        tokio::spawn(async move {
            let before_rules = shared.lock().session_rules.len();
            type_line(&tx, &format!("read {}", outside.display()));
            await_ui(&shared, prompt_open, "run K: first prompt open").await;
            let rows = frame(&shared);
            // As in run I: the path is a temp directory, long enough
            // to wrap across rows, so only the file's own name is
            // checked rather than the whole "read {path}" line.
            assert!(
                rows.iter().any(|r| r.contains("outside.txt")),
                "run K: {rows:?}"
            );
            assert!(
                rows.iter().any(|r| r.contains("asked by tool-read")),
                "run K: {rows:?}"
            );
            key(&tx, KeyCode::Char('a'), KeyModifiers::NONE);
            await_ui(
                &shared,
                |ui| outcomes_since(ui, start) >= 1,
                "run K: first outcome",
            )
            .await;
            {
                let ui = shared.lock();
                assert_eq!(
                    ui.session_rules.len(),
                    before_rules + 1,
                    "run K: no session rule was recorded"
                );
                let entries = &ui.entries[start..];
                assert!(
                    entries.iter().any(|e| matches!(e, Entry::Trace(t) if t == &format!(
                        "gwennol: read {} from tool-read (call read toolu_s1): allowed at the prompt for the rest of the session",
                        outside.display()
                    ))),
                    "run K: {entries:?}"
                );
                assert!(
                    entries.iter().any(
                        |e| matches!(e, Entry::Assistant(t) if t.starts_with("It says: outside"))
                    ),
                    "run K: {entries:?}"
                );
            }
            let mid = shared.lock().entries.len();
            type_line(&tx, &format!("read {}", outside.display()));
            // A prompt would hang here for 10s: the mutation this run
            // guards (passing `&[]` for the session slice) makes the
            // request prompt again instead of deciding at once.
            await_ui(
                &shared,
                |ui| outcomes_since(ui, mid) >= 1,
                "run K: second outcome",
            )
            .await;
            {
                let ui = shared.lock();
                let entries = &ui.entries[mid..];
                assert!(
                    entries
                        .iter()
                        .any(|e| matches!(e, Entry::Trace(t) if t.ends_with(&format!(
                            "allowed by session rule read:{} for plugin tool-read",
                            outside.display()
                        )))),
                    "run K: {entries:?}"
                );
                assert!(
                    !entries
                        .iter()
                        .any(|e| matches!(e, Entry::Trace(t) if t.contains("at the prompt"))),
                    "run K: a prompt trace appeared for the remembered rule: {entries:?}"
                );
                assert!(ui.prompts.is_empty(), "run K: a prompt is open");
            }
            type_line(&tx, "/exit");
        })
    };
    let code = run_drive(session, &shared, &mut rx, None).await;
    driver.await.unwrap();
    assert_eq!(code, ExitCode::SUCCESS, "run K");

    reset_editor(&shared);
    // ---- run L: a session rule for a spawn (grep), made with `d`.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Input>();
    let start = shared.lock().entries.len();
    let grep_argv = r#"["grep","-rnI","--exclude-dir=.git","-E","-e","needle","--","."]"#;
    let driver = {
        let shared = shared.clone();
        tokio::spawn(async move {
            type_line(&tx, "grep needle");
            await_ui(&shared, prompt_open, "run L: first prompt open").await;
            let rows = frame(&shared);
            assert!(
                rows.iter()
                    .any(|r| r.contains(&format!("spawn {grep_argv}"))),
                "run L: {rows:?}"
            );
            assert!(
                rows.iter().any(|r| r.contains("asked by tool-grep")),
                "run L: {rows:?}"
            );
            key(&tx, KeyCode::Char('d'), KeyModifiers::NONE);
            await_ui(
                &shared,
                |ui| outcomes_since(ui, start) >= 1,
                "run L: first outcome",
            )
            .await;
            {
                let ui = shared.lock();
                let entries = &ui.entries[start..];
                assert!(
                    entries.iter().any(|e| matches!(e, Entry::Trace(t)
                        if t.ends_with("denied at the prompt for the rest of the session"))),
                    "run L: {entries:?}"
                );
                assert!(
                    entries.iter().any(|e| matches!(e, Entry::Trace(t)
                        if t.starts_with("gwennol: !! grep toolu_s1: ") && t.contains("operator denied spawn of \"grep\""))),
                    "run L: {entries:?}"
                );
            }
            let mid = shared.lock().entries.len();
            type_line(&tx, "grep needle");
            await_ui(
                &shared,
                |ui| outcomes_since(ui, mid) >= 1,
                "run L: second outcome",
            )
            .await;
            {
                let ui = shared.lock();
                let entries = &ui.entries[mid..];
                assert!(
                    entries
                        .iter()
                        .any(|e| matches!(e, Entry::Trace(t) if t.ends_with(&format!(
                            "denied by session rule spawn:{grep_argv} for plugin tool-grep"
                        )))),
                    "run L: {entries:?}"
                );
                assert!(ui.prompts.is_empty(), "run L: a prompt is open");
            }
            type_line(&tx, "/exit");
        })
    };
    let code = run_drive(session, &shared, &mut rx, None).await;
    driver.await.unwrap();
    assert_eq!(code, ExitCode::SUCCESS, "run L");

    reset_editor(&shared);
    // ---- run M: a spawn no session rule can hold (it carries
    // stdin): decided once, remembering nothing, each time.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Input>();
    let start = shared.lock().entries.len();
    let driver = {
        let shared = shared.clone();
        tokio::spawn(async move {
            let before_rules = shared.lock().session_rules.len();
            type_line(&tx, "sh echo from stdin");
            await_ui(&shared, prompt_open, "run M: first prompt open").await;
            let rows = frame(&shared);
            for needle in [
                "spawn [\"sh\"] with 15 bytes on stdin",
                "asked by tool-sh",
                "the child will read this on stdin (15 bytes):",
                "echo from stdin",
            ] {
                assert!(
                    rows.iter().any(|r| r.contains(needle)),
                    "run M: missing {needle:?} in {rows:?}"
                );
            }
            let keys_once_head: String = gwennol::tui::prompt::KEYS_ONCE.chars().take(30).collect();
            assert!(
                rows.iter().any(|r| r.contains(&keys_once_head)),
                "run M: {rows:?}"
            );
            key(&tx, KeyCode::Char('a'), KeyModifiers::NONE);
            await_ui(
                &shared,
                |ui| outcomes_since(ui, start) >= 1,
                "run M: first outcome",
            )
            .await;
            {
                let ui = shared.lock();
                let entries = &ui.entries[start..];
                assert!(
                    entries.iter().any(
                        |e| matches!(e, Entry::Trace(t) if t.ends_with("allowed at the prompt"))
                    ),
                    "run M: {entries:?}"
                );
                assert_eq!(
                    ui.session_rules.len(),
                    before_rules,
                    "run M: a rule was remembered for a stdin spawn"
                );
                assert!(
                    entries
                        .iter()
                        .any(|e| matches!(e, Entry::Assistant(t) if t.contains("from stdin"))),
                    "run M: {entries:?}"
                );
            }
            let mid = shared.lock().entries.len();
            type_line(&tx, "sh echo from stdin");
            await_ui(&shared, prompt_open, "run M: second prompt open").await;
            key(&tx, KeyCode::Char('y'), KeyModifiers::NONE);
            await_ui(
                &shared,
                |ui| outcomes_since(ui, mid) >= 1,
                "run M: second outcome",
            )
            .await;
            type_line(&tx, "/exit");
        })
    };
    let code = run_drive(session, &shared, &mut rx, None).await;
    driver.await.unwrap();
    assert_eq!(code, ExitCode::SUCCESS, "run M");

    reset_editor(&shared);
    // ---- run N: a compiled `deny` from the config file decides
    // outright — no prompt at all.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Input>();
    let start = shared.lock().entries.len();
    let driver = {
        let shared = shared.clone();
        tokio::spawn(async move {
            type_line(&tx, "write forbidden.txt");
            await_ui(
                &shared,
                |ui| outcomes_since(ui, start) >= 1,
                "run N: outcome",
            )
            .await;
            type_line(&tx, "/exit");
        })
    };
    let code = run_drive(session, &shared, &mut rx, None).await;
    driver.await.unwrap();
    assert_eq!(code, ExitCode::SUCCESS, "run N");
    {
        let ui = shared.lock();
        let entries = &ui.entries[start..];
        assert!(
            entries.iter().any(|e| matches!(e, Entry::Trace(t)
                if t.contains("denied by deny \"write:forbidden.txt\" (") && t.contains(" rule 1)"))),
            "run N: {entries:?}"
        );
        assert!(
            entries.iter().any(
                |e| matches!(e, Entry::Trace(t) if t.starts_with("gwennol: !! write toolu_s1: "))
            ),
            "run N: {entries:?}"
        );
        assert!(
            ui.prompts.is_empty(),
            "run N: a prompt opened for a compiled deny"
        );
        let user_at = entries
            .iter()
            .position(|e| matches!(e, Entry::User(_)))
            .expect("run N: this run's User entry");
        assert!(
            !entries[user_at + 1..]
                .iter()
                .any(|e| matches!(e, Entry::Trace(t) if t.contains("at the prompt"))),
            "run N: a prompt trace appeared for a compiled deny: {entries:?}"
        );
    }
    assert!(
        !f.workspace.join("forbidden.txt").exists(),
        "run N: forbidden.txt was written"
    );

    reset_editor(&shared);
    // ---- run O: cancel with a prompt open.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Input>();
    let start = shared.lock().entries.len();
    let driver = {
        let shared = shared.clone();
        tokio::spawn(async move {
            type_line(&tx, "write cancel.txt");
            await_ui(&shared, prompt_open, "run O: prompt open").await;
            assert!(
                frame(&shared).iter().any(|r| r.contains(TITLE)),
                "run O: the box is not shown"
            );
            // `tx` drops here, its only owner: the channel closes
            // while the prompt is still open.
        })
    };
    let began = std::time::Instant::now();
    let code = run_drive(session, &shared, &mut rx, None).await;
    driver.await.unwrap();
    assert!(
        began.elapsed() < Duration::from_secs(10),
        "run O: cancel took too long to unwind"
    );
    assert_eq!(code, ExitCode::SUCCESS, "run O");
    {
        let ui = shared.lock();
        let entries = &ui.entries[start..];
        assert!(
            entries
                .iter()
                .any(|e| matches!(e, Entry::Outcome(t) if t == "gwennol: cancelled")),
            "run O: {entries:?}"
        );
        assert!(
            entries.iter().any(|e| matches!(e, Entry::Trace(t)
                if t.starts_with("gwennol: !! write toolu_s1: interrupted: the turn was cancelled"))),
            "run O: {entries:?}"
        );
        assert!(ui.prompts.is_empty(), "run O: the prompt is still open");
    }
    assert!(
        !frame(&shared).iter().any(|r| r.contains(TITLE)),
        "run O: the box is still shown after drive returned"
    );
    let last = session.transcript().last().cloned().unwrap();
    let content = last["content"].as_array().cloned().unwrap_or_default();
    assert!(
        content.iter().any(|b| b
            .get("content")
            .and_then(Value::as_str)
            .is_some_and(|c| c.starts_with("interrupted: the turn was cancelled"))),
        "run O: the transcript's tool_result does not carry the interruption: {last:?}"
    );
    assert!(
        !f.workspace.join("cancel.txt").exists(),
        "run O: cancel.txt was written"
    );
}
