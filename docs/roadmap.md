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
| **Review mode** | A pull-request-style review over the real buffers, comments sent in one go. Claude's turn is reviewed at its end and can be undone as a whole, a run's file changes included. A second Claude can review the changes, or a board of them — one per angle, then a skeptic per group of their claims, only what a skeptic traced pinned as confirmed (`space v C`) — and risk grades mark what a person must look at. Each review is recorded: how long a person spends on each change and what it ended as, never its code ([code review](claude-integration.md#code-review)) |
| **Reviews in the background** | `space v c` (the reviewer) and `space v C` (the board) no longer take the session's screen: they read a snapshot of the changes through tarae's own `read` and `search` (no reading copy), findings land on the lines as they are now, and the status line says one is reading. Against Claude Code's tools: the same bugs found, −57 % tokens, −39 % $ (bgreview P1 + P2, `7f6b8834`, 2026-10-20) |
| **Images in `read`** | tarae's `read` sends an image as a picture, decoded and scaled in-process by memory-safe code (the `image` crate, on Claude Code's Read ladder), so the chat runs with none of Claude Code's tools. PDFs later (readimg, `99eb6c2d`, 2026-10-20) |
| **Cards and the inbox** | Questions come as cards, one calm stack. The inbox lists every card and edit waiting, in every session. Phases P0–P5 of the card model are built, and `:granted` lists what Claude may do without asking ([design/cards.md](design/cards.md)) |
| **Sandbox, macOS** | Claude's commands run under a deny-default Seatbelt profile, as Anthropic's sandbox-runtime: writes only in the project, never to files that run later outside the sandbox; reads in home confined, keys and tokens never readable — a file named like a key closed where a grant or a default opened outside the project too; the network closed unless opened ([the sandbox](claude-integration.md#the-sandbox)) |
| **Sandbox, Linux** | The same policy, enforced by Landlock for files and seccomp for sockets, with tarae deciding each address. Each session's runs are forked by a keeper of its own, in namespaces of their own where the system allows it. A run changes no file's owner or flags, nor a `security.` or `trusted.` attribute; its mode, times, extended attributes and ACLs are served where it may write (tiers own and host), and so is an owner call that changes nothing. Sessions are kept apart as far as the system allows, and each run says its tier ([on Linux](claude-integration.md#on-linux), [the keeper](#the-linux-sandbox-keeper) below) |
| **Repository trust** | A project's risky settings, and a repository's own git config and hooks, wait for a yes before anything of theirs runs |
| **Queues** | A run waits its turn instead of being refused, and says what it waited for (phases 0, H and 1 of [design/async.md](design/async.md)) |
| **UI state** | What's on screen as data: `tarae state`, a socket for outside programs, Claude's `ui` tool, ids that are links, pointing at things (phases 1–3 of [design/ui-state.md](design/ui-state.md)) |
| **Notifications** | Toasts as one calm card, `:messages`, news from sessions behind, the inbox's count on the status line — all inside tarae |
| **Restart with sessions** | `:restart` brings back every session as it was, with its chat, code review and waiting cards; unsaved edits are kept and offered back after a crash ([features.md](features.md#updating-without-losing-your-place)) |
| **Language packs** | A language is a folder of data: grammar, queries, servers, tests, debugger, and what its tools need from the sandbox. The core names no language |
| **Shared rust-analyzer** | One server for a repository's checkouts, swapped in the background as sessions come and go |
| **Usage** | What each session's Claude spends, from claude's own stream: the chat's last row, `:usage`, `tarae state`. Each turn's requests in order with their calls' tool names (`$state/calls/`, `cargo xtask turns`). A big conversation waiting on you is kept warm by a one-word ping of a fork before its hour's cache goes cold (`llm.keep-warm`, on by default for sessions expected back, your config only) |

The Claude Code IDE protocol (M5) was retired on 2026-10-04, once the chat did all of it inside tarae.

### The Linux sandbox keeper

Done 2026-10-14. The design is [design/sandbox-keeper.md](design/sandbox-keeper.md). It gives every session one
long-lived keeper that forks its runs, so what a run leaves running is the session's and ends with it, and sessions are kept apart
from each other. Daemons such as Gradle's stay warm between runs.

1. **Landlock's network rights** — done. The kernel refuses a run's own connects and binds; only tarae makes them.
2. **One keeper per session** — done. A fork server and child subreaper on every system.
3. **Namespaces by tier** — done. Where user namespaces work, the keeper is PID 1 of the session's own user,
   network, IPC, PID and mount namespaces. Each tier (own, host, offline) says what it keeps apart.
4. **Routing every socket** — done. The supervisor routes each address-bearing call by one table: to the session's
   own loopback, to this machine, or out. An outbound socket is replaced by one made in tarae's own network, so
   the session's network stays closed. Link-local, metadata, multicast, broadcast and unspecified addresses are
   refused. Measured: a cost of about 0.2 s per Gradle run, seen only in tier own.
5. **The egress proxy** — done. One proxy in tarae, as threads: connections go out by host
   name. It reads the HTTP CONNECT line and the TLS SNI, and never decrypts; plain HTTP goes through it too. A host
   the run's rules don't name waits for the user's card; `llm.sandbox-hosts` lists the ones allowed. On macOS each
   run gets a port and a token of its own. On Linux the relay splices a connection once it has read its start.
   Tools that ignore the proxy variables are told by their packs. Where tarae itself goes out through a proxy,
   it chains to that one, http:// or SOCKS5 (host and SNI checks kept; the address checks are that proxy's, said
   in the report).
   Later: an `https://` upstream, and Negotiate/NTLM.
6. **Out by the proxy alone** — done. Every tier goes out by step 5's proxy variable. A tool that ignores it, or
   raw TCP, fails closed, and the report points at `outside_sandbox`.
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

### Sandbox simplification (2026-10-17)

Fewer moving parts where they bought no security, Anthropic's sandbox-runtime (srt) followed where it has a
recipe, and the tests runnable without a cloud VM.

1. **The claude process runs as you** on both OSes, as every other tool runs it. What protects is the tool gating
   and the runs' sandbox; confining claude broke its login on macOS and needed `~/.claude` whole on Linux.
2. **Three tiers** — own, host, offline — in place of six letters, so a user can tell them apart.
3. **No serving by copies.** It had a likely Landlock escape. Where `pidfd_getfd` is refused (Docker's default
   profile) the session is offline, and says `--cap-add SYS_PTRACE` serves it.
4. **No transparent proxy.** A session's own network already fails closed; it cost time per connection, and its
   tests ran only on a VM. Every tier goes out by the proxy variable.
5. **macOS denies by default**, as srt: every allow besides files and the network is one table, each row with the
   tool that needed it. Other processes' command lines and environments, System V IPC and priority are now kept.

## Next

In this order.

### 1. Publishing

First (decided 2026-10-15); stabilization (§9) waits. The Cyber Verification Program, meant to unblock the
fuzzing work in §2 that the model's safeguards stopped (the control messages), waits for users (item 5).

Items 1–3 are done (2026-10-16); 4 is the user's: the repository made public. 5 is not applied for.

1. **What's in flight finishes** — done: the card model's P3 piece 4 and the diagram's P3 layout.
2. **`SECURITY.md`** at the repository's root — done: where to report (`security@sinabro.io`), what's in scope,
   how reports are handled. The address receives mail: it's forwarded like the project's other addresses, and the
   user's test mail arrived (2026-10-15).
3. **The pull requests** — done: PR #4 and PR #5 merged on GitHub; Q1 (§2) is in, before the repository goes
   public.
4. **The repository made public** — by the user.
5. **Anthropic's Cyber Verification Program** — not applied for (user, 2026-10-19): it doesn't fit yet, because
   tarae has no users; the user applies once it does, as an individual, on the open-source track ("Maintainer of
   a widely depended-on open-source project"; an individual with maintainer authority can apply). What it needs:
   maintainer status, shown by the public repository; a one-to-three-sentence scope ("Security engineering for
   tarae, an open-source code editor I maintain: building and testing the sandbox that confines an AI coding
   agent's commands, on my own machines and test VMs"); the security contact, `security@sinabro.io`. No homepage
   needed. The hope: the sandbox's own testing (seccomp, fd passing, namespaces) goes on without the model's cyber
   safeguards stopping it. Until then §2's stopped items stay deferred as they are (`cyber-verification-items.md`
   keeps them); nothing else in the order changes.

After it, the rest goes on in this order.

### 2. Sandbox validation

The aim is to state what the sandbox guarantees and show that it holds.

1. **One threat-model document** — done: [threat-model.md](threat-model.md), what is kept from a run and what
   isn't, per tier and per OS, each claim with its test. The claude process runs as you; what protects is the tool
   gating and the runs' sandbox.
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
   the run's report, and `sendmsg` gets a buffer tarae built. Still open: fuzzing that forwarding further stays
   deferred until tarae has users and the Cyber Verification Program is applied for (§1; the model's safeguards
   stopped that work twice); a run's own `setsockopt` is unchanged (its own rights).
3. **A "must be refused" property suite** — done on both systems: nothing a run does changes the session's
   rules, and nothing is reached but through tarae's proxy and sockets. macOS (`df2f70e3`); Linux (`08ec1cb3`), rows
   21–26 measured against an unsandboxed baseline. Since sandbox C (2026-10-20, `4b130f35`) it holds each user's
   runs to their own table. Left: running it on several kernels, on a schedule.
4. **Yama ptrace scope 2** — done (2026-10-18, D-h): Yama asks for `CAP_SYS_PTRACE` in a run's user namespace,
   which tarae owns in the session's, so tier own is served at scope 2 (the tier probe tries it on its keeper's
   run); without namespaces the session stays offline, said with the way out. No capability on tarae, no privileged
   helper (user decision).
5. **A speed comparison** against a bubblewrap-style sandbox.
6. **A file's attributes on Linux** (linmeta, found by the speed work's parity checks against srt and Codex):
   Landlock has no right for them. Part A done (2026-10-18, `dbe7086c`, D-h): the seccomp filter refuses a file's
   owner, extended attributes, ACLs and inode flags in every tier, the project's files too; its mode and times stay
   open, said in the report, `tarae state` and the state line; macOS's Seatbelt is pinned to refuse them all outside
   the write places. Part B done (2026-10-19, `df088ee2`): in tiers own and host a run's mode and times calls are
   tarae's — the path read once and opened by the kernel from the run's root, decided on tarae's own descriptor and
   changed on it, let only where the run may write (or on a descriptor it holds open for writing, or one that names
   no file); a path through `/proc`, a descriptor's name in `/dev` or a magic link refused and said, never emulated.
   A change of owner or group stays refused; an owner call that changes nothing (each id `-1` or the file's own:
   `rsync -a`, `cp -a`) is let there as a mode is (sandbox C, 2026-10-20, `4b130f35`). Sandbox D2 B done
   (2026-10-20, `43688c80`): extended attributes and ACLs are served as a mode is in tiers own and host, so
   `install -m`, `cp --preserve=all` and `rsync -AX` work in the project; `security.` and `trusted.` are refused by
   name, as root too. Tier offline stays a full tier with mode and times open, said (user decision 2026-10-18).
   **D2 A** (user decision 2026-10-20): in tier own the key files in the key places outside the project are hidden
   per command, found by a fresh breadth-first walk each run capped at 20,000 entries — what no run writes first,
   by descriptors beneath each place (no link followed, no other filesystem entered, places on NFS/FUSE skipped
   by type), the folders not reached named in the report; a hidden key a run can't remove is named too. Built
   outside, imported into the `sandboxa` session, six reviewer rounds; landed 2026-10-21 (`d6033711`) after the
   outside session's four tier-own runs (the two-deep test as a user, both break-onces caught — three of the
   runs found the test's own fixture and assertions wanting, never the sandbox). Left: the child's skipped keys reported back before `exec` (a protocol
   change — the report says "if still there"); nested user namespaces in tier own are a piece of their own.

### 3. Frictions, and lighter cards and reviews

Done so far, under way, and the order after it (2026-10-17, brought up to 2026-10-20):

- **The compatibility suite** (below): pieces 0–3 done, up to the first macOS table and its findings B1–B7.
  Piece 4 done (`8993751a`): an image per toolchain, a volume per stack, and the first Linux tables, own and host
  (Ubuntu 24.04, kernel 7.0). Piece 6 done: its first fixes (`ca5b79bc`, B1–B4 and B8: Python's bytecode,
  golangci-lint's cache and uv's cache are the project's own run homes, never the user's) and the rest
  (`c646ca83`, B5, B6, B9–B11). B12 done (2026-10-18, `c3e9a605`): pnpm's store on the project's filesystem, where
  pnpm keeps it when its home is on another, is found and told to a run, read and never written — passed over,
  said why, wherever another could change it or a run may write it; and a variable's name is no longer taken for a
  token. Sandbox D (2026-10-20, `c0155ea3`, `36ea9076`): tarae's own `cargo test --bin tarae` passes whole as
  Claude's run — a run's pty cap counts the ptys it holds open at once (it makes 431), `stty` reads a pty's line
  discipline, and on macOS a run's TMPDIR is short enough for its sockets (instances named `<pid>-<k>`, run tags
  with the instance's start).
- **The card model**: P3 done (cardauto pieces 1–4). P4 done: scopes (piece 6, `30a38311`: `s`, this session, on
  every card whose grant may outlive the conversation) and the grant list (piece 7, `951dcdb4`: `:granted`).
  Piece 5 is §9.3's.
- **The sandbox UX pass** (user direction 2026-10-16): tarae builds its own sandbox runtime, so every restriction
  it adds — a login, a card, a held setting — is tarae's to make pleasant, held against what other tools ask
  (`target/sb/sandbox-ux-*.md`: the inventory, the comparison of six tools, the proposal, the user's decisions).
  One conversation a piece, in this order:
  - **A** — done (`f80f38f6`): the chat no longer waits for the project's Claude settings card. claude starts at
    once with them held back, and `y` gives them to it.
  - **B** — done (`b795fd6b`): no sandbox, or a partial one, asks once a session (`y` ask each time · `s` run unasked this
    session · `n` don't run them), never kept across a restart. And the API-key card when claude isn't logged in
    and a key is held back: `y` this session · `A` writes `llm.environment-key = "keep"` to the user's own config.
  - **C** — done (C1 `3248e87f`, C2 `35565bba`; C1b below): the network for Claude's commands. With the network
    closed, the first connection to a new host waits on a card: `y` this run · `a` this chat · `s` this session ·
    `n`. "Always" stays the setting, `llm.sandbox-network`. A registry's yes lets in its pack's other registries.
    Claude asks for a host with `also: {"network": host}`. Not built: tarae's own fetch for Rust and Go
    (`cargo fetch`, `go mod download` with `GOTOOLCHAIN=local`), outside the sandbox and running no project code,
    so the toolchain caches stay closed to Claude's commands.
  - **D** — done (`10c92ebe`): every refusal Claude can ask for has its `also` (`localhost`, `env`, `terminals` beside
    `read`, `write`, `port`; `space m s` lists every port and the variables the output names). Gradle's loopback
    gets its card at the first refusal, holding Claude's call, with `s`/`A` (no pack default). A rerun from
    `space m s` sends its end to Claude by itself, only with the chat idle, in the same conversation, for the
    last turn's failed run; the card says so. Open: Gradle's card before the first run.
  - **F** (to be re-judged, 2026-10-17): words and memory — a hook's yes remembered (a rewritten hook asks again), one vocabulary for the
    answer rows, manifest tools (`cargo add`) said before they run, the docs.
  - **H** — done: Claude's `commit {message, paths}` tool (`committool.rs`). The card shows the message and the diff of
    exactly those paths; `y` commits them through tarae's own git, plumbing and no hooks, what else is staged left
    as it was; `n` does nothing; it never pushes or amends.
  - **E** — done: one list — the trust answers (tarae's, Claude's, git's, a hook's), the plugins' approvals, the
    packs' preflight told and the no-sandbox `s` join `:granted`, each taken back with `d`, what it let run held
    back at once; what's kept for the repository's other checkouts is counted, not used.
  - **J** — done: "always" per repository. What names no place in a checkout (reads and writes outside them all,
    ports, hosts, variables, the terminals) is kept for every checkout; a key file or a place in a checkout stays
    that checkout's own. The read card offers the outside repository's top (never home or above, keys, what runs
    later, nor where a token-like file is), and says what `A` is for: every checkout of the repository, or this
    one.
  - **I** — done: one trust card when tarae's, Claude's and git's wait together as the repository opens (each part
    joins it as it's found, nothing waits); a later change asks only about its own part. `:git-trust` asks git's
    part again.
  - **G**, manifest tools run in a copy (`cargo add` → a change set of `Cargo.toml` and `Cargo.lock`): only if
    `cargo xtask cards` shows manifest refusals often.

  Review at the end stays off by default.
- **After the sandbox UX pass**, from reading agent-security papers (2026-10-16: Progent, CaMeL, a
  prompt-injection SoK, AgentBound, Scaffold Effect, Meta-Harness):
  - **Injection that travels between agents.** Another checkout of the repository is read unasked, and the
    reviewer's notes reach the author's Claude. A row in the threat model (how it's delivered, how it travels),
    and a mark of where text came from on what's read from another checkout or a reviewer's note.
  - **A property loop in `caps.rs`** — done (2026-10-18, `57da092c`): no change made without a card or your own
    config widens what a session may do, and a take-back narrows at once. Progent's split, without a solver:
    seeded sequences of the real changes (cards' answers, config reloads, chats, sessions, quits, `:restart`) over
    a scratch project, each step's reach held to one `check` in every `cargo test` (`src/caps/props.rs`). What it
    found is fixed: a take-back whose store write failed came back, and so did one made before the kept sessions
    were back; a conversation ended left its answers in the session's `Caps`; a trust taken back was held back
    only after its write. From its reviews: one writer per store, landing in the order given, with counters rather
    than clocks between taraes; quitting waits for what's being kept.
  - **C1b (built, 2026-10-17)**: with `llm.sandbox-network` on, `llm.sandbox-hosts` defaults to the packs'
    registries (others asked); `"*"` is any host. It narrows G4.
  - Later: `cargo xtask compat --propose`, a pack-data draft from the refusals a stack met, for a person to
    review, never applied by itself. At Plugin API 0.3 (§7), a plugin's WebAssembly imports checked against the
    permissions it declares.
  - Not taken:
    - micro-VMs — another threat model, and no Firecracker on macOS;
    - warming a run before the call arrives — not until `timing` shows hundreds of milliseconds;
    - applying a policy draft without review.
- Then, in this order (decided 2026-10-15):
  1. **Code mode**: fewer round trips a turn. Each tool call reads the whole conversation again, which is most of a
     turn's cost (measured 2026-10-14). Research: `target/sb/research-auto-code-mode.md`. The plan is in
     (`target/sb/codemode-plan.md`: compound answers on tarae's own tools first, a `code` tool on the WebAssembly
     runtime behind a gate, Anthropic's programmatic tool calling watched), and so is the call-order record it
     turns on (`86590e44`): each turn's requests in order with their calls' tool names, `$state/calls/`, names and
     numbers only. **Next** waits on that record's data: `cargo xtask turns` over the user's real turns, held
     against the plan's gate (requests in runs of 3 or more, turns with a result over 8k tokens).
  2. **The review lens** (below; the plan is the `reviewlens` session's target/sb/reviewlens-plan.md). R done
     (2026-10-18, `34b21cf3`): the review record, `$state/reviews/`, a line per change seen — the time a person
     spent on it (on screen, a key within 20 s), what it ended as, its grade and high rule by name, a keyed id,
     never code text — summed up by `cargo xtask reviews`; `review.record`, on, your config only. It records from
     now. **A + E next** (the why on each hunk; build order and groups), once R has a baseline of one to two
     weeks; then B, C1, F, and D or C2, each measured as the plan orders. The measuring decisions were taken on
     2026-10-18 (the plan's §4: R first, the record's terms); the lens's own choices in its §7 are still open —
     where it shows, claims C1 or C0, whether the reviewer sees the claims, who explains a hunk, the reading order,
     grouped marks, the A/B by day, the keys.
- **Auto mode** is built only if the card record shows it's worth it: one to two weeks of `$state/cards` after P3
  runs in the user's tarae, each class's residual rate held against D1's bar (at least one card of a class per
  session-hour; then the 300-card safety bar). Likely not built.
- **Frictions**: batches 24–35 done. Batch 29 (2026-10-18, `a61cca7d`): a file named like a key outside the
  project asks for itself — Claude's Read on the key's own card, tarae's own tools leave it out — and on macOS a
  run doesn't read one where a grant or a default opened (the shared temp folders too; its own `$TMPDIR` reads);
  `cargo xtask linux` proves the tree it mounts is this checkout, and a filter that matched no test fails. Left from
  it, each its own piece: such names closed to runs outside home, and on Linux outside the project (Landlock only
  grants); its item 4, the high-risk gate after a whole-file look, back on the list with a property test over
  looks. Batch 30 (2026-10-19, `ed16927d`, `target/sb/frictions30-plan.md`, its batch 1): frictions29's small
  ones — a host card outlives its run (`y` is for the next run), `C-q` brings the card's keys with it, review notes
  and replies wait for the turn's end, a landing grades the other bring cards again, a failed preflight names the
  tools' variables it ran with, and what a card, a report or the inbox said too little of; the terminals asked
  again after a `y` take none of the turn's `ALSO_MAX`. Batch 31 (`61e590a4`): a resolved conflict's commit
  message, instruction files that follow main's, which account a chat runs on, `]u` past a viewed file. The
  terminals' A (`5a68b5ca`): a sandboxed run's ptys come from tarae's broker, no card. Batch 32 (`bd5c6ab0`): a
  rebuilt program is never "script unchanged", a read's no holds for the turn, a search's card names its pattern,
  the board asks before it spends. Batch 33 (2026-10-19, `523fdef3`): a stop ends what left its run's group, the
  bring queue, short e2e sockets. Batch 34 (2026-10-20, `e7911bdb`, `target/sb/frictions34-plan.md`): where a filter holds a
  run's output, the stuck card counts a new process at work in its tree as progress, and it says what the filter
  does to the output; a stopped rebase reaches a chat whose Claude hasn't started, or waits in the inbox; a failed
  preflight's bring card comes back naming the test and its panic, `v` its output; the edit tools' reviews are
  titled with what you asked (a rename's and a replace's keep their own); `:restart` keeps the symbols with
  cargo's build-dir apart. Batch 35 (2026-10-20, `5b7aa937`): the reviewer reads the project's rules, all of a big
  change and the session's whole change; a card hides the cursor under it; a run busy at nothing it shows is said;
  System V segments a run made are named with `ipcrm` to free them, never removed (a Mac's 32 can fill up).
  Batch 36 (2026-10-21, `78a4b589`): viewed marks kept across a review's re-open and a quit; a moved bracket never
  asked by itself; `:commit <file>` from a review; a session's CLAUDE.md changes carried into main's on its bring or
  end; a rebase's reloads one toast; tarae's own tests need no terminals grant (the pty broker serves the sandboxed
  runs' ptys); `:restart`'s own build told apart from a run it ended, a send after it waits for claude; the state's
  `refused`, `waits` and a session's answer line (V 10); the usage asked where the stream's rate-limit event carries
  no percent; the reviewer's delta round measured (no saving, left as it was); the outside run card for
  `userns-run.sh` designed only.
  Batch 37 (2026-10-21, `0a83af3a` + `b1b4d0e4`): a bring card after a failed preflight says so up front (`y bring anyway`); `]u` lands
  on a file changed on disk since its mark; the watch reads another program's write once it settles (one reload, one
  toast); a session's end counts only what it removed; the broker test's missing skip; `tab` between the slot's
  panels from `:procs`; the first start's trust card beside the title page; and the stuck test's real cause (the
  card drawn cut under a long `$TMPDIR` — not load).
  The load-flake hunt (2026-10-21, `29c39437`, friction 80): seven tests that fell only while three sessions built —
  five fixed at their cause, two of them real bugs (a disk-watch round an earlier reload overtook was dropped by a
  git action's batch; the first snapshot's index copy lost the checkout's mtime on Linux, so git trusted a
  same-second change), two the tests' own (a shared log file written in parts; a pty pair's node still open); the
  e2e SIGKILL instrumented to name its sender, not found; two never fell in 50–60 runs. Left open (friction 90): a
  plain write read half-done needs a settle wait in the watch; the VM degraded mid-loop.
  frictions29's item 12, the web tool: not built — measured
  first; if needed, tarae's `read` takes an https URL (GET, text, `select`), not a new tool; Claude Code's
  WebSearch stays held back.

- **`:session-apply`** — done (2026-10-19, `a525a775`): an outside tree's change — a patch, `--base <rev>` for
  where it was made — brought in as one reviewed change set, never written unseen.

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
evidence panel, "explain this hunk" — its record (R) is in, the rest as above. The second reviewer is a board now
(2026-10-18, `1b511766`, `space v C`): one Claude per angle (correctness, tests, security, design), a skeptic per
group of their claims, and only what a skeptic traced pinned as confirmed — measured outside tarae first, it found
what the one pass missed.

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
- Off by default. On only through the user's own config (`ui.shuttle-keys`, `trust::YOURS_ONLY`); never a
  project's, trusted or not.
- While one is connected, the status line says so (`shuttle connected — a program is typing into tarae`), the
  person's key pauses it, `:shuttle-end` cuts it off, and each card's record says who answered it: you or the
  shuttle.

The person at the keyboard already has the same guards: a card ignores keys typed as it appears, `y` counts only
after the text was in sight, and no key changes state in silence. This was "guarded keys", phase 4 of
[design/ui-state.md](design/ui-state.md), dropped there and now back under its new name. Phase 1 is done
(2026-10-19, `aebf0bfd`: the protocol and its guards — [design/ui-state.md](design/ui-state.md#shuttle-keys--phase-1-built-2026-10-19-shuttlers)):
`tarae keys` types over the UI socket, each step's `expect` checked in the job that hands its keys on; the grant
card is for the connecting process alone (`--for <pid>` widens it, said); a shuttle answers light cards and a
weighty card's `n`/`v` only, nothing that widens trust. Before it, watchers hear what a batch that drew nothing
changed (`41910769`). Weighty answers with proof the card was read (`read`) come next, then the zellij scripts
move over.

**Then, before the remote split** — plans decided 2026-10-19, each built only once its checkout is rebased on a
main that has what it waits for, and the user says go:

- **PDFs in `read`**, later (user, 2026-10-20): its own piece after images (readimg, [done](#done)), starting from readimg's measurement (tests/live.rs
  `mcp_image_and_pdf_reach_the_model`: a PDF an MCP tool hands Claude Code never reaches the model — it's saved
  to a file, the model told its name), so pages go as pictures or text. Options to weigh then, measured: poppler
  in tarae's sandbox (deny-default, reads listed, nothing of home), or a pure-Rust renderer in-process.
- **A review started by itself** (bgreview P3, `target/sb/bgreview-plan.md`): at a turn's end
  (`review.auto = "turn"`, off by default, your config only) — its own PR once P1–P2 (done) have been used.
- **View mode, then quoting** (the quoteplan checkout's `target/sb/quote-plan.md`): every surface — the chat's
  transcript, the terminal panel, the task, preflight and test panels — gets a view mode, one rule: a cursor on
  its rows as read-only text, Helix's keys, to read, search and copy. Then quoting (인용): select part of any of
  them and ask about it, a chip over the input like pointing's, sent with where it came from, cut to a limit and
  said so; `C-s` follows `llm.screen-terminal`. Phases: P1 view mode for the chat and the terminal, P2 quoting, P3
  the run panels, P4 the mouse. Under way: P1a done (2026-10-20, `2696b066`), the chat's view mode — `esc` from its
  input or `space >` from anywhere, `q`/`esc` leave, `y` copies; held still while Claude writes, `G` catches up; one
  cursor on screen, where the keys go (`keys_to()`). P1b done (2026-10-21): the terminal's — `C-\` goes round
  (code → program → view → code), its scrollback and screen read as text, the program's output held until you
  catch up, the wheel into it; `[keys.terminal]` replaced `editor.terminal-escape`. P2 done (2026-10-21, `8e17fc36`): quoting —
  `enter` on a view's selection, `space >` (`:quote`) on the code's, a chip over the input; `<quoted>` with its own
  closing id, the source said, cut past 16 KB and said, the terminal's token-like lines out; `C-s` follows
  `llm.screen-terminal`. P3 done (2026-10-21, `1b988324`): the task, preflight and test panels' keys are their view
  mode (`Handler::RunView` for `Bottom`, `[view.run]`: `tab` the slot's panels, `K`, `o`, `enter` runs the waiting run or
  quotes the selection as `<quoted from="task|preflight|test">`), held still while a run prints, `G` catches up;
  `space` in any view mode opens view mode's own which-key; the search prompt names the view (the run panels' half
  became panes P2c's documents). P4, the mouse, after panes' P2b.
- **Panes** (the panes checkout's `target/sb/panes-plan.md`): splits as first-class, so tarae alone does what
  zellij does for you today — several terminals, any surface in a pane, a floating terminal, shells kept across
  `:restart`. The user's direction (2026-10-20): every surface — the tree, the editor, the chat, the terminal, the
  panels — a pane on one base; focus decides the one cursor; focus moves as zellij's (`Alt`+arrows, `Alt h/j/k/l`)
  and as Helix's (`C-w h/j/k/l`); sizes by key and by drag. Inside zellij its `Alt` keys are zellij's, so a twin is
  settled in the plan. After view mode and the measurement (both touch the terminal panel). It brings tarae's own terminal engine (a workspace
  crate, in place of `vt100`, measured against a baseline taken first) and a terminal that understands the shell
  while the shell stays yours. **The skeleton first** (the user, 2026-10-21: design / session / tab / pane before anything else;
  the plan's §2.1b–d): **linked panes** (a master–detail pair is one group — one header, one `q`, the list drives the
  detail), **everything is a pane** (cards, pickers, which-key, prompts are floating panes under the one focus; a
  card is a pane that asks; a group lays its members side by side or stacked under one header, `tab`/`S-tab`
  between them, never `]`/`[`), **session → tabs → panes as zellij's** (a tab is one of a session's named layouts;
  the tab bar top-left only with two or more), **a session is Claude's task on its own checkout** (the session list
  is the task list, the main checkout the home session, the chat the spine). Done: P2a (`e002ceb1`: focus as one
  value — `Session.at`, the six flags gone; one search and selection per pane — the user's friction 84 replayed as a
  test; `C-w h/j/k/l` from every surface, said where nothing is that way; the `C-w`-inside-a-terminal decision in
  the plan's §2.6), view P3 (`1b988324`). Order from here: design pass 2's D2/D3 (under way) → **P2b** surfaces as
  leaves (+ `C-w` tarae's inside a terminal, `C-w C-w` to the program; part 1 `e6d8891c`: the tree holding every surface, the layout from it, hops of any kind, zoom, the code splits' sizes, `C-w` inside a terminal's program, restart and the state's panes; part 2 `e131542c`: floating panes under the one focus — `At::Float`, the pane under remembered —, the tests panel as two linked panes under one header, `C-w H J K L`/`t`/`w` over every kind, the layout kept across quit and start in `$state/layouts.json`, the divider lit beside the pane with the keys; four panes' key-to-frame 0.48 ms) → **P2c** text surfaces
  as documents (the user, 2026-10-21: the chat's transcript, the run panels' output and later the terminal as
  buffers under the editor's keys — view mode becomes normal mode on a read-only document); step 1 (`d95b3f13`): the task's, a preflight's and the tests'
  detail are read-only documents of the session's, normal mode on them with `[run]` over, edits put back and said,
  held still while the run prints; step 2 (`54bcfbdf`, 2026-10-22): the chat's transcript a document, a line a
  paragraph, its view mode normal mode on it (`● normal · chat`), its input a small document in insert mode, a
  run's output kept once (footprint −60 % on 20,000 lines); the document pane one module (`docpane.rs`); the
  terminal's waits for the engine → **P3** panes of their own (the tests panel
  the first linked group; its 12 decisions taken 2026-10-22; step 1 (`3d7ca662`): the
  slot's header in the line where it meets the code, one drawer with a frame's top line, the code a row taller; step 2
  (`3b00bb04`): groups as one model; step 3 (`66a80e78`): the task's places and a preflight's steps as linked groups, the
  one output scrolled to the place's line or the step's heading; step 3b (`41aa64ec`): a group's members
  named in the line over their own columns, the one with the keys lit, side by side or stacked when narrow; step 4
  (`4ede51f9`): `:procs` as a group, its detail what the selected process is; step 5 (`5295fbde`): the git tab's
  selection drives the code, a file's changes inline against the index while the tab has the keys, the files passed
  over one italic preview buffer; the polish (`05037fbf`): a stacked group as tall as the member in sight, one run-time
  format; step 6 (`e864ba15`): a group docked beside the code — `C-w H L K`, `C-w J` back to the slot, its size its
  kind's, its members laid by its pane's shape (`C-w t`), folded into the slot when narrow, kept across `:restart`;
  next one format for every elapsed time, then step 6b: every pane's top one line with its name (user, 2026-10-23); step 7 adds the asks' bottom-right corner and the
  follow card as a floating pane — user, 2026-10-22), beside it **C1–C3** of the participants plan (the user, 2026-10-22:
  a session = one participant, a workspace = a checkout with its layout; C1 (`1eb04ee5`) where
  every session reads and writes, shown wherever that file is, each session its name in its colour; C2 (`c4513975`) an overlap between workspaces said the moment it
  would conflict (to a Claude only in the `edit`/`read` of that file) — 1 rebase in 3 stopped on a conflict over 10-16…21, ~6 min each; C2b the woven view (`:weave` — the user's name, 2026-10-22: a view, never a merge done) — everyone works in their own workspace and tarae shows the merge, each change in its
  participant's colour (part 1 `3501b885`: a file, in memory on a worker; 1b under way: the name, deletions shown, `enter` goes to that session's line), the woven tree's preflight on demand (part 2); C3 workspaces kept close to main) → **C0** the split of `Workspace` out of `Session` → **P3b** tabs (the workspace's) and the task list → the terminal line: several terminals → the engine →
  reflow → the shell understood → `tarae` verbs → output for Claude → another session beside → the floating
  terminal → shells across `:restart`. After its own terminal engine, then images at full
  resolution (kitty graphics, iTerm2's inline images) where the terminal speaks them, the half-block view
  elsewhere (user, 2026-10-20).
- **Orchestra, Part I** (the orchestra checkout's `target/sb/orchestra-plan.md`): a session's Claude starts
  sessions of its own and steers them through session tools that act on the editor's model, never on the screen —
  nothing switches the front session or types into an input you can see. P1 children (start them on a card, send
  to them, read their reports, news told once), P2 watching (wakes, cost per child), P3 reviews and proposals.
  P1 builds on bgreview (landed; P3 builds on its start); what stays yours — cards, brings, ends — stays yours.

### 5. A remote split

tarae as a host — sessions, Claude, runs, the sandbox — and its interfaces — the screen and the keys — apart,
joined by one thin protocol: the screen's changes go out (tarae already draws by cell diff), keys come in. An
interface reaches the host from anywhere the host can be reached; how it authenticates and encrypts is decided in
its plan. Two interfaces:

- **The terminal**, as today.
- **A web client**: the thin client built to WebAssembly, in a browser. The same screen, not a GUI.

The egress proxy is designed to live with the runs, wherever they are.

After it, **Orchestra, Part II** (the same plan, §§10–16): the parent session's Claude as the improver — a
fixed procedure; a library of lessons in the project's `.tarae/`, the only thing that grows; card delegation
written down as your rules, off unless a rule names a card (never a bring); checks the parent can't loosen; safety
kept outside the loop. After Part I's P1–P3 and this split.

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
([plugins.md](plugins.md)). The database plugin waits for its regions; the to-do and knowledge-base plugins wait for calls not in this
list yet — a search through tarae's engine for both, a file's comments for the to-do one, a project file written
on a reviewed edit and a link followed by `gd`/`enter` for the knowledge base ([queued](#queued-not-scheduled)).

### 8. The forge plugin

GitHub, read and write, through `gh`'s login. Pull requests, CI, review comments beside the code; create, comment,
approve, merge — every write on a card.

### 9. Stabilization before release

No new features: the release waits on numbers, not a feeling (2026-10-15: every finished piece still held 3–5
bugs its reviewer found, two of tarae's own gates misbehaved in daily use, four tests failed only under load).

1. **The bar**: no test that fails only under load or only sometimes; every test green three runs in a row on
   macOS and on every supported kernel (§2's set); a set period of building tarae with tarae with no new serious
   bug. flakyfix (2026-10-20, `5d14cf65`): ten tests that failed now and then, each fixed at its cause and pinned.
2. **Bug hunting: deterministic simulation testing**, built in-house — no commercial service (user decision
   2026-10-18; the plan: `target/sb/detsim-plan.md`). AI made writing code easier, not checking that it works, and
   the bugs left are about *when*: a kill sent to a pid already reaped and someone else's (timing), two bring cards
   for one session (event order), a grant's take-back lost when its store write failed and one made before the
   sessions list was restored (a fault, start-up order), tests that fail only under load. FoundationDB's way: every
   source of nondeterminism behind one seeded simulator, the whole editor run under random schedules and faults,
   any failure replayed from its seed. P1 done (2026-10-18, `7a3f142b`, test builds only): the simulated loop and
   its first scenario, sessions and their bring cards, which finds the two-bring-cards bug with `b56cdf41`'s fix
   taken out. Two lints hold what's left of the raw clock reads and sleeps, lists that only shrink, and new code
   with a job, a card or a timer gets a step in a scenario (the board's line and the review record have theirs).
   The pieces:
   - **The simulated loop** — built in P1, but shrinking (P2): the jobs (`event.rs`) run one at a time at the
     simulator's choice, which result lands next and when decided by the seed against a simulated clock, the
     clock read through one place (`clock.rs`); unmodelled threads are refused; each seed runs twice and its
     traces are compared; a failure replays from its seed and shrinks to a short schedule.
   - **Faults at the worker boundary**, seeded: a write fails, a read sees half a file, a process exits early or
     never, a server answers late or out of order, the disk watch fires twice or late.
   - **Scenarios and invariants**: random walks over sessions, cards, chat turns with a fake claude, disk changes
     and restarts, checked after every step — one bring card a session, every call asked is answered, the inbox is
     the cards, `Caps` never wider without a card, `tarae state` agrees with the frame, undo restores.
   - **The threads that stay real**: the few lock-and-channel pieces (`llm::kill`'s) under the same scheduler;
     Linux's keeper and supervisor perturbed by seed at their interception points, many runs on the test VMs.
   - **How it runs**: a fixed set of seeds in every `cargo test`, many on a schedule on the VMs; a seed that found
     a bug joins the fixed set.

   Alongside it, the rest of what tests hold still: the screen's width, the load, and the property loops widened
   with real output.
3. **Less code**: what the card model's P3 replaced goes (its piece 5: the parser's gates, ~2,200 lines), and
   whatever else the hunt finds unused. The binary with it: back under the old budgets (12 MB, 17 MB bundled —
   raised to 13 / 18 on 2026-10-15 at 11.98 / 17.07, from 8.7 / 12.5 on 2026-10-04) or a reason why not.
   `cargo bloat` first, for where the size is, then what P3 piece 5 removes and what the hunt finds unused.
4. **Independent reviews of the sandbox** by Claude sessions that didn't build it: each given the threat model's
   claims and the code, not the design's reasoning, trying to show a claim false; their findings fixed like any
   reviewer's. A human security review when one can be had.
5. **A small beta**: a few people use tarae first; what they hit goes into the suites.

### 10. The homepage

tarae.sinabro.io. The brand it wears is in (2026-10-18, `ec987191`, [brand/](brand)): the seal — a skein's
thread carved out of an accent block, unwinding to a cursor, in hanji and meok; vector only (the start screen is
set in type, 2026-10-19) — and the mascot, a ball of thread painted in ink, finished with the seal as its 낙관
(small, at its lower left, 2026-10-19): the one picture at the README's top, over its title. An animation that feels like several agents working at once, and the README's demo, both from
one scripted recording: `cargo xtask shot` extended to record frames, Claude's parts played by the tests' fake
claude, so it can be recorded again when the UI changes. To do: the recording script, the scenes, the site, the
domain. The scenes are picked with the user: a chat edit and its card, the end-of-turn review, a commit from the
git tab, search, a short Kubernetes one.

- Claude's startup program, applied for as the organization (the Claude Console organization, already made) once
  tarae is public and has users (decided 2026-10-09). The Cyber Verification Program moved to §1.

## Alongside, not ordered

- **The simulation harness's first piece** (§9.2) — done (detsim P1, `7a3f142b`): the simulated loop and one
  scenario. New code is tested in it from now on, so the harness grows with what's built instead of being fitted
  to it after.
- **Linux in real use**, on test machines:
  - The terminal, the clipboard, file watching, Java (jdtls), the Kubernetes plugin, WSL, and working over ssh.
  - Several distributions, kernels and languages, and a large repository. Crash recovery, with no work lost.
  - A BPF measurement kit for development, outside the tarae binary. Its start is in [`trace/`](../trace)
    (`tarae-trace`, see [CONTRIBUTING](../CONTRIBUTING.md)).
- **One experience, all through tarae** — under way (the user's direction, 2026-10-20; CLAUDE.md's first UX
  rule): the same act has the same key, the same words and the same look on every surface. The UX audit (the
  `uxaudit` session's `target/sb/uxaudit-plan.md`) laid every surface side by side, held against Helix and zellij
  first. Decided: `q` closes every list or output panel (the review rail's `q` ends the review), `esc code` where a
  surface stays and `esc close` where it goes, line numbers absolute by default. Landed: piece 1, those words and
  keys (`c885086e`, 2026-10-21); piece 2, a card never taking the keys mid-typing (one quiet second after the last
  key, said as `▲1 waiting · C-q` and in the state; test and shot drivers run with it off) and hints where none
  were — pickers, prompts, the rail's rows, the bottom panels' way in (`62491cdf`). Landed too (`8f453a08`): pieces 4 and
  6 — scrolling alike (one `list_move` table for every list and output panel, `C-e`/`C-y`, the selection following
  into sight, `S-PageUp` over the terminal's history) and Helix's picker keys (`C-v`/`C-s` split, `C-t` preview,
  `A-ret`). Landed (`129906f1`): 5, the first
  hour — the tutor drawn as text with its keys in colour and sections for the panels, the terminal's round, cards,
  the review, sessions and the inbox; the palette opening on what you ran and a first hour's commands, word starts
  first; the session list saying its letters are keys (what was the first-use check); 7 (`8480c8ef`), inside
  zellij — where a hinted key is one zellij takes first (its defaults, `g q p n s t h o b`; `editor.zellij-keys`, yours
  only, for a changed config), tarae says its twin in which-key, the palette, the cards, the chat, the tutor,
  `:config-show` and the keymap reference, only inside zellij; shots drop `ZELLIJ`. The audit's eight pieces are in. Driving it logged frictions 59–81 (`target/sb/frictions29.md`: the landing's cost — repeated
  grants, viewed marks lost on a review's re-open, moved-brace re-flags, the reviewer re-reading whole, ten keys
  to commit — and tier-own tests only runnable outside); batch 36 took 66–79 and 81, 80 (the load flakes) is its own hunt.
- **A memory card and panel**: what Claude remembers for the project, in Claude Code's own format.
- **The run-flow panel**: one run's processes, writes, connections and refusals, from sources that need no
  privileges, on macOS too.
- **Image view, half-block** (user, 2026-10-20): a picture drawn with `▀` cells in any truecolor terminal (a
  `.png` opened, the picker's preview, a thumbnail on the chat's tool row, markdown's pictures), on `media.rs`'s
  decode and scale; small, any time between the big pieces.
- **A grant scoped to Docker.**
- **코드멍** (*code-meong*, "code-gazing", as one gazes at a fire): Claude's code typed out character by character
  instead of appearing at once, maybe in the chat too. Now 글타래's P7: the model's edits streamed live. The reveal
  for code that arrives whole (P8) is dropped (user, 2026-10-20): an invented delay, and a key eaten to end it.
- **글타래** (*geul-tarae*, "a skein of writing"): a visual pass. Design details that suit ink (meok) and hanji
  paper, and a way of showing and generating text that suits tarae (the plan: the `geultarae` session's
  target/sb/geultarae-plan.md). P0–P4 done: the shots as a scene list (`cargo xtask scenes`, `900833ab`), the
  audit's fixes (`680f9326`), the chat laying out a message once (`66bebbc5`), 번짐, the head of what Claude
  is writing as ink not yet dry (`660435df`), and weight and margins (2026-10-19, `0ceac0aa`): `● normal` for the
  solid pill, the start screen a title page set in type with no seal, the edit review's head in ink, the chat's
  prose at a 72-cell measure, list cards their own height. P5 done (2026-10-19, `25a2c4c2`): what you decided is
  stamped with the key that decided it (`▐y▌ accepted` in the edit review, `m`/`u` on a turn's files, `:granted`'s
  answers by their key). P6 done (2026-10-20, `b135cf3b`): a turn's stops as knots — `●` the stop in hand, `◦`
  the rest, `▲` a change with its stamp — in the chat's rows and the follow card; no thread line (tried in the
  margin, it read as a border or a quote bar; the user chose the knots alone). P7 done (2026-10-21, `4d39285a`): live edits — what
  Claude writes into a file is drawn there as it's written (`input_json_delta` read as it streams, the place found
  on a worker, provisional rows on the proposed wash, the head of the line in progress wet; nothing applied, no key
  taken; the review opens on the same rows), `editor.motion` full · calm · none with macOS's Reduce Motion read once
  at start; a stream batch with live rows in a big file 0.3 ms, a native Write over 5,000 lines 1.4 ms (was 8).
  P8, 코드멍's reveal, is dropped — an invented delay, and a key eaten to end it.
- **Design pass 2 — 선명함과 구조** (the user, 2026-10-21: tarae looked dull and without structure beside Helix with
  ayu_evolve; the plan `target/sb/design2-plan.md`, the designer's note `target/sb/design2-D1-note.md`). D1 done
  (`9ad62527` + `45a51568`): meok and hanji take ayu's hues fitted to ink and paper (keywords orange, types blue,
  strings green, constants magenta; the accent stays the seal's coral/vermilion; ink a step darker, `#0c0e12`;
  comments upright and readable, doc comments on the ground with the bar only; every text/ground pair at its floor,
  pinned by `builtin_themes_clear_their_contrast_floors`). The 붓 mock (수묵화's 농담: values to the ends, one stroke on the
  surface with the keys) chose the thin line — "pane을 나누는 느낌, zellij처럼" — over the heavy `▌`, and kept the
  range (the ground near-ink, the panels on it). D2 + D3 done (2026-10-21, `89f4ba6f`): `ui.edge` = `lines` (default) ·
  `ground` (gone 2026-10-22: lines only, `ui.edge` with it) — one thin line where docked surfaces meet, lit for the surface with the keys; what takes the keys
  floats in a frame named in its top line (`╭─ space ─╮`), a card's answer keys in its bottom line, two cards never
  touching; `pane_names` — a group's members in one header with `·`, the one in sight lit (`files · outline · git`,
  `terminal · check · procs`); comments a warm wash apart from the virtual text's cool dim (ΔE ≥ 12, tested).
  A polish after (`de0b8890`): a floating frame covers its whole rect, a full row of air under Claude's card,
  scrollbar thumbs `▎` beside a line. D4 the contrast test is in; D5 the docs pictures retaken (28 differ; kept once
  the user has seen them); motion (honest only, the note's table) as its own small piece after panes P2b.
- **Follow leads the cursor to what Claude writes** (the user, 2026-10-21: "커서가 안 따라가고 가만히 있으니까
  어색한데" — follow moves the cursor for a read but only keeps an edit's head in sight; while the keys are in the
  chat the code pane shows no cursor) — landed (`a571c4e5`, 2026-10-22): the cursor goes to the line an edit
  rewrites and stays there when it lands; Claude's caret, an accent bar after the last character written, rides
  the writing every frame (the person's cursor stays a block); `● follow` / `● chat · follow`; a key of yours that
  moves, edits or enters INSERT/SELECT pauses it, a key sequence under way only holds it; the saver's writes are
  jobs the process's end waits for.
- **The docs' pictures from scenes** — phase 1 done (2026-10-19, `f3a08b30`): every picture the docs show is a row
  naming the scene that makes it; `cargo xtask scenes --docs` retakes them into `target/docshots/` and compares,
  and `--keep` copies only what you've seen. Rows with no scene yet are named apart; a `@?` wait that timed out
  isn't said yet (frictions29 39 b), next.

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
- **To-do plugin** (user, 2026-10-20): both kinds in one plugin — one side tab, two lists. *In the code*: the
  project's `TODO`, `FIXME`, `NOTE`… — the tags the comment grammar already colours
  (`runtime/queries/comment/highlights.scm`), found by tarae's own search and kept only where the tree puts them in
  a comment, never a text guess; grouped by file, filtered by tag, `enter` to the line. *Yours*: what you mean to
  do in the project, ticked off in place. Open, for its plan: which tags it lists by default (the grammar's
  include notes, not tasks: `SAFETY`, `INVARIANT`, `DEBUG`); where yours are kept (Markdown checkboxes in a file
  of the project, readable without tarae, or the plugin's own folder); an item tied to a session; Claude adding or
  ticking items (API 0.3's tools for Claude, every write said). API 0.2 gives `find` and `read-file`
  ([plugins.md](plugins.md)); it needs a search through tarae's engine and where a file's comments are.
- **Knowledge-base plugin** (user, 2026-10-20): notes as Markdown files linked with `[[wiki links]]`, read and
  written in tarae as any file, on what tarae is and is building — its search engine for full text and backlinks, the file
  tree and picker for notes by folder and tag, Markdown as tarae draws it (headings, links, code in its language,
  pictures through the half-block image view), a side tab for a note's backlinks and outline, the diagram view
  (style A) for its links as a graph. Claude reads and cites notes through tarae's `read`/`search` and adds or
  links one on a reviewed edit (API 0.3's tools for Claude); a note tied to a session; a quote from the chat or the
  code into a note (view mode's quoting). Open, for its plan: where notes live (in the project, or a folder of
  yours outside it — reads there ask, as any read outside the project); Obsidian-compatible links and front
  matter; one store or separate from the memory card and panel ([alongside](#alongside-not-ordered)) and
  Orchestra's library of lessons in `.tarae/` (Part II, [§5](#5-a-remote-split)). It waits on three pieces not
  built yet: the half-block image view ([alongside](#alongside-not-ordered)), the diagram's view (its plan's P4)
  and view mode's quoting ([§4](#4-shuttle-keys)). Beyond API 0.2/0.3 it needs a search through tarae's engine (as
  the to-do plugin), a project file written on a reviewed edit, and a link followed by `gd`/`enter`.
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
- **TLS termination in the egress proxy** (user, 2026-10-19), a sandbox piece of its own, a plan first. It was
  "not taken" while it meant TLS interception no one else did; Anthropic's sandbox-runtime now has it as an
  experimental feature (srt `626a596`, its README's "TLS termination": `network.tlsTerminate` — HTTPS CONNECTs
  terminated in-process with a per-run CA put in the run's trust bundle (`SSL_CERT_FILE`, `GIT_SSL_CAINFO`, …),
  each decrypted request through `filterRequest`, credential hooks such as sigv4, `excludeDomains` for pinning
  clients and mTLS, `refuseOpaqueTunnels`, `requireHostMatch`).
  - What it would give: rules by method and path, not only host (GET only for a docs host, a grant for one
    repository's path); a run never holding a token — the proxy adds it on the way out, so `tokens.rs`'s
    token-free copies could go.
  - What it costs: each tool's trust variable as pack data; clients that take no CA from the environment, or pin,
    excluded and so held at the host level only; tarae's proxy holding plaintext and tokens; TLS and certificate
    minting in Rust.
  - Not part of it: TPROXY — that was routing, gone since 2026-10-17; every tier goes out by the proxy variable.
- **Raw TCP to an address**: user-written `ip:port` / `cidr:port` rules in `llm.sandbox-hosts` (never a card), for
  a LAN database or a container's or VM's IP. Waiting for the user's call.
- **Open from the keeper's work**: `go get` sometimes takes 36–40 s in the sandbox (cause unknown);
  `gradle_builds_in_the_sandbox` fails in the M5 image's tier host (Gradle 8.10.2 starts an ordinary daemon; next
  frictions batch); a crash-recovery test that failed once.

Non-goals: a GUI, vim emulation, copying Helix behavior for its own sake.

Not doing: a cheaper hash for `risk::Key` — no size win, and it widens a collision surface (size audit,
2026-10-09).
