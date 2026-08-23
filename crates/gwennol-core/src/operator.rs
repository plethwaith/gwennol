//! The [`Operator`] trait: the seam between the host and a frontend.
//!
//! The host never decides policy. Every native step that reaches outside the
//! sandbox — a filesystem write, a process spawn, an outbound HTTP request —
//! describes what it wants as an [`ApprovalRequest`] and asks the operator.
//! Secrets are fetched the same way, so the host never holds a key it was not
//! handed for a specific use.

use std::path::PathBuf;

/// Something a native step wants to do outside the sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ApprovalRequest {
    /// Read a path.
    ReadPath(PathBuf),
    /// Create or overwrite a path.
    WritePath(PathBuf),
    /// Spawn a process with the given argv, in the given working directory.
    Spawn {
        /// Program and arguments.
        argv: Vec<String>,
        /// Working directory.
        cwd: PathBuf,
    },
    /// Make an outbound HTTP request to a host.
    Egress {
        /// Host (and optional port) being contacted.
        host: String,
    },
}

/// The operator's answer to an [`ApprovalRequest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Allow this request once.
    Allow,
    /// Allow this and equivalent requests for the rest of the session.
    AllowSession,
    /// Refuse.
    Deny,
}

/// Something the loop wants the frontend to show.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Event {
    /// A chunk of model output text.
    Text(String),
    /// The model is invoking a tool.
    ToolCall {
        /// Tool name as declared in its plugin manifest.
        name: String,
        /// JSON-encoded arguments.
        arguments: String,
    },
    /// A tool returned.
    ToolResult {
        /// Tool name.
        name: String,
        /// JSON-encoded result.
        result: String,
    },
    /// The current turn is complete.
    TurnComplete,
}

/// One user turn handed to the loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    /// The user's message.
    pub text: String,
}

/// What a frontend must provide. See the module docs.
#[async_trait::async_trait]
pub trait Operator: Send + Sync {
    /// Decide whether a native step may do something outside the sandbox.
    async fn approve(&self, request: ApprovalRequest) -> Decision;

    /// Produce a secret by name, if the operator is willing to hand it over.
    /// The host only asks for names a plugin declared in `usesSecrets`.
    async fn secret(&self, name: &str) -> Option<String>;

    /// Render an event.
    fn emit(&self, event: Event);

    /// Block for the next user turn. `None` ends the session.
    async fn input(&self) -> Option<Turn>;
}
