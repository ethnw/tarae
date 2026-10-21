# Roadmap

tarae aims to be an editor you can hand real work to: Helix's keys, Claude built in, several branches of work going
at once, and nothing that makes you wait. Running Claude's commands is only worth it if what they can reach is
enforced by the system, and if the cards and reviews that are left are few enough to read. Most of what comes
next follows from that.

How to read this file:

- **Done** is what's in the code today, checked against it.
- **Now** is under way. **Next** comes right after, in order. **Alongside** is done between them, in no order.
  **Later** is planned but not scheduled closely.
- Items are in the order they're meant to happen. Each links its design document where there is one
  ([`design/`](design)).
- "Not confirmed" marks a status that couldn't be checked against the code.

For how tarae is built and why, see [architecture.md](architecture.md). For what each feature does, see
[features.md](features.md) and [claude-integration.md](claude-integration.md).

- [Done](#done)
  - [The Linux sandbox keeper](#the-linux-sandbox-keeper)
- [Next](#next)
- [Alongside, not ordered](#alongside-not-ordered)
- [Later](#later)

## Done

| Stage | Scope |
|---|---|
| **M0–M2** | Core model (rope, multiple selections, transactions, undo), movement and editing, a non-blocking event loop, config as data with live reload, performance budgets as tests, tree-sitter highlighting, themes, pickers, search |
| **M3** | Language servers: diagnostics, navigation, hover, completion, signature help, inlay hints, code actions, rename, format |
| **M4** | Claude: select → instruct → diff, the chat panel, follow mode, notes on lines, every edit reviewed before it's written |
| **Experience, Workbench** | which-key, palette, tutor, start screen, mouse, side panel, git tab, merge conflicts, tests, debugger, built-in terminal |
| **M6** | Plugins in Rust, compiled to WebAssembly, sandboxed on tarae's own runtime ([plugins.md](plugins.md)) |
| **Sessions** | Several branches of work at once, each in its own checkout with its own panes, chat, terminal, tests and tasks. A session is brought into its base by fast-forward, or by a rebase whose conflicts its own Claude resolves ([claude-integration.md](claude-integration.md#rebasing-a-session)) |
| **Review mode** | A pull-request-style review over the real buffers, comments sent in one go. Claude's turn is reviewed at its end and can be undone as a whole, a run's file changes included. A second Claude can review the changes, and risk grades mark what a person must look at ([code review](claude-integration.md#code-review)) |
| **Cards and the inbox** | Questions come as cards, one calm stack. The inbox lists every card and edit waiting, in every session. Phases P0–P5 of the card model are built, and `:granted` lists what Claude may do without asking ([design/cards.md](design/cards.md)) |
| **Sandbox, macOS** | Claude's commands run under Seatbelt: writes only in the project, never to files that run later outside the sandbox; reads in home confined, keys and tokens never readable; the network closed unless opened ([the sandbox](claude-integration.md#the-sandbox)) |
| **Sandbox, Linux** | The same policy, enforced by Landlock for files and seccomp for sockets, with tarae deciding each address. Each session's runs are forked by a keeper of its own, in namespaces of their own where the system allows it. Sessions are kept apart as far as the system allows, and each run says its tier ([on Linux](claude-integration.md#on-linux), [the keeper](#the-linux-sandbox-keeper) below) |
| **Repository trust** | A project's risky settings, and a repository's own git config and hooks, wait for a yes before anything of theirs runs |
| **Queues** | A run waits its turn instead of being refused, and says what it waited for (phases 0, H and 1 of [design/async.md](design/async.md)) |
| **UI state** | What's on screen as data: `tarae state`, a socket for outside programs, Claude's `ui` tool, ids that are links, pointing at things (phases 1–3 of [design/ui-state.md](design/ui-state.md)) |
| **Notifications** | Toasts as one calm card, `:messages`, news from sessions behind, the inbox's count on the status line — all inside tarae |
| **Restart with sessions** | `:restart` brings back every session as it was, with its chat, code review and waiting cards; unsaved edits are kept and offered back after a crash ([features.md](features.md#updating-without-losing-your-place)) |
| **Language packs** | A language is a folder of data: grammar, queries, servers, tests, debugger, and what its tools need from the sandbox. The core names no language |
| **Shared rust-analyzer** | One server for a repository's checkouts, swapped in the background as sessions come and go |

The Claude Code IDE protocol (M5) was retired on 2026-10-04, once the chat did all of it inside tarae.

### The Linux sandbox keeper

Done 2026-10-14. The design is [design/sandbox-keeper.md](design/sandbox-keeper.md). It gives every session one
long-lived keeper that forks its runs, so what a run leaves running is the session's and ends with it, and sessions are kept apart
from each other. Daemons such as Gradle's stay warm between runs.

1. **Landlock's network rights** — done. The kernel refuses a run's own connects and binds; only tarae makes them.
2. **One keeper per session** — done. A fork server and child subreaper on every system.
3. **Namespaces by tier** — done. Where user namespaces work, the keeper is PID 1 of the session's own user,
   network, IPC, PID and mount namespaces. Each tier, from A+ (all of them) to D (none), says what it keeps apart.
4. **Routing every socket** — done. The supervisor routes each address-bearing call by one table: to the session's
   own loopback, to this machine, or out. An outbound socket is replaced by one made in tarae's own network, so
   the session's network stays closed. Link-local, metadata, multicast, broadcast and unspecified addresses are
   refused. Measured: a cost of about 0.2 s per Gradle run, seen only in tier A+.
5. **The egress proxy** — done. One proxy in tarae, as threads: connections go out by host
   name. It reads the HTTP CONNECT line and the TLS SNI, and never decrypts; plain HTTP goes through it too. A host
   the run's rules don't name waits for the user's card; `llm.sandbox-hosts` lists the ones allowed. On macOS each
   run gets a port and a token of its own. On Linux the relay splices a connection once it has read its start.
   Tools that ignore the proxy variables are told by their packs. Where tarae itself goes out through a proxy,
   it chains to that one, http:// or SOCKS5 (host and SNI checks kept; the address checks are that proxy's, said
   in the report).
   Later: an `https://` upstream, and Negotiate/NTLM.
6. **The transparent proxy** — done (6c–6f′ on main). In tiers A+ and A, sockets tarae plants in the session's
   network take every connection and UDP datagram (TPROXY), marked per run and program. DNS is answered in the
   session with synthetic addresses, and no proxy variable is set. Raw TCP to a name and a name's SRV/TXT records
   each ask on a card of their own. The supervisor is slimmed. Docker's default profile is served by copies in
   tier C. Elsewhere step 5's per-run proxy stays.
7. **Sessions kept apart** — done. Tests that one session can't reach another's processes, sockets or files, on
   Linux in every tier and on macOS, and the fixes they found: a run's folder no other run can write, on macOS no
   signal or terminal outside a run's own. The keepers' memory with many sessions, measured; tarae's log names each
   session. A run asks before it reads a file named like a key inside the project (`.env`, `*.pem`, `id_*` — the
   names in runtime/secret-names.txt).
8. **The loopback rule** — done. Where sessions share one loopback (tiers without a network namespace), a
   session reaches only its own listeners and binds onto none of the others' ports.
9. **Showing the tier** — done. What a run's sandbox keeps apart and shares, said in Claude's result, the sandbox
   panel, `:procs`, `tarae state` and the run's environment, and how tarae's AppArmor profile would raise it.
10. **Docs** — done. The design's tier table is checked against the code by a test.

## Next

In this order.

### 1. Publishing

First (decided 2026-10-15), because the Cyber Verification Program's approval unblocks the fuzzing work in §2
that the model's safeguards stopped (the control messages); stabilization (§9) waits.

Items 1–3 are done (2026-10-16); 4 and 5 are the user's: the repository made public, then the application.

1. **What's in flight finishes** — done: the card model's P3 piece 4 and the diagram's P3 layout.
2. **`SECURITY.md`** at the repository's root — done: where to report (`security@sinabro.io`), what's in scope,
   how reports are handled. The address receives mail: it's forwarded like the project's other addresses, and the
   user's test mail arrived (2026-10-15).
3. **The pull requests** — done: PR #4 and PR #5 merged on GitHub; Q1 (§2) is in, before the repository goes
   public.
4. **The repository made public** — by the user.
5. **Anthropic's Cyber Verification Program**, applied for by the user as an individual, on the open-source track
   ("Maintainer of a widely depended-on open-source project"; an individual with maintainer authority can apply).
   What it needs: maintainer status, shown by the public repository; a one-to-three-sentence scope ("Security
   engineering for tarae, an open-source code editor I maintain: building and testing the sandbox that confines an
   AI coding agent's commands, on my own machines and test VMs"); the security contact, `security@sinabro.io`. No
   homepage needed. The hope: the sandbox's own testing (seccomp, fd passing, namespaces) goes on without the
   model's cyber safeguards stopping it. The risk: the track names "a widely depended-on project", and a
   repository made public that week may be turned down. If it is, §2's stopped items stay deferred as they are
   (`cyber-verification-items.md` keeps them) and the user applies again once tarae has users; nothing else in
   the order changes.

After it, the rest goes on in this order.

### 2. Sandbox validation

The aim is to state what the sandbox guarantees and show that it holds.

1. **One threat-model document** — done: [threat-model.md](threat-model.md), what is kept from a run and what
   isn't, per tier and per OS, each claim with its test; and the `claude` process itself confined by default on
   both OSes (macOS: Seatbelt, V1d; Linux: Landlock on the user's own `~/.claude` and login, granted whole — G25
   —, and a seccomp filter that refuses unix sockets, G24; claudeown, user decision 2026-10-16 (B)). tarae's own
   claude folder and second login are gone; where the rules can't be made claude starts as the user and says why
   (`llm.claude-sandbox-required`: not at all).
2. **Fuzzing the parsers that read what a run sends** — the supervisor's reading of system-call arguments, and
   the proxy's parsers (seeded loops in `cargo test`, `cargo fuzz` targets by hand —
   [sandbox-keeper.md](design/sandbox-keeper.md), threat model G12). Steps 1–3 done (`bc1b91e6`): what the
   supervisor reads of a run's memory is `wire.rs`, built and tested on every OS; `sendmmsg`'s offsets can't
   wrap; a unix name is bound by the bytes it was checked by; property loops over all of it. Steps 4–5 done
   (`c38b1938`): the readers checked against the kernel's own answers and by seeded loops; three findings, two
   fixed (a preflight step can no longer say a step ended, V2-1; a thread named past ASCII no longer makes the
   supervisor refuse its every call, V2-3). Step 6 done (`6200c01c`): each reader's properties are one function,
   `check`, called by its loop in every `cargo test` and by its fuzz target in `fuzz/parsers`. Step 7 under way
   (2026-10-16): an hour per target on a 16-core Linux VM. Two findings so far: V2-4, a control length with a null
   pointer, which tarae sent on without control where the kernel answers EFAULT (fixed; a Linux test pins the
   kernel's answer), and V2-5, in the tests' fake memory, not the reader. Q1 done (2026-10-16): of the control
   messages it forwards only descriptors go (`SCM_RIGHTS`, at most 253), every other type refused and named in
   the run's report, and `sendmsg` gets a buffer tarae built. Still open: fuzzing that forwarding further waits
   for Anthropic's Cyber Verification Program (§1; the model's safeguards stopped that work twice); a run's own
   `setsockopt` is unchanged (its own rights).
3. **A "must be refused" property suite**, run on several kernels on a schedule. With the transparent proxy:
   nothing a run does changes the session's rules, and nothing is reached but through tarae's sockets.
4. **Yama ptrace scope 2**: today tarae can't serve a run's sockets there and falls back to plain mode.
5. **A speed comparison** against a bubblewrap-style sandbox.

### 3. Frictions, and lighter cards and reviews

Done so far, under way, and the order after it (2026-10-16):

- **The compatibility suite** (below): pieces 0–3 done, up to the first macOS table and its findings B1–B7.
  Piece 4 done (`8993751a`): an image per toolchain, a volume per stack, and the first Linux tables, A+ and C
  (Ubuntu 24.04, kernel 7.0). Piece 6's first fixes done (`ca5b79bc`, B1–B4 and B8): Python's bytecode,
  golangci-lint's cache and uv's cache are the project's own run homes, never the user's. The rest of piece 6
  next.
- **The card model**: P3 done (cardauto pieces 1–4). P4 done: scopes (piece 6, `30a38311`: `s`, this session, on
  every card whose grant may outlive the conversation) and the grant list (piece 7, `951dcdb4`: `:granted`).
  Piece 5 is §9.3's.
- **The sandbox UX pass** (user direction 2026-10-16): tarae builds its own sandbox runtime, so every restriction
  it adds — a login, a card, a held setting — is tarae's to make pleasant, held against what other tools ask
  (`target/sb/sandbox-ux-*.md`: the inventory, the comparison of six tools, the proposal, the user's decisions).
  One conversation a piece, in this order:
  - **A** — done (`f80f38f6`): the chat no longer waits for the project's Claude settings card. claude starts at
    once with them held back, and `y` gives them to it.
  - **B** — under way: no sandbox, or a partial one, asks once a session (`y` ask each time · `s` run unasked this
    session · `n` don't run them), never kept across a restart. And the API-key card when claude isn't logged in
    and a key is held back: `y` this session · `A` writes `llm.environment-key = "keep"` to the user's own config.
  - **C** — under way, parts C1, C2, C3: the network for Claude's commands. With the network closed, the first
    connection to a new host waits on a card: `y` this run · `a` this chat · `s` this session · `n`. "Always"
    stays the setting, `llm.sandbox-network`. The card suggests the pack's registries; a suggestion is never a
    grant, the user's `y` is. For Rust and Go, tarae's own fetch as well (`cargo fetch`, `go mod download` with
    `GOTOOLCHAIN=local`): it runs outside the sandbox and runs no project code, so the toolchain caches stay
    closed to Claude's commands. Pack data; the core names no tool.
  - **D**: every refusal Claude can ask for has its `also`. Gradle's loopback gets a card at or before the first
    refusal, with `s`/`A` (no pack default). A rerun from `space m s` sends its end to Claude by itself, only with
    the chat idle, in the same conversation, for the last turn's failed run; the card says so.
  - **F**: words and memory — a hook's yes remembered (a rewritten hook asks again), one vocabulary for the
    answer rows, manifest tools (`cargo add`) said before they run, the docs.
  - **H**: Claude's `commit {message, paths}` tool. The card shows the message and the diff; `y` commits through
    tarae's own git with no hooks, `n` does nothing; it never pushes.
  - **E**: one list — settings and trust answers join `:granted`, each taken back with `d`.
  - **I**: one trust card when tarae's, Claude's and git's wait together as the repository opens; a later change
    asks only about its own part.
  - **G**, manifest tools run in a copy (`cargo add` → a change set of `Cargo.toml` and `Cargo.lock`): only after
    C, and only if `cargo xtask cards` shows manifest refusals often.

  Review at the end stays off by default.
- **After the sandbox UX pass**, from reading agent-security papers (2026-10-16: Progent, CaMeL, a
  prompt-injection SoK, AgentBound, Scaffold Effect, Meta-Harness):
  - **Injection that travels between agents.** Another checkout of the repository is read unasked, and the
    reviewer's notes reach the author's Claude. A row in the threat model (how it's delivered, how it travels),
    and a mark of where text came from on what's read from another checkout or a reviewer's note.
  - **A property loop in `caps.rs`**: no change made without a card ever widens what a session may do. Changes
    that narrow apply at once; widening always asks. Progent's split, without a solver: tarae's grants are sets
    of paths, ports and hosts.
  - **C1b (built, 2026-10-17)**: with `llm.sandbox-network` on, `llm.sandbox-hosts` defaults to the packs'
    registries (others asked); `"*"` is any host. It narrows G4.
  - Later: `cargo xtask compat --propose`, a pack-data draft from the refusals a stack met, for a person to
    review, never applied by itself. At Plugin API 0.3 (§7), a plugin's WebAssembly imports checked against the
    permissions it declares.
  - Not taken:
    - micro-VMs — another threat model, and no Firecracker on macOS;
    - a proxy that injects credentials — it needs TLS interception, and tarae's proxy never decrypts;
    - warming a run before the call arrives — not until `timing` shows hundreds of milliseconds;
    - applying a policy draft without review.
- Then, in this order (decided 2026-10-15):
  1. **Code mode**: fewer round trips a turn. Each tool call reads the whole conversation again, which is most of a
     turn's cost (measured 2026-10-14). Research: `target/sb/research-auto-code-mode.md`; a plan first.
  2. **The review lens** (below).
- **Auto mode** is built only if the card record shows it's worth it: one to two weeks of `$state/cards` after P3
  runs in the user's tarae, each class's residual rate held against D1's bar (at least one card of a class per
  session-hour; then the 300-card safety bar). Likely not built.
- **Frictions**: batch 24 under way.

Small fixes found while using tarae every day:

- The same safe command asks every time — for example, a card for the Docker socket that can be answered only for
  one run at a time. Docker's socket stays `y` per command (#193, user decision 2026-10-15); like cards are grouped
  instead.
- Small core-only-core moves from the size audit (2026-10-09; its moves are ~40 KB in all, so the reason is the
  rule, not the size): `term.rs` `output_style`'s pass/fail lines → pack data; the `.java` frame in
  `test_results` → a pack regex; `SELF_SKIP` → Rust pack data. (Done: the Docker socket names are the containers
  pack's `engine-sockets`; the files never looked in for a token are the code packs' `not-settings` and a core
  floor.)

**IMPORTANT — the card and review burden.** Cards and reviews must stay few enough to read, or they protect
nothing. Risk marks precise enough to trust (one large review had well over a hundred high-risk marks); like
cards and marks grouped; fewer wrong cards. This includes the card model's last two phases: **P3**, deciding by
what the session may do rather than by what a command looks like (done), and **P4**, scopes and a list of grants
(done — [design/cards.md](design/cards.md)). And a review lens: links between what Claude claims and the code, an
evidence panel, "explain this hunk".

**A "must work" compatibility suite for the sandbox** (2026-10-15), ahead of §2's items 3–5 in priority. The sandbox
must not be too strict for an open editor, and this repository, Rust only, never shows what other stacks hit. So
popular stacks' everyday commands (install, build, test, lint, format, a dev server) run inside tarae's sandbox
regularly, exactly as Claude's runs are, on macOS and on Linux's tiers, with a table per stack × command × OS. A
breakage is found before a user meets it, fixed in pack data or the core, and kept from coming back by an entry the
suite runs again.

### 4. Shuttle keys

A **shuttle** is a program that works a running tarae from outside: an AI assistant, a test script. ("Agent" in
tarae means a session's own Claude.) Today a shuttle reads tarae's state as data (`tarae state`). Shuttle keys
let it send keys too, in a way that can't land somewhere it didn't mean:

- Each key says where it must land (`card#N` at a given revision), or it is refused.
- Off by default. On only through the user's own config (`ui.shuttle-keys`, a risky setting); never a project's.
- While one is connected, the status line says so (`shuttle connected — a program is typing into tarae`), a key
  cuts it off, and each card's record says who answered it: you or the shuttle.

The person at the keyboard already has the same guards: a card ignores keys typed as it appears, `y` counts only
after the text was in sight, and no key changes state in silence. This was "guarded keys", phase 4 of
[design/ui-state.md](design/ui-state.md), dropped there and now back under its new name.

### 5. A remote split

tarae as a host — sessions, Claude, runs, the sandbox — and its interfaces — the screen and the keys — apart,
joined by one thin protocol: the screen's changes go out (tarae already draws by cell diff), keys come in. An
interface reaches the host from anywhere the host can be reached; how it authenticates and encrypts is decided in
its plan. Two interfaces:

- **The terminal**, as today.
- **A web client**: the thin client built to WebAssembly, in a browser. The same screen, not a GUI.

The egress proxy is designed to live with the runs, wherever they are.

### 6. Working together

Two or more at one tarae — a person and a person, a person and a shuttle, two shuttles (pair or mob
programming). Builds on the remote split — one host, several interfaces, people and shuttles through the same
protocol — and on shuttle keys.

- Everyone gathers in the main session by default. Each edits the code directly and sees the others' edits as
  they happen: one host holds the buffers, and each person has their own cursor, selection and mode.
- Everyone sees who is where (presence, following someone), and each card's record says who answered.
- Each has their own chat (their own `space l`) and their own Claude, signed in as them: the work is theirs, and
  so is the bill.
- Anyone can open a session of their own for work apart, and bring it back as today.
- The shared room chat is for the people to talk. What an agent made goes there only when its person sends it.
- A session's cards are its owner's, or a shuttle's working for them. In the main session, a Claude's cards are
  its person's.
- Open, for the plan: how one joins and is let in, two people (and two Claudes) changing the same lines, how
  agents in the room are paced.

### 7. Plugin API 0.3

One batch with the forge plugin: regions, cards, tools for Claude, a masked prompt, notes, the repository
([plugins.md](plugins.md)). The database plugin waits for its regions ([queued](#queued-not-scheduled)).

### 8. The forge plugin

GitHub, read and write, through `gh`'s login. Pull requests, CI, review comments beside the code; create, comment,
approve, merge — every write on a card.

### 9. Stabilization before release

No new features: the release waits on numbers, not a feeling (2026-10-15: every finished piece still held 3–5
bugs its reviewer found, two of tarae's own gates misbehaved in daily use, four tests failed only under load).

1. **The bar**: no test that fails only under load or only sometimes; every test green three runs in a row on
   macOS and on every supported kernel (§2's set); a set period of building tarae with tarae with no new serious
   bug.
2. **Bug hunting**: tests that vary what tests usually hold still — the order state changes in, the screen's
   width, the load — and the property loops widened with real output.
3. **Less code**: what the card model's P3 replaced goes (its piece 5: the parser's gates, ~2,200 lines), and
   whatever else the hunt finds unused. The binary with it: back under the old budgets (12 MB, 17 MB bundled —
   raised to 13 / 18 on 2026-10-15 at 11.98 / 17.07, from 8.7 / 12.5 on 2026-10-04) or a reason why not.
   `cargo bloat` first, for where the size is, then what P3 piece 5 removes and what the hunt finds unused.
4. **Independent reviews of the sandbox** by Claude sessions that didn't build it: each given the threat model's
   claims and the code, not the design's reasoning, trying to show a claim false; their findings fixed like any
   reviewer's. A human security review when one can be had.
5. **A small beta**: a few people use tarae first; what they hit goes into the suites.

### 10. The homepage

tarae.sinabro.io. An animation that feels like several agents working at once, and the README's demo, both from
one scripted recording: `cargo xtask shot` extended to record frames, Claude's parts played by the tests' fake
claude, so it can be recorded again when the UI changes. To do: the recording script, the scenes, the site, the
domain. The scenes are picked with the user: a chat edit and its card, the end-of-turn review, a commit from the
git tab, search, a short Kubernetes one.

- Claude's startup program, applied for as the organization (the Claude Console organization, already made) once
  tarae is public and has users (decided 2026-10-09). The Cyber Verification Program moved to §1.

## Alongside, not ordered

- **Linux in real use**, on test machines:
  - The terminal, the clipboard, file watching, Java (jdtls), the Kubernetes plugin, WSL, and working over ssh.
  - Several distributions, kernels and languages, and a large repository. Crash recovery, with no work lost.
  - A BPF measurement kit for development, outside the tarae binary. Its start is in [`trace/`](../trace)
    (`tarae-trace`, see [CONTRIBUTING](../CONTRIBUTING.md)).
- **A first-use check.** which-key, the palette (`space ?`) and `:tutor` exist, and the start screen lists
  `:tutor`. Go through them as a first-timer: do they cover sessions, review mode, cards, bringing a session in,
  the git tab?
- **A memory card and panel**: what Claude remembers for the project, in Claude Code's own format.
- **The run-flow panel**: one run's processes, writes, connections and refusals, from sources that need no
  privileges, on macOS too.
- **A grant scoped to Docker.**
- **코드멍** (*code-meong*, "code-gazing", as one gazes at a fire): Claude's code typed out character by character
  instead of appearing at once, maybe in the chat too. A plan first.
- **글타래** (*geul-tarae*, "a skein of writing"): a visual pass. Design details that suit ink (meok) and hanji
  paper, and a way of showing and generating text that suits tarae. A plan first.
- **Quoting** (인용, user 2026-10-17; `quote` in the docs and commands, as pointing is `point`): select part of
  any text tarae shows and ask about it at once. Code has its selection, and test and preflight output open in a
  buffer (`space x o`, the task panel's `o`); pointing gives a whole element's id, `C-s` the whole screen. Missing:
  the chat's transcript (a line of an answer, a code block), the terminal panel (its screen and scrollback — what
  the screen holds, never keys typed) and the run panels as drawn (task, test, preflight; a plain task's output
  has no buffer). How is the plan's: Helix's selection keys in a read-only view of that surface, a mouse drag, or
  both; the quote is a chip over the input like pointing's and a screen's, Backspace takes it off. What goes: the
  text with where it came from (`<quoted from="terminal" …>`), cut to a limit and said so; Claude's level rules
  hold (nothing unsent, other sessions never). The terminal's rows follow `llm.screen-terminal` (off → not
  quotable); a quote is the user's act, never a card. A plan first; each new screen element after its shots.

## Later

### Queued, not scheduled

- **Cost-efficient by default.** Every tool call re-reads the whole conversation, so what tarae's own tools return
  and what each message carries is most of a turn's cost. Principles (decided with the user 2026-10-10):
  invisible by default; a summary never hides the original (it can always be fetched); thrift doesn't depend on
  billing — one setting, user config only, `llm.thrift = "off" | "normal" | "strong"` (default `"normal"`):
  normal = the invisible ones for everyone (summaries with the original fetchable, ranges, a stable prefix);
  strong = also tighter output and a quiet hint to start fresh at ~30 % context (never a card); off = the
  originals as they are. Billing changes no behaviour: the first switch to the API only adds a one-time hint that
  the setting exists. Measure first — the `tokens` lines in tarae.log (sizes per tool result and per message's
  editor context, by session) and `:usage`'s table of tool results by tool. Ideas, to be ranked by that
  measurement:
  - run output summarized from the packs' test-result parsing, the full log fetchable;
  - read and search tools returning ranges by default, with "more";
  - a stable prompt prefix: the volatile editor context last, and small.
  - (user, 2026-10-10) **Sessions on the Messages API directly — later.** A backend beside Claude Code: a
    session can run on tarae's own agent loop over the API (billed to the key), so tarae controls the whole
    prompt, its cache breakpoints and what stays in context. Claude Code stays for sessions on the plan (its
    usage is only reachable through it). The way there: small closed tasks first (the second reviewer, a
    one-shot question, summaries), then whole sessions; the `tokens` measurement says what it would save. Not
    now.
- **The lineup** ([design/async.md](design/async.md) phases 2–4): one scheduler for heavy runs across sessions,
  exclusive runs (perf) on a quiet machine, the queue position shown, resources declared by language packs, and
  language-server work that holds a resource. Measured again after sandbox validation. As of 2026-10-08, build
  folders of each checkout's own cut the waits for cargo's lock to seconds (5.8 s in a quick preflight, three
  sessions at work). What's left is CPU contention and showing the wait. It's scheduled then if that shows.
- **Core audit**: a session's own build folders. (Sandbox rules as pack data are done.)
- **Image viewing** — undecided: an external viewer, a canvas in the core with a WebAssembly decoder plugin, or a
  viewer in the core.
- **A size pass.** The release binary's budget was raised to 12 MB (2026-10-09), then to 13 MB (18 bundled,
  2026-10-15) until stabilization (§9) brings it back; `cargo perf` fails a jump of more than 0.3 MB meanwhile. Four helpers LTO had copied into many callers are kept out of line
  (−66 KB measured). From the size audit (2026-10-09):
  - Per-package `opt-level = "s"` for crates off the hot path, the frame path kept at 3 (tarae, ropey,
    tree-sitter, the grep crates, vt100), decided by a full `cargo perf` A/B. The whole binary at `s` measured
    −2.36 MB and 12–40 % slower: that's the upper bound.
  - `plugin::cli` into a binary of its own; decide `dsyms`.
  - Test-code regions and the Cargo helper → pack data and a plugin, when the testing packs are touched.
  - Java debugging → a plugin, once the plugin API has a debug-way hook.
- **ACP**: hosting other agents inside tarae. Claude is the only agent tarae runs today.
- **Kubernetes plugin, next parts (P1–P6)**, and a separate **fluxcd plugin**. Contexts follow kubectl, with a
  context screen. Notes: the `k8s` session's target/sb/.
- **Database plugin**: steps 0–2 and 8–9 landed; steps 3–7 and 10 wait for plugin API 0.3; DynamoDB on hold.
  Notes: the `db` session's target/sb/db-plan.md.
- **Viewport-bounded injections**: branch `viewport-injections` (10 commits past main, last 2026-10-02; draft PR
  #1 on ethnw/tarae). Rebase and land, or drop: to decide with the user.
- **Grammar distribution**: branch `grammar-wasm-proto` (15 commits; grammars as WebAssembly on tarae's own
  runtime, 1.6–2.0× native) against prebuilt native or bundled grammars. Undecided; to decide with binary
  releases (publishing).
- **Release builds on macOS signed with the hardened runtime** (with binary releases): then no other program of the
  user's can attach a debugger to tarae and run code as it. Claude's commands already can't (step 7: Seatbelt keeps the task port to a run's
  own processes, 2026-10-10).
- **A memory comparison with other editors** (Helix, Neovim with an AI plugin, Zed, VS Code). Method agreed
  2026-10-03: the same scenario; the editor alone and its whole process tree; measured numbers only; a one-off
  script.
- **Raw TCP to an address**: user-written `ip:port` / `cidr:port` rules in `llm.sandbox-hosts` (never a card), for
  a LAN database or a container's or VM's IP. Waiting for the user's call; 6e+'s design note has what it takes.
- **Open from step 6**: `go get` sometimes takes 36–40 s in the sandbox (cause unknown);
  `gradle_builds_in_the_sandbox` fails in the M5 image's tier C (Gradle 8.10.2 starts an ordinary daemon; next
  frictions batch); a crash-recovery test that failed once.

Non-goals: a GUI, vim emulation, copying Helix behavior for its own sake.

Not doing: a cheaper hash for `risk::Key` — no size win, and it widens a collision surface (size audit,
2026-10-09).
