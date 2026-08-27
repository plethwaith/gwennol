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

A `tool_use` block is the model asking for a tool: `id` names this call,
`name` names the tool, `input` is arguments per the tool's declared
schema. The caller runs the tool and carries the answer back as a
`tool_result` block — `tool_use_id` echoing the `id` — in the next `user`
message.

`tools` is what the model may call this turn, and each entry is a Gwead
tool descriptor on the wire: `input_schema` is the implementing action's
`tool.parameters`, verbatim (see `TOOL` below).

### Response, buffered

```json
{
  "message": {"role": "assistant", "content": [ … ]},
  "stop_reason": "end_turn" | "tool_use" | "max_tokens",
  "usage": {"input_tokens": 17, "output_tokens": 42}
}
```

`stop_reason: "tool_use"` means the message contains `tool_use` blocks and
the model is waiting on their results.

### Response, streamed

With `stream: true` the result is instead `{"stream": <handle>}` — an
integer Gwead stream handle, valid only inside the execution that returned
it. The stream yields UTF-8 newline-delimited JSON, one event per line
(`streamEventShape` in the contract):

- `{"type": "text", "text": "…"}` — an increment of assistant text;
  concatenate in arrival order.
- `{"type": "tool_use", "id": "…", "name": "…", "input": { … }}` — a
  complete tool call. Providers buffer partial tool-call deltas and emit
  the call whole: arguments are machine-consumed, so incremental display
  buys nothing and every consumer would otherwise reassemble JSON
  fragments.
- `{"type": "end", "stop_reason": "…", "usage": { … }}` — the final event
  of every successful stream, followed by end-of-stream.

End-of-stream **without** an `end` event means the turn failed mid-stream;
a failure before any bytes flow is an ordinary step error. Consumers must
treat a truncated stream as a failed turn, not a short answer.

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

### Selecting a tool

Role dispatch answers "give me *a* fulfiller", which is the right question
for `LLM_CHAT` (one provider at a time) but not for tools — a deployment
registers many `TOOL` plugins at once. Selection is by descriptor: the
model names a tool, the loop finds the descriptor with that `name`, and
its `plugin_key`/`action_name` say exactly what to execute. The role still
earns its keep: it is the registration-time contract check on every tool
plugin, the uniform calling convention that makes descriptor-driven
dispatch possible, and the audit handle (`roles: ["TOOL"]` is one grep
away from "everything the model can call").

## Evolving a contract

The documents carry a `version` field the kernel does not interpret.
Additive change — a new optional field in an open object, a new action
marked `"optional": true` — is a minor bump; anything a current
implementation would mis-handle is a new contract. The closed
(`additionalProperties: false`) objects in the schemas are deliberate:
they mark exactly where adding a field is a contract change rather than a
compatible extension.
