//! The interactive frontend: a terminal session with streamed output,
//! a line editor, and cancel. `screen` guards the terminal's raw mode,
//! alternate screen and kitty keyboard flag, restoring them on every
//! way out, a panic included; `keys` turns crossterm's events into the
//! frontend's own `Input`, so the loop never touches the terminal
//! directly; `editor` is the line editor: history, word navigation,
//! bracketed paste, slash commands; `ui` holds the pane's entries, the
//! turn's state and the editor, and maps loop [`gwennol_core::Event`]s
//! onto them; `operator` is the [`gwennol_core::Operator`] that turns
//! approvals and events into pane updates; `drive` is the loop itself,
//! driving [`gwennol_core::agent::Session::turn`] one turn at a time so
//! a failed or cancelled turn leaves the session running rather than
//! ending it, as [`gwennol_core::agent::Session::run`] would; `prompt`
//! is the approval prompt a request no rule decides opens in the
//! pane; `pane` is the pane's own keys: paging, and focusing a tool
//! call or result to expand it.

pub mod drive;
pub mod editor;
pub mod keys;
pub mod operator;
pub mod pane;
pub mod prompt;
pub mod screen;
pub mod ui;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use gwennol_core::Session;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::policy::RuleSpec;
use crate::record::{self, TraceFile};
use crate::{Cli, Fatal, Mode, frontend};
use drive::drive;
use keys::TerminalKeys;
use operator::Interactive;
use screen::{Kitty, Screen};
use ui::{Entry, Shared};

/// Boot for a session: `frontend::start` with the interactive operator,
/// the startup warnings as the pane's first entries. No terminal state
/// is touched: a startup error prints to a normal terminal. Both record
/// files (`--transcript`, `--trace`) are created before anything boots,
/// so a path that cannot be written is a startup error; the transcript
/// then holds the conversation as the provider sees it, from the start.
pub fn start(
    cli: &Cli,
    workspace: PathBuf,
    flag_rules: Vec<RuleSpec>,
) -> Result<(Session, Arc<Shared>), Fatal> {
    let trace = cli.trace.as_deref().map(TraceFile::create).transpose()?;
    if let Some(path) = &cli.transcript {
        record::create(path).map_err(|e| Fatal(format!("transcript {}: {e}", path.display())))?;
    }
    let shared = Shared::new();
    shared.update(|ui| {
        ui.workspace = workspace.clone();
        ui.trace = trace;
        ui.transcript = cli.transcript.clone();
    });
    let mut warnings = Vec::new();
    let session = frontend::start(
        cli,
        workspace,
        flag_rules,
        Mode::Interactive,
        |policy, secrets, workspace| {
            Arc::new(Interactive::new(
                policy,
                secrets.clone(),
                workspace.to_path_buf(),
                cli.verbose,
                shared.clone(),
            ))
        },
        &mut warnings,
    )?;
    if let Some(path) = &cli.transcript {
        record::write_transcript(path, &session.chat_input())?;
    }
    shared.update(|ui| {
        for w in warnings {
            ui.push(Entry::Trace(format!("gwennol: {w}")));
        }
    });
    Ok((session, shared))
}

/// The session: start, then the terminal and the loop, then the record
/// failures once the terminal is back.
pub async fn run(
    cli: Cli,
    workspace: PathBuf,
    flag_rules: Vec<RuleSpec>,
) -> Result<ExitCode, Fatal> {
    let (mut session, shared) = start(&cli, workspace, flag_rules)?;
    let code = on_terminal(&mut session, &shared, cli.first_turn()).await;
    finish(code, &shared, &mut std::io::stderr())
}

/// The loop on the terminal: raw mode and the alternate screen are
/// entered here and restored when this returns, whichever way it does.
async fn on_terminal(
    session: &mut Session,
    shared: &Arc<Shared>,
    first: Option<String>,
) -> Result<ExitCode, Fatal> {
    screen::install_panic_hook();
    let screen = Screen::enter(std::io::stdout(), true, Kitty::Probe)
        .map_err(|e| Fatal(format!("terminal setup: {e}")))?;
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))
        .map_err(|e| Fatal(format!("terminal: {e}")))?;
    let mut keys = TerminalKeys::new();
    let code = drive(session, shared, &mut terminal, &mut keys, first).await;
    drop(terminal);
    drop(screen);
    code
}

