//! The Gwennol agent loop and the contracts it consumes.
//!
//! Gwennol (Welsh: *shuttle*) is an agentic coding harness built on the
//! [Gwead](https://docs.rs/gwead) wasm plugin microkernel. This crate is the
//! loop: a [`Session`] runs a conversation against one `LLM_CHAT` provider
//! and the `TOOL` plugins it offers, on any Gwead kernel that has the
//! [`spi`] contracts registered. It links no step bodies of its own: what a
//! plugin's steps do, and who is asked before they do it, belong to
//! whoever builds the kernel.
//!
//! # Layering
//!
//! ```text
//! frontend                    drives a Session, renders its events
//!         │
//! gwennol-core                the loop, the SPI contracts, the event types
//!         │
//! gwead                       manifest resolution, sandboxing, step dispatch
//!         │
//! plugins                     tools + model providers, composed from steps
//! ```
//!
//! Everything model-shaped lives in plugins: the `LLM_CHAT` SPI is implemented
//! by provider plugins, never by native code in this crate.
//!
//! A session sends each [`Event`] to the [`EventSink`] it was started with
//! ([`Session::new`]) and reads no process-global state, so several
//! sessions can share a kernel and keep their events apart.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod agent;
pub mod cancel;
pub mod content;
pub mod context;
pub mod events;
pub mod spi;

pub use agent::{
    Failure, RetryPolicy, Session, SessionConfig, SessionError, StopReason, TurnError, TurnOutcome,
    Usage, resolve_provider,
};
pub use content::{
    ContentError, Image, ImageError, ImageFormat, ImageReference, ImageReferenceError, UserContent,
};
pub use events::{Event, EventSink, ToolCall};
pub use gwead;
