//! The agent loop: a user turn, a provider call, the tool calls it asks
//! for, their results, and again — until the model stops asking.
//!
//! A [`Session`] holds one conversation against one `LLM_CHAT` provider
//! and the `TOOL` plugins it offers: every one the kernel has, or the
//! ones [`SessionConfig::tool_plugins`] names. [`Session::turn`] runs one
//! user turn to completion. Everything the loop shows a frontend goes
//! through the session's [`EventSink`] as an [`Event`]. The tool call
//! that caused a kernel invocation travels with it as an execution
//! context, so a step reached several dispatches later can name it (see
//! [`crate::context`]).
//!
//! The loop is the first consumer of both contracts, so it is where
//! their consumer-side rules are implemented, once:
//!
//! - **The provider's output is checked fail-closed.** An unknown stop
//!   reason, an unknown stream event, a block the closed schemas do not
//!   admit: the turn fails as [`TurnError::Contract`] rather than being
//!   guessed at. A stream that ends without its `end` event is a failed
//!   turn ([`TurnError::StreamEnded`]), and one whose source fails is
//!   [`TurnError::StreamFailed`], carrying the text the kernel recorded
//!   for the read — never a short answer either way — unless the
//!   turn's token had fired by the time the read that met the end or
//!   the failure came back, in which case it is the cut arriving
//!   ([`TurnError::Cancelled`]), carrying the source's text when it
//!   was a failure the read found waiting. The reader reads the
//!   token once with each read's code; by that reading's ordering, a
//!   token that fires after it is not seen by it, and the turn ends
//!   as what the read saw.
//! - **A streamed assistant message is rebuilt** as the events in
//!   order — adjacent text coalesced, `tool_use` and `opaque` blocks
//!   whole and in place — and replayed verbatim on the next round. The
//!   loop never reads an `opaque` block.
//! - **Tool arguments are validated** against the tool's declared
//!   schema before dispatch; the kernel validates no payloads.
//! - **Every tool call is answered** in the immediately following user
//!   message, in order, results before any text. A tool that reports
//!   `is_error` is answered with that; a tool that *cannot* answer —
//!   the model named a tool that does not exist, sent arguments the
//!   schema refuses, or the call failed as a step (the step refused it,
//!   the kernel refused, the plugin returned a malformed result) — is
//!   answered with an `is_error` result carrying the reason verbatim,
//!   and the frontend is told which case it was ([`Event::ToolFailed`]
//!   against [`Event::ToolResult`]). The loop branches on none of that
//!   text. Settled here rather than ending the turn because an
//!   unanswered call leaves the conversation unreplayable, and the
//!   step's refusal in particular is something the model should
//!   go around, not something the user should have to re-prompt past.
//!   Calls run one at a time, in the order the model made them, so
//!   their effects happen in that order. A round the
//!   model ended in a refusal while still asking for tools is stored
//!   with each call answered as not run: the refusal is not continued,
//!   and the model keeps the memory of having refused.
//! - **Provider failures are data or fatal, never guessed.** A
//!   `Failure` the provider marked `retryable` is retried, unchanged,
//!   with bounded backoff ([`RetryPolicy`]); any other `Failure` ends
//!   the turn as [`TurnError::Provider`]. A provider *step* error is
//!   fatal ([`TurnError::Step`]) and never retried — the contract says
//!   its message carries nothing a consumer may act on.
//! - **Cancellation tears the turn down.** The token handed to
//!   [`Session::turn`] reaches every kernel invocation the turn makes
//!   and the loop's own waits: a stream being read is closed (the
//!   relay sees its reader gone and winds down), a step that has not
//!   begun its work can report that it was stopped before it started
//!   ([`crate::cancel`]), a running tool step observes the token and
//!   stops, reported as the kernel's typed cancellation. The turn's own
//!   token is the authority: once it has fired, whatever a step reports
//!   is the cut arriving (a step's text goes to the log; a stream
//!   source's failure the cut hid travels on
//!   [`TurnError::Cancelled`]), and a cancellation arriving *without*
//!   it is a step failure, reported as such and logged.
//!   `reports_cancellation` decides which errors count as a
//!   cancellation, and [`crate::cancel`] says what a step sends.
//!   The cut-off call is answered as *interrupted while running* (it
//!   may have acted) or *interrupted before starting*, the calls after
//!   it as before starting, and the turn ends as
//!   [`TurnError::Cancelled`] once that exchange is stored: the model
//!   is told the truth about a call that did not complete, in words
//!   that name cancellation and not a tool failure.
//! - **The transcript holds only whole things.** A provider round
//!   that did not reach its `end` event is dropped, and so is a round
//!   whose message is empty (nothing to replay, and a vendor refuses
//!   an empty assistant message anywhere but last); a round that did
//!   is kept together with the answer to every call it made. A failed
//!   or cancelled turn therefore leaves the transcript ending in a
//!   user message — the user's content, or the last round's results — and
//!   the next turn's content joins that message. Nothing that happened is
//!   lost from the model's view, and nothing half-happened enters it.
//!
//! What the loop deliberately does not do: choose policy, persist
//! anything, or manage the
//! context window — the roadmap's "Beyond the MVP". A frontend that
//! stores [`Session::transcript`] hands it back with
//! [`Session::restore`].

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use gwead::kernel::{ExecutionContext, Kernel, KernelError};
use gwead::serde_json::{Map, Value, json};
use gwead::tokio_util::sync::CancellationToken;

use crate::content::{ContentError, UserContent};
use crate::context::{CONTEXT_KEY, exec_context_with};
use crate::events::{Event, EventSink, ToolCall};
use crate::spi;
use crate::spi::HarvestError;

mod stream;
mod tools;
mod transcript;

use stream::{EventReader, ReadError};
use tools::{ToolTable, parse_output};
use transcript::{MessageBuilder, ToolUse, Transcript, check_assistant_message, check_block};

/// Why a turn the vendor answered failed — the `LLM_CHAT` `Failure`.
///
/// Consumers branch on `retryable` and nothing else; `kind` and
/// `message` are for people.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// The provider's account of the failure.
    pub message: String,
    /// Worth repeating unchanged (`Some(true)`), will fail again
    /// (`Some(false)`), or the provider could not say (`None`).
    pub retryable: Option<bool>,
    /// The provider's own class identifier, informational only.
    pub kind: Option<String>,
}

