# gwennol-core

The Gwennol agent loop and the contracts it consumes. A `Session` runs a
conversation against one `LLM_CHAT` provider and the `TOOL` plugins it
offers, on any Gwead kernel that has the bundled SPI contracts registered
(`spi::register`). It sends each `Event` to the `EventSink` the session was
started with, and reads no process-global state.

The crate links no native step bodies of its own and makes no policy
decisions: what a plugin's steps do, and who is asked before they do it,
belong to whoever builds the kernel. `gwennol-host` is one such host.
