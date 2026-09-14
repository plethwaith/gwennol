//! Rendering that does not depend on which `Operator` is in
//! charge, so a trace line and a prompt say the same thing in
//! the same words: an access in the words a prompt would use,
//! with the URL's userinfo, query and fragment scrubbed because
//! the screen or the trace is a record and a rule judged the full
//! URL, and the exact subject a session rule is made of; a tool
//! call as name and id; a bounded one-line preview; the outcome
//! line and the exit status it decides; and the trace's lines: a
//! decision, a call, its result, its failure, a retry.

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
                write!(f, "spawn {}", spawn_subject(argv, cwd, self.workspace))?;
                if let Some(stdin) = stdin {
                    write!(f, " with {} bytes on stdin", stdin.len())?;
                }
                Ok(())
            }
            Access::Http { method, url } => match http_subject(method, url) {
                Some(s) => f.write_str(&s),
                None => write!(f, "{method} request (unparseable URL)"),
            },
            _ => f.write_str("an access this frontend does not know"),
        }
    }
}

/// The argv as a JSON array — unambiguous about where each argument
/// ends, which a space-joined line is not — plus ` in {cwd}` when the
/// spawn's cwd is not `workspace`.
fn spawn_subject(argv: &[String], cwd: &Path, workspace: &Path) -> String {
    let mut out = serde_json::Value::from(argv.to_vec()).to_string();
    if cwd != workspace {
        out.push_str(&format!(" in {}", cwd.display()));
    }
    out
}

/// The scrubbed `{method} {url}`, with the same `?…` and `(credentials
/// in the URL cut)` markers `ShowAccess` writes: userinfo, a query
/// string and a fragment can all carry a credential, and the rule that
/// judged the request judged the whole URL, not this shortened one.
/// `None` for a URL that fails to parse: unlike every other subject,
/// that text is the same placeholder for every such request regardless
/// of what was actually asked, so it must never be remembered — the
/// same reason a spawn carrying stdin has no subject either.
fn http_subject(method: &str, url: &str) -> Option<String> {
    let mut u = url::Url::parse(url).ok()?;
    let had_userinfo = !u.username().is_empty() || u.password().is_some();
    let had_query = u.query().is_some() || u.fragment().is_some();
    gwennol_core::steps::http::scrub(&mut u);
    let mut out = format!("{method} {u}");
    // Say that something was cut, without saying what.
    if had_query {
        out.push_str("?…");
    }
    if had_userinfo {
        out.push_str(" (credentials in the URL cut)");
    }
    Some(out)
}

