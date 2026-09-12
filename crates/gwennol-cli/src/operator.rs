//! The headless [`Operator`]: policy answers approvals, sources answer
//! secrets, events go to the terminal, and there is no input.
//!
//! Model text goes to stdout, a provider round at a time, and nothing
//! else goes there, so a run's answer can be piped. A round is held
//! until the loop has accepted it — the tool calls it made are being
//! dispatched, or the turn is complete — because a round the provider
//! failed part-way is retried from the start and its text streamed
//! again: written as it arrived, stdout would carry the fragment
//! twice. What stdout holds is therefore exactly the text in the
//! transcript. Everything else — each tool call and its result, and
//! every approval decision with the rule behind it — goes to stderr,
//! one line each, prefixed `gwennol:` so it is told apart from
//! whatever a spawned command prints. The URL in an HTTP decision is
//! scrubbed the way the host scrubs its own logs: the rule judged the
//! full URL, but a query string can carry a key and a trace is a
//! record.

use std::fmt;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use gwennol_core::{ApprovalRequest, Decision, Event, Operator, Turn};

use crate::policy::Policy;
use crate::secrets::{Found, Secrets};
use crate::show::{ShowAccess, ShowCall, preview};

/// The headless frontend.
pub struct Headless {
    policy: Policy,
    secrets: Secrets,
    workspace: PathBuf,
    /// `-v` count: at 1 and above, tool results are shown whole.
    verbosity: u8,
    /// Text of the provider round in progress, written to stdout once
    /// the loop accepts the round.
    pending: Mutex<String>,
    /// Whether the last byte written to stdout was not a newline, so a
    /// completed turn can end its line.
    line_open: Mutex<bool>,
}

impl Headless {
    /// A frontend judging by `policy`, answering secrets from
    /// `secrets`, for a workspace at `workspace` (canonical).
    pub fn new(policy: Policy, secrets: Secrets, workspace: PathBuf, verbosity: u8) -> Self {
        Self {
            policy,
            secrets,
            workspace,
            verbosity,
            pending: Mutex::new(String::new()),
            line_open: Mutex::new(false),
        }
    }

    fn note(&self, line: impl fmt::Display) {
        eprintln!("gwennol: {line}");
    }

    /// Write the round's text, now that the loop has accepted it.
    fn flush_round(&self) {
        let text = std::mem::take(&mut *self.pending.lock().unwrap());
        if text.is_empty() {
            return;
        }
        let mut out = std::io::stdout().lock();
        // A closed pipe is the reader's business, not a reason to fail
        // the turn; the model's answer still lands in the transcript.
        let _ = out.write_all(text.as_bytes());
        let _ = out.flush();
        *self.line_open.lock().unwrap() = !text.ends_with('\n');
    }

    /// Drop the round's text: the provider failed it and will stream
    /// it again.
    fn discard_round(&self) {
        self.pending.lock().unwrap().clear();
    }

    fn end_line(&self) {
        let mut open = self.line_open.lock().unwrap();
        if *open {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(b"\n");
            let _ = out.flush();
            *open = false;
        }
    }
}

#[async_trait::async_trait]
impl Operator for Headless {
    async fn approve(&self, request: ApprovalRequest) -> Decision {
        let judgement = self.policy.judge(&request);
        let cause = match &request.cause {
            Some(call) => format!(" (call {})", ShowCall(call)),
            None => String::new(),
        };
        self.note(format_args!(
            "{} from {}{cause}: {judgement}",
            ShowAccess {
                access: &request.access,
                workspace: &self.workspace,
            },
            request.plugin,
        ));
        judgement.decision
    }

    async fn secret(&self, plugin: &str, name: &str) -> Option<String> {
        match self.secrets.lookup(plugin, name) {
            Some((value, found)) => {
                match found {
                    Found::Rule { origin, source } => {
                        tracing::debug!(plugin, name, %origin, ?source, "secret supplied")
                    }
                    Found::Convention(var) => {
                        tracing::debug!(plugin, name, var, "secret supplied by convention")
                    }
                }
                Some(value)
            }
            None => {
                // Warned once at startup, when the manifest was read;
                // here it is the same fact per request.
                tracing::info!(
                    plugin,
                    name,
                    "no value for secret: set {}",
                    self.secrets.describe_source(plugin, name)
                );
                None
            }
        }
    }

    fn emit(&self, event: Event) {
        match event {
            Event::Text(text) => self.pending.lock().unwrap().push_str(&text),
            Event::ToolCall(call) => {
                self.flush_round();
                self.end_line();
                self.note(format_args!(
                    "-> {}: {}",
                    ShowCall(&call),
                    preview(&call.arguments)
                ));
            }
            Event::ToolResult {
                call,
                content,
                is_error,
            } => {
                let verdict = if is_error { "error" } else { "ok" };
                self.note(format_args!(
                    "<- {}: {verdict}, {} bytes",
                    ShowCall(&call),
                    content.len()
                ));
                if self.verbosity >= 1 {
                    for line in content.lines() {
                        eprintln!("    {line}");
                    }
                } else if !content.is_empty() {
                    eprintln!("    {}", preview(&content));
                }
            }
            Event::ToolFailed { call, error } => {
                // A call the loop never dispatched — cut off by a
                // cancellation, or made in a round the model ended in
                // a refusal — arrives with no ToolCall before it, and
                // the round it belongs to is in the transcript: its
                // text is owed to stdout, and before this line.
                self.flush_round();
                self.end_line();
                self.note(format_args!("!! {}: {error}", ShowCall(&call)));
            }
            Event::Retry {
                attempt,
                max_attempts,
                failure,
            } => {
                self.discard_round();
                self.note(format_args!(
                    "provider failure, retrying ({attempt}/{max_attempts}): {failure}"
                ));
            }
            Event::TurnComplete => {
                self.flush_round();
                self.end_line();
            }
            // The event set is non-exhaustive; something this frontend
            // does not know how to show is said at the default level
            // rather than lost.
            other => tracing::warn!(?other, "event this frontend cannot show"),
        }
    }

    /// Non-interactive: the frontend drives its one turn itself and
    /// never asks for another.
    async fn input(&self) -> Option<Turn> {
        None
    }
}
