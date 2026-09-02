# Gwennol

**Gwennol** (Welsh: *shuttle*; also *swallow*) is an agentic coding harness
from [Plethwaith Labs](https://plethwaith.com), built on the
[Gwead](https://github.com/plethwaith/gwead) WebAssembly plugin microkernel.

The idea: in every coding agent today, tools run as host-native code with
permission prompts bolted on afterwards. In Gwennol, tools and model providers
are sandboxed Gwead plugins. A plugin's manifest declares the host step types
it may use, and the kernel refuses anything else at dispatch time — so the
manifest is an accurate statement of what a tool can reach, and "where can my
API key go?" is answered by reading ten lines of JSON.

## The two gates

Everything that reaches outside the sandbox — a file, a process, a socket —
goes through a host step type, and every one of those passes two gates in
order:

1. **The manifest**, enforced by the kernel. A plugin must hold
   `step_type:host_fs.read` (or `host_process.run`, or `host_http.post`, …)
   and, for HTTP, `network:egress:<host>`. Without the grant, dispatch is
   refused before any host code runs.
2. **The operator** — whichever frontend is in charge — shown the concrete
   path, argv or URL, the plugin asking, and the model's tool call behind it.

So the manifest says what a tool *can* reach; the operator decides what it
*may* reach right now. Because every host step type shares a `host_` prefix,
"what in this deployment can touch the outside world?" is one grep across the
manifests.

## Status

**Pre-alpha.** The native host step types (milestone 1), the
`LLM_CHAT`/`TOOL` role contracts (milestone 2, [docs/SPI.md](docs/SPI.md)),
the plugin substrate (milestone 3 — Rust guests compiled to wasm32,
[docs/SUBSTRATE.md](docs/SUBSTRATE.md)), the bundled plugins
(milestone 4 — the Anthropic provider and the `read`, `write`, `grep`
and `bash` tools, [plugins/](plugins/)) and the agent loop (milestone 5
— `gwennol_core::agent::Session`) exist and are exercised by
integration tests against a real kernel and a stubbed Messages API;
nothing user-facing runs yet — the non-interactive CLI is milestone 6.
The 0.0.0 release on crates.io is a name reservation.

See [docs/ROADMAP.md](docs/ROADMAP.md) for the architecture decisions, the
naming rules, and the seven milestones to a usable harness.

## Building and testing

```sh
rustup target add wasm32-unknown-unknown   # once — the tests compile the
                                           # guest plugins from source
cargo test --workspace
cargo xtask bundle                         # registrable manifests with their
                                           # guest modules filled, under target/bundle/
```

Everything else is stock `cargo`. The wasm target is the one extra
prerequisite; without it the guest builds fail with a message naming
this command. No compiled module is ever committed: a guest-backed
manifest under `plugins/` names the crate that builds it, and the
bundler fills the slot ([plugins/README.md](plugins/README.md)).

## Layout

```
crates/gwennol-core/   host library: kernel config, native host steps, loop, Operator trait
crates/gwennol-cli/    first frontend (non-interactive CLI)
crates/gwennol-guest/  guest-side helper for plugins written in Rust → wasm32
crates/sse-guest/      example guest plugin: SSE body in, contract NDJSON out
crates/provider-anthropic/  the bundled model provider's guest code
crates/xtask/          `cargo xtask bundle`: compile guests, fill manifests
plugins/               bundled SPI contracts, provider and tool manifests
docs/                  roadmap and design notes
```

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
