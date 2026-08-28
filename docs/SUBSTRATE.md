# The plugin substrate

Milestone 3's question: Gwead bundles no script runtime, so how does a
plugin that *needs code* — parsing a chunked body, shaping a JSON
request — get written? The roadmap offered two shapes: (a) a minimal
Rust → wasm32 guest helper, or (b) a bundled interpreter plugin
claiming a `(script, <language>)` slot.

**Decided: (a).** A non-declarative plugin's guest code is ordinary
Rust, compiled to `wasm32-unknown-unknown`, registered as the plugin's
own "script runtime", with the step's `source` string selecting an
entry point rather than carrying code. The helper is
[`crates/gwennol-guest`](../crates/gwennol-guest); the worked example
is [`crates/sse-guest`](../crates/sse-guest) with its integration
suite at `crates/gwennol-core/tests/guest_substrate.rs`.

## The reality the decision was made against

Three facts, each verified in Gwead's source rather than assumed:

1. **The `wasm` step type is compute-only.** Its module is instantiated
   against an *empty* linker — no stream imports, no result-reporting
   import, `() -> ()` entry, `null` result. Nothing that needs I/O can
   live there.
2. **The script-runtime ABI is the only guest surface with reach.** A
   module in that slot gets the full `gwead1` import set: stream
   read/write/close, the pre-provisioned `long_running` dataflow
   output, `host_invoke`/`host_invoke_streaming` back into the kernel,
   result/error reporting, logging, and the cancellation flag. The
   roadmap's milestone-3 constraint (streaming needs guest code
   concurrent with its consumer) is satisfiable *only* here — so both
   candidate substrates were always going to target this one ABI.
3. **The "interpreter" contract does not require interpreting.** The
   slot's contract is three exports (`alloc`, `execute`, `memory`) and
   an opaque `source` string. Gwead's own test suite registers a
   `wat`-built stub in the slot; a Rust module that treats `source` as
   an entry-point name honours the contract exactly.

Given fact 3, option (b) strictly contains option (a): a bundled
interpreter is *also* a wasm module somebody writes against the
`gwead1` imports — the binding layer is the same work — plus an entire
interpreter, plus the actual plugin logic rewritten in untyped strings
inside manifest JSON, unreachable by `cargo test`, `clippy`, or
`rustfmt`. And the milestone's done-when requires the module built by
a documented command CI runs, not a committed blob: `cargo build
--target wasm32-unknown-unknown` is that command natively, where a C
interpreter (Lua, QuickJS) would drag a wasi-sdk/emscripten toolchain
into CI or force the forbidden blob.

