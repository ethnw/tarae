# Contributing to tarae

Thanks for taking a look. This page covers how to build and test tarae and the handful of rules that keep it fast,
correct, and good-looking. For the why behind them, read [docs/architecture.md](docs/architecture.md) first.

## License and the CLA

tarae is licensed under GPL-3.0-or-later. Before your first pull request can be merged, you sign the project's
Contributor License Agreement once. The agreement lets the project keep its licensing options open, for example to
offer tarae under other terms as well. The CLA is being set up. Until it's published, please open an issue before
sending a pull request.

## Building and checking

You need [rustup](https://rustup.rs) and a C compiler (grammars are compiled from C). The Rust version is pinned in
[`rust-toolchain.toml`](rust-toolchain.toml): in this folder rustup uses that one (and installs it, with its
components and the wasm target the plugin SDK builds for, the first time).

```sh
cargo run -- path/to/file                # try it
cargo test                               # unit + key-sequence + screen snapshot + e2e tests (opt-level 0: quick to build)
cargo perf                               # performance: budgets + real-use runs, numbers kept and compared
cargo test --test e2e                    # just the end-to-end tests (the real binary in a pseudo-terminal)
cargo live                               # live runs with the real rust-analyzer, cargo and claude (see Tests)
cargo test --features bundled-grammars   # also exercises the bundled-grammar path
cargo clippy --workspace --all-targets   # must stay at 0 warnings (xtask, netlink too)
cargo fmt                                # rustfmt.toml: max_width = 110
```

Before sending a change, these checks must all pass — `:preflight-full` runs them in tarae (`.tarae.toml`'s
`[preflight]`), and CI runs the same ones. `:preflight` alone — and bringing a session in — runs the quick
set: fmt, clippy and test (the bundled-grammars builds and perf are marked `full = true`):

<!-- the checks: the same as .tarae.toml's [preflight] and CI's jobs — a test keeps them so (preflight.rs) -->
```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --all-targets --features bundled-grammars -- -D warnings
cargo test --workspace
cargo test --workspace --features bundled-grammars
cargo perf
```

Checks of your own go in `.tarae.local.toml` at the root — its `[preflight] steps` run after these, for you only
(`cargo xtask linux` is a good one: the Linux checks in Docker; with `note = "skipped"` its run without Docker
reads as a pass with a note, and with `full = true` it waits for `:preflight-full`). The repository's `.gitignore` lists it: tarae reads that file only while git
ignores it ([docs](docs/configuration.md#where-it-lives)).

Each checkout's Linux checks build in a Docker volume of its own (5–7 GB), labelled with this machine's id (made
once in tarae's data folder, `machine-id`, and copied to the repository's git folder, `tarae-machine-id`). A run
drops this machine's volumes whose checkout is gone — also when the whole repository was deleted — and never
another machine's (a Docker shared through `DOCKER_HOST`). A run of yours always labels the volume it makes; one
of Claude's, in tarae's sandbox, reads only the git folder's copy, so it labels once a run of yours (or `prune`)
has made that copy, and before then says it labels none. A volume without the id — made so, or made before ids —
is judged by its checkout's path alone, as this machine's, and is never relabelled — only with `prune --mine`:
nothing a Docker says tells it's this machine's alone (a socket may be forwarded from another machine, a host's
name collides), so otherwise only volumes carrying this machine's id are dropped. `cargo xtask linux prune` says
how many it judged by path, or kept. Past
30 GB a run says what tarae's volumes take (or that Docker couldn't measure them), the
checkouts' targets apart from the shared ones (registry, toolchains), which only `cargo xtask linux prune --all`
frees: it lists every volume and image of this repository's on this machine — gone checkouts' targets,
toolchains, the registry, trace's, the `tarae-trace` and `tarae-redirect` images (not `rust:<version>`, a pulled
one; volumes without the machine id and the images only with `--mine`) — with their sizes, and drops them only
when run again with `--yes`; the next run builds them again. A live checkout's target
stays, said why — its folder is there, or the tarae it's run from has its session open (asked of its UI-state
socket, `TARAE_UI`, set for tarae's terminal and your tasks; run elsewhere, or by Claude, the folders alone
say). To drop a
session's volume as the session ends, put `[session] ended = ["cargo", "xtask", "linux", "prune"]` in that
`.tarae.local.toml`.

