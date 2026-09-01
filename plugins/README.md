# Plugins

The Gwead documents bundled with Gwennol, registered by `gwennol-core` at
boot. Each is a single JSON file: Gwead resolves nothing from disk, so a
manifest carries everything it needs inline, and a plugin never spans
files. The folders group documents by kind, in registration order — role
contracts must be registered before the plugins that claim them.

- `spi/` — role contracts, one document per role: `llm_chat.json`
  (`LLM_CHAT`, streaming) and `tool.json` (`TOOL`). SPI definitions are
  not plugins; they are the contracts plugins are checked against.
  These files are canonical; `gwennol-core` ships byte-identical copies
  under `crates/gwennol-core/resources/spi/` (a published crate cannot
  embed files outside its own directory), pinned equal by a test.
- `providers/` — plugins implementing `LLM_CHAT`: `anthropic.json`
  (`provider-anthropic`), the Messages API over `host_http.post`,
  streamed and buffered. Guest-backed: the request shaping and the
  stream translation are Rust in `crates/provider-anthropic`, compiled
  to wasm32 and occupying the plugin's own script-runtime slot
  ([../docs/SUBSTRATE.md](../docs/SUBSTRATE.md)). The manifest owns
  everything that reaches outside the sandbox — one `host_http.post`
  per turn shape, the egress grant, the `api_key` secret on the
  `x-api-key` header — and the guest never sees the key.
- `tools/` — plugins implementing `TOOL`: `read.json`, `write.json`,
  `grep.json` and `bash.json` (`tool-read`, `tool-write`, `tool-grep`,
  `tool-bash`). Declarative: each is one host step — `host_fs.read`,
  `host_fs.write`, `host_process.run` — and a branch on its outcome,
  which the host steps report as data (`docs/SPI.md`, "Outcomes are
  data"), so no tool ever wraps a step in `try`. Each manifest's
  `permissions` names exactly the host step it uses, pinned by a test.

## Guest modules are bundled, not committed

A guest-backed manifest names its module by the crate that builds it:

```json
"wasmModules": { "guest": { "path": "crates/provider-anthropic" } }
```

The kernel refuses that form, so the committed file is honestly not
registrable, and no compiled blob is ever committed. `cargo xtask bundle`
(`crates/xtask`) compiles each named crate to `wasm32-unknown-unknown`
and writes the manifests with the `path` form replaced by the inline
`base64` form under `target/bundle/plugins/`, mirroring this layout. The
integration suite (`crates/gwennol-core/tests/bundled_plugins.rs`)
bundles through the same library function, so what is tested is the
committed file plus the compiled crate — never a re-typed copy.

Conventions the bundler relies on: a guest crate's directory name is its
package name, and a manifest's `language` selector is its own plugin
name.

The host step types these use are *not* here: they are native code in
`gwennol-core`, published by the `host_fs`, `host_process` and
`host_http` manifests it ships.

A plugin never touches the filesystem or the network itself. It declares
what it needs (a tool: `step_type:host_fs.read`; the provider:
`network:egress:api.anthropic.com`) and its manifest is therefore an
accurate statement of what it can reach.
