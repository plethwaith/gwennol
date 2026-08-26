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
  a `sh -c` leaves no orphans; `host_fs.list` caps the entries it returns —
  each pinned by an integration test against a real kernel.
- **Not in scope:** any plugin that *uses* these steps; the agent loop; a
  sandbox around an approved child process.
- **Open question to settle here:** symlinks. The operator is shown a
  lexically normalised path, which may still be a symlink out of the
  workspace, and nothing re-checks between approval and open. Interactive
  review tolerates that; a policy file matching on paths (milestone 6) does
  not.

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
- **Open question to settle here:** how a tool's input schema reaches the
  model — derived from the `TOOL` plugin's manifest, or declared separately.

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
- **Not in scope:** the Anthropic provider itself; installing plugins from
  outside the binary.

### 4. Provider and tools

- **Produces:** `provider-anthropic`, implementing `LLM_CHAT` over
  `host_http.post` with SSE; the `read`, `write`, `grep` and `bash` tools,
  each composed from host steps.
- **Done when:** the provider streams a response against a stub HTTP server;
  every tool manifest declares only the host step types it actually uses; a
  model-issued tool call executes end to end against a stubbed provider.
- **Not in scope:** the loop; any frontend.

### 5. Agent loop

- **Produces:** the loop in `gwennol-core` — turn, provider call, tool calls,
  results, repeat — setting the execution context per tool call so approvals
  can name their cause; cancellation wired to the kernel's token.
- **Done when:** a multi-turn conversation with tool calls runs against a
  stubbed provider; a failing tool is reported *to the model* rather than
  ending the turn; cancelling mid-stream tears the turn down cleanly.
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
  and any guest modules inside the binary.
- Installing plugins from outside the binary. This is where the sandboxing
  thesis pays off, and where questions that are moot for bundled plugins stop
  being moot: which plugins may claim a `(script, <language>)` slot, and
  whether a manifest should have to declare that a plugin runs scripts at
  all.
- A sandbox around approved child processes.
