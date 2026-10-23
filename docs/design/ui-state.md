# UI state — what's on screen, as data

Status: design approved (2026-10-04) with the five recommendations (see "Decided"); phases 1, 2 and 3 built;
phase 4's guarded keys came back as **shuttle keys** (user decision 2026-10-19, phase 1 built — "Shuttle
keys"), its speaking still dropped. The direction was approved after queue phase 0 (1085db8).

## Why

The screen is tarae's truth, but only people can read it. Four readers need it as data, from one state:

1. **Outside drivers.** A person's script, or another agent driving tarae through its terminal. Today that's
   Claude in zellij, reading screen dumps and sending keys. Every slip on 2026-10-04 was "the keys went where I
   didn't think the focus was":
   - a `y` typed into the chat input,
   - a message sent to the terminal's shell,
   - ` W` typed into a chat input that wasn't on screen,
   - a lost card nobody noticed for 6 minutes.
2. **e2e tests.** They should check state, not grep the screen.
3. **tarae's own Claude.** It reads the state and points into it: which card, which panel row, which review
   file the user is looking at. Then "why did this card come up?" is answered about the right card. It never
   answers a card and never acts.
4. **Later, a screen-reader mode.** It speaks the focus, a new card, a run's end.

Beside these four, the user's idea: **show the user what Claude was given and what it cites**.
- (a) Quiet marks on UI elements that went into Claude's context.
- (b) Element ids in Claude's answers become links that jump to the element and highlight it.
- (c) The user points at an element ("이거 왜 떴어?") and its id goes to Claude.

The words are *given* and *cited*, not "attention".

## What's there already (the seams)

The state is mostly gathering. Most of it exists, spread around:

| What | Where | What it lacks for this |
|---|---|---|
| Where the keys go | `chrome::Keys` / `Editor::keys_at` (status-line label) | Not the dispatcher. `handle_key` is its own chain: `card_key`, `C-q`, `terminal_key`, `offer_key`, completion, then `handle_key_inner` (`review_key`, picker, `chat::key`, `rail_key`, `sidebar::key`, procs, plugin, `bottom_key`, start screen, prompt…). `keys_at` has no prompt over the code, no pending prefix (`Editor.pending`), and gives the terminal's *program* and tarae's keys on the terminal panel the same `Slot(Terminal)` — the very difference behind slip 2 |
| Cards | `offer::Offer` (`id` from `offer_seq`, unique in the process; `rev`, `since`), `Editor.offers` (up, front first), `Session.parked`, `Editor.offer_seen` (in sight since), `Session.asking` + `asking_sync` (a lost card is found and said) | The answer keys aren't data: the card's hint row works them out from `lasts`/`keeps`/`preflights`/`aborts`/`end_dirty`… |
| Sessions | `Session`, `Editor::doing` → `Doing`, `cards_waiting`, `session_spots` | — |
| Geometry of the last frame | `editor::Screen` (panes and their line origins, `chat_x`, `picker_list`, `sidebar`, `review_rail`, `slot_tabs`, `toast_card`, `note_card`, `thought_card`, `terminal_rows`, …) | Filled by `term.rs` every frame. Most panels have their visible rows here already |
| After a frame | `term::render` → `toasts_drawn`, `trace_drawn`, `frame_drawn`, `last_frame`, `seeing::after_frame` | The hook point for "state as drawn" |
| Did my input land | `Editor.input_seq` (keys, clicks, the wheel, pastes) | Not visible from outside |
| Runs | `trace::RunTrace` steps, `waits::Waiting::shown`, `Session.runs_next` (queued) | No run id |
| What Claude was given | `chat::context` (`on screen:` / `also on screen:`), `follow::Gaze`, replay stops | UI elements aren't in it |
| Ids in answers | `chat::refs_in` (`path:line` → links), `<show at=…>` | Only places in code |
| Pointing | `labels.rs` (`gw` labels) | Only code positions |
| Claude sees the screen | `seeing.rs` (`screen` tool: the frame as text, asked on a card) | All or nothing; no structure |

## Principles

