# Architecture

Why tarae is built the way it is, where it's going, and where things live in the source. For the working rules when
changing the code, see [CONTRIBUTING.md](../CONTRIBUTING.md).

- [Principles](#principles)
- [How it stays fast](#how-it-stays-fast)
- [Design language](#design-language)
- [Roadmap](#roadmap)
- [Plugins](#plugins)
- [Source layout](#source-layout)

## Principles

1. **Your hands are in Helix.** Keymap, selection → action grammar, and command names match Helix, so key configs carry
   over. Everything else — architecture, UI, configuration, feature set — is tarae's own.
2. **Batteries included.** Needed features go into the core, kept small.
3. **Configuration is data.** No code in config files. Changes apply on save, or at runtime with `:set`, and a setting's
   name is its `:set` path. Bad settings are reported with line numbers instead of blocking startup.
4. **Never make you wait.** Keystrokes and screen updates never block on LSP, the LLM, disk, or parsing. Slow work runs
   in the background, late results are discarded, and performance budgets are tests.
5. **The LLM is a first-class verb.** In a selection-first editor, the LLM takes the action slot — instruct, review the
   diff, accept. What's on screen is open to outside drivers as data (`tarae state`).
6. **Makes you want to try it at a glance.** Design weighs as much as features. Floating things speak the same color
   language as the code, breathe with whitespace, and fold by priority when space runs out.
7. **Small and testable.** The core doesn't know about the terminal; it's tested whole by feeding it key sequences
   (`"xd"`, `"Ci-<esc>"` …).

## How it stays fast

- **One event loop.** An input thread and a single event queue ([`src/event.rs`](../src/event.rs)); queued events are
  handled together and drawn once. A frame is drawn whole, but only the cells that changed go to the terminal
  ([`src/screen.rs`](../src/screen.rs)). Slow work goes through `Jobs::spawn` on a worker thread, and its result
  comes back as a closure applied on the main loop.
- **Language servers** get a writer thread (owns stdin) and a reader thread (owns stdout) each; the main thread only
  pushes onto channels — even serializing a whole document for `didOpen` happens on the writer thread. Changes go out
  as incremental `didChange` once per event batch. Positions are negotiated as UTF-8, so there's no conversion layer.
- **Saving** is written on a saver thread, in the order asked ([`src/saver.rs`](../src/saver.rs)), as Helix does:
  `:w` checks the file where it's asked, hands over the text (a rope clone) and returns; "Saved" comes when it's on
  disk — temp file, fsync (F_FULLFSYNC on macOS), rename. Quitting waits for what's still being written. `:wa` of ten
  files held the main loop 93 ms before; now about 1 ms.
- **Parsing** runs on a worker thread and is incremental: edits apply to the existing tree immediately, and edits made
  during a parse are replayed onto the new tree. A full parse of a 20k-line Rust file takes 145 ms — in the background.
- **Positions are byte offsets** everywhere (tree-sitter, regex, and LSP all speak bytes), always on character
  boundaries; "one character" is a grapheme cluster.
- **Budgets are tests.** Worst key → frame on a 200k-line file: 1.0 ms in a debug build, 52 µs in release (budget
  16 ms). Worst key → frame with highlighting on: 1.1 ms (release). A 100 MB file shows its first screen in 18 ms and is
  fully loaded in 75 ms. The same 16 ms budget holds, as tests, for the side panel over a 200k-file project, typing
  in the file picker over those files, the git tab with 20k changed files, a program flooding the terminal panel,
  and 500 merge conflicts in a 200k-line file.
- **Lists are built when they change**, not when they're drawn: the side panel's rows are rebuilt when a folder
  opens or the list, git status, or outline changes. The picker narrows its last matches as the query grows,
  keeps the last few queries' for a backspace, and filters a big list (50k+) on a worker, on every core. A terminal program's output is read on its own thread and batched.

## Design language

tarae's look is a pair: **meok** (ink — a dark ink background with a coral accent) and **hanji** (paper — warm paper
with a vermilion accent). The two built-in themes have exactly the same keys.

- **One accent color** (`ui.accent`), used boldly and sparingly; syntax colors stay calm
- **Floating things are cards** — half-cell blocks give half a line of padding; themes without a background color get
  rounded borders instead. The selected row gets an accent bar `▎`; typed characters are accent bold
- **Two dim levels only** — `ui.virtual` (dim) and `ui.linenr` (faint)
- **Glyphs** only from the set every common monospace font has (JetBrains Mono, Hack, Menlo, SF Mono):
  `ƒ τ ν π § ◦ # ¶ ● ▲ › ▎ ▐ ▌ ▄ ▀`

Early mockups that set the direction: [ink](design/a-ink.png) · [paper](design/b-paper.png) ·
[aurora](design/c-aurora.png).

## Roadmap

| Stage | Scope |
|---|---|
| **M0** ✅ | Core model (rope, multiple selections, transactions, snapshot undo), movement and editing, buffers, `:sh` |
| **M1** ✅ | Non-blocking event loop, config schema and live reload, performance budget tests, background loading, search and regex selection, registers, macros |
| **M2** ✅ | Byte positions, tree-sitter highlighting, themes, `m` mode, pickers, global search, status line |
| **M3** ✅ | LSP — diagnostics, navigation, hover, completion, signature help, inlay hints, code actions with preview, rename, format |
| **M4** ✅ | Claude — select → instruct → diff, streaming answers, chat panel; the chat explores the project, the editor follows it with its thinking beside the code, notes pinned to lines you answer in place, its edits reviewed before they're written, and a pull-request-style review of all changes sent back in one go |
| **M5** (retired 2026-10-04) | Claude Code IDE protocol — selection sharing, diagnostics, in-buffer review of edits; removed once the chat (`space l`) did all of it in tarae |
| **Experience** ✅ | which-key, palette, tutor, start screen, mouse, toasts, git gutter, reopening the last files, persistent undo, disk sync, doc comments, splits, tests, debugger, attach |
| **Workbench** ✅ | Side panel (files, outline, list, git), one folder tree for every list of files, indentation from `indents.scm`, several language servers per file, the list (quickfix) and build tasks, git tab (stage, commit, push, log, branches), merge conflicts, `:diff`, built-in terminal |
| **M6** ✅ | Plugins — commands in Rust, compiled to WebAssembly, sandboxed on tarae's own runtime (interpreter + JIT); `tarae plugin new/dev/list` |

Non-goals: a GUI, vim emulation, copying Helix behavior for its own sake.

What landed since, what's under way and what comes next: [roadmap.md](roadmap.md).

## Plugins

Language features stay in the core. Plugins are for the rest: commands written in Rust, compiled to WebAssembly, run
sandboxed on tarae's own runtime ([`src/wasm/`](../src/wasm)) by the plugin host ([`src/plugin/`](../src/plugin)).
How to write and install one: [plugins.md](plugins.md).

### The runtime

It's tarae's own, not wasmi or wasmtime, because of what those would add to the binary:

| | Binary | Load | fib(30) | JSON round trip | sort 1M | n-body 1M steps |
|---|---|---|---|---|---|---|
| **tarae — JIT** | +243 KB (aarch64), +305 KB (x86-64), with the host | 1.4 ms | 3.1 ms | 3.0 ms | 20 ms | 62 ms |
| tarae — interpreter | (included) | 1.4 ms | 34 ms | 45 ms | 359 ms | 1090 ms |
| wasmi 2 | +1 MB | 0.3 ms | 17 ms | 18 ms | 151 ms | 620 ms |
| wasmtime 49 (cranelift) | +5.4 MB | 48 ms | 3.2 ms | 2.2 ms | 15 ms | 44 ms |

These are the same Rust plugin workloads on an M3 Max, with fuel metering on. The binary column is measured inside
tarae's release build (wasmi's and wasmtime's without a host).

- **Scope**: WebAssembly 1.0, plus what rustc turns on by default for `wasm32-unknown-unknown`: bulk memory,
  multi-value, mutable globals, non-trapping float → int, reference types, and sign extension. It leaves out SIMD,
  threads, GC, exceptions, tail calls and memory64. It also takes **no imports**: a plugin gets no WASI and no host
  functions, only its input.
- **Pipeline**: decode → validate while compiling to *register ops* → run.
  - Register ops give every operand a fixed frame slot. `local.get` and constants cost nothing, a compare feeding a
    branch fuses with it, and branches know where values go.
  - They run on the **JIT** (aarch64 and x86-64) or on the **interpreter**. The interpreter covers every other CPU,
    and machines that refuse executable memory: for example a hardened-runtime macOS build without the
    `allow-jit` entitlement, or SELinux `deny_execmem`.
- **JIT**: a few instructions per op over the frame slots, with these on top:
  - a per-block register cache, with a float side: float arithmetic stays in float registers
  - home registers for each loop's busiest locals, float registers for f32/f64 ones
  - bounds checks once per address per block. On AArch64 a check is one compare against memory length − 4 KB,
    with the exact check out of line for accesses near the end, and a zero-offset access is one instruction
  - a load of something just stored takes the stored value
  - calls and rare ops (tables, bulk memory, `memory.grow`) go through small Rust helpers
  - long runs of values (block results, returns) copied in a loop, so code size stays proportional to the module's.
    A module that would still compile to over 64 MB, or with branches out of AArch64's reach, runs on the
    interpreter
- **Limits**: fuel is charged at every function entry and loop head for what the body costs (an op each, a move of
  n values n, the locals a call zeroes), and per byte by the ops that copy, fill or grow — so fuel bounds time. The
  value stack, call depth, memory and tables are capped. Loading is bounded too: at most 1000 params or results per
  type, 4M locals per module, 100 tables, and a budget of compiling steps, so no small module takes long or much
  memory to load. Compiled calls nest on the native stack, up to 96
  bytes each, so the default call depth needs 1.9 MB of it. `unsafe` is used only in `jit/mod.rs`, for executable
  memory and entering it.
- **Checked by**:
  - the official spec test suite, all of it within scope, on both engines and both backends, on macOS and Linux
  - differential fuzzing: JIT against interpreter exactly, fuel and stack limits included; against wasmi, with
    wasmtime deciding when they differ

  Both tools are in [`fuzz/`](../fuzz).

### The host

- **API**: [`plugin-sdk/wit/tarae.wit`](../plugin-sdk/wit/tarae.wit), 0.2 (until 1.0 each 0.x breaks plugins, so
  only this one loads). A plugin exports `commands()`, `subscriptions()`, `permissions()`, `run(name, context)`,
  `on(event, context)` and `on-events(events, now)`, each answering a list of actions. Plugins are built with
  wit-bindgen as plain core modules, and tarae
  implements just the part of the component model's canonical ABI that the API uses (`cabi.rs`, about 260 lines).
  Against JSON for the same plugin, measured: a 24 KB plugin instead of 94 KB, 0.4 ms to load instead of 1.5 ms, and
  calls 3–8× faster (8 ms instead of 62 ms with an 8 MB buffer).
- **Thread**: plugins run on one thread of their own, one call at a time. It starts the first time the plugins
  folder has something in it. The config watcher looks at the folder, so no plugins means no thread.
- **What's handed over**: the editor clones its rope (O(1)) and copies the selections' positions. The strings are
  made on the plugin thread, and the buffer's text only for commands registered with `text: true`. On an 8 MB
  buffer, asking a plugin costs the editor under 2 ms, and key → frame stays at 0.4 ms while a plugin loops for ever
  (both are tests).
- **Budget**: about a second of typical code per call, measured on each engine. A floating-point division chain is
  the slowest per unit of fuel, at under 3 seconds. After a failure the next call gets a fresh instance.
- **Answers**: actions apply in order on the main loop. The edits among them are one transaction and one undo
  step, on the buffer and selections the command was given, refused if the buffer changed while the plugin ran.
  `status-bar` puts a short text in the status line, per plugin and buffer.
- **Events** (`opened`, `saved`, `closed`, `changed`, `before-save`) only queue work for the plugin thread, and
  nothing is queued with no subscriber. Subscribers are told one after another, each seeing the buffer as the one
  before left it. `changed` waits for input to stop (300 ms). There's one per buffer at a time, and none for a
  plugin's own edit, so no plugin and edit call each other for ever. `before-save` holds `:w` until the answers are
  in, up to 2 s (auto-save doesn't wait), and drops late ones.
- **Beyond the buffer** (`io.rs`, `perm.rs`, `panel.rs`): still no imports — a plugin asks with actions (spawn,
  connect/send, read-file, read-env, timer, stop, panel, prompt, show/append) and what comes back arrives at
  `on-events`, with the time and fresh randomness (a plugin has neither). Only what `permissions()` declared and you
  approved (kept in the state folder); a new host:port, or a file outside its patterns, is asked about once. Paths
  are checked as written and resolved. Programs run in process groups of their own, with a plain environment, found on
  `PATH`'s absolute folders; connections are read and written on threads of their own; timers share one clock
  thread; a plugin has at most 16 programs, 16 connections and 256 timers. Everything a plugin started ends with it.
  What comes back while it's busy waits and goes in one call; past 4 MB waiting, its readers pause (backpressure,
  not memory). Each load has a generation, so nothing meant for one reaches the next.
- **Cards that ask** (a permission, a file, a host) answer `y` only after half a second in sight, and only while
  they're still the card that was drawn — a key typed for the code can't approve them.
- **Nothing a plugin answers is trusted**: pointers, lengths and UTF-8 are checked, and reading an answer is capped
  at 64 MB (entries can alias the same memory). `run` reaches movement commands only, so a plugin has no way to
  `:sh` or `:w`. Text is shown without control characters, so there are no terminal escapes.
- **Checked by**: a demo plugin ([`plugin-sdk/demo`](../plugin-sdk/demo)) with a command for every action, plus some
  that misbehave on purpose. Its build is committed, so the tests need no wasm toolchain.

Still open:
- `tarae plugin install` from a URL or a registry
- guard pages instead of explicit bounds checks: not planned. Removing the checks outright makes memory-heavy code
  at most about 18% faster, and the cheaper AArch64 checks already get most of that. Guard pages would mean a
  signal handler in the editor's process
- a measurement on a native x86-64 machine: so far only Rosetta and qemu

## Source layout

```
src/
  main.rs            entry point, `tarae grammar` and `tarae plugin` subcommands
  editor.rs          editor state, key dispatch, key-sequence tests
  term.rs            rendering and the event loop — the only module that knows about the terminal
  term_snapshots.rs  screen snapshot tests (golden files in snapshots/)
  screen.rs          what the terminal shows, cell by cell — a frame writes only what changed
  event.rs           event queue + Jobs (slow work on threads, results back on the main loop)

  selection.rs       selection model (byte ranges)
  transaction.rs     multi-selection edits + position mapping — every edit goes through here
  document.rs        rope + selection + path + snapshot undo
  movement.rs        pure movement functions, visual column
  graphemes.rs       grapheme cluster boundaries, cell width
  textobject.rs      text objects, matching brackets, surround
  indent.rs          new lines' indentation (indents.scm), the file's indent unit
  search.rs          search and regex selection
  split.rs           split-window tree
  wrap.rs            soft wrap
  viewalign.rs       view moves (`z` mode)
  structure.rs       moving by syntax (A-o, ]f …)
  labels.rs          gw jump labels
  pairs.rs           auto-pairs
  repeat.rs          `.` — repeat the last insert
  jumplist.rs        jump list (C-o, C-i, C-s)
  shell.rs           selections through shell commands (| ! …)

  key.rs keymap.rs   key notation, keymap trie + TOML parser
  default_keys.toml  the default keymap
  commands.rs        command registry (Helix command names)
  typed.rs           `:` commands
  cmdline.rs         `:` completion and the `:` command table

  settings.rs        settings schema — one row per setting, shared by parsing, :set, and :config-show
  config.rs          config layers, watching, :set! writes
  theme.rs themes/   theme loading; built-in meok and hanji

  disk.rs            sync with changes on disk, auto-save
  saver.rs           saving off the main loop, in order; quitting waits for it
  fswatch.rs         waking when files change (inotify, kqueue; a timer elsewhere)
  restore.rs         reopening the files last open in a folder, at their spots
  recent.rs          recent files (start screen)
  undofile.rs        persistent undo
  clipboard.rs       + register = system clipboard
  log.rs             tarae's log (:log-open)
  procs.rs           :procs — the processes tarae started, their cost, its checks on them
  git.rs             branch and changed-line hunks
  gitmenu.rs         space g — changed files, hunk card, reset, stage, blame
  gitpanel.rs        the git tab — status, stage, discard, commit, push/pull, log, branches
  conflict.rs        merge conflicts — found, drawn, resolved
  diffview.rs        :diff and tarae --diff
  session.rs         sessions — a checkout's panes, chat, terminal, tests, tasks, git; one in front
  worktree.rs        making, ending and bringing in a session (a git worktree), the list, keeping them

  syntax.rs          language detection, grammar loading, background parsing, highlights, injections
  languages.rs       the language table — every pack's languages, grammars and servers, read once
  grammar.rs         grammar download and build
  runtime.rs         embedded queries and grammars
  offer.rs           download offer cards
  install.rs         offering a missing language server or debugger, the way its pack says
  doccomment.rs      rendered doc comments
  markdown.rs        Markdown → colored lines (hover, docs, chat)
  picker.rs          fuzzy pickers, file listing, global search
  filetree.rs        the folder tree every list of files is shown as
  sidebar.rs         the side panel — files, outline, the list, git
  quickfix.rs        the list ]q walks (quickfix)
  task.rs            build tasks — found in the project, run, their places into the list
  terminal.rs        the built-in terminal (a pty, vt100)
  replace.rs         replace across files
  tutor.md, tutor.rs the :tutor text, drawn as text (its marks taken out, kept as colours)
  palette.rs         the palette's order with nothing typed: what you ran, then a first hour's

  lsp.rs             LSP client (threads, framing, position encoding)
  lsp_editor.rs      editor ↔ LSP (didChange, requests, responses)
  completion.rs      completion state, filtering, snippets
  signature.rs       signature help
  editdiff.rs        workspace edit previews (code actions, rename)
  virtualdoc.rs      a server's own documents (a pack's `virtual`: jdtls's jdt:// classes)
  java.rs            Java debugging: java-debug's command chain through jdtls (for a plugin, later)

  testing.rs         test discovery and runs
  packs.rs           language packs: tests found and run, and tasks suggested, the way a pack's data says
  test_results.rs    test output parsing
  dap.rs             debugger (DAP) client
  attach.rs          attaching to running programs

  llm.rs             claude -p process, streaming, response parsing, review
  chat.rs            chat panel
  follow.rs          follow mode — the editor goes where Claude reads
  notes.rs           notes pinned to lines (the chat's MCP tool)
  edits.rs           the chat's edits, reviewed before they're written
  codereview.rs      pull-request-style review sent to Claude in one go

  wasm/              the plugin runtime (see Plugins): mod.rs the API · read.rs bytes · module.rs sections ·
                     compile.rs validation → register ops · exec.rs the interpreter · jit/ machine code —
                     mod.rs executable memory and entry (the only `unsafe`), a64/ and x64/ the backends
  plugin/            the plugin host: mod.rs commands and answers in the editor · host.rs the plugin thread ·
                     abi.rs the API's types, loading and calling a plugin · cabi.rs the canonical ABI subset ·
                     perm.rs permissions · io.rs programs, connections, files, timers · panel.rs its panel,
                     line and buffers · events.rs buffer events · cli.rs `tarae plugin new|dev|list`
runtime/
  queries/           tree-sitter queries (from Helix 25.07.1, MPL-2.0)
  packs/             language packs (built in; one in ~/.local/share/tarae/packs/ wins, table by table)
  grammars.tar.gz    C sources of the bundled grammars
tests/
  e2e.rs             end-to-end tests — the real binary in a pseudo-terminal
docs/
  reference/         generated from the code — settings.md, keymap.md
xtask/               dev tasks (cargo xtask — not part of the tarae binary): vendor-grammars, and
                     shot — the real screen as a PNG, drawn from a virtual terminal (docs/screenshots)
plugin-sdk/          wit/tarae.wit the plugin API · template/ what `tarae plugin new` writes · demo/ the plugin
                     the tests load (demo.wasm, its build) · sdk/ tarae-plugin, the Rust helpers ·
                     kubernetes/ the cluster in the code: state beside manifests, pure Rust
fuzz/                dev tools for src/wasm, outside the workspace (own lockfile): the differential fuzzer
                     and the spec test runner
```
