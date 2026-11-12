//! The Gwennol host.
//!
//! Gwennol (Welsh: *shuttle*) is an agentic coding harness built on the
//! [Gwead](https://docs.rs/gwead) wasm plugin microkernel. This crate is the
//! host: it configures the kernel, provides the native step types that touch
//! the outside world, and bundles the manifests that publish them. The agent
//! loop and the SPI contracts it consumes are in `gwennol-core`. Frontends
//! (CLI, TUI, web) implement [`Operator`] and own all policy.
//!
//! # Layering
//!
//! ```text
//! frontend (gwennol-cli, …)   implements Operator: approve / secret / emit / input
//!         │
//! gwennol-host                KernelConfig, native steps, host manifests
//!         │
//! gwennol-core                the loop, the SPI contracts, the event types
//!         │
//! gwead                       manifest resolution, sandboxing, step dispatch
//!         │
//! plugins                     tools + model providers, composed from host steps
//! ```
//!
//! Everything model-shaped lives in plugins: the `LLM_CHAT` SPI is implemented
//! by provider plugins using the host's `host_http.post` step, never by
//! native code.
//!
//! A frontend names both crates. It starts its session with
//! `Session::with_events`, handing it an [`OperatorEvents`] over the operator
//! it gave [`boot`], and drives it with [`run`] or with `Session::turn`.
//!
//! # The two gates
//!
//! A plugin reaches the outside world only through a `host_*` step — the
//! `host_fs`, `host_process` and `host_http` plugins publish them — and each
//! such step passes two gates in order:
//!
//! 1. **The manifest**, enforced by the kernel: the plugin must hold
//!    `step_type:host_fs.read` (or whichever step it uses) — and, for HTTP,
//!    `network:egress:<host>` — or dispatch is refused before any host code
//!    runs.
//! 2. **The operator**, asked by the host: [`Operator::approve`] sees the
//!    concrete path, argv, or URL, the name of the plugin asking, and the
//!    model's tool call that set it off (see [`gwennol_core::context`]).
//!
//! The manifest is therefore an accurate statement of what a plugin *can*
//! reach; the operator decides what it *may* reach right now.
//!
//! Both gates apply per *hop*, not per step: an HTTP redirect is a fresh
//! request to a host the plugin may never have declared, so it is resolved
//! by the host and run through both gates again rather than followed by the
//! HTTP client. What a plugin *cannot* do is inherit authority it never
//! declared — which is also why a spawned process gets the environment
//! [`ProcessEnv`] describes rather than the one the agent was launched
//! with.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod host;
pub mod kernel;
pub mod operator;
pub mod secrets;
pub mod session;
pub mod steps;

pub use host::{DEFAULT_ACTION_TIMEOUT, HostConfig, ProcessEnv};
pub use kernel::{
    BootError, HOST_FS_MANIFEST, HOST_HTTP_MANIFEST, HOST_MANIFESTS, HOST_PROCESS_MANIFEST, boot,
    boot_with,
};
pub use operator::{Access, ApprovalRequest, Decision, Operator, OperatorEvents, Turn};
pub use session::run;
