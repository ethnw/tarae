# Async and queues in tarae

Design, 2026-10-04 (roadmap #6, widened). Status: proposal. Nothing here is built yet.

## Headline

tarae's main loop is in good shape. Its waits are rare and measured in milliseconds. The problem
is **heavy runs**: builds, checks, tests, perf. Today each session has its own single slot for
them, and nothing coordinates across sessions. The machine's real limits are one cargo
build-directory lock and the CPU. They're fought over invisibly by five sessions' runs plus five
rust-analyzers' `cargo check`s, while Claude's runs that hit a busy slot are refused, so Claude
polls.

The fix is **one small scheduler for heavy runs only**, called the *lineup*. It's shared by every
session. It holds runs back until their resources are free, ranks the user's runs above Claude's
and the front session above the ones behind, and joins runs that are already the same. It says
"queued, 2nd, behind X" to both the user and Claude. Languages declare their resources as pack
data. Interactive work stays as it is: latest-wins, never queued.

The first step pays off on its own, needs no scheduler, and is about a day of work. When cargo
prints `Blocking waiting for file lock`, show it as the run's state in the panel and in Claude's
report, and trace each run's phases in the log. The 37 invisible seconds become visible, and the
trace tells us where the other 36 went.

---

## 1. What runs where, today

### 1.1 The skeleton

- **One event channel.** It's an unbounded `mpsc` (`src/event.rs:62`). Keys, mouse, pastes and
  every job result arrive on it in FIFO order (`event.rs:16-27`). Results made for a session are
  tagged with it, and `with_session` swaps that session in to apply them (`event.rs:126-131`,
  `session.rs:464`).
- **The main loop** (`term.rs:148-210`):
  1. Draw a frame (`render`, then the cell diff in `screen.present`).
  2. Block on the channel.
  3. Handle the first event.
  4. Drain more events for at most 16 ms (`term.rs:199-208`).
  5. Draw again, but only if something changed.

  Every event also runs about 25 sync hooks (`editor.rs:1398-1420`): `schedule_parses`,
  `lsp_flush`, `git_schedule`, `disk_sync`, `saves_due`, `sessions_sync`, `edits::next` and so on.
  An idle tarae draws nothing and isn't woken (CLAUDE.md; e2e
  `an_idle_editor_draws_nothing_and_sleeps`).
- **`Jobs::spawn` makes one OS thread per job.** There's no pool and no limit
  (`event.rs:141-166`). The work runs there, and its result comes back as a closure applied on the
  main loop. There are 149 call sites in 43 files.
- **Long-lived threads** belong to their owners: input (`term.rs:225`), the config watch
  (`config.rs:506`), one git HEAD watch per session (`git.rs:63`, made in `session.rs` `session_add`),
  `tarae-procs` (`procs.rs:311`), the log (`log.rs:38`), the saver (`saver.rs:75`), the plugin host
  (`plugin/host.rs:290`), 3 per language server (`lsp.rs:589/601/622`), 2 per Claude process
  (`llm.rs:296/300`), 2 per task or test run (`task.rs:587/591`, `testing.rs:649/652`), 2 per terminal
  (`terminal.rs:255/296`), and the agent's websocket (`agent.rs:235/313/319`).

