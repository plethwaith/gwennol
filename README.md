# Gwennol

**Gwennol** (Welsh: *shuttle*; also *swallow*) is an agentic coding harness
from [Plethwaith Labs](https://plethwaith.com), built on the
[Gwead](https://github.com/plethwaith/gwead) WebAssembly plugin microkernel.

The idea: in every coding agent today, tools run as host-native code with
permission prompts bolted on afterwards. In Gwennol, tools and model providers
are sandboxed Gwead plugins. A plugin's manifest declares the host step types
it may use, and the kernel refuses anything else at dispatch time — so the
manifest is an accurate statement of what a tool can reach, and "where can my
API key go?" is answered by reading ten lines of JSON.

## Status

**Pre-alpha.** The workspace scaffold exists; nothing runs yet. The 0.0.0
release on crates.io is a name reservation.

## Layout

```
crates/gwennol-core/   host library: kernel config, native steps, loop, Operator trait
crates/gwennol-cli/    first frontend (non-interactive CLI)
plugins/               bundled SPI + plugin manifests
```

## First milestone

1. Native host steps: `fs.read`, `fs.write`, `fs.list`, `process.run`,
   `http.post` (streaming), each routed through `Operator::approve`.
2. SPIs: `llm.chat` (streaming) and `tool`.
3. Plugins: one provider (Anthropic) and four tools (read, write, grep, bash).
4. The agent loop in core, the CLI frontend, and one end-to-end test that runs
   a tool call through a real sandboxed plugin against a stubbed provider.

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