1. **One truth.** The state is read from the same functions that route keys and place things. It never redoes
   layout or guesses routing. Where no single function exists today (the keys, a card's answers), one is
   pulled out, and both the drawing or routing *and* the state use it.
2. **Structure and labels, not contents.** Ids, kinds, titles, row labels, paths, counts, what a card asks.
   Never code text, terminal output, chat bodies, previews, diffs. The screen dump stays the way to see
   contents.
3. **Read-only.** Nothing in it acts. Drivers act with real keys (see "What it is not").
4. **Computed when asked.** Nobody asking means nothing runs: no timers, no wakes, no writes.
5. **One state, two levels.** *Full* for drivers and tests; *Claude's* is a strict subset (see "Privacy").

## The shape

`src/uistate.rs`: `UiState` (serde), built by `Editor::ui_state(level)` from the editor and `editor.screen`.
JSON for programs, and a short text form for people and Claude, from the same struct.

```json
{
  "v": 12,
  "tarae": { "pid": 4182, "boot": "9f3ac1d0", "version": "0.9.0" },
  "frame": 4812,
  "input": 1033,
  "size": [160, 48],
  "mode": "normal",
  "keys": {
    "to": "card#12", "region": "card", "takes": "answers",
    "answers": ["y", "a", "n", "v"], "pending": "", "drawn": true, "under": "chat"
  },
  "prompt": null,
  "front": "s1",
  "sessions": [
    { "id": "s0", "name": "main", "doing": "idle", "cards": 0 },
    { "id": "s1", "name": "uistate", "branch": "uistate", "doing": "waiting", "cards": 1 }
  ],
  "cards": [
    { "id": "card#12", "session": "s1", "whose": "claude", "kind": "run",
      "title": "Claude wants to run a command", "asks": "cargo test -p tarae",
      "answers": [{ "key": "y", "does": "run it" }, { "key": "a", "does": "also `cargo test` unasked, this chat" },
                  { "key": "n", "does": "tell Claude no" }],
      "state": "asking", "weighty": true, "settled": true, "rev": 0,
      "up": "front", "waited_ms": 4100, "in_sight_ms": 3900, "seen_whole": true, "at": [96, 30, 62, 9] }
  ],
  "inbox": [
    { "id": "card#12", "session": "s1", "kind": "run", "asks": "Claude wants to run a command · cargo test -p tarae",
      "waited_ms": 4100, "up": "front" },
    { "id": "edit#s0:review", "session": "s0", "kind": "edit", "asks": "Change 1 of 2 · src/a.rs",
      "waited_ms": 900, "up": "review" }
  ],
  "toasts": [ { "id": "toast#311", "kind": "warning", "text": "…", "where": "card", "shown": true } ],
  "panels": { "code": {…}, "side": {…}, "review": {…}, "edit_review": null, "picker": null,
              "slot": {…}, "chat": {…}, "follow": {…} },
  "runs": [ { "id": "run#7", "of": "claude", "label": "cargo test", "state": "waiting",
              "waiting": "cargo's lock · 0:37", "session": "s1" } ]
}
```

The same state as text (`tarae state`; Claude's tool returns this form at its level):

```
tarae 4182 · frame 4812 · input 1033 · 160×48
keys → card#12 (answers y a n v) · under it: chat
session s1 uistate (front) · claude waiting · 1 card     s0 main · idle
card#12 claude · run · "cargo test -p tarae" · y run it · a also `cargo test` unasked · n no · settled
run#7 claude · cargo test · waiting for cargo's lock · 0:37
side git · 4 rows · in hand git:src/term.rs
```

### Fields

- **`keys`** — the field that would have stopped all four slips.
  - `to`: the element id that takes the next key.
  - `region`: `code` · `prompt` · `picker` · `chat` · `view:chat` (the chat's view mode — normal mode on its
    transcript, a read-only document, `chatdoc.rs`; its handler `chat-view`, V 8) · `view:terminal` (the terminal's — tarae's keys on its panel, its handler
    `terminal-view`; there's no `slot:terminal`) · `output:task` · `output:preflight` · `output:tests` (panes P2c,
    `rundoc.rs`: a run panel's output as a read-only document, normal mode on it with `[run]` over — their handler
    `output`; no `slot:task` or `slot:tests`) · `output:procs` (`:procs`' detail, what its table selected — panes P3
    step 4, the same document pane) · `list:<kind>` (a linked group's list, below; `:procs`' table is `list:procs`,
    handler `list` — there's no `slot:procs` and no `procs` handler since) · `side:<tab>` · `review` ·
    `edit-review` · `slot:plugin` · `terminal-program` · `card`. In a view mode or on an output `pending` and `char_next` are its own
    (its count, `g`, `f`).
  - `takes`:
    - `commands`: normal mode, panels.
    - `text`: letters become text — insert mode, the chat input, a prompt, a picker query.
    - `program`: everything goes to the terminal's program.
    - `answers`: a card's keys.

    A driver about to send `y` checks `takes`.
  - `answers`: when a card has the keys (an edit's review: `y n a q`).
  - `held`: `C-q` gave the card the keys — every key is its (else only its answers are, the rest go `under`).
  - `pending`: a key sequence under way, as which-key shows it (a count first). `char_next`: the next key is
    taken as a character (`f`, `r`, a `gw` label).
  - `drawn`: the region is on screen this frame.
  - `under`: what has the keys once a `C-q` card lets go.
  - `waiting` (V 9, friction 60): the card that takes the keys once they've been quiet a second — it came while
    keys were coming in; until then they go where `region` says (text form: `card#3 takes them once the keys are
    quiet`). A driver about to answer it waits for the next state with `waiting` gone, or sends `C-q` first. A
  shuttle's step frees a card whose quiet has come before it checks anything, and never between its check and its
  key: a step checked as the code's lands in the code.

  `keys` comes from one new `Editor::keys_to()`, and the routing test (phase 1) holds `handle_key` to it.
- **`mode`, `prompt`** — mode, and the prompt's kind (`PromptKind::label`) and text (full level only).
- **`sessions[].view`** (V 8, the front session's only) — a surface's view mode (`chat` · `terminal`), or a run's
  output with the keys (`task` · `preflight` · `tests` — `rundoc::state`, the same shape): `surface`,
  the cursor's `line` of `lines` (V 11: the chat's count its transcript's lines, a paragraph each — a drawn row
  before), `selections`, `selects` (wider than a character), `mode` (`normal` · `select`),
  `held` (something came since it was made — the terminal's: its program's output waiting). Shapes, never text; Claude's level never has it.
- **`sessions[].with`** (the front session's only; absent when empty) — what goes with the chat input's next
  message, in the order added: `{kind: "pointed", id}` · `{kind: "screen", cols, rows}` · `{kind: "quote", from,
  lines, cut, hidden}` (`chat::Chat::with_state`). Shapes, never text; Claude's level never has it (unsent).
- **`sessions`** — id, name, branch, `doing` (`Doing`: `idle` · `working` · `waiting` · `done` ·
  `stuck {tool, secs}` — its chat's Claude), `reviewer: "reviewing"` while its reviewer reads, `"waits to review"`
  while it waits its turn in the line (`reviewer.rs`, #195; absent else — a field of its own, not a `doing`: the two work apart and often at once, and a driver
  waiting for `doing: idle` waits for the chat's turn, not a review), and `reviewer_run`, how its reviewer's
  last run went, from its start until the next one, whether or not its review is open:
  - `state`: `waiting` (its turn in the line, none of it reading yet) · `reading` · `done` · `stopped`; `ms`: how long it read (so far, while it reads); `reads`.
  - `findings`: those still pinned; `answered`: those you or the author answered; `unsent`: those not sent to
    the author yet (`space v s`).
  - `high`: those on a high-risk change, `null` when its review isn't the one open.
  - `verdict`: its first line; `why`: why it stopped; `by_you`: you stopped it (`:reviewer-stop`); `of`: what
    its review was of.
  - `news`: it ended behind, and the session hasn't been in front since.

  And `review`, its review under way as the review record follows it (`reviews.rs`; an edit's review if one
  is up, else its code review's; absent: none): `kind` (`edit` · `turn` · `uncommitted` · `branch` ·
  `replay`), `changes` seen so far, `decided` (answered, or undone), `looked` (`{looked, high}`, a code review
  in front; else null), `active_s` (on screen with you there), `in_hand` (`{file, line, s}`). Never at Claude's
  level.

  Claude's level keeps every session's state and counts, its own session's too, and never the reviewer's
  words (`verdict`, `why`, `of`). Nothing of the reviewer reaches the author until you send its findings
  (`space v s`), and its verdict never does; its `reviewer:` line says when findings aren't sent yet, and how
  they come (f24). Then come
  cards waiting, `rebasing n/m`, the reloaded `•`, and once its Claude compacted its
  conversation (`compaction.rs`), `compacted`: `count` since tarae started, the `last` one's `at_ms`, `trigger`
  (`auto` · `manual`), `pre_tokens`, `post_tokens`, and `unseen` (it came behind, not in front since). Claude's
  level has only its own session's. `answer` (frictions29 #76): where its last answer leaves it, one line a driver
  keys on — `ready` · `working` · `blocked: <the card or edit that waits for you>`, then ` · context N %` once
  reported; null without a chat. `key_source_seen` (apibill): the last `apiKeySource` its chat's claude
  reported (`none` · `apiKeyHelper` · `ANTHROPIC_API_KEY` · `/login managed key`) — absent before one; Claude's
  level has only its own session's.
- **`notes`** (f24, `notes.rs`) — the front session's notes in the order pinned, where each is, never what it
  says: `id` (`note#4`), `path` (from the session's root), `line` (1-based), `kind` (`finding` the reviewer's ·
  `comment` yours · `claude` its Claude's), `sent` (not a draft `space v s` hasn't sent), `state` (`waiting` for
  Claude's answer · `answered` · `open`). Claude's level lists only the sent ones (`notes sent to you: …`) —
  what `reply_to` answers; `reply_to` on a draft is refused.
- **`limits`** (apibill, `limits.rs`) — the account's plan limits as any claude said them: each window known now
  by Claude Code's name (`five_hour`, `seven_day`, `seven_day_<family>`, `seven_day:<model_scoped name>`) →
  `percent` (whole), `resets_at` and `seen_at` (unix ms), `status` (`allowed` · `allowed_warning` · `rejected`,
  or null: `get_usage` doesn't say), `from` (`rate_limit_event` · `get_usage`). A window past its reset is left
  out. `{}` when nothing said.
- **`cards`** — every card: up front, stacked behind it, or parked in a session behind.
  - `whose`: `claude` when `Offer::claudes`, else `tarae`.
  - `kind`: the `What` variant in kebab case.
  - `title`.
  - `asks`: the title plus the question's `decided_lines` — the part drawn at full strength, not a hook's
    body, which `v` shows. Whole at the full level (JSON, text line, a watcher's `card-up`, which carries
    it as `asks` too): a driver reads all of a command it's asked to approve. Cut at Claude's level and in
    the inbox picker's one-row label.
  - `answers`: from a new `Offer::answers()`, which the hint row is drawn from too — a weighty card in front not
    seen whole has `v` first (its `y` waits until it is; frictions29 #69).
  - `settled`: in sight past `SETTLE`, so `y` would count.
  - `seen_whole`: `offer_read`.
  - `refused` (frictions29 #69): the front card's last key it didn't take, and why — what it says on itself
    (`offer_refused`; a `y` before it was seen whole: `y — see it whole first (v)`) — null until one, and again
    once the card changes.
  - `session` is the session a card is about — a bring card's the session it brings (frictions29 #70) — and
    `shown_in` where it stands when that's another (else null).
  - `waited_ms` / `in_sight_ms`: from `since` and `offer_seen`.
  - `at`: its rect, as drawn.
  - (a run, in `runs`: `ran` — cardburden 4 — where Claude's command ran, from its run's record: `where`
    (`sandbox`, `sandbox own + Docker socket`, `outside`), `past` (past the sandbox), `why`, and `warned` — each
    warning's `id`, `said`, `whose`; `null` for another's run or one in line.)
  - `same_as` (cardburden 3): a run card the same as one answered before since tarae started (`sinceyes.rs`)
    — `{ card, answer, at, unchanged }`: that card's id, its answer (`y` `a` `A` `n`) and when, and whether
    nothing changed since; `null` otherwise.
  - `held` (frictions25 R8): why its yes isn't among its answers (`Offer.held` — high-risk changes no one has
    looked at), `null` when it isn't held; the text form says it on the card's line, `y held: …`. `aside`: it
    stepped aside for what its `h` went to (`Offer.aside`) — `aside, C-q answers it` on the line. `waits` (V 9):
    `keys quiet` while it waits for the keys to go quiet (`keys.waiting` names it), else `null` — `waits for the keys
    quiet, C-q answers it` on the line.
  - `text` (cardburden 5, full level only): the card's text whole, `[{ line, role }]` — `role` as the card draws
    the line: `warning` (what yes risks, the warning colour: `Offer.warning` and the data's `warnings`) over
    `diff` (what changed since a `y`, `sinceyes`) over `decided` (`decided_lines`, full strength) — the rest
    `said` (faint). A driver tells a card's weight from it without reading the frame. Left out at Claude's
    level (its `asks` stays cut); the text form keeps its one line.
  - `warnings` (cardburden 5): the warnings the card says (`cardwarn.rs`), by id; left out when none. Claude's
    level has them, but not a plugin's, `git-trust`, `claude-settings` or `repo-trust` card's.
- **`inbox`** (2026-10-05, `inbox.rs`) — what waits for the user, every session's, oldest first: the cards above
  that wait (not downloading) and the edits waiting for their review (`edit#s0:review` up in its session,
  `edit#s1:<request>` queued), reads queued for their card, a change set's files; after all that asks, Claude's
  runs gone quiet (`silent-run`) and a reviewer that ended in a session behind (`reviewer#s1`, kind `reviewer`,
  up `ended`: its findings and `space v r`, or why it stopped — gone once that session is in front). Built from
  `cards` in the same call, so the two can't disagree; `C-q` goes to its first. Those two are news, not
  questions (`Entry::asks_answer`): the status line's `▲n waiting` and a card's count leave them out.
- **`toasts`** — the card's rows, and where they are: on a card, folded into the status line, or waiting for a
  card (U7).
- **`panels`** — for the front session. Each holds what it shows and its *visible* rows (id + label +
  selected), with totals. The visible rows come from the frame (`Screen.sidebar`, `review_rail`, picker
  `view`).
  - `code`: panes (id, path, lines on screen, focused, cursor line), the follow or replay stop.
  - `side`: tab, focused, rows.
  - `review`: base, files, viewed n/m, the file in hand, the change in hand i/n.
  - `edit_review`: `id` (which review: the next edit to the same file is a new one, #216), file, change i/n,
    undecided count, the first line of the reason (always Claude's — its `by`
    went with `space c` in `v` 2), `warnings` — what the change warns of, by id (`edit-protected`, cardburden
    4b).
  - `picker`: title, query (full level), rows, total, loading or searching.
  - `slot`: tabs with their badges, which is in front, has the keys.
  - `chat`: open, focused, minimized, busy, queued count, conversation, plan step, last call. Draft length
    (full level adds its text).
  - `follow`: on, paused, the gaze.
  - Where the keys are is one value, the session's `at` (panes P2a, `session::At`: the code, the chat, the
    terminal's program, the slot, a group's list, a plugin's panel, the side panel, the review's rail): `side`'s,
    `review`'s and `chat`'s `focused` and `slot`'s has-the-keys read it, so at most one is true — the one `keys`
    names. A code pane's `focused` is the pane the code means (`Session.focus`), wherever the keys are.
    Panes P2b: a floating pane with the keys — a card (held, or taking its answers), the picker, a prompt,
    which-key — is `at` itself (`At::Float(pane id)`), the docked pane it rose over `Session.under`, the keys' again
    once it goes (`Editor::float_sync`, every key and event); a linked group's list (panes P3 §2.1, `Session.groups` —
    the tests', the task's places, a preflight's steps, `:procs`' table) is `At::List(pane id)`, `keys.region`
    `list:<kind>` (`list:tests`, `list:task`, `list:preflight`, `list:procs`, `grouplist::key`), its detail the slot's
    output document (`At::Slot`, P2c; `:procs`' since P3 step 4, `output:procs`).
  - `panes` (per session, panes P2b): every leaf of its tree in the tree's order — `{id: pane#N, shows:
    code:src/a.rs | side | rail | slot | chat, rect, at}` (rects the front's, as the last frame laid them out), the
    members of a group after its host while it lays them out (`list:tests`, `detail:tests`, `list:task` … — the host's keys the
    detail's), then the floating ones up (`card#N`, `picker`, `prompt`, `which-key`, `float: true`); `at` is true on
    exactly the one with the keys. A group docked beside the code (panes P3 step 6) is a leaf of its own, `shows:
    group:<kind>` with `place: left | right | top`, its members after it with `group: pane#N` (its host); while it
    has the keys `keys.region` is its member's as in the slot (`list:tests`, `output:tests`).
- **`runs`** — task, tests, preflight steps, Claude's runs, and queued ones (`runs_next`).
  - State: `queued` · `running` · `waiting` (with `Waiting::shown`) · `quiet` (cardburden 5: a run of
    Claude's printing nothing, held from the stuck card — under its own time limit still to come, or its work
    maybe elsewhere and not yet `llm.stuck-after-outside` quiet: `stuck::held`) · `passed` · `failed` ·
    `stopped`.
  - `since`.
  - A run id, new: a sequence number given with its `RunTrace`.
- **`procs`** (#143, 2026-10-05) — what `:procs` lists, as of its last look (`looked_ms` ago: every 30 s, each
  second while it's open): each row's pid, parent, depth, kind, title, what for, session, `up_s`, CPU, memory
  (footprint, else resident), resident, `zombie` (ended, not reaped), `flagged`; and `checks`, the self-checks
  holding now. Full level only — Claude has its `procs` tool. The text form lists them a step in under what
  started them.

## Element ids

Short, so they can be read in prose and typed. Unique within one tarae process.

| Kind | Id | From |
|---|---|---|
| card | `card#12` | `Offer.id` (`offer_seq`) |
| toast | `toast#311` | its index in `Editor.messages` (`:messages`) |
| session | `s1` | `SessionId` |
| pane | `pane#3` | `PaneId` — one counter for every pane: the code's, the surfaces', the tests' two, a floating one's |
| run | `run#7` | new sequence, given with the `RunTrace` |
| Claude's tool call | `call#5` | new per-turn index (claude's `toolu_…` ids are too long to read) |
| note | `note#4` | `notes` ids |
| panel | `chat`, `side`, `git`, `review`, `tests`, `task`, `terminal`, `picker`, `prompt` | singletons of a session |
| panel row | `git:src/a.rs`, `review:src/a.rs`, `tests:editor::tests::x`, `list:src/a.rs:12`, `files:src/` | **its content key, never its index** (indexes move as lists change) |

- **Session-qualified ids:** panels and rows are per session. `s2/git:src/a.rs` names another session's;
  unqualified means the front session, and for Claude its own session.
- **Gone elements:** a small ring (the last 200) remembers what went and how: answered `y`, declined, lost,
  replaced, expired. So a link or a question about `card#12` after it went still resolves ("answered `y` at
  14:02").
- **`:restart`:** it `exec`s the new binary in the same process (`restart.rs`), so the pid stays and the
  counters start again. `tarae.boot` (random per start) tells a driver its ids no longer mean the same
  elements.

## Versioning

- `v` is the schema version.
- Adding a field doesn't change it. Readers ignore fields they don't know.
- Removing or renaming a field, or changing what one means, bumps `v`. Each bump and its reason is on `uistate::V`
  — now 11: the chat's `view` `line` / `lines` count its transcript's lines, a paragraph each (panes P2c).
- The text form isn't a contract (people and Claude read it). The JSON is.
- A golden test pins the JSON of one fixed scene (two sessions, a card, a review, a run), the way the
  `term_snapshots` pin frames. A change to the shape shows up as a diff in review.

## How it's served

### When a snapshot is taken

**A snapshot is the state as of a drawn frame.** A request goes to the main loop as an `Event::Job`. It
doesn't set `unchanged`, so the loop draws. When nothing changed, `screen::Screen::present` writes nothing to
the terminal. The answer is built in `render`'s after-frame hook, beside `seeing::after_frame`. So a reply
never describes a state the user hasn't seen, and `frame` lets a driver tell two snapshots apart.

`after_input: N` holds the reply until `input_seq ≥ N` and a frame after that. That replaces sleeping after
sending keys. For example: read `input` = 1033, send ` W`, then ask with `after_input: 1035`. The reply is about
what those two keys did. (crossterm makes one key event per key or escape sequence, and one per paste;
`input` also counts a mouse press and a drag, not a release or the wheel over a panel.)

### Tests (in-process)

`ed.ui_state(Level::Full)` after `term::render`, in the `run(...)` key-sequence tests and the unit tests. The
e2e harness gets `Term::state()` / `wait_state(|s| …)` over the socket, and screen greps move to state where
they check focus, cards or runs. Snapshots keep checking the look.

### Outside drivers: a socket per tarae

- **Where:** `$state/ui/<pid>.sock` (`config::state_dir`). The folder is 0700 and the socket 0600: bound
  under umask 077, then checked.
- **Who:** every connection's peer uid is checked (`getpeereid`) and must be ours. Its peer pid
  (`LOCAL_PEERPID` on macOS, `SO_PEERCRED` on Linux) is refused when it's under one of Claude's processes or
  runs, which `procs` already knows. Claude has its own tool (below), at its own level.
- **Claude can't reach it:** Seatbelt only lets Claude's runs connect to Unix sockets in the project and
  their own `RunDir`. A test under the real profile pins that, as
  `what_the_user_granted_opens_under_the_real_profile` does for grants.
- **One per process.** A listener thread sits blocked in `accept`: no wakes, no CPU.
- **Leftover sockets:** tarae removes only the socket at its own pid's path, which can only be a dead
  process's leftover now that the pid is ours. It never removes another's — the `agent.rs` lock-file lesson.
- **Too long a path:** over `sun_path`'s 104 bytes (108 on Linux) the socket is bound from its folder — at
  startup, before any thread runs, as the umask is set — and `tarae state` connects the same way. (Built so in
  phase 1: an e2e sandbox's state folder is past 104 bytes; with no socket there, nothing would test it.)
  Other clients use the path as it is.
- **Protocol:** one JSON line per request. Snapshots and events are one JSON value per line:
  - `{"get":"state"}`
  - `{"get":"state","after_input":1035}`
  - `{"watch":["keys","cards","runs","turns","toasts","sessions"]}`
- **Finding it:** tarae sets `TARAE_UI=<socket>` in the terminal panel's and the user's tasks' environment
  (not Claude's runs), so a script run inside tarae finds its own.

### `tarae state`

The same binary, a subcommand:

```sh
tarae state                      # text form of the one tarae whose session root holds the cwd ($TARAE_UI first)
tarae state --json               # the JSON
tarae state --pid 4182           # that one; with several and no match: lists them (pid, front session, root)
tarae state --after-input 1035   # waits for the keys sent to be handled and drawn
tarae state --watch cards,keys   # events, one per line, until interrupted
tarae state --chat linux7        # that session's last turn as its chat shows it (an id — s1 — or a name)
tarae state --review linux7      # that session's reviewer: its verdict and findings whole
tarae state --frame f.vt         # the frame drawn, as the terminal got it, into f.vt
cargo xtask shot --live out.png  # that frame as a picture (--pid N, else $TARAE_UI's tarae)
```

This is what an agent in zellij uses before sending keys: `tarae state` shows `keys → chat (text)`, so it
doesn't send `y`.

`--chat [SESSION]` (#204, 2026-10-13) answers instead of the state: one session's last turn, from the last
message sent on — the tool rows as the chat draws them (`◦ read  src/a.rs  L1–40`), then the answer; the
session in front when none is named. Over the socket: `"chat": "s1"` (`""` for the one in front) in the
`state` request; the JSON is `{"v", "chat": {session, name, doing, turn: [{role, text}]}}`, pinned by
`src/snapshots/ui_chat.json`. It's the user's level only: it isn't part of `UiState`, so Claude's level —
another session as counts — never carries it.

`--review [SESSION]` (#213) is the same for a session's reviewer, as `space v r` reads it: `"review": "s1"` in
the `state` request; the JSON is `{"v", "review": {session, name, reviewer, verdict, findings: [{path, line,
end, high, answered, thread: [{by, text}]}]}}` — `reviewer` is the state's `reviewer` row (null: none yet),
`verdict` whole, `high` null when its review isn't the one open. The user's level only, as `--chat`. Beside
them, `high_unseen` (#214): the high-risk changes of the session's open review no one has looked at yet —
`[{path, line, end, why, since, text}]` (`since`: lines since an earlier look; `text`: the lines where the file is
open, else null), null with no review open. A driver reads them to prepare a look; only a person's key in review
mode (`space v h`, `space v H`) marks one.

`--frame FILE` (#209) asks `{"get":"frame"}`: the last frame as the terminal got it — `{"v", "frame": {cols,
rows, frames, input, bytes}}`, `after_input` as for the state, always JSON. `cargo xtask shot --live out.png`
asks the same and draws it with the shot's renderer, so a driver sees colours, alignment and cards, not only
text. It's the shot's fonts and background (a transparent theme's ground is the shot's), with no hardware
cursor. The user's level only: a frame shows every session's tabs and cards, and Claude's own `screen` stays a
card.

### tarae's Claude: the `ui` tool and the context line

- **The `ui` tool** returns the text form at Claude's level, with ids. It's read-only
  (`readOnlyHint`), answered at once, and asks no card (see "Privacy" for why that's safe; decided).
- **The context line.** Every message's `<editor-context>` gains one line, and only when it says something:
  `keys: card#12 · cards up: card#12 run "cargo test -p tarae" (claude) · toast#311 warning "…"`. Most "why did
  this come up" questions are then answered without a tool call. Claude's system prompt gets one sentence
  on citing ids.
- **Pointing (c)** adds `<pointed id="card#12">…that element's state…</pointed>`.

## Events

A snapshot is one question. An event stream tells you about change without polling.

| Event | Carries | Emitted by |
|---|---|---|
| `keys` | old → new `keys` | diff |
| `card-up` | the card | diff |
| `card-gone` | id, how: `answered y` · `declined` · `lost` · `replaced` · `turn-ended` | explicit, at `offer_finished`/`offer_dismiss`/`asking_sync`/`drop_cards` (a diff can't tell how) |
| `session` | id, doing old → new, `reviewer` (its reviewer reading: `idle → idle · reviewing`) | diff |
| `reviewer` | session, `reviewer_run` (above), `was`: its state before (`reading` → `done`); only when it starts, ends or stops | diff |
| `review` | session, `what`: `began` (kind) · `decided` (an edit review's change answered: file, its number in the review, outcome) · `ended` (changes, `active_ms`, outcomes counted) | explicit, at `reviews.rs`'s frame and ends |
| `turn` | session, `started` / `ended` (`chat` Done), calls, edits | explicit |
| `run` | id, `queued` · `started` · `waiting <what>` · `ended <how> <took>` | explicit, beside the `RunTrace` steps phase 0 already logs (and `Waiting::line`) |
| `toast` | the toast | diff |
| `procs` | processes `started`, `ended`; checks `found`, `over` — only when a look found something changed | explicit, at each `:procs` look (`procs_said`) |
| `limits` (kind `usage`) | `limits` (above) — only when a reading changed what's known | explicit, at `limits::seen` |

- **The diff:** after each batch of events, `uistate` compares a small summary (keys, card ids and revs,
  session `doing`, run states, toast ids) with the last one sent. That happens only while someone watches:
  `Editor.ui_watch: Option<…>`, so with no watcher it's one branch.
- **No wakes.** The diff runs after batches tarae handles anyway. A watcher never makes tarae wake or draw.
- **Each event carries `frame`, `input` and `said`.** `said` is one plain-words line ("Claude asks to run
  cargo test — y n"). The text CLI prints it, and the screen reader will speak it.
- **A slow reader** is dropped after 256 unread events, and told so on its last line. It reconnects and reads
  a snapshot. Gaps show in `frame`.

## Given and cited (the user's idea)

The rule: **accent only for what asks**. Given and cited marks are faint (`ui.linenr`), never the accent.

### (a) Given

A turn's *given* set holds the ids that went to Claude: from the context line, a `<pointed>`, or a `ui`
answer. While that turn and its answer are current, a given element carries a faint `◦`:

- a card: at the right end of its title row, before `1/N`;
- a panel row: in its right padding;
- a pane: in its path bar.

The marks go at the next question. Toasts carry none: they come and go too fast for a mark to mean anything.

### (b) Cited

`card#12`, `git:src/a.rs` and the like in Claude's answer become links, parsed beside `chat::refs_in` and drawn
like an answer's places (underlined, dim). The element's mark turns to `•`.

A click or `C-]` brings the element into sight **without changing what the keys answer**:
- another session comes to the front;
- a side tab unfolds and the row scrolls into view;
- a stacked card's title row flashes in place. It doesn't come forward: `offer_to_front` would change what `y`
  answers.

The flash is ~1.2 s of `ui.menu.selected` behind the element (the selected-row background, without the `▎`).
A gone element gives a toast naming how it went, from the ring.

### (c) Pointing

A chat key starts pointing:
- every addressable element on screen gets a two-letter label, in the `gw` look (`labels.rs`);
- typing a label (or clicking an element) puts its id as a chip in the chat input;
- the message carries `<pointed>`.

The key: `C-o` in the chat, `space @` (`default_keys.toml`) from the code — see "Phase 3".

All three are new screen elements, so each is committed only after a `cargo xtask shot`. The shots cover
meok and hanji, a narrow screen, the stacked-card case, and a mark next to the `1/N` counter.

## Cost

- **Nobody asks:** one listener thread blocked in `accept`. No timers, frames, wakes or disk writes. The
  socket is made once at startup.
- **A snapshot:** one frame drawn, usually writing nothing to the terminal, plus building the state. That's
  copying a few dozen strings, and JSON for drivers.
- **A watcher:** a summary compare per batch, after work done anyway.
- **The e2e check:** `an_idle_editor_draws_nothing_and_sleeps` gets two more cases — the socket listening, and
  a watcher connected. Both must draw nothing and keep the context switches flat.

## Privacy

- **What's in it at all:** labels and structure, never contents. A card's `asks` is what the card shows at
  full strength (a command, a path, a setting), not a hook's body. No code text, terminal output, previews,
  diffs or chat bodies. No env values, ever. One exception, at the full level only (cardburden 5): a card's
  `text` — what the card itself shows the user, its diff since a `y` among it, so a driver answering it reads
  what the user would; never at Claude's level.
- **Drivers (full level):** same uid only, and not from under Claude's processes. They also get unsent text
  (the chat draft, the prompt and picker query). Any process of the user's could read the files and saved
  chats anyway; what it can't read elsewhere is unsent text and the screen's structure. Holding that back
  from same-user processes isn't worth a setting.
- **Claude's level** is a strict subset:
  - its own session in detail; other sessions as name + `doing` + counts, never their cards' or chats'
    text (a session's Claude doesn't read another session's conversation);
  - no unsent text: what the user hasn't sent isn't Claude's;
  - no terminal panel beyond "open, focused, program running / exited N";
  - a plugin's cards by title only.

  It's less than the `screen` tool sends, which is why it needs no card of its own.

## What it is not

- **Not a way for Claude to act.** No `ui` call answers, focuses, scrolls or presses. A cited link moves
  things only when the user clicks it.
- **Remote control only as keys, checked where they land.** Drivers act with keys — through the terminal, or as
  a shuttle over the socket ("Shuttle keys") — so what they do goes through the UX a person uses, and any
  friction shows up as friction. No actions (`answer card N`), no clicks.
- **Not a screen dump.** Contents stay on the screen; `screen` and zellij dumps remain for that.
- **Not a plugin API.** Plugins have their own (0.2). A read-only state permission could come later.

**Should drivers get a narrow "answer card N"?** No: keys, each saying where it must land — shuttle keys,
below.

## Phases

### Phase 1 — where the keys go (the smallest step that pays off alone) — built 2026-10-04

Built as below, with two changes: Claude's level waits for phase 3 (its `ui` tool is its only reader), and
the long-path rule above. What it found and what it cost: see "Phase 1, measured".

- **`Editor::keys_to()`:** finer than `chrome::Keys` (`takes`, `pending`, `drawn`, `under`). `keys_at` and
  the status line read it too, so TERMINAL can say whether `i` types in it or the program has the keys.
- **The routing test:** a matrix of states (mode × prompt × picker × chat focused/minimized/draft ×
  card/`C-q` × terminal program/panel × side/review focus × pending prefix) times a probe key. Under
  `cfg(test)`, each handler in `handle_key`'s chain records that it took the key, and that must be what
  `keys_to()` said. Each case is broken once to see it fail.
- **`Offer::answers()`:** shared with the card's hint row.
- **`UiState`:** keys, mode, prompt, sessions, cards (up, stacked, parked), toasts, `input`, `frame`; JSON
  plus text.
- **Socket:** snapshots only, `after_input`, peer checks. `tarae state [--json] [--pid] [--after-input]`.
- **Tests:**
  - the four slips, each as a test where the state tells the truth (`takes: text` under a `y`;
    `terminal-program`; `drawn: false`; a lost card in the ring and in `card-gone` once phase 2 adds it);
  - the golden JSON;
  - the idle e2e with the socket;
  - the sandbox profile refusing the socket.

**Size:** about queue phase 0's (974 lines added): ~1,000–1,300 lines with tests. The routing test is the
biggest single piece.

**Why it pays off alone:** a driver checks `keys` before sending and waits on `input` instead of sleeping.
The routing test pins today's dispatch, and slip 3 (` W` into an input not on screen) suggests it will find
at least one case where the keys go somewhere not drawn.

**Risks:**
- `handle_key`'s chain has order rules collected over many fixes (#24, #61, #70, B1); `keys_to()` has to match
  them, not simplify them. That's what the matrix is for.
- The socket on Linux: `SO_PEERCRED` and the path differ; checked with `cargo xtask linux test`.

### Phase 2 — panels, runs, events — built 2026-10-04

Built as below (`uistate/panels.rs`, `uistate/watch.rs`), with these choices:
- **Run events come from the run's trace**: every `RunTrace` step (`queued`, `saved`, `spawned`, `waited for
  cargo's lock`, `exit 0` …) is a `run` event, kept on the main loop's thread only while someone watches. A
  run's id is its trace's, from its ask to its end — a run waiting in line carries its trace, so it keeps its
  id when it starts.
- **The gone ring holds cards** (how each went: `answered y`, `dropped — its turn ended`, `lost`, else `gone`).
  A run that ended stays in `runs` until another takes its panel.
- **Every event goes out at the end of the batch of events it came in** (explicit ones too): after the frame
  that showed it, stamped with that frame — or, when the batch drew none (a card parked in a session behind),
  at once, stamped with the last frame drawn (`uistate::undrawn`; watchlate, 2026-10-19: told only after
  frames, a grant card behind was heard 20 minutes late, at the person's next key).
- **A panel under a picker lists no rows** (`covered_by_picker`): the side panel's and the rail's rows were
  drawn, then covered — the snapshot test caught the state listing them.
- `tarae state --watch [kinds]` prints the `said` lines (`--json`: the events); a watch's first line is
  `watching`.

- `panels` with visible rows and content-keyed row ids; `runs` with run ids; the gone ring.
- `watch`: the diff plus the explicit `card-gone` / `turn` / `run` events, with `said`.
- The harness's `wait_state`. The flakiest screen greps (focus, cards, runs) move to state.

**Size:** ~1,300–1,700 lines.

**Risks:**
- A panel's rows drawn one way and listed another. Mitigation: the visible rows only ever come from what
  `term.rs` recorded in `editor.screen`. A snapshot test checks that every visible row's label is on the
  frame.
- Event volume while Claude streams. The summary excludes chat text, so a stream of words is no events.

### Phase 3 — Claude — built 2026-10-05

- The `ui` tool at Claude's level, the context line, the system-prompt sentence.
- Cited ids as links (jump and flash, the ring for gone elements); pointing (labels, then a chip, then
  `<pointed>`); given and cited marks.

Built as below (`uistate/claude.rs`, `uistate/point.rs`), with these choices:
- **Claude's level** is the full state filtered (`ui_state_claude`), never built apart: no prompt or picker
  query, no chat draft; other sessions as id, name, `doing`, card count; only its own session's cards, runs
  and gone cards (the ring now keeps each card's session); a plugin's card by title. A session behind gets
  its parked cards and runs only — nothing of the screen is its. Its text form (`text_claude`) drops pid,
  frame and input, and adds the panes and, per panel, the rows on screen by id (`rows git: …`, 12 at most).
- **Given** = the context line's ids, what was pointed at, and what a `ui` answer names one by one (cards,
  toasts, runs, panes, the row in hand) — not the `rows …` lists: every row on screen marked after one call
  would be noise. **Cited** = the ids in this turn's answer, read again as it streams and at its end.
- **Links**: ids are parsed beside `refs_in` (`ids_in`: not inside a word, a path or a URL; a `path:line`
  inside a row id is the id's). `C-]` walks places and ids in the order written (`chat::Hop`). An id that was
  never an element here (`ui_known`: not up, parked, in the ring, said, run, a view or a note) is drawn faint,
  not underlined; followed, it says `no such element` (an info toast, no flash). Rows are taken as written and
  found when followed (`… isn't in the git tab now`).
- **Flash**: the selected row's colour behind the element for `FLASH` (1.2 s) — a row that's already the
  selected one flashes in the labels' accent wash instead (else nothing would show). One wake-up at its end
  (`tick_wanted`), nothing between.
- **Marks**: a card's at its title row's end before `1/N` (only where the row has room — the card never
  widens for it), a stacked card's at its peek row's end, a row's in its right padding, the pane's after the
  file in the path bar. Only the front session's chat's, only this turn's.
- **Pointing's key** (decided here): `C-o` in the chat — unused there and in Helix's insert mode (Helix's
  `C-o` is normal mode's jump back, which the chat input doesn't have); "o" for *object* / *on screen*.
  From the code, `space @` — unbound in Helix's space menu and tarae's; `@` reads as "mention". Both run
  `chat_point` (`:chat-point` too). While labels are up every key is pointing's (`keys_to` region `point`,
  takes `label`, the status line `POINT · esc stop pointing`); after the first letter only the matching labels
  stay, their second letter shown, as `gw` does. Labels sit on margins, never over words: a card's on its
  margin and `▎`, a row's in its left padding, a pane's on its top line past its path (P3 step 6b), a slot tab's on
  the rule under it.

**Size:** ~1,200–1,600 lines, plus a design pass with shots for each of the three looks. Built: ~1,240 lines
of code, ~525 of tests.

**Risks:**
- Marks as noise. They're faint, only for the current turn, and none on toasts. Look at them in a shot of a
  busy screen before committing.
- Claude citing ids it made up. A link that resolves to nothing says "no such element" faintly. No flash.
- Context cost: one line, only when non-trivial.

### Phase 4 — speaking dropped (user decision, 2026-10-04); guarded keys back as shuttle keys (2026-10-19)

- **Speaking** isn't built: no `ui.speak`, nothing sent to the platform's speech. Events keep their `said`
  line (the text CLI prints it), so a screen reader could start from it later.
- **Guarded keys** came back once phases 1–2 showed that `after_input` plus checking `keys` before sending
  isn't enough: three slips in a week, each the gap between the check and the key (a `y` answering the card
  that came in front as it was typed, a paste before the commit buffer opened, a toggle sent to a state the
  script never saw). Built as shuttle keys — below.

### Shuttle keys — phase 1 built (2026-10-19, `shuttle.rs`)

A **shuttle** is a program the user runs that works tarae from outside — an assistant, a test script. Over the
same socket, one connection carries one shuttle: a hello, then a step a line, each answered.

```json
{"shuttle": {"name": "claude-code", "for": 51234}}
{"keys": "space v m", "expect": {"keys": {"to": "pane#4", "pending": ""}, "review:src/a.rs": {"viewed": false}}}
{"text": "fix: …", "expect": {"keys": {"to": "pane#7", "takes": "text"}, "pane#7": {"path": "COMMIT_EDITMSG"}}}
{"resume": true}
```

- **Checked and handled in one step.** The socket's thread sends the step to the main loop as one job: the
  guards, then `expect` compared with what the state would say now (`keys` with `keys_to`, any other id with that
  element's fields — a card's `rev`, a pane's `path` and `version`, a review row's `viewed` and `in_hand`), then
  the keys in one by one, each an event of its own as a person's are (what runs after every event runs between
  them), each after the first still going where the step began. No key, job or
  timer runs between the check and the key. An empty `expect`, or one without `keys.to`, is refused: every step
  says where it goes. `text` is one paste with a person's paste rules (a `:` line opens the command line, never
  runs).
- **The answers.** Landed (after the frame that draws it): `{"landed": "card#12", "took": "card", "input": …,
  "frame": …, "said": "card#12 went: answered y by shuttle claude-code"}`. A key the card swallowed is never
  landed: `{"took": "card", "taken": false, "said": …}`. Refused, nothing done: `{"refused": reason, "why": …,
  "keys": what has them now}` — `expect`, `moved` (a sequence stopped partway; `landed_keys`), `paused`
  (`retry_after_ms` on a resume), `weighty`, `never`, `config`, `off`, `ended`; at hello also `name`, `for`, `peer`, `another`,
  `no`.
- **Who.** `ui.shuttle-keys` (`off` · `light` · `weighty`), your own config only (`trust::YOURS_ONLY`). A peer
  of this user, not under Claude's processes nor under what runs the project's code (tasks, tests, servers,
  debuggers, plugins — `TARAE_UI` is in your tasks; the terminal panel is yours); never a `for` that is tarae or
  runs it (its shell, its terminal, the login session), whoever asks; under tarae only from its terminal panel, its
  `for` at or under the panel's shell (never a child no list knows of — a hook, a formatter). A peer whose parent
  is pid 1 — one that daemonized or forked twice, re-parented to launchd/init — is refused ("not started from a
  shell or a program you run"). What the parent chain can't guarantee: "under" is as far as it shows at hello. A
  process that leaves it another way isn't seen — re-parented to a subreaper (systemd's user manager, some
  terminals and multiplexers) rather than pid 1, or started by something Claude or a task asked another program
  to start (a launch agent, `at`, a terminal's new window). For those the card is the guard: it names the
  executable, which the program can't dress up. Without a `for` the
  grant is the connecting process's own (each `tarae keys` asked for on its own, until it exits); `--for` widens it
  to that process and every program it starts — the card then names both by their executables and says, in the
  warning colour, that every program the `for` process starts may type until it ends (a `for` that is an
  interactive shell isn't told apart: the warning is what you read). The card is weighty — when it comes is the
  program's, so a `y` typed as it came isn't one — and a hello whose card goes unanswered (dropped, a restart)
  hears so at once. The card shows the process's name with only the characters a
  shuttle's name may have, its executable's path (cut from the left), and its start dated when it isn't today. The first hello under a `for`
  process (by pid and start) brings the card — `y` until that process ends, kept in memory only; its end is
  waited on with no polling. One shuttle at a time.
- **Your key wins.** Any key, click or paste of yours pauses it (said once in `:messages`, the chip says
  `paused`); it goes on when it asks after you've been idle 5 s. `:shuttle-end`, or a click on its chip, cuts it
  off with its grant.
- **What stays yours.** Phase 1 lets a shuttle answer light cards and every `n` — checked against every letter
  the card acts on (`Offer::acts_on`, the one list `offer_key` takes: `Y` on a run card's group, `s`, `$` …), on the
  card the key routes to or the front one whenever it has the keys; a weighty card's answers wait for
  phase 2 (`read`: a digest of the card's text the shuttle computes), and those that widen trust or outlive the
  session never (`A`, `s`, `N`, a project's or a repository's trust, Claude's settings, hooks, no-sandbox, the run
  outside, a made file kept, the shuttle's own card; `:set!`, `:set` of a risky or yours-only setting,
  `:sandbox-blocked` — refused where typed lines run, so a key bound to one or the palette runs it no more than
  Enter does). What only a person's look may do goes by `person_seq`, which counts only your input:
  follow's pause, a card's place among yours, an error read; a review's looks (`space v h`) refuse a shuttle.
  **tarae's config files** are yours to edit: a key or paste that would land in the buffer of a file in your
  config folder (`config.toml`, keys, themes — as written and its real path, read at hello) or of a
  `.tarae.toml` / `.tarae.local.toml` (by name, anywhere) is refused (`config: yours to edit`) — any key there,
  moving ones too: the person moves the keys away; and where typed lines run, `:config-open`, `:e`/`:o`/a split
  naming one (`path:12` too), `:w` to one or of one, `:wa` with one changed. What these guard is what a
  shuttle's *keys* reach in tarae — tarae isn't the tool that edits your config, answers trust or saves a setting
  for a program. What they can't: the program runs as you, with your permissions, as everything you start does —
  it can write those files itself (`ui.shuttle-keys` included), tarae's state, or run the commands itself; the
  git tab's and a review's keys on a config file's changes aren't buffer keys and aren't refused. The grant card
  is the boundary: say `y` only to a program you'd let do that anyway.
- **Seen and recorded.** `◦ shuttle claude-code` on the status line, its step in the accent a moment after each,
  last on the left (nothing beside it moves); `keys.shuttle` in the state (Claude's level: `connected` — its name
  taken out of toasts and the gone ring too, `shuttle::for_claude`); `by=shuttle:<name>` in the
  card line and `by` in the record; `By::Shuttle` in the feature counts.
- **Tests**: the routing matrix's second entrance (every scene × probe as a step lands where a person's key would,
  refused written for another place — broken once), the three slips as key-sequence tests, a simulation of steps
  among the person's keys, a card asked again and the process ending, the socket, and the idle test with a
  shuttle connected and paused.

## What to measure

| Measurement | How |
|---|---|
| `ui_state` build time on a busy scene (3 sessions, 3 cards, a review open, a run waiting) | A `perf_…` number in `cargo perf`. A budget only after the first measurement — expected tens of µs |
| Size | JSON bytes (drivers) and the text form's length (Claude). Target: Claude's text ≤ ~1.5 KB on that scene |
| Round trip | `tarae state` → reply, frame included: median in e2e |
| Idle | 0 frames and flat context switches, with the socket and with a watcher (the extended idle test) |
| Keystroke → frame with a watcher connected | Unchanged within noise (`cargo perf`'s key-to-frame number) |
| The outcome | The drive-through-tarae friction log: count "keys went somewhere else" slips before and after phase 1 |

## Phase 1, measured (2026-10-04, `perf` profile, this Mac)

| | |
|---|---|
| State built, busy scene (3 sessions, 3 cards, an edit's review, a task, toasts) | 2.9 µs; as JSON 34 µs; as text 8.5 µs (medians of 200) |
| Size, busy scene | JSON 2,154 B (~540 tokens); text 691 B (~173 tokens) — a token per 4 bytes, no tokenizer here |
| Round trip, frame included | socket 0.40 ms; `tarae state --json` 2.5 ms (its process start too); medians of 30 |
| Idle | 0 ms/s CPU (`e2e_idle_cpu`, the socket listening); the idle e2e: 0 frames and flat switches with the socket and a driver connected |
| Release binary | 9,113,936 → 9,180,272 B (+66 KB, +0.7%) |

What the routing test and the pulled-out answers found:
- An edit's review under a focused side panel had the keys (`y` accepted Claude's change) while the status line
  said OUTLINE/GIT.
- A side panel focused in insert mode: the status line said OUTLINE, the keys typed into the code.
- A prompt over the terminal's program: the status line said TERMINAL, the prompt had the keys.
- The terminal's program and tarae's keys on its panel were both TERMINAL (slip 2) — now `TERMINAL  i type in
  it` for the second.
- The close-a-reviewed-buffer card's hint row said `y Install · n Not now` (the download card's default) while
  its text said `y` declines Claude's change.
- The only state where the keys go somewhere not on screen is the chat minimized while Claude works (slip 3).

## Phase 2, measured (2026-10-04, `perf` profile, this Mac)

| | |
|---|---|
| Keystroke → frame, 200k lines | 0.211 ms; with a watcher connected 0.205 ms (noise) |
| Events while Claude streams (a word every 50 ms, in front) | 0 a second over 3 s — the summary has no chat text |
| Idle | 0 ms/s CPU with a watcher (`e2e_idle_cpu_watched`); the idle e2e: 0 frames, flat switches, the watcher told nothing |
| State built, busy scene (now with panels and runs) | 3.4 µs; JSON 41.8 µs, 2,659 B (~665 tokens); text 9.2 µs, 811 B (~203 tokens) |
| Round trip | socket 0.69 ms, `tarae state --json` 2.8 ms |
| Release binary | 9,329,424 → 9,428,608 B (+99 KB, +1.1%) |

## Phase 3, measured (2026-10-05, this Mac)

Tokens from claude's own `usage` (input + cache written + cache read of a request — the whole prompt), the same
one-word question in the same project, the kept phase-2 build against this one (`tests/live.rs`:
`chat_prompt_tokens*`, `chat_ui_call_tokens`):

| | |
|---|---|
| Fixed part every request carries (system prompt, tools' definitions) | 13,062 → 13,271 tokens (+209; budget +300) |
| The context line, one toast up | +26 tokens (13,071 → 13,306 with it, less the +209) |
| A `ui` call, small scene (the call and its 144-byte answer) | +116 tokens between its two requests |
| Busy scene (3 sessions, 3 cards, an edit's review, a task, toasts) | `ui` answer 678 B (~170 tokens); context line 252 B (~63) |
| Release binary | 9,362,624 → 9,428,784 B (+66 KB, +0.7%) |

## No silent state change (user decision, 2026-10-04)

Whatever changes state the user didn't see change, or refuses a key, says so where they're looking.
- **The minimized chat**: the first key that would type, edit or send there brings it back first
  (`chat::unfold_key`; `takes: "unfold"` in the state). A letter comes back typed, in sight (#151, 2026-10-05:
  a follow-up lost its first letter); space (the leader — slip 3), Enter and editing keys aren't, and that's
  said. Its own keys (Esc, `C-c` stop,
  `C-f` …) act as before. A paste into it brings it back with what was pasted.
- **Cards**: a key a card refuses — a `y` before its text was read, a key as it came up, one that isn't an
  answer, a click as it came up — is said on the card itself (a row in the warning colour over its answers,
  or its bar's text), and kept in `:messages`; it went to the status line, or nowhere. While a card holds the
  keys over its own text in its buffer, the keys that read it (`g e`, `G`, `j`/`k`, `C-d`…) move that buffer.
- **A paste in normal mode**: a `:` line opens the command line with it, not run (Enter runs it) — a pasted
  `:w` meant a command, and landed in a commit message's subject unsaid; making it text would have kept the
  slip, and running it would run what nobody saw. Any other paste outside insert mode is held, not put in
  (#124, 2026-10-05 — it went into the file with only a toast to say so): in the default register, `p` puts
  it after the selection, `P` before (`pasted 3 lines — held, not in the file: p puts it after …`).
- **Keys a panel has no use for** (the side panel, the review's list, `:procs`, the bottom slot, an edit's
  review, `C-` keys in the chat and the command line) are said; `f`/`r`/`"` waiting for a character show on
  the status line (`f…`). The routing matrix pins it: `no_key_is_refused_in_silence` (what's left does nothing
  on purpose — Esc with nothing to leave, `C-w` with nothing to delete, `j` at a list's end).

## Decided (user, 2026-10-04)

1. **`ui` tool:** asks no card — it's less than `screen` sends, and its own session only.
2. **The socket:** on by default — same uid, 0600, Claude's sandbox can't reach it, nothing idle.
3. **Guarded keys for drivers:** replaced by the user's decisions on shuttle keys (2026-10-19): keys only, no
   direct actions; `YOURS_ONLY`; a card per program; the person's key pauses it ("Shuttle keys").
4. **Pointing's key** in the chat: decided in phase 3 — `C-o` in the chat, `space @` from the code (see
   "Phase 3").
5. **Other sessions in Claude's view:** counts only (name, `doing`, how many cards), never their titles.

The format: JSON (JSON Lines for events) for drivers and scripts — `serde_json` is in already, no new
dependency, readable with `jq`; the compact text form for Claude, where tokens are the cost; the structs
themselves in tests. No binary format (a new dependency, and the size budget).
