//! Rendering that does not depend on which `Operator` is in
//! charge, so a trace line and a prompt say the same thing in
//! the same words: an access in the words a prompt would use,
//! with the URL's userinfo, query and fragment scrubbed because
//! the screen or the trace is a record and a rule judged the full
//! URL; a tool call as name and id; a bounded one-line preview;
//! the outcome line and the exit status it decides; and the trace's
//! lines: a decision, a call, its result, its failure, a retry.

use std::fmt;
use std::path::Path;
use std::process::ExitCode;

use gwennol_core::{Access, ApprovalRequest, Failure, ToolCall, TurnError, TurnOutcome};

use crate::policy::Judgement;
use crate::{EXIT_CANCELLED, EXIT_TURN_FAILED};

/// Most characters of a tool call's arguments, or of a result shown
/// as one line rather than whole, so a trace and a prompt cut to
/// the same length.
pub const PREVIEW_CHARS: usize = 200;

/// An `Access` on one line, in the words a prompt would use: a URL's
/// userinfo, query and fragment cut. A query or a fragment (either
/// sets it) writes the `?…` marker; userinfo writes the
/// `(credentials in the URL cut)` marker.
pub struct ShowAccess<'a> {
    /// The access to show.
    pub access: &'a Access,
    /// The canonical workspace: a spawn's cwd is shown only when it
    /// differs.
    pub workspace: &'a Path,
}

impl fmt::Display for ShowAccess<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.access {
            Access::ReadFile(p) => write!(f, "read {}", p.display()),
            Access::WriteFile(p) => write!(f, "write {}", p.display()),
            Access::ListDir(p) => write!(f, "list {}", p.display()),
            Access::Spawn { argv, cwd, stdin } => {
                // argv as a JSON array: unambiguous about where each
                // argument ends, which a space-joined line is not.
                write!(f, "spawn {}", serde_json::Value::from(argv.clone()))?;
                if cwd != self.workspace {
                    write!(f, " in {}", cwd.display())?;
                }
                if let Some(stdin) = stdin {
                    write!(f, " with {} bytes on stdin", stdin.len())?;
                }
                Ok(())
            }
            Access::Http { method, url } => match url::Url::parse(url) {
                Ok(mut u) => {
                    let had_userinfo = !u.username().is_empty() || u.password().is_some();
                    let had_query = u.query().is_some() || u.fragment().is_some();
                    gwennol_core::steps::http::scrub(&mut u);
                    write!(f, "{method} {u}")?;
                    // Say that something was cut, without saying what.
                    if had_query {
                        f.write_str("?…")?;
                    }
                    if had_userinfo {
                        f.write_str(" (credentials in the URL cut)")?;
                    }
                    Ok(())
                }
                Err(_) => write!(f, "{method} request (unparseable URL)"),
            },
            _ => f.write_str("an access this frontend does not know"),
        }
    }
}

/// A tool call as the trace names it.
pub struct ShowCall<'a>(pub &'a ToolCall);

impl fmt::Display for ShowCall<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0.id {
            Some(id) => write!(f, "{} {id}", self.0.name),
            None => f.write_str(&self.0.name),
        }
    }
}

