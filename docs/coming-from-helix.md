# Coming from Helix

tarae keeps **Helix's keymap, its selection → action grammar, and its batteries-included spirit** — and rebuilds
everything else to fix what bothered us about Helix. It is not a fork.

| Helix pain point | tarae |
|---|---|
| Settings are a chore to change | One TOML file, applied on save; `:set` at runtime with descriptions and completion |
| Slow when a language server attaches | Keystrokes and rendering never wait — LSP, parsing, disk, and git all run in the background |
| No LLM support | Select → instruct → diff review and a chat panel with Claude, built in |
| Immature plugins | Features go straight into the core; for the rest, [plugins](plugins.md) in Rust run sandboxed on tarae's own small WebAssembly runtime |

## Keys

The keys, command names, and key-config notation are Helix's — a `[keys.normal]` table from your Helix config works
as it is (put it in tarae's config). Where tarae needed a key for something Helix doesn't have, it took these:

| Key | In tarae | In Helix |
|---|---|---|
| `space g` | The git panel, lazygit-style: files → their hunks and conflicts (`s` stage, `d` discard, `o`/`t`/`a`), commit (`C`: Claude drafts the message), `p`/`P` pull/push, `B` branches, `L` log, stash — `?` lists them | Changed files (a picker) |
| `space e` · `space o` · `space q` | Side panel: files · outline · the list (quickfix) | File explorer (a picker) |
| `space i` · `space l` · `space n` · `space v` | Ask Claude to edit the selection · chat · a note on the line · review your changes | — |
| `space u` · `space U` | Undo what Claude changed in its last turn, across files · bring it back | — |
| `space m` · `space x` · `space T` | Build tasks · tests · terminal | — |
| `]q` `[q` · `]x` `[x` · `]m` `[m` · `]n` `[n` | The list · failed tests · conflicts · notes | — |

Everything else — `space f`, `space /`, `gd`, `m` mode, `z` view mode, `C-w` windows, `space G` debugging — is where
Helix has it. The [keymap reference](reference/keymap.md) lists every key.
