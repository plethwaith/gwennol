# Security Policy

## Reporting a vulnerability

Please report suspected security vulnerabilities privately to
**security@plethwaith.com**. Do not open a public issue for security reports.

You should receive an acknowledgement within a few days.

## Scope

Gwennol runs tools and model providers as sandboxed
[Gwead](https://github.com/plethwaith/gwead) plugins. In scope: anything that
lets a plugin reach the filesystem, a process, the network, or a secret it
did not declare and the operator did not approve; and anything that lets
model output cause a host step to run without the operator's decision.

Sandbox escapes in the kernel itself should be reported to Gwead's security
contact (same address) and will be handled there.
