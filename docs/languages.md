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

When a file opens and its language server isn't found (once per language a session), or debugging finds no debugger,
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
  `space m c` stops it. **`n`**: not now (a server is asked about again next session, a debugger the next time you
  debug). **`N`** never offers that tool again (the list is `~/.local/state/tarae/never-install`).
- **Only with the language's own tools, or Homebrew:** `go install`, `rustup`, `npm`, `uv`/`pipx`, `cargo`, `brew` —
  the first way the language's pack lists whose program is on your `PATH`. Where none fits (Linux without Homebrew,
  say), or it would need more — clangd and lldb-dap (from Xcode or LLVM), debugpy (into your project's virtualenv) —
  the log says where to get it, or the debugger's error does. Nix's `nil` too: a Nix setup adds it in its own
config.
- **rust-analyzer through rustup** is on `PATH` even when the component isn't installed; when it says so, the offer
  comes then.
- **Off your `PATH`:** `go install` puts tools in `$GOBIN` or `~/go/bin`, uv and pipx in `~/.local/bin` — often not
  on `PATH`. tarae looks there too and says so; add the folder to your shell's `PATH` to use them outside tarae.
- `editor.offer-installs = false` turns the offers off.

## Other tools tarae uses

| Tool | For | Without it |
|---|---|---|
| `git` + a C compiler | Downloading and building grammars on first open | Build with `--features bundled-grammars` for 26 grammars built in |
| `git` | File picker in a repository (`git ls-files`, respects `.gitignore`) | A plain directory walk |
| `rg` (ripgrep) | Global search `space /` | A built-in regex search |
| `claude` ([Claude Code](https://claude.com/claude-code)) | [Claude features](claude-integration.md) | Everything else works |
| zellij or tmux | `space c` — Claude Code in a side pane | It runs in the built-in terminal below the code |
| `pbcopy` (macOS) · `wl-copy` / `xclip` / `xsel` (Linux) · `win32yank` or PowerShell (WSL) | The `+` register — `space y`, `space p` | Clipboard commands report an error |
