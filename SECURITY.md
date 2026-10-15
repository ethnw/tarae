# Security

## Reporting a vulnerability

Email **security@sinabro.io** — not a public issue. Please include:

- what you did (the steps, a script or a repository that shows it),
- what happened, and what you expected instead,
- the OS and version (on Linux the kernel too, and the tier a run's report names),
- tarae's commit (`git rev-parse HEAD` in your checkout).

## What's in scope

- tarae's sandbox for Claude's commands: Seatbelt on macOS; Landlock, seccomp and the keeper on Linux.
- The confinement of the `claude` process itself.
- The trust gates: a project's settings (`.tarae.toml`), a repository's git config and hooks, a repository's
  Claude settings.
- The egress proxy Claude's commands reach the network through.
- The plugins' WebAssembly runtime and the permissions a plugin is given.
- The UI socket (`tarae state`): only this user's processes reach it, and claude's are refused.

[The threat model](docs/threat-model.md) says what tarae claims to keep from a run and what it doesn't. Its known
gaps ([§4](docs/threat-model.md#4-gaps-in-one-list)) aren't new reports — unless you find one is worse than it
says.

## Supported versions

`main` only. There are no releases yet.

## How reports are handled

An acknowledgement within 7 days, a fix on `main`, and credit in the commit and the release notes if you want it.
There's no bounty.
