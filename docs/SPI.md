# The SPI contracts

Gwennol defines two Gwead roles: `LLM_CHAT`, implemented by model provider
plugins, and `TOOL`, implemented by tool plugins. The contract documents
live in [`plugins/spi/`](../plugins/spi/) and are registered by
`gwennol_host::boot` before any plugin can claim a role. This page is the
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
  agent loop (`gwennol_core::agent`) validates model-emitted tool
  arguments against the tool's declared schema before dispatching the
  tool's action, and reads everything a provider returns fail-closed.
  `gwennol_core::provider_accepts`, which the loop never calls, reads the
  `accepts` answer: a key this version knows fail-closed, any other key
  ignored.

## `LLM_CHAT`

One required action, `chat`: one model turn. One optional action, `accepts`:
what the provider takes (below).

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
| `image` | `source` | `user`, or inside a `tool_result`'s `content` |

An `opaque` block is provider state the consumer carries but never
reads — a vendor's thinking block, which the vendor requires replayed
verbatim in a later tool-use turn, is the case it exists for. Produced
only by a provider, consumed only by a provider: a consumer keeps it in
its position in the assistant message and replays it unchanged — never
rendered, altered, reordered, or dropped — and assumes nothing about the
shape of `data`. `provider` names the producing plugin, so a provider
ignores blocks another produced (a conversation that changes provider
mid-way loses only what the new provider could not have used).

An `image` block is an image shown to the model, in its position among the
blocks around it: a user message's, or a `tool_result`'s content. Its
`source` carries the image one of two ways: the file's bytes inline, as
standard base64, or a `reference` (a path, a storage
key); both name a media type, and the schema lists the fields and the media
types. The consumer never
interprets a reference: it stores and replays it, and resolving it is the
provider's. A provider that cannot resolve a reference, or cannot take an
image, answers with a `Failure`. A provider can also say beforehand, through
`accepts`, which image formats and source types it takes, in either place,
or that it takes none. The loop builds the inline block from an `Image`, whose format it
reads from the bytes, and the reference block from an `ImageReference`,
whose format the caller declares;
`SessionConfig::max_image_bytes` measures images, not references, and
`Session::turn_with` refuses an image over it before storing anything; a
tool's image over it answers the call as a failure (see `TOOL`).
Limits on dimensions and count are the vendor's.

A `tool_use` block is the model asking for a tool: `id` names this call,
`name` names the tool, `input` is arguments per the tool's declared
schema. The caller runs the tool and carries the answer back as a
`tool_result` block — `tool_use_id` echoing the `id` — in the next `user`
message. A `tool_result`'s `content` is a string, or a non-empty list of
`text` and `image` blocks in order.

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
- In that user message, `tool_result` blocks come before any `text` or
  `image` blocks. An image inside a `tool_result`'s content is part of
  that result.
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

### Failure — settled

