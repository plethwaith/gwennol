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
