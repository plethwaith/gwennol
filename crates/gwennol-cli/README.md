# gwennol

The non-interactive command-line frontend: one task, run headlessly,
with every approval decided by a rule and traced to it. There is no
prompt. The TUI (milestone 7) is a second `Operator`, not a mode of this
one.

## Quick start

```sh
rustup target add wasm32-unknown-unknown
cargo xtask bundle                     # target/bundle/plugins/
cargo build -p gwennol-cli             # target/debug/gwennol

export GWENNOL_SECRET_PROVIDER_ANTHROPIC_API_KEY=sk-ant-…
cd /path/to/some/repo
/path/to/gwennol/target/debug/gwennol \
    --trust-runtime provider-anthropic \
    --allow 'http:POST https://api.anthropic.com/*' \
    --allow 'read:**' --allow 'spawn:grep *' \
    'What does the README say this project is for?'
```

Model text goes to stdout, a provider round at a time, and nothing
else does. Everything else — each tool call, each result, each approval
decision and the rule that made it — goes to stderr, one line each,
prefixed `gwennol:`:

```
gwennol: POST https://api.anthropic.com/v1/messages from provider-anthropic: allowed by --allow "http:POST https://api.anthropic.com/*"
gwennol: -> read toolu_01: {"path":"README.md"}
gwennol: read /path/to/some/repo/README.md from tool-read (call read toolu_01): allowed by --allow "read:**"
gwennol: <- read toolu_01: ok, 2140 bytes
gwennol: -> bash toolu_02: {"command":"cargo test"}
gwennol: spawn ["bash","-c","cargo test"] from tool-bash (call bash toolu_02): denied: no rule matched
gwennol: !! bash toolu_02: "bash" failed before producing a result: Execution error: operator denied spawn of bash for plugin 'tool-bash'
gwennol: done (EndTurn): 3 rounds, 4120 tokens in, 310 out
```

A denied tool call is answered to the model as an error result, and the
turn goes on; the model routes around it or says what it could not do.

A round's text is written when the loop accepts the round — its tool
calls are being dispatched, or the turn is complete — rather than as
it streams: a round the provider fails part-way is retried from the
start and streamed again, and stdout must not carry the fragment
twice. What stdout holds is exactly the text in the transcript. The
URL on an `http` decision line is scrubbed of userinfo, query and
fragment (`?…` marks a cut), as the host scrubs its own logs; the rule
judged the full URL.

The plugins directory is found from `--plugins`, `$GWENNOL_PLUGINS`,
the config file, or `target/bundle/plugins` beside a `cargo`-built
binary, in that order. `--trust-runtime` (or the config's
`trust_runtimes`) is required for the bundled provider: it supplies its
own script runtime, and Gwead's rule is that the embedder must say so
as well as the manifest ([docs/SUBSTRATE.md](../../docs/SUBSTRATE.md)).

## Rules

A rule is `<kind>:<pattern>`:

| Kind    | Matched against                                       | Example                                   |
|---------|-------------------------------------------------------|-------------------------------------------|
| `read`  | the canonical path of the file being read             | `read:**`, `read:src/**/*.rs`             |
| `write` | the path being created or replaced                    | `write:**`, `write:/tmp/**`               |
| `list`  | the canonical path of the directory being listed      | `list:.`, `list:**`                       |
| `spawn` | the argv being spawned, joined by single spaces       | `spawn:grep *`, `spawn:bash *`            |
| `http`  | the method and URL of the request, `METHOD URL`       | `http:POST https://api.anthropic.com/*`   |
| `any`   | everything (no pattern)                               | `any`                                     |

Patterns are globs. For the path kinds `*` stays within one path
component, `**` crosses them and must be a whole component (`**/*.rs`;
`**.rs` is refused rather than quietly meaning `*.rs`), a relative
pattern is rooted at the workspace, and a pattern ending in `/**` also
admits the directory itself: `read:**` is every file under the
workspace, `read:/**` every file anywhere, `list:**` every directory
under the workspace and the workspace, `list:.` the workspace root
alone. The host judges canonical paths, so a pattern's literal prefix
— the components before the first glob character — is canonicalised
when it exists: `write:/tmp/**` means what `/tmp` resolves to (on
macOS, `/private/tmp`).