impl Failure {
    /// Read a `Failure` object — the buffered form's `error` value, or
    /// a stream `error` event with its `type` already accounted for.
    fn parse(fields: &Map<String, Value>, allow_type: bool) -> Result<Self, String> {
        let mut failure = Failure {
            message: String::new(),
            retryable: None,
            kind: None,
        };
        let mut has_message = false;
        for (key, value) in fields {
            match (key.as_str(), value) {
                ("message", Value::String(s)) => {
                    failure.message = s.clone();
                    has_message = true;
                }
                ("retryable", Value::Bool(b)) => failure.retryable = Some(*b),
                ("kind", Value::String(s)) => failure.kind = Some(s.clone()),
                ("type", _) if allow_type => {}
                ("message" | "retryable" | "kind", other) => {
                    return Err(format!("failure field {key:?} has the wrong type: {other}"));
                }
                _ => {
                    return Err(format!(
                        "failure carries a field the contract does not allow: {key:?}"
                    ));
                }
            }
        }
        if !has_message {
            return Err("failure has no `message`".into());
        }
        Ok(failure)
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            Some(kind) => write!(f, "{} ({kind})", self.message),
            None => f.write_str(&self.message),
        }
    }
}

/// Why the model stopped generating — the contract's `StopReason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The model finished.
    EndTurn,
    /// The model wants tool results. Never the stop reason of a
    /// completed turn: the loop answers and continues.
    ToolUse,
    /// The generation cap was hit.
    MaxTokens,
    /// The model declined to continue; the turn is not retried.
    Refusal,
}

impl StopReason {
    fn parse(value: &Value) -> Result<Self, String> {
        match value.as_str() {
            Some("end_turn") => Ok(Self::EndTurn),
            Some("tool_use") => Ok(Self::ToolUse),
            Some("max_tokens") => Ok(Self::MaxTokens),
            Some("refusal") => Ok(Self::Refusal),
            _ => Err(format!("unknown stop_reason {value}")),
        }
    }
}

/// Token accounting, summed over a turn's rounds. Only the two
/// contractual counters: a provider's extra counters are per round
/// and are not carried.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// Tokens the provider read.
    pub input_tokens: u64,
    /// Tokens the provider generated.
    pub output_tokens: u64,
}

impl Usage {
    fn parse(value: &Value) -> Result<Self, String> {
        let counter = |name: &str| {
            value
                .get(name)
                .and_then(Value::as_u64)
                .ok_or_else(|| format!("usage has no non-negative integer `{name}`"))
        };
        Ok(Self {
            input_tokens: counter("input_tokens")?,
            output_tokens: counter("output_tokens")?,
        })
    }

    fn add(&mut self, other: Usage) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
    }
}

/// A completed turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnOutcome {
    /// Why the last round stopped: never [`StopReason::ToolUse`].
    pub stop_reason: StopReason,
    /// Provider calls the turn took.
    pub rounds: u32,
    /// Tokens over all of them.
    pub usage: Usage,
}

/// Why a turn did not complete. The transcript is left as the module
/// docs describe: whole exchanges kept, the partial one dropped.
#[derive(Debug, thiserror::Error)]
pub enum TurnError {
    /// [`Session::turn_with`] refused its content. Nothing was stored,
    /// no event was sent and the provider was not called.
    #[error("the turn's content was refused: {0}")]
    Content(#[from] ContentError),
    /// The token was cancelled. `detail` is the text the kernel
    /// recorded for a stream source whose failure the read that met
    /// the cut found waiting: the kernel polls the source before the
    /// token on every poll of a read, so a failure that is there is
    /// what the read reports, and the reader reads the token with
    /// that code. The cut hid a real failure, and this is it; `None`
    /// is a plain cancellation. Displays as `cancelled`, alone or
    /// followed by the text, so a frontend's `<name>: {e}` is its own
    /// outcome line.
    #[error("cancelled{}", stream::beside_the_cut(.detail))]
    Cancelled {
        /// The stream source's recorded failure text, when the cut
        /// hid one; `None` for a plain cancellation.
        detail: Option<String>,
    },
    /// The vendor refused, and the failure is not one to repeat — or
    /// was, and the retries ran out.
    #[error("provider refused the turn: {0}")]
    Provider(Failure),
    /// The provider's `chat` action failed as a step: the transport,
    /// a step, or the kernel said no before the vendor answered.
    /// Uniformly fatal; the message is not for consumers to read.
    #[error("provider step failed: {0}")]
    Step(KernelError),
    /// The provider's output is not the contract's.
    #[error("provider output violates the LLM_CHAT contract: {0}")]
    Contract(String),
    /// The stream ended before an `end` or `error` event: the source
    /// reached its end with the turn unfinished, so the kernel
    /// recorded nothing and the cause is lost. The provider's relay
    /// closes its output this way when the vendor's stream ends
    /// early. A source that *fails* is [`TurnError::StreamFailed`],
    /// which carries the text the kernel recorded.
    #[error("the stream ended before the turn did; the cause was lost")]
    StreamEnded,
    /// The stream's source failed before an `end` or `error` event,
    /// and `detail` is the text the kernel recorded for the read.
    /// For a relayed stream that text is the kernel's report of the
    /// relaying action's failure, `{plugin}.{action} failed: {e}`,
    /// with the action's own text as `e`; for a stream whose handle
    /// is the fetch step's body directly, the guard's text or the
    /// transport's error.
    #[error("the stream failed before the turn did: {}", stream::recorded(.detail))]
    StreamFailed {
        /// The text the kernel recorded for the failing read.
        /// `None` only if the kernel ever reported the failure with
        /// no recorded text, which does not happen today; kept as a
        /// canary, not a live case.
        detail: Option<String>,
    },
    /// The model kept asking for tools past [`SessionConfig::max_rounds`].
    #[error("the turn exceeded {0} provider rounds")]
    RoundLimit(u32),
}

/// Why a [`Session`] could not be started, or why [`Session::restore`]
/// refused a transcript.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// No plugin fulfils `LLM_CHAT`.
    #[error("no LLM_CHAT provider is registered")]
    NoProvider,
    /// Several plugins fulfil `LLM_CHAT` and the config named none.
    /// Refused rather than picked: role dispatch's "first wins" is
    /// silent, and which provider a session talks to should not be.
    #[error(
        "several LLM_CHAT providers are registered ({0:?}); name one in SessionConfig::provider"
    )]
    AmbiguousProvider(Vec<String>),
    /// The config named a provider that is not a registered fulfiller.
    #[error("no LLM_CHAT provider named {0:?} is registered")]
    NoSuchProvider(String),
    /// The tool inventory is unusable.
    #[error(transparent)]
    Harvest(#[from] HarvestError),
    /// [`SessionConfig::context`] carries the key the loop writes the tool
    /// call under.
    #[error(
        "SessionConfig::context may not carry the key {:?}: it is where the loop names a tool call",
        CONTEXT_KEY
    )]
    ReservedContextKey,
    /// [`Session::restore`] was given a list that breaks one of the rules
    /// it checks.
    #[error("restore refused the list at position {index}: {error}")]
    Transcript {
        /// The offending message's position; the list's length when the
        /// list ends in an unanswered tool call.
        index: usize,
        /// The rule it breaks.
        error: String,
    },
    /// A tool's declared argument schema does not compile.
    #[error("tool {tool:?} declares a schema that does not compile: {error}")]
    ToolSchema {
        /// The tool's name.
        tool: String,
        /// The compiler's reason.
        error: String,
    },
}

