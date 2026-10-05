# Configuration

tarae is configured with one TOML file in tarae's own schema. Configuration is data — no code runs in it. Every
setting, with its type and default, is listed in the [settings reference](reference/settings.md); every default key in
the [keymap reference](reference/keymap.md).

- [Where it lives](#where-it-lives)
- [Changing settings while you work](#changing-settings-while-you-work)
- [Full example](#full-example)
- [Keys](#keys)
- [Language servers](#language-servers)
- [Build tasks](#build-tasks)
- [Custom themes](#custom-themes)

## Where it lives

- **User config**: `~/.config/tarae/config.toml` (or `$XDG_CONFIG_HOME/tarae/config.toml`). `:config-open` opens it
- **Project config**: `.tarae.toml`, found by searching upward from the current directory — handy for per-project
  debugger [attach targets](testing-and-debugging.md#attaching-to-a-running-program)
- **Your settings for one project**: `.tarae.local.toml` beside its `.tarae.toml` (or in the folder you start
  tarae in). It's yours, so nothing in it waits for trust — which is why it's read only while git says it's
  **untracked and ignored** (add it to `.gitignore`, or `.git/info/exclude` to keep that to yourself). A committed
  one came with the repository, an unignored one may have come with a download of it, and outside a git
  repository git can't tell: then it isn't read, and a warning says why. Its `[preflight]` steps go after the
  project's, not in their place
- **Layers**, later ones winning: defaults < user config < project `.tarae.toml` < your `.tarae.local.toml` <
  `:set` at runtime
- **A project's file is trusted before it runs anything.** A `.tarae.toml` usually comes with the project, so
  someone else may have written it. What in it runs commands or loosens how Claude's commands are guarded waits
  until you trust the project: `llm.command`, `llm.args`, `llm.run-allow`, `llm.review-ask`, `llm.review-at-end`,
  `llm.chat-settings`, `llm.sandbox*`, `editor.restart-build` and `restart-binary`, `session.worktrees` (where checkouts are
  written), `[task.*]`, `[preflight]` steps, a `[lsp.*]`
  `command` or `args`, an `[[attach]]` `before`, and keys bound to `:` commands. A card shows each one as written: `y` trusts the
  project, `n` ignores that part. The rest of the file applies either way. The answer is kept per project and
  asked again when any of those values change. `:project-trust` shows the card again, and `:project-untrust`
  takes trust back

One file holds everything — editor, keys, languages, language servers, and Claude. There's no separate
`languages.toml`.

- **Applied on save** — no restart (`:config-reload` forces a reread)
- **Mistakes don't block startup** — a bad value or an unknown name is reported as `file:line: message`, and the rest
  of the file still applies
- **Helix config files aren't read.** Only the key notation (`"A-x"`) and command names are shared, so a Helix
  `[keys.normal]` table carries over as it is

## Changing settings while you work

The name of a setting is its `:set` path — the same dotted name as in the file.

| Command | Does |
|---|---|
| `:set editor.scrolloff` | Show the current value, its description, and the allowed values |
| `:set editor.scrolloff 8` | Change it until you quit (it stays even if the config file is reloaded) |
| `:set! editor.scrolloff 8` | Change it and write it to your config file — comments and formatting are kept |
| `:toggle editor.color-modes` | Flip an on/off setting, or move a choice setting to its next value |
| `:config-show` | The full effective configuration, with descriptions as comments, in a new buffer — usable as a config file as it is |
| `:config-open` · `:config-reload` | Open your config file · reread it |

`Tab` completes setting paths and, for on/off and choice settings, their values. `space t` picks a theme with a live
preview and saves your choice.

## Full example

```toml
theme = "default"            # meok · hanji · meok-transparent · hanji-transparent · default (follows the terminal)

[editor]
line-numbers = "relative"
scrolloff = 5
tab-width = 4
color-modes = true           # color the mode pill, cursor, and line number per mode
auto-save = "focus"          # "focus" · "idle" · "off"

[editor.cursor]
normal = "block"             # "block" · "bar" · "underline"
insert = "bar"
select = "underline"

[llm]
model = "haiku"              # "" = the CLI's default model
context-lines = 20           # lines around the selection sent along

[keys.normal]
"A-," = "goto_previous_buffer"
"A-." = "goto_next_buffer"
C-l = ":sh zellij run -c -f -- lazygit"
```

## Keys

Bindings go in `[keys.normal]`, `[keys.select]`, and `[keys.insert]` and are merged on top of the
[default keymap](reference/keymap.md). The notation and command names are Helix's:

```toml
[keys.normal]
X = ["extend_line_up", "extend_to_line_bounds"]  # a list runs in order
"A-w" = ":buffer-close"                          # a leading ':' runs a : command
C-k = ":sh tmux split-window -h"                 # … including shell commands

[keys.normal.space]
e = "file_picker"                                # tables are sub-menus (space e)
```

- Keys: `a`, `A` (shift), `C-x` (ctrl), `A-x` (alt), and names like `space`, `ret`, `esc`, `tab`, `backspace`, `del`,
  `left`, `pageup`, `home`, `F5`
- Sub-menus merge key by key — adding `space e` keeps the rest of `space`
- Like Helix, `[keys.normal]` doesn't spill into select mode; bind a key in `[keys.select]` too if you want it there
- `space ?` lists every command with a description, so you can find a name to bind

## Language servers

tarae tries a built-in list of servers per language and uses the first one on your `PATH` (the list:
[Language support](languages.md)). Define a server with `[lsp.<name>]` and pick servers per language with
`[lang.<language>]`:

```toml
[lsp.basedpyright]
command = "basedpyright-langserver"
args = ["--stdio"]

[lang.python]
lsp = ["basedpyright"]     # tried in order; the first one on PATH starts
also = ["ruff"]            # these start beside it (each if on PATH) — the default for Python

[lsp.typescript-language-server]
except = ["format"]        # a built-in server needs no command; `except` leaves a feature to the others
```

A file can have several servers: the main one from `lsp`, and linters or formatters with a server of their own
from `also` (Python gets `ruff` beside pyright out of the box). Every server gets the file and its changes; their
diagnostics show together; `space a` lists every server's code actions (saying whose, when there's more than one);
completion asks every server that completes and merges their lists as they answer (TypeScript and Tailwind
together, say); anything else goes to the first server — main one first — that says it can, so a formatter beside a server that
doesn't format just works. `except` takes `diagnostics`, `format`, `completion`, `hover`, `code-action`, `rename`,
`goto`, `signature-help`, `inlay-hints`, `symbols`, `highlights`. The status line names the ones beside the main
one: `python + ruff`.

`editor.lsp = false` turns language servers off entirely.

## Build tasks

Tasks the project suggests (`cargo check`, `npm run lint`, `make`, …) are there without setup — they come from the
language packs ([Tasks from packs](testing-and-debugging.md#tasks-from-packs)). Add your own — they come first in
`space m p`:

```toml
[task.lint]
cmd = "cargo clippy --all-targets --message-format=short -- -D warnings"

[task.web]
cmd = "npm run build"
cwd = "web"                # relative to where tarae started
```

A task runs with `sh -c`; `file:line:col: message` lines in its output go into the list (`]q`).

## Checks before a push (`:preflight`)

`:preflight` runs a project's checks one after another in the task panel — every one, whatever the ones before
did — each with its time and how it ended, the places any of them points at in the list (`]q`), and a one-line
summary at the end (`5 of 6 passed — clippy failed · 4m 12s`).

```toml
[preflight]
steps = [
  { name = "fmt", cmd = "cargo fmt --all --check" },
  { name = "clippy", cmd = "cargo clippy --all-targets -- -D warnings" },
  { name = "test", cmd = "cargo test" },
  # a line matching `note` (a regex) makes the pass one with a note; `full = true`: only before publishing
  { name = "perf", cmd = "cargo perf", note = "not measured|SLOWER", full = true },
]
```

Two sets, for the two moments: `:preflight` runs the **quick** one — every step but those marked `full = true` —
and so does `p` on the card that brings a session into its base after a rebase. `:preflight-full` runs
**every** step: the slow ones (a second feature build, measurements, a Linux run in Docker) are for before you
publish. The panel's tab and the summary say which ran (`quick preflight: all 3 passed · 1m 02s — the full set (3
more steps) runs before publish: :preflight-full`). A project that marks no step has one set: both commands run
every step, as before.

Where the steps come from:

- **The project's** `[preflight]` in `.tarae.toml` — run once you've trusted the project. (In your own config
  it's for every project.)
- **Else the language packs' defaults**, by the files that mark the project — only where the language's own
  toolchain defines the check: Rust (`Cargo.toml`: `cargo fmt --all --check`, `cargo clippy --all-targets -- -D
  warnings`, `cargo test`), Go (`go.mod`: `gofmt -l` must list nothing, `go vet ./...`, `go test ./...`) and
  Java with the Gradle or Maven installed here (`build.gradle(.kts)` or `settings.gradle(.kts)`: `gradle test
  --console=plain`; `pom.xml`: `mvn -B test`), together in a project that is several. The commands are
  tarae's, but what they run is the project's code — its tests and build scripts — so the first run in a
  project shows them on a card that says so (`y` runs them): run them only for code you trust.

  A Java project with its own wrapper (`./gradlew`, `./mvnw`) builds with that, a script from the repository:
  tarae's defaults never run it, and `:preflight` says to list it in the project's `[preflight]` — `{ name =
  "test", cmd = "./gradlew test --console=plain" }` — which runs once you trust the project. Other languages
  have none, and their steps go in the project's `[preflight]`: JavaScript has npm, pnpm, yarn and bun to run
  it and jest, vitest, mocha … to test it (and `npm init`'s default `test` script fails on purpose);
  TypeScript's `tsc` usually lives in the project's `node_modules/.bin`, and monorepos and frameworks check
  types their own way (`tsc -b`, `vue-tsc`, `svelte-check`); Python has no single standard to guess.
- **Then yours**, from `.tarae.local.toml`'s `[preflight]`: added after either (`full = true` works there too).

## Custom themes

Themes live in `~/.config/tarae/themes/<name>.toml`; select one with `theme = "<name>"`, `:theme <name>`, or
`space t`.

```toml
inherits = "meok"                      # build on a built-in theme or one of your own
"keyword" = "accent"                   # color only
"comment" = { fg = "dim", italic = true }
"diagnostic.error" = { underline = { color = "rose", style = "curl" } }

[palette]
accent = "#ef8a5a"
link = "accent"                        # palette entries can point to each other
```

- Colors: `#rrggbb`, `#rgb`, ANSI names (`red`, `light-blue`, …), or palette names
- Modifiers: `bold`, `italic`, `underline`, `dim`, `reversed`, `crossed` (a `modifiers = [...]` list works too)
- Typos in color names and unknown attributes are reported as warnings, not silently ignored
- tarae-only keys: `ui.accent` (the single accent color) and `ui.tint` (the base that faint colors blend into — this is
  what the transparent variants set instead of a background)
- Built-in themes: `meok`, `hanji`, `meok-transparent`, `hanji-transparent`; `default` picks meok or hanji to match the
  terminal background