/// What is left to say once the terminal is back: each record failure
/// the session kept is written to `err`, where it outlives the
/// alternate screen, before an error `code` is returned as it is; a
/// `code` of success becomes exit 2 when any failed ([`record::settle`]).
fn finish(
    code: Result<ExitCode, Fatal>,
    shared: &Shared,
    err: &mut impl std::io::Write,
) -> Result<ExitCode, Fatal> {
    let failures = shared.lock().record_failures.clone();
    for message in &failures {
        let _ = writeln!(err, "gwennol: {message}");
    }
    code.map(|code| record::settle(code, !failures.is_empty()))
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, FromArgMatches};

    use super::*;

    /// The committed provider and edit-tool plugins' own `name` fields
    /// (`plugins/providers/anthropic.json`, `plugins/tools/edit.json`).
    /// Neither guest crate is a dev-dependency of this binary — see
    /// `gwennol-cli/Cargo.toml`.
    const PROVIDER: &str = "provider-anthropic";
    const EDIT: &str = "tool-edit";

    /// `tui::start` shows every startup warning it would otherwise
    /// only log, as the pane's first entries — except the no-rules
    /// one (D6): a session asks at a prompt for what no rule
    /// decides, so "every request will be denied" is false there,
    /// and it goes only to the log. This is the only unit test in
    /// this crate that boots the host (`gwennol_core::host`'s
    /// `OnceLock` installs once per process, and `boot_with` fails
    /// every call after the first with `BootError::AlreadyInstalled`;
    /// nothing else here reaches it). An explicit `--secret` rule
    /// points the lookup at an environment variable this test names
    /// and never sets, rather than the convention variable
    /// `GWENNOL_SECRET_PROVIDER_ANTHROPIC_API_KEY`, which a
    /// developer's own shell may have set for real use — a rule
    /// always wins over the convention variable (`secrets.rs`'s
    /// `source_for`), so this is isolated from the machine's
    /// environment regardless. Guards the no-rules warning going only
    /// to the log in a session, not the pane. Mutation: restore the
    /// `warnings.push` in `frontend.rs` — two entries instead of one.
    /// With `--trace`, the warning is also the trace file's first
    /// line, because `start` gives the pane its trace before it pushes
    /// the warnings. Mutation: move `ui.trace = trace;` after the
    /// warnings loop — the file is empty.
    #[test]
    fn startup_warnings_are_the_first_entries() {
        let root = tempfile::tempdir().unwrap();
        let bundled = xtask::bundle(&xtask::workspace_root())
            .unwrap_or_else(|e| panic!("bundling plugins/ failed: {e}"));
        let bundle_root = root.path().join("bundle");
        xtask::write_bundle(&bundled, &bundle_root).unwrap();
        let plugins_dir = bundle_root.join(xtask::PLUGINS_DIR);

        let config_path = root.path().join("empty-config.toml");
        std::fs::write(&config_path, "").unwrap();

        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let workspace = workspace.canonicalize().unwrap();

        let matches = Cli::command().get_matches_from([
            "gwennol",
            "--plugins",
            plugins_dir.to_str().unwrap(),
            "--trust-runtime",
            PROVIDER,
            "--trust-runtime",
            EDIT,
            "--config",
            config_path.to_str().unwrap(),
            "--secret",
            &format!("{PROVIDER}:api_key=env:GWENNOL_TEST_STARTUP_WARNING_38_UNSET"),
            "--trace",
            root.path().join("t.log").to_str().unwrap(),
        ]);
        let cli = Cli::from_arg_matches(&matches).unwrap();
        // No --allow/--deny rule at all: the empty-policy warning
        // fires to the log alone, per D6, and not into the pane.
        let (_session, shared) = start(&cli, workspace, Vec::new()).unwrap();
        let guard = shared.lock();
        assert_eq!(guard.entries.len(), 1, "{:?}", guard.entries);
        match &guard.entries[0] {
            Entry::Trace(text) => assert!(
                text.starts_with(
                    "gwennol: plugin provider-anthropic declares secret \"api_key\" \
                     but no source has it: set environment variable \
                     GWENNOL_TEST_STARTUP_WARNING_38_UNSET"
                ),
                "{text}"
            ),
            other => panic!("expected a Trace entry, got {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(root.path().join("t.log")).unwrap(),
            format!("{}\n", guard.entries[0].text()),
            "the startup warning is in the trace file"
        );
    }

    /// `tui::start` creates both record files before it boots anything,
    /// so a path that cannot be written is a startup error naming it.
    /// `--plugins` points at a directory that does not exist, so a
    /// missing pre-boot check falls through to the plugins error and
    /// never reaches a boot. Mutations: remove the pre-boot `create` of
    /// either file: the message is then the plugins one.
    #[test]
    fn a_session_reports_an_unwritable_record_before_it_boots() {
        for (flag, path, prefix) in [
            (
                "--transcript",
                "/nonexistent/dir/t.json",
                "transcript /nonexistent/dir/t.json: ",
            ),
            (
                "--trace",
                "/nonexistent/dir/t.log",
                "trace /nonexistent/dir/t.log: ",
            ),
        ] {
            let matches = Cli::command().get_matches_from([
                "gwennol",
                "--plugins",
                "/nonexistent/plugins",
                flag,
                path,
            ]);
            let cli = Cli::from_arg_matches(&matches).unwrap();
            match start(&cli, PathBuf::from("."), Vec::new()) {
                Err(Fatal(message)) => {
                    assert!(message.starts_with(prefix), "{flag}: {message}")
                }
                Ok(_) => panic!("{flag}: expected a startup error"),
            }
        }
    }

    /// A record failure kept before the terminal could be entered is
    /// still said, ahead of the terminal error that ends the run, and a
    /// clean run becomes exit 2. Mutation: return an error `code`
    /// before reading `record_failures` (nothing is said).
    #[test]
    fn kept_record_failures_are_said_even_when_the_terminal_fails() {
        let shared = Shared::new();
        shared.update(|ui| ui.record_failures.push("trace t.log: boom".to_string()));
        let mut said = Vec::new();
        let code = finish(
            Err(Fatal("terminal setup: no tty".to_string())),
            &shared,
            &mut said,
        );
        assert_eq!(
            String::from_utf8(said).unwrap(),
            "gwennol: trace t.log: boom\n"
        );
        assert!(
            matches!(&code, Err(Fatal(m)) if m == "terminal setup: no tty"),
            "{code:?}"
        );

        let mut said = Vec::new();
        let code = finish(Ok(ExitCode::SUCCESS), &shared, &mut said);
        assert_eq!(code.unwrap(), ExitCode::from(crate::EXIT_USAGE));
        assert_eq!(
            String::from_utf8(said).unwrap(),
            "gwennol: trace t.log: boom\n"
        );
    }
}