/// How a `retryable` provider failure is retried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Attempts per round, the first included. `1` never retries; `0`
    /// is read as `1`.
    pub max_attempts: u32,
    /// Wait before the second attempt; doubles per attempt after.
    pub initial_backoff: Duration,
    /// Ceiling on the wait.
    pub max_backoff: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(8),
        }
    }
}

/// What a session is: the frontend's choices, none of which the loop
/// makes for it.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// The `LLM_CHAT` plugin to use. `None` resolves by role, and
    /// refuses when more than one plugin fulfils it.
    pub provider: Option<String>,
    /// The system prompt, if any.
    pub system: Option<String>,
    /// The generation cap passed to the provider, if any.
    pub max_tokens: Option<u64>,
    /// Ask for the streamed form (the default) or the buffered one.
    pub stream: bool,
    /// The `$config` each plugin runs under, by plugin name — a
    /// provider's model and endpoint, say. A plugin not listed runs
    /// under `{}`. Applies to the action the loop dispatches: Gwead
    /// hands a caller's config through to the actions it invokes, so
    /// a plugin that invokes another runs it under its own config. The
    /// bundled plugins invoke only themselves; re-keying config per
    /// callee is a dispatch-orchestrator concern that arrives with
    /// installable plugins.
    pub plugin_configs: BTreeMap<String, Value>,
    /// Most provider calls one turn may make before it is abandoned.
    pub max_rounds: u32,
    /// Retry policy for `retryable` provider failures.
    pub retry: RetryPolicy,
    /// Most bytes one stream event may span. Events are unbounded by
    /// contract; this bounds what the loop will buffer for one.
    pub max_event_bytes: usize,
    /// The embedder's own entries in the execution context of every
    /// invocation the session makes, so that a secret resolver or a
    /// native step can tell what the invocation is for.
    ///
    /// A tool call runs under these entries plus [`CONTEXT_KEY`] naming
    /// the call; a provider round runs under these entries alone, or,
    /// when the map is empty, under the empty context. A map carrying
    /// [`CONTEXT_KEY`] refuses the session
    /// ([`SessionError::ReservedContextKey`]).
    pub context: Map<String, Value>,
    /// The `TOOL` plugins the session offers, by plugin name. `None`
    /// offers all the kernel's `TOOL` plugins.
    ///
    /// Selection applies before the duplicate tool-name refusal, so two
    /// plugins that advertise one tool name can share a kernel as long as
    /// every session on it selects at most one of them (a session whose
    /// `tool_plugins` is `None` offers both, and is refused). A selected
    /// plugin brings every tool it offers; a name that offers no tool
    /// refuses the session ([`SessionError::Harvest`]). `Some` of an empty
    /// set offers no tools.
    pub tool_plugins: Option<BTreeSet<String>>,
    /// The most bytes one image of a turn may have, checked by
    /// [`Session::turn_with`] before anything is stored. Not applied by
    /// [`Session::restore`]. Images travel inline and the conversation is
    /// sent again every round, so what the provider's guest can hold bounds
    /// the conversation's images together, not one image: a request that
    /// is too large for the bundled provider's guest fails the turn as
    /// [`TurnError::Step`]. An embedder that raises the kernel's memory
    /// limit can raise this with it.
    pub max_image_bytes: usize,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            provider: None,
            system: None,
            max_tokens: None,
            stream: true,
            plugin_configs: BTreeMap::new(),
            max_rounds: 64,
            retry: RetryPolicy::default(),
            max_event_bytes: 16 << 20,
            context: Map::new(),
            tool_plugins: None,
            max_image_bytes: 2 << 20,
        }
    }
}

/// The `LLM_CHAT` plugin a session would talk to: `named` when it is
/// a registered fulfiller, else the only fulfiller there is. The one
/// implementation of the rule [`SessionConfig::provider`] states —
/// [`Session::new`] applies it, and a frontend that needs the name
/// before the session exists (to set that plugin's `$config`, say)
/// asks here rather than re-deriving it.
pub fn resolve_provider(kernel: &Kernel, named: Option<&str>) -> Result<String, SessionError> {
    let mut candidates = kernel.role_candidates(None, spi::llm_chat::ROLE);
    match named {
        Some(named) => {
            if !candidates.iter().any(|c| c == named) {
                return Err(SessionError::NoSuchProvider(named.to_string()));
            }
            Ok(named.to_string())
        }
        None => match candidates.len() {
            0 => Err(SessionError::NoProvider),
            1 => Ok(candidates.remove(0)),
            _ => {
                candidates.sort();
                Err(SessionError::AmbiguousProvider(candidates))
            }
        },
    }
}

/// One conversation: a provider, the tools, and the transcript so far.
pub struct Session {
    kernel: Arc<Kernel>,
    events: Arc<dyn EventSink>,
    config: SessionConfig,
    provider: String,
    tools: ToolTable,
    transcript: Transcript,
    no_config: Value,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("provider", &self.provider)
            .field("messages", &self.transcript.messages().len())
            .finish_non_exhaustive()
    }
}

/// One provider round, checked.
struct Round {
    message: Value,
    calls: Vec<ToolUse>,
    stop_reason: StopReason,
    usage: Usage,
}

/// What a provider call came back with.
enum RoundResult {
    Message(Round),
    Failed(Failure),
}

