# gwennol-core

The Gwennol host library. It owns the Gwead `KernelConfig`, registers the
native host step types (`host_fs.read`, `host_fs.write`, `host_fs.list`,
`host_process.run`, `host_http.get`, `host_http.post`), ships the bundled
SPI contracts (`LLM_CHAT`, `TOOL`) and the host plugin manifests, runs the
agent loop (`agent::Session`), and defines the [`Operator`] trait through
which a frontend supplies approvals, secrets, input, and event rendering.

It makes no policy decisions of its own: every step that reaches outside the
sandbox asks the `Operator`, shown the concrete path, argv or URL, the
plugin asking, and the model's tool call behind it.
