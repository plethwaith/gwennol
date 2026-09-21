# gwennol

`gwennol [options] [prompt]` opens an interactive session by default:
the model's text streams into a transcript pane, a line editor takes
the next turn, and an approval no rule decides is asked at a prompt,
`y`/`n` once, `a`/`d` for the rest of the session when the request can
be remembered (a spawn carrying stdin, an `http` URL that fails to
parse, or a request of a kind this frontend does not know, cannot
be); a rule
always decides first, and every
decision traces into the pane, in the same
words this file's examples show on stderr. `-p`/`--print` — what the
rest of this file walks through — is one task run headlessly instead,
with every approval decided by a rule and traced to it, and a request
no rule matches is denied; a run with no terminal on stdin or stdout
is print mode too, with one stderr line saying so.

## Quick start

```sh
rustup target add wasm32-unknown-unknown
cargo xtask bundle                     # target/bundle/plugins/
cargo build -p gwennol-cli             # target/debug/gwennol

export GWENNOL_SECRET_PROVIDER_ANTHROPIC_API_KEY=sk-ant-…
cd /path/to/some/repo
/path/to/gwennol/target/debug/gwennol -p \
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

## Session

Without `-p` the screen is four parts: a transcript pane (the model's
text, the user's own submitted lines, trace lines — in the words
this file's stderr examples use when collapsed; expanded, a result
carries the whole content a print run's `-v` writes, and a call the
whole arguments no print run writes, both rewrapped to the pane's
width, which drops the four-space indent both forms carry — and
an outcome line per turn), the approval box (open only
while a request awaits an answer; see below), a status line (the
turn's state and elapsed seconds while one runs; a notice such as a
Ctrl-C hint or an unknown command; the `scrolled up · End follows the
tail` marker when the pane is not following the tail), and a line
editor.

| Keys | Do |
|---|---|
| `Enter` | Send the line as the next turn (while one is already running, it stays in the editor and the status line says so) |
| `Esc` | Cancel the running turn |
| `Up` / `Down` | Walk the input history |
| `Alt+Left`, `Ctrl+Left`, `Alt+b` | Word left |
| `Alt+Right`, `Ctrl+Right`, `Alt+f` | Word right |
| `Alt+Backspace`, `Ctrl+Backspace`, `Ctrl+w` | Delete the word behind the cursor |
| `Alt+d`, `Alt+Delete` | Delete the word ahead of the cursor |
| `Home`/`End`, `Ctrl+a`/`Ctrl+e` | Start/end of the line |
| `PageUp` / `PageDown` | Page the pane |
| `Home` / `End` (editor empty) | Reach the pane's top / its tail |
| `Tab` / `Shift+Tab` | Focus a tool call or result, newest first |
| `Enter` (editor empty, an entry focused) | Expand or collapse it |

An open approval prompt takes every key itself instead: `y`/`n` allow
or deny once, `a`/`d` for the rest of the session (see "Rules" for
what that can and cannot cover), `Esc` denies once, and `Up`/`Down`/
`PageUp`/`PageDown` scroll its own box. Ctrl-C is bound to nothing but
a hint (`Esc` cancels the turn, `/exit` ends the session): raw mode
makes it an ordinary key whose meaning differs by platform, so it is
never wired to cancel.

Slash commands: `/exit` cancels a running turn first and ends the
session once it has unwound; a second `/exit` sent while the first is
still unwinding leaves at once, status 130. `/help` lists the commands,
`Esc`, the approval prompt's `y`/`n`/`a`/`d`, and the pane's own keys
shown above — not the editor's own line-editing bindings.
Anything else starting with `/` is an unknown command,
reported on the status line rather than sent to the model. A pasted
line never submits by itself: its newlines become spaces, so a
multi-line paste lands as one line the editor still waits on `Enter`
for.

`--log FILE` collects the host's log there instead of discarding it (a
session without `--log` collects none, so nothing competes with the
pane); `-v`/`-vv` raise its level the same way they do in a print run,
and `RUST_LOG` overrides the level either sets. `-v` also starts every
tool result's pane entry expanded, matching a print run's `-v`.
`--transcript` is print mode only; a session refuses it at startup
rather than silently writing nothing.

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
— the components before the first glob character — is spelled the way
the host spells a path: its deepest canonicalisable ancestor canonical, the
rest as written. `write:/tmp/**` means what `/tmp` resolves to (on
macOS, `/private/tmp`), and `write:link/new/**` the link's target
plus `new`, whether or not `new` exists yet. One exception mirrors the
host's: a fully literal `write:` rule naming a symlink means the link
itself, because the host approves a write to a symlink under the
link's own name and then refuses to write through it. `..` folds
before any of this, as the host folds it; a `..` after a glob
component, like a space inside an `http` URL, could never match and
is refused rather than left dead.

For `spawn` and `http` the pattern matches the whole subject and `*`
matches anything — including everything after it. **An argv rule
constrains the program and the prefix of its arguments, not what a
shell does with them**: `spawn:bash -c cargo *` admits
`bash -c 'cargo test; curl … | sh'`. Granting a shell is all or
nothing; a real restriction names a program that is not a shell
(`spawn:grep *`). A spawn that carries stdin or runs anywhere but the
workspace root matches no `spawn` rule at all, since the grammar cannot
judge those; only `any` reaches them, in either direction — a
`deny spawn:*` does not stop such a spawn that a later `allow any`
admits; `deny any` does — and the trace says so. When no rule, not
even `any`, decides them, a session asks at a prompt instead of
denying — a spawn outside the workspace root can then be remembered
for the session, one carrying stdin cannot. An `http`
pattern names the method first — `http:POST …`, upper-case, one space,
or `http:* …` for any method — because a URL that may be fetched is
not one that may be posted to; a method that could never match is
refused rather than admitting nothing.

Rules are tried in order — `--allow`/`--deny` flags in command-line
order, then the `--policy` file's `[[rules]]`, then the config file's —
and the first match decides. A request no rule matches is **denied**
in a print run, and the trace says `denied: no rule matched`; a
session asks at a prompt instead. In a session an `a`/`d` answer adds
an exact-text rule tried after all of these, so it can never pre-empt
a flag's or a file's rule. So a narrow deny goes
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
allow = ["CARGO_HOME", "RUSTUP_HOME"]  # `allow` beside inherit is a startup error)

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
| 0      | the turn completed (`done (…)` names the stop reason), or the user ended the session |
| 1      | the turn failed: the provider refused, a contract was broken; or the session ended right after a failed turn |
| 2      | usage, configuration or startup error                         |
| 130    | print run: cancelled by Ctrl-C; session: a second `/exit` while the first is still unwinding |

In a print run, Ctrl-C cancels the turn through the loop's token: a
pending approval is withdrawn, a running tool step is cancelled, a
stream being read is closed, and the process exits 130 once the
exchange is stored. When the cut hid a failure of the stream itself —
the vendor's body went quiet past its idle timeout in the same
instant — the line says so:
`gwennol: cancelled; the stream's source failed with:
provider-anthropic.stream_turn failed: …`. A second Ctrl-C exits at
once. In a session Ctrl-C only shows a hint: `Esc` cancels the turn,
and `/exit` ends the session.

Print mode only: `--transcript FILE` writes the conversation as the
provider saw it — the whole chat input: the system prompt, the tools
as harvested from the manifests, every message with thinking carried
as `opaque` blocks, and the generation settings — at the end, after a
failure too. It is what the provider was handed on the last round plus
that round's answer, so the file can be read or replayed as a request.
The outcome line comes first; a transcript that cannot be written is
reported after it and makes a completed turn exit 2, while a failed or
cancelled turn keeps its own status. A session refuses `--transcript`
outright at startup rather than silently writing nothing.

## What it does not do

Take more than one line of input, use the mouse, restyle the pane, or
take a secret at the keyboard: out of scope for the MVP.
Persist or resume a conversation, manage the context window, or
install plugins from outside the bundle: the roadmap's "Beyond the
MVP". Author a rule at the prompt or keep a session's answers past it:
`a`/`d` are in memory and match the request's text as the prompt
showed it — for an `http` URL that text is the scrubbed one with a
marker for what was cut, so an answer given for a URL that carried a
query or fragment covers any query string at that path, one given for
a URL that carried credentials covers any credentials at it, and one
given for a clean URL covers only itself; a pattern is written in the
config file between sessions.
