# tarae (타래)

**tarae makes tarae.** It's written in itself — with Claude, inside it.

A terminal editor started out of love for both code and AI: Helix's keys and selection-first editing, Claude
built in, and an editor that never makes you wait.

<p align="center">
  <img src="docs/screenshots/hero-meok.png" width="49%" alt="tarae in meok (ink): the project's files as a tree on the left, Rust code with a changed line marked in the gutter, rust-analyzer's completion list under the cursor">
  <img src="docs/screenshots/hero-hanji.png" width="49%" alt="the same screen in hanji (paper): warm paper background, vermilion accent — picked automatically on light terminals">
</p>

**Status: early.** Version 0.1, no releases yet — build it from source.

## Quick start

```sh
git clone https://github.com/ethnw/tarae && cd tarae
cargo install --path .        # rustup and a C compiler; --features bundled-grammars builds 26 grammars in
tarae .                       # space ? finds any command · :tutor teaches the keys in 10 minutes
```

macOS or Linux (Windows under WSL). Claude features need [Claude Code](https://claude.com/claude-code) logged in;
everything else works without it.

## What's in it

- **Helix's keys** — the same keymap and command names; your `[keys.normal]` works as it is ([coming from Helix](docs/coming-from-helix.md))
- **Claude as a verb** — select, `space i`, say what you want; the answer lands as a diff you accept change by change
- **Never waits** — LSP, parsing, disk and git run in the background; a key → frame stays under 16 ms on 200k lines
- **The workbench** — files, outline, git, a terminal, tests and a debugger, without leaving the keys
- **Plugins in Rust** — sandboxed WebAssembly; the [Kubernetes plugin](docs/plugins.md) shows the cluster beside your manifests

## Docs

[Features](docs/features.md) · [Claude](docs/claude-integration.md) · [Threat model](docs/threat-model.md) ·
[Languages](docs/languages.md) ·
[Tests and debugging](docs/testing-and-debugging.md) · [Configuration](docs/configuration.md) ·
[Plugins](docs/plugins.md) · [Keymap](docs/reference/keymap.md) · [Architecture](docs/architecture.md) ·
[Contributing](CONTRIBUTING.md)

## License

[GPL-3.0-or-later](LICENSE). The plugin SDK is MIT OR Apache-2.0; Helix's query files stay MPL-2.0 — see
[NOTICE](runtime/queries/NOTICE). Contributions need a signed CLA ([Contributing](CONTRIBUTING.md#license-and-the-cla)).
