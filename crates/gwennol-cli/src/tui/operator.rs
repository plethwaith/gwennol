//! The interactive [`Operator`]: a rule, compiled or made at a
//! session's own prompt, still answers what it can, and sources still
//! answer secrets, exactly as the print-mode frontend does; every
//! event and decision becomes a pane update instead of a line on
//! stderr. A request no rule matches becomes a prompt in the pane
//! instead of a denial: the one thing a
//! session can do that a print run cannot is ask. A key answers it in
//! `drive` — `y`/`n` decide once, `a`/`d` for the rest of the session
//! as a `SessionRule` tried after every compiled rule — and the
//! prompt comes down by construction when the turn is cancelled under
//! it, because [`crate::tui::prompt::PromptGuard`]'s drop removes it
//! by id whenever this future is dropped. The frontend drives `Session::turn`
//! itself (D7), so `input` never runs.

use std::path::PathBuf;
use std::sync::Arc;

use gwennol_core::{ApprovalRequest, Decision, Event, Operator, Turn};

use crate::policy::Policy;
use crate::secrets::Secrets;
use crate::show;
use crate::tui::prompt::{Prompt, PromptGuard};
use crate::tui::ui::{Entry, Shared};

/// The interactive frontend: judges by `policy`, answers secrets from
/// `secrets`, and renders through `shared`.
pub struct Interactive {
    policy: Policy,
    secrets: Secrets,
    workspace: PathBuf,
    verbosity: u8,
    shared: Arc<Shared>,
}

impl Interactive {
    /// A frontend judging by `policy`, answering secrets from
    /// `secrets`, for a workspace at `workspace` (canonical), showing
    /// tool results expanded by default at `-v` and above, through
    /// `shared`.
    pub fn new(
        policy: Policy,
        secrets: Secrets,
        workspace: PathBuf,
        verbosity: u8,
        shared: Arc<Shared>,
    ) -> Self {
        Self {
            policy,
            secrets,
            workspace,
            verbosity,
            shared,
        }
    }
}