For `spawn` and `http` the pattern matches the whole subject and `*`
matches anything — including everything after it. **An argv rule
constrains the program and the prefix of its arguments, not what a
shell does with them**: `spawn:bash -c cargo *` admits
`bash -c 'cargo test; curl … | sh'`. Granting a shell is all or
nothing; a real restriction names a program that is not a shell
(`spawn:grep *`). A spawn that carries stdin or runs anywhere but the
workspace root matches no `spawn` rule at all, since the grammar cannot
judge those; only `any` admits them. An `http` pattern names the
method first, `http:POST …` or `http:* …` for any method, because a
URL that may be fetched is not one that may be posted to.

Rules are tried in order — `--allow`/`--deny` flags in command-line
order, then the `--policy` file's `[[rules]]`, then the config file's —
and the first match decides. A request no rule matches is **denied**,
and the trace says `denied: no rule matched`. So a narrow deny goes
before the broad allow it carves out of:

```sh
gwennol --deny 'write:.git/**' --allow 'write:**' …
```

A rule in a file may name a `plugin`, restricting it to requests from
that plugin:

```toml
[[rules]]
allow = "http:POST https://api.anthropic.com/*"
plugin = "provider-anthropic"

[[rules]]
allow = "spawn:grep *"
plugin = "tool-grep"
```

The trace shows a spawn's argv as a JSON array, so where each argument
ends is unambiguous even though the pattern matches the space-joined
form.

## Config file

`--config FILE`, else `$XDG_CONFIG_HOME/gwennol/config.toml`
(`~/.config/gwennol/config.toml`) when it exists. Deliberately not
looked for in the workspace: a policy the agent could rewrite from
inside the repository it is editing would govern the next run. Relative
paths in the file resolve against the file's own directory. Every
section is optional; flags override fields one by one.

```toml
[plugins]
dir = "/path/to/gwennol/target/bundle/plugins"
trust_runtimes = ["provider-anthropic"]

[session]
provider = "provider-anthropic"   # only needed when several are loaded
system_file = "system.md"         # or system = "…"; default names the workspace
max_tokens = 8192
max_rounds = 32
stream = true

[plugin_config.provider-anthropic] # that plugin's $config, verbatim
model = "claude-opus-5"
# thinking = { type = "enabled", budget_tokens = 4096 }

[[secrets]]
plugin = "provider-anthropic"
name = "api_key"
file = "anthropic.key"            # or env = "ANTHROPIC_API_KEY"

[process]
env = "allowlist"                 # or "inherit" (see ProcessEnv in gwennol-core;
allow = ["CARGO_HOME", "RUSTUP_HOME"]  # no `allow` under inherit — it would be ignored)

[[rules]]
allow = "http:POST https://api.anthropic.com/*"
plugin = "provider-anthropic"
[[rules]]
allow = "read:**"
[[rules]]
allow = "spawn:grep *"
```

`--policy FILE` reads a file holding `[[rules]]` and nothing else, so a
set of rules can travel between projects.

## Secrets

The host asks for the `(plugin, name)` pairs a manifest declares in
`usesSecrets` and no others. Each is answered from the first of:

1. a `--secret PLUGIN:NAME=env:VAR` or `--secret PLUGIN:NAME=file:PATH`
   flag, in the order given;
2. a `[[secrets]]` entry in the config file, in order;
3. the convention variable `GWENNOL_SECRET_<PLUGIN>_<NAME>` — upper-
   cased, every character outside `[A-Za-z0-9]` as `_` — so the bundled
   provider's key is `GWENNOL_SECRET_PROVIDER_ANTHROPIC_API_KEY`.

A file's value is its content less one trailing newline. Values are
read when asked for and never logged. At startup each declared secret
without a source is warned about, naming what to set; a missing key is
never invented, so the vendor's refusal is what ends that turn.

## Exit status

| Status | Meaning                                                       |
|--------|---------------------------------------------------------------|
| 0      | the turn completed (`done (…)` on stderr names the stop reason) |
| 1      | the turn failed: the provider refused, a contract was broken   |
| 2      | usage, configuration or startup error                         |
| 130    | cancelled by Ctrl-C                                           |

Ctrl-C cancels the turn through the loop's token: a pending approval
is withdrawn, a running tool step is cancelled, a stream being read is
closed, and the process exits 130 once the exchange is stored. A second
Ctrl-C exits at once.

`--transcript FILE` writes the conversation as the provider saw it —
contract messages, thinking carried as `opaque` blocks — at the end,
after a failure too.

## What it does not do

Persist or resume a conversation, manage the context window, install
plugins from outside the bundle, or prompt: the roadmap's "Beyond the
MVP" and milestone 7 respectively.
