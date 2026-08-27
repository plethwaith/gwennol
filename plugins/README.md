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
- `providers/` — plugins implementing `LLM_CHAT`: `anthropic.json`, built
  on `host_http.post`.
- `tools/` — plugins implementing `TOOL`: `read.json`, `write.json`,
  `grep.json` and `bash.json`, each composed from `host_fs.*` and
  `host_process.run`.

The contracts come first (milestone 2), then the substrate
non-declarative plugins are written in (milestone 3), then the plugins
themselves (milestone 4). See [../docs/ROADMAP.md](../docs/ROADMAP.md).
If milestone 3 chooses wasm guests, their source lives under `crates/`
and the build injects the compiled module into the manifest; the file
here remains the plugin.

The host step types these use are *not* here: they are native code in
`gwennol-core`, published by the `host_fs`, `host_process` and
`host_http` manifests it ships.

A plugin never touches the filesystem or the network itself. It declares
what it needs (a tool: `step_type:host_fs.read`; a provider:
`network:egress:api.anthropic.com`) and its manifest is therefore an
accurate statement of what it can reach.