A test that fails only now and then on Linux: `cargo xtask linux test --repeat 20 --bin tarae recovery::` runs
`cargo test` with those arguments twenty times in one container and stops at the first run that fails. Flakes
that need the rest of the suite beside them show up with `--bin tarae` alone. A failure seen on one architecture
only: `cargo xtask linux --platform linux/amd64 …` runs on that one (its own target and toolchains) — emulated on
an aarch64 VM, slow and liable to stall; a Docker on a real x86_64 machine (`DOCKER_HOST`) is better.

`cargo perf` runs the performance budgets (a key → frame on a 200k-line file must stay under 16 ms) and
real-use runs of the built editor on a pseudo-terminal (startup, a key in a big file, scrolling, idle CPU, memory
with this repository open), in the `perf` profile — release, with line tables. It runs them three times, keeps
each number's median in `target/perf/history.tsv` with the commit and date, and says which are more than 15% slower
than the median of their last runs. On a busy machine (load 5 or more) it measures nothing and says so. A slow
change thus fails a budget, and a slower one shows up as a number. `cargo test` builds at opt-level 0 for a quick
edit-and-test loop, where these skip themselves, saying so. On a slow shared machine, `TARAE_PERF_SLACK=3`
multiplies the budget (CI does this).

[CI](.github/workflows/ci.yml) runs the same checks when started by hand (Actions → CI → Run workflow) — fmt,
clippy (warnings are errors), and tests on Linux and macOS, with and without bundled grammars — except `cargo perf`:
shared runners are too noisy for numbers, so CI runs only the budgets, in the `perf` profile. It also downloads and
builds a grammar from GitHub (`cargo test -- --ignored`: the tests that need the network — but not the one that needs claude installed,
`installed_claude_speaks_a_revision_we_speak`, a local check). It builds with the pinned toolchain too
(`rustup toolchain install` reads `rust-toolchain.toml`), so a check that passes here passes there.

### The toolchain

tarae is built with exactly one Rust: the latest stable, pinned in `rust-toolchain.toml` (`rust-version` in
`Cargo.toml` says the same, as a floor for `cargo install`). There's no older version to keep building on. To move
to a new stable release, as a change of its own:

1. Set `channel` in `rust-toolchain.toml` to the new version (`1.100.0`), and `rust-version` in `Cargo.toml` to its
   `major.minor` (`1.100`).
2. Run every check on it — `:preflight-full` in tarae, or `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D
   warnings` (and with `--features bundled-grammars`), `cargo test`, `cargo perf`. A new release brings new lints
   and deprecations: fix them in the same change (a deprecated call gets its replacement, not an `allow`).
   A new rustfmt may reformat — `cargo fmt` and commit that too.
3. Linux, where CI runs too: the same checks in a container on the matching `rust:<major.minor>` image.
4. `cargo perf`'s numbers change with the compiler: the first run after a bump is the new baseline, worth a look
   against the old one.

Code comments are in English.

### Generated docs

[`docs/reference/settings.md`](docs/reference/settings.md) and [`docs/reference/keymap.md`](docs/reference/keymap.md)
are generated from the code, and tests fail when they fall out of date. After changing a setting, a default key, a
command's doc string, or a `:` command, regenerate them:

```sh
TARAE_BLESS=1 cargo test settings_reference
TARAE_BLESS=1 cargo test keymap_reference
```

The example in [`docs/configuration.md`](docs/configuration.md) (and the TOML blocks in the other docs) is parsed by a
test too — keep it valid.

## Developing tarae in tarae

tarae is written in tarae, and the repository is set up for it. [`.tarae.toml`](.tarae.toml) at the root tells
`:restart` how to get the next tarae: build it with `cargo build --release`, then run `target/release/tarae`.
Start a release build from the repository and work in it:

```sh
cargo build --release && target/release/tarae
```

