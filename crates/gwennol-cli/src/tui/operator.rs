//! The interactive [`Operator`]: policy still answers approvals and
//! sources still answer secrets, exactly as the print-mode frontend
//! does; every event and decision becomes a pane update instead of a
//! line on stderr. The frontend drives `Session::turn` itself (D7), so
//! `input` never runs.

use std::path::PathBuf;
use std::sync::Arc;

use gwennol_core::{ApprovalRequest, Decision, Event, Operator, Turn};

use crate::policy::Policy;
use crate::secrets::Secrets;
use crate::show;
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
    /// tool results whole at `-v` and above, through `shared`.
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
    async fn approve(&self, request: ApprovalRequest) -> Decision {
        let judgement = self.policy.judge(&request);
        let line = format!(
            "gwennol: {}",
            show::decision(&request, &judgement, &self.workspace)
        );
        self.shared.update(|ui| ui.push(Entry::Trace(line)));
        judgement.decision
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

    use gwennol_core::ToolCall;

    use super::*;

    fn call() -> ToolCall {
        ToolCall {
            id: Some("t1".to_string()),
            name: "read".to_string(),
            arguments: "{}".to_string(),
        }
    }

    fn last_trace(shared: &Arc<Shared>) -> String {
        match shared.lock().entries.last() {
            Some(Entry::Trace(t)) => t.clone(),
            other => panic!("expected a Trace entry, got {other:?}"),
        }
    }

    /// Guards the session's `-v` (no test before this round;
    /// `show::tool_result`'s verbosity branch was exercised only
    /// through `headless.rs`): `emit` passes its own `verbosity`
    /// to `Ui::apply`, not a hardcoded value, so a multi-line result
    /// shows whole at `-v` and cut to one line without it. Mutation:
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
        let whole = last_trace(&shared);
        assert!(
            whole.contains("\n    line one")
                && whole.contains("\n    line two")
                && whole.contains("\n    line three"),
            "verbosity 1 did not show each line on its own indented line: {whole:?}"
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
        let preview = last_trace(&shared0);
        assert!(
            !preview.contains("\n    line two"),
            "verbosity 0 showed each line on its own indented line: {preview:?}"
        );
    }
}
