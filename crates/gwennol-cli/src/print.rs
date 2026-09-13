//! Print mode: one task, one turn, the model's text on stdout, the
//! trace on stderr, no terminal needed. Every approval is decided by a
//! rule, as a session decides it; Ctrl-C cancels the turn, once, and a
//! second Ctrl-C exits at once.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use gwennol_core::gwead::tokio_util::sync::CancellationToken;
use serde_json::Value;

use crate::operator::Headless;
use crate::policy::RuleSpec;
use crate::show::outcome_line;
use crate::{Cli, EXIT_CANCELLED, Fatal, frontend};

/// Run the one task in `cli.prompt` (or stdin) to completion.
pub async fn run(
    cli: Cli,
    workspace: PathBuf,
    flag_rules: Vec<RuleSpec>,
) -> Result<ExitCode, Fatal> {
    // ---- the task, before anything expensive: an empty one, or a
    // closed stdin, should not cost a kernel boot to find out.
    let task = match cli.prompt.as_deref() {
        Some("-") | None => {
            let mut text = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)
                .map_err(|e| Fatal(format!("reading the task from stdin: {e}")))?;
            text
        }
        Some(text) => text.to_string(),
    };
    if task.trim().is_empty() {
        return Err(Fatal("the task is empty".into()));
    }

    let mut session = frontend::start(
        &cli,
        workspace,
        flag_rules,
        |policy, secrets, workspace| {
            Arc::new(Headless::new(
                policy,
                secrets.clone(),
                workspace.to_path_buf(),
                cli.verbose,
            ))
        },
        &mut Vec::new(),
    )?;

    // ---- one turn, cancellable.
    let cancel = CancellationToken::new();
    {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                eprintln!("gwennol: interrupted; cancelling the turn");
                cancel.cancel();
            }
            if tokio::signal::ctrl_c().await.is_ok() {
                eprintln!("gwennol: interrupted again; exiting");
                std::process::exit(i32::from(EXIT_CANCELLED));
            }
        });
    }
    let outcome = session.turn(&task, &cancel).await;
    let (line, code) = outcome_line(&outcome);
    eprintln!("{line}");
    // After the outcome is reported, so a transcript that cannot be
    // written never hides how the turn went. It is still a failure of
    // what was asked for — but the turn's own failure or cancellation
    // is the more important fact, and a wrapper keying on that status
    // must keep seeing it.
    if let Some(path) = &cli.transcript
        && let Err(Fatal(message)) = write_transcript(path, &session.chat_input())
    {
        eprintln!("gwennol: {message}");
        if code == ExitCode::SUCCESS {
            return Ok(ExitCode::from(crate::EXIT_USAGE));
        }
    }
    Ok(code)
}

/// The whole chat input, pretty-printed: what the provider was handed
/// on the last round plus its answer, so the file is a request someone
/// can read or replay, not just the messages.
fn write_transcript(path: &Path, chat_input: &Value) -> Result<(), Fatal> {
    let text = serde_json::to_string_pretty(chat_input).expect("a Value serialises");
    std::fs::write(path, text).map_err(|e| Fatal(format!("transcript {}: {e}", path.display())))
}