impl Session {
    /// Start a session on a booted kernel — the `Arc` from
    /// [`Kernel::into_arc`], with the SPI contracts, the provider and
    /// the tools registered — that sends its events to `events`.
    /// Resolves the provider, harvests the tools and compiles their
    /// schemas; each can refuse, see [`SessionError`].
    ///
    /// The session reads no process-global state. Several sessions can
    /// share a kernel and keep their events apart, each with its own
    /// sink, and a session is driven with [`Self::turn`].
    pub fn new(
        kernel: Arc<Kernel>,
        config: SessionConfig,
        events: Arc<dyn EventSink>,
    ) -> Result<Self, SessionError> {
        if config.context.contains_key(CONTEXT_KEY) {
            return Err(SessionError::ReservedContextKey);
        }
        let provider = resolve_provider(&kernel, config.provider.as_deref())?;
        let tools = ToolTable::harvest(&kernel, config.tool_plugins.as_ref())?;
        Ok(Self {
            kernel,
            events,
            config,
            provider,
            tools,
            transcript: Transcript::default(),
            no_config: json!({}),
        })
    }

    /// Replace the conversation with `messages`, between turns.
    ///
    /// The whole list is checked first, against the rules the loop
    /// keeps while it runs, and on refusal the transcript is untouched
    /// ([`SessionError::Transcript`] gives the position). What
    /// [`Self::transcript`] returned, from this session or another, is
    /// accepted. The list may be empty, and may end in a user message
    /// (the next turn's content joins it).
    ///
    /// Not checked: that `tool_use` ids are unique across the
    /// conversation and that a tool the list names is one this session
    /// offers (the loop checks neither of what a provider sends), that
    /// a `tool_result` carries `is_error`, which the loop always writes,
    /// that an image block's `data` is base64 or decodes to the format its
    /// `media_type` names, and that an image is within
    /// [`SessionConfig::max_image_bytes`] (a transcript another session
    /// returned is accepted whatever that session's cap was). If
    /// the model calls an unoffered tool again, the loop answers that no
    /// tool by that name is available.
    pub fn restore(&mut self, messages: Vec<Value>) -> Result<(), SessionError> {
        self.transcript = Transcript::from_messages(messages)
            .map_err(|(index, error)| SessionError::Transcript { index, error })?;
        Ok(())
    }

    /// Send one event to wherever this session's events go.
    fn emit(&self, event: Event) {
        self.events.emit(event);
    }

    /// The provider plugin this session talks to.
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// The conversation so far, as the provider sees it: contract
    /// messages, oldest first.
    pub fn transcript(&self) -> &[Value] {
        self.transcript.messages()
    }

    /// Run one user turn to completion: provider rounds and the tool
    /// calls between them, until the model stops asking for tools, the
    /// turn fails, or `cancel` fires.
    pub async fn turn(
        &mut self,
        text: &str,
        cancel: &CancellationToken,
    ) -> Result<TurnOutcome, TurnError> {
        self.turn_with(vec![UserContent::Text(text.to_string())], cancel)
            .await
    }

