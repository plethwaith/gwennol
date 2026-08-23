//! Gwennol host library.
//!
//! Gwennol (Welsh: *shuttle*) is an agentic coding harness built on the
//! [Gwead](https://docs.rs/gwead) wasm plugin microkernel. This crate is the
//! host: it configures the kernel, provides the native step types that touch
//! the outside world, bundles the SPI and plugin manifests, and runs the agent
//! loop. Frontends (CLI, TUI, web) implement [`Operator`] and own all policy.
//!
//! # Layering
//!
//! ```text
//! frontend (gwennol-cli, …)   implements Operator: approve / secret / emit / input
//!         │
//! gwennol-core                KernelConfig, native steps, loop, bundled manifests
//!         │
//! gwead                       manifest resolution, sandboxing, step dispatch
//!         │
//! plugins                     tools + model providers, composed from host steps
//! ```
//!
//! Everything model-shaped lives in plugins: the `llm.chat` SPI is implemented
//! by provider plugins using the host's `http.*` steps, never by native code.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod operator;

pub use operator::{ApprovalRequest, Decision, Event, Operator, Turn};
