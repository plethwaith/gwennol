//! The [`Operator`] trait: the seam between the host and a frontend.
//!
//! The host never decides policy. Every native step that reaches outside the
//! sandbox — a filesystem read or write, a process spawn, an outbound HTTP
//! request — describes what it wants as an [`ApprovalRequest`] and asks the
//! operator. Secrets are fetched the same way, so the host never holds a key
//! it was not handed for a specific plugin.

use std::path::PathBuf;

/// What a native step wants to do outside the sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Access {
    /// Read the contents of a file. The path is canonical — symlinks
    /// resolved — and the host has verified it names the file it will
    /// actually read.
    ReadFile(PathBuf),
    /// Create or overwrite a file. The path is canonical up to its deepest
    /// existing ancestor; a symlink destination is refused before asking.
    WriteFile(PathBuf),
    /// List a directory, named by its canonical path.
    ListDir(PathBuf),
    /// Spawn a process.
    Spawn {
        /// Program and arguments.
        argv: Vec<String>,
        /// Working directory.
        cwd: PathBuf,
        /// What will be piped to the child's stdin. Part of the request
        /// because for an interpreter (`sh`, `python`, …) it is the real
        /// payload — an approval that omitted it would be judging the
        /// envelope and not the letter.
        stdin: Option<String>,
    },
    /// Make an outbound HTTP request.
    Http {
        /// HTTP method.
        method: String,
        /// Full URL.
        url: String,
    },
}

/// A tool call the model asked for.
///
/// Carried through the kernel as an execution context (see
/// [`crate::context`]) so that a step body reached several dispatches later
/// can still name the call it serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    /// Provider-assigned id, when the provider issued one.
    pub id: Option<String>,
    /// Tool name, as the model asked for it.
    pub name: String,
    /// JSON-encoded arguments, as the model produced them.
    pub arguments: String,
}

/// An [`Access`], who is asking, and what the model asked for.
///
/// `plugin` is the Gwead plugin whose action is executing — the tool or
/// provider, never the host itself — so a frontend can show "the `bash` tool
/// wants to run …" and remember decisions per plugin.
///
/// `cause` is the model's tool call that set this in motion, when the
/// frontend ran the action through [`crate::context::exec_context`]; with it
/// a prompt can say *the model asked `bash` to run `rm -rf build`* rather
/// than merely *plugin `bash` wants to spawn a process*. It is `None` for an
/// action the frontend started itself, which is the honest answer: nothing
/// the model asked for is responsible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    /// Fully-qualified name of the plugin whose action is executing.
    pub plugin: String,
    /// The tool call this work serves, if any.
    pub cause: Option<ToolCall>,
    /// What it wants.
    pub access: Access,
}

/// The operator's answer to an [`ApprovalRequest`].
///
/// Remembering a decision is the operator's job, not the host's: the host
/// asks on every step, and an operator that answered `Allow` to an
/// equivalent request earlier is free to answer it again without prompting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Allow.
    Allow,
    /// Refuse. The step fails and the plugin sees an error.
    Deny,
}

/// Something the loop wants the frontend to show.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Event {
    /// A chunk of model output text.
    Text(String),
    /// The model is invoking a tool.
    ToolCall(ToolCall),
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

    /// Produce a secret for a plugin, if the operator is willing to hand it
    /// over. The host only asks for `(plugin, name)` pairs the plugin's
    /// manifest declared in `usesSecrets`, and the kernel narrows whatever
    /// comes back to that declaration regardless.
    async fn secret(&self, plugin: &str, name: &str) -> Option<String>;

    /// Render an event.
    fn emit(&self, event: Event);

    /// Block for the next user turn. `None` ends the session.
    async fn input(&self) -> Option<Turn>;
}