Settled with the first real provider to inform it ("4. Provider and
tools" in the roadmap). The buffered form has a third shape,
`{"error": {"message", "retryable"?, "kind"?}}` — the same three
fields the stream's `error` event carries — for a turn the vendor
answered and refused (a non-2xx status, or an error
document in place of a message), or one the provider declined (below),
which answers a streamed request, too. The provider fills `retryable` from
what it knows of its own API (rate limits, overload and transient
server errors are worth repeating unchanged; authentication,
malformed-request and not-found answers will fail again) and `kind`
with its own class identifier. Consumers branch on
`retryable` and nothing else: `kind` and `message` are informational.

What stays a step error is exactly what the provider could not classify
without reading error text: the transport failed before any answer
arrived, the operator denied the egress, the kernel refused the step. A
step error from a provider is uniformly fatal to the turn — the loop
must not retry it, and **must not** infer meaning from its message. That
is the same line the `TOOL` contract draws below: an outcome the caller
should react to arrives as data, or it is an error nobody should parse.

A provider that declines the request before calling a vendor (an image
source it cannot resolve) answers the same failed form, on either path;
on a streamed request it comes back in place of `{"stream": …}`, since no
stream was started. It is not retryable.

### Response, streamed

With `stream: true` the result is instead `{"stream": <handle>}` — an integer
Gwead stream handle — unless the provider declined the request before
starting a stream (the failed form, above). A handle is an index into the
`StreamRegistry` the caller attached to the execution with
`.with_streams(…)`, and is meaningless against any other table: each
table numbers its handles from 1, and an execution run *without* a
caller-supplied table has its streams drained when it returns. The table
the caller supplies is granted to the executed action wholesale, so scope
one table per call. Reading the handle after the action returns is the
designed pattern — it is how the agent loop consumes a turn.

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

A turn can fail mid-stream three ways: a provider-authored `error`
event (above), always followed by end-of-stream and never by `end`;
end-of-stream with no `end` or `error` event at all, the cause lost; or
the read itself failing, reporting the text the kernel recorded for
the failure rather than a misleadingly clean end. A failure before any
bytes flow is an ordinary step error, unless the provider declined the
request before starting the stream, which is the failed form (above).
Whichever shape it takes,
consumers must treat a stream that did not reach `end` as a failed
turn, not a short answer.
Events can be arbitrarily long — a `tool_use` event carries its whole
`input` on one line — so consumers must not assume bounded lines.

One dispatch caveat: `Kernel::execute_by_role` cannot carry a streams
table, so a streaming caller resolves the role first
(`Kernel::role_candidates`) and executes the winner with
`.with_streams(…)` — the loop and the integration tests show the
pattern.

### What a provider accepts

`accepts` takes nothing but `$config` and answers per kind of input, so the
answer can follow the model a config names. A key left out means the
provider does not know, an answer distinct from `false` (a provider that
can only ask its server, and cannot, leaves it out). This version defines
one key, `images`: `false`, or the media types and source types it takes,
for an image block in a user message or in a tool result.
The answer is open: a later version adds a key, a caller ignores one it
does not know, and a known key with a value the schema does not admit is a
contract violation.

The action is optional, and Gwead's registration check does not look for
it: a provider without it reads as one that does not say. The loop never
asks; `gwennol_core::provider_accepts` asks the provider a session config
resolves to, under the `$config` that config gives the provider, and its
context, for an embedder to call when the config changes. The answer binds
nothing: a turn the provider cannot serve still answers with the failed
form. The bundled provider declares inline images in every format the
contract names, from its manifest, without reading the configured model.

## `TOOL`

A `TOOL` plugin offers one tool for each of its actions that carries a
`tool` block. Every such action takes the model's arguments and returns one
uniform result. The contract requires no action, so Gwead's registration
check passes any `TOOL` claim; the loop checks the result shape on every
call.

```json
{"content": "what the model sees, verbatim", "is_error": false}
```

A result's `content` is a string or a non-empty list of `text` and `image`
blocks (the same blocks as `LLM_CHAT`'s, either source), in the order the
model should see them. The loop checks each inline image before it stores
the result: its data is standard padded base64, its bytes are a format the
contract names and the one its `media_type` declares, and it is within
`SessionConfig::max_image_bytes`. One that fails answers the call as a
failure the tool could not answer (`Event::ToolFailed`), like a malformed
result. A reference is passed on and never interpreted; a provider that
cannot resolve it answers the next round with a `Failure`. A result stays
in the transcript: an image the provider refuses is sent again every round
until `Session::restore` replaces the transcript.

`is_error: true` marks a failure the *model* should react to — file not
found, command exited nonzero. Infrastructure failures (the plugin lacked
a grant, the operator denied, the kernel refused) are step errors and
never masquerade as tool results. The loop still answers such a call —
the protocol requires every call answered — with an `is_error` result
carrying the step's message verbatim, and tells the frontend as data
that the tool did not answer (`Event::ToolFailed`), so the distinction
this boundary exists for is kept at every layer; see "5. Agent loop"
in the roadmap.

That boundary constrains the host-step layer, not just in
tool manifests. A declarative tool's only failure primitive is the `try`
intrinsic, and its `catch` sees the error as a *string* — so a tool that
wrapped `host_fs.read` in `try` could separate "file not found" from "the
operator said no" only by matching English error text, and a denial that
slipped the match would become `is_error: true`, exactly the masquerade
forbidden above. The rule is therefore: **an outcome the model should
react to must arrive as data, not as an error**. Settled at the
host-step layer: every `host_fs` step reports the answers a model
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
before the model sees it: on its own final line of a string, or as a text
block of its own after the last block of a list, so the model
learns "this is a prefix" in one fixed form whichever tool produced it.
A tool composed of several capped steps reports `truncated` when any of
them was cut.

### How a tool's schema reaches the model — settled

Derived from the manifest, not declared separately. The `tool` block on
the implementing plugin's action, whichever action carries it, is the
**single** declaration of the tool's name, description, and argument
schema:

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
The `TOOL` contract therefore fixes only the result shape and
deliberately leaves every tool's input schema open.

The harvest is not a passthrough. Gwead collects a `tool` block from
*any* action of *any* plugin, in unspecified order (its own docs say so —
the descriptor list comes off a `HashMap`), so the raw list is never what
the model gets. `gwennol_core::spi::harvest_tools` is the one
implementation of the harvest rules — call it rather than re-deriving
them:

- **only descriptors of a plugin that fulfils the `TOOL` role survive**,
  one for each of its actions that carries a `tool` block; a `tool` block
  on a plugin outside the role is not offered to the model;
- **duplicate tool names are refused, not resolved**
  (`HarvestError::DuplicateToolName`) — two plugins, or two actions of one,
  declaring the same `tool.name` make "the descriptor with that name"
  ambiguous, and real provider APIs reject duplicate tools anyway;
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
execute. The role is what the harvest keys on, and the audit handle:
`roles: ["TOOL"]` finds the plugins, and their `tool` blocks are everything
the model can call, *because* the harvest rules above admit nothing else.
That is a property of the embedder holding the line, not of the engine:
Gwead itself would happily harvest a `tool` block from a plugin that never
claimed the role.

A session can also offer a chosen subset: `SessionConfig::tool_plugins`
names `TOOL` plugins (by plugin name, the unit a manifest grants authority
to), and `spi::harvest_selected` harvests only those; a selected plugin
brings all its tools. The selection applies before the duplicate refusal
above, so two plugins that advertise one tool name can share a kernel as
long as every session on it selects at most one of them (a session whose
`tool_plugins` is `None` offers both, and is refused). A name that offers
no tool is refused (`HarvestError::NoSuchToolPlugin`).

### A plugin with several tools

- An action without a `tool` block is a helper and is never offered.
- Tool names are unique across everything a session offers, within one
  plugin included.
- Authority is the plugin's. A manifest's `permissions` cover every action
  of the plugin, and an approval request names the plugin and the model's
  tool call (`ApprovalRequest`). A frontend that remembers an answer per
  plugin applies it to every tool that plugin offers; the bundled CLI's
  session rules are keyed that way (`SessionRule::matches`). A plugin that
  offers one tool keeps the property that its manifest states what that
  tool can reach.
- A session selects plugins, not tools.

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
"3. Plugin substrate" through "7. TUI".

- **Thinking, as content.** A vendor's thinking travels only as an
  `opaque` block (above): replayed, never
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
- **Tool results beyond text and images.** `content` is a string or a
  list of text and image blocks. Other binary output (audio, a PDF) is
  the tool's problem (lossy conversion, or refusing). Truncation is
  signalled as data (`truncated`, above), never inside `content`.
- **How a reference is resolved.** The contract says the provider
  resolves an image reference, not how; no host step exists for it, and
  the bundled provider resolves none.
- **Limits in the `accepts` answer.** It says which formats and sources a
  provider takes, not how large, how many, or of what dimensions; the open
  `images` object leaves room for them.