    /// Run one user turn whose content is `content`: text and images, in
    /// the order given, stored as one run of blocks. Otherwise exactly
    /// [`Self::turn`].
    ///
    /// Refused as [`TurnError::Content`], with the transcript untouched,
    /// no event emitted and no provider call made, when `content` is empty
    /// or an image is larger than [`SessionConfig::max_image_bytes`]; every
    /// item is checked before the first is stored. The loop transforms no
    /// image. Dimensions, how many images a request may carry, and whether
    /// the model takes images at all are the vendor's to answer, as a
    /// [`TurnError::Provider`].
    ///
    /// As with any turn, content of a turn that fails stays in the
    /// transcript and the next turn's content joins it, so a refused image
    /// is sent again; [`Self::restore`] with the block removed is how an
    /// embedder drops it.
    pub async fn turn_with(
        &mut self,
        content: Vec<UserContent>,
        cancel: &CancellationToken,
    ) -> Result<TurnOutcome, TurnError> {
        if content.is_empty() {
            return Err(ContentError::Empty.into());
        }
        for (index, item) in content.iter().enumerate() {
            if let UserContent::Image(image) = item
                && image.bytes().len() > self.config.max_image_bytes
            {
                return Err(ContentError::ImageTooLarge {
                    index,
                    bytes: image.bytes().len(),
                    max: self.config.max_image_bytes,
                }
                .into());
            }
        }
        self.transcript
            .push_user_content(content.iter().map(UserContent::block).collect());
        let max_rounds = self.config.max_rounds.max(1);
        let mut usage = Usage::default();
        for round_no in 1..=max_rounds {
            let round = self.round_with_retry(cancel).await?;
            usage.add(round.usage);
            if !self.config.stream {
                // Streamed text was shown as it arrived.
                for block in round.message["content"].as_array().into_iter().flatten() {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        self.emit(Event::Text(text.to_string()));
                    }
                }
            }
            if round.stop_reason == StopReason::ToolUse && round.calls.is_empty() {
                return Err(TurnError::Contract(
                    "stop_reason is tool_use but the message has no tool_use block".into(),
                ));
            }
            if round.calls.is_empty() {
                // An empty message — a refusal or a cap hit before any
                // block completed — is not stored: there is nothing to
                // replay, and a vendor refuses an empty assistant
                // message anywhere but last. The next turn's content then
                // joins the user message this one left.
                if is_empty_message(&round.message) {
                    tracing::debug!(stop_reason = ?round.stop_reason, "empty assistant message not stored");
                } else {
                    self.transcript.push_assistant(round.message);
                }
                return self.complete(round.stop_reason, round_no, usage);
            }
            if round.stop_reason == StopReason::Refusal {
                // A refused turn is not continued, and a stored call
                // must be answered: each is answered as not run, so the
                // refusal — and any text before it — stays in the
                // model's memory and the transcript stays replayable.
                let results = round
                    .calls
                    .iter()
                    .map(|call| self.not_run(call, REFUSED))
                    .collect();
                self.transcript.push_assistant(round.message);
                self.transcript.push_tool_results(results);
                return self.complete(round.stop_reason, round_no, usage);
            }
            // Every call answered, in order, in the next message. A
            // cancellation part-way answers the rest as interrupted, so
            // the exchange is still whole when it is stored — a tool
            // before the cut may already have acted.
            let mut results = Vec::with_capacity(round.calls.len());
            for call in &round.calls {
                let block = if cancel.is_cancelled() {
                    self.not_run(call, Cut::BeforeStart.reason())
                } else {
                    match self.answer(call, cancel).await {
                        Ok(block) => block,
                        Err(Interrupted(cut)) => self.not_run(call, cut.reason()),
                    }
                };
                results.push(block);
            }
            self.transcript.push_assistant(round.message);
            self.transcript.push_tool_results(results);
            if cancel.is_cancelled() {
                return Err(TurnError::Cancelled { detail: None });
            }
        }
        Err(TurnError::RoundLimit(max_rounds))
    }

    /// The turn is over: tell the frontend, and say how.
    fn complete(
        &self,
        stop_reason: StopReason,
        rounds: u32,
        usage: Usage,
    ) -> Result<TurnOutcome, TurnError> {
        self.emit(Event::TurnComplete);
        Ok(TurnOutcome {
            stop_reason,
            rounds,
            usage,
        })
    }

    fn config_for(&self, plugin: &str) -> &Value {
        self.config
            .plugin_configs
            .get(plugin)
            .unwrap_or(&self.no_config)
    }

    /// The `chat` input the next round would send: the conversation as
    /// the provider sees it, whole — the system prompt, the tools as
    /// harvested, every message so far, and the generation settings —
    /// rather than the messages alone ([`Self::transcript`]). This is
    /// the builder the loop itself dispatches with, so what a frontend
    /// records from here is exactly what the provider was handed on the
    /// last round plus the round's own answer; a saved copy is a
    /// reviewable, reproducible request.
    pub fn chat_input(&self) -> Value {
        let mut input = Map::new();
        if let Some(system) = &self.config.system {
            input.insert("system".into(), Value::String(system.clone()));
        }
        input.insert(
            "messages".into(),
            Value::Array(self.transcript.messages().to_vec()),
        );
        if !self.tools.is_empty() {
            input.insert("tools".into(), self.tools.wire().clone());
        }
        if let Some(max_tokens) = self.config.max_tokens {
            input.insert("max_tokens".into(), Value::from(max_tokens));
        }
        if self.config.stream {
            input.insert("stream".into(), Value::Bool(true));
        }
        Value::Object(input)
    }

    /// One round, repeated on a `retryable` failure per the policy.
    async fn round_with_retry(&self, cancel: &CancellationToken) -> Result<Round, TurnError> {
        let max_attempts = self.config.retry.max_attempts.max(1);
        let mut backoff = self.config.retry.initial_backoff;
        for attempt in 1..=max_attempts {
            match self.round(cancel).await? {
                RoundResult::Message(round) => return Ok(round),
                RoundResult::Failed(failure)
                    if failure.retryable == Some(true) && attempt < max_attempts =>
                {
                    tracing::warn!(attempt, max_attempts, failure = %failure, "provider failure marked retryable; retrying");
                    self.emit(Event::Retry {
                        attempt: attempt + 1,
                        max_attempts,
                        failure,
                    });
                    // Clamped before it is waited, doubled without
                    // overflow: the policy's values are the frontend's
                    // and may be anything.
                    let wait = backoff.min(self.config.retry.max_backoff);
                    tokio::select! {
                        biased;
                        () = cancel.cancelled() => return Err(TurnError::Cancelled { detail: None }),
                        () = tokio::time::sleep(wait) => {}
                    }
                    backoff = backoff.saturating_mul(2);
                }
                RoundResult::Failed(failure) => return Err(TurnError::Provider(failure)),
            }
        }
        unreachable!("the attempt loop returns on its last iteration")
    }

    /// One provider call, streamed or buffered per the config.
    async fn round(&self, cancel: &CancellationToken) -> Result<RoundResult, TurnError> {
        let input = self.chat_input();
        let mut request = self
            .kernel
            .execute(&self.provider, spi::llm_chat::CHAT, input)
            .with_config(self.config_for(&self.provider))
            .with_cancel(cancel.child_token());
        if !self.config.context.is_empty() {
            request = request.with_exec_ctx(ExecutionContext::new(Value::Object(
                self.config.context.clone(),
            )));
        }
        // A provider step error is fatal — unless the turn's own token
        // fired, in which case the error is the cancellation arriving.
        let fatal = |e: KernelError| {
            if cancel.is_cancelled() {
                tracing::info!(error = %e, "provider round ended by the turn's cancellation");
                TurnError::Cancelled { detail: None }
            } else {
                warn_if_unrequested(&e);
                TurnError::Step(e)
            }
        };
        if !self.config.stream {
            let out = request.run().await.map_err(fatal)?.output;
            return interpret_buffered(&out);
        }
        // One table per call, holding nothing but this turn's stream.
        let streams: gwead::kernel::streams::SharedStreamRegistry = Default::default();
        let out = request
            .with_streams(streams.clone())
            .run()
            .await
            .map_err(fatal)?
            .output;
        let handle = match out.as_object() {
            Some(fields) if fields.len() == 1 => fields.get("stream").and_then(Value::as_u64),
            _ => None,
        };
        let id = handle
            .and_then(|h| u32::try_from(h).ok())
            .and_then(NonZeroU32::new)
            .ok_or_else(|| {
                TurnError::Contract(format!(
                    "streamed chat output is not {{\"stream\": handle}}: {out}"
                ))
            })?;
        let reader = EventReader::new(streams, id, self.config.max_event_bytes);
        self.consume_stream(reader, cancel).await
    }

    /// Drain a streamed turn: show text as it comes, rebuild the
    /// message, stop at the terminal event.
    async fn consume_stream(
        &self,
        mut reader: EventReader,
        cancel: &CancellationToken,
    ) -> Result<RoundResult, TurnError> {
        let mut builder = MessageBuilder::default();
        loop {
            let event = match reader.next(cancel).await {
                Ok(Some(event)) => event,
                Ok(None) => return Err(TurnError::StreamEnded),
                Err(e) => return Err(stream_read_failure(e)),
            };
            let Some(fields) = event.as_object() else {
                return Err(TurnError::Contract(format!(
                    "stream event is not an object: {event}"
                )));
            };
            match fields.get("type").and_then(Value::as_str) {
                Some("text") => {
                    check_block(&event).map_err(TurnError::Contract)?;
                    let text = fields
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    self.emit(Event::Text(text.to_string()));
                    builder.text(text);
                }
                Some("tool_use" | "opaque") => {
                    check_block(&event).map_err(TurnError::Contract)?;
                    builder.block(event);
                }
                Some("end") => {
                    for key in fields.keys() {
                        if !matches!(key.as_str(), "type" | "stop_reason" | "usage") {
                            return Err(TurnError::Contract(format!(
                                "end event carries a field the contract does not allow: {key:?}"
                            )));
                        }
                    }
                    return round_from(
                        builder.finish(),
                        fields.get("stop_reason"),
                        fields.get("usage"),
                    )
                    .map(RoundResult::Message)
                    .map_err(TurnError::Contract);
                }
                Some("error") => {
                    let failure = Failure::parse(fields, true).map_err(TurnError::Contract)?;
                    return Ok(RoundResult::Failed(failure));
                }
                Some(other) => {
                    return Err(TurnError::Contract(format!(
                        "unknown stream event type {other:?}"
                    )));
                }
                None => {
                    return Err(TurnError::Contract(format!(
                        "stream event has no string `type`: {event}"
                    )));
                }
            }
        }
    }

    /// Answer one tool call: the `tool_result` block the model gets.
    /// Every path produces a block — see the module docs for why a call
    /// that could not run is still answered — except the invocation
    /// reporting its own cancellation, which the caller answers as
    /// interrupted.
    async fn answer(
        &self,
        call: &ToolUse,
        cancel: &CancellationToken,
    ) -> Result<Value, Interrupted> {
        let tool_call = ToolCall {
            id: Some(call.id.clone()),
            name: call.name.clone(),
            arguments: call.input.to_string(),
        };
        self.emit(Event::ToolCall(tool_call.clone()));

        let answer: Result<tools::ToolOutput, String> = match self.tools.find(&call.name) {
            None => Err(format!("no tool named {:?} is available", call.name)),
            Some(tool) => match self.tools.validate(tool, &call.input) {
                Err(e) => Err(format!(
                    "arguments for {:?} do not match its schema: {e}",
                    call.name
                )),
                Ok(()) => {
                    let outcome = self
                        .kernel
                        .execute(
                            &tool.descriptor.plugin_key,
                            &tool.descriptor.action_name,
                            call.input.clone(),
                        )
                        .with_config(self.config_for(&tool.descriptor.plugin_key))
                        .with_exec_ctx(exec_context_with(&self.config.context, &tool_call))
                        .with_cancel(cancel.child_token())
                        .run()
                        .await;
                    match outcome {
                        Ok(result) => parse_output(&result.output).map_err(|e| {
                            format!("{:?} returned a malformed result: {e}", call.name)
                        }),
                        // The turn's token is the authority on
                        // cancellation: once it fired, whatever the
                        // step reports is the cut arriving — its text
                        // goes to the log, and the model is told the
                        // call was interrupted, which is true whatever
                        // else went wrong. The code only says how far
                        // the step got.
                        Err(e) if cancel.is_cancelled() => {
                            tracing::info!(tool = %call.name, id = %call.id, error = %e, "tool call ended by the turn's cancellation");
                            return Err(Interrupted(Cut::from_error(&e)));
                        }
                        // A cancellation nobody asked for is a failure
                        // to report, not a cut; `reports_cancellation`
                        // decides what counts as one.
                        Err(e) if reports_cancellation(&e) => {
                            warn_if_unrequested(&e);
                            Err(format!(
                                "{:?} stopped with a cancellation the turn did not request — the kernel's action ceiling, or the plugin's own doing: {e}",
                                call.name
                            ))
                        }
                        Err(e) => Err(format!(
                            "{:?} failed before producing a result: {e}",
                            call.name
                        )),
                    }
                }
            },
        };

        let (content, is_error) = match answer {
            Ok(output) => {
                let content = spi::tool::render_content(&output.content, output.truncated);
                self.emit(Event::ToolResult {
                    call: tool_call,
                    content: content.clone(),
                    is_error: output.is_error,
                });
                (content, output.is_error)
            }
            Err(error) => {
                tracing::warn!(tool = %call.name, id = %call.id, %error, "tool call could not be answered by the tool");
                self.emit(Event::ToolFailed {
                    call: tool_call,
                    error: error.clone(),
                });
                (error, true)
            }
        };
        Ok(json!({
            "type": "tool_result",
            "tool_use_id": call.id,
            "content": content,
            "is_error": is_error,
        }))
    }
}

