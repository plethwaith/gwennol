//! The types a session's frontend sees: the tool calls the model makes,
//! the [`Event`]s the loop emits, and the [`EventSink`] they go to.

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

/// Something the loop wants the frontend to show.
///
/// One event per thing that happened, in the order it happened: text as
/// it streams (or per block, on a buffered turn), each tool call as the
/// loop dispatches it, and exactly one of [`Event::ToolResult`] or
/// [`Event::ToolFailed`] per call the model made — including a call the
/// loop never dispatched, which gets its [`Event::ToolFailed`] with no
/// [`Event::ToolCall`] before it, so a frontend hears about every
/// result the model gets. What the model is *told* is the same in both
/// cases — a `tool_result` block, `is_error` when the tool did not
/// succeed — but the frontend is told which it was as data, so a tool
/// that reported a failure and a tool that could not run are never
/// confused (the boundary `docs/SPI.md` draws).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Event {
    /// A chunk of model output text.
    Text(String),
    /// The loop is about to run a tool the model asked for.
    ToolCall(ToolCall),
    /// The tool answered. `content` is what the model sees: text, or a list
    /// of text and images, rendered through the shared truncation
    /// convention, and `is_error` is the tool's own verdict on it.
    ToolResult {
        /// The call this answers.
        call: ToolCall,
        /// The result as handed to the model.
        content: crate::content::ToolContent,
        /// The tool reported a failure the model should react to.
        is_error: bool,
    },
    /// The tool could not answer, or was never asked. The model named a
    /// tool that is not registered, sent arguments its schema refuses,
    /// or the call failed as a step (the step refused it, the kernel
    /// refused, the plugin returned a malformed result); or the turn
    /// was cancelled while it ran; or the loop did not run it at all —
    /// the turn was cancelled before it started, or the model refused
    /// its own turn after asking — in which case no [`Event::ToolCall`]
    /// precedes this. The model is told, as an `is_error` result
    /// carrying `error` verbatim; the loop never branches on the text.
    ToolFailed {
        /// The call that got no answer.
        call: ToolCall,
        /// Why, as the model is told it.
        error: String,
    },
    /// The provider refused a turn in a way it marked worth repeating,
    /// and the loop is about to repeat it. Text already shown for the
    /// failed attempt will be shown again.
    Retry {
        /// The attempt about to be made, counting the first as 1.
        attempt: u32,
        /// The most the loop will make.
        max_attempts: u32,
        /// The provider's reason for the failed attempt.
        failure: crate::agent::Failure,
    },
    /// The current turn is complete.
    TurnComplete,
}

/// Where a session's [`Event`]s go.
///
/// A session sends every event to the sink it was started with. `emit` is called synchronously from the task
/// driving the session's turn, in the order the events occur, so it must
/// not block: a sink that blocks stalls the task driving the turn (a future
/// joined with it included) and the thread running it, and one that panics
/// unwinds the turn's task.
pub trait EventSink: Send + Sync {
    /// Take one event. Called from the task driving the session's turn.
    fn emit(&self, event: Event);
}
