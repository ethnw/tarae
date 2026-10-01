# Contributing to tarae

Thanks for taking a look. This page covers how to build and test tarae and the handful of rules that keep it fast,
correct, and good-looking. For the why behind them, read [docs/architecture.md](docs/architecture.md) first.

## License and the CLA

tarae is licensed under GPL-3.0-or-later. Before your first pull request can be merged, you sign the project's
Contributor License Agreement once. The agreement lets the project keep its licensing options open, for example to
offer tarae under other terms as well. The CLA is being set up. Until it's published, please open an issue before
sending a pull request.

## Building and checking

You need Rust 1.90+ and a C compiler (grammars are compiled from C).

```sh
cargo run -- path/to/file                # try it
cargo test                               # unit + key-sequence + screen snapshot + e2e tests, performance budgets
cargo test --test e2e                    # just the end-to-end tests (the real binary in a pseudo-terminal)
cargo live                               # live runs with the real rust-analyzer, cargo and claude (see Tests)
cargo test --features bundled-grammars   # also exercises the bundled-grammar path
cargo clippy --all-targets               # must stay at 0 warnings
cargo fmt                                # rustfmt.toml: max_width = 110
```

Before sending a change, all of `cargo fmt`, `cargo clippy --all-targets`, and `cargo test` must be clean. The test
suite includes performance budgets (a key → frame on a 200k-line file must stay under 16 ms), so a slow change fails
a test, not just a benchmark. On a slow shared machine, `TARAE_PERF_SLACK=3` multiplies the budget (CI does this).

[CI](.github/workflows/ci.yml) runs the same checks when started by hand (Actions → CI → Run workflow) — fmt,
clippy (warnings are errors), and tests on Linux and macOS, with and without bundled grammars — plus a build on the
minimum Rust version from `Cargo.toml`, and downloads and builds a grammar from GitHub (`cargo test -- --ignored`).

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
  take the same path. A whole insert-mode session is one undo step.
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
- Grammars are built from their **upstream C sources** at the commit pinned in [`src/languages.toml`](src/languages.toml)
  (`[[grammar]]` git, rev, subpath) — not from crates.io grammar crates. The default build embeds only the grammars marked
  `core = true` (TOML, Markdown, comment). Debug builds — `cargo run`, `cargo test` — also embed the `dev = true` one,
  Rust: the tests and screen snapshots are written against it. `bundle = true` ones are embedded with
  `--features bundled-grammars`.
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

- **New behavior gets a key-sequence test.** In `editor.rs`, `run("text", "keys")` feeds keys to a fresh editor, and you
  assert on the text and selections:

  ```rust
  assert_eq!(text(&run("foo bar", "wd")), "bar");
  ```

- **No real servers or real Claude in tests.** Language server tests use a fake server (`sh -c "cat > log"`): grade the
  JSON it was sent via the file and inject responses with `on_lsp_message`. LLM tests use a fake `claude` written in
  `sh` — use `read -r`, or backslashes in the JSON disappear.
- Background behavior that would make single-event tests flaky is off under `cfg(test)`: the git gutter
  (`git_auto`), session and recent-file state, grammar download offers, and the waiting animation. The clipboard uses
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

## Design and UI

Design weighs as much as features here — the screen should make someone want to open the editor. Any new screen element
is committed only **after you've looked at it in a screenshot** and polished it: alignment, spacing, color hierarchy,
jitter when state changes, empty and failure states.

```sh
cargo build --release
TARAE_SHOT_XDG=/tmp/empty-config TARAE_BACKGROUND=dark cargo xtask shot out.png 'keys to send'
```

`TARAE_SHOT_XDG` should point at an empty config so your own settings don't leak in, and `TARAE_BACKGROUND=dark|light`
picks the theme (the virtual terminal can't answer the background-color query). Session, recent files, and undo
history go to a fresh temp dir, never yours; `@3` among the keys waits 3 s (a language server indexing). The
other knobs are listed at the top of [`xtask/src/shot.rs`](xtask/src/shot.rs).

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
