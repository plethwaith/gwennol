# The SPI contracts

Gwennol defines two Gwead roles: `LLM_CHAT`, implemented by model provider
plugins, and `TOOL`, implemented by tool plugins. The contract documents
live in [`plugins/spi/`](../plugins/spi/) and are registered by
`gwennol_core::boot` before any plugin can claim a role. This page is the
prose half of those contracts: the wire shapes, and the rules the JSON
schemas cannot state.

The shapes below are Gwennol's own, not any vendor's. A provider plugin's
job is exactly the translation between this wire shape and its API.

## What the kernel does and does not enforce

Gwead checks a plugin against a role's contract **once, at registration**,
and the check is action presence: a plugin claiming `LLM_CHAT` without a
`chat` action is refused. Two consequences matter:

- **Order is load-bearing.** A claim on a role whose contract is not yet
  registered loads with only a warning, and is then never checked. `boot`
  registers both bundled contracts before returning, so this cannot happen
  to the bundled roles — but it is why the contracts must stay first in any
  future registration path.
- **Payloads are never validated by the kernel.** The `input`/`output`
  schemas in the contract documents are documentation and tooling input.
  Whoever dispatches is responsible for conformance — concretely, the
  milestone-5 loop must validate model-emitted tool arguments against the
  tool's declared schema before dispatching a `call`.

## `LLM_CHAT`

One required action, `chat`: one model turn.

### Request

```json
{
  "system": "optional system prompt",
  "messages": [ … ],
  "tools": [ {"name": "…", "description": "…", "input_schema": { … }} ],
  "max_tokens": 4096,
  "stream": false
}
```

`messages` alternate `user` and `assistant` roles (the system prompt is a
top-level field, not a message). A message's `content` is an array of
blocks:

| Block | Fields | Appears in |
| --- | --- | --- |
| `text` | `text` | either role |
| `tool_use` | `id`, `name`, `input` | `assistant` |
| `tool_result` | `tool_use_id`, `content`, `is_error?` | `user` |
| `opaque` | `provider`, `data` | `assistant` |

An `opaque` block is provider state the consumer carries but never
reads — a vendor's thinking block, which the vendor requires replayed
verbatim in a later tool-use turn, is the case it exists for. Produced
only by a provider, consumed only by a provider: a consumer keeps it in
its position in the assistant message and replays it unchanged — never
rendered, altered, reordered, or dropped — and assumes nothing about the
shape of `data`. `provider` names the producing plugin, so a provider
ignores blocks another produced (a conversation that changes provider
mid-way loses only what the new provider could not have used).

A `tool_use` block is the model asking for a tool: `id` names this call,
`name` names the tool, `input` is arguments per the tool's declared
schema. The caller runs the tool and carries the answer back as a
`tool_result` block — `tool_use_id` echoing the `id` — in the next `user`
message.

`tools` is what the model may call this turn, and each entry is a Gwead
tool descriptor on the wire: `input_schema` is the implementing action's
`tool.parameters`, verbatim (see `TOOL` below).

### The tool-call protocol

Rules a provider will otherwise enforce with a rejected request, so they
are contract here:

- An assistant message may carry several `tool_use` blocks (parallel
  calls), mixed with text. **Every** `tool_use` must be answered, and
  **all** the answers arrive in the immediately following `user` message
  — one `tool_result` per `tool_use`, none deferred to a later turn.
- In that user message, `tool_result` blocks come before any `text`
  blocks.
- `id` values are opaque and unique within a conversation. The loop
  echoes them; it never parses or fabricates them.
- The assistant message that asked for the tools is replayed exactly as
  the provider produced it, `opaque` blocks in place. A vendor that
  reasons before calling a tool checks that its reasoning came back
  with the call, and refuses the turn when it did not.
- A streamed turn that hits `max_tokens` while a tool call is still
  being generated drops the partial call: the provider emits no
  `tool_use` event for it and ends with `stop_reason: "max_tokens"`. A
  half-generated call is not actionable, so it does not exist on the
  wire.

### Response, buffered

```json
{
  "message": {"role": "assistant", "content": [ … ]},
  "stop_reason": "end_turn" | "tool_use" | "max_tokens" | "refusal",
  "usage": {"input_tokens": 17, "output_tokens": 42}
}
```

`stop_reason: "tool_use"` means the message contains `tool_use` blocks and
the model is waiting on their results. `refusal` means the model declined
to continue; the turn ends and must not be retried or silently continued.
An assistant `content` array may be empty — a refusal, or `max_tokens`
hit immediately, can stop a turn before any block completes. `usage` is
always present — on the buffered form and on the stream's `end` event
alike — and open beyond its two required fields: a provider may add its
own counters (cache reads, say), and consumers must tolerate and may
ignore them.

### Failure, buffered — settled

