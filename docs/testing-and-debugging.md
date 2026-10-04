# Tests and debugging

Run the test under your cursor, see failures on the line that failed, and step through code — for Rust, Go, Python,
and Java (plus C and C++ in the debugger). Which tools each language needs: [Language support](languages.md).

- [Tests](#tests)
- [Debugger](#debugger)
- [Attaching to a running program](#attaching-to-a-running-program)

## Tests

`space x`, then:

| Key | Does |
|---|---|
| `x` | Run the test at the cursor |
| `d` | Debug the test at the cursor |
| `f` | Run this file's tests |
| `l` | Run the last test again (as a debug run if it was one) |
| `o` | The run's whole output in a buffer, at the failure you were looking at (search it, copy from it) |
| `c` | Close the panel (stops a run that's still going) |

The wheel over the panel scrolls what it shows beside the list: the failure's message from its top, or the
output, which keeps following its end until you scroll up (and again once you're back at the end).

| Language | Runs with |
|---|---|
| Rust | `cargo test -- module::path::name --exact` (integration tests with `--test`) |
| Go | `go test -v -run '^TestX$'` (benchmarks with `-bench`) |
| Python | `pytest file::Class::function`, with the project's `.venv` or `$VIRTUAL_ENV` Python |
| Java | Gradle `./gradlew :module:test --tests pkg.Class.method` (nested classes as `Outer$Inner`) or Maven `./mvnw -pl module -Dtest=Class#method test` — JUnit 4/5 and TestNG |

**Finding tests** works on the tree-sitter tree — `#[test]`-style attributes and `mod` paths in Rust, `TestXxx` in Go,
`class Test…`/`def test…` in Python, `@Test`-style annotations in Java. If the cursor isn't inside a test, the enclosing
`mod tests` or test class runs; failing that, the whole file. If the language's grammar isn't installed yet, tarae asks
once — one `y` downloads it and runs the test right away ([screenshot](screenshots/grammar-needed.png)). The same goes
for `F5` and attach.

Modified files are saved before a run. **Results** fill a panel where the debug panel sits: a header like
`FAILED  math::*   ● 1  ▲ 1   26 ms`, each test on the left as `●` passed · `▲` failed · `◦` skipped (shapes, not just
colors), and the selected failure's message and location on the right. The failing line gets a red `▲ message` at its
end; `]x` `[x` jump between failures.

Results are read from libtest output, `go test -v`, `pytest -v --tb=short`, and for Java the Gradle/Maven JUnit XML
reports. While results can't be read yet (still building, a compile error), the full output shows instead.

[Rust](screenshots/test-results-rust.png) · [Go](screenshots/test-results-go.png) ·
[Python](screenshots/test-results-python.png) · [Java](screenshots/test-results-java.png) ·
[run output](screenshots/test-run.png) · [Java build output](screenshots/test-run-java.png)

### Language packs

Tests are found and run through **language packs**: data, not code. A pack is a `pack.toml` and a tree-sitter
query, built in from [`runtime/packs/`](../runtime/packs). One of your own in `~/.local/share/tarae/packs/<language>/`
adds a language tarae doesn't know, or changes one it does — no rebuild. Yours replaces the built-in pack a table at
a time: a `[test]` of your own keeps the built-in `[tasks]`, and the other way round. A query file of yours replaces
the built-in one of the same name.

```toml
# ~/.local/share/tarae/packs/go/pack.toml — run from the module root, with the race detector
[test]
query = "tests.scm"                  # captures each test (@test), group (@group) and their names (@name)
files = ["*_test.go"]                # which files hold tests
cwd = { nearest = ["go.mod"] }       # or "file-dir"; `or = [...]` for a second choice
parse = "go"                         # how the output is read: go · rust · python
run = ["go", "test", "-race", "-run", "^{name}$", "./..."]        # the test at the cursor
run-file = ["go", "test", "-race", "-run", "^({names})$", "./..."] # the file's tests, joined with |
debug = { mode = "test", program = "{cwd}", args = ["-test.run", "^{name}$"] }  # the debugger's launch configuration
```

- **Three scopes.** The test at the cursor (`run`, `debug`, `label`); the group around the cursor when it's in no
  test, like a `mod tests` or a `class Test…` (`run-group`, `debug-group`, `label-group`); and the file (`run-file`,
  `debug-file`, `label-file`).
- **Variables:**
  - `{name}` — the test
  - `{groups}` — the groups around it, joined with `::` (`group-sep` to change)
  - `{path}` — groups and name
  - `{groups/.}`, `{path/.}` — the same, joined with what follows the `/`
  - `{path|re}` — any of them escaped for a regular expression (`-t "^{path|re}$"`)
  - `{first-group}` — the file's first group (its class)
  - `{names}` — the file's tests
  - `{cwd}`, `{file}`, `{filename}`, `{rel}` (the file from `{cwd}`)
- **Helpers.** An argument that is just a list variable becomes its items, and one that ends up empty is left out.
  - `helpers = ["python"]` adds `{python}`, the project's virtualenv Python.
  - `helpers = ["cargo"]` adds `{cargo-target}` (`--lib`, `--bin x`, `--test x`) and `{module}` (the file's module
    path). `{cargo-test-binary}` in a debug template is the test binary, built when debugging starts.
  - `helpers = ["java"]` adds `{java-root}` (the top of a multi-module build), `{java-module-dir}`, `{java-build}`
    (`gradle` or `maven`), `{java-tool}` (the wrapper if there is one), `{java-task}` (`:app:test`), `{java-pl}`
    (`-pl app`) and `{package}`. `cwd` can be a template too: `cwd = "{java-root}"`.
- **Variants.** `variant-by = "{java-build}"` picks `[test.variant.gradle]` or `[test.variant.maven]`, whose
  settings override the rest.
- **JUnit XML.** With `parse = "junit"`, results come from the reports written to `reports` during the run, and
  failures point into `sources` (through a Java stack, or the first `path:line` in the failure).
- **Debugging by attaching.** Instead of a `debug` template, `debug-args` are added to the command so the test
  waits for a debugger, and `debug-attach = "java"` attaches java-debug once the JVM says where.
- **Kinds.** A query pattern can give its tests a kind with `(#set! kind "bench")`. `[test.kind.bench]` then has
  settings of its own, and `file = false` keeps those tests out of a file's run. `@start` makes a test begin
  earlier, at its attributes.

The built-in [Go](../runtime/packs/go), [Rust](../runtime/packs/rust), [Python](../runtime/packs/python) and
[Java](../runtime/packs/java) packs show it all.

#### A pack of your own: vitest

TypeScript has no built-in tests. Two files add vitest (`it`/`test` in `describe`), with results read from its JUnit
report:

```toml
# ~/.local/share/tarae/packs/typescript/pack.toml — vitest
[test]
query = "tests.scm"
files = ["*.test.ts", "*.spec.ts"]
cwd = { nearest = ["package.json"] }
parse = "junit"
reports = ["{cwd}/node_modules/.cache/tarae"]
sources = ["{cwd}"]
group-sep = " > "                    # vitest 5's full name for a test: `math > edge > fails`
run = ["npx", "vitest", "run", "{rel}", "-t", "^{path|re}$", "--reporter=default", "--reporter=junit", "--outputFile.junit=node_modules/.cache/tarae/junit.xml"]
run-group = ["npx", "vitest", "run", "{rel}", "-t", "^{groups|re} > ", "--reporter=default", "--reporter=junit", "--outputFile.junit=node_modules/.cache/tarae/junit.xml"]
run-file = ["npx", "vitest", "run", "{rel}", "--reporter=default", "--reporter=junit", "--outputFile.junit=node_modules/.cache/tarae/junit.xml"]
label = "{path}"                     # as the report names it
```

```scheme
; ~/.local/share/tarae/packs/typescript/tests.scm
((call_expression
   function: [(identifier) @_f (member_expression object: (identifier) @_f)]
   arguments: (arguments . (string (string_fragment) @name))) @test
 (#any-of? @_f "it" "test"))

((call_expression
   function: [(identifier) @_f (member_expression object: (identifier) @_f)]
   arguments: (arguments . (string (string_fragment) @name))) @group
 (#eq? @_f "describe"))
```

`-t` takes a regular expression; `|re` escapes the name for one, so `adds (1+1)?` runs just that test. Failures
land on their line: vitest's report has it (`❯ src/math.test.ts:10:21`). For `.js` files, the same two files go in
`packs/javascript/`.

### Installing from packs

A pack also says how to get what its language needs, for tarae to [offer](languages.md#when-one-is-missing) when
it's missing:

```toml
# ~/.local/share/tarae/packs/go/pack.toml — gopls your way; the built-in dlv recipe stays
[install.gopls]                      # the program, as it's run
what = "Go's language server"
ways = [                             # the first whose program (or `needs`) is on PATH
    { run = ["go", "install", "golang.org/x/tools/gopls@latest"], bin = ["~/go/bin"] },
    { run = ["brew", "install", "gopls"] },
]
# hint = "…"                         # what to say when no way fits
```

`bin` is where it lands when that isn't on `PATH` (`~` and `${VAR}` work — a list like `GOPATH` gives its first);
tarae looks for the tool there too. A recipe of your own replaces the built-in one for that tool only.

### Tasks from packs

The build tasks tarae suggests (`space m p`) come from the packs too: `[tasks]` says which files make a project of
that kind, and what to run.

```toml
# ~/.local/share/tarae/packs/rust/pack.toml — replaces the built-in Rust tasks
[tasks]
when = ["Cargo.toml"]                # the project's root has one of these
rank = 10                            # where they go in the list, lower first (50 unless set)

[[tasks.task]]
name = "cargo check"
cmd = "cargo check --all-targets --message-format=short"   # run by sh -c

[[tasks.task]]
name = "nextest"
cmd = "cargo nextest run"
needs = "cargo-nextest"              # only when it's on PATH
```

`each = { json = "package.json", keys = "scripts" }` makes a task for each script, as `{key}` in `name` and `cmd`
(quoted for the shell there; the built-in [JavaScript pack](../runtime/packs/javascript) does). Tasks of your own in
the config come first ([Build tasks](configuration.md#build-tasks)).

`:preflight`'s default steps — for a project with none of its own ([Checks before a
push](configuration.md#checks-before-a-push-preflight)) — come from `[[preflight]]`, one entry per kind of project
(Java has two: Maven, Gradle):

```toml
# ~/.local/share/tarae/packs/go/pack.toml — replaces the built-in Go preflight defaults
[[preflight]]
when = ["go.mod"]
rank = 20                            # several kinds of project: their steps together, lower rank first
steps = [
  { name = "go vet", cmd = "go vet ./..." },
  { name = "go test", cmd = "go test ./..." },
]
```

## Debugger

DAP for Rust, C, and C++ (lldb-dap — Rust is built with `cargo build` first), Go (dlv), Python (debugpy), and Java
(java-debug inside jdtls — the current file's `main`, otherwise the project's first; Maven, Gradle, or a folder with no
build file). Everything lives under `space G`.

| Key | Does |
|---|---|
| `F9` / `space G b` | Toggle breakpoint (a red `●` before the line number) |
| `F5` / `space G l` | Start, or continue |
| `F10` / `space G n` | Step over |
| `F11` / `space G i` | Step in |
| `F12` / `space G o` | Step out |
| `space G p` | Pause |
| `space G t` | Stop (or detach, when attached) |
| `space G C-c` | Conditional breakpoint — `break when: step == 3` |
| `space G C-l` | Logpoint — prints `a is {a}` to the output without stopping |
| `space G w` / `space G W` | Add / remove a watch expression (`:watch expr`, `:unwatch`) |

When paused, the line gets a `▶` and a band, **the values of the variables used up to that line appear at its end**
(`a = 2  b = 3`), and the debug panel below shows the state (RUNNING · PAUSED · EXITED), variables with types, the call
stack (your frames crisp, library frames dimmed), and program output (stderr in red).
[Rust](screenshots/ux-debugger.png) · [Python](screenshots/ux-debugger-python.png) ·
[Go](screenshots/ux-debugger-go.png) · [Java](screenshots/ux-debugger-java.png)

A breakpoint with a condition turns orange, a logpoint turns accent, and the condition shows dimmed at the end of the
line. If the adapter rejects it (a bad expression), the reason appears right there in red. Watch expressions are
evaluated in the current frame on every pause and listed under WATCH at the top of the panel
([screenshot](screenshots/ux-debugger-conditions.png)).

- **macOS**: lldb needs a one-time `sudo DevToolsSecurity -enable` (or approving the password prompt). If it can't start
  within 8 seconds, tarae tells you
- **Go**: dlv must be newer than the installed Go. If it's out of date, the notice shows dlv's own explanation
  (`go install github.com/go-delve/delve/cmd/dlv@latest`)
- **Java**: the java-debug extension is offered for download the first time — see [Java](features.md#java)

## Attaching to a running program

`space G a` opens a picker of targets, or `:attach name` / `:attach host:port`:

| Language | Start the program with | tarae connects via |
|---|---|---|
| Python | `python -m debugpy --listen 5678 app.py` | TCP, straight to debugpy |
| Go | `dlv … --headless --listen :2345 --accept-multiclient` | TCP, straight to dlv |
| Java | `-agentlib:jdwp=transport=dt_socket,server=y,address=*:5005` | JDWP, through java-debug in jdtls |
| Rust / C | `gdbserver`, `lldb-server gdbserver`, or `debugserver` — or a local process by `pid` | lldb-dap |

In an attached debug run `space G t` **detaches** and leaves the program running. Give the targets you use often a name in
your config (a project `.tarae.toml` works well for this):

```toml
[[attach]]
name = "api (k8s)"
lang = "java"                                                # defaults to the current file's language
port = 5005                                                  # host defaults to 127.0.0.1 · for a local process, pid = …
before = "kubectl -n app port-forward deploy/api 5005:5005"  # started first, stopped with the debug run
remote-root = "/app"                                         # source root in the container ↔ this project
# program = "target/debug/app"                               # Rust/C: the executable to read symbols from
```

`before` runs a helper first (a port-forward, an ssh tunnel); tarae attaches once it prints a line or after 2 seconds,
and stops it when the debug run ends. `remote-root` maps breakpoint paths when the program was built somewhere else —
without it, breakpoints set in your copy won't match the paths the remote recorded.

[Python](screenshots/attach-python.png) · [Go](screenshots/attach-go.png) ·
[Java](screenshots/attach-java.png) · [Rust](screenshots/attach-rust.png)