**Measured now** (2026-10-04 15:45, the user's tarae pid 58185, five sessions, `ps -M`):
**66 threads, 20 child processes**:

| Children | Count | Notes |
|---|---|---|
| claude | 6 | |
| rust-analyzer | 4 | 1.31, 0.95, 0.48, 1.33 GB RSS, about 4.1 GB together |
| taplo | 3 | |
| marksman | 1 | |
| node (yaml/bash servers) | 1 | |
| `log stream` (a sandbox watch) | 1 | |
| `sh` (a task) | 1 | |
| terminal's zsh | 1 | |
| `<defunct>` | 1 | Unreaped. Cause unknown and out of scope here. `:procs`' zombie check should name it. |

The children and their memory, not the threads, are where the weight is.

### 1.2 Inventory

Column meanings:

- **How**: *main* = on the main loop. *job* = `Jobs::spawn`. *thread* = a long-lived thread of its
  own. *proc* = a child process.
- **Cancel**: how it's stopped or made moot.
- **Order**: does it matter that results land in order, and how is that enforced.

| Work | How | Competes for | Cancel | Order | Shown |
|---|---|---|---|---|---|
| Keys → edit → frame | main | CPU of one core | — | strict (FIFO channel) | the frame. Key → frame 0.19 ms in 200k lines (`cargo perf`) |
| Frame output to the pty | main, `write_all` (`term.rs:171`) | the terminal's reading | — | strict | Only the changed cells go out (`screen.rs`). It was the only main-thread wait found in the Sept off-CPU pass. |
| Tree-sitter parse + injections | job per dirty doc (`editor.rs:2548-2566`) | CPU | latest wins: `syn.in_flight`, generation in `finish_parse`. Dormant docs aren't parsed (`trees_follow_sight`). | edits during a parse are reapplied | colours. One frame uncoloured after waking. |
| Indent's fresh tree | main, capped at 3 ms (`Syntax::fresh_tree`) | CPU | gives up after one timeout | — | — |
| LSP requests | the main loop pushes onto a channel. A writer thread serializes (`lsp.rs:589-600`, `notify_lazy`); a reader thread sends each message as a job (`lsp.rs:601-616`). | the server's own queue | late answers dropped by `Pending` (doc, version, cursor). ContentModified gets re-asked (`ask_again`). | per server, FIFO | per feature. Go-to timing in the log (N1 `Trace`). |
| LSP server start | **main**: `Command::spawn` (`lsp_editor.rs:570`, `lsp.rs:576`, posix_spawn, 0.15 ms). Root/PATH lookups are stats on main. | CPU, memory. RA's `Building compile-time-deps` runs cargo, so **the build-dir lock** (up to 16.1 s today, log 12:58:53). | `Client::drop` kills, reaped on a thread (`lsp.rs:553-560`). Rest/stop of sessions behind (`sessions_sync` → `lsp_rest`, `lsp_editor.rs:1947`). | — | progress in the status line, `:procs` |
| rust-analyzer's `cargo check` (flycheck) | inside RA, on our `didSave` | **the build-dir lock**, CPU | held while Claude builds, front session only (`saves_due`/`claude_building`, `lsp_editor.rs:653-679`) | — | progress, logged. **237 since 12:17 today, 54 of them ≥ 20 s, max 64.6 s.** |
| Inlay hints, signature, completion | 120 ms debounce job → request (`inlay_schedule`), one in flight (`signature_inflight`) | the server | version checks | latest wins | inline |
| Git gutter hunks | job, 80 ms debounce, one in flight (`git.rs:252-277`) | CPU | version + base `Arc::ptr_eq` | latest wins | gutter |
| Git HEAD watch | thread per session (`git.rs:62-101`), fswatch | — | ends when HEAD goes | — | branch, bases reloaded |
| Git tab actions | **per-session FIFO**, one at a time (`Session.git_queue`, `gitpanel.rs:656-680`) | `.git/index.lock` | — | strict FIFO | the tab. Hooks card (`hooks.rs`). |
| Git status/blame/branches | jobs (`gitpanel.rs`, `gitmenu.rs:486`, `editor.rs:3067`) | disk | latest lands (`git_reads`) | latest wins | the tab, blame |
| Save | **one saver thread**, FIFO (`saver.rs:7-10,70-99`), temp + F_FULLFSYNC + rename | disk flush (9 ms for a 270 KB file, `saver.rs:4`) | — | strict per file | "Saved …" toast |
| Waiting for saves | **main**, `flush_saves` waits up to `QUIT_WAIT` = 10 s (`saver.rs:117,198`) before every task (`task.rs:408,428`), test (`testing.rs:581,745`), preflight (`preflight.rs:355`), restart, agent diff, checkpoint step, and quit | — | — | — | nothing (the screen just holds) |
| Disk watch/reload | one job per round, parked in fswatch (`disk.rs:179-231`). The read and `Rope::from_str` happen there. | disk | `disk_poll_gen` and a `Waker` end the old round | rounds replace each other | toast, `:messages` |
| Task runs (user, Claude, preflight, restart build, rebase git, installs) | **one slot per session** (`Session.task_run`). `sh -c` spawned **on main with `pre_exec(setsid)`** (`task.rs:510`, `dap.rs:192-205`). A reader thread per stream (`task.rs:587-649`). | **CPU and the build-dir lock**, across all sessions, uncoordinated | `Run::drop` SIGTERMs the group (`task.rs:325-333`). A new user task **kills a running Claude run** ("another task took its place", `task.rs:523`). | `task_generation` drops stale lines | bottom-slot panel, toast. Claude gets `run_report`. |
| Test runs | one slot per session (`Session.test_run`), spawned on main with setsid (`testing.rs:597`) | same | the same, plus `test_generation` | — | test panel, `▲` at failures |
| Claude's `run` | `tools.rs:651`. Light checks happen inline. A read-only check runs as a job (`tools.rs:774`). A card goes up if needed. Then the guard job (`tools.rs:1270-1306`: probe, `sandbox::policy` with `cargo metadata`/`go env` cached per root by mtimes (`sandbox.rs:772-815`), log-stream `Watch` + 150 ms `WATCH_READY`), then `task_start_for` (`task.rs:418`). | as task runs | turn end makes it moot (each closure checks `c.busy()`). Dropped runs are answered. | — | run card, then the panel. **Refused while busy**: "already running … wait for it to finish, then run again" (`tools.rs:1063-1071`), "hasn't answered your last command yet" (`tools.rs:810-815`, `1328-1331`). |
| Claude's `run_tests` | find the target in a job (`tools.rs:261`), then guard, then `test_start_guarded` | as tests | the same | — | **refused while tests run** (`tools.rs:281-289`) |
| Claude's reads (Read/Grep/Glob outside the project) | job (`reads.rs`), then a card if needed, **one card at a time** (`Chat.reads` queue) | disk | — | FIFO | Read card |
| Claude's edits | `can_use_tool` → review queue (`Chat.edits.queue`, `edits::next` every event). The write happens on a worker (refactor `land`). | the user's attention | deny | FIFO | in-buffer review, change-set card |
| `:preflight` | **one `sh` script for all steps** (`preflight.rs:114-125`) in the task slot. Refused while a task runs (`preflight.rs:344-349`). | everything: fmt, clippy ×2, test ×2, perf | stop the task | steps in order inside the script | panel with step chips. **Today: 15m52s, 12m21s and 15m04s (log 13:55, 14:54, 15:19). "perf: not measured (machine busy: load 6.6)" once.** |
| perf's own guard | `xtask/src/perf.rs:259-270`: waits up to 90 s for load < 5, then "not measured" | — | — | — | its note in the preflight summary |
| Chat stream | claude process. `pump` thread + stderr thread (`llm.rs:290-330`); text batched every 25 ms | the network, Anthropic's side | `interrupt` control request, SIGKILL for once-asks | strict | chat |
| Chat stdin | **main**, `writeln!` + `flush` into the pipe (`chat.rs:670,984,1074,1082,1112`) | the pipe (64 KB, measured below) | — | strict | — |
| Claude spawn / prewarm | main, posix_spawn (`llm.rs:211-221`, `Warm::ensure`). The old spare is reaped on a thread. | CPU (cold start 8–10 s, all in the child) | kill | — | chat state |
| Plugins | **one host thread**, FIFO across plugins (`plugin/host.rs:282-307`). Fuel-budgeted. Outbox coalesces what comes while busy (`plugin/io.rs:20-22,69-83`). **4 MB backlog** back-pressure (`io.rs:51,101-109`). ≤ 16 programs/sockets each (`io.rs:53-55`). | CPU, the plugin's network/programs | fuel, `stop`, reload | per plugin FIFO | plugin panels |
| Search (`grep.rs`) | ripgrep crates in-process, ≤ 4 threads (`grep.rs:19,136`), on a job | disk, CPU | `AtomicU64` generation asked per file and per match (`editor.rs:2085-2131`), 50 ms debounce (`LIVE_PAUSE`) | latest wins | live picker, partial results at 100/400 ms |
| Terminal panel | openpty + setsid. Spawned in a job (`Session.terminal_starting`). Reader thread with **one batch in flight** (`terminal.rs:255-292`), writer thread (`terminal.rs:294-302`). | the user's own programs (their cargo too, invisibly) | close → SIGTERM | strict | the panel |
| Debugger | adapter spawned and connected in a job, setsid, threads (`dap.rs:159,226,324,716`) | — | disconnect/kill | — | debug pane |
| Processes (`:procs`) | one thread, every 30 s or 1 s while open (`procs.rs:30-31,311`) | ~450 µs a look | — | — | panel, `▲` |
| Sessions: make/end/bring/rebase | `shared_jobs` for `git worktree` (`worktree.rs`). Rebase as a task run (`task_start_git`, `task.rs:471`). | `.git` | `Rebase::why` refuses overlaps | — | cards, tabs |
| `:restart` | turn → writes → find → build as a task run (`restart.rs:29-43`, `task_start_restart`) → check → exec | the release profile's lock, CPU | `:restart` again | staged | panel, toasts |
| Clipboard, `:sh`, theme load, config reload | jobs (`clipboard.rs:8,20`, `typed.rs:550-564`, `editor.rs:2570`) and the config watch thread | — | — | — | — |

### 1.3 Waits on the main thread, measured

Tarae's Sept off-CPU pass found only pty writes, and those were fixed. Reading the code again
finds three more. All are small or rare, but each is a wait on the keystroke path.

I built a std-only measurement with `rustc` in `/tmp` and removed it afterwards. It was a process
holding 30 / 230 / 800 MB touched memory and 64 parked threads, like tarae (230 MB RSS, 66
threads):

| What | 30 MB | 230 MB | 800 MB |
|---|---|---|---|
| `Command::spawn`, plain (posix_spawn) | 0.16 ms | 0.15 ms | 0.15 ms |
| `Command::spawn` with `pre_exec`, i.e. fork+exec (what `dap::setsid` forces) | 1.0 ms (max 1.8) | **2.9 ms (max 6.5)** | **7.2 ms (max 17.9)** |
| `thread::spawn` + join (one job's thread) | 22 µs | 25 µs | 22 µs |
| bytes a pipe takes before `write` blocks | 64 KB | 64 KB | 64 KB |

1. **Task and test spawns fork the whole editor on the main loop**: `task.rs:510`,
   `testing.rs:597`. That's 3–7 ms a run at today's RSS, growing with memory. It's a quarter to
   half a frame, and every run pays it. LSP and Claude spawns don't use `pre_exec`, so they cost
   0.15 ms.
2. **`flush_saves` before every run** (§1.2) blocks until the saver is idle, up to 10 s. Usually
   it's a few ms per file (9 ms for 270 KB with F_FULLFSYNC), but on a slow or network disk it
   freezes the screen. Async save landed after the Sept perf pass as `saver.rs`. What's left is
   these waits for it.
3. **Chat stdin writes are synchronous** (`chat.rs:984` and others). One user message can carry
   the whole file (≤ 60 KB, `chat.rs:116`), other unsaved buffers (≤ 120 KB, `chat.rs:120`) and a
   screen. That's well past the measured 64 KB pipe. A prewarmed claude that's still
   cold-starting (8–10 s) isn't reading yet, so the main loop would sit in `write` until it does.
   I haven't measured this with the real CLI. It's the first thing to check in phase H.

Thread-per-job is not a problem: 25 µs each, and 66 threads. **No async runtime or thread pool is
needed.** The event.rs header ("an async runtime only if streaming proves it necessary") still
holds.

---

## 2. Where queues help, and where they hurt

### 2.1 Today's evidence (five sessions, one machine)

| Seen | Cause in the code |
|---|---|
| A check waited **37 of 48 s**, and nothing said so | One build-dir lock for every workspace and worktree (`~/.cargo/config.toml`: `build-dir = "{cargo-cache-home}/build/shared"`). cargo printed `Blocking waiting for file lock on build directory` to stderr. tarae showed it as an output line, not as the run's state, and Claude's report doesn't single it out. |
| rust-analyzer's checks took 20–65 s, 54 times | Five RAs flychecking on every save into the same lock. `saves_due` holds didSave only while *Claude's* run in the *front* session builds (`lsp_editor.rs:670-679`). The user's runs and the sessions behind aren't covered. |
| A run sat **36 s with no card and no process** | Between the call and the spawn there are the read-only job, a card, and the guard job (probe, `cargo metadata`, 150 ms watch). `cargo metadata` measured at 0.2 s just now. A card lost, as #94 (it happened to this design session's own `cargo metadata` run), fits best. Phase 0's trace will say for sure. |
| perf flaked under other sessions' load | Nothing stops other sessions starting heavy runs during `cargo perf`. perf's own guard can only give up ("not measured", load 6.6). |
| Claude re-sent refused runs 2–3 times | Busy slots refuse with "wait for it to finish, then run again" (`tools.rs:1066`, `284`, `812`, `1329`). Claude has no way to wait, so it polls. |
| Cards got lost | #94. In frictions4 (not landed), `asking_sync` now tells the call its card was lost. A queued or asked run must also be findable from somewhere other than its card (§3.6). |
| Preflights took 12–16 min | Three sessions' preflights overlapped, each running clippy ×2, test ×2 and perf into one lock. Run one after another, each would be a fraction of that, and none would be "machine busy". |

### 2.2 Where a queue helps

- **Heavy runs that share a resource** (a lock or the CPU). Running them in parallel only moves
  the wait into cargo, where nobody sees it, and slows everyone through CPU contention. A queue
  makes the same wait visible, ordered by what matters, and shorter in total.
- **Exclusive runs** (perf, benchmarks). Only a gate in front of everyone else's runs can give
  them a quiet machine.
- **Identical requests.** Claude's re-sends, `space m m` twice, two sessions asking a check of the
  same checkout. One run, every asker answered.
- **Asks to the user.** These are already queues (`Chat.reads`, `edits.queue`, the offers stack).
  They need to be *listed*, not just stacked, so a lost card is still answerable.

### 2.3 Where a queue hurts, so it stays out

- **Anything interactive**: keys, frames, parse, completion, hover, go-to, search, git hunks,
  previews. Today they're latest-wins with generation or `in_flight` checks (§1.2). That's right:
  a FIFO queue would add latency and do stale work first. Budgets that would be broken: key → frame
  0.19 ms; warm LSP 35–68 ms (N1); search's first results at 100 ms.
- **Short reads.** `git status`, Claude's read-only commands (`readonly::read_only`), `cargo
  metadata`, `cargo fmt --check`. Queued behind a 15-minute preflight, a 0.2 s read would wait
  15 minutes. Light work must be decided *before* anything is queued, and must never wait.
- **In front of language servers.** They have their own queues and cancellation. A second queue
  only adds delay. What tarae can do is *put off server work that takes a resource* (flycheck), as
  `saves_due` already does.
- **The user's terminal.** It isn't tarae's to schedule. It can be observed (§3.7), never held.
- **Running heavy runs.** No preemption by default: killing a build loses minutes of work. The one
  exception is work that's cheap to redo and that tarae can ask to stop (RA flycheck).

---

## 3. Proposed design

### 3.1 Job classes

| Class | What | How it runs | Queued? |
|---|---|---|---|
| **frame** | input handling, drawing, the event hooks | main loop. Each batch ≤ 16 ms (as now). | never |
| **interactive** | parse, LSP requests, search, hunks, previews, inlay, completion | jobs, latest wins (generation / `in_flight`) | never. Deduped by key, superseded by newer. |
| **light** | reads, stats, git status/blame/branches, small git runs, read-only commands, guard prep, `cargo metadata` | jobs at once. The per-session git FIFO stays. | never |
| **heavy** | builds, checks, tests, installs, Claude's non-read commands, the user's tasks, preflight steps, the restart build | **the lineup** | yes |
| **exclusive** | perf, benchmarks, anything declared to need a quiet machine | the lineup, alone among heavy runs | yes |
| **resident** | language servers, Claude processes, the terminal, the debugger | started at once. Their *heavy phases* are observed holders (§3.7). | no; governed by rest/stop policy (§3.9) |

How a command gets its class, first match wins:

1. Declared exclusive (pack, project or settings).
2. Light by `readonly::read_only` / `tools::filtered` / a pack's `light` list.
3. Declared to take a resource (pack).
4. `tools::is_check` (`tools.rs:553-597`).
5. **Anything else Claude or the user runs as a task is heavy.** This is the agreed "unknown =
   just heavy".

### 3.2 One scheduler, Editor-wide: the *lineup*

All sessions live in one tarae process (`Editor.session` + `Editor.sessions`). So one in-process
scheduler on `Editor` sees every tarae-started run, with no cross-process locking. It goes in a
new module, `lineup.rs` (`Editor.lineup`). The word avoids a clash with `event::Queue`. To the
user it's "queued".

```rust
struct Lineup {
    waiting: Vec<Entry>,          // not started
    running: Vec<Holder>,         // started by tarae: what they hold
    observed: Vec<Observed>,      // not tarae's to start: RA flycheck, a cargo in the terminal (§3.7)
}
struct Entry {
    id: u64, key: Key,            // dedupe key (§3.4)
    session: SessionId, who: Who, // User | Claude(Call) | Preflight | Restart | Install
    class: Class, takes: Vec<Resource>,   // resource = (name, key) e.g. ("cargo-build-dir", "/…/build/shared")
    asked: Instant, joined: Vec<Waiter>,  // every caller to answer
    start: Box<dyn FnOnce(&mut Editor) -> Option<Holder>>,  // today's task_spawn / test_start, unchanged inside
}
```

The rule for starting, checked when something is added and when something ends (pure; tests pass
`now`). A waiting entry starts when all of these hold:

- No exclusive entry is running, and none is waiting ahead of it. A waiting exclusive is a
  barrier: it drains the heavy runs and lets no new ones start, or it would starve.
- Fewer than `run.heavy-max` heavy runs are running.
- Each of its resources has fewer holders than the resource allows, counting observed holders it
  can't put off.
- If it's exclusive: nothing heavy is running. Observed holders get `EXCLUSIVE_GRACE` (60 s), then
  it runs anyway, saying so. perf's own load gate stays as the backstop.

A run's slot is taken **at spawn**, never while a card is up. Asks never hold resources, so a
lost or slow card can't block anyone else.

### 3.3 Priorities

Ranked from first to last:

1. **The user's own run, front session** (`space m m`, `space x …`, preflight started by hand).
2. **Claude's run, front session.**
3. **`:restart`'s build, and `:preflight` runs started by a card's `p`.** They're asked by the
   user, but they're long. They rank below a short run of the user's that came later, and above
   anything behind.
4. **Sessions behind**: the user's, then Claude's.
5. **Server work tarae can put off** (RA flycheck). It isn't queued, but it's held while anything
   above it waits on the same resource.

Ties go to the oldest. **Aging**: every `AGE_STEP` (2 min) of waiting moves an entry up a rank, so
a session behind isn't starved by a busy front. Switching sessions changes which session is in
front, so the ranks are recomputed on `session_switch`.

The user's new task **no longer kills Claude's running command** (`task.rs:523`). It goes first in
line. The waiting row in the panel offers `enter run now — stops <X>` for when the user does want
it now.

### 3.4 Dedupe and supersede

- **Key** = the checkout's real path + the normalized command (words, `cd` resolved,
  `readonly::parse`) or the test target (`Target.argv` + cwd) + the policy's grants.
- **Waiting duplicate**: the new asker joins the entry, and every waiter gets the one result.
  Claude's re-send becomes harmless.
- **Running duplicate**: the asker joins only if no file in that checkout was saved since the run
  started. The saver's done stamps per root say so (`saves_done`, `saver.rs:132`). Otherwise the
  ask is queued fresh, and deduped against anything else waiting.
- **Two checkouts are never duplicates**: different code.

### 3.5 Cancellation

- **Waiting**: removed freely. The user does it from the panel or `:procs`. Claude's turn ending
  or interrupted removes its entries; the closures already check `c.busy()`, and they move into
  `Lineup::prune`. Ending a session removes its entries. Each removed waiter is answered: "not run
  — <why>".
- **Running**: as now, SIGTERM to the group (`Run::drop`, `TestRun::drop`).
- **`:restart`** answers every waiting entry ("tarae restarted — run it again"). A running entry is
  waited for, as the restart's turn already is.

### 3.6 How a queued run is seen

Design-language rules apply: one line each, counters keep their width, the accent only for what
asks.

- **Task/test panel**: the run's panel opens at once, so there's no blank interval.
  `queued · 2nd · behind cargo test (rebase) · 0:37`. The position and the holder are what the
  user needs; the timer is `{:>4}`. Then `waiting for cargo's lock · 0:12` once started but
  blocked (phase 0). Then running as today. The row's hints: `enter run now · d drop`.
- **Session tabs / list / chip**: a session whose run waits gets a quiet mark with its position.
  The glyph is chosen in the shot pass. `◦` is taken by "stuck", and `Doing` gets a `Queued`
  variant so a waiting call doesn't read as stuck (`session.rs:263-307`).
- **`:procs`**: a "lineup" block above the list. Per resource: who holds it (tarae's runs *and*
  observed holders: "rust-analyzer's check (frictions4)", "cargo in your terminal"), then what
  waits, in order. The asks waiting on the user are listed here too (`What::Run`, `NoSandbox`,
  `Read`), each with an "answer" key. This is the backstop for lost cards (#94).
- **Claude's tool answer**: the call waits, as `run_tests` already does (90 s measured fine). Its
  report starts with what the wait was: `Waited 37 s — 21 s queued behind cargo test in session
  rebase, 16 s for cargo's build-directory lock. Ran 11 s.` If the expected wait is long (the
  holder's past duration from the trace, over `LONG_WAIT` = 3 min), it's answered at once instead:
  `Queued 3rd behind :preflight (≈ 12 min). It stays queued; call run again with the same command
  to wait for its result.` The re-call joins (§3.4), so even polling costs nothing.
- **No card for queueing.** It isn't a question. Cards stay for asks.

### 3.7 Observed holders

These are resources held by work tarae didn't start through the lineup:

- **Server work, by progress.** A pack names the server and the `$/progress` title that means "I
  hold this resource". RA's flycheck shows as `cargo check`; tarae already logs its begin and end
  (`Progress.began`). While a lineup entry waits on that resource, tarae holds `didSave` for that
  server, generalizing `saves_due` to every session and every asker. If the pack gives a cancel
  message (`rust-analyzer/cancelFlycheck`), tarae also sends it, and re-runs the check
  (`rust-analyzer/runFlycheck`) when the resource frees. Both messages need checking against the
  RA we ship with before use.
- **Descendants by name.** `procs::tree` already lists every descendant. A `cargo` or `rustc`
  under the terminal's shell counts as an observed holder of `cargo-build-dir` (display, and the
  exclusive drain). It's never stopped.
- **Lock waits from output.** A pack's `waits` patterns turn a line like `Blocking waiting for
  file lock on build directory` into run state, not output. It ends at the next line that isn't
  one.
- **Other tarae processes, CI, the user's other terminals** are invisible. cargo's own lock still
  serializes them, and the lock-wait state at least names it.

### 3.8 Language-pack declarations

New tables in `pack.toml`. They're data: no code, a runtime-folder pack wins table by table (as
`packs.rs:1-4`), and an unknown language gets nothing declared, so it's just heavy.

```toml
# runtime/packs/rust/pack.toml

# What a run of these commands holds while it runs
[[resource]]
name = "cargo-build-dir"
takes = ["cargo build", "cargo check", "cargo clippy", "cargo test", "cargo nextest", "cargo run",
         "cargo doc", "cargo bench", "cargo install", "cargo xtask", "cargo perf"]
key = "{cargo-build-dir}"     # one lock per build dir: from `cargo metadata` (sandbox.rs already reads it)
holders = 1
waits = ['^\s*Blocking waiting for file lock on (?<what>.+)$']
server = { name = "rust-analyzer", progress = ["cargo check", "Building compile-time-deps"],
           cancel = "rust-analyzer/cancelFlycheck", rerun = "rust-analyzer/runFlycheck" }

[run]
light = ["cargo metadata", "cargo fmt", "cargo tree", "rustc --version"]
exclusive = ["cargo bench"]
```

Notes on other languages:

- **Java**: `name = "gradle-project"`, `key = "{java-root}"`, `takes = ["{java-tool}"]`,
  `waits = ['Waiting for lock', 'Timeout waiting to lock']`. The Gradle daemon serializes per
  project itself. The declaration makes the wait visible and ordered.
- **JavaScript/TypeScript**: `name = "node_modules"`, `key = "{nearest package.json}"`,
  `takes = ["npm install", "npm ci", "pnpm install", "yarn install", "bun install"]`, `holders = 1`.
  Tests take no resource, so they're just heavy.
- **Go**: no resource. The build cache is safe concurrently. `go test`/`go build` are just heavy.

A project's `.tarae.toml` can add its own. This repo would declare `[run] exclusive = ["cargo
perf"]`: an xtask alias the pack can't know is a benchmark. These keys only delay runs, never run
anything, so they stay out of `trust::RISKY`. A hostile project could make runs wait, but visibly
and bypassably (`run now`).

New settings, one line each in `settings.rs`:

| Setting | Type | Default | Meaning |
|---|---|---|---|
| `run.lineup` | bool | true | |
| `run.heavy-max` | int | 2 | heavy runs at once across sessions |
| `run.exclusive` | list | [] | patterns |

### 3.9 Slot: rust-analyzer per session, shared or swapped

Sources: `target/measure/ra-measure-`, `ra-share-` and `ra-swap-2026-10-04.md`. "Swap" there means
replacing the shared server A with a new server B whose workspace folders include the new checkout,
then switching every session to B once B is quiescent.

| | Value | Source |
|---|---|---|
| RA footprint per session | ≈ 0.85 GB (RSS hides it: as low as 259 MB); 4 servers ≈ 3.4 GB. 4 RAs = 4.1 GB RSS right now. | ra-measure §1, `ps` today |
| One server for N checkouts | +0.1–0.25 GB per checkout; 1.1–1.4 GB for 4. Correct per checkout: nothing mixed except `workspace/symbol`, which tarae would filter. | ra-share §1–2 |
| Cold start to a right answer | 3.1–6.1 s for one checkout, 6.2–6.6 s for 4. A second ≈2 s `null` gap follows when compile-time-deps land late (lock held). | ra-measure §1, ra-swap "Right isn't loaded" |
| Coming back to a rested (killed) server | 3.1–4.4 s until hover/definition are right again | ra-measure §2 |
| **Swap: the other sessions' wait** | **none**: slowest answer 41–81 ms (160 ms with checks on); every one of 1,000+ answers per run was right | ra-swap §1 |
| **Swap: the new checkout's first right answer** | 5.3–6.7 s, the same as a restart's and no slower than a server of its own (4.8–8.5 s) | ra-swap §1, §4 |
| **Swap: memory** | overlap ≈ A + 0.9 GB for 7–21 s, peak 2.5–2.9 GB (3.9 GB with checks on). After: one server, 1.36–1.64 GB for 4 checkouts. | ra-swap §1 |
| **Swap: CPU and lock** | B's load ≈ 19–20 s CPU. With `checkOnSave` off until quiescent: no `cargo check`. With it on: 1–4 full checks (21 s CPU here, up to ≈68 s for differing checkouts), the lock held 12–48 s. | ra-swap §2 |
| Live add instead (`didChangeWorkspaceFolders`) | the new root takes 54–96 s (checks on), and the old roots get **2.4 s of `null`** | ra-share §2, ra-swap §4 |

**What the numbers favour: (c), done as a swap.** That means one rust-analyzer per repository and
toolchain, changed by swapping rather than by restarting or live add, following the ra-swap recipe:

- spawn B with `checkOnSave` off;
- `didOpen` every open buffer into B, and send each `didChange` to both servers;
- switch at quiescent, not at B's first right answer;
- batch new sessions made close together into one swap.

Why (c) wins:
- It's the only option where nobody waits at either moment. Switching between sessions costs 0 s,
  because every session's server is always warm. Making a session costs the others 0 s, and the new
  one gets answers in 5–7 s.
- It costs about a third of (a)'s memory: 1.4–1.6 GB against ≈3.4 GB.
- The price is a short overlap: about +0.9 GB for 7–28 s per swap.

Why not the others:
- (b) saves memory too, but you switch sessions far more often than you make them. Each return costs
  3–4 s, plus the late ≈2 s gap.
- (a) keeps ≈0.85 GB per session for 10 minutes and still pays (b)'s cost after that.

Open, before building it:
- the lost check on two saves milliseconds apart in different roots (ra-share §4);
- a big change (rebase, branch switch) in one root while the others ask;
- falling back to a restart when there isn't ≈1–1.8 GB to spare.

The decision stays the user's.

How it plugs into the lineup, whatever is chosen:

- An RA start or restart runs `Building compile-time-deps` through cargo, so it **holds
  `cargo-build-dir`** (an observed holder, by progress, §3.7). A swap at session switch then waits
  behind a running build instead of racing it, and the user sees why the switch's go-to isn't
  ready.
- The policy runs where it runs today: `sessions_sync` → `lsp_rest` (`session.rs:424-453`,
  `lsp_editor.rs:1947`). The candidates are:
  - (a) a server per session, resting after idle and stopped after `IDLE_STOP`. That's today's
    policy, or ra-measure's version: rest after 2 min, at most 2 live servers.
  - (b) a server per session, stopped at once when the session goes behind and restarted in front.
  - (c) one shared RA for checkouts of the same repository, changed by swap.

  The lineup only needs RA's heavy phases to be visible.
- A swap is itself a lineup entry, keyed by the server (repository + toolchain): one at a time, as
  the recipe asks. Its B holds `cargo-build-dir` through compile-time-deps (it waited 7 s on the
  lock in one run). Keeping `checkOnSave` off until quiescent spares the lock N full checks. The same
  run-time switch (`didChangeConfiguration`, which takes effect without a reload, per ra-share §4) is
  how §3.7 can hold flycheck instead of the unverified cancel message.
- ra-share's side finding matters to the lineup too. A check of one checkout makes the next check of
  another a full ≈12 s / 17 s CPU, because `build.rs` prints absolute rerun-if-changed paths into
  the shared build dir. So among equal-rank waiting entries on `cargo-build-dir`, the lineup should
  prefer the checkout that held it last (*affinity*). Serializing alone doesn't remove that cost.

---

## 4. What it takes

### Phases

| Phase | What | Size | Pays off alone? |
|---|---|---|---|
| **0. See the waits** | (1) A pack's `waits` patterns become run state: panel `waiting for cargo's lock · 0:37`, and Claude's report says how much of the run was lock wait. (2) A per-run trace in the log, like N1's go-to `Trace`: asked → card up/answered → guard ready → spawned → first output → lock wait start/end → end, one `timing` line. | ~200 lines + 3 tests (fake `sh` printing the Blocking line). 1 day. | **Yes.** The 37 s becomes visible to user and Claude, and the trace tells us where the 36 s went before anything is built. |
| **H. Main-thread hygiene** (any time, independent) | Task/test spawns without fork: a job, or posix_spawn with a setsid flag. Check whether the 1.99 toolchain has a stable `CommandExt` setsid; else `libc::posix_spawn` + `POSIX_SPAWN_SETSID`. Chat stdin through a writer thread like `lsp.rs:589`. `flush_saves` before runs becomes "start once the saver is idle" (an event, not a wait). | ~250 lines. 1 day. | Yes. Removes 3–7 ms per run, an unbounded save wait, and a possible seconds-long stall. Measure the chat case first. |
| **1. Wait, don't refuse** | **Session frictions4 (not landed) does part of this**:<br>• a second run while a card is up gets a card of its own, not the "hasn't answered" refusal;<br>• a run said yes to while the slot is busy waits in `Session.runs_next`, a per-session FIFO of `tools::Next`, and `run_next` starts it when the slot frees. Calls whose turn has ended are skipped (frictions4 `tools.rs:1327-1356`; the run's end calls it at `task.rs:612-616`);<br>• a lost card's call is told so (`Session.asking`, `asking_sync`, frictions4 `offer.rs:109-130`, `session.rs:501`), #94.<br>**What's left:**<br>• the *unasked* path, which is the common one (an allowed check, a read-only command, a chat pattern), still refuses at the door (`run_busy(ed)?`, frictions4 `tools.rs:767`), and so does `go` when the slot was taken meanwhile (`:1475`); both should go into `runs_next`;<br>• `run_tests` still refuses (`busy`, frictions4 `tools.rs:265`, `341`, `348`) and needs the same queue on the test slot;<br>• dedupe (§3.4), so a re-send joins rather than queues twice;<br>• the user's task still kills Claude's run (frictions4 `task.rs:514`);<br>• the waiting run is only a `note` (`:messages`), so it needs a panel row and `Doing::Queued`;<br>• Claude's answer should say how long it waited. | ~200 lines on top of frictions4. 1 day. | Yes. Ends the re-send loop for every path, not just the asked one. `runs_next` becomes the per-session shape that phase 2's lineup replaces. |
| **2. The lineup** | Editor-wide scheduler, `run.heavy-max`, priorities with aging, `[[resource]]` in packs with the cargo key from `cargo metadata`, panel / tabs / `:procs` / report visibility, a shot pass. | ~900 lines + shots. 4–5 days. | The main gain. Concurrent sessions stop fighting in cargo's lock. |
| **3. Exclusive runs and preflight steps** | Exclusive class with drain and grace. `:preflight` runs each step as its own lineup entry (it already tracks per-step state: `StepRun`, `preflight.rs:140-160`) instead of one `sh` script, so perf waits for a quiet machine and the cheap steps of another session's preflight can interleave. | ~400 lines. 2 days. | Yes. perf measured, not "machine busy". |
| **4. Observed holders + RA** | Server progress as holders, generalized `saves_due`, flycheck cancel/rerun, terminal descendants via `procs::tree`, the RA policy (§3.9). | ~300 lines + the RA policy. 2 days. | Yes, once phase 2 exists. |

The order I'd recommend: **0 → H → 1 → 2 → 3 → 4**. Phase 0's trace may change phase 2's
defaults. For example, if lock waits dominate, `heavy-max` = 1 per resource matters more than the
CPU limit.

### Risks

- **Misclassified light work.** A short command ends up behind a long run. Mitigations: light is
  decided first; `run now`; the trace flags any "light" run over 2 s; packs list light commands.
- **Claude's tool-call patience.** Is a 15-minute MCP wait fine with the real CLI? Unmeasured; 90 s
  is known fine. Answering early above `LONG_WAIT`, with joining re-calls, works either way. Measure
  before phase 1 sets `LONG_WAIT`.
- **Starvation and deadlock.** Aging bounds starvation. Asks never hold slots. The restart build
  enqueues only after its turn ends (`restart.rs` `Stage::Turn`), so it can't wait on a Claude run
  queued behind it. Nested cargo inside a run is inside its holder, never queued. Tests cover each
  as a pure `Lineup` test with `now` passed in.
- **The "it's slower now" perception.** A queued run starts later than today's parallel one, even
  when it finishes sooner overall. The panel must show position and holder from the first frame,
  and the measurements below must show total time down.
- **RA's flycheck messages.** Their names and behaviour in the shipped RA are unverified, so check
  them before phase 4.
- **One process only.** Two tarae processes don't see each other's lineups. cargo's lock still
  serializes, and phase 0's state names the wait. A cross-process lineup (a lock file in `$state`)
  is out of scope unless it shows up in use.

### What to measure, before and after

From phase 0's trace and the log, over a working day with 3+ sessions:

- Per heavy run: **time queued, time in cargo's lock, time running**. The goal: lock time near 0,
  queued time ≤ today's lock time, total per run down.
- **Preflight wall time** with sessions overlapping (today 12–16 min), and the "perf: not
  measured" rate (today 1 in 3).
- **RA flycheck durations** (today 54 of 237 ≥ 20 s, max 64.6 s) and the count of flychecks
  started while a build held the lock.
- **Claude's refused or re-sent runs per turn**: count `already running` answers in `:messages`
  before; joins after.
- **Lost asks**: runs waiting on a card that isn't up anywhere (`:procs` lineup block). Should be 0
  once #94 lands; the block makes any regression visible.
- **Main thread**: spawn time per run (2.9 ms → 0.15 ms); `flush_saves` time per run; chat stdin
  write time for a ≥ 100 KB message to a cold-starting claude; key → frame during a run's output
  flood (`cargo perf` adds a scenario).
- **Idle**: `an_idle_editor_draws_nothing_and_sleeps` still passes. A waiting entry's timer asks
  for frames only while its panel is in sight, like the toasts' `schedule_tick` rule.