#[async_trait::async_trait]
impl Operator for Interactive {
    /// Judge under one `update`: a rule or a session rule decides and
    /// traces with no `await`. Otherwise open a prompt carrying the
    /// request, the reason no rule could judge it, the rendered
    /// access line, and the subject a session rule would be made
    /// from, and await the person's answer; a guard removes the
    /// prompt when this future is dropped, and a receiver error (the
    /// guard's sender gone without an answer) denies.
    async fn approve(&self, request: ApprovalRequest) -> Decision {
        let judged = self.shared.update(|ui| {
            let judgement = self.policy.judge_with(&request, &ui.session_rules);
            if judgement.rule.is_some() || judgement.session.is_some() {
                let line = format!(
                    "gwennol: {}",
                    show::decision(&request, &judgement, &self.workspace)
                );
                let decision = judgement.decision;
                ui.push(Entry::Trace(line));
                Ok(decision)
            } else {
                Err(judgement.unjudgeable)
            }
        });
        let unjudgeable = match judged {
            Ok(decision) => return decision,
            Err(u) => u,
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        let access_line = show::ShowAccess {
            access: &request.access,
            workspace: &self.workspace,
        }
        .to_string();
        let subject = show::subject(&request.access, &self.workspace);
        let _guard = PromptGuard::open(
            &self.shared,
            Prompt::new(request, unjudgeable, access_line, subject, tx),
        );
        // Dropped here when the turn is cancelled under the prompt: the guard's
        // drop removes the prompt by id unconditionally. Order is the reverse of
        // what the declarations suggest — `rx` is moved into the awaited temporary,
        // created after `_guard`, and a suspended future drops in reverse creation
        // order, so the receiver goes first and the guard's retain runs after it.
        rx.await.unwrap_or(Decision::Deny)
    }

    async fn secret(&self, plugin: &str, name: &str) -> Option<String> {
        self.secrets.supply(plugin, name)
    }

    fn emit(&self, event: Event) {
        let verbosity = self.verbosity;
        self.shared.update(|ui| ui.apply(event, verbosity));
    }

    /// The frontend drives [`gwennol_core::agent::Session::turn`]
    /// itself and never runs [`gwennol_core::agent::Session::run`], so
    /// the loop never asks; `None` says so honestly.
    async fn input(&self) -> Option<Turn> {
        None
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::task::Poll;

    use gwennol_core::ToolCall;

    use super::*;

    fn call() -> ToolCall {
        ToolCall {
            id: Some("t1".to_string()),
            name: "read".to_string(),
            arguments: "{}".to_string(),
        }
    }

    /// The last entry's rendered text, whatever kind of entry it is.
    fn last_text(shared: &Arc<Shared>) -> String {
        shared
            .lock()
            .entries
            .last()
            .expect("an entry")
            .text()
            .into_owned()
    }

    /// Guards the session's `-v` (no test before this round;
    /// `show::tool_result`'s verbosity branch was exercised only
    /// through `headless.rs`): `emit` passes its own `verbosity`
    /// to `Ui::apply`, not a hardcoded value, so a multi-line result
    /// shows whole at `-v` and cut to one line without it, and the
    /// entry's own `expanded` field carries the same default. Mutation:
    /// hardcode `ui.apply(event, 0)` in `emit` — the first assertion
    /// below fails (the whole-content run instead shows a preview).
    #[test]
    fn emit_passes_its_own_verbosity_through() {
        let policy = crate::policy::Policy::compile(Vec::new(), Path::new("/ws")).unwrap();
        let content = "line one\nline two\nline three".to_string();

        let shared = Shared::new();
        let verbose = Interactive::new(
            policy.clone(),
            Secrets::new(Vec::new()),
            PathBuf::from("/ws"),
            1,
            shared.clone(),
        );
        verbose.emit(Event::ToolResult {
            call: call(),
            content: content.clone(),
            is_error: false,
        });
        let whole = last_text(&shared);
        assert!(
            whole.contains("\n    line one")
                && whole.contains("\n    line two")
                && whole.contains("\n    line three"),
            "verbosity 1 did not show each line on its own indented line: {whole:?}"
        );
        assert!(
            matches!(
                shared.lock().entries.last(),
                Some(Entry::ToolResult { expanded: true, .. })
            ),
            "verbosity 1 did not start the entry expanded"
        );

        let shared0 = Shared::new();
        let quiet = Interactive::new(
            policy,
            Secrets::new(Vec::new()),
            PathBuf::from("/ws"),
            0,
            shared0.clone(),
        );
        quiet.emit(Event::ToolResult {
            call: call(),
            content,
            is_error: false,
        });
        let preview = last_text(&shared0);
        assert!(
            !preview.contains("\n    line two"),
            "verbosity 0 showed each line on its own indented line: {preview:?}"
        );
        assert!(
            matches!(
                shared0.lock().entries.last(),
                Some(Entry::ToolResult {
                    expanded: false,
                    ..
                })
            ),
            "verbosity 0 started the entry expanded"
        );
    }

    /// Guards D1, D3: a compiled rule and a session rule both decide
    /// at once, tracing the same shape a rule decision always has,
    /// with no prompt ever opened. Mutation: pass `&[]` for the
    /// session slice in `approve` — the session-rule case below
    /// regresses to a prompt (`Poll::Pending`).
    #[test]
    fn a_rule_or_a_session_rule_decides_without_a_prompt() {
        use crate::policy::{Kind, RuleSpec, Source};
        use crate::tui::prompt::tests::{op, write_req};

        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);

        let deny_policy = crate::policy::Policy::compile(
            vec![RuleSpec {
                decision: Decision::Deny,
                text: "write:**".to_string(),
                plugin: None,
                source: Source::Flag,
            }],
            Path::new("/ws"),
        )
        .unwrap();
        let (interactive, shared) = op(deny_policy);
        let mut fut = interactive.approve(write_req());
        assert!(matches!(
            fut.as_mut().poll(&mut cx),
            Poll::Ready(Decision::Deny)
        ));
        assert!(shared.lock().prompts.is_empty());
        assert!(
            last_text(&shared).ends_with(r#"denied by --deny "write:**""#),
            "{}",
            last_text(&shared)
        );

        let empty_policy = crate::policy::Policy::compile(Vec::new(), Path::new("/ws")).unwrap();
        let (interactive, shared) = op(empty_policy);
        shared.update(|ui| {
            ui.session_rules.push(crate::policy::SessionRule {
                decision: Decision::Allow,
                plugin: "tool-write".to_string(),
                kind: Kind::Write,
                subject: "/ws/out.txt".to_string(),
            });
        });
        let mut fut = interactive.approve(write_req());
        assert!(matches!(
            fut.as_mut().poll(&mut cx),
            Poll::Ready(Decision::Allow)
        ));
        assert!(shared.lock().prompts.is_empty());
        assert!(
            last_text(&shared)
                .ends_with("allowed by session rule write:/ws/out.txt for plugin tool-write"),
            "{}",
            last_text(&shared)
        );
    }
}
