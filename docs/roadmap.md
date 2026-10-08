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
- [Now: the Linux sandbox keeper](#now-the-linux-sandbox-keeper)
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
| **Cards and the inbox** | Questions come as cards, one calm stack. The inbox lists every card and edit waiting, in every session. Phases P0, P1, P2 and P5 of the card model are built ([design/cards.md](design/cards.md)) |
| **Sandbox, macOS** | Claude's commands run under Seatbelt: writes only in the project, never to files that run later outside the sandbox; reads in home confined, keys and tokens never readable; the network closed unless opened ([the sandbox](claude-integration.md#the-sandbox)) |
| **Sandbox, Linux** | The same policy, enforced by Landlock for files and seccomp for sockets, with tarae deciding each address. Each session's runs are forked by a keeper of its own, in namespaces of their own where the system allows it ([on Linux](claude-integration.md#on-linux), [design/sandbox-keeper.md](design/sandbox-keeper.md) steps 1–4) |
| **Repository trust** | A project's risky settings, and a repository's own git config and hooks, wait for a yes before anything of theirs runs |
| **Queues** | A run waits its turn instead of being refused, and says what it waited for (phases 0, H and 1 of [design/async.md](design/async.md)) |
| **UI state** | What's on screen as data: `tarae state`, a socket for outside programs, Claude's `ui` tool, ids that are links, pointing at things (phases 1–3 of [design/ui-state.md](design/ui-state.md)) |
| **Notifications** | Toasts as one calm card, `:messages`, news from sessions behind, the inbox's count on the status line — all inside tarae |
| **Restart with sessions** | `:restart` brings back every session as it was, with its chat, code review and waiting cards; unsaved edits are kept and offered back after a crash ([features.md](features.md#updating-without-losing-your-place)) |
| **Language packs** | A language is a folder of data: grammar, queries, servers, tests, debugger, and what its tools need from the sandbox. The core names no language |
| **Shared rust-analyzer** | One server for a repository's checkouts, swapped in the background as sessions come and go |

The Claude Code IDE protocol (M5) was retired on 2026-10-04, once the chat did all of it inside tarae.

## Now: the Linux sandbox keeper

The design is [design/sandbox-keeper.md](design/sandbox-keeper.md). It gives every session one long-lived keeper
that forks its runs, so what a run leaves running is the session's and ends with it, and sessions are kept apart
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
6. **The transparent proxy** — on Linux where the session has a capable user namespace (tiers A+ and A; stock
   Ubuntu with tarae's AppArmor profile). Sockets tarae plants in the session's network take every connection and
   UDP datagram, each keeping its address (TPROXY, set up by tarae's own netlink): no proxy variables, nothing a
   tool can ignore, IPv6 and UDP included. It replaces step 4's swap and the in-session proxy there. Where it
   can't run (macOS, Linux without a capable user namespace) the per-run proxy of step 5 stays, the supervisor
   enforcing it on Linux; where even the supervisor can't run, the network is closed. Inside it: netlink, the
   session's rules at birth, accepting and checking, DNS, the supervisor slimmed, telling runs apart.
7. **Sessions kept apart** — tests that one session can't reach another's processes, sockets or files. Also: the
   keepers' memory with many sessions, and tarae's log holding every session's activity. Also: a run asks before
   it reads a file named like a key inside the project (`.env`, `*.pem`, `id_*` — the names in
   runtime/secret-names.txt), with the sandbox's policy moved to its own module.
8. **The loopback rule** where sessions share one loopback (tiers without a network namespace): a session reaches
   only its own listeners. It now matters mostly in the per-run proxy's tier.
9. **Showing the tier** — what the sandbox guarantees for a run, said in its report, the sandbox panel, `tarae
   state` and the run's environment. Also when tarae's AppArmor profile would raise the tier.
10. **Docs.**

## Next

In this order, after the keeper's last step.

### 1. Sandbox validation

After step 10. The aim is to state what the sandbox guarantees and show that it holds.

1. **One threat-model document**: what is kept from a run and what isn't, per tier and per OS. It also decides
   whether the `claude` process itself should be confined. Today it has only Read, Grep, Glob, Edit and Write,
   and its commands go through tarae's `run` into the sandbox.
2. **Fuzzing the parsers that read what a run sends**: the supervisor's reading of system-call arguments, and the
   proxy's parsers.
3. **A "must be refused" property suite**, run on several kernels on a schedule. With the transparent proxy:
   nothing a run does changes the session's rules, and nothing is reached but through tarae's sockets.
4. **Yama ptrace scope 2**: today tarae can't serve a run's sockets there and falls back to plain mode.
5. **A speed comparison** against a bubblewrap-style sandbox. An external review some day.

### 2. Frictions, and lighter cards and reviews

Small fixes found while using tarae every day:

- The "Stuck?" card shows for runs on a remote Docker that aren't stuck.
- A new session's bring card offers the language pack's default checks instead of the project's own quick set.
- The same safe command asks every time — for example, a card for the Docker socket that can be answered only for
  one run at a time.
- `tarae state` cuts a card's question short; it should show the whole command.
- Each of Claude's commands says where it ran: in the sandbox (its tier), outside it, or with a socket granted — a
  mark on its row in the chat, with the reason on a click or `K`.

**IMPORTANT — the card and review burden.** Cards and reviews must stay few enough to read, or they protect
nothing. Risk marks precise enough to trust (one large review had well over a hundred high-risk marks); like
cards and marks grouped; fewer wrong cards. This includes the card model's last two phases: **P3**, deciding by
what the session may do rather than by what a command looks like, and **P4**, scopes and a list of grants
([design/cards.md](design/cards.md)). And a review lens: links between what Claude claims and the code, an
evidence panel, "explain this hunk".

### 3. Shuttle keys

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

### 4. A remote split

tarae as a host — sessions, Claude, runs, the sandbox — and its interfaces — the screen and the keys — apart,
joined by one thin protocol: the screen's changes go out (tarae already draws by cell diff), keys come in. An
interface reaches the host from anywhere the host can be reached; how it authenticates and encrypts is decided in
its plan. Two interfaces:

- **The terminal**, as today.
- **A web client**: the thin client built to WebAssembly, in a browser. The same screen, not a GUI.

The egress proxy is designed to live with the runs, wherever they are.

### 5. Working together

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

### 6. Plugin API 0.3

One batch with the forge plugin: regions, cards, tools for Claude, a masked prompt, notes, the repository
([plugins.md](plugins.md)). The database plugin waits for its regions.

### 7. The forge plugin

GitHub, read and write, through `gh`'s login. Pull requests, CI, review comments beside the code; create, comment,
approve, merge — every write on a card.

### 8. Publishing

One draft pull request.

### 9. The homepage

tarae.sinabro.io. An animation that feels like several agents working at once, and the README's demo, both from
one scripted recording: `cargo xtask shot` extended to record frames, Claude's parts played by the tests' fake
claude, so it can be recorded again when the UI changes. To do: the recording script, the scenes, the site, the
domain. The scenes are picked with the user: a chat edit and its card, the end-of-turn review, a commit from the
git tab, search, a short Kubernetes one.

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

## Later

### Queued, not scheduled

- **The lineup** ([design/async.md](design/async.md) phases 2–4): one scheduler for heavy runs across sessions,
  exclusive runs (perf) on a quiet machine, the queue position shown, resources declared by language packs, and
  language-server work that holds a resource. Measured again after sandbox validation. As of 2026-10-08, build
  folders of each checkout's own cut the waits for cargo's lock to seconds (5.8 s in a quick preflight, three
  sessions at work). What's left is CPU contention and showing the wait. It's scheduled then if that shows.
- **Core audit**: a session's own build folders. (Sandbox rules as pack data are done.)
- **Image viewing** — undecided: an external viewer, a canvas in the core with a WebAssembly decoder plugin, or a
  viewer in the core.
- **A size pass.** The release binary's budget was raised to 10 MB for now.
- **ACP**: hosting other agents inside tarae. Claude is the only agent tarae runs today.

Non-goals: a GUI, vim emulation, copying Helix behavior for its own sake.
