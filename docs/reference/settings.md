<!-- Generated from src/settings.rs — do not edit; regenerate with `TARAE_BLESS=1 cargo test settings_reference`. -->
# Settings reference

Every setting tarae reads from `~/.config/tarae/config.toml` (or a project `.tarae.toml`).
The name is also the `:set` path: `:set editor.scrolloff 8` changes it for this session,
`:set!` also writes it to your config file. How files and layers work: [configuration guide](../configuration.md).

## Top level

| Setting | Type | Default | Description |
|---|---|---|---|
| `theme` | string | `"default"` | Theme name — built-in meok, hanji, meok-transparent, hanji-transparent, or a file in ~/.config/tarae/themes; "default" = meok/hanji by terminal background |

## `[editor]`

| Setting | Type | Default | Description |
|---|---|---|---|
| `editor.line-numbers` | `"absolute"` · `"relative"` | `"relative"` | Gutter line numbers |
| `editor.scrolloff` | integer 0–100 | `5` | Lines kept visible above/below the cursor |
| `editor.tab-width` | integer 1–16 | `4` | Display width of a tab; the indent step when a file doesn't show its own |
| `editor.color-modes` | bool | `true` | Color the mode indicator by mode |
| `editor.cursorline` | bool | `true` | Highlight the line of the primary cursor (theme: ui.cursorline.primary) |
| `editor.header` | bool | `true` | Top bar: path › definition under the cursor, and open buffers |
| `editor.inlay-hints` | bool | `true` | Show language server inlay hints (types, parameter names) as dim virtual text |
| `editor.auto-save` | `"off"` · `"focus"` · `"idle"` | `"focus"` | Save modified files automatically: when the terminal loses focus, or also after 2 s idle |
| `editor.terminal-escape` | string | `'C-\'` | The key that gives the keys back to the code from the terminal (space T) |
| `editor.persistent-undo` | bool | `true` | Keep undo history across restarts (saved next to your state, dropped if the file changed elsewhere) |
| `editor.restart-build` | string | `""` | For working on tarae itself: a command :restart runs first, e.g. "cargo build --release" ("" = none; :restart! skips it) |
| `editor.restart-binary` | string | `""` | The tarae :restart runs, from the project root if relative ("" = the same path as the one running now) |
| `editor.offer-grammars` | bool | `true` | When a file's language has no syntax grammar yet, offer to download and build it (y/n) |
| `editor.offer-installs` | bool | `true` | When a language server or debugger is missing, offer to install it the way its pack says (y/n/N) |
| `editor.render-doc-comments` | bool | `true` | Show doc comments (///, //!, /** */) rendered as Markdown; raw when the cursor enters the block |
| `editor.restore-session` | bool | `false` | Reopen the files you had open in this folder (with cursor positions) when started without files |
| `editor.inline-diagnostics` | bool | `true` | Show diagnostic messages at the end of their line and tint error/warning lines |
| `editor.cursor-diagnostics` | bool | `true` | Cursor on an underlined problem whose message doesn't fit at the line end: show it in full in a card (Esc hides it) |
| `editor.soft-wrap` | `"always"` · `"prose"` · `"never"` | `"always"` | Show long lines as several rows: always, in prose only (Markdown, commit messages, plain text), or never |
| `editor.format-on-save` | bool | `true` | :w formats with the language server first (saves as is if it can't within 2 s) |
| `editor.git-blame` | bool | `true` | Show who last changed the cursor line, when, and why at its end (b in the git tab flips it) |
| `editor.auto-pairs` | bool | `true` | Typing ( [ { " ' ` adds the closer; typing the closer steps over it; backspace removes both |
| `editor.indent-guides` | bool | `true` | Faint vertical guides in leading indentation (theme: ui.virtual.indent-guide) |
| `editor.scrollbar` | bool | `true` | Scrollbar on the right edge, with error/warning marks for the whole file |
| `editor.lsp` | bool | `true` | Start language servers (servers: [lsp.&lt;name>] command/args, per language: [lang.&lt;lang>] lsp = [...]) |

## `[editor.cursor]`

| Setting | Type | Default | Description |
|---|---|---|---|
| `editor.cursor.normal` | `"block"` · `"bar"` · `"underline"` | `"block"` | Cursor shape in normal mode |
| `editor.cursor.insert` | `"block"` · `"bar"` · `"underline"` | `"bar"` | Cursor shape in insert mode |
| `editor.cursor.select` | `"block"` · `"bar"` · `"underline"` | `"underline"` | Cursor shape in select mode |

## `[llm]`

| Setting | Type | Default | Description |
|---|---|---|---|
| `llm.claude-code` | bool | `true` | Let Claude Code connect (space c opens the connection; then `claude` → /ide works too): it sees your selection and diagnostics, and its edits come here as a y/n diff |
| `llm.command` | string | `"claude"` | LLM CLI speaking claude's stream-json protocol |
| `llm.args` | list of strings | `["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose", "--include-partial-messages", "--tools", "", "--no-session-persistence", "--strict-mcp-config", "--setting-sources", ""]` | Arguments for llm.command (default: claude -p with speed flags) |
| `llm.model` | string | `""` | Model for llm.command, e.g. "haiku" ("" = the CLI's default) |
| `llm.effort` | `""` · `"low"` · `"medium"` · `"high"` · `"xhigh"` · `"max"` | `""` | How hard Claude thinks, for models that take it ("" = the model's default) |
| `llm.follow` | bool | `true` | Chat: the editor follows the code Claude is reading, with its thoughts beside it (C-f toggles per session) |
| `llm.follow-resume` | integer 0–3600 | `20` | Chat: seconds after your last key that follow takes over again while Claude still works (0 = only C-f) |
| `llm.review-at-end` | bool | `false` | Chat: Claude's edits go in as it makes them, and you review its turn when it ends — every new chat starts so (space v a turns it on and off, and saves it here) |
| `llm.chat-settings` | bool | `true` | Chat: read your CLAUDE.md files and Claude Code settings, like `claude` does (hooks stay off, edits always ask) |
| `llm.screen-terminal` | bool | `true` | Chat: when Claude looks at your screen (you say yes on a card), the terminal panel's contents go too — off: its rows go blank |
| `llm.run-allow` | list of strings | `[]` | Chat: commands Claude runs without asking, past the checks (cargo build/test/clippy/fmt, git status/diff/log…) — word prefixes, like "npm test" |
| `llm.review-ask` | list of strings | `["build.rs", "Cargo.toml", "package.json", "Makefile", "justfile", ".envrc", ".tarae.toml", ".cargo", ".github", ".claude", ".vscode"]` | Chat, review at the end (space v a): files whose edits still ask first — a name matches anywhere ("build.rs"), a path from the project root ("src/secret/"); .git always asks |
| `llm.sandbox` | bool | `true` | Chat: Claude's commands run in a sandbox — they write only in the project (not .git), temp and the build's folders, and connect to nothing but the ports you open (macOS) |
| `llm.sandbox-network` | bool | `false` | Chat: the sandbox lets Claude's commands reach the network — but not this machine's risky ports (Docker, Jupyter…) unless in llm.sandbox-ports |
| `llm.sandbox-ports` | list of ports (at most 200) | `[]` | Chat: ports on this machine Claude's commands may connect to in the sandbox — a dev server, a test's database ([3000, 5432]) |
| `llm.sandbox-localhost` | bool | `false` | Chat: every port on this machine open to Claude's commands (tests that talk to their own servers) — the risky ones too (Docker, Jupyter…): the sandbox can't leave some out |
| `llm.sandbox-write` | list of strings | `[]` | Chat: more folders the sandbox lets Claude's commands write to ("~/.npm") — never the project's own folders above it |
| `llm.sandbox-protect` | list of strings | `[]` | Chat: paths in the project the sandbox keeps Claude's commands from changing ("migrations/"); .git and .tarae.toml always |
| `llm.context-lines` | integer 0–1000 | `20` | Lines of context sent around each selection |

## `[session]`

| Setting | Type | Default | Description |
|---|---|---|---|
| `session.worktrees` | string | `""` | Where sessions' checkouts go, a folder per repository ("" = $XDG_DATA_HOME/tarae/worktrees) |

## Tables you name yourself

These are read alongside the settings above; see the [configuration guide](../configuration.md).

| Table | What it holds |
|---|---|
| `[keys.normal]` · `[keys.select]` · `[keys.insert]` | Key bindings on top of the [default keymap](keymap.md) |
| `[lsp.<name>]` | A language server: `command`, `args`, `except` (features it's not asked for) |
| `[lang.<language>]` | Per language: `lsp = ["name", …]` — which server to try, in order; `also = […]` — servers that run beside it |
| `[[attach]]` | A named debugger attach target: `name`, `lang`, `host`, `port` or `pid`, `before`, `remote-root`, `program` |
