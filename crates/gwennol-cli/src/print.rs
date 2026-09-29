//! Print mode: one task, one turn, the model's text on stdout, the
//! trace on stderr, no terminal needed. Every approval is decided by a
//! rule, as a session decides it; Ctrl-C cancels the turn, once, and a
//! second Ctrl-C exits at once. With `--trace` the trace is also written
//! to a file.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex, PoisonError};

use gwennol_core::gwead::tokio_util::sync::CancellationToken;

use crate::operator::Headless;
use crate::policy::RuleSpec;
use crate::record::{self, TraceFile};
use crate::show::outcome_line;
use crate::{Cli, EXIT_CANCELLED, Fatal, Mode, frontend};

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

    // ---- the trace file, before anything boots: a path that cannot be
    // written is a startup error, not a run that traces nothing.
    let trace = cli
        .trace
        .as_deref()
        .map(TraceFile::create)
        .transpose()?
        .map(|trace| Arc::new(Mutex::new(trace)));

    let mut session = frontend::start(
        &cli,
        workspace,
        flag_rules,
        Mode::Print,
        |policy, secrets, workspace| {
            Arc::new(
                Headless::new(
                    policy,
                    secrets.clone(),
                    workspace.to_path_buf(),
                    cli.verbose,
                )
                .with_trace(trace.clone()),
            )
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
    record::say(trace.as_ref(), &line);
    // After the outcome is reported, so a transcript that cannot be
    // written never hides how the turn went. It is still a failure of
    // what was asked for, but the turn's own failure or cancellation
    // is the more important fact, and a wrapper keying on that status
    // must keep seeing it: `settle` keeps a 1 or a 130.
    let mut transcript_failed = false;
    if let Some(path) = &cli.transcript
        && let Err(Fatal(message)) = record::write_transcript(path, &session.chat_input())
    {
        transcript_failed = true;
        record::say(trace.as_ref(), &format!("gwennol: {message}"));
    }
    let trace_failed = trace.as_ref().is_some_and(|trace| {
        trace
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .failed()
    });
    Ok(record::settle(code, transcript_failed || trace_failed))
}
