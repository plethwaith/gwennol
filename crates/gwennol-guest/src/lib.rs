//! Write a Gwennol plugin's guest code in Rust.
//!
//! Gwead's `script` step hands a step's `source` string to whatever wasm
//! module is registered for the step's `language` — and that module's
//! contract (`alloc`/`execute`/`memory` exports, host imports from the
//! `gwead1` module) nowhere requires that anything be *interpreted*. This
//! crate exploits exactly that: a plugin's guest logic is ordinary Rust,
//! compiled to `wasm32-unknown-unknown`, occupying the interpreter's
//! slot, with the step's `source` string serving as an entry-point
//! selector rather than as code. The decision record is
//! `docs/SUBSTRATE.md` in the Gwennol repository.
//!
//! A guest crate is a `cdylib` that declares its entry points once:
//!
//! ```ignore
//! use gwennol_guest::{Args, entrypoints};
//! use serde_json::{Value, json};
//!
//! fn shape(args: Args) -> Result<Value, String> {
//!     let who = args.field("who").and_then(Value::as_str).unwrap_or("world");
//!     Ok(json!({ "greeting": format!("hello, {who}") }))
//! }
//!
//! entrypoints! {
//!     "shape" => shape,
//! }
//! ```
//!
//! and a manifest action runs it with a step of
//! `{"type": "script", "language": "<plugin-name>", "source": "shape"}`.
//!
//! # What an entry point can reach
//!
//! - [`Args`] — the step's resolution context: the action's input
//!   fields, `config`, the secrets named by the step's `passSecrets`
//!   allowlist, and prior steps' results.
//! - [`Stream`] — byte-stream I/O on Gwead stream handles, including
//!   the pre-provisioned output of a `long_running` step in a
//!   `dataflow: true` action ([`Stream::output`]), with NDJSON line
//!   framing ([`Stream::write_json_line`]) for contract events.
//! - [`sse`] — an incremental server-sent-events parser, for the
//!   provider-shaped guests that relay a vendor stream.
//! - [`invoke`]/[`invoke_streaming`] — dispatch back into the kernel,
//!   into another action of the same plugin (always permitted) or into
//!   another plugin or role (gated by the manifest's `invoke:*` grants).
//! - [`log`] and [`cancelled`] — host-side tracing and the step's
//!   cancellation token.
//!
//! Returning `Ok(value)` makes `value` the step's result; returning
//! `Err(message)` fails the step. Panics abort the wasm instance and
//! surface as an opaque trap, so prefer `Err` for anything a caller
//! might want to read.
//!
//! # ABI coupling
//!
//! The raw imports live in [`sys`] and target Gwead's script-runtime ABI
//! version 1 (import module `"gwead1"`). Gwead documents the ABI in its
//! `STREAMS_ABI.md`; until Gwead 1.0 the ABI may change between
//! releases, and guest modules must be rebuilt against the kernel
//! version that hosts them. This crate is where such a change lands.
//!
//! # Host builds
//!
//! The crate compiles on non-wasm targets so that workspace-wide
//! formatting, linting, docs, and the pure-logic unit tests cover it —
//! but every function that crosses the ABI panics there. Nothing in the
//! host process should ever call into this crate.

mod args;
mod entry;
mod invoke;
pub mod sse;
mod stream;
pub mod sys;

pub use args::Args;
pub use entry::{__alloc_impl, __execute_impl, EntryFn, dispatch};
pub use invoke::{Target, invoke, invoke_streaming};
pub use stream::{Delivery, Stream, StreamError};

/// Severity for [`log`], mapped onto the host's `tracing` levels.
///
/// The numeric mapping (debug 0, info 1, warn 2, error 3) is the
/// script-runtime ABI's; the host treats unknown values as info.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Host-side `tracing::debug!`.
    Debug,
    /// Host-side `tracing::info!`.
    Info,
    /// Host-side `tracing::warn!`.
    Warn,
    /// Host-side `tracing::error!`.
    Error,
}

impl Level {
    fn code(self) -> i32 {
        match self {
            Level::Debug => 0,
            Level::Info => 1,
            Level::Warn => 2,
            Level::Error => 3,
        }
    }
}

/// Emit a message into the host's tracing subscriber (target
/// `gwead::script_runtime`).
///
/// This is a diagnostic side channel, not an output: nothing in the
/// step's result reflects it.
pub fn log(level: Level, message: &str) {
    sys::host_log(level.code(), message.as_bytes());
}

/// Whether an HTTP status is worth repeating unchanged — the one
/// answer every guest that fills the contract's `retryable` from a
/// status gives, so two guests cannot disagree about a 409. Timeouts
/// (408), contention (409), rate limits (429) and server-side failures
/// (5xx) are worth a retry; every other client error will fail again.
/// The same set the vendor SDKs retry on.
pub fn retryable_http_status(status: i64) -> bool {
    matches!(status, 408 | 409 | 429) || (500..=599).contains(&status)
}

/// Whether the parent step's cancellation token has fired.
///
/// Long-running loops should poll this between chunks and wind down
/// promptly when it turns true — the host's wallclock watchdog and the
/// embedder's cancel both arrive through it.
pub fn cancelled() -> bool {
    sys::is_cancelled() != 0
}
