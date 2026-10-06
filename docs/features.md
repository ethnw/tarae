# Features

A tour of what tarae does. Every key is listed in the [default keymap](reference/keymap.md); every setting in the
[settings reference](reference/settings.md). Tests and the debugger have [their own page](testing-and-debugging.md),
and so does [Claude](claude-integration.md).

- [First steps](#first-steps)
- [Editing](#editing)
- [Look and feel](#look-and-feel)
- [Getting around](#getting-around)
- [Files and git](#files-and-git)
- [Syntax highlighting](#syntax-highlighting)
- [Language servers](#language-servers)
- [Java](#java)
- [Updating without losing your place](#updating-without-losing-your-place)

## First steps

You don't need to know the keys to start:

- **Command palette** `space ?` — find any command by what it does, with its key on the right. `:` commands such as `:w`
  and `:chat-new` are in there too ([screenshot](screenshots/ux-palette.png))
- **which-key** — press `space`, `g`, `m`, `[`, or `]` and a card shows the keys that can come next and what they do
  ([screenshot](screenshots/ux-which-key.png))
- **`:tutor`** — a 10-minute hands-on tutorial in a practice buffer; editing it never blocks quitting
  ([screenshot](screenshots/ux-tutor.png))
- **Start screen** — launched without a file, tarae shows first steps and your recent files; open one with its number key
  ([screenshot](screenshots/ux-welcome.png))
- **A folder** — `tarae .` or `tarae ~/src/app` works in that folder (its `.tarae.toml`), starting on the start
  screen. When you had files open there last time, the start screen's first row offers them back: `r` continues
  where you left off ([screenshot](screenshots/ux-welcome-continue.png)). `editor.restore-files = true` reopens
  them right away instead
- **Mouse** — click to place the cursor, drag to select, alt+click for another cursor, wheel to scroll. In pickers the
  wheel moves the selection and a click opens. (Your terminal's own text selection is usually shift+drag.)
- **Key hints** — while a completion, a picker or a docs window is open, the keys that operate it show dimmed on the
  status line, right after the mode pill

## Editing

tarae keeps Helix's keys, command names, and selection model — `w` *selects* a word, `d` deletes it — so Helix key
configs carry over as they are.

- Normal / insert / select modes, count prefixes (`3w`, `2gg`, `2x`), `:` command line
- View — `zz`/`zc` put the cursor line mid-screen, `zt` at the top, `zb` at the bottom, `zj`/`zk` scroll a line
  (the cursor stays unless the view leaves it); `Z` does the same and stays open until Esc · `gt` `gc` `gb` send the
  cursor to the top, middle, or bottom of the screen
- `gw` — two-letter labels on every word on screen (nearest first, the rest of the text dimmed); type one to select
  that word (a jump; in select mode the selection grows to it)
- Movement `h j k l` `w b e` `gg ge gh gl gs G` `C-f C-b C-d C-u` · character find `f t F T` · `A-.` repeats the last
  motion (including the character it looked for)
- Selection `x`/`X` (whole lines, downward/upward), `A-x` (to line bounds), `%`, `;`, `A-;`, `,` · multiple cursors
  `C`/`A-C`
- Regex selection `s` (select matches inside the selection), `S` (split on matches), `A-s` (split into lines),
  `K`/`A-K` (keep/remove selections that match)
- Editing `i a I A o O` `d c y p P` `A-d A-c` `r` `~` `` ` `` `` A-` `` `> <` `J` · `u U` — a whole stay in insert mode is one
  undo step · `.` repeats the last insert (the command that started it and
  what you typed, at the current selections; with a count) · `space c` (or `C-c`) comments/uncomments the selected
  lines (block comments where a language has only those) · `space C` puts a block comment around each selection
  (a cursor's line; line comments where a language has no block comment)
- **Auto-indent** — Enter, `o`, and `O` indent the new line from the syntax tree (`indents.scm` in 36 languages):
  into a block or an argument list, back out after one, a closer moved to its own line lines up with its
  opener's line, Python's `return` ends its block, a continued parameter list aligns under the first
  parameter. Enter between brackets `{|}` puts the closer on a line of its own with the cursor indented
  between. The new indentation is the line above's plus the tree's difference, so a file's own style
  survives an imperfect query; the unit is the file's own (tabs in Go and Makefiles, 2 or 4 spaces as the
  file does), and `Tab`, `>`, `<` use it too. Without a grammar: one level more after an opening bracket.
  A whitespace-only line keeps none of it when you leave it
- Insert mode keeps Helix's keys: `C-w`/`A-Backspace` delete a word back, `A-d` forward, `C-u`/`C-k` to the line
  start/end, `C-r x` types register x, `C-s` makes what you've typed an undo step, `C-h` `C-d` `C-j`, `Home`/`End`
- Auto-pairs `editor.auto-pairs` — `( [ { " ' `` ` `` get their closer (not before a word, and a quote not after one, so
  `don't` stays one quote; Rust lifetimes don't pair); typing the closer steps over it; backspace in `()` removes both
- Syntax moves (tree-sitter) — `A-o` grows the selection to the enclosing node, `A-i` shrinks it back, `A-n`/`A-p`
  select the next/previous sibling (also `A-↑ ↓ → ←`) · `]f` `[f` next/previous function, `]t` type, `]a` argument,
  `]c` comment, `]T` test (each a jump) · `]p` `[p` paragraphs · `]space` `[space` add a blank line below/above
- `m` mode — `mm` matching bracket; `mi`/`ma` + `w W p ( [ { < " ' `` ` `` or the tree-sitter objects `f t a c T`
  (function, type, argument, comment, test — repeat to widen one layer at a time); `ms` `mr` `md` surround
- Shell — `|` pipes each selection through a command (its output replaces it), `A-|` only sends it, `!`/`A-!` insert
  a command's output before/after each selection, `$` keeps the selections the command succeeds on (`:pipe`,
  `:pipe-to`, `:insert-output`, `:append-output`). Commands run in the background; nothing changes if they fail
- Registers `"x`, `_` (black hole), and `+` = the system clipboard (`space y`, `space p`, `space P`) · macros: `Q`
  records, `q` replays (with a count), `"xQ` records into a named register
- Pasting with your terminal's paste goes in as it is — no auto-indent, no auto-pairs — as one undo step, at the
  cursor while typing. In normal mode it's held, not put in: `p` puts it after the selection, `P` before (a
  `:` line opens the command line instead). While a code review is open, a paste in normal mode goes into the
  chat's input when the chat is open, and the toast says so (`i` pastes into the file)
- Grapheme-cluster aware everywhere — movement, deletion, `r`, `f`, and drawing treat `é`, NFD Hangul, 👍🏽, 👨‍👩‍👧, and
  `\r\n` as one character; wide characters get the right width
- **Soft wrap** `editor.soft-wrap` — long lines show as several rows, broken after a space (mid-word only when a word is
  wider than the screen). Continuation rows are indented like the line, hang under a list marker (`- `, `1. `, `> `),
  and have no line number; `j`/`k` and paging move by rows. `"always"` (default) wraps code too (inlay hints then
  stay off lines that wrap); `"prose"` wraps only Markdown, commit messages, and plain text; `"never"` scrolls sideways.
  Per buffer: `z w` (or `:soft-wrap on|off`) flips it for the buffer in front alone; a card's whole text (`v`) always
  wraps, and so does Claude's edit under review
- Vertical movement remembers the visual column (tabs and wide characters included), so you come back to where you
  were after passing short lines

## Look and feel

![tarae in meok (ink): completion card, docs window, path bar, mode pill](screenshots/hero-meok.png)

- **Themes** — meok (ink) and hanji (paper), plus `meok-transparent` and `hanji-transparent`, which leave the
  background to the terminal so its color and blur show through. The default `theme = "default"` asks the terminal for
  its background color (OSC 11 — it never waits, even on terminals that don't answer) and picks meok on dark, hanji on
  light; force it with `TARAE_BACKGROUND=light|dark`. `space t` (`:theme`) opens a small picker that previews each theme
  live on your code — `Esc` reverts, `Enter` saves it to your config ([screenshot](screenshots/ux-theme-picker.png)).
  You can also [write your own](configuration.md#custom-themes)
- **One accent color** — coral in meok, vermilion in hanji — used boldly and sparingly; syntax colors stay calm
- **Cards** — completion, docs, hover, and pickers float as cards with half a line of padding above and below and an
  accent bar `▎` on the selected row. Themes without a background color get rounded borders instead
- **Path bar** — `project › folder › file ● › ƒ enclosing_fn`: the function, type, or module around the cursor comes
  from the tree-sitter tree. With several buffers open, the list shows on the right. Turn it off with
  `editor.header = false`
- **Status line** — mode pill (`▐ NORMAL ▌`, a color per mode) · git branch and `+3 −2` · Claude status (`…` waiting,
  `● review` ready) · `● errors ▲ warnings` · language, colored by its server's state — dim italic starting, green
  connected (a "ready" toast says so too), red failed or exited, faintest when none is installed · encoding and line
  endings · selection count · position (keys you've started — a count, `g`, `space w` — show in its place until
  they're done). It's the screen's last row: a `:` command, a `/` search or any other prompt opens as a card
  standing on it, its completions in the same card above what you type. When the terminal is narrow, the least
  important items fold away first
- **Log** `:log-open` — what language servers print, when they start, get ready, or exit, what's missing from PATH, and
  every message tarae showed, with local times (`$XDG_STATE_HOME/tarae/tarae.log`, the previous run's kept as `.old`)
- **Processes** `:procs` — every process tarae started (language servers by project, the debugger, tasks and test
  runs, the terminal's shell, Claude, plugins' programs, and what each of them started), with what it's for, how long
  it's run, its CPU and its memory (the footprint Activity Monitor shows, on macOS, beside what's resident) — tarae
  itself first. `s` stops one, `r` restarts it where that means something, `l` shows what it said (its panel, or
  tarae's log at its last line). tarae looks every 30 s while the list is closed (well under a thousandth of a
  core) and checks what it sees: busy for minutes, memory far above what that program usually takes, a server
  left running with no file of it open (stopped), one that keeps exiting (not started again until `r`), a child
  it doesn't know. Each check is listed with when and what tarae did, and while one holds, the status line says
  so with a `▲` — nothing otherwise
- **Toasts** — status messages fade in as cards at the top right, the left bar colored by kind (info, saved, warning,
  error). The more serious, the longer they stay ([screenshot](screenshots/ux-toasts-sync.png))
- **Rendered doc comments** — `///`, `//!`, and `/** */` blocks show as rewrapped prose: markers stripped, paragraphs
  joined, headings in accent, `code`, lists, `@param` tags, and example code in its language's colors, on a lightly
  tinted panel. In normal mode `j`/`k` step over a block as one line; it unfolds into its source when you edit or
  select it (`editor.render-doc-comments`, [folded](screenshots/ux-doc-comments.png) ·
  [unfolded](screenshots/ux-doc-comments-open.png)). Plain `//` comments are left alone, apart from `TODO`/`NOTE`
  colors
- **Indent guides** (the block around the cursor one tone brighter), a **scrollbar** with error and warning ticks for
  the whole file, current-line highlight, per-mode colors and cursor shapes ([screenshot](screenshots/ux-guides.png))

## Getting around

- **Pickers** (nucleo fuzzy matching, the same matcher as Helix) — `space f` files (`git ls-files` in a repository, so
  `.gitignore` is respected; with nothing typed they show as a folder tree opened up to the file you're editing —
  Enter or `→` opens a folder, `←` folds it or goes up), `space b` buffers, `space /` searches the project as
  you type (ripgrep's own engine, built in — nothing to install): a regex, smart case (all lowercase matches any
  case), what `.gitignore` leaves out left out and hidden files searched (not `.git`), each key's search stopping
  the one before, the first results within a fraction of a second and the rest as they come. Each row is a line's code with its matches standing out, the preview at
  the line. A pattern not finished yet (`fn draw(`) keeps the last results and says why beside the query —
  quietly, no error. Lists fill in on a worker thread. `C-n`/`C-p` or arrows, `Tab`, `Enter`, `Esc`
- **Side panel** — `space e` shows the project's files as a tree left of the code, `space o` the outline of the
  file you're editing (its functions, types, and modules from the tree-sitter tree, nested — no language server
  needed), `space q` the list below, `space g` git. Left alone it follows you: the current file with its folders
  opened, or the definition around the cursor. With the keys in it, `j`/`k` move, `h`/`l` fold and unfold, Enter
  opens (the code gets the keys back), Tab steps files → outline → list → git, Esc leaves it showing, `q` or the same key again closes it; `space` and `:` still
  work. `C-w e` goes to it from whichever window you're in, whatever tab it shows (closed, it opens at the tab it
  last showed), and back again; `C-w h`/`j`/`k`/`l` stay between the code's windows. Folders you scroll past stay pinned at its top. A name too long for it loses its middle, not its end
  (`user_prof…model.ts`), and the selected one shows whole, over the code. A wide terminal makes it wider (up to
  12 columns, the code keeping 100). A click opens a row, the wheel scrolls. It folds away
  when the terminal leaves the code fewer than 80 columns ([files](screenshots/ux-side-panel.png) ·
  [outline](screenshots/ux-outline.png))
- **The list (quickfix)** — places to go through one by one, kept after the picker that found them closes:
  `C-q` in `space /`, `gr`, `space D` (any picker of places) keeps what's listed; `]q` / `[q` go to the next /
  previous one (wrapping) with its message as a toast, and `space q` shows them in the side panel under their files.
  Each one's message sits at its line's end (where no diagnostic speaks already), and they follow your edits —
  add lines while fixing the third and the fourth is still where it was
- **Build tasks** — `space m m` runs the task again (the first time, `space m p` picks one), `space m c` stops it and
  closes its panel, `:task <name>` (`:make`) runs one by name. Tasks come from the project — `cargo check`/`clippy`/
  `build`, `go build`/`vet`, `tsc`, every `npm run` script, `ruff`/`mypy`, `make`, as the language packs say — and from `[task.<name>] cmd =
  "…"` (optional `cwd`) in the config. It runs in the background with its output below the code; every
  `file:line:col: message` it prints (rustc, gcc/clang, go, eslint, ruff, mypy — and the forms the packs add with
  `[[places]]`: rustc's long one, tsc's, Python tracebacks) goes into the list as it comes, so `]q` walks the errors.
  Modified files are saved first
- **Terminal** `space T` — your shell in a panel below the code, with the keys: everything you type goes to it
  (colors, full-screen programs, the lot). `C-\` moves the keys between the code and it, both ways (inside it the key
  is `editor.terminal-escape`); `space T` shows it with the keys — from wherever it is, behind another tab or
  shown without them — and hides it (it keeps running) only from inside: tarae's keys on its panel (`C-w j`, a
  click on its header). The wheel over it scrolls
  back; a click in it gives it the keys, a click outside takes them back; a drag selects text and copies it. A
  program that asks for the mouse (htop, vim, lazygit) gets clicks and the wheel instead (alt+drag still selects —
  the panel's title says so), a full-screen one that
  doesn't gets the wheel as arrow keys, and a paste arrives marked as one (a shell doesn't run it line by line)
- **One tree for every list of files** — the file picker while you type, `space /` results, changed files, the
  replace preview, references, `space D`, and the code review's rail all show files under their folders, with faint
  guides. While typing, the best match comes first and its folders come with it (a folder holding just one folder
  shares its row), matched letters light up in folder names too, and the selection steps over the folder headings;
  search hits and diagnostics hang under their file. On terminals
  at least 100 columns wide, a syntax-highlighted preview sits on the right ([screenshot](screenshots/m2-file-picker.png)). The wheel over the preview scrolls it, as do `C-f`/`C-b`. `space '` reopens the last one as you
  left it
- **Replace across files** — in the `space /` results, `C-r` asks for the replacement (`$1` for groups) and replaces
  the matches still listed (type to narrow them first). A preview lists each file with its diff; Enter replaces in
  every listed file (one undo step each, unsaved buffers included); `:wa` saves them
- **Search** `/ ? n N *` — matches highlight as you type, the view follows the first match (`Esc` puts you back), and
  the status line shows `/pattern  8/13`. `Esc` in normal mode clears the highlight
  ([screenshot](screenshots/ux-search.png))
- **Split windows** — `C-w v` side by side · `C-w s` stacked · `C-w w` / `C-w h j k l` move · `C-w q` close ·
  `C-w o` only this one (`space w` works too); `:vsplit file`, `:hsplit file`. Each pane has a title line; the focused
  one gets an accent bar and the rest dim. The same document in two panes shares its cursor but scrolls separately
  ([screenshot](screenshots/ux-splits.png))
- **`:` completion** — command names (fuzzy, with aliases and descriptions), then arguments: file paths, themes,
  setting paths and their values, languages. `Tab`/`Shift-Tab` cycle; `→` takes the dimmed suggestion
  ([screenshot](screenshots/ux-cmdline-complete.png))
- **Buffers** — `gn` `gp`, with a bufferline once two or more are open; `:bc`, `:bn`, `:bp`
- `:sh command` runs a shell command in the background

## Files and git

- **Disk sync** — when an open file changes elsewhere (git checkout, a formatter, an agent), tarae notices at once,
  told by the kernel (inotify on Linux, kqueue on macOS) — on a network file system, within 0.5 s. If you haven't
  touched it, only the changed ranges reload — cursor, scroll, and undo are kept, and `u` brings the old content back.
  If you have, you get a conflict notice and `▲ changed on disk` in the path bar: `:reload` takes the disk version, `:w!` keeps yours; plain `:w` is refused
  ([screenshot](screenshots/ux-toasts-sync.png))
- **Auto-save** `editor.auto-save` — `"focus"` (default, when the terminal loses focus), `"idle"` (also after 2 s
  without typing), or `"off"`. A config file other programs read the moment it's on disk is held while it
  doesn't parse — `Cargo.toml`, `.cargo/config.toml`, tarae's own, `.tarae.toml`, `package.json`, Claude Code's
  `settings.json` and the like (TOML and JSON): `config.toml not auto-saved: it doesn't parse (…) — :w writes
  it anyway`; once it parses again, auto-save writes it. Saves (these and `:w`) are written in the background, so typing never waits on the
  disk; quitting waits until they're written, and a test or task run waits too — they run the code on disk
- **Recovery** `editor.recovery` — a buffer with unsaved edits is copied into your state folder after a pause in
  typing (1 s; at most 10 s apart while you type on), on a thread of its own, so a crash, a kill, a power cut or a
  closed terminal loses nothing. The next tarae started in that project shows a card per file: `y` puts the text
  back in the buffer (unsaved — `u` goes back to the file, nothing is written until you save), `c` shows it
  against the file the way `:diff` does, `D` throws it away, `n` asks again at the next start. The card says when
  the file changed on disk since, or was deleted. Saving, undoing back to the file or closing the buffer removes
  the copy; `:q` removes them all, a hangup or `:restart` keeps them. Never copied: files where keys are (`~/.ssh`,
  `~/.aws`, `~/.gnupg`, credentials …, by their real path), files named like keys anywhere (`.env`, `*.pem`,
  `*.key`, `id_ed25519` …), files over 8 MB, and what `editor.recovery-skip` lists
- **Restore** — launched without arguments, tarae reopens the files you had open in this folder, at their
  cursor and scroll positions; open a file by name and you land where you last were in it
- **Persistent undo** — on save, the last 1000 undo steps are written next to your state, so `u`/`U` keep working after
  a restart. If the file changed outside tarae, the history is quietly dropped
- **Git gutter** — a thin bar between the line numbers and the text: added lines green, modified yellow, deletions a
  red underline, compared against the index (so staged changes leave it). Ticks on the scrollbar, `+3 −2` in the
  status line, `]g` `[g` between hunks. Stage or commit elsewhere and come back, and the baseline is reread
  ([screenshot](screenshots/ux-git.png))
- **Git** `space g` — the side panel's git tab, with the keys, like lazygit beside your code: what's staged and what
  isn't, each a tree of files with status letters in the gutter's colors and `+a −r`, the branch and how far it is
  from its upstream (`main ↑1`); it catches up as soon as Claude edits a file or a command it ran ends. Enter (or `l`) on a changed file opens it to its **hunks and merge conflicts**, one
  row each (where it starts, `+a −r`); moving onto one shows it in the editor. On a hunk `s` stages it and `d` puts
  the index's lines back (an undoable edit); on a conflict `o` / `t` / `a` keep ours / theirs / both. On files,
  folders, or a whole section: `s` / `u` stage / unstage, `d` discards (press it twice — an open file's old text is
  one `u` away; an untracked file is deleted), `e` opens it in the editor, `D` shows it against HEAD. `c` commits —
  the message is written in a buffer (`:w` commits it, `:q` cancels), `A` amends; `p` pulls (fast-forward only),
  `P` pushes (setting the upstream the first time), `f` fetches — none ever waits on a password prompt. `B`
  switches branch, `L` is the log (each commit's diff as the preview), `z` stashes, `Z` takes it back, `b` flips
  blame, `?` lists every key. From the code, `space g` lands on the row for what's under the cursor, and the next
  key acts there: `space g s` stages the hunk you're in, `space g t` keeps theirs — then the code has the keys again.
  Moving is yours from the first key: `k` and `g` reach a section's header (`Changes`, `Staged`) even while the
  file's hunks are still being read, and `s` there stages the whole section.
  When the repository's hooks folder (`core.hooksPath`) is inside the checkout — a folder Claude can write — a
  file there that's new, changed or untracked is shown on a card before any commit, push, pull, switch or other
  git-tab action (the hooks it's likely to run are marked): `y` runs them, `n` goes on without hooks this time.
  Hooks committed as the base has them run as always
- **Merge conflicts** — a file a merge or rebase left `<<<<<<<` / `=======` / `>>>>>>>` blocks in shows ours on the
  green wash and theirs on the amber one (a diff3 base faint), and a card under the conflict at the cursor names both
  sides as git labeled them: `space g o` keeps ours, `space g t` theirs, `space g a` both (one undo step each); `]m` /
  `[m` go between conflicts. When none are left, `s` in the git tab marks the file resolved
- **Diff** `:diff` — the buffer against the last commit, shown in place like a review (removed lines on a red wash,
  changed and added ones on a green one, `]g` / `[g` between them) while you keep editing; `:diff main`, `:diff
  HEAD~3` against a revision, `:diff path/to/other` against another file; `:diff-off` ends it. The path bar says
  what it's against. `tarae --diff OLD NEW` opens NEW that way — `git difftool -x 'tarae --diff'`
- **`space g`** — `f` changed files (status letters in the gutter's colors, `+N −M`, each file's diff against HEAD
  as the preview; Enter opens it at its first change) · `p` the change under the cursor as a
  card · `r` undo it (an ordinary undoable edit) · `s` stage just that change (what the buffer has, saved or not) ·
  `b` blame at the cursor line's end — author, age, commit summary — on by default (`editor.git-blame`), `b` flips it
- **Large files** (≥ 1 MB) load in the background — a 100 MB file shows its first screen in 18 ms. Editing and saving
  wait until the file is fully read, so a half-loaded buffer can never overwrite it

## Syntax highlighting

Tree-sitter, parsed incrementally in the background — edits apply to the existing tree immediately, so colors keep
up, and queries run only on the visible lines. Grammars are built from their upstream C sources at the commits pinned
in each language's [pack](languages.md#adding-a-language): 54 languages, including C, C++, Java, Python, JS/TS, Go, Rust, Bash,
SQL, Lua, Zig, YAML, Terraform (HCL), Dockerfile, and Helm templates ([screenshot](screenshots/syntax-helm.png)).

- The default build embeds TOML, Markdown, and comment (6.5 MB binary). Other languages, Rust included, are
  **offered on first open** — a card at the bottom right asks, `y` downloads and builds the grammar (needs git and a
  C compiler), and colors switch on in place; `n` means not until you quit ([screenshot](screenshots/grammar-offer.png)). Several offers stack,
  and only the front card takes keys ([screenshot](screenshots/offer-stack.png))
- `cargo install --path . --features bundled-grammars` embeds 26 common grammars instead (9.8 MB binary, no network
  needed later)
- `tarae grammar list` · `tarae grammar install [lang…]` · `:grammar-install [lang|all]`. Turn the offers off with
  `editor.offer-grammars = false`. Downloaded grammars live in `~/.local/share/tarae/grammars/`. A missing language
  server or debugger is offered too ([When one is missing](languages.md#when-one-is-missing))
- **Injections** — Markdown code blocks in their language, bold/code/links inside paragraphs, `TODO`/`NOTE` in comments,
  code inside macros, and Go templates inside Helm YAML ([screenshot](screenshots/syntax-injections.png))
- **Locals** — a name that refers to a local definition looks like that definition wherever it's used: a
  parameter stays a parameter (italic, in the built-in themes) through the whole body, even when the signature is
  off screen, while `let` bindings and same-named things elsewhere don't. From `locals.scm` in 19 languages
  (Rust, Python, Go, C, JS/TS, Ruby, Kotlin, Swift, …); lookups reach up to 64 KB above the screen
- `:lang <name>` sets a buffer's language by hand
- The highlight queries in [`runtime/queries/`](../runtime/queries) come from Helix 25.07.1 (MPL-2.0) and are embedded
  in the binary

## Language servers

Opening a file starts the first matching server on your `PATH` — plus any that run beside it, like `ruff` for Python ([several servers](configuration.md#language-servers)) — rust-analyzer, gopls, pyright, typescript-language-server,
clangd, jdtls, marksman, taplo, and more; the full list is in [Language support](languages.md). Choose or add servers with
`[lsp.<name>]` and `[lang.<language>]` ([how](configuration.md#language-servers)), or turn them off with
`editor.lsp = false`.

Each server gets its own reader and writer threads, so the editor never waits on one. Responses that arrive after the
cursor or document moved on are dropped.

- **Diagnostics** — `●` in the gutter, underlines (curly where the terminal is known to support them), the message at
  the end of the line with a faint wash of its color (`editor.inline-diagnostics`,
  [screenshot](screenshots/ux-error-lens.png)), `]d` `[d`, and a `space d` list. When the line end can't hold a message
  (a second line with the details, a long code line, more than one), putting the cursor on its underline shows it in
  full in a card, with `code` in syntax colors (`editor.cursor-diagnostics`; Esc hides it for that line)
- **Completion** — pops up as you type, including after trigger characters like `.`, and filters locally as you keep
  typing. Snippets expand with the cursor at the first stop, auto-imports come along, and the selected item's docs show
  alongside. `C-x` asks by hand ([screenshot](screenshots/m3-completion.png))
- **Signature help** — after `(` or `,`, a card above the cursor line with the current parameter in accent and its docs
  below; `1/3` for overloads ([screenshot](screenshots/m3-signature.png))
- **Inlay hints** — types and parameter names as dim italic text, requested only for what's on screen
  (`editor.inlay-hints`, [screenshot](screenshots/m3-inlay-hints.png))
- **Code actions** `space a` — quick fixes first, and a diff on the right shows exactly what each one will change before
  you pick it, across files ([screenshot](screenshots/m3-code-action-preview.png))
- `space r` rename across files · `:format`, and `:w` formats first (`editor.format-on-save`; a slow or failing server
  gets 2 s, then the file is saved as is) · server-sent edits, including creating, moving, and deleting files — one undo
  step per file
- `gd` definition · `gD` declaration · `gy` type definition · `gi` implementation · `gr` references · `space k` hover
  — docs render as Markdown with code in its own colors ([screenshot](screenshots/m3-hover.png)). A list of
  places (`gr`, or several `gi`) shows each one's code: the line trimmed, the name standing out, line numbers
  right-aligned, by file then line. For `gr` the declaration (the server's own definition) comes first, marked `decl`.
  While the server is still starting or loading the project, `gr` doesn't wait: the name's text matches (whole
  word, case as written) show at once, titled `text matches`, and the server's references take their place when
  they come (it says so). A server that answers within a moment never shows them
- **Jump list** per pane — `C-o` back · `C-i`/Tab forward · `C-s` saves a spot · `space j` lists them (with a preview;
  picking one keeps where you were, so `Tab` comes back). Go-tos, searches, `gg`/`ge` and
  switching files record where you were, and a saved spot follows later edits · `ga` = the file shown before this one
- `space h` selects every use of the symbol under the cursor in this file (the server's, not text matches) — then `c`
  edits them all · `space D` lists diagnostics in every file the server reported, open or not, errors first ·
  `:lsp-restart`, `:lsp-stop`
- **Symbols** — `space s` in this file, `space S` across the workspace, with kind glyphs in the same colors as code
  ([file](screenshots/ux-symbols.png) · [workspace](screenshots/ux-workspace-symbols.png))
- Server progress (indexing and the like) shows in the status line

## Java

Put [jdtls](https://download.eclipse.org/jdtls/snapshots/jdt-language-server-latest.tar.gz) (it needs Java 21+) on
your `PATH`.

- Multi-module Maven and Gradle projects share one server, rooted at the topmost `pom.xml`, `settings.gradle`, or
  `build.gradle` inside the repository. A folder with no build file works too. Import progress shows in the status line
- `gd` into libraries and the JDK opens their source (from source jars) or the decompiled class in a read-only buffer
  ([screenshot](screenshots/java-jdt-source.png))
- Debugging needs the java-debug extension, which jdtls doesn't ship. `F5` offers to download it from Maven Central and
  loads it into the running jdtls — no restart ([screenshot](screenshots/java-debug-offer.png)). Copies already
  downloaded by VS Code or Neovim's mason are found too
- Tests and debugging for Java: see [Tests and debugging](testing-and-debugging.md)

## Updating without losing your place

Install a new tarae (`cargo install …` again) while one is running, then type `:restart`. The running tarae
saves your files and starts the new one in the same terminal, in the same zellij or tmux pane, and you're back
where you were — in every session, each with its own: the open files with their cursors, selections, and scroll,
the split windows, the side panel, the chat with the same conversation (and the words you were typing in it), and
a code review — of the uncommitted changes or since a branch — on the file it was on, its comments not yet sent
and its viewed marks with it. The settings you changed with `:set` come along too, and the session that was in front is in front. A session behind
comes back quiet: its language servers start when you bring it to the front, and its Claude when you send it a
message (resuming the same conversation). A toast says what came back (`Restarted · 3 sessions · 9 files as
they were`), and one after it names what didn't (`checkout gone: old-parser · file gone: old.rs`).

It runs the tarae at the path the running one was started from, so whatever `cargo install` put there is what
comes up. Before it lets go, it checks that the new binary runs and answers as a tarae; one that doesn't (a
broken build, the wrong file) is caught while the old tarae is still running, and you stay in it with a message
saying why. Nothing is built by default. `editor.restart-binary` points it at another binary, and
`editor.restart-build` makes it build one first, which is how tarae is developed in tarae
([how](../CONTRIBUTING.md#developing-tarae-in-tarae)). `:restart!` skips that build. A card says which checkout
it builds in and at what commit — `Restart into main's build (1085db8)?` — the one tarae started in; with another
session in front, `f` builds that one's instead. If the build leaves the binary as it was and it's older than
that checkout's commit, a card says so rather than restarting into the same tarae (`y` restarts anyway).
On macOS, once a restart goes ahead (not on one turned down at a card), a cargo build's binary with a
`tarae.dSYM` beside it (tarae's own release build) has that dSYM kept under its UUID in its target folder's
`dsyms/`, the newest five, so the tarae you're running can still be symbolized after the next build (the status
line says `keeping its symbols` meanwhile). The new tarae's log says where they went, with its pid — or why
they couldn't be kept, and the restart goes on. A binary elsewhere (`~/bin/tarae`) keeps nothing. On
disk that's at most five dSYMs of ~70 MB per target folder — clones of the build's at first, their own space
once a build replaces it; a session's checkout's go with the checkout.

Cards waiting for you come back as well as they can. One a command of yours raised — the preflight's first run,
ending a session, bringing one, deleting a branch — is asked again by that command in its session, with fresh
checks, and always on its card — nothing runs or goes until you answer it, even if by then nothing would be lost
or the project has checks of its own. One
whose cause runs again (a grammar to download, a plugin's permissions, the project's settings, unsaved
edits a gone tarae kept) comes back by itself. The rest are a moment's decision about something live — a force
push, hooks about to run, closing a buffer under review — and the restart's card names them, with their session,
so you can answer them first.

Some things don't come along. A shell in the terminal panel (`space T`) ends; the new tarae says so, naming the
session when it's one behind, and `space T` starts a fresh one. Language servers start again, so a large project is indexed again before
go-to-definition and the like answer. The turns `space u` can take back and Claude's notes come back, each in
its own session; the list (`]q`) belongs to the old tarae and stays behind. Undo within a file (`u`/`U`)
keeps working, as after any restart (`editor.persistent-undo`).

Some things make it wait, and some stop it. If Claude is answering in any session's chat, the restart waits for
those turns to end, and for any edits you accepted to land on disk. Until it happens the status line says what it
waits for (`restart waits for gitescape`); `:restart` again asks before calling it off. It stops, with a message
saying what to do, if something open can't be carried over or would be cut short: text in a buffer that has no
file, a file that changed on disk under your edits, a change of Claude's waiting for your answer, a review of
Claude's turn (`space v t`), a commit message being written, a task or tests still running — in any session, the message naming which — or a debug run. When a
file can't be saved, it stops too, and nothing is lost.
