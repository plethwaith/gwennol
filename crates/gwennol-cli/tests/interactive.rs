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
use gwennol::tui::screen::{Kitty, Screen};
use gwennol::tui::ui::{Entry, Shared, TurnState, Ui, render};
use gwennol::{Cli, frontend, ordered_rule_flags, tui};
use gwennol_core::Session;
use provider_anthropic::PLUGIN_NAME as PROVIDER;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::Value;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

// ------------------------------------------------------------- fixture

struct Fixture {
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

        let stub = stub();
        let key_file = root.join("anthropic.key");
        std::fs::write(&key_file, API_KEY).unwrap();

        let config = root.join("scripted.toml");
        std::fs::write(
            &config,
            format!(
                "[plugins]\ndir = {plugins:?}\ntrust_runtimes = [{provider:?}]\n\n\
                 [plugin_config.{provider}]\nmodel = \"claude-fixture\"\nbase_url = \"http://{addr}/scripted\"\n",
                plugins = plugins.display().to_string(),
                provider = PROVIDER,
                addr = stub.addr,
            ),
        )
        .unwrap();

        Fixture {
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
/// several `drive` calls (D12), and `/exit` deliberately leaves the
/// editor as it is (D9): un-cleared, the next run's typed text lands
/// after it and reads as one unknown command instead of a turn.
fn reset_editor(shared: &std::sync::Arc<Shared>) {
    shared.update(|ui| {
        ui.editor = gwennol::tui::editor::Editor::default();
        ui.notice = None;
    });
}

/// How many `Outcome` entries `ui.entries[start..]` holds: one per
/// turn that has finished, the sync point a fast, all-local stub makes
/// necessary (a turn can complete before the driver task is even
/// scheduled, so waiting on `TurnState::Idle` alone can observe a
/// *later* turn's idle instead of the one just submitted).
fn outcomes_since(ui: &Ui, start: usize) -> usize {
    ui.entries[start..]
        .iter()
        .filter(|e| matches!(e, Entry::Outcome(_)))
        .count()
}

/// The `text` field of every `assistant`-role message from `from`
/// onward, concatenated: what the transcript holds for the completed
/// turns since a point, to compare against `Ui::assistant_text_since`.
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
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Input>();

    // ---- run A: two turns, then an idle /exit.
    let start = shared.lock().entries.len();
    let driver = {
        let shared = shared.clone();
        let tx = tx.clone();
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
        // since that turn's user message (`transcript()[1..4]`, as
        // planned: index 0 is the user message itself).
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
    let start = shared.lock().entries.len();
    let driver = {
        let shared = shared.clone();
        let tx = tx.clone();
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
    // unknown word, `draft/exit`, and land as a plain turn instead of
    // the exit command, exactly as a real, un-cleared editor would.
    let start = shared.lock().entries.len();
    let editor_had_draft = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let driver = {
        let shared = shared.clone();
        let tx = tx.clone();
        let editor_had_draft = editor_had_draft.clone();
        tokio::spawn(async move {
            type_line(&tx, "stall");
            await_ui(
                &shared,
                |ui| matches!(ui.turn, TurnState::Working { .. }),
                "run C: stall working",
            )
            .await;
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
    // Neither cancelled round left a trace in the transcript: the last
    // message is still the "stall" turn's own user text, unaccompanied
    // by any assistant reply, and "stream-stall" never sent one either
    // (both are one message, since a turn's own text is appended to an
    // existing trailing user message rather than opening a new one).
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
    let start = shared.lock().entries.len();
    let driver = {
        let shared = shared.clone();
        let tx = tx.clone();
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
    let start = shared.lock().entries.len();
    let driver = {
        let shared = shared.clone();
        let tx = tx.clone();
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
    {
        let ui = shared.lock();
        let entries = &ui.entries[start..];
        assert!(
            entries.iter().any(|e| matches!(e, Entry::Outcome(t) if t.starts_with("gwennol: turn failed: provider refused the turn"))),
            "run E outcome: {entries:?}"
        );
    }

    // ---- run F: slash-command edge cases touch no turn. `tx` (not a
    // clone) moves into the driver so dropping it there actually
    // closes the channel: every other clone has already gone out of
    // scope with the runs that made it.
    reset_editor(&shared);
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

    // A closed key source ends this test: later runs need a fresh
    // channel.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Input>();

    // ---- run G: a second /exit forces the session out at once.
    let driver = {
        let tx = tx.clone();
        tokio::spawn(async move {
            type_line(&tx, "sleep");
            type_line(&tx, "/exit");
            type_line(&tx, "/exit");
        })
    };
    let began = std::time::Instant::now();
    let code = run_drive(session, &shared, &mut rx, None).await;
    driver.await.unwrap();
    assert!(
        began.elapsed() < Duration::from_secs(10),
        "run G: forced exit took too long"
    );
    assert_eq!(
        code,
        ExitCode::from(130),
        "run G: a second /exit forces status 130"
    );

    let _ = frame(&shared); // exercised at least once, as tests/interactive.rs's rendering pin.
}
