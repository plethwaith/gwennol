# Roadmap

Gwennol's thesis is narrow: in a coding agent, the tools and the model
providers should be sandboxed plugins whose manifests state exactly what they
can reach, with the host making no policy decisions of its own. Everything
below serves that. The goal is the smallest thing that proves it — not a
feature-complete rival to existing harnesses.

## Architecture (decided)

- **Two layers.** `gwennol-core` is the host library: Gwead `KernelConfig`,
  the native host step types, bundled SPI and plugin manifests, the agent
  loop, and the `Operator` trait. Frontends implement `Operator`
  (approve / secret / emit / input) and own *all* policy. CLI first, then a
  TUI, later web/desktop — each a second `Operator`, never a rewrite.
- **Everything model-shaped is a plugin.** The `LLM_CHAT` SPI is implemented
  by provider plugins using `host_http.post`; there is no native LLM code.
  Tools are tiny plugins composing `host_fs.*` and `host_process.run`.
- **Two gates on every out-of-sandbox step**, in order: the manifest
  (kernel-enforced `step_type:` grants, plus `network:egress:<host>` which
  the host step checks explicitly), then the operator, shown the concrete
  path, argv or URL, the plugin asking, and the model's tool call behind it.
  Per *hop*, not per step: the HTTP client follows no redirects of its own,
  so a `Location` faces both gates like any other request, and one that
  leaves the origin travels without the plugin's headers.
- **No ambient authority.** A plugin reaches only what its manifest declares
  and the operator allows. A spawned child therefore gets the environment the
  frontend's policy describes, not the one the agent was launched from.
- **Policy stays in the frontend.** The approval surface is the trust root;
  it is not itself a plugin. An advisory policy plugin may come later.

### Naming

Gwead enforces the shape, so this is grammar rather than convention. A step
type Gwead does not itself define must be dotted, and the part before the dot
must be the declaring plugin's own name — bare names are reserved for kernel
intrinsics, so a plugin cannot shadow one. Step types therefore arrive as
`<plugin>.<name>`, and the host's are split across three small plugins:

| Plugin | Step types |
| --- | --- |
| `host_fs` | `host_fs.read`, `host_fs.write`, `host_fs.list` |
| `host_process` | `host_process.run` |
| `host_http` | `host_http.get`, `host_http.post` |

The shared `host_` prefix is the audit surface: everything that can reach
outside the sandbox is one grep (`step_type:host_`) across every manifest in
a deployment, and it stays one grep when a fourth host plugin appears. HTTP
is split by method rather than taking a `method` parameter so that a tool
that only fetches can be *provably* read-only in its manifest; the remaining
methods get step types when something needs them.

SPI role names are a different grammar: they are plain identifiers and may
not contain dots (`LLM_CHAT`, `TOOL`), because roles appear inside permission
strings.

## Milestones

Each milestone names what it produces and the test that proves it. "Not in
scope" is load-bearing: it is what keeps a milestone finishable.

### 1. Native host steps

The only code in Gwennol that touches the filesystem, spawns processes, or
opens sockets.

- **Produces:** the `host_fs`, `host_process` and `host_http` plugin
  manifests and their native step bodies; the `Operator` trait; the process
  environment policy; the execution context that carries a tool call down to
  an approval; `boot` / `boot_with`.
