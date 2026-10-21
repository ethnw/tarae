<!-- Generated from src/settings.rs — do not edit; regenerate with `TARAE_BLESS=1 cargo test settings_reference`. -->
# Settings reference

Every setting tarae reads from `~/.config/tarae/config.toml` (or a project `.tarae.toml`).
The name is also the `:set` path: `:set editor.scrolloff 8` changes it until you quit,
`:set!` also writes it to your config file. How files and layers work: [configuration guide](../configuration.md).

## Top level

| Setting | Type | Default | Description |
|---|---|---|---|
| `theme` | string | `"default"` | Theme name — built-in meok, hanji, meok-transparent, hanji-transparent, or a file in ~/.config/tarae/themes; "default" = meok/hanji by terminal background |

## `[editor]`

| Setting | Type | Default | Description |
|---|---|---|---|
| `editor.line-numbers` | `"absolute"` · `"relative"` | `"absolute"` | Gutter line numbers |
| `editor.scrolloff` | integer 0–100 | `5` | Lines kept visible above/below the cursor |
| `editor.tab-width` | integer 1–16 | `4` | Display width of a tab; the indent step when a file doesn't show its own |
| `editor.color-modes` | bool | `true` | Color the mode indicator by mode |
| `editor.cursorline` | bool | `true` | Highlight the line of the primary cursor (theme: ui.cursorline.primary) |
| `editor.header` | bool | `true` | Top bar: path › definition under the cursor, and open buffers |
| `editor.inlay-hints` | bool | `true` | Show language server inlay hints (types, parameter names) as dim virtual text |
| `editor.auto-save` | `"off"` · `"focus"` · `"idle"` | `"focus"` | Save modified files automatically: when the terminal loses focus, or also after 2 s idle |
| `editor.persistent-undo` | bool | `true` | Keep undo history across restarts (saved next to your state, dropped if the file changed elsewhere) |
| `editor.recovery` | bool | `true` | Keep a copy of each buffer with unsaved edits (after a pause in typing, in your state folder) — after a crash or a kill, the next start offers them back |
| `editor.recovery-skip` | list of strings | `[]` | Files whose unsaved edits are never copied — a name matches anywhere, in any case ("*.secret"), a path is from the project root ("private/") or ~/; keys and tokens never are (~/.ssh, .env, *.pem…) |
| `editor.restart-build` | string | `""` | For working on tarae itself: a command :restart runs first, e.g. "cargo build --release" ("" = none; :restart! skips it) |
| `editor.restart-binary` | string | `""` | The tarae :restart runs, from the project root if relative ("" = the same path as the one running now) |
| `editor.offer-grammars` | bool | `true` | When a file's language has no syntax grammar yet, offer to download and build it (y/n) |
| `editor.offer-installs` | bool | `true` | When a language server or debugger is missing, offer to install it the way its pack says (y/n/N) |
| `editor.render-doc-comments` | bool | `true` | Show doc comments (///, //!, /** */) rendered as Markdown; raw when the cursor enters the block |
| `editor.restore-files` | bool | `false` | Reopen the files you had open in this folder (with cursor positions) when started without files |
| `editor.inline-diagnostics` | bool | `true` | Show diagnostic messages at the end of their line and tint error/warning lines |
| `editor.cursor-diagnostics` | bool | `true` | Cursor on an underlined problem whose message doesn't fit at the line end: show it in full in a card (Esc hides it) |
| `editor.soft-wrap` | `"always"` · `"prose"` · `"never"` | `"always"` | Show long lines as several rows: always, in prose only (Markdown, commit messages, plain text), or never |
| `editor.zellij-hints` | `"auto"` · `"on"` · `"off"` | `"auto"` | Hints say a key zellij takes by the keys that do the same: inside zellij (auto), always, or never — off for its unlock-first preset or keys of your own |
| `editor.zellij-keys` | list of strings | `["g", "q", "p", "n", "s", "t", "h", "o", "b"]` | The letters zellij takes with Ctrl before tarae sees them — its default normal mode's; a config of your own names its own (session on Ctrl y: "y" for "o") |
| `editor.format-on-save` | bool | `true` | :w formats with the language server first (saves as is if it can't within 2 s) |
| `editor.git-blame` | bool | `true` | Show who last changed the cursor line, when, and why at its end (b in the git tab flips it) |
| `editor.auto-pairs` | bool | `true` | Typing ( [ { " ' ` adds the closer; typing the closer steps over it; backspace removes both |
| `editor.indent-guides` | bool | `true` | Faint vertical guides in leading indentation (theme: ui.virtual.indent-guide) |
| `editor.scrollbar` | bool | `true` | Scrollbar on the right edge, with error/warning marks for the whole file |
| `editor.lsp` | bool | `true` | Start language servers (servers: [lsp.&lt;name>] command/args, per language: [lang.&lt;lang>] lsp = [...]) |
| `editor.lsp-share` | list of strings | `["rust-analyzer"]` | Language servers one of which serves every checkout of a repository (a session each) on the same toolchain — swapped as checkouts come and go; unset: those whose pack says `share` |

## `[editor.cursor]`

| Setting | Type | Default | Description |
|---|---|---|---|
| `editor.cursor.normal` | `"block"` · `"bar"` · `"underline"` | `"block"` | Cursor shape in normal mode |
| `editor.cursor.insert` | `"block"` · `"bar"` · `"underline"` | `"bar"` | Cursor shape in insert mode |
| `editor.cursor.select` | `"block"` · `"bar"` · `"underline"` | `"underline"` | Cursor shape in select mode |

## `[llm]`

| Setting | Type | Default | Description |
|---|---|---|---|
| `llm.command` | string | `"claude"` | LLM CLI speaking claude's stream-json protocol |
| `llm.args` | list of strings | `["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose", "--include-partial-messages", "--tools", "", "--no-session-persistence", "--strict-mcp-config", "--setting-sources", ""]` | Arguments for llm.command (default: claude -p with speed flags) |
| `llm.model` | string | `""` | Model for llm.command, e.g. "haiku" ("" = the CLI's default) |
| `llm.effort` | `""` · `"low"` · `"medium"` · `"high"` · `"xhigh"` · `"max"` | `""` | How hard Claude thinks, for models that take it ("" = the model's default) |
| `llm.follow` | bool | `true` | Chat: the editor follows the code Claude is reading, with its thoughts beside it (C-f toggles it in a chat) |
| `llm.follow-resume` | integer 0–3600 | `20` | Chat: seconds after your last key that follow takes over again while Claude still works (0 = only C-f) |
| `llm.stuck-after` | integer 0–1440 | `10` | Chat: minutes a run of Claude's may print nothing at next to no CPU before a card asks whether it's stuck (0 = never) |
| `llm.stuck-after-outside` | integer 0–1440 | `30` | Chat: …and for a run whose work may go on elsewhere — granted a daemon's socket (Docker's), run outside the sandbox on your y, connected outside its processes — with no time limit of its own (a run all under its own `timeout` isn't asked before that ends, two hours at most) (0 = as llm.stuck-after) |
| `llm.review-at-end` | bool | `false` | Chat: Claude's edits go in as it makes them, and you review its turn when it ends — every new chat starts so (space v a turns it on and off, and saves it here) |
| `llm.edit-tools` | `"tarae"` · `"both"` · `"native"` | `"tarae"` | Chat: what Claude edits with — "tarae": tarae's edit (replacements across files in one call, new files, moves and deletes, one review); "both": edit and Claude Code's Edit and Write; "native": Edit and Write only. A new chat (C-l) starts with it. Only your own config sets it |
| `llm.keep-warm` | `"expected"` · `"marked"` · `"off"` | `"expected"` | Chat: a big conversation waiting on you gets a one-word ping 5 minutes before its prompt cache goes cold, so its next turn reads the cache instead of writing it all again (a ping costs about a tenth of that) — "expected" (the default, on its own — at most llm.keep-warm-max pings a wait): while a review, a card, a rebase or a run of its waits, or you marked it (:keep-warm); "marked": only marked; "off": never. Each ping is said on the chat's last row and in :usage. Only your own config sets it |
| `llm.keep-warm-min-tokens` | integer 1000–10000000 | `100000` | Chat: the smallest conversation (tokens of context) llm.keep-warm keeps warm. Only your own config sets it |
| `llm.keep-warm-max` | integer 1–24 | `3` | Chat: pings at most while a conversation waits (each keeps it warm about another hour); a turn starts the count again. Only your own config sets it |
| `llm.chat-settings` | bool | `true` | Chat: read your CLAUDE.md files and Claude Code settings, like `claude` does (hooks stay off, edits always ask) |
| `llm.card-record` | bool | `true` | Keep every card and its answer — with the command it was about — and every run of Claude's refused with no card in cards/&lt;month>.jsonl in tarae's state folder, on this machine only, read by no run (cargo xtask cards sums them up). Only your own config changes it |
| `llm.environment-key` | `"drop"` · `"keep"` | `"drop"` | Claude: an ANTHROPIC_API_KEY or ANTHROPIC_AUTH_TOKEN in tarae's environment — "drop": Claude runs on your Claude Code login; "keep": passed on, for a login by key alone or a gateway (ANTHROPIC_BASE_URL with its token). When Claude isn't logged in and a key was dropped, a card offers it for the session, or A writes "keep" here. Inside a Claude Code session always dropped (the host's). Only your own config sets it |
| `llm.billing` | `"subscription"` · `"api"` | `"subscription"` | Who pays for a new session's Claude — "subscription": your Claude Code login; "api": your Anthropic API key, from the macOS Keychain (item tarae.anthropic-api-key — tarae never reads it). A session keeps its own; :session-billing switches it. Only your own config sets it |
| `llm.reviewer-billing` | `"same"` · `"subscription"` · `"api"` | `"same"` | Who pays for the reviewer's Claude (space v c) — "same": as its session. Only your own config sets it |
| `llm.screen-terminal` | bool | `true` | Chat: when Claude looks at your screen (you say yes on a card), the terminal panel's contents go too — off: its rows go blank |
| `llm.run-allow` | list of strings | `[]` | Chat: commands Claude runs without asking, past the checks (cargo build/test/clippy/fmt, git status/diff/log…) — word prefixes, like "npm test" |
| `llm.review-ask` | list of strings | `[]` | Chat, review at the end (space v a): more files whose edits still ask first — a name matches anywhere, in any case ("schema.sql"), a path from the project root ("src/secret/"); the protected ones (.git, .tarae.toml, build.rs, pom.xml… — every language pack's) always ask |
| `llm.sandbox` | bool | `true` | Chat: Claude's commands run in a sandbox (Seatbelt on macOS, Landlock and seccomp on Linux) — they write only in the project (not .git, build.rs, pom.xml… — the protected names, every language pack's), temp and the build's folders, read no keys, and connect only where llm.sandbox-ports, -localhost and -network open |
| `llm.sandbox-network` | bool | `false` | Chat: the sandbox lets Claude's commands reach the network beyond this machine, only through tarae's proxy — HTTPS_PROXY, set for every run, where a tool that ignores it can't go out; on Linux where the session has a network of its own, no variable: the proxy takes every connection and answers the names; never link-local addresses, the cloud metadata servers tarae knows of, multicast or broadcast. This machine's ports — on loopback or its own addresses — only as llm.sandbox-ports and -localhost open them |
| `llm.sandbox-hosts` | `"registries"` · `"*"` or a list of hosts | `"registries"` | Chat, with llm.sandbox-network on (unused while it's off): the hosts Claude's commands reach without asking — "registries", the default, is every built-in language pack's package registries (crates.io, npm, PyPI, Go's proxy, Maven Central…, whatever the project's languages; a registry takes uploads too); "*" is every host ("any" reads the same); a list ("github.com", "*.crates.io" — its subdomains, "host:8443"; else 443, or 80 for plain http) is those hosts alone, the registries not added. Any other host waits up to 20 s on a card: this run, this chat, this session, always here, or no |
| `llm.sandbox-upstream` | `"env"` · `"none"` | `"env"` | Chat, with llm.sandbox-network on: "env" — where tarae's own environment names a proxy (HTTPS_PROXY, HTTP_PROXY, ALL_PROXY; NO_PROXY's hosts direct), tarae's proxy goes on through it — http://, or SOCKS5 (socks5:// and socks5h:// alike: the name goes to it): hosts and TLS server names are still checked, the addresses they lead to are that proxy's to look up and aren't; an https:// or SOCKS4 one isn't chained to, and nothing goes that way. "none": straight out, every address checked |
| `llm.sandbox-ports` | list of ports (at most 200) | `[]` | Chat: ports on this machine Claude's commands may connect to in the sandbox — a dev server, a test's database ([3000, 5432]) |
| `llm.sandbox-localhost` | bool | `false` | Chat: every port on this machine open to Claude's commands (tests that talk to their own servers) — the risky ones too (Docker, Jupyter…): the sandbox can't leave some out. On macOS that opens other runs' ports to tarae's proxy too, which then only their tokens guard |
| `llm.sandbox-kept-days` | integer 1–3650 | `14` | Chat: days a toolchain's home kept for Claude's commands (Go's build cache, Gradle's home and daemon) may go unused before tarae's start removes it |
| `llm.sandbox-write` | list of strings | `[]` | Chat: more folders the sandbox lets Claude's commands write to ("~/scratch") — never the project's own folders above it |
| `llm.sandbox-env` | list of strings | `[]` | Chat: more of your environment Claude's commands get ("DATABASE_URL", "MYAPP_*") — they start with PATH, HOME, the locale and the toolchains' own; a token-like one only by its whole name (:sandbox-env lists them) |
| `llm.sandbox-protect` | list of strings | `[]` | Chat: paths in the project the sandbox keeps Claude's commands from changing ("migrations/"); the protected names (.git, .tarae.toml, build.rs, pom.xml… — every language pack's) always, anywhere in it |
| `llm.context-lines` | integer 0–1000 | `20` | Lines of context sent around each selection |

## `[features]`

| Setting | Type | Default | Description |
|---|---|---|---|
| `features.count` | bool | `false` | Count which of tarae's features you use — each command, card, panel, picker and Claude tool's count and last day, never what it was used on — in features.json in tarae's state folder, on this machine only (:features shows them). Only your own config turns it on |

## `[ui]`

| Setting | Type | Default | Description |
|---|---|---|---|
| `ui.shuttle-keys` | `"off"` · `"light"` · `"weighty"` | `"off"` | Let a program you run (an assistant, a test script) type into tarae over its UI socket (tarae keys), each step saying where it must land — off · light: keys outside cards, light cards' answers, every n · weighty: weighty answers too, once read (not built yet: as light). A card asks once for each program; your own key pauses it, :shuttle-end ends it. Only your own config sets it |

## `[session]`

| Setting | Type | Default | Description |
|---|---|---|---|
| `session.worktrees` | string | `""` | Where sessions' checkouts go, a folder per repository ("" = $XDG_DATA_HOME/tarae/worktrees) |
| `session.ended` | list of strings | `[]` | A command, its words, run once a session's checkout is removed (:session-end) — in the checkout tarae started in, outside the sandbox, TARAE_ENDED_CHECKOUT naming the one removed; stopped after 2 minutes; how it ended goes to the log (e.g. ["cargo", "xtask", "linux", "prune"]; [] = none) |
| `session.keep` | list of strings | `[]` | Folders of a session's checkout ("target/sb") moved into the main checkout's &lt;folder>/sessions/&lt;name>/ when the session ends — only where git ignores that there, 64 MB at most, no links or protected names; the end card says what goes |

## `[review]`

| Setting | Type | Default | Description |
|---|---|---|---|
| `review.risky` | list of strings | `[]` | Code review: files whose every change is high-risk — looked at by you (space v h) before the review ends or the session is brought. A name matches anywhere, in any case, one * per part ("sandbox*"); a path from the project root (".github/"). The language packs' patterns (unsafe, spawning…) and the protected names count too. Read from the checkout tarae started in |
| `review.record` | bool | `true` | Keep, for every change you review, how long it was on screen with you there and what it ended as (accepted, rejected, undone, looked at…) in reviews/&lt;month>.jsonl in tarae's state folder, on this machine only — its file, an id and counts, never its code (cargo xtask reviews sums them up). Only your own config changes it |
| `review.quiet` | list of strings | `[]` | Code review: files that are tests, docs or snapshots — their changes graded quiet (never over a high-risk one). Names and paths as review.risky |
| `review.screen` | list of strings | `[]` | Code review: the files that draw the screen — a change in one brings the board's design angle (space v C). Names and paths as review.risky |
| `review.angles` | list of tables | correctness, security, tests, design | The board's angles (space v C): [[review.angles]] tables of name, ask, when (always, behaviour, high, screen), paths, languages, findings-max — replacing the built-in correctness, security, tests, design. Only your own config sets it |
| `review.board-at-once` | integer 1–8 | `3` | The board's Claudes reading at once, every session's together (one under memory pressure, none while it's critical). Only your own config sets it |
| `review.skeptics-max` | integer 0–50 | `12` | The board's skeptics at most — the claims of groups past it go to you unchecked. Only your own config sets it |

## Tables you name yourself

These are read alongside the settings above; see the [configuration guide](../configuration.md).

| Table | What it holds |
|---|---|
| `[keys.normal]` · `[keys.select]` · `[keys.insert]` | Key bindings on top of the [default keymap](keymap.md) |
| `[keys.view]` · `[keys.view.chat]` · `[keys.view.terminal]` · `[keys.chat]` | View mode's keys and the chat input's, on top of the defaults |
| `[keys.terminal]` | Keys taken from the terminal's program, on top of the defaults (`C-\` → its view mode); a key bound to `to_program` goes back to the program |
| `[lsp.<name>]` | A language server: `command`, `args`, `except` (features it's not asked for) |
| `[lang.<language>]` | Per language: `lsp = ["name", …]` — which server to try, in order; `also = […]` — servers that run beside it |
| `[[attach]]` | A named debugger attach target: `name`, `lang`, `host`, `port` or `pid`, `before`, `remote-root`, `program` |
| `[[cards.warn]]` | What a card for one of Claude's commands warns of, besides the language packs' — only adds (an id a pack or your config has is left out): `id`, `run`, `options`, `option-values`, `missing`, `unless-part`, `unless-all`, `same`, `unless-in`, `fact`, `say`, `asks`, `start` — see [Claude integration](../claude-integration.md) |
