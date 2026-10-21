# Cards: what a session may do, not what a command looks like

Design, 2026-10-04. Status: **approved** with the five recommendations of §8 (2026-10-04). P0, P1, P2 and
P5 are built (`src/cards.rs`, `allow::defaults`; `sandbox.rs`' write rules; its read rules; `snapshot.rs` —
§9 "P2, as built", "P5, as built"); P3's gate switch too (§9 "P3, as built"); P4 isn't yet.

## Headline

Today tarae decides, command by command, whether Claude may run something without asking. It reads
the shell line (`readonly.rs`, `tools.rs`: about 1,700 lines of product code and 1,800 of tests) and
matches it against lists of commands, options, environment names, git subcommands, script endings
and loops. Every friction (#102 #110 #112 #113 #114 #116 #117) was fixed by one more rule. The cards
that are left come too often to read: the user counted 20–40 per 30 minutes across 5–7 sessions,
and tarae's log shows at least 48 in the busiest half hour today (§1). A person answers that many
without reading them, so they protect nothing.

The parser has to judge safety because **the sandbox doesn't confine reads**. The profile is
`(allow default)` plus a deny on writes (`src/sandbox.rs:591-592`). Any command can read `~/.ssh` and
print it, and its output goes to Claude. So tarae has to work out *where a command reads* before it
runs. That's the job that keeps growing.

There's a second reason the parser isn't really the boundary. **The checks already run arbitrary
code unasked.** `cargo test` runs whatever tests Claude wrote this turn, and with review at the end
the user hasn't seen them. Today the only things between that code and the user's secrets are the
sandbox's write rules, and those don't cover reads.

The proposal:

1. **The sandbox enforces everything a session may do**: read, write, connect, and reach sockets.
   Reads are confined too, with the user's home closed except for named places.
2. **A session has capabilities**: sets of places and ports, with defaults that cover a day's work.
   Anything inside them runs without a card, *whatever the command is*. The parser only classifies
   commands (feeds the user's list, the lineup's light or heavy class, dedupe). A wrong guess costs a
   queue position, not safety.
3. **A card asks for a capability, not a command.** It says what opens and for how long: this run ·
   this chat · this session · always in this project. Every grant is listed and can be taken back.

Estimated effect on today's mix: from about 40–80 cards an hour at 5–7 sessions to about 1–3 an hour,
mostly the first time a project needs something (§7). About 2,000 lines of parser and tests go away.
About 900 come in. The first step (§9, P0) is a log line per card, ½ day. Today's cards aren't
logged anywhere I can read, so the estimates here need it to be confirmed.

---

## 1. Measured today

### 1.1 From tarae's log

`~/.local/state/tarae/tarae.log` covers 17:06–20:04 today (one tarae, 9 sessions over the period:
cleanup, frictions5, phase0, phaseH, presplit, ra-policy, size, spacing2, uistate).

- **`<session> · Claude waits for your answer — C-q goes there`** (`src/session.rs:600`): logged
  once each time a session *behind* starts waiting on the user. That's a card of its Claude, or an
  edit to review (`Session::waits`, `session.rs:251`). 137 in 3 hours. By 10-minute block:

  | 17:1x | 17:2x | 17:3x | 17:4x | 17:5x | 18:0x | 18:1x | 18:2x | 18:3x | 18:4x | 18:5x | 19:0x | 19:1x | 19:2x | 19:3x | 19:4x | 19:5x | 20:0x |
  |---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
  | 7 | 12 | 21 | 15 | 13 | 3 | 3 | 1 | 3 | 7 | 5 | 9 | 8 | 9 | 5 | 6 | 7 | 4 |

  The busiest 30 minutes (17:20–17:50, 5–6 sessions working) had **48**. This is a lower bound for
  three reasons. Cards in the front session aren't logged. Back-to-back cards in one waiting spell
  count once. And edit reviews can't be told apart from cards.

- **`Claude may read … in this conversation`**: an `a` on a read card. There were 14, and 12 of them
  fall into two kinds:

  | Read | Count | Kind |
  |---|---|---|
  | `…/worktrees/tarae-8f35164e/async-design/docs/design/async.md` (×3), `~/Workspace/tarae/{target/measure, target/perf/history.tsv ×2, docs/design/async.md, CLAUDE.md}` | 8 | **another checkout of the same repository** |
  | `/private/tmp/tarae-presplit-shot/{bring,running,passed,bring2}.png`, `/private/tmp/tarae-card-shot/…`, `/private/tmp/tarae-spacing/out/…` | 6 | **what the session's own runs made, read in a later turn** (`reads::turn_over` clears `Chat.ran`, `reads.rs:341`) |

  The four `tarae-presplit-shot` cards came within 6 minutes. #102's folder offer didn't fire
  because an `a` answer doesn't go into `Chat.read_once` (`reads.rs:315-320`). Only `y` answers do.
- No `timing` lines: the running binary predates phase 0's run trace. There's no record of which run
  cards were asked or why. `note()` lines (`… runs without asking for the rest of this chat`) aren't
  logged.

### 1.2 From the frictions

Each one was a card for something that could never have hurt the user once the sandbox does its job:

| # | What asked | Rule added |
|---|---|---|
| 102 | 12 reads in one folder, 12 cards | `allow::grant_places` folder offers |
| 110 | `git show :2:a.rs`, `git diff --no-index` | git read rules, `--no-index` judged as a read |
| 112 | reading the toolchain's sources, `$(rustc --print sysroot)` | `code_trees`, `SYSROOT`, `allow::sysroot` (runs `rustup`) |
| 113 | `for f in …; do wc -l "$f"; done` | `unrolled`, variable marks (`VAR`) |
| 114 | `A=… B=… cargo xtask shot` | `ENV_NEVER` (46 names), patterns that set names |
| 116 | `cargo tree`, `cargo metadata`, `nm`, `otool` | more `RUN_ALLOW`, `COMMANDS` |
| 117 | `awk -f x`, `python x.py` | `INTERPRETERS`, `scripts_run`, scripts shown on the card |

Each rule is also attack surface. `ENV_NEVER` exists because a pattern that lets
`RUSTC_WRAPPER=x cargo build` through runs `x`. `script_part` once let `/bin/sh -c '<anything>'`
through (the review note at `readonly.rs:630`). A mistake in any of them is a way out.

### 1.3 What to log to measure for real (P0)

One line per card, when it's answered or dropped (built in P0 — `src/cards.rs`):

```
20:01:13  @size            tarae            card     card#12 · run · why=not-a-check:awk · shadow=none · a · 900ms · up 4.2s
20:01:40  @ra-policy       tarae            card     card#13 · read · why=read · class=home · shadow=grant:read · y · 1.4s · up 1.9s
20:02:02  @frictions5      tarae            card     card#14 · run · why=socket · shadow=socket · y · 6.2s · up 6.3s
20:02:30  @size            tarae            card     card#15 · run · why=reads-outside · class=temp,elsewhere · shadow=grant:read · n · not drawn · up 0.4s
20:03:10  @size            tarae            card     card#16 · run · why=not-a-check:awk · shadow=none · same=#12 · y · 1.1s · up 1.3s
```

- `@size`: the session, in the log's own column (`log::Session`, linux7 piece 9) — as the session list names it
  (the checkout tarae started in by its branch), cut to the column with a `…` past 15 characters
  (`@fix-login-ever…`). `grep @size tarae.log` is one session's lines; `warned` and `denied` lines carry it the
  same way.

- `card#<id>`: the card (cardburden 3), so a later card's `same=#12` — a run card the same as one answered
  before, `same=#12:changed` when what it runs changed since — names a line here.
- The kind: `run`, `read`, `no-sandbox`, `protected-made` (Claude's next run, after one made a protected
  name Landlock couldn't refuse), `screen`, and the sandbox's own `grant` and `outside` (the user opening
  what it blocked).
- `why` is the rule that sent it to a card: `not-a-check:<prog>` (the first part no rule lets through),
  `unreadable`, `script`, `script-changed`, `env-name`, `glob`, `reads-outside`, `cd-unknown`,
  `runs-outside`, `socket`, `outside`, `no-sandbox`, `protected-made`, `read` (Claude's Read/Grep/Glob),
  `screen`, `blocked`.
  Reads add `class`: `other-checkout`, `temp`, `toolchain`, `home`, `elsewhere`, `secret`, `unknown`.
- `shadow` is what the capability model (§3) would do, from `cards::decide` — the one function P3 will
  decide by: `none` (runs or reads unasked), `grant:<read|write|port>`, `socket`, `outside`, `no-sandbox`,
  `screen`.
- The answer (`y a A n`, or `dropped` when the turn takes the card away), the time from the card first
  being *drawn* in front to the answer (`not drawn`: answered or dropped unseen), and `up`: from the ask.
  A lost card keeps its own `cards lost` line (#94).

From a day of these, `:messages`-free:

- cards per hour per session, by `why` and by `shadow`;
- **the stamp rate**: the share of `y`/`a` answered within 1.5 s, and the share of `n`. If `n` is
  near 0 and the median is under 2 s, the cards carry no information;
- what the capability model would still ask (`shadow ≠ none`). That's the after-number, measured
  before anything changes.

Plus a `denied` line per sandboxed run with denials (the `Watch` has them already, `sandbox.rs:393`).
After the switch, denials are what turns into grant requests.

---

## 2. Why per-command judgement keeps growing

`tools::run` (`src/tools.rs:829-890`) goes through, in order:

1. `allowed`: `RUN_ALLOW` (`tools.rs:489`) or a chat pattern, word for word, with `WRITES` options
   refused and `ENV_NEVER` names refused.
2. `filtered`: a check piped into `FILTERS`.
3. `chain_allowed`: `;` `&&` `||` `|` of allowed parts.
4. `readonly::read_only`: `COMMANDS`, `GIT`, `sed_prints`, and option rules per program
   (`readonly.rs:840-864`). Then `outside` on a worker resolves every word that might be a path, after
   `cd`s and git `-C`s, and fills in the sysroot. Then `runs_outside`.
5. Script pins (`script_pins`): scripts a chat allowed, by content hash.
6. Otherwise a card. The card shows unseen files the command names (`named`, `moved`, `showable`)
   and scripts it runs (`scripts_run`).

Steps 1–5 answer one question: *could this line read something outside, or run something the user
hasn't seen?* That question is undecidable for a shell line in general, so every friction is a new
special case. The sandbox can answer the same question exactly, for every program, at the system
call. Today it isn't asked.

---

## 3. The model

### 3.1 Capabilities

A **capability** is a place or a port with a verb: `read <path>`, `write <path>`, `connect
<port>`, `connect <socket>`, `network`. A session's capabilities are the defaults plus its grants.
**One list, two enforcers:**

- **Claude's own tools** (Read, Grep, Glob, which run in the claude process): `reads::covered`
  checks the path against the read set, as today (`reads.rs:133`).
- **Claude's commands** (`run`, `run_tests`): the Seatbelt profile is made from the same set.
  Everything else is denied by the kernel.

**Defaults** (no card, for every session):

| Verb | Places | Today |
|---|---|---|
| read | the session's checkout | parser + Read gate |
| read | **the repository's other checkouts**: the main one and every `git worktree list` entry, real paths | asked (8 of today's 14 `a`s) |
| read | the toolchains' sources and downloaded crates (`allow::code_trees`), the project's build dirs (`sandbox::build_dirs`) | unasked since #112, by the parser |
| read | the temp folders: `/private/tmp`, `$TMPDIR` | each read asked unless a run made it *this turn* |
| read | tarae's log | as today |
| read | everything outside home (`/usr`, `/opt`, `/etc`, `/Applications` …): programs need it | unconfined |
| read | in home, what toolchains read to start: `~/.cargo` (not `credentials*`), `~/.rustup`, `~/.gitconfig`, `~/.config/git`, `~/go/pkg`, … (**measured in P2**, not guessed) | unconfined |
| write | the checkout but `PROTECTED` (§3.3), temp folders, the toolchain's own (as today) | as today |
| connect | localhost ports in `llm.sandbox-ports`, all with `llm.sandbox-localhost` | as today |

**Never readable, whatever is granted**: the `SECRETS` places (`allow.rs:207`: `.ssh`, `.aws`,
`.gnupg`, `.config/gh`, `.kube`, `.docker`, `.netrc`, `.npmrc`, `.pypirc`, `.password-store`,
`Library/Keychains`), plus `~/.claude`, `~/.cargo/credentials*` and tarae's own `$state`. These are
denied in the profile *after* every allow, because the later rule wins (measured,
`sandbox.rs:11`). A card can open one of them for **this run only**, and says plainly what it is.

**Run without asking** = *any command*, inside the capabilities. That includes the checks, scripts
Claude wrote, `awk -f`, loops, `env A=1 …`, and `$(…)`. This is the main change, and the argument
for it is §2's second point: the checks already run any code Claude writes. "Checks only" was never a
safety line. It stood in for confined reads.

### 3.2 What still asks, and why

| Card | When | Scopes offered | Why it stays |
|---|---|---|---|
| **Grant** | Claude asks for a capability past the defaults: a read elsewhere (its Read tool, or a run's `also`, §3.5), a write outside, a port, the network | `y` this run · `a` this chat · `s` this session · `A` always here | It's a real widening. It comes once per thing, not once per command. |
| Grant, narrow | a read in a `SECRETS` place, home itself or above it | `y` only | A key can't be un-read. |
| **Socket** | one unix socket (#119) | `y` · `a` | A server's socket is the server. Docker's is the whole machine. |
| **Outside the sandbox** | `outside_sandbox` with a reason | `y` only | All the user's permissions. |
| **No sandbox here** | Linux until Landlock, tarae nested in a sandbox | `y` · `a` (as today) | Nothing enforces the capabilities. |
| **Screen** (S2) | Claude's `screen` tool | `y` · `a` (as today) | It's already a capability with a scope. |
| Edit review | Claude's Edit/Write to `PROTECTED` names in review-at-end mode, or to anything without it | (as today) | That's review, not permission. |

Unchanged and not Claude's: trust, hooks, preflight defaults, grammar/install offers.

What **stops being a card**:

- **Writes to `PROTECTED` by a run** (build.rs, `.cargo/`, `.tarae*.toml` …). The sandbox denies
  them, and the report tells Claude to use Edit, which is reviewed. No card.
- **Git that changes refs.** `.git` is already write-denied for runs (`sandbox.rs:679`). `git commit`
  fails in the sandbox by construction. Claude is told that git writes are the user's (the git tab).
  If it really needs one, that's `outside_sandbox`.
- **Writes outside.** These are already denied. The denial report turns into a grant request (§3.5).
- **Deleting** (see §3.4: my one deviation from the brief).

### 3.3 `PROTECTED`: one list for "runs later, outside the sandbox"

There's one list, `llm.review-ask` (`src/llm.rs:122`: `build.rs`, `Cargo.toml`, `package.json`,
`Makefile`, `justfile`, `.envrc`, `.tarae.toml`, `.cargo`, `.github`, `.claude`, `.vscode`), plus
**`.tarae.local.toml`** and `.git`. Today it means "Claude's *Edit* asks first". It becomes **also**
"Claude's *runs* can't write it". In Seatbelt that's
`(require-not (require-all (subpath (param "ROOT")) (regex #"/build\.rs$")))`: the names are static
in the profile and the path stays a parameter, as `profile` requires (`sandbox.rs:586`).

**Two holes this closes, both open today:**

1. **A run can write `build.rs`, `.cargo/config.toml`, `.envrc`, `Makefile` …** Only `.git` and
   `.tarae.toml` are protected (`sandbox.rs:679`). Under review at the end, `cp x build.rs` or a
   test that writes one goes unseen. The next *unsandboxed* build runs it: the user's `space m m`,
   `:preflight`, `:restart`'s build, or the terminal.
2. **A run can write `.tarae.local.toml`.** It's read as the user's own config when git says it's
   untracked and ignored (`config::personal`, `config.rs:210`), with no trust card. Its `[preflight]`
   steps run unsandboxed.

**Suspected third hole, to measure in P1:** `~/.cargo/registry` is writable whole
(`sandbox.rs:691-692`). That lets a sandboxed run rewrite a downloaded crate's `build.rs` in
`registry/src`, which every project's unsandboxed build then runs. cargo doesn't re-check
extracted sources. The fix is to allow writes to `registry/src` only when the network is open (no
download can happen otherwise), or to create only. Measure what an offline `cargo build` writes there.

### 3.4 Deleting: by undo, not by card (deviation, for your decision)

The brief lists deleting as always-asks. I recommend instead that **a run's changes to the checkout
can be taken back**, with no card:

- Outside the checkout, deleting is already impossible (writes are denied).
- Inside, the sandbox can't tell a delete from a save. Formatters and editors save by writing a temp
  file and renaming it over the original. Whether Seatbelt sees that as `file-write-unlink` is to be
  measured, but if it does, denying unlink breaks `cargo fmt`. Asking by command word (`rm`, `git
  clean`, `find -delete`, a Python `os.remove`) is exactly the per-command judgement this design
  removes.
- **Undo instead (P5).** Before each run that isn't a check, tarae snapshots the working tree into
  git's object store, unsandboxed and on a worker: `GIT_INDEX_FILE=<temp> git add -A && git
  write-tree`. It only hashes stat-dirty files, so the cost scales with what changed. The run's
  changes (deletions included) then join the turn's checkpoint. `space u` takes them back, and the
  turn review at the end shows them like Claude's edits.

If you'd rather keep a card: deny `file-write-unlink` on *tracked* paths, passed as literal
parameters (tarae's tree has a few hundred, and 1,000 rules cost 20 ms per run, `sandbox.rs:575`).
That's measured in P2, together with what it breaks.

### 3.5 How Claude asks for more

`run` gains one parameter, replacing the guesswork:

```json
"also": { "read": ["~/src/other-lib"], "write": ["~/.npm"], "ports": [5432],
          "network": true, "socket": "~/.colima/default/docker.sock" },
"reason": "…"
```

- **Without `also`**, the command just runs. If the sandbox denies something, the report names it
  (as today, `Guard::report`) and says: *"to get it, call run again with `also` and a reason. The
  user decides."*
- **With `also`**, one card shows the command, then what opens (in tarae's words, from
  `Grant::shown`), then Claude's reason (faint: its words, not tarae's). Then the scopes. Docker's
  socket and the risky ports keep their warning line (`allow.rs:114`).
- Claude's **Read/Grep/Glob** outside the read set go to the same Grant card. That's today's read
  card, with `s` added.
- `outside_sandbox` stays as it is.

**Anti-nagging, which matters under prompt injection:**

- An `n` is remembered for the chat. The same ask again is answered `the user said no to <x> —
  ask them in words` without a card.
- At most 3 grant cards per turn. After that Claude is told to ask in the chat.
- Grants are never asked for in bulk. Each path or port is a line of its own, and none is home or
  above it.

### 3.6 Scopes, the list, revoking

Scopes are `run` (this call), `chat` (`Chat.allowed`, as today), `session` (new: kept in
`$state/sessions` with the session, gone when it ends), and `project` (`$state/sandbox.json`, as
today).

**Shown:**

- At grant: the toast, as now (`Claude's commands may read ~/src/other-lib in this session`).
- Always: when any grant past the defaults is live, the chat's title row carries `› 3 granted`
  (faint, clickable). That's the same place as today's `› no sandbox` (`tools::unsandboxed`).
- In Claude's run report, as now (`The user also let it …`, `sandbox.rs:132-135`).
- **`:granted`** (`:sandbox-allowed` kept as an alias; and a key under `space v`, chosen in the
  shot pass). This is a picker grouped by scope, newest first. Each row: verb, place (`~/`-spelled),
  scope, when granted. The defaults come first under a faint `by default` heading, so the user sees
  the whole picture and not only the exceptions. Empty: `nothing granted past the defaults — Claude's
  commands can read <checkout>, …` (faint, first row).

**Revoked:** `d` or Enter on a row (a default row says it can't be taken back here). A revoke
applies to the next run, because policies are built per run (`with_allowed`, `tools.rs:1574`), and
to the next Read at once. Claude hears it with the next message (`checkpoints.tell`: *"The user took
back: read ~/src/other-lib"*). Revoking a project grant writes `sandbox.json` on a worker (as
`keep_later`).

### 3.7 What the parser keeps

`readonly::parse` (words, `cd`, `; && || |`, `sh -c` once) stays, **for classifying only**:

- `tools::is_check` / `CHECKS`: whose places feed the user's list (#28, #32).
- the lineup's classes (`docs/design/async.md` §3.1): light, heavy, or exclusive.
- the lineup's dedupe key (async.md §3.4).

None of these decide safety. A misread line feeds the list wrongly or waits in the wrong queue.

---

## 4. Threat model

**Assets**: the user's secrets (keys, tokens, other projects' `.env`s), the user's *unsandboxed*
execution (their builds, preflight, terminal, git hooks), git history and refs, the network and
local services, and the user's attention (a stamped card is a lost guard).

**Who acts**: Claude making mistakes; Claude steered by text it reads (prompt injection in file
contents, dependency sources, command output, a hostile repository); and a hostile repository opened
in tarae (covered by project trust, unchanged).

| Path | Today | With capabilities |
|---|---|---|
| A run reads a secret and prints it to Claude | **open** for any code a check runs (tests Claude wrote, under review-at-end), and for any command the parser misjudges | the kernel denies it (`SECRETS` deny last); a card can open it for one run only, naming it |
| Claude's Read tool reads a secret | gated (`reads.rs`) | the same gate, the same list |
| A run makes a **hard link** in the checkout to `~/.ssh/id_ed25519`, then Claude Reads `./k` (real path in the checkout) | **likely open** through a check's code (same volume; `link()` needs no read of the source; the profile only denies writes, and whether Seatbelt counts a link as a write at its source is unmeasured) | **P2 blocker**: deny `file-link` in the profile and measure that it holds. Otherwise `reads::covered` refuses files with `nlink > 1` whose other names it can't place. Symlinks are fine (real paths both ways). |
| A run writes what runs later outside the sandbox | `.git`, `.tarae.toml` protected; **build.rs, `.cargo/`, `.envrc`, `.tarae.local.toml` open** | `PROTECTED` denied for runs (§3.3); Edit to them reviewed |
| A run poisons a crate in `~/.cargo/registry/src` | **suspected open** | `registry/src` write-closed while the network is closed (P1, measured); P1b: all of `registry/` (index, cache, src) and `git/` (db, checkouts), but cargo's lock, last-use and `CACHEDIR.TAG` files and its one file-sync mark (measured) |
| Claude widens its own grants | can't write `$state`, `~/.config/tarae`, `.tarae.toml` (trust-gated) | the same + `.tarae.local.toml`. A grant needs a card; the card states the effect, Claude's reason is secondary; `n` sticks; ≤ 3 per turn |
| Exfiltration over the network | closed by default; **`localhost:*` opens every local service** (this repo's `.tarae.toml` sets it) | unchanged, but stated on the `:granted` list as a default. Recommend per-port for this repo (the tests only need their own port-0 listeners) |
| Clipboard, keychain via mach services (`allow default`) | open: `pbpaste`, `security find-generic-password` for items whose ACL trusts `/usr/bin/security` | measure in P2; deny `com.apple.pasteboard.*` and the security server lookups if the toolchains don't need them |
| Deleting the user's work in the checkout | a card for `rm`; open through any check's code | snapshot and undo (§3.4) |
| Card fatigue | 40–80/h, answered by reflex | 1–3/h, each about something new |
| Resource abuse (disk fill, fork bomb, servers that never end) | open through checks | the same; the lineup and `:procs` (stray child, busy) show it, and stopping the run ends it |

**What this doesn't stop**: Claude reading the checkout and the repository's checkouts (that's the
job; it includes a `.env` in them, as today), and the user running a script Claude wrote, by hand,
outside tarae. Prompt injection can still make Claude *ask* for things. The card's job is to make
those asks rare enough to read.

---

## 5. Today's rules, mapped

| Today | Where | Fate |
|---|---|---|
| `RUN_ALLOW`, `allowed`, `allowed_words`, `part_allowed`, `chain_allowed`, `rule_matches`, `rule_env`, `env_allowed_words` | `tools.rs:486-635` | **gone** |
| `llm.run-allow`, `Chat.run_allow`, `pattern`, `Pattern`, `card_pattern`, `words_pattern`, `pattern_shown`, `RUNNERS` | `tools.rs:694-826` | **gone**; `llm.run-allow` warns "no longer needed" for a release, then goes |
| `INTERPRETERS`, `scripts_run`, `read_scripts`, `script_part`, `script_pins`, `Chat.run_scripts` (#93, #117) | `tools.rs:986-1121`, `readonly.rs:628-682` | **gone**: a script runs in the sandbox like anything else |
| `named`, `moved`, `showable`, `only_moves`, `Checkpoints.carried` (#76) | `tools.rs:1379-1502`, `readonly.rs:724-745` | **gone**: unseen code runs inside the capabilities; what matters is `PROTECTED`. `draws_as_itself` stays (hooks.rs). |
| `read_only`, `reads_only`, `COMMANDS`, `GIT`, `GIT_QUIET`, `git_plain`, `sed_prints`, `WRITES`, `FILTERS` as gates | `readonly.rs:693-864`, `tools.rs:511` | **classification only** (lineup "light"); no option lists |
| `outside`, `candidates`, `runs_outside`, `expand`, `SYSROOT`, `allow::sysroot` (#112) | `readonly.rs:866-1006`, `allow.rs:335-361` | **gone**: the kernel sees where a read goes |
| `unrolled`, `fill`, `VAR`, variable marks (#113) | `readonly.rs:255-511` | **gone** |
| `ENV_NEVER`, `env_allowed`, `one_command_env`, `env_words` (#44, #93, #114) | `readonly.rs:527-626` | **gone**: `RUSTC_WRAPPER=x` runs `x` *in the sandbox* |
| `CHECKS` (since 2026-10-05 the packs' `checks`), `is_check`, `filtered` | `tools.rs:567-692` | **stay** (the user's list, the lineup) |
| `Grant`, `Allowed`, `Store`, `keepable`, `SECRETS`, `NEVER_WRITE`, `write_choices`, `code_trees`, `defaults` (was `also_readable`), `checkouts` | `allow.rs` | **stay**: this is the capability store. `Grant::Read`, scope `Session`. |
| `grant_places` folder offers (#102) | `allow.rs:275` | **stays** for real outside reads; run outputs no longer ask |
| `reads::Made` / `Chat.ran` (#81) | `reads.rs:79-129` | **simplified**: temp folders are readable by default. `Made` goes. |
| `What::Run` with reads, patterns, scripts, unseen files | `tools.rs:1230-1372`, `offer.rs:69` | **only** `also`, socket, outside, no-sandbox |
| `What::Read`, `What::Grant`, `What::Outside`, `blocked.rs` | | **stay**; `Read` and `Grant` become one card |
| `Policy::profile` | `sandbox.rs:588` | **gains** read rules, `SECRETS` deny, `PROTECTED` names, `file-link` deny, maybe mach denies |
| run tool's description listing the read commands | `tools.rs:452-468` | **shrinks** to what the sandbox allows and how to ask with `also`. That's fewer tokens per turn, and Claude stops shaping commands around the parser (avoiding `$(…)`, splitting chains). |

Net: about −1,000 product lines and −1,200 test lines; +600 product and +300 tests (profile rules,
`also`, scopes, `:granted`, the snapshot).

## 6. Migration

- **`$state/sandbox.json`**: read, write and ports load as project-scope grants as they are. The
  schema gains `scope` and `granted` (a time) per entry; old entries get the file's mtime. Read
  entries now covered by a default (another checkout, temp, toolchain) are dropped on the first
  write, and the toast says which.
- **`Chat.allowed`** becomes chat-scope grants (it's the same type). `Chat.read_once`: gone.
- **`Chat.run_allow`** patterns and `Chat.run_scripts` pins: dropped. They allowed *commands*, and
  every such command now runs without asking. If a chat had any, the first run after the update
  notes once: `commands no longer need allowing — they run in the sandbox; :granted lists what it
  opens`.
- **`llm.run-allow`** (a setting, in `trust::RISKY`): read and ignored, with one config warning per
  start. Removed a release later. The settings test's round-trip still passes.
- **Cards in flight at `:restart`**: answered "tarae restarted", as now.
- **`a`/`A` keep their meaning** (this chat · always here). `y` is still once. `s` (this session)
  is the only new letter.

---

## 7. What the user sees per day

The mix of today's cards is an estimate. They aren't logged, and P0 replaces this table with
counts. It's grounded in the frictions (every one in §1.2 is a read-only or project-local command)
and in the log's 14 read answers (12 of them in the two new default classes).

| Card today | Share (est.) | After |
|---|---|---|
| reads outside, by tool or command: other checkouts, run outputs, toolchain, `/tmp` | ~45% | **0**: defaults |
| commands the parser can't place: xtask with env, scripts, awk/python, loops, `$(…)`, `cd` chains | ~35% | **0**: they run in the sandbox |
| edit reviews (counted in `waits()`) | ~10% | unchanged (not a permission) |
| a real widening: socket, outside, write outside, port, network, reads of other projects | ~10% | **a card, once per scope**: most become session or project grants the first time |

Today: about 40–80 an hour at 5–7 sessions (the user's count; the log's lower bound is 46 an hour
over 3 hours, 96 an hour at the peak). That's **300–500 a working day**.

After, excluding edit reviews: **1–3 an hour** while a project is new to tarae (its socket, its
database port, a sibling library it reads), falling to **0–1 an hour** once its project grants
settle. That's **10–25 a day**, most on the first day. At that rate a card is an event, and there's
time to read it.

---

## 8. Decisions (user, 2026-10-04)

All five recommendations taken:

1. **Any command runs unasked inside the capabilities.** The checks already run arbitrary code (§2).
2. **Deleting by undo, not by card** (§3.4) — with a condition: **P5 lands before (or with) P3**, and
   until it does today's card for `rm` and the like stays. So no build ever has neither.
3. **Home closed by an allow-list**, the list measured in P2 (what the toolchains actually read), never
   guessed.
4. **The network is grantable up to this session at most**; "always" stays the project's trusted
   `llm.sandbox-network`. Built in sandbox UX piece C1 (2026-10-16, `target/sb/sandbox-ux-pieceC-plan.md`): the
   network closed, every host is asked — `y` this run · `a` this chat · `s` this session · `n`; `Caps::storable`
   keeps a host to the session.
5. **This repo's `llm.sandbox-localhost` narrows to its tests' ports** (`llm.sandbox-ports`), so one
   `curl` can't reach every local service. To do: which ports the tests use — the agent/attach tests'
   listeners are port 0, so the list is what they bind, found by running them in the narrowed sandbox.

---

## 9. Phases

| Phase | What | Size | Pays off alone? | Risks |
|---|---|---|---|---|
| **P0. Log the cards** | One `card` line per answer (§1.3): kind, `why`, read `class`, `shadow` (the capability model's decision, computed beside today's), answer, time from drawn to answered. A `denied` line per run with denials. | ~150 lines + 2 tests. **½ day.** | **Yes.** It gives real numbers for §7, the stamp rate, and what the new model would still ask, before anything changes. | `shadow` must not drift from P3's real rule: share the function. |
| **P1. Close the write holes** | `PROTECTED` = `llm.review-ask` + `.tarae.local.toml` in the profile, as static name regexes under the `ROOT` parameter. `registry/src` write-closed while the network is closed (measure offline cargo first). Tests under the real profile (`what_the_user_granted_opens_under_the_real_profile` style). | ~150 lines. **1 day.** | **Yes**: two holes open today. Independent of everything else. | `cargo fmt` on an unformatted `build.rs` is denied (rare; the report says why). `cargo add` writing `Cargo.toml` is denied (it needs the network anyway). |
| **P2. Confine reads** | Measure first: run tarae's preflight, a Go and a Node project, `cargo xtask shot`, the e2e harness inside a profile with home read-denied, and collect the denials through `Watch`. That gives the home allow-list. Then: read rules from the capability set, `SECRETS` denied last, `file-link` denied (measure: hard link to an outside file from inside fails), mach lookups for pasteboard and keychain (measure). Read grants go into the profile (`with_allowed`). | ~300 lines + ½ day measuring. **2–3 days.** | **Yes**: closes the secret-read path the checks have today, even with every card unchanged. | Tools that read unexpected dotfiles fail with a denial. The report names it, `:granted` opens it, and the allow-list grows from data. Seatbelt cost of more rules (measure; tens, not thousands). |
| **P3. Decide by capability** | `tools::run`: no `also`/outside/socket and a sandbox → run. `also` → the Grant card. Read/Grep/Glob → the same card. Delete §5's "gone" rows. New run description. `n` sticks, ≤ 3 grant cards per turn. Defaults: other checkouts (`git worktree list`, worker, cached per HEAD watch), temp folders. | +400 / −2,200 (with tests). **3–4 days.** | The main gain: cards drop to §7's numbers. | Everything rests on P2 being right, so P3 waits for P2's measurements and a week of `denied` lines. On Linux nothing changes (no sandbox: every run still asks, as today). |
| **P4. Scopes and the list** | `s` (session scope, kept with the session), `:granted` grouped by scope with defaults shown, revoke with `d`, the `› N granted` chip, Claude told on revoke, `sandbox.json` schema step, migration notes, a shot pass (empty state, narrow, both themes). | ~400 lines + shots. **2 days.** | Yes, though it needs P3 to matter much. | The chip has to stay calm (faint, no jitter when the count changes). |
| **P5. A run's changes, undoable** | A write-tree snapshot before each non-check run (worker, unsandboxed, temp index). The run's changes join the turn's checkpoint and the turn review. `space u` takes them back. | ~400 lines. **2–3 days.** | Yes: deletions and run-made edits become reviewable and undoable. It goes before P3 (§8.2): `rm`'s card stays until it's in. | Snapshot cost in big untracked trees (measure on the 200k-file tree). Ignored files aren't covered (by design: build output). |

**Order (decided 2026-10-04): P1 and P0 together → P2 → P5 → P3 → P4.** P1 is another session's;
P0 is this one's. P5 moved ahead of P3 so that `rm`'s card only goes once a run's changes can be taken
back (§8.2). P3 must not start before P2's measurements and the `file-link` result.

**P0, as built (2026-10-04):**

- `src/cards.rs`: `Class`/`class` (where a read goes), `Why`, `Request` → `decide` → `Decision` (the
  capability model's answer — what P3 will decide by, so the `shadow` logged now is what it will do),
  the `card` line (`line`, `said`) and the `denied` line (`denied_line`, called where a watched task or
  test run ends — `task.rs`, `testing.rs`).
- `Offer.asked` (what a card asks, set where Claude's cards and the sandbox's are made: `tools::ask_with`,
  `ask_unprotected`, `reads::next`, `seeing`, `blocked`) and `Offer.drawn` (first drawn in front,
  `term::render_frame`). Lines come from the answer paths (`offer_accept`, `offer_dismiss`,
  `offer_lasting`) and `drop_cards`. Since cardburden 5 every card has its line: one with no `asked` (tarae's
  own — a preflight's, a stuck run's, a review's end, notes gone stale) as `why=own · shadow=own`
  (`Why::Own`, `Request::Own` → `Decision::Own`), the kind as `tarae state` names it (`Offer::kind`; it was
  `other`), and the answers by a card's own letter (`w` `f` `p` `X` `D` `R` `c` `N` — `offer_answered`) too.
- The user-visible gain: `allow::defaults` (was `also_readable`) adds **the repository's other
  checkouts** (`allow::checkouts` — read from git's own files, no git run, taken only when the links go
  both ways, so a `.git/worktrees` entry that came with a download can't name some other folder) and **the
  temp folders** (`Editor.read_temp`, off in tests, whose projects live there) to what Claude's tools and
  read-only commands read unasked. That's 12 of the day's 14 read answers.
- Tests: `cards::tests` (decide, class, both lines), `allow::tests::the_repositorys_other_checkouts_both_ways_only`
  (real git), `reads::tests::other_checkouts_and_the_temp_folders_read_unasked`,
  `tools::tests::each_answer_on_a_card_is_one_line_in_the_log` (drawn, `n`, outside, `dropped`).

Next, from a few days of these lines: the real §7 table, and the stamp rate.

**Before P2, a small batch (approved 2026-10-05)**, from a day of answering cards across six sessions — cards
behind missed for minutes (#90, #94), one session asking to run the same test outside the sandbox ten times in an
hour, each time read whole:

- **The inbox** (`src/inbox.rs`): every waiting card and edit of every session, oldest first, built on the UI
  state's cards (`Editor::ui_cards`) so the two can't disagree; `UiState.inbox` is its order. `space I` lists it
  (a picker: it's empty most of the day, a moment's choice when it isn't); the status line counts it; `C-q` goes
  to its first, any session.
- **Same as an earlier card** (`src/sinceyes.rs`; outside runs 2026-10-05, every run card cardburden 3): a run
  whose card was answered before since tarae started (`Session.answered`, the same run asked the same way)
  starts with `Same as card#41 — y at 18:02` and a row for each part of what it runs — scripts, a pack's
  `[claude] entry` files, build files, the rest named — and shows what it runs that changed (past the
  sandbox — outside it or with a socket — the project's whole hunks; a git tree per answer, the run's own
  snapshot, staged into tarae's own index). Kept until tarae quits, not with the conversation. It still waits
  for its key. The `card` line's `same=#41`/`same=#41:changed` counts it.

**P2, as built (2026-10-05)** — measured first (`target/sb/p2-plan.md`, `p2-measured.md`, the scripts beside
them):

- **Reads in home are confined** (`sandbox::Reads`, `reads_for`, `Policy::profile`): `file-read-data` and
  `file-read-xattr` denied under home. They're opened again for the project, the run's folder, every folder
  it may write in, cargo's files, and the measured list (`HOME_READS`: `~/.cargo`, `~/.rustup`,
  `~/.gitconfig`, `~/.config/git`, `~/.CFUserTextEncoding`, `~/go`, Go's cache and settings, `~/.npm`,
  `~/Library/Fonts`). Also cargo's and rustup's homes wherever they are, tarae's grammars, the folders on
  PATH, the repository's other checkouts, `.cargo/config{,.toml}` in the folders above the project (measured:
  a crate below a folder with one doesn't build without it), and **the user's read grants**
  (`tools::Widened.reads`: what Claude's Read was let read, its runs read too). Metadata stays open:
  every workload `stat`s home and the folders above the project thousands of times, and closing it breaks
  `getcwd` (measured).
- **Kept from every run, last** (`kept_reads`): `allow::SECRETS`, `~/.claude`, `~/.claude.json*`, cargo's
  `credentials*`, tarae's `$state`, with its log opened after. A kept place that holds the project (a checkout
  under `~/.claude/worktrees`) is kept but for the project.
- **Hard links**: no `file-link` rule. Seatbelt refuses a hard link whose new name would have any privilege
  its source lacks (`forbidden-link-priv<op>`, measured with `link(2)` itself). So a source the run can't
  read (`<file-read-data>`) can't come in. Neither can anything from outside the project and the run's
  folder: they can't bind unix sockets (`<network-inbound>`). Each lock was seen holding alone. A `file-link`
  rule would cover exactly the write rules' places, and denied everywhere it costs a denial per object file
  rustc links.
- **Mach**: `com.apple.pasteboard.*`, `com.apple.SecurityServer`, `com.apple.securityd` denied. No workload
  looked either up. `pbpaste` gets exit 1 instead of 0. `security`'s exit status can't tell a missing item
  from an unreachable keychain (44 both ways), but its only lookup is the denied one and the keychain files
  are kept.
- **A denied read opens like a write** (`Grant::Read`, `allow::read_choices`, `blocked.rs`): `space m s`
  offers the file's folder and those above it, up to just below home, never a kept place or one holding one.
- **Lost denial reports**: about 1 in 10 at random, also from processes that live on (measured), so waiting
  can't fix it. `Policy::said_in_output` takes a path from a failing line at or up to 3 lines before
  `operation not permitted` (any case: Go and Node print it lowercase). It judges the path by the profile's
  rules in Rust (`reads_allow`, `writes_allow`) and turns it into a denial "said in the output", for the
  report and for `space m s`. It replaces the cargo-caches-only fallback.
- Confirmed under this profile: the quick preflight (1117 + 35 + 15 tests), e2e (35), a shot, Go
  (build/vet/test/gofmt) and Node (node, npm test/run/ls) all pass. Left denied: Go's telemetry and npm's log
  writes (as before P2), `~/.zshenv` (a test starts the user's shell), `~/.vscode/extensions` and tarae's
  `java-debug` (lookups that find nothing), and a unit test's stray real `claude` (it reads `~/.claude.json`
  and the keychain: to be found and faked). The whole profile costs 7.5 ms a run over a bare `sandbox-exec`
  (24 rules, 68 parameters).

**Tokens in the settings a run reads (2026-10-05, after P2; the user chose copies after measuring)** —
`tokens.rs`, measured in `target/sb/tokens-checkpoint.md`:

- Where a file a run reads holds a token-like line (`tokens::token_like`: a URL with a password, a key
  named for a secret with a value written there, an `Authorization:` header, a known prefix), the run gets a
  copy without those lines in its `RunDir`, through the tool's variable (`GIT_CONFIG_GLOBAL`, `GOENV`,
  `PIP_CONFIG_FILE`, `NPM_CONFIG_USERCONFIG`). The file itself is denied after the allows
  (`Reads::copies.hidden`, `T<n>` parameters), unless the user granted a read of it. Made on the policy's
  worker, by tarae, never by Claude's run.
- `~/.npmrc` stays in `SECRETS` and always gets its copy. pip's settings joined `HOME_READS`: denied, pip
  loses them without a word.
- Cargo can't take a copy, so its config stays readable. The user is told once per file
  (`$state/tokens-said`), by a toast, not the run's report: only the user can move a token, and the report
  would point Claude at it.
- The read card, its folder for `a`/`A`, and `space m s`'s read grants say when the place holds a
  token-like line (`tokens::found_in`, on a worker), by file name only.

**P5, as built (2026-10-05)** — plan and numbers in `target/sb/p5-plan.md`, `p5-checkpoint.md`, the scripts
beside them:

- **The snapshot** (`src/snapshot.rs`): `git add -A` + `write-tree` into **tarae's own index per checkout**
  (`tarae-snapshot.index` in its git folder, first made from a copy of the real one), kept between snapshots.
  A fresh copy each time (what "since your last y" did) hashes every untracked file every time: 21 s at 200k
  untracked files against 0.4 s kept. Taken in `tools::go` after the saves (what the user saved isn't the
  run's), on a worker, for **every run of Claude's that isn't read-only** (`readonly::read_only` —
  classification, as §3.7 keeps it): commands, checks and `run_tests` alike. Checks run arbitrary code (§2),
  and a test that deletes files is what undo is for (one check here once deleted far more than it should).
  They pay ~40 ms before and ~41 ms after (`perf_snapshot_around_a_check`: 600 tracked files, an ignored
  `target/` of 21k files the check writes into). Read-only commands skip it: two snapshots cost ~110 ms on
  the runs Claude makes most.
- **The run's end** (`tools::run_done`, `run_dropped`, `tests_done`, `tests_dropped`): the tree again, `diff-tree`, both sides' blobs through
  one `cat-file --batch`. The changes join the turn as written unseen (`checkpoint::record_run`). The turn
  ends in its review and `space u` takes them back. Claude's report names them. The line's next run waits
  for this, so its writes aren't counted in the diff. Files the editor wrote meanwhile (`Editor::wrote`)
  aren't the run's. A turn that's over by then (stopped) gets a checkpoint of its own.
- **`Touched.gone`** (the run deleted it: undo writes it back, its folder too; redo deletes it again) and
  **`Touched.bytes`** (not text, or over 1 MB: git blob ids; undo and redo write the blob out on a worker,
  only over a file still as expected). Kept in the turns' JSON.
- **Measured** (macOS, APFS): 200k untracked files, first snapshot **91 s and 800 MB** of loose objects
  (unsandboxed; 95 s in the sandbox), then 0.38–0.85 s. So **past `NEW_MAX` (5,000) new files at once a
  snapshot keeps what it had** (`add -u`, the new ones counted): 0.18 s first, 0.17 s again, 0.21 s for a
  run's whole after-side, nothing written. The report says how many weren't kept. 5,000 new files: 2.7 s
  once, then 47 ms. This repository: about 55 ms a snapshot (about 20 ms of it starting git twice).
- **§3.4's question, decided: no `file-write-unlink` deny** (`target/sb/p5-unlink.sh`, `p5-rules.sh`). A deny
  on tracked literals refuses `rm`, `mv x y`, **rename-over** (`printf > tmp && mv tmp x`) and `sed -i`, which
  is how many tools save. In-place writes and `rustfmt` pass. Its cost grows faster than linearly: per run, 0 /
  1,000 / 2,000 / 5,000 literals = 9 / 189 / 726 / 4,548 ms. It breaks working tools, costs a fifth of a
  second at a thousand tracked files, and still leaves untracked files open. The snapshot covers all three.
- **Ignored files aren't covered** (build output): a run that deletes an ignored file (`rm .env.local`,
  `cargo clean`) can't be undone by `space u`. It is **named** in Claude's report as not kept, capped at 12
  names: the ignored files and folders are listed before and after (`ls-files -o -i --directory`, so an ignored
  folder is one entry and is never walked). A file deleted inside an ignored folder isn't seen; the folder
  going is. P3 decides whether such deletes ask. Links, submodules and mode-only changes aren't covered either
  (named in the report).

**Java under P2 (2026-10-05)** — measured with Gradle 9.8.0 (Homebrew), a 9.7.1 wrapper, and Maven 3.9.16
(the wrapper's cached distribution) on small offline projects, every read and write in home reported
(`target/sb/java-plan.md`, `java-checkpoint.md`, the scripts beside them):

- **Reads added**: `~/Library/Java/JavaVirtualMachines` (the `/usr/bin/java` stub finds no JDK without it),
  `~/.m2/repository`, `~/.m2/wrapper`, and in the Gradle home `caches/modules-2` and `wrapper/dists`.
- **Gradle's home isn't the run's to write** — compiled build scripts, distributions and dependencies run in
  the user's later builds. Each run gets its own (`sandbox::gradle_run_home`): the dependencies read in place
  (`GRADLE_RO_DEP_CACHE`), the distributions linked in (only the wrapper's `.zip.lck` files are written —
  `gradle_locks`), no daemon after the build. A small `./gradlew clean test` took 4.5 s cold and 3.7 s warm.
  (Linux, 2026-10-06: a Gradle project's home is kept per project and policy — the java pack's run home with
  `daemon` — and its daemon lives on, tarae its last parent: `reaper.rs`, `target/sb/subreaper.md`. +0% over a
  plain `gradle test`. Kotlin's compile daemon gets its registry there too; kept homes are pruned at start —
  `runhomes.rs`, `target/sb/kept-homes-plan.md` §5.)
- **The daemons' registry is kept** (`GRADLE_KEPT`): with it readable and localhost open, a sandboxed
  `./gradlew` handed its build to a daemon started outside, and that build wrote a file the sandbox keeps.
- **Gradle needs localhost**: its client reaches its own daemon, even with `--no-daemon`'s single-use one, over
  a random port. With the ports closed it can't build at all, and the report says so (`llm.sandbox-localhost`).
- Maven `compile` passes offline under the profile with no denials. Its `test` couldn't be measured offline
  here: surefire's dependencies were never downloaded.
- **Protected names are the packs' (user decision, 2026-10-05)**: each `pack.toml` lists what runs later
  outside (`protect`). Rust has `build.rs`, `Cargo.toml` and `.cargo`; JavaScript `package.json`; make
  `Makefile`, `GNUmakefile` and just's files; Java its build files, wrappers, `buildSrc` and `.mvn`. The core
  keeps only `.tarae.toml`, `.envrc`, `.claude`, `.github`, `.vscode`, `.tarae.local.toml` and `.git`
  (`sandbox::CORE_NAMES`). Every built-in pack's list applies in every project, read once from the binary
  (`sandbox::protected_names`): the profile, the report and Edit's ask all use it, and `llm.review-ask` only
  adds (its default is now empty). The names go into the profile as several regexes, because Seatbelt refuses
  a string literal past about 1 KB ("Error reading string": 1032 characters refused, every run failing).

**P3, as built (2026-10-15, cardauto piece 4)** — plan in `target/sb/cardauto-plan.md` §1.2–§1.3:

- **The gate** (`tools::run` → `in_sandbox`): sandboxed here (`cards::sandboxed`: not off, not missing, not
  partial) and nothing past it asked (no `outside_sandbox`, `unix_socket` or `also`) → `cards::decide` on a
  `Run { past: No, reads: [] }` → it runs. Where no `Caps` could change that (`cards::decide_now`, also the
  shadow's `unbuilt` rule) it's decided at once, so a run is admitted as a check was (the queue's tests hold);
  when a warning that `asks` could come true (D4: the network open) or the line moves a file Claude wrote unseen,
  a worker builds `Caps` (`Basis::build`), reads the scripts for the warnings, and carries the unseen mark
  (`carry`) first. `Outward` → today's run card. Not sandboxed: today's gate, unchanged, until piece 5.
- **`also`** (`also_of`, `also_ask`, `also_grant`): one of `{read}`, `{write}`, `{port}` with a reason — and since
  piece D `{localhost}` (every port here, `Grant::Localhost`: its card names the risky ports it opens too),
  `{env: NAME}` (one variable left out, passed whole — `Grant::Env`; never a name `readonly::env_allowed` keeps
  off; judged by the session's last run's environment; a token-like name this session at most) and
  `{terminals}` (macOS). Resolved on
  a worker against `Caps` as a grant card for a blocked run offers it (`blocked::choices`): open already → runs;
  no card opens it (home, keys, what runs later, a risky port, a protected name in the project) → said, no card;
  else the Grant card (`blocked::also_card`, `Why::Also`, Claude's call waits on it — `BlockedRun.claude`).
  `y`/`a`/`A` run Claude's command with it; `n` is said to Claude and sticks for the chat (`Chat.also_refused`);
  `ALSO_MAX` (3) cards a turn (`Chat.also_asked`, new each turn).
- **The report** (`also_candidates`, `also_line`): only a run that *failed* gets the `also` line (§8.4: a
  passing cargo test of this repository reports taplo walking up from its folder, mDNSResponder and the test
  binary's own probes); and only for a read or write of a place, a port the settings don't open, a terminal —
  never a look at a folder above the project or the run's own folder, a socket, a host. A port refused to a
  program a pack's `refused-port` names (Gradle's JVM, which picks its ports as it starts) is `{localhost}`,
  not that port — but below `PICKED_FROM` (1024, never a picked port: Seatbelt names no address, so it's a host
  beyond this machine) neither that card nor `{localhost}`: the report says the refusal, and with the network
  closed the pack's `beyond` line; a variable left out that the output names is `{env}` (`RunEnv::named_in`).
- **Gradle's loopback** (piece D2, `tools::loopback_card`): that refusal in a failed run of Claude's puts every
  port's card up at once, in the pack's `card` words, Claude's call waiting on it with the run's report
  (`BlockedRun.report`): `y`/`a`/`s`/`A` run it again as Claude's run; `n` gives Claude the report and the no.
  Open already, the turn over, or a no in this chat: no card. Follow-up once piece C is on main: before the
  first run, by pack data naming what needs loopback. The kernel's report of that refusal is often lost (the
  shots: 3 or 4 of 6 under load; real Gradle with localhost closed: 0 of 4, `pieceD-gradle-measure.out`), so a
  connect is also read from the output as the kernel would have said it (`Policy::connect_said_in`, the packs'
  `refused-connect`): only words naming the port — macOS's nc, and Gradle's "Could not connect to the Gradle
  daemon." with the address its daemon gave its clients in the log tail it prints (`when` + `line`). A JVM's
  own `SocketException: Operation not permitted` names none.
- **A rerun from `space m s`** (piece D3, the user's Q2): when it runs again a failed run of Claude's last
  turn (`Chat.failed`, cleared as a turn starts), in the same conversation, with the chat idle, its end goes to
  Claude as a message by itself (`tools::told`, `chat::submit`); the card says so (`blocked::then_said`) by the
  state as it's made, and the end looks again. Otherwise, with the next message.
- **Kept for the cards P3 keeps** (outside, socket, and no sandbox): `named`, `showable`, `read_scripts`,
  `carry`/`moved` — `tools::the_frictions_commands_run_unasked_in_the_sandbox` checks the outside card still
  shows a script written unseen this turn, and refuses one too long to show.

---

## 10. Linux

The model is sandbox-agnostic. Landlock (the next round) is *deny by default, allow by path*, which
fits capabilities better than it fits a parser: reads and writes from the same set, TCP connect by
port from ABI 4 (Linux 6.7), and scoped unix sockets from ABI 6 (6.12). Until then Linux keeps
today's no-sandbox card per run (or `a` for the chat). The parser didn't spare Linux any cards
either.
