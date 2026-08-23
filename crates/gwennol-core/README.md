# gwennol-core

The Gwennol host library. It owns the Gwead `KernelConfig`, registers the
native host step types (`fs.*`, `process.*`, `http.*`, `approval`), ships the
bundled SPI and plugin manifests, runs the agent loop, and defines the
[`Operator`] trait through which a frontend supplies approvals, secrets, input,
and event rendering.

It makes no policy decisions of its own: every step that reaches outside the
sandbox asks the `Operator`.