impl Session {
    /// Answer a call that was not run, and tell the frontend: one
    /// [`Event::ToolFailed`] per such call, with no [`Event::ToolCall`]
    /// before it since the loop never dispatched it.
    fn not_run(&self, call: &ToolUse, reason: &str) -> Value {
        self.emit(Event::ToolFailed {
            call: ToolCall {
                id: Some(call.id.clone()),
                name: call.name.clone(),
                arguments: call.input.to_string(),
            },
            error: reason.to_string(),
        });
        json!({
            "type": "tool_result",
            "tool_use_id": call.id,
            "content": reason,
            "is_error": true,
        })
    }
}

/// The turn's cancellation cut a tool call off.
struct Interrupted(Cut);

/// Where the cancellation found the call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cut {
    /// Nothing had happened: the loop had not dispatched it, or the
    /// step reported it was stopped before it started.
    BeforeStart,
    /// The step was somewhere in its work, and cannot say how far.
    WhileRunning,
}

impl Cut {
    /// Read how far the cancelled step got from what it reported. Only
    /// a step's structured error can say it never started; the kernel's
    /// typed in-work cancellation, a cancellation flattened by a nested
    /// invoke, or any other shape, is read as the cautious answer.
    fn from_error(e: &KernelError) -> Self {
        match e {
            KernelError::PluginError { code, params, .. }
                if code == crate::cancel::CANCELLED_CODE
                    && params["phase"] == crate::cancel::CANCELLED_BEFORE_START =>
            {
                Cut::BeforeStart
            }
            _ => Cut::WhileRunning,
        }
    }

    /// What the model is told. Names cancellation, not the tool, and
    /// claims no more than is known.
    fn reason(self) -> &'static str {
        match self {
            Cut::WhileRunning => {
                "interrupted: the turn was cancelled while this call was running; it may have run in part or in full"
            }
            Cut::BeforeStart => "interrupted: the turn was cancelled before this call started",
        }
    }
}