Settled in milestone 4, with a real provider to inform it. The buffered
form has a third shape, `{"error": {"message", "retryable"?, "kind"?}}`
— the same three fields the stream's `error` event carries — for a
turn the vendor answered and refused: a non-2xx status, or an error
document in place of a message. The provider fills `retryable` from
what it knows of its own API (rate limits, overload and transient
server errors are worth repeating unchanged; authentication,
malformed-request and not-found answers will fail again) and `kind`
with the vendor's own class identifier. Consumers branch on
`retryable` and nothing else: `kind` and `message` are informational.

What stays a step error is exactly what the provider could not classify
without reading error text: the transport failed before any answer
arrived, the operator denied the egress, the kernel refused the step. A
step error from a provider is uniformly fatal to the turn — the loop
must not retry it, and **must not** infer meaning from its message. That
is the same line the `TOOL` contract draws below: an outcome the caller
should react to arrives as data, or it is an error nobody should parse.

### Response, streamed

With `stream: true` the result is instead `{"stream": <handle>}` — an
integer Gwead stream handle. A handle is an index into the
`StreamRegistry` the caller attached to the execution with
`.with_streams(…)`, and is meaningless against any other table: each
table numbers its handles from 1, and an execution run *without* a
caller-supplied table has its streams drained when it returns. The table
the caller supplies is granted to the executed action wholesale, so scope
one table per call. Reading the handle after the action returns is the
designed pattern — it is how the milestone-5 loop will consume a turn.

The stream yields UTF-8 newline-delimited JSON, one event per line
(`streamEventShape` in the contract):

- `{"type": "text", "text": "…"}` — an increment of assistant text;
  concatenate in arrival order.
- `{"type": "tool_use", "id": "…", "name": "…", "input": { … }}` — a
  complete tool call. Providers buffer partial tool-call deltas and emit
  the call whole: arguments are machine-consumed, so incremental display
  buys nothing and every consumer would otherwise reassemble JSON
  fragments.
- `{"type": "opaque", "provider": "…", "data": …}` — a complete
  `opaque` block, emitted whole in its position among the turn's
  blocks.
- `{"type": "end", "stop_reason": "…", "usage": { … }}` — the final event
  of every successful stream, followed by end-of-stream.
- `{"type": "error", "message": "…", "retryable": …, "kind": "…"}` — the
  turn failed and the provider can still say why. Always the last event:
  followed by end-of-stream, never by `end`. `retryable` marks failures
  worth repeating unchanged (rate limit, overload) as opposed to ones
  that will fail again (bad credentials); absent means unknown. `kind` is
  a provider-specific identifier, informational only.

The assistant message a consumer rebuilds from the stream — to replay
on the next turn — is the events in order: adjacent `text` events
coalesced into one text block, `tool_use` and `opaque` events kept
whole and in place. That is what keeps a vendor's reasoning in front of
the tool call it led to.

End-of-stream without an `end` or `error` event means the turn failed
mid-stream with the cause lost; a failure before any bytes flow is an
ordinary step error. Either way, consumers must treat a stream that did
not reach `end` as a failed turn, not a short answer. Events can be
arbitrarily long — a `tool_use` event carries its whole `input` on one
line — so consumers must not assume bounded lines.

One dispatch caveat: `Kernel::execute_by_role` cannot carry a streams
table, so a streaming caller resolves the role first
(`Kernel::role_candidates`) and executes the winner with
`.with_streams(…)` — the integration tests show the pattern.

## `TOOL`

One required action, `call`: the model's arguments in, one uniform result
out.

```json
{"content": "what the model sees, verbatim", "is_error": false}
```

`is_error: true` marks a failure the *model* should react to — file not
found, command exited nonzero. Infrastructure failures (the plugin lacked
a grant, the operator denied, the kernel refused) are step errors and
never masquerade as tool results.

That boundary constrains milestone 4 at the host-step layer, not just in
tool manifests. A declarative tool's only failure primitive is the `try`
intrinsic, and its `catch` sees the error as a *string* — so a tool that
wrapped `host_fs.read` in `try` could separate "file not found" from "the
operator said no" only by matching English error text, and a denial that
slipped the match would become `is_error: true`, exactly the masquerade
forbidden above. The rule is therefore: **an outcome the model should
react to must arrive as data, not as an error**. Settled in milestone 4
at the host-step layer: every `host_fs` step reports the answers a model
can act on — `not_found`, `is_directory`, `not_a_directory`,
`permission_denied`, and for a write `is_symlink` — as a result whose
`outcome` names them, with a one-line `message` fit to hand over
verbatim, and `host_process.run`
already returned a nonzero exit status as data. A bundled tool branches
on `outcome` and never wraps a host step in `try`; the string path is
left to the failures it is for.

### Truncation — settled

A tool that cut its output says so as data: `truncated: true` on the
result, with `content` the prefix it kept. The tool never appends a
marker itself — composing one declaratively would mean a branch per
truncatable field, and four tools would grow four spellings. The caller
renders the one shared marker, `gwennol_core::spi::tool::TRUNCATED_MARKER`,
onto the end of `content` before the model sees it —
`spi::tool::render_content` is the single implementation — so the model
learns "this is a prefix" in one fixed form whichever tool produced it.
A tool composed of several capped steps reports `truncated` when any of
them was cut.