Nothing is foreclosed. When plugins become installable from outside
the binary (see the roadmap's "Beyond the MVP") and third-party
authors want a scripting language, an interpreter is *just another
plugin* registered through exactly the machinery this milestone built —
the trust gate, the `(script, <language>)` slot, the build-injection
pipeline. Choosing (a) now defers (b); it does not reject it.

## How a guest-backed plugin is put together

A guest-backed plugin is one artifact in two halves: a JSON manifest,
and a Rust crate compiled to wasm32 whose bytes travel inline in that
manifest.

### The guest crate

A `cdylib` depending on `gwennol-guest`:

```rust
use gwennol_guest::{Args, entrypoints};
use serde_json::{Value, json};

fn chat(args: Args) -> Result<Value, String> { /* … */ }
fn relay_sse(args: Args) -> Result<Value, String> { /* … */ }

entrypoints! {
    "chat" => chat,
    "relay_sse" => relay_sse,
}
```

`entrypoints!` generates the slot's `alloc`/`execute` exports; the
step's `source` string picks the entry. An entry returns
`Ok(value)` as the step's result or `Err(message)` as its failure;
panics surface as opaque wasm traps, so `Err` is the readable path.
Keep the parsing and shaping logic in pure functions — they unit-test
on the host target, and only the thin stream/invoke glue is
wasm-only.

### The manifest

```jsonc
{
  "name": "sse-guest",
  "permissions": [
    "provide:step_type:script:sse-guest",   // claim the runtime slot
    "step_type:host_http.post",             // plus whatever it reaches
    "network:egress:127.0.0.1"
  ],
  "wasmModules": { "guest": { "base64": "…" } },
  "stepTypeImpls": [
    { "stepType": "script", "matches": "sse-guest", "wasmModule": "guest" }
  ],
  "actions": {
    "chat": { "steps": [
      { "id": "run", "type": "script",
        "params": { "language": "sse-guest", "source": "chat" } }
    ]}
  }
}
```

**Naming convention: the `language` selector is the plugin's own
name.** The selector namespace is kernel-global, plugin names are
registry-unique, and `{"language": "sse-guest"}` reads as "run in
sse-guest's module" — which is the truth. A guest-backed plugin runs
its *own* module; nothing here shares runtimes between plugins.

### The trust gate

Supplying a script runtime is double-keyed, and both keys are
deliberate acts:

1. the manifest declares `provide:step_type:script:<name>`;
2. the embedder lists the plugin in
   `HostConfig::trusted_step_type_providers` at boot.

Gwead refuses the registration if either is missing. The second key
exists because supplying a runtime means other manifests' script steps
of that language would execute inside this plugin's code — listing a
name is a trust statement about code. Under the naming convention
above the blast radius is already narrow (a plugin's language is its
own name, so only its own steps select its module), but the gate is
enforced regardless, and `gwennol_core::boot` trusts nobody: bundling
a guest-backed plugin means going through `boot_with` and saying so.

Secrets stay narrow the same way: a script step's `passSecrets`
allowlist defaults to *empty*, so a guest sees no credentials unless
the step names the keys — visible in the manifest an operator reviews.

### The streaming composition

The roadmap's milestone-3 constraint, now demonstrated end to end by
the example: streams are reachable only from this ABI, and a streaming
provider is two actions on one plugin —

- `chat`, a plain action with one script step. The guest entry builds
  the vendor request and calls
  `invoke_streaming(Target::Plugin(self), "stream_turn", …)` —
  self-invocation needs no grant — then returns `{"stream": handle}`.
  The handle is minted in the calling invocation's stream table, which
  is the embedder-supplied table when the loop runs
  `execute(…).with_streams(…)`, so it is exactly the `LLM_CHAT`
  contract's streamed output and stays readable after `chat` returns.
- `stream_turn`, a `dataflow: true` action: a streaming
  `host_http.post` step, then a `longRunning` script step (with an
  explicit `dependsOn` on the fetch — dataflow steps run concurrently,
  and the relay must not start before the fetch's handle exists in its
  context) whose guest entry reads the SSE bytes and writes contract
  NDJSON to `Stream::output()`. The callee runs on a background task;
  a failure there surfaces to the consumer as end-of-stream without an
  `end` event — the contract's failed-turn shape, with no extra
  plumbing.

## Building

```sh
rustup target add wasm32-unknown-unknown   # once
cargo build -p sse-guest --target wasm32-unknown-unknown --release
```

No compiled module is committed. The integration suite runs the build
itself (same `cargo`, separate `--target-dir` so the outer test run's
lock and the guest build's lock never meet) and injects the bytes into
the manifest at test time; CI compiles the guest from source on every
run. When milestone 4 bundles real guest-backed plugins under
[`plugins/`](../plugins/), the same injection moves into the build:
the JSON file in the repo stays the plugin, and the packaging step
fills its `wasmModules` slot from the compiled artifact.

## ABI coupling

The imports live in one place, `gwennol_guest::sys`, and target
Gwead's script-runtime ABI version 1 — the wasm import module name
`"gwead1"` *is* the version handshake, so a kernel speaking a
different ABI fails module instantiation deterministically rather than
misbehaving mid-stream. Until Gwead 1.0 that ABI may change between
releases and guest modules must be rebuilt against the hosting kernel
version; when it changes, `sys` is where the bump lands, and every
guest crate picks it up by rebuilding.

## Accepted costs

- **Per-step instantiation.** The slot's contract instantiates the
  module fresh for every script step. For the harness's call shapes
  (one instantiation per model turn, roughly) this is noise; a
  workload that hurt would argue for kernel-side caching, not a
  different substrate.
- **Panics are opaque.** A guest panic is a wasm trap with no message
  worth reading. The helper's API is `Result`-shaped so the readable
  path is also the natural one.
- **Rust is the authoring bar.** Fine while every guest is bundled and
  first-party; the day third-party authoring matters is the day the
  interpreter-as-a-plugin option gets its hearing.
