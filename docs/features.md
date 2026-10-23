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
- [Sessions, review and the sandbox](#sessions-review-and-the-sandbox)
- [Updating without losing your place](#updating-without-losing-your-place)

## First steps

You don't need to know the keys to start:

- **Command palette** `space ?` — find any command by what it does, with its key on the right. `:` commands such as `:w`
  and `:chat-new` are in there too. With nothing typed, what you ran from it comes first, then a first hour's
  commands (the tutor, finding files, the panels, Claude, the review, sessions, the inbox); typed, a word's start
  ranks above letters inside a word, and those above letters here and there ([screenshot](screenshots/ux-palette.png))
- **which-key** — press `space`, `g`, `m`, `[`, or `]` and a card shows the keys that can come next and what they do
  ([screenshot](screenshots/ux-which-key.png))
- **`:tutor`** — a 10-minute hands-on tutorial in a practice buffer, plain text with its keys in colour; editing it
  never blocks quitting. Past the editing keys: the panels and where your keys are, the terminal and `C-\`, Claude,
  cards and `C-q`, the review, sessions and the inbox ([screenshot](screenshots/ux-tutor.png))
- **Start screen** — launched without a file, tarae shows first steps and your recent files; open one with its number key
  ([screenshot](screenshots/ux-welcome.png))
- **A folder** — `tarae .` or `tarae ~/src/app` works in that folder (its `.tarae.toml`), starting on the start
  screen. When you had files open there last time, the start screen's first row offers them back: `r` continues
  where you left off ([screenshot](screenshots/ux-welcome-continue.png)). `editor.restore-files = true` reopens
  them right away instead
- **Mouse** — click to place the cursor, drag to select, alt+click for another cursor, wheel to scroll. In pickers the
  wheel moves the selection and a click opens. (Your terminal's own text selection is usually shift+drag.)
- **Key hints** — while a completion, a picker or a docs window is open, the keys that operate it show dimmed on the
  status line, right after the mode
- **Inside zellij** — zellij's normal mode takes some `C-` keys before tarae sees them: the letters
  `editor.zellij-keys` names (your config only), by default zellij's own — lock, quit, and its modes, session on
  `C-o` ([the list](reference/keymap.md#inside-zellij)); a config of your own names its own (session on `C-y`:
  `y` for `o`). When tarae runs inside zellij (`ZELLIJ` is set), wherever one of those is hinted —
  which-key, the palette, the chat's and the cards' hints, the tutor — tarae says its twin, the key or command that does
  the same without it: `pageup` for `C-b`, `backspace` for insert mode's `C-h`, and for `C-q` what reaches where it
  goes — for the card in front the way back to the code, where it takes the keys itself (`esc` from insert mode, a
  prompt or a picker, `esc esc` from the chat, `C-\ C-\` from the terminal); for a card in a session behind or an
  edit, the way back and then `space I` (the inbox); for a turn that ended, the way back and `space W`. The chat's
  `C-t` `C-p` `C-s` `C-o` are `:model` `:chat-resume` `:chat-screen` `:chat-point`, typed in the code — the way back
  first (`esc esc :model`). The bindings stay as they are; a key with no twin works once `C-g` has
  locked zellij. Every key of a twin is said as bound (a remap moves it). `editor.zellij-hints` = `auto` (inside
  zellij) · `on` · `off` — `off` for zellij's unlock-first preset. `:config-show` lists the keys and their twins
  while they're said, the [keymap](reference/keymap.md#inside-zellij) always
- **What you use** — with `features.count` on in your config, `:features` shows which commands, cards, panels,
  pickers and Claude tools you use and which you never do; counted on this machine only, never sent
  ([feature counts](feature-counts.md))

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

![tarae in meok (ink): Claude's change under review in the code, the chat docked on the right with two sessions' tabs, path bar, the mode's word](screenshots/hero-meok.png)

- **Themes** — meok (ink) and hanji (paper), plus `meok-transparent` and `hanji-transparent`, which leave the
  background to the terminal so its color and blur show through. The default `theme = "default"` asks the terminal for
  its background color (OSC 11 — it never waits, even on terminals that don't answer) and picks meok on dark, hanji on
  light; force it with `TARAE_BACKGROUND=light|dark`. `space t` (`:theme`) opens a small picker that previews each theme
  live on your code — `Esc` reverts, `Enter` saves it to your config ([screenshot](screenshots/ux-theme-picker.png);
  the screen above [in hanji](screenshots/hero-hanji.png)).
  You can also [write your own](configuration.md#custom-themes)
- **One accent color** — coral in meok, vermilion in hanji — used boldly and sparingly; syntax colors stay calm
- **Edges** — where docked surfaces meet (the side panel, the code, the chat, the bottom panel's top) there's one
  thin line, as zellij divides panes, never a box; the docked panels sit on the code's own ground. The surface with
  the keys has its lines in the accent, every other line is a faint wash — move with `C-w h/j/k/l` and the accent
  moves with you. A group of panels shown one at
  a time names them all in its header — the side panel's `files · outline · git`, the bottom panel's `terminal ·
  tests · task` — the one in sight lit; `tab` / `S-tab` go to the next. The bottom panel's header is the line where
  it meets the code — `─ tests ▲ ── all tests ● 1 ▲ 1 648ms ────── tab failure  q close ─`, as a frame names itself in its
  top line: its panels, what the one in front says, its keys at the right (they fold first when narrow); the code
  keeps the row a separate rule took. A click on a name brings that panel in front; a press anywhere else on the line
  drags its height
- **Frames** — what takes your keys floats in a rounded frame with its name in the top line: which-key (`╭─ space
  ─╮`), a picker (its title; the list and the preview meet on one line), a prompt (`command`, `search`), and a card
  of Claude's while it has the keys — its question as the name, its answers set into the bottom line, `╰─ y Run · n
  No ─╯`. When the keys go elsewhere (you type in the chat) its lines go, its name and answers stay on its top and
  bottom rows, and it says how to answer it (`C-q`).
  Notes — completion, docs, hover, toasts, the follow card — float as cards of their own colour, without a frame; two
  cards never touch. The selected row has an accent bar `▎`
- **Path bar** — `─ project › folder › file ● › ƒ enclosing_fn ──`, on the code pane's top line: the function, type,
  or module around the cursor comes from the tree-sitter tree. With several buffers open, the list shows on the
  right. Every pane's top is one line with its name on it — the code's path, the chat's sessions, the side panel's
  views, a slot's tabs — lit in the accent where the keys are; split panes each have their own, the scope and the
  buffers on the focused one's
- **Status line** — the mode (`● normal`: the dot in the accent, or a color per mode, the word in ink bold; `● follow`
  while Claude's follow mode leads the cursor, `● chat · follow` with the keys in the chat) · git branch and `+3 −2` · Claude status (`…` waiting,
  `● change` a change of Claude's waits for its review) · `● errors ▲ warnings` · language, colored by its server's state — dim italic starting, green
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
  itself first. On Linux each session's sandbox keeper has a row with its tier: what that session's commands are
  kept from and what they share with other sessions' ([on Linux](claude-integration.md#on-linux)). `s` stops one, `r` restarts it where that means something, `l` shows what it said (its panel, or
  tarae's log at its last line). Beside the table, its **detail** says what the selected row is: what it's for, its
  command line whole, the folder it works in, the processes it's under up to tarae and the ones it started, how long
  it has run, its CPU and memory now and the most since `:procs` opened, tarae's checks on it; one that ended while
  selected stays in the table, said `gone`, until the selection leaves it (`s` and `r` refuse it). The two are a
  linked group as the tests' panel is: `tab`, `enter` or `C-w l` to the detail (normal mode on it, read-only: search,
  select, `y` copies, `enter` quotes), `tab` again or `S-tab` round the bottom slot's other tabs; narrower than about
  96 columns they're stacked, one open, both named in the slot's line. tarae looks every 30 s while the list is closed (well under a thousandth of a
  core) and checks what it sees: busy for minutes, memory far above what that program usually takes, a server
  left running with no file of it open (stopped), one that keeps exiting (not started again until `r`), a child
  it doesn't know, a disk running out of room (below 10 GB free, or a tenth of a small one: `▲ disk 8.0 GB free`;
  below 2 GB a card that waits for a key, once each time it gets there). Each check is listed with when and what
  tarae did, and while one holds, the status line says so with a `▲` — nothing otherwise. A run of Claude's that
  fails for space says the disk is full first. The session list (`space W`) shows each session's own build folder
  outside its checkout (`build 12.0 GB`) — one its end would remove — and `x` twice on a session behind with
  nothing running drops it (its next build starts cold)
- **Toasts** — status messages fade in as cards at the top right, the left bar colored by kind (info, saved, warning,
  error). The more serious, the longer they stay ([screenshot](screenshots/ux-toasts-sync.png))
- **Rendered doc comments** — `///`, `//!`, and `/** */` blocks show as rewrapped prose: markers stripped, paragraphs
  joined, headings in accent, `code`, lists, `@param` tags, and example code set in, in its language's colors — on the
  code's own ground, a faint bar at the left (the accent's on the block the cursor is in) (`editor.render-doc-comments`, [folded](screenshots/ux-doc-comments.png) ·
  [unfolded](screenshots/ux-doc-comments-open.png)). Plain `//` comments are left alone, apart from `TODO`/`NOTE`
  colors. **One rule everywhere** a file's code is drawn — every pane of a split, a code review, `:diff`, Claude's
  edit under review, an edit follow shows, a picker's preview — and for every move over it (`j`/`k`, clicks,
  `gw` labels): a block renders unless
  - the language has none, or the setting is off;
  - it's being edited or selected: in the pane you're editing in (the focused one), in INSERT or SELECT, any
    selection or cursor on it opens it; elsewhere (NORMAL, other panes, a review, a preview) only a selection wider
    than a character on it does (a line selected right above isn't on it). A preview reads the place it opens
    at as selected — a search hit in a doc comment shows raw. A prompt, picker, card, the chat or the terminal
    taking the keys leaves the mode as it was, so nothing changes (going to Claude's waiting edit from the inbox
    ends INSERT first);
  - a change shown on screen is in or right beside it — a hunk's changed, added or removed lines, or a merge
    conflict — so a diff is always read against the text it changes.

  A rendered block is flowed to the width, so scrolling sideways moves only the code lines. In NORMAL `j`/`k` step
  over a rendered block as one line (they land on its first line); through a raw one they go line by line. The view
  counts a rendered block's rows as drawn — keeping the cursor in sight, `zz`/`zt`/`zb`, the scroll margin — and
  `zj`/`zk` step over it whole; a click anywhere on it lands where the comment starts; a picker's preview never
  starts partway into one
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
  quietly, no error. Lists fill in on a worker thread. `C-n`/`C-p` or arrows, `Tab`, `C-d`/`C-u` half the rows in
  sight, `PageDown`/`PageUp` all of them, `Home`/`End` the first and last, `Enter`, `Esc` — and Helix's: `C-v` opens a
  file or a place beside, `C-s` under (a split), `A-Enter` opens it with the list kept up, `C-t` puts the preview away
  and back
- **Side panel** — `space e` shows the project's files as a tree left of the code, `space o` the outline of the
  file you're editing (its functions, types, and modules from the tree-sitter tree, nested — no language server
  needed), `space q` the list below, `space g` git. Left alone it follows you: the current file with its folders
  opened, or the definition around the cursor. With the keys in it, `j`/`k` move (as in every list panel — `C-n`/`C-p`
  a row too, `C-e`/`C-y` (and the wheel, three rows) the view with the selection staying in sight, `C-d`/`C-u` half the rows in sight,
  `C-f`/`C-b` all of them, `g`/`G` the ends: the review's rail, `:procs`, the test, task and terminal panels, a
  plugin's), `h`/`l` fold and unfold, Enter
  opens (the code gets the keys back), Tab steps files → outline → list → git, Esc leaves it showing, `q` or the same key again closes it; `space` and `:` still
  work. `C-w e` goes to it from whichever window you're in, whatever tab it shows (closed, it opens at the tab it
  last showed), and back again; `C-w h`/`j`/`k`/`l` go to the pane that way, the side panel among them. Folders you scroll past stay pinned at its top. A name too long for it loses its middle, not its end
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
  closes its panel, `:task <name>` (`:make`) runs one by name. With the keys on its panel (`C-w j`, a click), `q`
  closes it once the run has ended — one under way is left running and says `space m c` stops it (the test panel's
  `q` the same, with `space x c`); Esc gives the keys to the code (its header says `esc code`).
  - **The tests panel is two panes** — the list of results and, beside it, the failure picked (else the output),
    under one header, one `q` closing both. `C-w h`/`l` or `tab`/`S-tab` go between them; on the list `j`/`k` (and
    every list's moves) pick a test — the failure beside it follows — and `enter` goes to its place in the code.
    Each names itself in the line over its own column, as every pane does (`─ tests ▲ › results ── all tests ● 1 ▲ 1
    ─┬─ failure ── src/main.rs:31 ─`); the one with the keys has its stretch of that line lit and its name strong, the
    line between them lit while either has them. A click on a name gives that pane the keys. Too narrow for both
    (the list under 24 columns or the detail under 40), they're stacked, as zellij stacks panes: one open, both
    named in its line (`results · failure`); `tab` or `C-w h`/`l` open the other, and the one that had the keys last
    stays open. Docked tall beside the code they're over and under instead: the list as tall as its rows (a third of
    the pane at most), the detail under it, named in its own top line (`├─ failure ── src/main.rs:31 ─`); `C-w t`
    turns them side by side ↔ over and under, kept for that kind (and across `:restart`) until `C-w =` — where the
    choice has no room they're stacked until there is. Narrow, the line folds what the run says (its time, then its counts) before the names. A run under
    way says so once, by its name (the wave); where its details go, the time it has run so far, which becomes its
    whole when it ends.
  - **A task's places and a preflight's steps are lists too** — a run that pointed at two places or more lists them
    beside its output (`▲ src/main.rs:31  mismatched types`, the errors first, each as found); a preflight of two
    steps or more lists its steps (`● fmt 6ms`, `▲ test`, `◦` not run yet, the wave's one cell on the one running). The same
    keys as the tests' list: a place picked puts the output at the line that printed it, on the cursor line's band
    (`tab` lands the keys there); a step picked, its heading at the top. `enter` goes to the place in the code — on
    a step, its first place, else its output from its heading. On the steps' list `o` opens the step's whole output
    in a buffer; `o` on the output selects the step in view in the list. The list grows as the run prints, the
    selection staying on its place. A place whose line the panel no longer keeps says `cut`. One place, or one
    step, is the output alone, as before. Stacked, the panel is as tall as the one open: the list its rows, the
    output as tall as it has printed (never shrinking while it's open, back as tall as you last saw it).
  - **The output is a document** — a run panel's output (the task's, a preflight's, the tests' failure picked, else
    their output) is a read-only document, and with the keys on it (`C-w j`, a click — the cursor on that row) it's
    the code's normal mode on that document: every move, selection, search and count — `j` `k` `w` `x` `v` `/` `n`
    `*` `gg` `z`, your `[keys.normal]` too — `C-e` `C-y` a row, `y` copies to the clipboard, `G` the
    latest. What would edit it is put back and said (`the task's output is read-only — esc code`). The status line
    says `● normal · task` (`· preflight`, `· tests`; `● select · task` after `v`), its position and its search's
    count. It holds still: what the run prints meanwhile comes in below the rows in sight (the head says `↓ output
    after`), `G` catches up. `enter` on a selection quotes it to Claude (`<quoted from="task"` … `"preflight"`,
    `"test"` with the test's name); with nothing selected, your run waiting behind Claude's runs now (the status line
    says `enter run now`), else the chat's input. `tab` / `S-tab` the slot's other panels, `K` where Claude's
    command ran, `o` a preflight step's whole output (beside the steps' list, the step in view selected there), `esc`
    (a selection collapses first) to the code, `q` closes the
    panel once its run has ended. `C-w`, `:` and `space` are the editor's, as from every panel; `]x` / `[x` (the
    detail follows the failure), `]q` / `[q`, the palette and the chat act on the editor. The wheel over it scrolls
    it without taking the keys. `[keys.run]` lays your keys over normal mode's there. Claude's conversation is one
    too — its view mode ([claude-integration](claude-integration.md#reading-the-conversation-view-mode)) — and the
    chat's input a small buffer in insert mode.
  Tasks come from the project — `cargo check`/`clippy`/
  `build`, `go build`/`vet`, `tsc`, every `npm run` script, `ruff`/`mypy`, `make`, as the language packs say — and from `[task.<name>] cmd =
  "…"` (optional `cwd`) in the config. It runs in the background with its output below the code; every
  `file:line:col: message` it prints (rustc, gcc/clang, go, eslint, ruff, mypy — and the forms the packs add with
  `[[places]]`: rustc's long one, tsc's, Python tracebacks) goes into the list as it comes, so `]q` walks the errors.
  Modified files are saved first
- **Terminal** `space T` — your shell in a panel below the code, with the keys: everything you type goes to it
  (colors, full-screen programs, the lot — `esc` too). `C-\` goes round: from the code to the program, from the
  program to its **view mode**, from there to the code. `space T` shows it with the keys — from wherever it is,
  behind another tab or shown without them.
  - **View mode** (`C-\` from the program, `C-w j` to its panel, or the wheel up over it): a cursor on its
    scrollback and screen as text, the code's normal mode on them — `j` `k` `w` `x` `v` `/` `n` `*` `gg`, counts,
    the `z` views; `↑` `↓` a line, `←` `→` a page, `y` copies, `G` the latest. The panel's head says `● view`, the
    status line `● view · terminal`. It holds still: the program's output waits, and so does the program —
    once its next write fills the pty, it's held there as a real terminal's flow control holds it (`● view ·
    output waits`); nothing piles up in tarae. Its questions to the terminal are still answered. `G` catches up,
    leaving does too (the rows you read stay in sight), and the program goes on. `enter` on a selection quotes it
    to Claude — a chip over the chat's input, the program's wrapped lines joined, token-like lines taken out
    ([quoting](claude-integration.md#quoting-what-you-read); not while `llm.screen-terminal` is off). `i` (`a`,
    `enter` with nothing selected) types in the program again, `esc` (a selection collapses first) or `C-\` goes to the code, `q` hides the panel (the shell
    runs on), `tab` the slot's other tabs. The wheel back down to the bottom, nothing selected, gives the keys
    back where they were. `[keys.terminal]` adds keys taken from the program (`C-\` by default; every other key
    is the program's), and `to_program` gives one back; `[keys.view.terminal]` remaps its view mode.
  - Outside view mode `Shift+PageUp`/`Shift+PageDown` scroll its past a page while the program has the keys (the
    next key it gets brings it back down; a full-screen program, which keeps no past, gets them as from any
    terminal) — if your terminal leaves them to the program. A click in it gives the program the keys, a click
    outside takes them back; a drag selects text and copies it. A program that asks for the mouse (htop, vim, lazygit) gets clicks and the wheel instead
    (alt+drag still selects — the panel's title says so), a full-screen one that doesn't gets the wheel as arrow
    keys, and a paste arrives marked as one (a shell doesn't run it line by line). In view mode the mouse is the
    view's, whoever asked for it: the wheel scrolls it, a click puts its cursor on that cell, nothing goes to the
    program; a drag doesn't select there yet (said — `x`, `v` and `y` do)
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
- **Split windows** — every surface is a pane (the code's, the side panel, the review's rail, the bottom slot, the
  chat): `C-w v` side by side · `C-w s` stacked (from the chat or a panel it's the code pane last focused that splits;
  the terminal says a second one comes with the terminal work) · `C-w h j k l` to the pane that way, any kind · `C-w
  w` the next pane, every kind · `C-w H J K L` swap this code pane with the one that way — from a group (the tests',
  a task's, a preflight's, `:procs`) they dock it beside the code instead: `C-w L` right of it, `C-w H` left of it,
  `C-w K` over it, `C-w J` back to the bottom slot; a docked group is a pane of its own, the whole height, its name in
  its top line, its keys the ones it has in the slot; its size its kind's own (`C-w < >`, `C-w - +` over the code, a
  drag of its line, `C-w =`); closed, the code takes the room and its next run lands there again; too narrow for the
  code beside it (or, over it, too short for the code and the slot under it), it folds back into the slot's tabs until
  there's room · `C-w t` turn the code's
  split (side by side ↔ stacked) — from a group, its members (side by side ↔ over and under) · `C-w q` close — the window, or the panel that has the keys (side panel, the
  review's rail, `:procs`, a test, task or terminal panel, as its own `q` does; the chat closes with `space L`, so a
  slip can't cut Claude's turn off) · `C-w o` only this one (`space w` works too); `:vsplit file`, `:hsplit file`.
  Each pane has a title line; the focused one gets an accent bar and the rest dim, the line beside it lit. The same
  document in two panes shares its cursor but scrolls separately ([screenshot](screenshots/ux-splits.png)). A card,
  the picker, a prompt and which-key float over the panes: while one has the keys it's where they are, and they go
  back to the pane under it once it goes. Your panes are kept per folder (`$state/layouts.json`, written as they
  change) and come back with the files on the start screen's `r`
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
  from its upstream (`main ↑1`); it catches up as soon as Claude edits a file or a command it ran ends. Moving onto
  a file (or a click on it) shows it in the editor, at its first change, as a **preview**: one slot the next file you
  pass over takes, so the buffer list doesn't grow — its name in italic in the path bar and the buffer tabs, last in
  `space b` with a faint `preview`. It becomes a buffer of its own when you edit it, press `enter` on its row or
  double-click it; moving, searching or `esc` into it leave it a preview, and closing the tab leaves it as it is.
  The review's rail the same: `j`/`k` onto a file show it as the preview, the rail keeping the keys; `enter` or a
  double-click keeps one. `C-o` goes back through the files shown so, each opened again as the preview; a preview
  another pane shows stays open when the next comes (a buffer of its own then, said). A preview isn't
  reopened at the next start (`:restart` keeps it, a preview still). While the tab has the keys, the file in the editor shows its
  changes against the index **inline** — the removed lines rows of their own on the red wash, the changed ones on the
  green, the path bar saying `against index` — and once the keys leave the tab the gutter is the index's again. A
  file under `:diff` or in a review keeps that base: the tab never touches it. Enter (or `l`) on a changed file opens it to its **hunks and merge conflicts**, one
  row each (where it starts, `+a −r`); moving onto one shows it in the editor. On a hunk `s` stages it and `d` puts
  the index's lines back (an undoable edit); on a conflict `o` / `t` / `a` keep ours / theirs / both. On files,
  folders, or a whole section: `s` / `u` stage / unstage — `S` / `U` stage / unstage everything from any row —, `d` discards (press it twice — an open file's old text is
  one `u` away; an untracked file is deleted), `e` opens it in the editor, `D` shows it against HEAD. `c` commits —
  the message is written in a buffer (`:w` commits it, `:q` cancels), `A` amends; `p` pulls (fast-forward only),
  `P` pushes (setting the upstream the first time), `f` fetches — none ever waits on a password prompt. `B`
  switches branch, `L` is the log (each commit's diff as the preview), `z` stashes, `Z` takes it back, `b` flips
  blame, `r` reads the status again, `?` lists every key. From the code, `space g` lands on the row for what's under the cursor, and the next
  key acts there: `space g s` stages the hunk you're in, `space g t` keeps theirs — then the code has the keys again.
  Moving is yours from the first key: `k` and `g` reach a section's header (`Changes`, `Staged`) even while the
  file's hunks are still being read, and `s` there stages the whole section.
  When the repository's hooks folder (`core.hooksPath`) is inside the checkout — a folder Claude can write — a
  file there that's new, changed or untracked is shown on a card before any commit, push, pull, switch or other
  git-tab action (the hooks it's likely to run are marked): `y` runs them, `n` goes on without hooks this time.
  A `y` is kept for what each file says (with the project's trust answers), so the next action runs them
  without asking; a hook rewritten since asks again. Hooks committed as the base has them run as always
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

## Sessions, review and the sandbox

- **Sessions** `space W` — several branches of work side by side, each a checkout of its own with its own Claude,
  chat, terminal and tests; brought back into main, a rebase's conflicts resolved by that session's Claude
  ([rebasing a session](claude-integration.md#rebasing-a-session)); its git-ignored notes (`session.keep`:
  `target/sb`) moved to the main checkout's `target/sb/sessions/<name>/` when it ends; where each session's Claude
  is reading and writing shown wherever that file is, in any checkout (`▎` and a name tag, each session its own
  colour), and `space F` follows the next one at work — follow moves the view, never your cursor; two sessions'
  changes that would conflict said the moment they do (`fix ≈ main · src/term.rs L210–260`, `git merge-tree` of the
  working trees) — to you, never to a Claude; any file as every session's changes
  woven together, unsaved ones too (`:weave`, `space v w`: each change in its session's colour, conflicts side by
  side, `enter` into the workspace a change is from)
  ([several sessions](claude-integration.md#follow-mode))
- **Review like a pull request** — Claude's edits hunk by hunk, or all at the turn's end
  ([review at the end](claude-integration.md#review-at-the-end)); any change reviewed over the real buffers, and a
  second Claude reviewing as notes on the lines, `space v c`, or a board of them, one per angle with a skeptic
  checking each finding, `space v C` ([code review](claude-integration.md#code-review))
- **Claude's commands in a sandbox** — Seatbelt on macOS; Landlock, seccomp and a session's own namespaces on
  Linux; the network only through tarae's proxy, a card for anything past it
  ([the sandbox](claude-integration.md#the-sandbox), [threat model](threat-model.md))

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
line says `keeping its symbols` meanwhile). With cargo's `build.build-dir` set apart, the target folder may carry
no `CACHEDIR.TAG` of its own; the dSYM's link into the build-dir (tagged, the same profile folder) says it's a
cargo build's all the same. The new tarae's log says where they went, with its pid — or why
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

Work under way doesn't hold it, in any session. A review comes along: since main, uncommitted, or a turn's
(`space v t`), each back on the file it was on. What's running is named on the restart's card, by session and
how long: Claude answering in a chat (and, by file, its edits waiting for your answer — dropped with it: the
claude that asked doesn't outlive the restart), a command in the task panel, tests. `y` restarts anyway: they're ended, and the toast says which. Only what the
card named is ended: something that starts while the card is up makes `y` ask again, naming it, and what starts
after your answer (Claude asked again, a run) is waited for. `w` waits for them to end, and then the card asks
again. While it waits, the status line says what it's waiting for (`restart waits for gitescape: Claude's
turn`), and `:restart` again asks before calling it off. It always waits for edits you accepted to land on disk.

Some things stop it, with a message saying what to do, because they can't be carried over:
- text in a buffer that has no file;
- a file that changed on disk under your edits;
- a change of Claude's waiting for your answer, outside a turn;
- a commit message being written;
- an install running;
- a debug run.

The message names the session when there's more than one. When a file can't be saved, it stops too, and nothing is
lost.