/// Whether a kernel error carries a cancellation: the kernel's typed
/// [`KernelError::Cancelled`] — a step that stopped on its token,
/// or the kernel's own between-step check — or a step's structured
/// [`CANCELLED_CODE`](crate::cancel::CANCELLED_CODE). Never on its own a
/// reason to treat a turn as cancelled: the turn's token is the
/// authority. Without it, the structured code can come from any step
/// that sends it, the kernel's action ceiling stopping a step that had
/// not begun among them: that arrives as the code when the step unwinds
/// within the kernel's drop grace (100 ms past the deadline in gwead
/// 0.3.0) and as [`KernelError::ExecutionTimeout`] otherwise. The
/// ceiling never produces the typed shape — its
/// watchdog reports a step it stopped as the timeout — so without
/// the turn's token that shape can only come from a step that
/// reports cancellation on a token that has not fired, which the
/// kernel warns about and reports as cancelled all the same,
/// directly or through a callee. No step in this repo does, and a
/// guest cannot.
fn reports_cancellation(e: &KernelError) -> bool {
    match e {
        KernelError::Cancelled { .. } => true,
        KernelError::PluginError { code, .. } => code == crate::cancel::CANCELLED_CODE,
        _ => false,
    }
}

/// A cancellation the turn did not ask for leaves a footprint.
fn warn_if_unrequested(e: &KernelError) {
    if reports_cancellation(e) {
        tracing::warn!(error = %e, "a cancellation the turn did not request: the kernel's action ceiling stopped a step that had not begun, the plugin reported one itself, or a step reported cancellation on a token that had not fired");
    }
}

/// What the model is told about a call it made in a turn it then
/// refused to continue.
const REFUSED: &str = "not run: the model's turn ended in a refusal";

/// What a failed stream read means for the turn. The reader reads the
/// turn's token with each read's code, so a cancellation arrives
/// already decided — a plain one, or one that hid a source failure,
/// whose recorded text is carried beside it and warned about; without
/// the token, the source's failure ends the turn with the text the
/// kernel recorded for it. A handle-shaped code — closed, wrong
/// direction, unknown — cannot arise from a table the loop owns, and a
/// contract violation is reported as what it is, cut or no cut.
fn stream_read_failure(e: ReadError) -> TurnError {
    match e {
        ReadError::Cancelled {
            detail: Some(detail),
        } => {
            tracing::warn!(
                detail = %detail,
                "the stream's source failed under the turn's cancellation"
            );
            TurnError::Cancelled {
                detail: Some(detail),
            }
        }
        ReadError::Cancelled { detail: None } => TurnError::Cancelled { detail: None },
        ReadError::SourceFailed { detail } => {
            tracing::warn!(
                detail = stream::recorded(&detail),
                "the stream's source failed mid-turn"
            );
            TurnError::StreamFailed { detail }
        }
        other => TurnError::Contract(other.to_string()),
    }
}

/// An assistant message with no content blocks at all.
fn is_empty_message(message: &Value) -> bool {
    message["content"].as_array().is_some_and(Vec::is_empty)
}

/// A completed round from its pieces — the shared tail of the buffered
/// form and of the stream's `end` event.
fn round_from(
    message: Value,
    stop_reason: Option<&Value>,
    usage: Option<&Value>,
) -> Result<Round, String> {
    let calls = check_assistant_message(&message)?;
    let stop_reason = StopReason::parse(stop_reason.unwrap_or(&Value::Null))?;
    let usage = Usage::parse(usage.unwrap_or(&Value::Null))?;
    Ok(Round {
        message,
        calls,
        stop_reason,
        usage,
    })
}

/// Read the buffered form of `chat`'s output.
fn interpret_buffered(out: &Value) -> Result<RoundResult, TurnError> {
    let Some(fields) = out.as_object() else {
        return Err(TurnError::Contract(format!(
            "chat output is not an object: {out}"
        )));
    };
    if let Some(error) = fields.get("error") {
        if fields.len() != 1 {
            return Err(TurnError::Contract(
                "the failed form carries fields beside `error`".into(),
            ));
        }
        let Some(error) = error.as_object() else {
            return Err(TurnError::Contract(format!(
                "`error` is not an object: {error}"
            )));
        };
        return Failure::parse(error, false)
            .map(RoundResult::Failed)
            .map_err(TurnError::Contract);
    }
    if fields.contains_key("stream") {
        return Err(TurnError::Contract(
            "the streamed form came back for a buffered request".into(),
        ));
    }
    for key in fields.keys() {
        if !matches!(key.as_str(), "message" | "stop_reason" | "usage") {
            return Err(TurnError::Contract(format!(
                "chat output carries a field the contract does not allow: {key:?}"
            )));
        }
    }
    let message = fields
        .get("message")
        .ok_or_else(|| TurnError::Contract("chat output has no `message`".into()))?;
    round_from(
        message.clone(),
        fields.get("stop_reason"),
        fields.get("usage"),
    )
    .map(RoundResult::Message)
    .map_err(TurnError::Contract)
}