- **Done when:** the kernel refuses an ungranted step type *before* the
  operator is asked; every step asks the operator with its concrete
  arguments; an HTTP redirect faces both gates again per hop and loses the
  plugin's headers when it leaves the origin; a spawned child receives only
  the allow-listed environment; an approval raised two plugins deep still
  names the tool call that caused it; `host_fs.read` bounds what it reads
  rather than allocating the whole file and truncating afterwards;
  `host_fs.write` writes through a temporary file and renames, so a crash or
  a full disk cannot truncate the user's source; a timed-out
  `host_process.run` kills the child's process group, not just the child, so
  a `sh -c` leaves no orphans (best-effort: a descendant that detaches
  into its own session is out of the group's reach); `host_fs.list` caps
  the entries it returns —
  each pinned by an integration test against a real kernel.
- **Not in scope:** any plugin that *uses* these steps; the agent loop; a
  sandbox around an approved child process.
- **Settled: an approval binds to the real file, not the name.**
  `host_fs.read` opens the file, canonicalises the path, verifies by device
  and inode that the canonical path names the opened handle, shows the
  operator that canonical path, and reads from that same handle — a symlink
  into `~/.ssh` is judged as `~/.ssh`, and the bytes provably come from the
  approved file. `host_fs.write` refuses a symlink destination outright and
  canonicalises the deepest existing ancestor, so the approved path is
  where the bytes will land; `host_fs.list` lists the canonical directory.
  The residual race — a parent directory swapped between approval and
  rename — is tolerable under interactive review; the milestone-6 policy
  file should close it with directory-handle (`openat`-family) I/O.

### 2. SPI contracts

The role contracts every provider and tool implements, fixed before anything
implements them.

- **Produces:** `plugins/spi/llm_chat.json` and `plugins/spi/tool.json`; the
  tool-call wire shape written down in `docs/SPI.md`; a fixture provider and
  fixture tool implementing each role.
- **Done when:** the fixture provider and fixture tool are dispatched *by
  role* through the kernel in an integration test; a streamed response is
  exercised end to end through a stream handle; a tool call round-trips from
  provider output to tool input to tool result with no agent loop involved.
- **Not in scope:** any real provider or tool; the substrate decision below.
- **Settled: a tool's input schema is derived from the manifest.** The
  `tool` block on the implementing plugin's `call` action is the single
  declaration of the tool's name, description and argument schema; the
  embedder harvests Gwead's tool descriptors and hands them to the model
  as `chat`'s `tools` input. A second declaration in the contract would
  only be a copy that can drift — the manifest is already the audit
  surface, so it is also the source of truth. See `docs/SPI.md`.

### 3. Plugin substrate

The four tools are plausibly pure declarative manifests — host steps plus
Gwead's control-flow intrinsics, no guest code at all. A provider is not: it
has to parse server-sent events and shape JSON. Gwead bundles no script
runtime (a language runtime is itself an ordinary plugin supplying an
interpreter module), so this milestone picks how non-declarative plugins get
written, and it must be settled before a provider exists.

- **Produces:** the decision, recorded in `docs/`, and whichever of:
  (a) a minimal Rust → `wasm32` guest helper plus one example plugin, or
  (b) a bundled interpreter plugin claiming a `(script, <language>)` slot.
- **Done when:** one non-trivial plugin — parses a chunked body, builds a
  JSON request — runs sandboxed in a test, built by a documented command that
  CI runs, rather than from a blob committed to the repo.
- **Constraint (verified against Gwead, so the decision is made against
  reality):** a *streaming* provider needs guest code running concurrently
  with its consumer, and Gwead's streams are reachable only from the
  script-runtime ABI — the wasm *step-type* ABI has no stream imports. The
  feasible shape is: `chat` stays a plain action that calls
  `io.invoke_streaming` on a sibling `dataflow` action, whose single
  long-running guest step reads the SSE bytes and writes contract NDJSON;
  the readable end lands in the caller's own stream table, and a callee
  failure surfaces as early end-of-stream — exactly the contract's
  failed-turn rule. Whichever substrate is chosen must be able to occupy
  that long-running slot.
- **Not in scope:** the Anthropic provider itself; installing plugins from
  outside the binary.
- **Settled: Rust guests, not a bundled interpreter** — decision record
  in [docs/SUBSTRATE.md](SUBSTRATE.md). The script-runtime slot's
  contract nowhere requires interpreting: a plugin's guest logic is
  ordinary Rust compiled to `wasm32-unknown-unknown`
  (`crates/gwennol-guest` is the helper), registered as the plugin's
  own runtime under its own name, with the step's `source` string
  selecting an entry point. An interpreter would have been the same
  binding work *plus* an interpreter, with the plugin logic itself
  demoted to untyped strings in JSON; it remains available later as
  just another plugin, through the same trust gate and slot, when
  third-party authoring warrants it. Supplying a runtime is double-
  keyed — the manifest's `provide:` grant and the embedder's
  `trusted_step_type_providers` list at boot — and the example
  (`crates/sse-guest`) runs the constraint's exact composition end to
  end in CI, built from source by the test suite.

### 4. Provider and tools

- **Produces:** `provider-anthropic`, implementing `LLM_CHAT` over
  `host_http.post` with SSE; the `read`, `write`, `grep` and `bash` tools,
  each composed from host steps.
- **Done when:** the provider streams a response against a stub HTTP server;
  every tool manifest declares only the host step types it actually uses; a
  model-issued tool call executes end to end against a stubbed provider.
- **Owed to the `TOOL` contract:** an outcome the model should react to
  must reach the tool as *data*, never as an error to string-match — a
  declarative `try`/`catch` sees only English error text, which cannot
  safely separate "file not found" from "the operator said no". Where a
  host step lacks the data form (a `host_fs.read` miss), this milestone
  extends the host step rather than letting a tool match strings; it also
  writes the shared truncation convention for tool `content`. And the
  provider owes the buffered-path error taxonomy `docs/SPI.md` defers to
  here.
- **Not in scope:** the loop; any frontend.
- **Settled: outcomes are data.** Every `host_fs` step reports the
  answers a model can act on — `not_found`, `is_directory`,
  `not_a_directory`, `permission_denied`, a write's `is_symlink` — as a
  result whose `outcome` names them, with a one-line `message`; a miss
  is still approved first, under the path canonical to its deepest
  canonicalisable ancestor. The four tools are declarative — one host
  step and a branch on its outcome — and a test pins that no tool uses
  `try` and that each manifest's grants equal the host steps its steps
  use.
- **Settled: truncation is data too.** A tool reports `truncated: true`
  (`TOOL` 0.2.0) and never composes a marker; `spi::tool::render_content`
  appends the one shared marker before the model sees the result.
- **Settled: the buffered failure taxonomy** is the `LLM_CHAT` 0.2.0 (now 0.3.0)
  `{"error": Failure}` form — the vendor answered and said no, with
  `retryable` filled from its answer. What the provider cannot classify
  without reading error text stays a step error, uniformly fatal.
- **Settled: bundling.** A guest-backed manifest commits its module as
  `{"path": "crates/<name>"}`, a form the kernel refuses; `cargo xtask
  bundle` compiles the crate and fills the slot, the integration suite
  bundles through the same code, and no blob is ever committed.
- **Settled: thinking travels as an `opaque` block** (`LLM_CHAT`
  0.3.0). The milestone-2 exclusion was reopened here rather than left
  to a smoke test: the vendor's documentation is explicit that a
  tool-use turn must come back with its thinking blocks intact, and
  `provider-anthropic` sends no `thinking` field unless `$config`
  supplies one (absence is the setting every current model accepts;
  an explicit `disabled` is refused by the models that cannot turn
  thinking off), so thinking is on by the vendor's default. The
  provider carries each thinking block out as `opaque` and replays its
  own blocks verbatim; a consumer keeps them in place and never reads
  them. Anything it does drop — a block kind it does not know, another
  provider's opaque blocks — it logs. `pause_turn`, unknown stop
  reasons and missing usage counters are failures, never guesses.

### 5. Agent loop

- **Produces:** the loop in `gwennol-core` — turn, provider call, tool calls,
  results, repeat — setting the execution context per tool call so approvals
  can name their cause; cancellation wired to the kernel's token.
- **Done when:** a multi-turn conversation with tool calls runs against a
  stubbed provider; a failing tool is reported *to the model* rather than
  ending the turn; cancelling mid-stream tears the turn down cleanly.
- **Owed to milestone 3:** the loop harness is the first place a
  consumer-hangs-up turn becomes observable end to end (a step outcome
  the test can see), so it owes the pin on the example guest's
  reader-gone wind-down — the relay treating a closed output as a
  graceful stop, not a failed step — which milestone 3 could verify
  only by inspection.
- **Owed to milestone 4:** the assistant message the loop rebuilds from
  a stream and replays is the events in order — adjacent text coalesced,
  `tool_use` and `opaque` blocks whole and in place — and the loop
  renders tool results through `spi::tool::render_content`. Both are
  contract rules the loop is the first consumer of.
- **Not in scope:** any frontend beyond a test `Operator`.

### 6. Non-interactive CLI

Deliberately before the TUI, so the `Operator` contract is honest before any
prompt exists to paper over it.

- **Produces:** `gwennol-cli` implementing `Operator`; approvals from flags
  and a policy file; configuration and secret loading.
- **Done when:** a real task runs headlessly with every approval decision
  traceable to a flag or a policy rule, and no interactive prompt exists.
- **Not in scope:** the TUI.

### 7. TUI

- **Produces:** a ratatui frontend as a second `Operator`; interactive
  approvals; streamed output.
- **Done when:** an interactive session runs a task with prompts that name
  the tool call behind each request, and cancelling mid-stream works.

Milestones 1–7 are the MVP: a harness that can be pointed at a repository and
asked to change something, with every reach outside the sandbox declared and
approved.

## Beyond the MVP

Not scheduled, but known, and listed so they are not mistaken for oversights:

- Conversation persistence and resume, and context-window management — a
  coding session outlives a single turn.
- Distribution: `cargo install gwennol` has to carry the bundled manifests
  and any guest modules inside the binary — `target/bundle/`, the output
  of `cargo xtask bundle`, is the input to that, and today only the
  integration suite consumes it.
- Installing plugins from outside the binary. This is where the sandboxing
  thesis pays off, and where questions that are moot for bundled plugins stop
  being moot: which plugins may claim a `(script, <language>)` slot, and
  whether a manifest should have to declare that a plugin runs scripts at
  all.
- A sandbox around approved child processes.
