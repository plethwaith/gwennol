# Plugins

Gwead manifests (and the wasm or script sources behind them) bundled with
Gwennol. Each subdirectory is one plugin.

Planned for the first milestone:

- `spi/` — role contracts: `llm.chat` (streaming) and `tool`.
- `provider-anthropic/` — implements `llm.chat` over the host's `http.*` steps.
- `tool-read/`, `tool-write/`, `tool-grep/`, `tool-bash/` — tools composed
  from host `fs.*` and `process.*` steps.

A tool plugin never touches the filesystem or network itself. It declares the
host step types it needs (`step_type:fs.read`, `network:egress:…`) and the
manifest is therefore an accurate statement of what it can reach.