impl std::fmt::Debug for RoundResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RoundResult::Message(r) => write!(f, "Message({:?})", r.stop_reason),
            RoundResult::Failed(e) => write!(f, "Failed({e})"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session and the future of its turn can move to a spawned task:
    /// nothing a seam added is `!Send`. A compile-time check; the
    /// function is never called.
    #[test]
    fn a_session_and_its_turn_are_send() {
        fn assert_send<T: Send>(_: &T) {}
        fn check(session: &mut Session, cancel: &CancellationToken) {
            assert_send(session);
            assert_send(&session.turn("x", cancel));
        }
        let _ = check;
    }

    #[test]
    fn failures_and_stop_reasons_are_read_fail_closed() {
        let f = Failure::parse(
            json!({"message": "m", "retryable": true, "kind": "k"})
                .as_object()
                .unwrap(),
            false,
        )
        .unwrap();
        assert_eq!(f.retryable, Some(true));
        assert_eq!(f.to_string(), "m (k)");
        let f = Failure::parse(json!({"message": "m"}).as_object().unwrap(), false).unwrap();
        assert_eq!(f.retryable, None);
        assert_eq!(f.to_string(), "m");
        // The stream event carries `type`; the buffered value does not.
        Failure::parse(
            json!({"type": "error", "message": "m"})
                .as_object()
                .unwrap(),
            true,
        )
        .unwrap();
        Failure::parse(
            json!({"type": "error", "message": "m"})
                .as_object()
                .unwrap(),
            false,
        )
        .unwrap_err();
        Failure::parse(json!({"retryable": true}).as_object().unwrap(), false).unwrap_err();
        Failure::parse(
            json!({"message": "m", "retryable": "yes"})
                .as_object()
                .unwrap(),
            false,
        )
        .unwrap_err();
        Failure::parse(
            json!({"message": "m", "status": 429}).as_object().unwrap(),
            false,
        )
        .unwrap_err();

        assert_eq!(
            StopReason::parse(&json!("refusal")).unwrap(),
            StopReason::Refusal
        );
        StopReason::parse(&json!("pause_turn")).unwrap_err();
        StopReason::parse(&json!(null)).unwrap_err();
    }

    #[test]
    fn a_stream_read_failure_is_read_by_its_code() {
        use gwead::kernel::streams::{STREAM_CLOSED, STREAM_INVALID_HANDLE};

        // The reader has already read the token with the code, so a
        // cancellation arrives decided; the loop carries the text it
        // hid and nothing else changes.
        let cancelled = stream_read_failure(ReadError::Cancelled { detail: None });
        assert!(matches!(cancelled, TurnError::Cancelled { detail: None }));
        assert_eq!(cancelled.to_string(), "cancelled");

        let cancelled = stream_read_failure(ReadError::Cancelled {
            detail: Some("no response data for 50ms".into()),
        });
        assert!(
            matches!(&cancelled, TurnError::Cancelled { detail: Some(d) } if d == "no response data for 50ms")
        );
        assert_eq!(
            cancelled.to_string(),
            "cancelled; the stream's source failed with: no response data for 50ms"
        );

        // Without a fired token, a source failure ends the turn with
        // the recorded text, or the fallback when none was recorded.
        let failed = stream_read_failure(ReadError::SourceFailed {
            detail: Some("no response data for 50ms".into()),
        });
        assert!(
            matches!(&failed, TurnError::StreamFailed { detail: Some(d) } if d == "no response data for 50ms")
        );
        assert_eq!(
            failed.to_string(),
            "the stream failed before the turn did: no response data for 50ms"
        );
        let failed = stream_read_failure(ReadError::SourceFailed { detail: None });
        assert!(matches!(&failed, TurnError::StreamFailed { detail: None }));
        assert!(failed.to_string().contains("the kernel recorded no text"));

        // A handle-shaped code is about the handle, not its source,
        // and cannot arise from a table the loop owns.
        for code in [STREAM_CLOSED, STREAM_INVALID_HANDLE] {
            match stream_read_failure(ReadError::Io(code)) {
                TurnError::Contract(msg) => assert!(msg.contains(&code.to_string()), "{msg}"),
                other => panic!("{other}"),
            }
        }

        // A contract violation is still a contract violation.
        assert!(matches!(
            stream_read_failure(ReadError::NotJson("x".into())),
            TurnError::Contract(_)
        ));
        assert!(matches!(
            stream_read_failure(ReadError::TooLong { cap: 8 }),
            TurnError::Contract(_)
        ));
    }

    #[test]
    fn a_cut_is_read_from_a_steps_structured_error_only() {
        let withdrawn = KernelError::PluginError {
            code: crate::cancel::CANCELLED_CODE.into(),
            message: "cancelled".into(),
            params: json!({"phase": crate::cancel::CANCELLED_BEFORE_START}),
        };
        assert_eq!(Cut::from_error(&withdrawn), Cut::BeforeStart);
        let in_work = KernelError::PluginError {
            code: crate::cancel::CANCELLED_CODE.into(),
            message: "cancelled".into(),
            params: Value::Null,
        };
        assert_eq!(Cut::from_error(&in_work), Cut::WhileRunning);
        assert!(reports_cancellation(&in_work));
        let kernels_own = KernelError::Cancelled {
            at_step: "s".into(),
        };
        assert_eq!(Cut::from_error(&kernels_own), Cut::WhileRunning);
        assert!(reports_cancellation(&kernels_own));
        // A nested invoke flattens the code into text: cautious, and
        // not a cancellation report.
        let flattened = KernelError::Execution(
            "invoke failed: Plugin error gwennol.cancelled: cancelled".into(),
        );
        assert_eq!(Cut::from_error(&flattened), Cut::WhileRunning);
        assert!(!reports_cancellation(&flattened));
    }

    #[test]
    fn usage_needs_both_counters_and_tolerates_extras() {
        let u = Usage::parse(
            &json!({"input_tokens": 1, "output_tokens": 2, "cache_read_input_tokens": 3}),
        )
        .unwrap();
        assert_eq!(
            u,
            Usage {
                input_tokens: 1,
                output_tokens: 2
            }
        );
        Usage::parse(&json!({"input_tokens": 1})).unwrap_err();
        Usage::parse(&json!({"input_tokens": -1, "output_tokens": 2})).unwrap_err();
        let mut sum = Usage::default();
        sum.add(u);
        sum.add(u);
        assert_eq!(
            sum,
            Usage {
                input_tokens: 2,
                output_tokens: 4
            }
        );
    }

    #[test]
    fn the_buffered_form_is_read_fail_closed() {
        let ok = json!({
            "message": {"role": "assistant", "content": [{"type": "text", "text": "hi"}]},
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        });
        assert!(matches!(
            interpret_buffered(&ok).unwrap(),
            RoundResult::Message(_)
        ));
        let failed = json!({"error": {"message": "no", "retryable": false}});
        assert!(matches!(
            interpret_buffered(&failed).unwrap(),
            RoundResult::Failed(_)
        ));

        for (bad, why) in [
            (json!({"stream": 1}), "streamed form"),
            (
                json!({"error": {"message": "m"}, "usage": {}}),
                "beside `error`",
            ),
            (json!({"error": "m"}), "not an object"),
            (
                json!({"message": {"role": "assistant", "content": []}, "stop_reason": "end_turn"}),
                "usage",
            ),
            (
                json!({"message": {"role": "assistant", "content": []}, "stop_reason": "later", "usage": {"input_tokens": 0, "output_tokens": 0}}),
                "stop_reason",
            ),
            (
                json!({"message": {"role": "assistant", "content": []}, "stop_reason": "end_turn", "usage": {"input_tokens": 0, "output_tokens": 0}, "id": "m"}),
                "does not allow",
            ),
            (json!([]), "not an object"),
        ] {
            match interpret_buffered(&bad) {
                Err(TurnError::Contract(msg)) => assert!(msg.contains(why), "{bad}: {msg}"),
                other => panic!("{bad}: expected a contract error, got {other:?}"),
            }
        }
    }
}
