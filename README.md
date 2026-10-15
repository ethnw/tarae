<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/screenshots/hero-meok.png">
    <img src="docs/screenshots/hero-hanji.png" width="100%" alt="tarae: Rust code with Claude's change, a new line for hours, waiting for y or n under review; the chat docked on the right, with two sessions' tabs over it and Claude's turn: it read lib.rs, ran cargo test in the sandbox (ok), and said both tests pass">
  </picture>
</p>

<h1 align="center">tarae</h1>

<p align="center">
  <b>A terminal editor with Claude inside.</b><br>
  Helix's keys · Claude's commands sandboxed · sessions side by side
</p>

<p align="center">
  <a href="docs/features.md">Docs</a> ·
  <a href="docs/threat-model.md">Threat model</a> ·
  <a href="CONTRIBUTING.md">Contributing</a> ·
  <a href="SECURITY.md">Security</a>
</p>

<p align="center"><sub>tarae (타래, "a skein") is written in itself, with Claude · early, v0.1: build from source</sub></p>

## Install

```sh
git clone https://github.com/ethnw/tarae && cd tarae
cargo install --path .
tarae .
```

macOS or Linux (WSL on Windows), with rustup and a C compiler. Claude features need
[Claude Code](https://claude.com/claude-code) logged in. `space ?` finds any command; `:tutor` teaches the keys.

## Why tarae

- **Never waits** — LSP, parsing, git and disk stay off the key path. A frame in under 16 ms.
- **Claude as a verb** — select, `space i`, say it. Review the diff.
- **Sandboxed by default** — Claude's commands run confined: Seatbelt, or Landlock and seccomp. The network goes
  through tarae.
- **Sessions side by side** — a checkout, a Claude and a terminal each. Bring them back.
- **Review like a pull request** — hunk by hunk, or at the turn's end. A second Claude reviews too.
- **Helix's keys** — the same keymap and command names ([coming from Helix](docs/coming-from-helix.md)).
- **Plugins in Rust** — sandboxed WebAssembly. [Kubernetes](docs/plugins.md) beside your manifests.

## Docs

[Features](docs/features.md) · [Claude](docs/claude-integration.md) · [Threat model](docs/threat-model.md) ·
[Configuration](docs/configuration.md) · [Languages](docs/languages.md) · [Plugins](docs/plugins.md) ·
[Keymap](docs/reference/keymap.md) · [Architecture](docs/architecture.md)

## License

[GPL-3.0-or-later](LICENSE). The plugin SDK and the plugins in `plugin-sdk/` are MIT OR Apache-2.0
([MIT](plugin-sdk/LICENSE-MIT), [Apache](plugin-sdk/LICENSE-APACHE)); db's test transcripts hold servers' bytes
([NOTICE](plugin-sdk/db/NOTICE)). Helix's query files stay MPL-2.0 ([NOTICE](runtime/queries/NOTICE)).
Contributions need a signed [CLA](CONTRIBUTING.md#license-and-the-cla).
