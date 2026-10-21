# tarae tutorial

Ten minutes, hands on. This is a practice buffer — edit it freely, it's
never saved. Forget a key? Press `space` — a card shows what comes next,
and `space ?` finds any command by what it does.
@zellij
@zellij Inside zellij, its normal mode takes these before tarae sees them
@zellij (`editor.zellij-keys` names them):
@zellij   {zellij-keys}
@zellij Where tarae has another way, this tutor and the hints say it; for the
@zellij rest, `C-g` locks zellij — every key is tarae's then — and `C-g` again
@zellij unlocks it.

────────────────────────────────────────────────────────────────

## 1  Modes — a key types a letter, or runs a command

The word at the bottom left says where your keys go. It says **normal**
now: here keys are commands. `i` is **insert** — from then on what you
type is text. `esc` comes back.

  → Put the cursor at the end of the line below, press `i`, type
    anything, `esc`.
     here

────────────────────────────────────────────────────────────────

## 2  Moving around

  `h` ←   `j` ↓   `k` ↑   `l` →      (a click works too)
  `w` next word   `b` previous word   `e` end of word
  `gg` top   `ge` bottom   `gh` line start   `gl` line end

  → Move down with `j` and press `w` a few times.
     one two three four five

────────────────────────────────────────────────────────────────

## 3  Select first, then act (the heart of tarae)

`w` **selects** a word, and `d` deletes what's selected. You see what a
key will act on before it does. `x` selects a line (again: one more).

  → On the line below, move onto "wrong", select it with `w`, then `d`.
     This sentence has one wrong word in it.
  → Go to the line below and press `x` `d` — the whole line goes.
     This line should be deleted.

`c` deletes the selection and drops you into insert mode (change).

  → Select "blue", press `c`, type "red", `Esc`.
     The sky is blue

────────────────────────────────────────────────────────────────

## 4  Inserting

  `i` before cursor   `a` after cursor   `I` line start   `A` line end
  `o` new line below   `O` new line above

  → On the line below, press `A` and add " done.".
     This sentence isn't

────────────────────────────────────────────────────────────────

## 5  Undo

  `u` undo   `U` redo — don't be shy, press things and see.

────────────────────────────────────────────────────────────────

## 6  Multiple cursors

`C` adds a cursor on the line below. What you type now happens in every
place at once.

  → On the first "apple": `w`, `C` twice, `c` "pear" `esc` — three
    lines change at once.
     apple one
     apple two
     apple three

Select everything with `%`, then type a pattern after `s` — a cursor
lands on every match.

────────────────────────────────────────────────────────────────

## 7  Search

  `/` search (matches light up as you type)   `n` next   `N` previous
  `*` search for the selection
The bottom right says which match you're on (3/12); `esc` in normal
mode clears the highlight.

  → `/thread` `Enter`, then `n` a few times.
     one thread, another thread, a third thread

────────────────────────────────────────────────────────────────

## 8  The space menu — find anything without knowing the keys

  `space f` open a file      `space /` search the project
  `space b` open buffers     `space ?` find any command
  `space t` pick a theme (it changes as you move)

────────────────────────────────────────────────────────────────

## 9  Panels — and where your keys are

  `space e` files   `space g` git   `space o` outline   `space q` the list

One place has your keys at a time: the word at the bottom left names it,
and the place says its keys — a side panel at its foot, a panel below
the code in its header. Every list moves alike:
  `j` `k` move   `enter` open   `q` close   `esc` code
`esc` gives the keys back to the code and leaves the panel up.
`C-w` then `h` `j` `k` `l` goes between the code and the panels around it;
`C-w e` to the side panel and back; `C-w q` closes the one you're in.

  → `space e`, move with `j` `k`, `esc` back here. `C-w e` to the panel
    again, then `q` closes it.

────────────────────────────────────────────────────────────────

## 10  The terminal — C-\ goes round

`space T` opens a shell under the code, and your keys go to it. There
`esc` is the shell's — the programs you run need it. `C-\` goes round
instead: from the shell to its view mode, from there to the code, and
from the code back to the shell.

In view mode the shell's past and its screen are text you read with the
code's keys: `j` `k` `C-d` `C-u`, `/` search, `x` then `y` copies, `G` the
latest. While you read, what the shell prints waits for you.
  `i` type in the shell again   `esc` code   `q` hide it (it runs on)
  `C-w j` from the code goes to its view mode too
  `S-PageUp` `S-PageDown` scroll its past while you type in it

  → `space T`, type `ls` `enter`. `C-\`, then `k` and `x` `y` copy a
    line. `i` to type again, then `C-\` `C-\` back here.

────────────────────────────────────────────────────────────────

## 11  With Claude

  `space i`  ask Claude to rework the selection — its change unfolds in
             the buffer in red and green: `y` accept · `n` reject
  `space l`  the chat on the right — Claude sees the file, the cursor and
             the selection. `C-r` puts code from its answer in the
             selection

In the chat, `esc` puts a cursor on the conversation: the code's moves,
`/` search, `y` copies; `i` types again, `esc` once more to the code.

  → Select the line below with `x`, press `space i`, type "make it
    polite", `enter`.
     hey, look at this

────────────────────────────────────────────────────────────────

## 12  Cards — when tarae or Claude asks you something

A card asks before something happens: Claude reading a file outside the
project, a command it wants to run, a grammar to download. Answer with
the keys on the card — `y` yes · `n` no · `v` show more. `esc` never
answers a card.

@plain-q A card waits while you type in the chat or the terminal, and the status
@plain-q line says so: ▲1 waiting · C-q. `C-q` hands it your keys.
@zellij-q A card waits while you type in the chat or the terminal, and the status
@zellij-q line says so: ▲1 waiting. zellij takes `C-q`, so go back to the code —
@zellij-q `esc esc` from the chat, `C-\ C-\` from the terminal — and a card in
@zellij-q front takes your keys there.

  → `:e try.py` `enter` — unless Python's colours are here already, a
    card offers to download them. `n` for now, `:bc!` closes the file.

────────────────────────────────────────────────────────────────

## 13  Reviewing — your changes, as a pull request

`space v v` reviews what changed since the last commit: the files on the
left, each change in its file.
  `]g` `[g` next and previous change   `space v m` this file viewed
  `space n` a comment here             `space v s` send them to Claude
  `space v c` Claude reviews it too    `space v q` end the review
`q` in the file list ends it as well — it asks first if files are left
unviewed.

────────────────────────────────────────────────────────────────

## 14  Sessions — more than one piece of work at once

A session is a branch in a checkout of its own, with its own buffers,
chat, terminal and tests. `space W` lists them. Nothing is typed in that
list — its letters are keys:
  `n` new   `enter` or `1`–`9` switch   `d` end   `b` bring it in
"Bring it in" moves its base branch up to the session's work.

────────────────────────────────────────────────────────────────

## 15  The inbox — what waits for you

`space I` lists everything waiting for your answer: every card and every
change of Claude's, in every session, oldest first. `enter` goes to it.
@plain-q The status line counts them (▲2 waiting), and `C-q` takes you to the
@plain-q first.
@zellij-q The status line counts them (▲2 waiting). zellij takes `C-q`, which
@zellij-q goes to the first elsewhere — here `space I` lists them.

────────────────────────────────────────────────────────────────

## 16  Save and quit

  `:w` save   `:q` quit   `:q!` quit without saving
Switch to another window and your files save themselves. A file changed
on disk is reloaded for you.

That's it! Close this tutorial with `:bc!` and open a file with `space f`.
