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
use crate::{Cli, Fatal, frontend};
use drive::drive;
use keys::TerminalKeys;
use operator::Interactive;
use screen::{Kitty, Screen};
use ui::{Entry, Shared};

/// Boot for a session: `frontend::start` with the interactive operator,
/// the startup warnings as the pane's first entries. No terminal state
/// is touched: a startup error prints to a normal terminal. `--transcript`
/// is print mode only (`lib.rs`'s help says so): a session rejects it
/// here rather than silently writing nothing.
pub fn start(
    cli: &Cli,
    workspace: PathBuf,
    flag_rules: Vec<RuleSpec>,
) -> Result<(Session, Arc<Shared>), Fatal> {
    if cli.transcript.is_some() {
        return Err(Fatal(
            "--transcript is print-mode only (-p); a session does not write one".to_string(),
        ));
    }
    let shared = Shared::new();
    shared.update(|ui| ui.workspace = workspace.clone());
    let mut warnings = Vec::new();
    let session = frontend::start(
        cli,
        workspace,
        flag_rules,
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
    shared.update(|ui| {
        for w in warnings {
            ui.push(Entry::Trace(format!("gwennol: {w}")));
        }
    });
    Ok((session, shared))
}

/// The session: start, then the terminal, the loop, and the restore.
pub async fn run(
    cli: Cli,
    workspace: PathBuf,
    flag_rules: Vec<RuleSpec>,
) -> Result<ExitCode, Fatal> {
    let (mut session, shared) = start(&cli, workspace, flag_rules)?;
    screen::install_panic_hook();
    let screen = Screen::enter(std::io::stdout(), true, Kitty::Probe)
        .map_err(|e| Fatal(format!("terminal setup: {e}")))?;
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))
        .map_err(|e| Fatal(format!("terminal: {e}")))?;
    let mut keys = TerminalKeys::new();
    let code = drive(
        &mut session,
        &shared,
        &mut terminal,
        &mut keys,
        cli.first_turn(),
    )
    .await;
    drop(terminal);
    drop(screen);
    code
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, FromArgMatches};

    use super::*;

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
            provider_anthropic::PLUGIN_NAME,
            "--config",
            config_path.to_str().unwrap(),
            "--secret",
            &format!(
                "{}:api_key=env:GWENNOL_TEST_STARTUP_WARNING_38_UNSET",
                provider_anthropic::PLUGIN_NAME
            ),
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
    }

    /// `tui::start` rejects `--transcript` outright rather than
    /// accepting it and silently writing nothing, as a session did
    /// before this round (`lib.rs`'s help now says print-mode only).
    /// The check runs before the plugin bundle would be needed, so
    /// this needs none. Mutation: drop the `cli.transcript.is_some()`
    /// check — `start` then fails on the missing bundle instead, with
    /// no mention of `--transcript`.
    #[test]
    fn a_session_rejects_transcript() {
        let matches = Cli::command().get_matches_from(["gwennol", "--transcript", "/tmp/x"]);
        let cli = Cli::from_arg_matches(&matches).unwrap();
        match start(&cli, PathBuf::from("."), Vec::new()) {
            Err(Fatal(message)) => assert!(
                message.contains("--transcript"),
                "wrong rejection reason: {message}"
            ),
            Ok(_) => panic!("expected --transcript to be rejected"),
        }
    }
}