### How a tool's schema reaches the model — settled

Derived from the manifest, not declared separately. The `tool` block on
the implementing plugin's `call` action is the **single** declaration of
the tool's name, description, and argument schema:

```json
"actions": {
  "call": {
    "tool": {
      "name": "read",
      "description": "Read a file from the workspace.",
      "parameters": {"type": "object", "required": ["path"], "…": "…"}
    },
    "steps": [ … ]
  }
}
```

The embedder harvests these with Gwead's `get_tool_descriptors()` and maps
each descriptor to a `tools` entry: `tool.name → name`, `description →
description`, `parameters → input_schema`. Declaring the schema a second
time in the role contract would only create a copy that can drift; the
manifest is already the audit surface, so it is also the source of truth.
The `TOOL` contract therefore fixes only what every tool shares — the
action name and the result shape — and deliberately leaves `call`'s input
schema open.

The harvest is not a passthrough. Gwead collects a `tool` block from
*any* action of *any* plugin, in unspecified order (its own docs say so —
the descriptor list comes off a `HashMap`), so the raw list is never what
the model gets. `gwennol_core::spi::harvest_tools` is the one
implementation of the harvest rules — call it rather than re-deriving
them:

- **only descriptors whose plugin fulfils the `TOOL` role through its
  `call` action survive** — anything else never faced the contract check
  and is not offered to the model;
- **duplicate tool names are refused, not resolved**
  (`HarvestError::DuplicateToolName`) — two plugins declaring the same
  `tool.name` make "the descriptor with that name" ambiguous, and real
  provider APIs reject duplicate tools anyway;
- **the result is sorted by name** — tool order is model-visible and
  prefix-sensitive, so an unspecified order changes the prompt every
  process start and busts provider-side prompt caching.

### Selecting a tool

Role dispatch answers "give me *a* fulfiller", which is the right question
for `LLM_CHAT` (one provider at a time; with several registered, the
first at the nearest namespace wins, silently — register one) but not for
tools — a deployment registers many `TOOL` plugins at once. Selection is
by descriptor: the model names a tool, the loop finds the descriptor
with that `name`, and its `plugin_key`/`action_name` say exactly what to
execute. The role still
earns its keep: it is the registration-time contract check on every tool
plugin, the uniform calling convention that makes descriptor-driven
dispatch possible, and the audit handle — `roles: ["TOOL"]` is one grep
away from "everything the model can call" *because* the harvest rules
above admit nothing else. The grep is a property of the embedder holding
that line, not of the engine: Gwead itself would happily harvest a
`tool` block from a plugin that never claimed the role.

## Evolving a contract

The documents carry a `version` field the kernel does not parse — though
it is not inert: re-registering a role compares the whole document, so
any change, a bare version bump included, is refused as a conflicting
redefinition on a live kernel. Contract evolution happens across process
starts, never in place.

Additive change — a new optional field in an open object, a new action
marked `"optional": true` — is a minor bump; anything a current
implementation would mis-handle is a new contract. The closed
(`additionalProperties: false`) objects in the schemas are deliberate:
they mark exactly where adding a field is a contract change rather than a
compatible extension.

Enumerations and the stream-event `oneOf` are **fail-closed**: a consumer
receiving a `stop_reason` outside the enum, or a stream event whose
`type` it does not know, treats the turn as failed. In a harness whose
turns run tools, misreading "the model refused" as "the model finished"
is worse than failing loudly — so new values and new event types are
contract changes by construction, and the sets above are meant to be
complete for what a provider can express, not a starter list.

## Known exclusions

Doors closed on purpose for the MVP, listed so they read as decisions
rather than oversights. Each would be a contract change; none blocks
milestones 3–7.

- **Thinking, as content.** A vendor's thinking travels only as an
  `opaque` block (above, settled in milestone 4): replayed, never
  shown. Surfacing reasoning to the operator — a summary the frontend
  could display — would be a block the consumer *reads*, and is not on
  this wire.
- **Prompt caching, request side.** There is no `cache_control` or
  equivalent; a provider may cache however its API allows, and the open
  `usage` object lets it report the effect, but the contract offers no
  lever to place cache breakpoints.
- **Sampling and generation knobs.** No `temperature`, `stop_sequences`,
  or per-turn model selection; the provider's `$config` carries such
  choices for the MVP. Per-turn model switching (a cheap summarizer
  beside the main model) is the first thing likely to reopen this.
- **Vendor pause states.** A provider whose API can pause a turn
  (server-side tool use, long-turn pauses) resolves the pause inside its
  own implementation — continue or fail; no pause is expressible on this
  wire.
- **Non-text tool results.** `content` is a UTF-8 string. Binary output
  is the tool's problem (lossy conversion, or refusing); image results
  are a later block-type change. Truncation is signalled as data
  (`truncated`, above), never inside `content`.
