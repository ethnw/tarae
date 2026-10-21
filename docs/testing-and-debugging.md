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
query, built in from [`runtime/packs/`](../runtime/packs) — the same folder that says what the language is
([Adding a language](languages.md#adding-a-language)). One of your own in `~/.local/share/tarae/packs/<language>/`
adds a language tarae doesn't know, or changes one it does — no rebuild. Yours replaces the built-in pack a table at
a time: a `[test]` of your own keeps the built-in `[tasks]`, and the other way round ([what a pack of yours can
change](#what-a-pack-of-yours-can-change)). A query file of yours replaces the built-in one of the same name.

```toml
# ~/.local/share/tarae/packs/go/pack.toml — run from the module root, with the race detector
[test]
query = "tests.scm"                  # captures each test (@test), group (@group) and their names (@name)
files = ["*_test.go"]                # which files hold tests
cwd = { nearest = ["go.mod"] }       # or "file-dir"; `or = [...]` for a second choice
run = ["go", "test", "-race", "-v", "-run", "^{name}$", "./..."]        # the test at the cursor
run-file = ["go", "test", "-race", "-v", "-run", "^({names})$", "./..."] # the file's tests, joined with |
debug = { mode = "test", program = "{cwd}", args = ["-test.run", "^{name}$"] }  # the debugger's launch configuration
none-ran = [0]                       # how a run that matched no test ends

# How the output is read, a line at a time: the first rule that matches acts
[[test.line]]
match = '^\s*=== RUN\s+(?<name>\S+)'  # what follows is about this test
begin = true

[[test.line]]
match = '^\s*--- (?<status>PASS|FAIL|SKIP): (?<name>\S+)(?: \((?<secs>[\d.]+)s\))?'
status = { PASS = "passed", FAIL = "failed", SKIP = "skipped" }

[[test.line]]
match = '^    \s*(?<file>[^\s:]+\.go):(?<line>\d+): (?<message>.*)$'  # where it failed, and why
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
  `helpers = ["cargo"]` adds `{cargo-target}` (`--lib`, `--bin x`, `--test x`), `{module}` (the file's module path)
  and `{crate-root}` (the target's root file).
- **Variables of the pack's own** (`vars`, a list, in order — each may use those before it): how a build's layout
  is said. The [Java pack](../runtime/packs/java/pack.toml) works out `{java-root}` (the top of a multi-module
  build), the module, Gradle or Maven, the wrapper or the tool, the module's task and `{package}` with them:
  - `{ name = "root", topmost = ["pom.xml"] }` / `nearest = [...]` — the folder holding one, the topmost below the
    repository or the nearest up (`"roots"`: the language's `roots`); none: `else = "{…}"`, or the error `missing`
  - `{ name = "tool", first = ["{root}/gradlew", "gradle"] }` — the first that's a file, else a program name;
    `~/` and `${VAR}` are expanded, and one naming a variable that isn't set is passed over (Python's `{python}`:
    `["{cwd}/.venv/bin/python", "${VIRTUAL_ENV}/bin/python", "python3"]`)
  - `{ name = "module", parts = "{module-dir}", from = "{root}" }` — the folders between: `{module}` is `a/b`,
    `{module/:}` is `a:b`
  - `{ name = "x", if = "{module}", then = …, else = … }` (not empty), or `if-exists = "{root}/pom.xml"` — each side
    a template or a list (`then = ["-pl", "{module}"]`)
  - The test query's `@var.<name>` captures are variables too (Java's `@var.package-name`: the tree's `package`).
  `cwd` can be a template: `cwd = "{java-root}"`.
- **Variants.** `variant-by = "{java-build}"` picks `[test.variant.gradle]` or `[test.variant.maven]`, whose
  settings override the rest; a variant's own `vars` are worked out once it's picked.
- **Reading the output.** `[[test.line]]` rules read it a line at a time, as it comes; each line tries them in
  order and the first whose `match` matches acts, by its named groups — `name` (the test), `status`, `secs`,
  `file` and `line` (where it failed), `message` (a line of why):
  - `status = { ok = "passed", FAILED = "failed" }` by the `status` group's text (passed · failed · skipped), or
    `status = "skipped"` whatever the line
  - `begin = true`: the lines after are about the test this one names (libtest's `---- a::b stdout ----`); `end =
    true`: about none. A rule with no `name` group acts on that test. `known = true`: only a test already listed
    (by name, or as the end of one's `::` name) — a heading for no test is nobody's; `name-replace = { "." = "::"
    }` turns its name into the one the results use
  - `phase = "failures"`: the rule counts only in that part of the output (`""`: outside every one); `to = "…"`
    starts one (pytest's `FAILURES` block)
  - `place = "last"`: a later place replaces this one until the test has a message (a traceback's last frame);
    otherwise the first place that's a file stays
  - `none-ran = [0, 5]`: a run that listed no test and ended with one of these ran none — said as such, not passed

  The [Rust](../runtime/packs/rust/pack.toml) (libtest), Go and [Python](../runtime/packs/python/pack.toml)
  (pytest) packs read theirs this way. A test is failed from when it's first listed until a line says otherwise;
  one that said it skipped stays skipped when the runner calls it a pass.
- **JUnit XML.** With `parse = "junit"`, results come from the reports written to `reports` during the run, and
  failures point into `sources` (through a Java stack, or the first `path:line` in the failure).
- **Building before debugging.** `{built}` in a debug template is what `debug-build` builds when debugging
  starts — the same table as `[debug] build` ([Debuggers from packs](#debuggers-from-packs)); Rust's builds the test
  binary with `cargo test --no-run` and picks the one whose root file is `{crate-root}`.
- **Debugging by attaching.** Instead of a `debug` template, `debug-args` are added to the command so the test
  waits for a debugger, and `debug-attach = { wait-for = '…(?<port>\d+)', way = "java" }` attaches java-debug once
  a line of the run's output matches `wait-for` — its `port` group the port.
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
tarae looks for the tool there too. A recipe of your own replaces the built-in one for that tool only. A way can
also be a release tarae downloads and checks itself — see [Downloads](languages.md#downloads).

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

`unless` holds an entry back when one of its files is there too, and `held` says why when nothing else runs. The
Java pack's Gradle entry has `unless = ["gradlew"]`: a project with its own wrapper lists it in its own
`[preflight]`.

What runs later outside Claude's sandbox — build scripts, manifests, wrappers — each pack lists in `protect`:
Claude's commands can't write these and its edits of them always ask. Every built-in pack's names apply in every
project, whatever its languages, and a pack of your own can't take one out (`llm.review-ask` adds yours):

```toml
# runtime/packs/rust/pack.toml
protect = ["build.rs", "Cargo.toml", ".cargo"]   # names, anywhere in the project, in any case;
                                                 # "gradle/wrapper": those names in a row
outputs = ["target"]                             # tools' output folders: the file walks skip them
first-made = ["target/", "Cargo.lock"]           # what a first build makes beside those names
```

On Linux without a mount namespace of the run's own (tiers host and offline; in tier own each protected name is
bound read-only over itself instead, and every folder takes new files) a folder holding a protected name takes no
new entry from Claude's commands (a folder there may only go, once emptied — `cargo clean`), so a fresh clone's first
build couldn't make `target/` beside `Cargo.toml`: before a command runs, tarae makes the pack's `first-made`,
empty — a folder (written with a `/`) in every folder holding one of its names, a file (written in place, as
`Cargo.lock` is) in the outermost — then takes away what's still empty when the command is over. A name that
is, or would hold, a protected one is never made. That's all `first-made` does: the walks go by `outputs` alone
(Java's `build/` is made first, but a repository whose `build/` holds sources is still searched).

A run can wait on something that isn't its own work — another cargo holding the build directory. `[[waits]]` names
the lines that say so: such a line is the run's state, not output (the task or test panel shows `waiting for cargo's
lock · 0:37` until the next line), and Claude's report tells the wait from the run (`Waited 16.0s for cargo's build
directory lock. Ran 11.0s.`). Every pack's apply to every run; a language that declares none has none.

```toml
# runtime/packs/rust/pack.toml
[[waits]]
line = '^\s*Blocking waiting for file lock on (?<what>.+)$'   # its named groups go into the texts as {what}
for = "cargo's lock"                  # the panel: waiting for …
report = "cargo's {what} lock"        # Claude's report (`for` unless set)
```

A task's places come the same way. Every tool's `path:line[:col]: [severity] message` is tarae's own; a form one
tool has beyond it is its pack's `[[places]]` — `match` with groups `path`, `line`, `col` (or `place`, a
`path:line:col` text), `severity`, `message`; `then` when the place comes on a later line; `message = "…"` for one
of its own. A rule whose place isn't a file lets the next try. `checks` names the commands whose places a run of
Claude's puts in your list (`]q`); `color-off` what turns a tool's colours off in the runs tarae reads.

```toml
# runtime/packs/rust/pack.toml
checks = ["cargo build", "cargo check", "cargo test", "cargo clippy", "cargo nextest", "cargo fmt", "cargo perf"]
color-off = { CARGO_TERM_COLOR = "never" }    # beside NO_COLOR=1, in every task and test run

# rustc's long form: the message, then where (` --> src/a.rs:3:5`) a few lines on
[[places]]
match = '^(?<severity>error|warning)(?:\[[^\]]*\])?: ?(?<message>.*)$'
then = '^--> (?<place>.+)$'
```

### What a pack of yours can change

Each top-level key of a `pack.toml` says how a pack of your own counts for it:

| Keys | A pack of yours |
|---|---|
| `[test]`, `[tasks]`, `[[preflight]]`, `[[waits]]`, `[[places]]`, `[debug]` | replaces the built-in table |
| `[install.<tool>]`, `[grammar.<name>]`, `[debugger.<name>]` | replaces that entry; the others stay |
| `[language.<name>]`, `[server.<name>]` | replaces the keys it sets; the entry's other keys stay — in whichever folder the language is |
| `config-files`, `checks` | adds to the built-in list; nothing can be taken out |
| `outputs`, `first-made`, `protect`, `color-off`, `[builds]` | ignored, and said at the start: tarae reads them from its own packs only (they decide what Claude's runs may reach or make, or what's in their environment, and — `[builds]`: where a checkout builds — what a session's end may delete) |

A key no pack knows is a mistake in that pack, said with its name as one that doesn't parse is. Your
`[language]`, `[grammar]`, `[server]` and `checks` are read once, when tarae starts — after editing them,
`:restart`. `[test]`, `[tasks]`, `[[preflight]]`, `[[waits]]`, `[[places]]`, `[install]`, `[debug]` and
`[debugger]` are read when they're used, so they count at once.

## Debugger

DAP for Rust (lldb-dap — built with `cargo build` first), C and C++ (lldb-dap, by [attaching](#attaching-to-a-running-program)
to the program your build made), Go (dlv), Python (debugpy), and Java (java-debug inside jdtls — the current file's
`main`, otherwise the project's first; Maven, Gradle, or a folder with no build file). Everything lives under
`space G`. Which debugger a language has and how it starts is [its pack's](#debuggers-from-packs).

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

### Debuggers from packs

A language's debugging is its pack's: `[debug]` says which debugger, how `F5` launches a file and how a running
program is attached to; `[debugger.<name>]` says how that debugger is found and started, in whichever pack has it
(lldb-dap is the [C pack's](../runtime/packs/c), and Rust and C++ name it). A pack of yours replaces `[debug]` whole
and a `[debugger.<name>]` entry by entry.

```toml
# ~/.local/share/tarae/packs/go/pack.toml — F5 runs the module's main package, not the file's
[debug]
debugger = "dlv"                      # a [debugger.<name>]; Java's pack says way = "java" (code, through jdtls)
launch = { mode = "debug", program = "{cwd}", cwd = "{cwd}", outputMode = "remote" }   # the launch request
attach-pid = { mode = "local", processId = "{pid}", cwd = "{cwd}" }                    # :attach by pid
attach-tcp = { mode = "remote", substitutePath = [{ from = "{cwd}", to = "{remote-root}" }] }  # straight to host:port
```

- **`launch`** — what `F5` sends: `{file}`, `{filename}`, `{dir}` (the file's folder), `{cwd}` (the project's root,
  where the debugger runs), `{built}`, and the `vars` the `[debug]` has (as `[test]`'s — the Python pack's
  `{python}`, its virtualenv's). No `launch`: `F5` says to attach.
- **`build`** — built first, on a worker, the debug panel saying so: `run`'s output is read as JSON lines and the last
  one `pick` holds gives `{built}` from its field `take`. `pick` fields are dotted paths (`"target.kind" = "bin"`: a list
  holding it counts; paths compare as real paths). A failed build shows the first line `error.pick` holds, by its first
  `error.text` field there (rustc's own rendering), else the build's last stderr lines. `when` lists files the project
  has one of, else `otherwise` is said; `none` is said when it built but nothing was picked; `env` for the build.
- **Attaching** — `attach-pid` (`{pid}`, a number) and `attach-remote` (`{host}`, `{port}`: lldb's `gdb-remote`)
  start the debugger here to attach; `attach-tcp` connects straight to host:port, where the program's own debug
  server speaks DAP (debugpy, a headless dlv). `{program}` and `{remote-root}` come from `[[attach]]`; a key whose
  value names one that wasn't given is left out.

```toml
# runtime/packs/go/pack.toml
[debugger.dlv]
adapter-id = "go"                     # what initialize calls it (the name unless set)
find = ["dlv"]                        # in order; then where [install.dlv] puts it
run = ["{found}", "dap"]
listen = ["-l", "127.0.0.1:{port}"]   # it talks over TCP, on a port tarae picks (else stdio)
quiet = ["^Type 'dlv help'"]          # its own output lines dropped (not the program's stdout/stderr)
own-first = "^Building "              # a first stdout line that's its own, shown dim
```

`find` takes program names (`"lldb-dap-{22..14}"`: each number, in that order), `{ file = [...] }` (the first that
exists: `{cwd}`, `~`, `${VAR}`), `{ ask = ["xcrun", "-f", "lldb-dap"] }` (the path a command prints), each with a
`run` of its own if it needs one (the Python pack's virtualenv runs `-m debugpy.adapter`). `slow-start = { after = 8,
os = "macos", say = "…" }` is what to say when it hasn't started by then. The debugger's name is its
`[install.<name>]`: missing, it's offered, and debugging starts again once it's in.

Where each thing used to be code, and what says it now:

| Was in tarae's code | Pack key |
|---|---|
| Which debugger a language has | `[debug] debugger` (`way = "java"` for java-debug through jdtls) |
| Finding lldb-dap (`lldb-vscode`, versioned names, `xcrun`), dlv (and where `go install` put it), debugpy (the virtualenv's Python) | `[debugger.<name>] find`, `run` |
| dlv's `-l 127.0.0.1:<port>` | `[debugger.dlv] listen` |
| "install lldb-dap / dlv" | the debugger's `[install.<name>]` |
| macOS's 8-second lldb hint | `[debugger.lldb-dap] slow-start` |
| Dropping dlv's terminal hint and lldb's disassembly; dimming dlv's `Building` | `[debugger.<name>] quiet`, `own-first` |
| Rust's `cargo build` and picking the binary; Go's and Python's launch arguments | `[debug] build`, `launch`, `vars` |
| Rust's test binary (`cargo test --no-run`), `{cargo-test-binary}` | `[test] debug-build`, `{built}` |
| Reading libtest, `go test -v` and pytest output; "no tests ran" (pytest's exit 5) | `[[test.line]]`, `none-ran` |
| The virtualenv's Python (`{python}`) | `vars` with `first` (`${VIRTUAL_ENV}`) |
| Java's "Listening for transport dt_socket at address" | `[test] debug-attach = { wait-for, way }` |
| Which languages attach, and each one's attach arguments | `[debug] attach-pid`, `attach-remote`, `attach-tcp` |

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

([screenshot](screenshots/attach-python.png))