/// The exact text a [`crate::policy::SessionRule`] remembers for
/// `access`, made at a workspace rooted at `workspace`: the path for
/// the three path kinds; for a spawn without stdin, the argv and cwd
/// `spawn_subject` writes; for `http`, `http_subject`. `None` for
/// a spawn carrying stdin — the stdin is the payload, and each one is
/// a new request — for a kind this frontend does not know, and for an
/// `http` URL that fails to parse, whose placeholder text is the same
/// for every such request. Built
/// through the same helpers [`ShowAccess`] renders with, so the two
/// cannot drift: a session rule matches exactly the text the prompt
/// showed — for `http` that text is already the *scrubbed* URL, so
/// one answer admits every query string at the same path, not only
/// the one shown.
pub fn subject(access: &Access, workspace: &Path) -> Option<String> {
    match access {
        Access::ReadFile(p) | Access::WriteFile(p) | Access::ListDir(p) => {
            Some(p.display().to_string())
        }
        Access::Spawn {
            argv,
            cwd,
            stdin: None,
        } => Some(spawn_subject(argv, cwd, workspace)),
        Access::Spawn { stdin: Some(_), .. } => None,
        Access::Http { method, url } => http_subject(method, url),
        _ => None,
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

/// [`decided`] with a [`Judgement`] as the verdict: the shape
/// `Headless::approve` used to print itself, and what a rule (or
/// session rule) decision traces as.
pub fn decision(request: &ApprovalRequest, judgement: &Judgement<'_>, workspace: &Path) -> String {
    decided(request, judgement, workspace)
}

/// A decision line whose verdict is not a `Judgement`: what a prompt's
/// own answer writes.
pub fn decided(request: &ApprovalRequest, verdict: impl fmt::Display, workspace: &Path) -> String {
    let cause = match &request.cause {
        Some(call) => format!(" (call {})", ShowCall(call)),
        None => String::new(),
    };
    format!(
        "{} from {}{cause}: {verdict}",
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

    /// Guards D2: `subject` is the text after the kind word of
    /// `ShowAccess` (the whole line for `http`, which has no leading
    /// kind word), `None` for a spawn carrying stdin. `decided` and
    /// `decision` agreeing is not tested here — `decision` is a
    /// one-line delegation to `decided` (`decision`'s own doc), so
    /// the two calls are the same call and no assertion on them can
    /// fail; the trace's exact words for a prompted decision are
    /// really pinned by `prompt::tests::each_key_answers_as_the_legend_says`.
    /// Mutation: omit the cwd in `spawn_subject` — the elsewhere-spawn
    /// assertions below fail.
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

        let read = Access::ReadFile(PathBuf::from("/ws/a.txt"));
        assert_eq!(show(&read), "read /ws/a.txt");
        assert_eq!(subject(&read, ws), Some("/ws/a.txt".to_string()));

        let spawn_local = Access::Spawn {
            argv: vec!["bash".into(), "-c".into(), "echo a b".into()],
            cwd: PathBuf::from("/ws"),
            stdin: None,
        };
        assert_eq!(show(&spawn_local), r#"spawn ["bash","-c","echo a b"]"#);
        assert_eq!(
            subject(&spawn_local, ws),
            Some(r#"["bash","-c","echo a b"]"#.to_string())
        );

        let spawn_elsewhere_with_stdin = Access::Spawn {
            argv: vec!["sh".into()],
            cwd: PathBuf::from("/elsewhere"),
            stdin: Some("exit 1\n".into()),
        };
        assert_eq!(
            show(&spawn_elsewhere_with_stdin),
            r#"spawn ["sh"] in /elsewhere with 7 bytes on stdin"#
        );
        // The stdin is the real payload; no session rule can remember
        // a request like this one.
        assert_eq!(subject(&spawn_elsewhere_with_stdin, ws), None);
        let spawn_elsewhere = Access::Spawn {
            argv: vec!["sh".into()],
            cwd: PathBuf::from("/elsewhere"),
            stdin: None,
        };
        assert_eq!(
            subject(&spawn_elsewhere, ws),
            Some(r#"["sh"] in /elsewhere"#.to_string())
        );

        let http = Access::Http {
            method: "POST".into(),
            url: "https://api.anthropic.com/v1/messages".into(),
        };
        assert_eq!(show(&http), "POST https://api.anthropic.com/v1/messages");
        assert_eq!(
            subject(&http, ws),
            Some("POST https://api.anthropic.com/v1/messages".to_string())
        );

        // Userinfo and the query string can carry a credential: the
        // rule judged them, the trace does not repeat them.
        let secret_http = Access::Http {
            method: "GET".into(),
            url: "https://user:hunter2@x.example/p?key=sk-secret#frag".into(),
        };
        let shown = show(&secret_http);
        assert_eq!(
            shown,
            "GET https://x.example/p?… (credentials in the URL cut)"
        );
        assert!(!shown.contains("hunter2") && !shown.contains("sk-secret"));
        assert!(!shown.contains("user"));
        assert_eq!(subject(&secret_http, ws), Some(shown));

        // A URL that fails to parse: `ShowAccess` still shows the
        // same placeholder it always has, but that text is identical
        // for every such request, so no session rule can be made from
        // it — the same reason a spawn carrying stdin has none.
        // Mutation: `Some(format!(...))` in `http_subject`'s `Err`
        // arm — this assertion fails.
        let unparseable_http = Access::Http {
            method: "GET".into(),
            url: "not a url".into(),
        };
        assert_eq!(show(&unparseable_http), "GET request (unparseable URL)");
        assert_eq!(subject(&unparseable_http, ws), None);
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
