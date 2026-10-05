# Language support

Syntax highlighting covers 54 languages ([how grammars work](features.md#syntax-highlighting)). Everything beyond
colors comes from external tools that tarae finds on your `PATH` — install the ones for the languages you use, or
let tarae [offer to](#when-one-is-missing).

| Language | Language server (first one found) | Tests | Debugger |
|---|---|---|---|
| Rust | `rust-analyzer` | `cargo test` | `lldb-dap` (builds with `cargo build` first) |
| C / C++ | `clangd` | — | `lldb-dap` |
| Go | `gopls` | `go test` | `dlv` |
| Python | `basedpyright`, `pyright`, `pylsp` — and `ruff` beside it | `pytest` | `debugpy` |
| Java | `jdtls` (Java 21+) | Gradle (`./gradlew` or `gradle`) · Maven (`./mvnw` or `mvn`) | java-debug inside jdtls (offered for download) |
| TypeScript / JavaScript (incl. TSX, JSX) | `typescript-language-server` | — | — |
| Bash | `bash-language-server` | — | — |
| Lua | `lua-language-server` | — | — |
| Zig | `zls` | — | — |
| Nix | `nil` | — | — |
| TOML | `taplo` | — | — |
| YAML | `yaml-language-server` | — | — |
| Helm templates | `helm_ls` | — | — |
| Markdown | `marksman` | — | — |

Any other server can be added, or the choice changed, in your config — see
[Language servers](configuration.md#language-servers).

Notes:

- **lldb-dap** is found as `lldb-dap`, `lldb-vscode`, or a versioned name like `lldb-dap-18` (as Linux packages install
  it); on macOS, through `xcrun` as well. macOS needs a one-time `sudo DevToolsSecurity -enable`
- **dlv** must be newer than your Go (`go install github.com/go-delve/delve/cmd/dlv@latest`)
- **debugpy** and **pytest** run with the project's `.venv` Python, or `$VIRTUAL_ENV`, when there is one
- **java-debug** is a jdtls extension that jdtls doesn't ship. `F5` offers to download it from Maven Central (with
  `curl` or `wget`); copies installed by VS Code or Neovim's mason are found too

## When one is missing

When a file opens and its language server isn't found (once per language until you quit), or debugging finds no debugger,
tarae offers to install it — a card with the command it would run, from the language's
[pack](testing-and-debugging.md#language-packs):

```text
 ▎ Go's language server
   gopls isn't installed. Install it with:
   go install golang.org/x/tools/gopls@latest

   y Install   n Not now   N Never
```

- **`y`** runs it in the task panel, output in sight; once it's in, the servers start for the open files that had
  none, or debugging starts again as it was (the file, the test, the attach). While it runs, no other task starts;
  `space m c` stops it. **`n`**: not now (a server is asked about again the next time tarae starts, a debugger the next time you
  debug). **`N`** never offers that tool again (the list is `~/.local/state/tarae/never-install`).
- **Only with the language's own tools, or Homebrew:** `go install`, `rustup`, `npm`, `uv`/`pipx`, `cargo`, `brew` —
  the first way the language's pack lists whose program is on your `PATH`. Where none fits (Linux without Homebrew,
  say), or it would need more — clangd and lldb-dap (from Xcode or LLVM), debugpy (into your project's virtualenv) —
  a toast says where to get it (and the log), or the debugger's error does. Nix's `nil` too: a Nix setup adds it in
  its own config.
- **Downloaded by tarae:** a pack can say to fetch a release itself ([Downloads](#downloads)) — the Java pack does
  for jdtls where there's no Homebrew: Eclipse's newest milestone release, checked against the SHA-256 published
  beside it, into `~/.local/share/tarae/jdtls`. What it runs on is looked at first (jdtls: Java 21 or newer,
  `$JAVA_HOME`'s java, else `PATH`'s); when it's older or missing, a toast says so instead of the card.
- **rust-analyzer through rustup** is on `PATH` even when the component isn't installed; when it says so, the offer
  comes then.
- **Off your `PATH`:** `go install` puts tools in `$GOBIN` or `~/go/bin`, uv and pipx in `~/.local/bin` — often not
  on `PATH`. tarae looks there too and says so; add the folder to your shell's `PATH` to use them outside tarae.
- `editor.offer-installs = false` turns the offers off.

### Downloads

A way in a pack's `[install.<tool>]` ([Installing from packs](testing-and-debugging.md#installing-from-packs)) can be
a download instead of a command: tarae finds the newest release, fetches it with `curl` (or `wget`), checks it
against the SHA-256 its site publishes, unpacks it into `~/.local/share/tarae/<tool>`, and finds the program there
from then on. A release without a published checksum isn't downloaded. The Java pack's jdtls:

```toml
[[install.jdtls.ways]]
download = "https://download.eclipse.org/jdtls/milestones/{version}/{file}"   # where the file is
version = { from = "https://download.eclipse.org/jdtls/milestones/", pick = 'milestones/(\d+(?:\.\d+)+)' }
file = { from = "https://download.eclipse.org/jdtls/milestones/{version}/latest.txt", pick = '(jdt-language-server-\S+\.tar\.gz)' }
sha256 = "https://download.eclipse.org/jdtls/milestones/{version}/{file}.sha256"
unpack = "tar.gz"                    # or "file": the download is the program
program = "bin/jdtls"                # inside what's unpacked
needs = ["python3"]                  # on PATH besides (here: the launcher's interpreter)
runtime = { program = "java", home = "JAVA_HOME", ask = ["-version"], version = 'version "(?:1\.)?(\d+)', at-least = 21, get = "install a JDK 21 or newer (Ubuntu: openjdk-21-jdk) and point JAVA_HOME at it" }
```

- **Templates** (`download`, `sha256`, `program`, `file`, and the `from` pages) take `{version}`, `{file}`, `{os}`
  and `{arch}` — the last two as Rust names them (`linux`, `macos`; `x86_64`, `aarch64`), or as the release does
  with `rename = { x86_64 = "amd64", macos = "darwin" }`.
- **`version`**: the page at `from` and a regex; its first group (else the whole match) is a candidate, and the
  highest by its numbers wins — a folder listing, a `latest.txt`, a GitHub release's JSON
  (`pick = '"tag_name":\s*"([^"]+)"'` on `api.github.com/repos/<owner>/<repo>/releases/latest`). A version or file
  name is taken only when it's a plain name (letters, digits, `.`, `_`, `-`, `+`).
- **`file`**: a template (`"tool_{os}_{arch}.tar.gz"`), or read off a page the same way (`{ from, pick }`).
- **`sha256`**: a file holding the one hash, or `<hash>  <name>` lines (`sha256sums.txt`, `checksums.txt`) — the
  line naming the file.
- **`needs`**: programs it needs on `PATH`. With `tar.gz`, `tar` too; always `curl` or `wget`. The way is taken
  only when they're there, like a `run` way's program.
- **`runtime`**: what the program runs on, looked at before the card — `program` (in `$<home>/bin` first), run
  with `ask`, its major version the first group of `version`; older than `at-least`, or none, and the toast says so
  with `get`.

It covers a release found on a page, as a `.tar.gz` or as the program itself, with a checksum published beside it
— Eclipse's milestones, and GitHub releases like helm-ls's (a binary per platform, each with its `.sha256sum`). Not
covered: `.zip` archives, and releases that publish no checksum (lua-language-server's, as of 3.19).

## Other tools tarae uses

| Tool | For | Without it |
|---|---|---|
| `git` + a C compiler | Downloading and building grammars on first open | Build with `--features bundled-grammars` for 26 grammars built in |
| `git` | File picker in a repository (`git ls-files`, respects `.gitignore`) | A plain directory walk |
| `claude` ([Claude Code](https://claude.com/claude-code)) | [Claude features](claude-integration.md) | Everything else works |
| `pbcopy` (macOS) · `wl-copy` / `xclip` / `xsel` (Linux, with its display) · `win32yank` or PowerShell (WSL) | The `+` register — `space y`, `space p` | A copy goes through the terminal (OSC 52); a paste says to use the terminal's own paste |

Over SSH (`SSH_TTY` or `SSH_CONNECTION` set) a copy always goes through the terminal — OSC 52, which most terminals
take (some ask first, or need it turned on) — so it lands on your own computer's clipboard, not the server's. Inside
tmux tarae asks tmux what it lets through: with `set -g set-clipboard on` it goes as it is, else wrapped for
`allow-passthrough`; with neither the toast says so. Inside GNU screen it's wrapped in pieces. Reading the clipboard
that way is refused by most terminals, so there `space p` says to paste with the terminal's own paste key — that
comes in as a paste. A copy over 768 KB is too long for the terminal and says so.