/// The first `PREVIEW_CHARS` of `text`, on one line.
pub fn preview(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    for c in chars.by_ref().take(PREVIEW_CHARS) {
        out.push(if c == '\n' { ' ' } else { c });
    }
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

/// An approval decision, in the words `Headless::approve` used to print
/// itself: the access, who asked, the call it was made for (if any),
/// and the judgement.
pub fn decision(request: &ApprovalRequest, judgement: &Judgement<'_>, workspace: &Path) -> String {
    let cause = match &request.cause {
        Some(call) => format!(" (call {})", ShowCall(call)),
        None => String::new(),
    };
    format!(
        "{} from {}{cause}: {judgement}",
        ShowAccess {
            access: &request.access,
            workspace,
        },
        request.plugin,
    )
}

/// A tool call about to run.
pub fn tool_call(call: &ToolCall) -> String {
    format!("-> {}: {}", ShowCall(call), preview(&call.arguments))
}

/// A tool's result: whole at `-v` and above, a one-line preview
/// otherwise.
pub fn tool_result(call: &ToolCall, content: &str, is_error: bool, verbosity: u8) -> String {
    let verdict = if is_error { "error" } else { "ok" };
    let mut out = format!("<- {}: {verdict}, {} bytes", ShowCall(call), content.len());
    if verbosity >= 1 {
        for line in content.lines() {
            out.push_str(&format!("\n    {line}"));
        }
    } else if !content.is_empty() {
        out.push_str(&format!("\n    {}", preview(content)));
    }
    out
}

/// A call that got no answer.
pub fn tool_failed(call: &ToolCall, error: &str) -> String {
    format!("!! {}: {error}", ShowCall(call))
}

/// A retried round: the provider's failure and which attempt is next.
pub fn retry(attempt: u32, max_attempts: u32, failure: &Failure) -> String {
    format!("provider failure, retrying ({attempt}/{max_attempts}): {failure}")
}

/// The outcome line and the exit status it decides. A cancelled turn
/// prints the error's own text: `cancelled`, or `cancelled; the
/// stream's source failed with: …` when the cut hid a failure.
pub fn outcome_line(outcome: &Result<TurnOutcome, TurnError>) -> (String, ExitCode) {
    match outcome {
        Ok(outcome) => (
            format!(
                "gwennol: done ({:?}): {} round{}, {} tokens in, {} out",
                outcome.stop_reason,
                outcome.rounds,
                if outcome.rounds == 1 { "" } else { "s" },
                outcome.usage.input_tokens,
                outcome.usage.output_tokens
            ),
            ExitCode::SUCCESS,
        ),
        Err(e @ TurnError::Cancelled { .. }) => {
            (format!("gwennol: {e}"), ExitCode::from(EXIT_CANCELLED))
        }
        Err(e) => (
            format!("gwennol: turn failed: {e}"),
            ExitCode::from(EXIT_TURN_FAILED),
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn previews_are_one_line_and_bounded() {
        assert_eq!(preview("a\nb"), "a b");
        let long: String = "x".repeat(PREVIEW_CHARS + 5);
        let p = preview(&long);
        assert_eq!(p.chars().count(), PREVIEW_CHARS + 1);
        assert!(p.ends_with('…'));
        // Exactly the cap is shown whole.
        let exact: String = "y".repeat(PREVIEW_CHARS);
        assert_eq!(preview(&exact), exact);
    }

    #[test]
    fn the_trace_shows_what_a_prompt_would_have() {
        let ws = Path::new("/ws");
        let show = |access: &Access| {
            ShowAccess {
                access,
                workspace: ws,
            }
            .to_string()
        };
        assert_eq!(
            show(&Access::ReadFile(PathBuf::from("/ws/a.txt"))),
            "read /ws/a.txt"
        );
        assert_eq!(
            show(&Access::Spawn {
                argv: vec!["bash".into(), "-c".into(), "echo a b".into()],
                cwd: PathBuf::from("/ws"),
                stdin: None,
            }),
            r#"spawn ["bash","-c","echo a b"]"#
        );
        assert_eq!(
            show(&Access::Spawn {
                argv: vec!["sh".into()],
                cwd: PathBuf::from("/elsewhere"),
                stdin: Some("exit 1\n".into()),
            }),
            r#"spawn ["sh"] in /elsewhere with 7 bytes on stdin"#
        );
        assert_eq!(
            show(&Access::Http {
                method: "POST".into(),
                url: "https://api.anthropic.com/v1/messages".into(),
            }),
            "POST https://api.anthropic.com/v1/messages"
        );
        // Userinfo and the query string can carry a credential: the
        // rule judged them, the trace does not repeat them.
        let shown = show(&Access::Http {
            method: "GET".into(),
            url: "https://user:hunter2@x.example/p?key=sk-secret#frag".into(),
        });
        assert_eq!(
            shown,
            "GET https://x.example/p?… (credentials in the URL cut)"
        );
        assert!(!shown.contains("hunter2") && !shown.contains("sk-secret"));
        assert!(!shown.contains("user"));
    }

    #[test]
    fn the_outcome_line_carries_a_failure_the_cut_hid() {
        use gwennol_core::{StopReason, Usage};

        let (line, code) = outcome_line(&Ok(TurnOutcome {
            stop_reason: StopReason::EndTurn,
            rounds: 1,
            usage: Usage {
                input_tokens: 3,
                output_tokens: 4,
            },
        }));
        assert_eq!(
            (line.as_str(), code),
            (
                "gwennol: done (EndTurn): 1 round, 3 tokens in, 4 out",
                ExitCode::SUCCESS
            )
        );

        let (line, code) = outcome_line(&Err(TurnError::Cancelled { detail: None }));
        assert_eq!(
            (line.as_str(), code),
            ("gwennol: cancelled", ExitCode::from(EXIT_CANCELLED))
        );

        let (line, code) = outcome_line(&Err(TurnError::Cancelled {
            detail: Some("no response data for 500ms".into()),
        }));
        assert_eq!(
            (line.as_str(), code),
            (
                "gwennol: cancelled; the stream's source failed with: no response data for 500ms",
                ExitCode::from(EXIT_CANCELLED)
            )
        );

        let (line, code) = outcome_line(&Err(TurnError::StreamEnded));
        assert_eq!(
            (line.as_str(), code),
            (
                "gwennol: turn failed: the stream ended before the turn did; the cause was lost",
                ExitCode::from(EXIT_TURN_FAILED)
            )
        );
    }
}