**The loop.** Change the code, then `:restart`. Files are saved, the build runs in the task panel below the code
(its errors go into the list, so `]q` walks them), and when it passes, tarae checks the new binary answers
`--version` and becomes it, in the same terminal, with your files, cursors, panes, side panel, and chat
conversation as they were ([what comes along](docs/features.md#updating-without-losing-your-place)). A failed
build leaves you in the running tarae. `:restart` again while it builds cancels, and so does stopping the build
(`space m c`). `:restart!` skips the build, for when you've just built by hand.

**Claude checks its own work.** In the chat (`space l`), Claude runs `cargo build`, `check`, `test`, `clippy`, and
`fmt`, and git's read-only commands, with its `run` tool, without asking each time; anything else asks first
([running commands](docs/claude-integration.md#running-commands)). After a change it runs the tests that cover it
with `run_tests`, at a test file or a single test, in the test pane where you can watch
([running the tests](docs/claude-integration.md#running-the-tests)). A `:restart` typed while Claude is working
waits for the turn to end.

**Review before committing.** `space v c` has Claude review the uncommitted changes (`:review-claude main` for
everything since `main`): its findings are notes on the lines, its verdict is in the chat. Answer a note with
`space n`, and `space v s` sends your answers back together; `space v q` ends the review
([code review](docs/claude-integration.md#code-review)).

**Commit.** `space g` opens the git tab. Stage with `s`, then `C` commits with a message Claude drafts from the
staged diff in the style of the recent log. It lands as a review in the message buffer: take it (`a`), edit it,
and `:w` commits (`:q` cancels). `c` is the plain commit, written by you
([commit messages](docs/claude-integration.md#commit-messages)).

## Engineering rules

### Never block the main thread

Non-blocking is the reason tarae exists. Nothing on the keystroke or screen-update path may wait on the network, a
process, or the disk.

- Slow work goes to `editor.events.jobs().spawn(job)`: the job runs on a worker thread and returns a closure that is
  applied on the main loop. Code that waits on the main thread won't be merged.
- Results that come back late (the document or cursor moved on) are dropped, not applied.
- Language servers: the main thread only pushes onto channels; each server has its own writer and reader thread.
  Flush pending changes before sending a request (`lsp_request` does), always answer the server's own requests, and
  check responses against what was pending.

### Edits and positions

- **Every edit goes through a `Transaction`** — that keeps multiple selections consistent, and LLM diffs and agent edits
  take the same path. A whole stay in insert mode is one undo step.
- **Positions are byte offsets, always on character boundaries.** tree-sitter, regex, and LSP (UTF-8 negotiated) all use
  bytes, so there's no conversion layer; only ropey's editing API uses chars, and the conversion happens in one place,
  `Transaction::apply`.
- **"One character" is a grapheme cluster** ([`src/graphemes.rs`](src/graphemes.rs)). Single steps, "the character under
  the cursor", and selection ends use `next_boundary`/`prev_boundary`; only places that look at character classes (word
  motions, character find) use `next_char`/`prev_char`. **Never write `pos ± 1`** — ropey panics in the middle of a
  character. `stress_positions_stay_on_char_boundaries` in `editor.rs` throws random keys at Hangul, emoji, and CRLF
  text as a safety net.
- Columns shown to people (the status line) are converted to character counts.

### Commands, keys, and settings

- **Command names match Helix 1:1.** Watch the confusing pair: `extend_line_up`/`extend_line_down` are *extending
  movements* up and down, while `extend_line_above`/`extend_line_below` are *linewise selections*. A binding like
  `X = ["extend_line_up", "extend_to_line_bounds"]` depends on that difference (see the test
  `x_binding_selects_lines_upward`).
- **The default keymap lives in [`src/default_keys.toml`](src/default_keys.toml)** — never hardcode keys in code. It
  goes through the same parser as user config.
- Keep command doc strings **short and in plain words**: they are what which-key, the command palette, and the keymap
  reference show, and a which-key column holds about 30 characters.
- **A new `:` command** gets a line in `cmdline::COMMANDS` (names, aliases, argument kind, description) as well as its
  handler in `typed.rs`. The test `every_typed_command_is_listed` catches a missing one.
- **A new setting is one row in the `SETTINGS` table in [`src/settings.rs`](src/settings.rs)** (plus a field on
  `EditorConfig`). File parsing, `:set`, `:toggle`, completion, `:config-show`, and the settings reference all follow
  from that row. Configuration is data — never add code execution to config files.

### The core doesn't know the terminal

- Everything terminal-specific lives in [`src/term.rs`](src/term.rs). The editor core is driven by key sequences in tests
  and never touches the terminal.
- **Screen rows are not document lines.** Doc comment blocks fold into a single row, so drawing and mouse handling go
  through the view (`editor.view`), not raw line numbers. Per-line tables while drawing are indexed by document line,
  not by screen row.

### Syntax and language features

- tarae never reads a Helix installation — not its config, themes, or grammars.
- Grammars are built from their **upstream C sources** at the commit pinned in their language's pack,
  [`runtime/packs/<lang>/pack.toml`](runtime/packs) (`[grammar.<name>]` git, rev, subpath) — not from crates.io grammar
  crates. The default build embeds only the grammars marked
  `core = true` (TOML, Markdown, comment). Debug builds — `cargo run`, `cargo test` — also embed the `dev = true` one,
  Rust: the tests and screen snapshots are written against it. Other profiles embed it with `--features dev-grammars`
  (`cargo perf` does, for its syntax budgets) — not because they have debug info: release builds keep line tables
  in a dSYM. `bundle = true` ones are embedded with `--features bundled-grammars`.
- After changing the bundled list or a grammar commit, repack with `cargo xtask vendor-grammars`. Bumping a grammar can
  break its queries; `every_installed_grammar_loads_with_its_queries` catches that.
- Queries live in [`runtime/queries/`](runtime/queries) and are embedded at build time. To try query changes without
  rebuilding, point `TARAE_RUNTIME` at a folder with a `queries/` directory. Later patterns win.
- Language-aware features (test discovery, debugging) work **on the tree-sitter tree**, never by guessing from text. If
  the grammar is missing, offer the download and carry on with what the user was doing once they accept.

### The plugin runtime (`src/wasm`)

- **`unsafe` lives only in `src/wasm/jit/mod.rs`** (executable memory and calling into it). The rest of the runtime is
  `#![deny(unsafe_code)]`. The machine code the JIT writes is as trusted as `unsafe`, though: a missed bounds check in
  `emit.rs` is a hole in the sandbox.
- **The interpreter is the reference.** JIT and interpreter must agree exactly, traps, fuel and stack limits included.
  The interpreter is also the fallback wherever the JIT can't run.
- A change to `compile.rs`, `exec.rs` or `jit/` goes through the spec tests and a fuzz run on each architecture it
  touches. The tools are in [`fuzz/`](fuzz), outside the workspace:

  ```sh
  cd fuzz
  git clone --depth 1 https://github.com/WebAssembly/testsuite
  cargo run --release --bin spec -- testsuite/*.wast   # must pass, JIT and interpreter
  cargo run --release --bin fuzz -- 300                # 5 minutes; again with `-- 300 1 notraps`
  ```

  Several fuzz processes with different start seeds, each in its own directory, use every core. On Apple Silicon,
  x86-64 runs under Rosetta: `cargo test --target x86_64-apple-darwin`, and the same for `fuzz/`.
- New instruction encodings get a line in the backend's `asm.rs` test, checked against `clang -c`.

### The plugin host (`src/plugin`, `plugin-sdk`)

- **The API is [`plugin-sdk/wit/tarae.wit`](plugin-sdk/wit/tarae.wit)**, and `src/plugin/abi.rs` mirrors it, type for
  type. A change that breaks built plugins bumps the package's major version (and `abi::API`); tarae then refuses
  plugins built for the old one, with a message saying to rebuild.
- **wit-bindgen is pinned** to one version in the template, the demo and `abi::WIT_BINDGEN`. A test checks they agree.
- **The tests load [`plugin-sdk/demo.wasm`](plugin-sdk/demo.wasm)**, the demo plugin's build, so they need no wasm
  toolchain. After changing the demo or the WIT, rebuild it. The remap keeps your home folder's path out of the
  committed file:

  ```sh
  cd plugin-sdk/demo
  RUSTFLAGS="--remap-path-prefix=$HOME=~" cargo build --release
  cp target/wasm32-unknown-unknown/release/demo.wasm ..
  ```
- **Nothing a plugin answers is trusted**: pointers, lengths, UTF-8, positions and overlaps are all checked, and a
  bad answer is an error message, never a panic.

## Tests

- **Tests are the direction.** A test tells the next reader what the code must do and why. That means:
  - Its name states the property in plain words (`a_card_answers_only_once_drawn`, not `test_offer_3`).
  - A comment says why: which decision or incident it pins.
  - It has been seen failing once, with that property broken by hand, and then passing again.
  - It has one reason to fail.
  - Regression tests, perf budgets, goldens and fuzz targets also say what they pin.
  - Tests that mirror the implementation (restating the code step by step) aren't wanted, and neither are
    snapshots with no stated intent.
  - When a decision changes, its tests change in the same commit.

  The second reviewer (`src/reviewer.rs`) asks what each new test explains.
- **New behavior gets a key-sequence test.** In `editor.rs`, `run("text", "keys")` feeds keys to a fresh editor, and you
  assert on the text and selections:

  ```rust
  assert_eq!(text(&run("foo bar", "wd")), "bar");
  ```

- **No real servers or real Claude in tests.** Language server tests use a fake server (`sh -c "cat > log"`): grade the
  JSON it was sent via the file and inject responses with `on_lsp_message`. LLM tests use a fake `claude` written in
  `sh` — use `read -r`, or backslashes in the JSON disappear.
- **Dangerous behavior is tested with harmless markers.** A test of deleting, overwriting, killing processes, or
  reaching the network makes a file in its own temp dir and checks it. Its contents never hold the real thing
  (`rm -rf /`, a path outside the temp dir), not even in a branch meant never to run: a broken build or a wrong
  guard runs it. This happened on 2026-10-01, and macOS refused `/`.
- **Claude's commands run in a sandbox** ([`src/sandbox.rs`](src/sandbox.rs)), and none can start inside
  another. Under a sandboxed `cargo test` (`TARAE_SANDBOXED` is set), the real Seatbelt tests skip themselves
  through `sandbox::skipped_inside`, which writes `tarae-test: skipped <name> — <why>` past libtest's capture.
  libtest still prints `ok`, but tarae's test pane and Claude's results count them as skipped, not passed.
  They run in full in your own `cargo test` and in CI. Under `cfg(test)` Claude's commands skip the sandbox
  (`Availability::Bypass`) unless a test sets it.
- Background behavior that would make single-event tests flaky is off under `cfg(test)`: the git gutter
  (`git_auto`), restore and recent-file state, grammar download offers, and the waiting animation. The clipboard uses
  memory, so your real clipboard is never touched.
- Tests that need a grammar the default build doesn't include **skip silently** when it's missing (see
  `highlights_rust_when_grammar_available`); run `cargo test --features bundled-grammars` to cover them.
- **Screen snapshots** (`src/term_snapshots.rs`) render a frame with `term::render`, no terminal needed, and compare it
  with a golden file in `src/snapshots/`: the text grid plus two compact layers marking backgrounds and text colors by
  theme key (the format is described at the top of the file). A change to what's on screen shows up as a readable diff.
  When the change is intended, look at the new output, then regenerate and commit it:

  ```sh
  TARAE_BLESS=1 cargo test snapshot_
  ```

  Keep snapshots deterministic — no clock-dependent content, absolute paths, or grammars outside the built-in ones.
- **End-to-end tests** (`tests/e2e.rs`) start the real `tarae` binary in a pseudo-terminal, type keys, read the screen
  through a terminal emulator (`vt100`), and check files on disk — startup, save, quit and terminal restore, resize,
  config errors, graphemes. Each test runs in its own sandbox (`HOME`, XDG dirs, and the Claude Code lock directory all
  point into a temp dir). Never sleep: send a key, then `wait_for` its visible effect; on timeout the test prints the
  whole screen.
- **Live runs** (`tests/live.rs`, `cargo live`) use the same harness with the real tools: rust-analyzer (diagnostics,
  completion, go to definition), `cargo test` at the cursor, and a logged-in `claude` (`space i`, a chat edit accepted
  in the buffer). tarae's config and state still live in the temp dir; PATH and HOME are yours. They aren't part of
  `cargo test` — they take a while and the Claude ones use your plan's usage — so run them before a release and after
  touching the language server, test, or Claude paths. A run whose tool isn't installed says so and passes.
- **Measuring on Linux with eBPF** (`trace/`, its own workspace — `--workspace` never builds it): `tarae-trace`
  attaches to a tarae and measures it without changing it. Its probes:
  - `offcpu`: where its threads wait, by stack;
  - `syscalls`: its syscall mix, with `--stacks mprotect,…` for where a syscall is called from;
  - `pty`: its writes to the terminal, start to end.

  It runs privileged (root, or `CAP_BPF` + `CAP_PERFMON`; `CAP_SYSLOG` to name kernel frames) and needs clang,
  bpftool and libbpf/libelf headers to build. On a Linux machine:
  `sudo tarae-trace offcpu --pid <tarae's pid> [--children] [--for 1m]` — it ends at Ctrl-C, `--for` (10 min
  if not given) or the target's exit, and writes `<probe>-<name>.json` and folded stacks.

  For good stacks, build tarae with
  `RUSTFLAGS='-C force-frame-pointers=yes -C split-debuginfo=off' cargo build --profile perf --features
  dev-grammars` (a RUSTFLAGS replaces `.cargo/config.toml`'s, which keeps the debug info out of `.dwo` files the
  tool doesn't read).

  User stacks are unwound with the binaries' `.eh_frame`, so code without frame pointers keeps its frames —
  on x86_64, Rust's std and Debian's glibc. The first time a stack is seen, its registers and 32 KB of it go
  to the tool through a ring buffer, and it's unwound at the end. Above those 32 KB, the frame-pointer walk's
  frames carry on.

  Reproducibly, in one Docker container on the Docker your CLI reaches (colima here), with tarae built and driven
  there by `tests/perf.rs`'s scenarios:
  - `cargo xtask trace [offcpu|syscalls [--stacks …]|pty] [scenario …]` — results in `target/trace/<time>/`;
  - `cargo xtask trace test` — the tool's tests against fixtures;
  - `cargo xtask trace clippy`.

## Design and UI

Design weighs as much as features here — the screen should make someone want to open the editor. Any new screen element
is committed only **after you've looked at it in a screenshot** and polished it: alignment, spacing, color hierarchy,
jitter when state changes, empty and failure states.

```sh
TARAE_SHOT_XDG=/tmp/empty-config TARAE_BACKGROUND=dark cargo xtask shot out.png 'keys to send'
```

The shot builds tarae's `shot` profile first — release's behavior, built incrementally without LTO, so a shot after
an edit takes seconds rather than a release build's minute. `TARAE_SHOT_XDG` should point at an empty config so
your own settings don't leak in, and `TARAE_BACKGROUND=dark|light` picks the theme (the virtual terminal can't
answer the background-color query). Files to restore, recent files, and undo history go to a fresh temp dir, never yours;
`@3` among the keys waits 3 s, `@?ready` until `ready` is on screen (a language server indexing). The other knobs
are listed at the top of [`xtask/src/shot.rs`](xtask/src/shot.rs).

The design language:

- **One accent color** (`ui.accent`; if a theme lacks it, the primary cursor background is used)
- **Floating things are cards** (`card_style`, `card_padding`, `card_edge` — half-cell blocks for half-line padding);
  themes without a background color get a border instead. Floating things use the same syntax colors as code
- Selected row = a `▎` accent bar + `ui.menu.selected`; typed characters = accent bold
- **Only two dim levels**: `ui.virtual` (dim) and `ui.linenr` (faint)
- **Glyphs** only from what JetBrains Mono, Hack, Menlo, and SF Mono all have: `ƒ τ ν π § ◦ # ¶ ● ▲ › ▎ ▐ ▌ ▄ ▀`
- When space runs out, fold the least important information first
- The built-in themes meok and hanji ([`src/themes/`](src/themes)) have **the same set of keys** — change one, change
  the other
- Status messages are toasts: `set_status`, `set_success`, `set_warning`, `set_error`; `note` records without a toast
  (for things already visible on screen)
