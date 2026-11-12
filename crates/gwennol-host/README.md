# gwennol-host

The Gwennol host. It owns the Gwead `KernelConfig`, registers the native
host step types (`host_fs.read`, `host_fs.write`, `host_fs.list`,
`host_process.run`, `host_http.get`, `host_http.post`), ships the host
plugin manifests that publish them, and defines the `Operator` trait through
which a frontend supplies approvals, secrets, input, and event rendering.
The agent loop and the SPI contracts are in `gwennol-core`.

It makes no policy decisions of its own: every step that reaches outside the
sandbox asks the `Operator`, shown the concrete path, argv or URL, the
plugin asking, and the model's tool call behind it. `OperatorEvents` sends a
session's events to an operator, and `run` drives a session from the
operator's input.
