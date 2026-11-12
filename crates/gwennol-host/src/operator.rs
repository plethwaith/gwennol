//! The [`Operator`] trait: the seam between the host and a frontend.
//!
//! The host never decides policy. Every native step that reaches outside the
//! sandbox — a filesystem read or write, a process spawn, an outbound HTTP
//! request — describes what it wants as an [`ApprovalRequest`] and asks the
//! operator. Secrets are fetched the same way, so the host never holds a key
//! it was not handed for a specific plugin.
//!
//! A frontend starts its session with `Session::with_events` and hands it an
//! [`OperatorEvents`], which sends the session's events to its operator. One
//! that only needs the events implements [`EventSink`] instead and has no
//! operator.

use std::path::PathBuf;
use std::sync::Arc;

use gwennol_core::{Event, EventSink, ToolCall};

/// What a native step wants to do outside the sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Access {
    /// Read the contents of a file. The path is canonical — symlinks
    /// resolved — and the host has verified it names the file it will
    /// actually read. When nothing readable is there, the probe is still
    /// approved before the plugin learns so: the path is then canonical
    /// up to its deepest canonicalisable ancestor — the last component
    /// that exists, is a directory, and may be searched — with the rest
    /// spelled as given.
    ReadFile(PathBuf),
    /// Create or overwrite a file. The path is canonical up to its deepest
    /// canonicalisable ancestor. A symlink destination is never written;
    /// the probe is still approved, under the link's own name with its
    /// parent canonical, before the plugin learns the path is a link.
    WriteFile(PathBuf),
    /// List a directory, named by its canonical path — or, for a probe
    /// of a directory that is not there, canonical up to its deepest
    /// canonicalisable ancestor like [`Access::WriteFile`].
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

/// An [`Access`], who is asking, and what the model asked for.
///
/// `plugin` is the Gwead plugin whose action is executing — the tool or
/// provider, never the host itself — so a frontend can show "the `bash` tool
/// wants to run …" and remember decisions per plugin.
///
/// `cause` is the model's tool call that set this in motion, when the
/// frontend ran the action through [`gwennol_core::context::exec_context_with`] or [`gwennol_core::context::exec_context`]; with it
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
    ///
    /// The question can be withdrawn: when the invocation behind it is
    /// cancelled — the loop's turn was cancelled while this prompt was
    /// open — the host stops waiting and this future is dropped before
    /// it resolves. An implementation must therefore be safe to drop at
    /// any await point: a prompt it put on screen should come down
    /// (a guard type is the usual shape), and nothing it does after an
    /// await may be needed for consistency.
    async fn approve(&self, request: ApprovalRequest) -> Decision;

    /// Produce a secret for a plugin, if the operator is willing to hand it
    /// over. The host only asks for `(plugin, name)` pairs the plugin's
    /// manifest declared in `usesSecrets`, and the kernel narrows whatever
    /// comes back to that declaration regardless.
    async fn secret(&self, plugin: &str, name: &str) -> Option<String>;

    /// Render an event.
    fn emit(&self, event: Event);

    /// Block for the next user turn. `None` ends the session.
    ///
    /// Like [`Self::approve`], this future can be dropped before it
    /// resolves: [`crate::run`] stops waiting for input
    /// when its session token is cancelled. An implementation must be
    /// safe to drop at any await point.
    async fn input(&self) -> Option<Turn>;
}

/// Sends a session's events to an [`Operator`].
pub struct OperatorEvents(pub Arc<dyn Operator>);

impl EventSink for OperatorEvents {
    fn emit(&self, event: Event) {
        self.0.emit(event);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// Keeps the events it is sent; every other method is out of its reach.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<Event>>);

    #[async_trait::async_trait]
    impl Operator for Recorder {
        async fn approve(&self, _: ApprovalRequest) -> Decision {
            unreachable!("events never ask for approval")
        }
        async fn secret(&self, _: &str, _: &str) -> Option<String> {
            unreachable!("events never ask for a secret")
        }
        fn emit(&self, event: Event) {
            self.0.lock().unwrap().push(event);
        }
        async fn input(&self) -> Option<Turn> {
            unreachable!("events never ask for input")
        }
    }

    #[test]
    fn operator_events_forwards_each_event_in_order() {
        let recorder = Arc::new(Recorder::default());
        let sink = OperatorEvents(recorder.clone());
        let call = ToolCall {
            id: Some("c1".into()),
            name: "read".into(),
            arguments: "{}".into(),
        };
        sink.emit(Event::Text("hello".into()));
        sink.emit(Event::ToolCall(call.clone()));
        sink.emit(Event::TurnComplete);
        assert_eq!(
            *recorder.0.lock().unwrap(),
            vec![
                Event::Text("hello".into()),
                Event::ToolCall(call),
                Event::TurnComplete,
            ]
        );
    }
}
