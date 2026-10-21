# The sandbox keeper: one per session, its namespaces, one egress proxy (Linux)

Design, 2026-10-06. Status: **built** (2026-10-14: steps 1–9 of the [roadmap](../roadmap.md), §Steps). Every Linux
session's sandboxed runs are forked by a keeper of its own, born in the session's namespaces where user namespaces
work (`src/sandbox/keeper.rs`, `tier.rs`, `channel.rs`). tarae decides every call that names an address (`route.rs`,
`supervisor.rs`). What goes out goes through one proxy in tarae, named by the run's proxy variable in every tier
(`proxy.rs`; the supervisor hands a connect to its address over). Where sessions share a loopback, a run reaches no
other session's sockets (`owners.rs`). Each run says what its tier keeps apart and shares (`guarantees.rs`). macOS
keeps Seatbelt, as `src/sandbox.rs` has it; its two-session tests are `apart.rs`.

## What it's for

Before the keeper, the Linux sandbox (`landlock.rs`, `seccomp.rs`, `sandbox/supervisor.rs`) confined each command's
files and decided each address it reached. It left four things open:

- **What a run leaves running is known only by a tag.** A leftover is a process whose environment carries `MARK`.
  `env -i` drops the tag, and then nothing ends that process with its session.
- **Sessions aren't kept apart.** Two Claudes on two checkouts share one loopback, the abstract unix sockets, SysV IPC,
  one process view, the user's ptys and `/dev/shm`.
- **Kept daemons must keep working.** A per-command namespace (srt, Codex, Zed) ends Gradle's daemon with each
  command. That costs +7.5 s per build here (Measured).
- **Egress is by address, not by name**, and nothing reads the name a connection is for.

## Decisions

| | Decided |
|---|---|
| D19 | **The supervisor stays threads in tarae**, its state per session. **The keeper is a subprocess per session in every tier**: with its own namespaces where user namespaces work, else a plain fork server and child subreaper |
| D17/D20 | **One egress proxy, inside tarae, as threads**: no process, no listening socket, no per-session credential. It knows the session by the socket the supervisor hands it |
| — | **No bwrap.** Its per-command model fights daemon reuse, EACCES-not-ENOENT parity with macOS, and working without user namespaces |
| — | **Landlock and seccomp per command**, loaded by the child the keeper forks, from the policy current at that run. Only the namespaces live long, never the filters, so a grant applies at the next run |
| D4 | Landlock's network rights are handled with **no rule**: a kernel lock under the supervisor's routing |
| D2 | A loopback port opened by `llm.sandbox-ports`/`-localhost` goes to the session's own listener first, else this machine |
| D3 | A run's server isn't reachable from the user's browser for now; `:sandbox-expose <port>` comes later |
| D5 | Inside the user namespace the user's own ids map to themselves, not to root |
| D7 | `TARAE_SANDBOX_STATUS`, and the state line's words (Showing it) |
| D10 | The session's mount namespace changes only `/proc`, `/dev/pts` and `/dev/shm`. Files stay Landlock's: denied, never hidden |
| D11 | A dead keeper is remade, the run under way is reported failed, and tarae stops remaking after two deaths in 10 minutes |
| D12 | Where `/proc` can't be mounted there is no PID namespace (tier own without one), never the host's `/proc` |
| D13 | The boundary is the session. A new conversation, a session behind and the main checkout's session each keep their keeper — a new conversation unless the last one granted writes or ports: those are in the policy key (D14) |
| D14 | Any policy-key change ends the keeper; its daemons start cold once |
| D15 | The keeper is born by `clone3` from tarae, with raw system calls until its exec |
| D16 | `/tmp` stays shared |
| D18 | Another session's loopback listener is refused, even with `llm.sandbox-localhost` |
| D21 | Later, its own step: the supervisor and the proxy as one confined process per session |
| D23 | The supervisor and the proxy talk only over a Rust channel (`mpsc`) of typed, owned messages. **Supervisor → proxy**: the traffic end (`OwnedFd`), the session's id, and the connection's rules as a read-only snapshot (an `Arc`, never mutated). **Proxy → supervisor**: `Refused` and log lines. No socketpair between them, and no shared mutable state (no `Mutex` or other cell both sides write). The run's traffic socketpair stays. **Why not a socket**: in one process a socket isolates nothing; the channel keeps the same discipline at no serialization cost. If D21 is ever done, these messages move onto the keeper channel's wire format then (`channel.rs`). The keeper's own channel stays `SOCK_SEQPACKET` + `SCM_RIGHTS`: the keeper is another process |
| D-c | ~~With the network open, the resolvers `resolv.conf` names are reached on port 53 only~~ (2026-10-07) — superseded: through the proxy names are the proxy's (step 5c); 6f took the route's last of it out |
| D-d | Link-local addresses (the cloud metadata servers among them), multicast and broadcast, and their IPv4-mapped and carried forms, are never reached (2026-10-07; D-c's port-53 exception gone in 6f) |
| D-e | ~~Tier host goes out by the same swap~~ (2026-10-07) — superseded: tier host goes out by the proxy's variable and the pair (step 5; kept in 6f, F2) |
| D-f | Abstract unix names are the session's own in its netns (bound and reached there); refused without one (2026-10-07) |
| D-g | **No serving by copies** (2026-10-17): where `pidfd_getfd` is refused (Docker's default profile, no `CAP_SYS_PTRACE`) tarae can't serve a run's calls, and the session is tier offline, said with the way out (`guarantees::PIDFD_REFUSED`: `--cap-add SYS_PTRACE`). 6f′'s copies went: reopening a passed descriptor through `/proc/<tid>/fd/<n>` as tarae, with the flags read from `fdinfo` a moment before, let a thread that swapped the descriptor for an `O_PATH` of a file outside the sandbox receive it read-write, past Landlock; and its `accept`, signals and late close were tarae's emulation, not the kernel's. A session's namespaces always come with their netns (N1 went with it) |

## The shape

```
 tarae                                                     │ per session
 ─────                                                     │ ───────────
 the egress proxy: one, threads ◀── mpsc: traffic end ─┐   │ the KEEPER (a subprocess):
 the supervisor: threads, state per session ───────────┘   │   with user namespaces: PID 1 of user+net+ipc+pid+mnt
   decides each address, makes each allowed call           │   without: a fork server and child subreaper
 ── request channel (socketpair, close-on-exec) ──────────▶│   forks every run; parent or reaper of what runs leave
 ◀── each run's pidfd · status · its listener (SCM_RIGHTS) │     └─ run: Landlock + seccomp of its own, then exec
```

| | Proxy | Supervisor | Keeper |
|---|---|---|---|
| **Where** | threads in tarae | threads in tarae | a process: `tarae --sandbox-keeper` |
| **How many** | one for all sessions | one set per session (its policy, grants, report, log) | one per session, in every tier |
| **Does** | parses CONNECT and the TLS ClientHello's SNI, resolves the name itself, checks every address, connects to the very address it checked, relays | takes each run's seccomp listener, reads the address, decides (`route::route`), and makes the call itself on the run's socket. Never `CONTINUE` | forks each run, reaps it, reports its status; owns what runs leave |
| **Talks through** | a channel (`mpsc`) with the supervisor only (D23): in, an owned message per connection (the traffic end, the session's id, its rules as a read-only snapshot); out, `Refused` and log lines. No mutable state shared with the supervisor | the policy at run start, `Refused` out, the listener by descriptor (nothing else, so moving it into a process later, D21, is mechanical); to the proxy, the channel (D23) | the request channel (`SOCK_SEQPACKET`, `channel.rs`) |
| **Holds** | the session's rules per socket | the run's listener; descriptors it takes with `pidfd_getfd` | stdio from `/dev/null`, its channel's end; no capability |

**The supervisor needs nothing from tarae mid-run.** Its `Policy` is an `Arc` cloned when the run starts
(`sandbox::spawn`, `supervisor::serve`). Grants take effect at the next run (`blocked.rs`), cards come before or
after a run (`tools.rs`, `blocked.rs`), and a refusal goes to the run's `Watch` (`supervisor::refuse`, `sandbox::tell`).

**Why threads, not a supervisor process.** It is the part of tarae that parses what a hostile run writes. In tarae, a
panic there aborts the editor (`panic = "abort"`), and a memory-safety bug there owns tarae. A process of its own
contains that only when it is also confined (Landlock read-only, no `execve`, its own environment floor), so moving it
out is D21's step, decided and measured as that.

## The keeper

### Its birth

On the worker that makes a run's policy (`sandbox::confined` → `keeper::ready`), before the session's first sandboxed
run; a run never makes one on the main loop:

| Tier | Birth (*built*) | What it is |
|---|---|---|
| with user namespaces | `clone3(CLONE_NEWUSER \| NEWNET \| NEWIPC \| NEWPID \| NEWNS \| CLONE_PIDFD)` | PID 1 of the session's namespaces |
| … where `/proc` can't be mounted (Docker's masked `/proc`) | the same without `CLONE_NEWPID` | the namespaces' fork server, a child subreaper |
| without user namespaces | `clone3(CLONE_PIDFD)`: no namespaces | a plain fork server and child subreaper (`PR_SET_CHILD_SUBREAPER`) |

Where a filter answers `clone3` with `ENOSYS` (Docker's default profile does, so that libcs fall back), `clone(2)`
takes the same flags.

**Which namespaces, per policy** (`tier::for_policy`, so part of the policy key, D14):
- **Every policy gets the session's netns** (step 4), whatever it opens: tarae routes what it opens out of the
  netns (§Sockets and routing). (D-a, no netns for a served policy that opens something where the kernel can't put
  a socket in a run's place, went in 6f with its probe: the 5.14 floor — `SECCOMP_ADDFD_FLAG_SETFD`, already in the
  v5.9 uapi header, is older than it.)
- **No private `/dev/shm` where the project, a place it writes or a file it writes is under `/dev/shm`** (it would
  hide them from the run). That keeper says `/dev/shm shared: the project or a place it writes is in it`.

Until its exec the child is a copy of a multithreaded process, so it makes raw system calls only. Each failure goes on
a birth pipe as (step, errno) — `tier::BirthStep` — and its exec closes the pipe:

1. It waits on a go pipe while tarae, the namespace's owner, writes `setgroups` = `deny`, `uid_map` and `gid_map`: the
   user's own ids (D5).
   - This needs tarae dumpable: the `/proc` files of a non-dumpable process's child are root's. The probe says so
     when that's the cause.
2. Mount propagation to slave; a fresh `procfs` on `/proc` (with a PID namespace only); a new `devpts` instance with
   its `ptmx` bound over `/dev/ptmx`; a `tmpfs` on `/dev/shm` unless shared (D10).
3. `lo` up (`ioctl(SIOCSIFFLAGS)`), with a network namespace only; then the address families tarae's own network
   has, mirrored (`keeper::Families`, decided from `getifaddrs` before each birth — what glibc's `AI_ADDRCONFIG`
   counts: IPv4 but loopback, IPv6 but loopback and link-local): `192.0.2.1/32` as `lo:1` (`SESSION_V4`:
   `SIOCSIFADDR`, `SIOCSIFNETMASK`) and `2001:db8::1/128` (`SESSION_V6`: `SIOCSIFADDR` on an IPv6 socket, `lo`'s
   index 1), documentation addresses both. Without them glibc reads "no IPv4 here" (or IPv6) and a lookup asking
   only for that family comes back empty (M5); with them an `AI_ADDRCONFIG` lookup answers inside as it would
   outside. Nothing is reached through them: a bind there is refused, a connect there is routed as any address
   beyond this machine. Only lookups need them, so one that can't be made (IPv6 disabled in new namespaces, say)
   doesn't fail the birth: the child says it on the birth pipe (`LO_NOTES`), tarae logs it, the keeper goes on without
   it and says so with its tier (`Keeper.lo_missing`, `:procs`).
4. Every capability dropped: each bounding-set bit up to `cap_last_cap`, each checked gone; no ambient set; then
   `capset` to zero.
5. stdio to `/dev/null`, its channel to fd 3, and `SIGCHLD` blocked, so every thread of the new process starts with it
   blocked.
6. `execveat` of tarae's own binary (an `O_PATH` descriptor of `/proc/self/exe`, opened once: the binary running, not
   whatever replaced it on disk) as `tarae --sandbox-keeper`, with an **empty environment**.

Steps 2–4 are the namespaced tiers'. **A keeper that can't start fails the run with the reason. It never falls back to
running unsandboxed, nor to another tier while tarae runs** (D-b: the tier is the probe's until tarae starts again).
The confining thread + `posix_spawn` path is retired.

**The session's device places, from the keeper.** The session's `devpts` and `/dev/shm` are mounts over this system's,
and Landlock's walk doesn't reach a rule on a root another mount covers. Measured: a rule on this system's `/dev/shm`
or `/dev/pts` doesn't let a run write in the session's; a rule on the session's own roots, on `/dev` or on `/` does.
So tarae's rules for this system's places don't hold there.
- **The keeper opens `O_PATH` handles** of its own `/dev/pts`, `/dev/pts/ptmx`, `/dev/ptmx` (where that's a node, not
  a link) and `/dev/shm` first thing in its `main`, and hands them to tarae with `Ready`.
- **tarae grants `DEVICES`' rights on them** in that keeper's runs' rules (`linux::session_devices`; the keeper is made
  before the rules).
- **Why these handles are trusted:** they're opened before any run exists. Nothing a run does can change what they
  name, since only the keeper's birth mounts there and a run has no capability.
- tarae takes only the places it knows, each once.
- A shared `/dev/shm` handle is this system's, already granted with the project's protected names left out, so it
  isn't granted again whole.

**A run's pid.** A keeper in a PID namespace says its own number for the run in `Started`. tarae takes its own from
the run's pidfd (its fdinfo's `Pid:`). A **held** run, which is every run of such a keeper, waits before its exec for a
byte on its handoff socket, sent once tarae has read the pid. Without the hold, a quick run could end and be reaped
before tarae looks.

The keeper, first thing in tarae's `main`, before anything of the editor:
- sets `PR_SET_DUMPABLE` 0, `PR_SET_NO_NEW_PRIVS` and `PR_SET_CHILD_SUBREAPER`;
- drops every capability, as step 4 does (without namespaces it holds some only as root, in a container);
- marks its channel close-on-exec;
- makes a session of its own;
- sets every signal to its default (Rust's runtime had ignored `SIGPIPE`, and an ignored signal survives exec);
- keeps `SIGCHLD` blocked before making its `signalfd`, so no handler or default ever runs for it.

It then runs one single-threaded `poll` loop over its channel, its `signalfd` and each run's failure pipe. It installs
no signal handler: as PID 1 it then takes no signal from inside its namespace.

**Why the keeper holds no Landlock or seccomp of its own:**
- **Nothing of tarae's is in it to take**: an empty environment, only its channel and stdio, and no capability.
- **No run can trace it or read its `/proc`**: it's non-dumpable, and Landlock's ptrace rule keeps a domain off
  processes outside it.
- **From ABI 6 no run can signal it** (Landlock's scope).
- **Below 6, a run that kills it only fails closed**: its runs fail, what they left ends, and tarae says so.

### The request channel

A `socketpair(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC)` made at the birth: unnamed, one end each, every forked child
closes the keeper's end first. Its framing and bounded parser are one protocol module (`channel.rs`). The
supervisor–proxy messages would move onto it only if they ever leave tarae's process (D21, D23).
- **One record is one message**: a kind byte, then its fields.
- **Limits**: a record ≤ 1 MiB, ≤ 8 descriptors, a list ≤ 4096 items, exactly the descriptors its kind takes.
- Each limit is a refusal, never a panic.

| tarae → keeper | |
|---|---|
| `Run { id, program, argv, envp, cwd, comm, net, held }` + `SCM_RIGHTS` | `program` resolved by tarae (`spawn::resolve`); `envp` the run's environment as `spawn::environment` makes it; `comm` the name the run gives itself (≤ 15 bytes). The descriptors: the Landlock ruleset tarae built (`linux::confine`), stdin (`/dev/null`, tarae's), stdout, stderr, and, served or held, the run's handoff socket. `net` (one byte, `tier::RunNet`, decided once with the rules in `linux::confine`): `Served` (no netns: tarae makes its sockets), `Routed` (served in the session's netns: its own sockets, its address-naming calls tarae's), `Own` (not served, in the netns: Landlock's TCP and UDP unhandled), `Plain`, `Copied` (served by copies, no netns: tarae makes every socket it has, §By copies) — its seccomp mode follows. `held`: it waits for tarae to know its pid (a keeper in a PID namespace). A malformed request gets `Failed`, never a crash. No `pty`: no sandboxed run asks for one, so it waits for a caller (a run opening `/dev/ptmx` gets the session's own) |
| `Signal { id, sig }` | cancel: the keeper signals the run's process group, only while it hasn't reaped it (`dap::term_group` asks here: once the keeper has reaped a run, a kill from tarae could reach a reused pid) |
| `AskUsage` | its own Rss and Pss (M3): its `smaps_rollup` is unreadable to tarae |
| `End`, or EOF | end every run and leftover, then exit |

| keeper → tarae | |
|---|---|
| `Ready { pid, places }` + `SCM_RIGHTS` | once, at its start, with an `O_PATH` handle of each device place it has (§Its birth) |
| `Started { id, pid }` + `SCM_RIGHTS` | the child's pidfd; `pid` is the keeper's number (tarae reads its own from the pidfd). The main loop waits for it (≤ 1 s, as it waited for `posix_spawn`) |
| `Exited { id, status }` | the keeper is the child's parent and reaps it (`spawn::wait` asks here: a keeper's run isn't tarae's child) |
| `Failed { id, errno, step }` | the child failed before exec (`chdir`, Landlock, seccomp, the handoff, exec), also said in its stderr; it exits 127 |
| `Reaped { n }` | orphans it reaped, as a count (the log) |
| `Usage { rss_kb, pss_kb }` | the answer to `AskUsage` |

**The forked child**, step by step:
1. Closes the channel.
2. Sets `PR_SET_DUMPABLE` 1 again: the supervisor reads the run's memory and its `/proc/<tid>/fd` (and, served on
   taken descriptors, `pidfd_getfd`s them), which needs ptrace access to the run, refused by a non-dumpable process
   unless the caller has `CAP_SYS_PTRACE`. With a user namespace (step 3), tarae's capabilities in it cover it. A
   dumpable run is what every run was before, and what exec makes an ordinary program anyway. Landlock's ptrace rule
   keeps other confined runs off it.
3. `setsid` (and the controlling terminal with `pty`); `dup2`s its stdio; `chdir(cwd)`.
4. `PR_SET_NAME` = `comm`, which Landlock's audit names beside the run's own pid.
5. Unblocks every signal.
6. `landlock_restrict_self(ruleset)`; then seccomp (`seccomp::apply`) — served, by the handoff below.
7. Execs.

Each step's failure is written on a close-on-exec pipe the keeper reads.

**The handoff** (6f′, G1 (b), `keeper::hand_over`; every served tier). Once the served filter is on, a `sendmsg` goes
to its listener, which nobody reads before tarae has it: the child can't send the listener itself after step 6. So:
1. A helper thread of the child (a raw `clone` with `CLONE_THREAD` on a stack `Exec` allocated before the fork — the
   child allocates nothing) puts the filter on itself and gets the listener (`SECCOMP_FILTER_FLAG_NEW_LISTENER`).
2. The main thread, still unfiltered, sends it on the handoff socket with one byte (`SCM_RIGHTS`). tarae (a thread of
   its own, not the main loop) receives it close-on-exec, hands it to `supervisor::serve`, and writes one byte back.
3. The helper syncs the main thread onto its filters: `seccomp(SET_MODE_FILTER, TSYNC)` with an allow-all filter on
   top (`TSYNC` gives every thread the caller's whole chain, `no_new_privs` with it). A thread it couldn't sync, or
   any error, fails the run (`Step::Seccomp`).
4. The main thread waits for tarae's byte and execs; the exec ends the helper.

The two threads meet on atomics and `sched_yield`, raw calls only (the helper shares the main thread's TLS: glibc's
`errno` is the one thing it touches, and the main thread doesn't read it meanwhile). Measured in M8b
(`target/sb/m8b/`) on 6.1, 6.8 and 7.3, under Docker's default profile and without it. `pidfd_getfd` is out of the
handoff, so it works where Docker refuses that call.

The keeper's `Started` message carries the run's pidfd: tarae reads its own number for the run from it (in a PID
namespace) and kills a run it can't number through it. It goes no further than that — what `Keeper::start` returns
(`keeper::Started`) has no pidfd since 6f′ (it was the handoff's), and the supervisor opens a pidfd of its own per
served call (`Caller::open`) for `pidfd_getfd`. Yama 1 holds: tarae → keeper → run, and what runs leave stays below
the keeper.

### What runs leave

- **With a PID namespace**, orphans go to its PID 1, the keeper. Ending the session's leftovers is one kill of the
  keeper: the kernel ends the namespace.
- **Without one**, the keeper is a child subreaper, so an orphan always goes up to it. The session's leftovers are its
  descendants that aren't a current run. On `End` or EOF it ends them itself (TERM, then KILL after `LEFT_TERM`),
  then exits. No `/proc` walk by tag.
- **When the keeper dies** (OOM, a bug, `:procs`; below Landlock ABI 6 without a PID namespace a run can kill it), its
  orphans go to tarae, the subreaper (`reaper.rs`).
  - **Its runs fail closed**: their waits answer "the session's keeper ended", and tarae ends them by their pids.
  - **What they left** is ended by `MARK`, kept only for this.
  - **tarae says so in the log, and the next run's policy makes a new keeper** (D11). A second death within 10
    minutes and none is made: runs fail, saying why.
  - **The runs' seccomp filters stay on**: with its listener gone, an address-bearing call fails and reaches nothing.
- `:procs` shows each session's keeper (`sandbox keeper` — `the session's keeper · <checkout>`), its runs and
  leftovers under it. Its memory is the resident one from its `stat`, which stays readable for a non-dumpable
  process; its Pss it says itself (`Usage`).

### When it ends

- **The session ends** (`:session-end`), or **tarae quits, restarts or crashes**: `End`, or EOF on its channel, which
  is close-on-exec.
- **The session's policy key changes** (D14): a daemon started under the old key would keep its rights, narrower or
  wider. The old keeper takes no run more. What its runs left ends at once, and it ends itself after its last run
  (`keeper::ready`).
- **There is no idle end**; Gradle's and Kotlin's own timeouts end their daemons.

### Its protection

| Property | The session's namespaces | The same, no PID namespace | No namespaces | Test |
|---|---|---|---|---|
| non-dumpable | yes | yes | yes | `the_keeper_is_not_dumpable` |
| no capability in what it forks | dropped before exec | dropped before exec | dropped at its start (it has some only as root) | `a_command_holds_no_capability` |
| unreachable by its runs' signals | PID 1, and Landlock ABI 6 | Landlock ABI 6; below it a run can kill it | as without a PID namespace | `a_command_cant_signal_the_keeper` (from ABI 6, and below it with a PID namespace); else `a_killed_keeper_fails_closed_and_is_remade` |
| unreachable by ptrace and `/proc/<pid>/{mem,environ,fd}` | non-dumpable + Landlock's ptrace scope | same | same | `the_keeper_is_not_dumpable` |
| holds only its channel and stdio | yes | yes | yes | `a_command_holds_only_its_stdio` |
| ends with tarae | yes, its namespaces too | yes, its tree first | yes, its tree first | `the_keeper_ends_with_its_channel` |

## Landlock's network rights

The supervisor makes every allowed address-bearing call itself, as tarae, unconfined. So a run never needs to
`connect`, `bind` or address a `send` by itself, and each network right an ABI has is handled **with no rule**: the
kernel refuses the run's own call under the seccomp routing that already sends it to tarae. Below an ABI the routing is
the only lock on that path; from it on, the kernel is a second one (io_uring's `IORING_OP_CONNECT` was such a path;
the filter refuses io_uring).

| ABI | Right | Use | Below it |
|---|---|---|---|
| **4** | `BIND_TCP`, `CONNECT_TCP` | handled, no rule (`landlock::handled_net`) | the routing alone |
| 6 | scopes: signal, abstract unix socket | as before (`landlock::scopes`) | the supervisor refuses abstract names; signals as the same uid allows |
| **9** | `RESOLVE_UNIX`: connect or send to a unix socket by path | handled, no rule (`landlock::UNGRANTED`) | the supervisor's path check (`unix_target`) |
| **10** | `BIND_UDP`, `CONNECT_SEND_UDP` (setting the remote port, or sending to a port given with the send) | handled, no rule. A send with no address on a socket tarae connected isn't checked (the v7.2 uapi header's words); the auto-bind of a first connect or send happens in tarae's call | the routing alone |

Not served inside the session's namespaces (`Own`, step 3 on) the run makes its own TCP and UDP calls, so those rights
are left unhandled there; the netns, with only `lo`, is the lock. Served in it (`Routed`, step 4) they stay handled:
tarae makes every address-naming call, unconfined. Not served without namespaces (`Plain`), internet sockets are
refused outright, as before.

## Sockets and routing (*built*, step 4)

**One decision per run** (`tier::RunNet`, made in `linux::confine` with its rules, carried to the keeper as the run's
`net`): `Routed` in tier own, `Served` in host, `Own` and `Plain` offline. In `Routed` the run makes its own
internet sockets — TCP's and UDP's only, by type and protocol, as C's `socket()` makes them (`seccomp::Mode::Routed`):
not raw, ICMP's, UDP-Lite's, SCTP's or MPTCP's, whose own options and messages (SCTP's `CONNECTX`, its `DSTADDR`
cmsgs) choose destinations tarae never sees; they belong to the session's netns. `connect`, `bind`, `listen`, an
addressed `sendto`, `sendmsg` and `sendmmsg` still go to the supervisor, which routes them with one pure function
(`route::route`, its table pinned by `route::tests::the_route_table` and
`this_machines_own_addresses_are_this_machine`):

| Destination | Route | How |
|---|---|---|
| loopback (`127/8`, `::1`, mapped, `0.0.0.0`/`::`): a port the session listens on, or one not opened to this machine | **the session** | tarae makes the call on the run's own socket (`pidfd_getfd`); it stays in the session's netns. Not opened, nothing is read; opened, the session's listeners are read from the caller's own `/proc/<tid>/net/{tcp,udp}[6]` |
| loopback: a port opened by `llm.sandbox-ports`/`-localhost` that nothing in the session listens on (D2) | **this machine** | egress, on a socket of tarae's bound to `lo` |
| one of this machine's own addresses beyond loopback (its LAN's, Docker's bridge's `172.17.0.1` — every interface's, `getifaddrs` when the run's policy is made: `Policy.own_addresses`; unreadable: every address beyond loopback refused, network open or not — never opened by a port, never out; logged when the policy is made and said in the run's report) | opened port: **this machine** (`OwnAddress`); else refused: `EPERM`, `network-outbound` | egress, on a socket of tarae's bound to no device (one bound to `lo` reaches loopback's addresses only — measured: `ETIMEDOUT`). A service listening on every address isn't reached through them unless its port is opened |
| a resolver `resolv.conf` names, its port 53, network open, through the proxy (step 5c) | refused: `EPERM`, `network-direct`, said as a lookup | names are the proxy's to look up. Read once into `Policy.resolvers` (`route::resolvers`, glibc's three) |
| link-local (with `169.254.169.254`), the cloud metadata servers that aren't (Alibaba Cloud's `100.100.100.200`; AWS's `fd00:ec2::254` and EKS Pod Identity's `fd00:ec2::23`, Google Cloud's `fd20:ce::254`), multicast, broadcast — and those IPv4 ones carried in NAT64's `64:ff9b::/96` or 6to4's `2002::/16` — network open (D-d) | refused: `EPERM`, `network-never` | before any connect |
| the proxy's reserved address (`192.0.2.1:3128`), network open | **the proxy** (`Route::Proxy`) | the pair (§The egress proxy) |
| anything else beyond, network open | refused: `EPERM`, `network-direct` | only the proxy goes out: raw TCP, a datagram, a tool that ignores the variable fail closed, the report pointing at a run outside (6f took `Out` — a socket of tarae's connected beyond, unreachable since step 5 — away; the TPROXY way went 2026-10-17) |
| anything else, network closed | refused: `EPERM`, `network-outbound` | as before |

In tier host (`Served`, no netns) the same table without the session: loopback is this machine's where opened, else
refused; beyond goes only by the proxy's address.

**The egress swap** (`supervisor::swapped_for`), for Machine and OwnAddress (kept until connect leaves the supervisor:
6f's F1):
1. tarae makes a socket in its own namespace, of the run's domain, type and protocol (TCP or UDP — any other is
   refused here too), non-blocking as the run's is; for this machine's loopback, bound to `lo`.
2. It carries what the run set and can read back (`CARRIED`: `SO_REUSEADDR`, `SO_REUSEPORT`, `SO_KEEPALIVE`,
   `SO_BROADCAST`, `SO_OOBINLINE`, `SO_PRIORITY`, the buffer sizes, `TCP_NODELAY`, the keepalive timers,
   `TCP_USER_TIMEOUT`, `TCP_FASTOPEN_CONNECT`, `IP_TOS`, `IP_TTL`, `IP_RECVERR`, `IP_MTU_DISCOVER`, `IPV6_V6ONLY`,
   `IPV6_RECVERR`; `SO_LINGER`, `SO_RCVTIMEO`, `SO_SNDTIMEO`). Close-on-exec comes from the run's `fdinfo`.
   `TCP_CONGESTION` isn't carried. One that can't be set fails the call.
3. It records it as put out of the session (`Served.swapped`, by inode; the run's old socket's entry out) and only
   then puts it in place of the
   run's at the same descriptor number (`SECCOMP_IOCTL_NOTIF_ADDFD` with `SECCOMP_ADDFD_FLAG_SETFD`, Linux 5.9) —
   another thread of the run's calling on that number the moment it's there finds it tarae's, never the
   session's — then makes the call. A failed `ADDFD` takes the record back. A non-blocking connect answers
   `EINPROGRESS` as the run's own would.
4. What can't be swapped is refused with `EADDRNOTAVAIL` and said (`network-bound`): a socket bound to an address or
   port of its own (its source address can't move namespaces), one with a BPF filter or bound to a device, one tarae
   already put out of the session bound otherwise (to `lo`, or to none). A socket bound to every address and a port the kernel picks
   (`0.0.0.0:0`, as Rust's `UdpSocket` and Java's `DatagramSocket` bind) is swapped as an unbound one: tarae made
   that bind, so it knows (`Served.any_port`). A connected stream: `EISCONN`.
5. A socket put out of the session takes no bind and no `listen` (`EINVAL`, `network-bind`): it's in tarae's
   namespace, where a bind would listen on this machine — and so would a `listen` on one left unbound (a connect that
   failed), which the kernel binds to every address at a port of its picking. A datagram one that sent has a port of
   the kernel's on this machine's addresses, as any UDP client has: it hears what's sent to that port.
6. A socket handed to the run from outside the session (over a unix socket in the project, by a process outside)
   isn't the session's: in `Routed` a call the session's would make on its own socket — the `Session` route, a bind,
   a `listen`, an abstract name — is refused (`EPERM`, said "from outside the session") unless `SIOCGSKNS` says the
   socket's netns is the caller's. tarae has `CAP_NET_ADMIN` over the session's netns (its user namespace is
   tarae's) and not over this machine's, where the ioctl is refused: not the session's, as anything unreadable.
   Tier C never had it (no session netns: every call is judged as this machine's).

(Its probe, `swap_unavailable`, and the refusals where the swap was missing went in 6f: the reason is the 5.14
floor — `SETFD`, already in the v5.9 uapi header, is older than it.)

Unix sockets by path cross namespaces, so the supervisor's path rule (the session's project and `RunDir`) and ABI 9
stay their guard. Abstract names are per netns: in `Routed` tarae binds and connects them on the run's own socket,
so a run reaches only its session's (D-f; this machine's listener on the same name isn't reached); in host they stay
refused.

**Where sessions share a loopback** (*built*, step 8, `owners.rs`; tier host), D18's rule: tarae makes every internet socket a run has, so one table (`owners::OWNERS`) holds each
socket's cookie (`SO_COOKIE`: 64-bit, never reused) against its session and its keeper, from the served `socket`
(again at `bind`, `listen`, a connect and a send). A connect
or an addressed send that `route` sends to this machine (`Machine`, `OwnAddress` — an own address lands on what
listens on every address) asks for the sockets on that port of tarae's netns (`NETLINK_SOCK_DIAG`, filtered by port
in the kernel, a TCP ask listeners only; `owners::diag`), takes those the kernel would deliver it to
(`owners::listener_at`: an exact address before a wildcard one, an IPv6 `::` socket takes IPv4 unless
`IPV6_V6ONLY`, every member of a `SO_REUSEPORT` group), and if one is another session's the call is refused with
`ECONNREFUSED` — what a session's own loopback says where nothing listens (user decision 8) — and the report a
line: "connecting to 127.0.0.1:5173 — port 5173 is another tarae session's". The run's own session's sockets — a
daemon an earlier run left too — and sockets of no session's (the user's listener on an opened port, tarae's) are
reached as before. A call to `0.0.0.0` or `::` (which the kernel sends to loopback) is asked about every socket of its
family on the port.

A bind naming a port — and a `listen` — can't join another session's socket either. The kernel lets one user's
sockets share a port (`SO_REUSEPORT`: a group the kernel spreads calls over; `SO_REUSEADDR`: a TCP listener at an
exact address beside one at every address, UDP sockets even at one address), so a run could take in what was meant for another session's listener. A bind
naming a port (an IPv4 socket's `AF_UNSPEC` too, which the kernel takes as every address at that port) is refused
with `EADDRINUSE` — what the kernel says of a port taken — and said ("binding 0.0.0.0:5173 — port 5173 is another
tarae session's"), when another session has a socket of that protocol on the port at an address that overlaps it
(`owners::overlap`, as the kernel's bind conflict compares two: the same address, either at its family's every
address, IPv6's `::` at IPv4's too unless `IPV6_V6ONLY`, a mapped address as its IPv4 one): of TCP its listeners,
of UDP every one — and, of UDP, one *connected to* that port at an address the bind takes: tarae serves its connect
once, never its plain `send()`s after, which would land on whatever binds there later (nothing was there when it
connected, or its own session's socket that has since closed). The kernel lists a TCP socket only once it listens, so two bound side by side before either
listened pass the bind; the second `listen` is asked the same and refused. A port the kernel picks needs no such
check for TCP — it never gives a port another socket holds (`inet_csk_find_open_port` asks for a conflict-free
one, reuse options or not) — but does for UDP: with either reuse option on, `udp_lib_lport_inuse` counts a port held
by a socket with them too as free, so a bind to port 0 (or a first send or connect, which binds the socket so) may
be given another session's port — measured, `owners::tests::a_port_the_kernel_picks_is_another_sockets_only_for_udp_reusing`
(the port made the only one the kernel may pick, `IP_LOCAL_PORT_RANGE`). tarae makes that bind with both options
off and puts them back after (`supervisor::alone`; a first send or connect is bound so first, to every address at
port 0 as the kernel would — `bound_alone`): the kernel then picks a port no socket has. That holds within a session
too: a UDP port 0 never joins a socket's port, the session's own neither — a program that wants to share a port
names it. A port it picks that another session's UDP socket is connected to is let go again — a disconnect unbinds a
socket bound to port 0 (`udp_disconnect`) — and another asked for, four times at most, then refused and said
(`supervisor::picked`): by chance one bind in tens of thousands, on purpose only by narrowing the range
(`IP_LOCAL_PORT_RANGE`).

A keeper's rows go when it does — ended by tarae, at once; died, or killed for not ending in time, as each socket
goes (`Owners::let_go`, asked from 100 ms, doubling to 30 s, on a thread, for an hour at most — `LET_GO_FOR`: what
still holds a socket then outlived its keeper for good, and its rows stay as they are, a cookie naming one socket
ever): what its runs left still holds them, and
where ending it by its tag fails (it never came to tarae, or outlived the TERM) a listener it holds stays its
session's — another session's to everyone else — until it's gone (pinned for both: died unasked, and asked but
killed past `END_WITHIN`). A session past `owners::KEPT` rows keeps the sockets still in tarae's tables and those
touched since the sweep before (made, or named by a served bind or listen, or by a connect or send while still
unbound — where it binds itself, which makes it a row again; a bound one is in the kernel's list). The sweep reads
that list with the table let go: every session's calls take the table, and a dump of every socket can take a while. Keyed by cookie, not inode:
inode numbers come from one 32-bit counter the machine's pipes and pseudo files share, so a stale row would name a
newer socket; sock_diag gives each socket's cookie in its sockid. Where sock_diag can't be had the lookup reads
tarae's `/proc/net/{tcp,udp}[6]` whole, which find a row by its inode (no cookie) and don't say `IPV6_V6ONLY` — both
err only to refusing; where neither can be read, whose the socket is can't be told and the call or bind is refused
as that (`network-untold`, "refused: whose the port is couldn't be told here" — never said to be another
session's; the report tells Claude to tell the user, the log says why: `supervisor::told`, `Refusal::Untold`). A
UDP socket whose reuse options can't be turned off for the kernel's pick isn't bound, said (`network-bind`) and
logged. Measured (M8.1, `owners::tests::perf_a_loopback_lookup`, 2026-10-14), a
lookup with 100 / 1 000 / 10 000 sockets in the netns, median:

| machine | TCP listeners: sock_diag | `/proc/net/tcp{,6}` | UDP sockets: sock_diag | `/proc/net/udp{,6}` |
|---|---|---|---|---|
| Ubuntu 24.04, kernel 7.0, amd64 (the real C) | 0.075 / 0.118 / 1.01 ms | 2.2 / 4.1 / 29.3 ms | 0.068 / 0.147 / 1.21 ms | 0.39 / 3.1 / 102 ms |
| colima's VM, arm64 (host) | 0.41 / 0.41 / 0.74 ms | 1.6 / 2.2 / 11.3 ms | 0.025 / 0.063 / 0.66 ms | 0.22 / 1.1 / 27.6 ms |

A UDP ask walks every UDP socket of the namespace in the kernel (no table of listeners, as TCP has), the port
filtered there: on Ubuntu a fifth more than TCP's at 10 000 sockets, on colima no more — nothing to change for it. The `/proc` fallback for UDP is dearer than TCP's at 10 000 (102 ms against 30 a lookup,
each served call to an opened loopback port): a machine without sock_diag and that many UDP sockets would want the
tables read once per batch of calls — not built while no supported kernel lacks sock_diag.
(An earlier Ubuntu run, TCP only: 0.045 / 0.127 / 0.98 ms, `/proc` 2.0 / 4.5 / 30.8 ms.) What step 8 adds to one
served call (M8.3, `keeper::tests::perf_a_served_loopback_send`: 2 000 UDP sends to the run's own socket on an
opened loopback port, and to 192.0.2.1 — away from loopback, refused where the network is closed, served all the
same —, against the same session with step 8's work off, `supervisor::APART_OFF`; medians of five): loopback,
Ubuntu 24.04 327 µs against 222, colima 109 against 76; away, 219 against 209 and 77 against 76 (only the unbound
check: within the runs' spread) — a send that's never served (connected) 3–5 and 0.9 µs. The lookup is most of it. Not built, what would take it off a run that sends to one port again and
again (statsd, a test's UDP server): its answer kept per port by the run's supervisor until any session's served
bind, listen or made socket moves a tarae-wide count. `/proc` walks every connection of the namespace, so it's only
the fallback. Accepted and said (user decision 10): the check and the call are two moments — a listener
can go and another come between them. macOS has no such rule (decision 9): Seatbelt opens `localhost:*` whole, so
the report says that what listens on an opened port is reached whoever's it is. Pinned by
`keeper::tests::two_sessions_cant_reach_each_others_sockets` (TCP and UDP, IPv4 and IPv6: refused, said, nothing
taken in), `a_bind_onto_another_sessions_port_is_refused` (TCP and UDP at its address, IPv4's and IPv6's every
address refused, `[::1]` not, a late listen refused, port 0 never given it; the holder's own session's binds taken),
`a_port_another_sessions_udp_socket_sends_to_is_kept_from_a_bind` (named, every address, port 0 and a first send
narrowed to it: refused, said, nothing taken in; the sender's own session takes them and its datagrams),
`a_dead_keepers_leftover_listener_stays_its_sessions`, `a_killed_keepers_leftover_listener_stays_its_sessions`,
`a_session_reaches_its_own_listener`,
`the_users_listener_is_reached_where_opened`, `an_ended_sessions_sockets_leave_the_table`,
`supervisor::tests::a_lookup_falls_back_to_the_tables_then_refuses`, `owners::tests::a_listener_is_found_as_the_kernel_finds_it`,
`addresses_overlap_as_the_kernel_compares_them`, `the_kernel_lists_a_ports_sockets_with_their_v6only`,
`a_sweep_reads_the_kernels_list_with_the_table_let_go`.

### Where `pidfd_getfd` is refused (D-g)

Docker's default profile refuses `pidfd_getfd` without `CAP_SYS_PTRACE` (a dev container started the usual way), so
the supervisor can't take a run's socket to make a call on it. The probe (`supervisor::unavailable`) says so —
`guarantees::PIDFD_REFUSED`, the way out with it — and the session is tier offline: its runs' filter is `Plain`, no
socket at all but netlink's routing one (no network, no port on this machine, not even the project's unix sockets).
The panel says it in two rows (`not served: pidfd_getfd refused · --cap-add SYS_PTRACE serves the calls`); Claude's
report, `tarae state` and the log whole. With `--cap-add SYS_PTRACE` the session is served (tier host: Docker's
profile refuses new user namespaces too).

## The egress proxy

**No listening socket.** For a connection routed out, the session's supervisor makes a `socketpair`: one end replaces
the run's socket (`SETFD`), the other goes to the proxy **with the session and its rules attached**. Nothing can be
stolen or forged. Explicit proxying: `HTTPS_PROXY`/`HTTP_PROXY` in the run's `RunEnv` name a reserved address, and a
connect there is handed over the same way.

**One socketpair for the traffic, a channel for the rest, nothing mutable shared (D23).** The run's traffic goes over
the socketpair above. Everything else between the supervisor and the proxy goes over a Rust channel (`mpsc`) of typed,
owned messages:
- **supervisor → proxy**: one message per connection, carrying the traffic end (`OwnedFd`), the session's id, and the
  connection's egress rules as a read-only snapshot (an `Arc`, never mutated once sent);
- **proxy → supervisor**: `Refused` (the session, the name, why) and log lines.

No `Mutex` or other cell both sides write crosses between them, so the proxy knows only what a message carried.
- **Why not a socket.** In one process, a socket would isolate nothing: a bug in either side still reaches all of
  tarae's memory. The channel keeps the same discipline without serializing anything.
- **If D21 is ever done** and they move into a confined process, these messages move onto the keeper channel's wire
  format (`channel.rs`), fuzzed as it is, without changing what they say.

| Parser | Input | Limits |
|---|---|---|
| the CONNECT line | `CONNECT host:port HTTP/1.1` and headers to the blank line; only `Host` read. Any other method is `405` | head ≤ 8 KB; one CONNECT per connection; 10 s to the blank line; host ≤ 253 bytes, a valid name or literal address; port 1–65535 |
| the TLS ClientHello's SNI | the first record(s) up to the ClientHello's end; `server_name` only | ≤ 16 KB + record headers; 10 s; no SNI, or one that isn't a valid host name, is refused |

Both parsers never panic (a panic aborts tarae), never read past their limits, and answer every input with a name or
a refusal: seeded mutation loops over captured CONNECT heads and real ClientHellos (curl, cargo, Gradle's JVM, Go,
Python) in `cargo test`, and `cargo fuzz` targets by hand.

**How it connects**: it resolves the name with tarae's own resolver; checks every address it got and refuses
loopback, link-local, the cloud metadata addresses (`169.254.169.254`, `fd00:ec2::254`), unspecified and multicast,
their IPv4-mapped forms, and what the session's rules don't allow; **connects to the very address it checked** (no
second lookup); with CONNECT, the SNI must name the CONNECT host; then relays. Every refusal lands in the session's run
report with the name and why. **No TLS interception.**

**Named limits**: domain fronting (SNI and the inner `Host` can differ); Encrypted Client Hello hides the real SNI;
through the proxy's address only TLS (CONNECT) and plain `http://` go out — no raw TCP, and to another machine by its
address nothing goes (no rule names an address).

## Tiers

| Tier | The keeper's namespaces | Supervisor | Beyond this machine, the network open (6f) | Leftovers owned by |
|---|---|---|---|---|
| **own** | user, net, ipc, pid, mnt (`/proc`, `devpts`, `/dev/shm`); where `/proc` can't be mounted fresh (Docker's masked `/proc`), no pid and no fresh `/proc` (D12) — processes shared, said | serving | the proxy's variable and the pair, its own loopback kept (6f's F3) | the keeper's PID namespace: one kill (without one, the keeper's tree) |
| **host** | none (no unprivileged user namespaces) | serving | the proxy's variable and the pair (6f's F2) | the keeper's tree |
| **offline** | as own, or none | plain (Yama 2/3, inside another listener, `pidfd_getfd` refused — D-g) | nothing: no socket at all but netlink | as own, or the keeper's tree |

The tier is what a user can tell apart: whether a command reaches the network at all (served or offline), and whether
its loopback, IPC, `/dev/shm` and terminals are the session's own (own) or this machine's (host). The rest — a PID
namespace or not, how the network goes, the session's namespaces in offline or none — is each row's and `why`'s to
say (§What each tier keeps apart).

In every served tier opened ports and this machine's own addresses go by the swap (§Sockets and routing) until connect
leaves the supervisor (Landlock ABI 9's step, D2). The file guarantees (Landlock ABI ≥ 3 full, 2 partial, < 2 none)
are orthogonal (`linux::availability`).

### What each tier keeps apart

What a run's sandbox keeps apart from other sessions' commands, and what it shares, is one value per run
(`sandbox::Guarantees`, §Showing it). The table below is its golden, `src/snapshots/sandbox_guarantees.txt`, at
Landlock ABI 6 and for Seatbelt; `the_docs_tier_table_is_the_codes` (`sandbox/golden.rs`) fails when the two differ,
and prints the table to paste. The golden also has ABIs 2–5, 7, 9 and 11: below 6 a run can signal its own
session's other commands (signals are kept apart only out of sight), and at ABI 2 files are shared too (no rule on
truncating: a run can empty any file of the user's).

<!-- tiers: from src/snapshots/sandbox_guarantees.txt, ABI 6 and Seatbelt — the_docs_tier_table_is_the_codes -->
| Tier, and what differs | Network | Kept apart from other sessions' commands | Shared with them |
|---|---|---|---|
| own, tarae's proxy | tarae's proxy | files (but the temp folders), processes, signals, System V IPC, /dev/shm, terminals, loopback (its own) | nothing |
| own, the project in /dev/shm | tarae's proxy | files (but the temp folders), processes, signals, System V IPC, terminals, loopback (its own) | /dev/shm |
| own, its lo without an IPv6 address | tarae's proxy | files (but the temp folders), processes, signals, System V IPC, /dev/shm, terminals, loopback (its own, without an IPv6 address) | nothing |
| own, the network closed | closed | files (but the temp folders), processes, signals, System V IPC, /dev/shm, terminals, loopback (its own) | nothing |
| own without a PID namespace, tarae's proxy | tarae's proxy | files (but the temp folders), signals, System V IPC, /dev/shm, terminals, loopback (its own) | processes (command lines, priority) |
| offline, the session's namespaces | no socket | files (but the temp folders), processes, signals, System V IPC, /dev/shm, terminals, loopback (its own) | nothing |
| offline, the session's namespaces without a PID one | no socket | files (but the temp folders), signals, System V IPC, /dev/shm, terminals, loopback (its own) | processes (command lines, priority) |
| host, tarae's proxy | tarae's proxy | files (but the temp folders), signals, terminals (only those it makes), loopback (this machine's, other sessions' sockets refused) | processes (command lines, priority), System V IPC, /dev/shm |
| host, Ubuntu's restriction, no profile | tarae's proxy | files (but the temp folders), signals, terminals (only those it makes), loopback (this machine's, other sessions' sockets refused) | processes (command lines, priority), System V IPC, /dev/shm |
| offline, no namespaces | no socket | files (but the temp folders), signals, loopback (no socket) | processes (command lines, priority), System V IPC, /dev/shm, terminals |
| offline, no namespaces, Ubuntu's restriction, no profile | no socket | files (but the temp folders), signals, loopback (no socket) | processes (command lines, priority), System V IPC, /dev/shm, terminals |
| Seatbelt, no port open, the network closed | closed | files (but the temp folders), processes, signals, System V shared memory, loopback (none of its ports open) | POSIX shared memory |
| Seatbelt, ports open, the network closed | closed | files (but the temp folders), processes, signals, System V shared memory | POSIX shared memory, loopback (the ports it opens, whoever's) |
| Seatbelt, no port open, the network open | tarae's proxy | files (but the temp folders), processes, signals, System V shared memory, loopback (none of its ports open) | POSIX shared memory |
| Seatbelt, ports and the network open | tarae's proxy | files (but the temp folders), processes, signals, System V shared memory | POSIX shared memory, loopback (the ports it opens, whoever's) |
<!-- /tiers -->

- **Processes shared** means one process view: command lines, `status`, `/proc/<pid>/net`, and `setpriority`,
  `sched_setaffinity`, `prlimit64` on others' processes — no Landlock hook covers those. Another process's
  environment is never read (Landlock keeps `/proc/<pid>/environ` to a run's own).
- **Kept apart in every tier**: writes (Landlock), unix sockets by path (the supervisor), the environment
  (`runenv.rs`), and what a run leaves running (the keeper's tree).
- **Shared in every tier**: reads of the repository's other checkouts, and the temp folders (D16).

**The probe**, once on a worker: Landlock's ABI and the seccomp architecture as today, then a keeper made for real and
a `true` run through it under a time limit — a timeout is no namespaces, not success. What failure says:

| Seen | Why |
|---|---|
| the first mount refused (`EPERM`), `apparmor_restrict_unprivileged_userns` = 1 | Ubuntu 23.10+/24.04's restriction: the new namespace has no capability. tarae writes the id maps from outside, as the namespace's owner, so that part works; measured on colima and Ubuntu 26.10 (`refused at making its mounts its own`) |
| the id map write refused (`EACCES`) while tarae is non-dumpable | its ids changed after it started: its children's `/proc` files are root's |
| a mount refused under an AppArmor profile other than `unconfined` | the profile (Docker's `docker-default`) denies mounting |
| `clone3`/`unshare` EPERM or ENOSYS in a container | Docker's default seccomp profile |
| ENOSPC, `user.max_user_namespaces` = 0, `kernel.unprivileged_userns_clone` = 0 | user namespaces turned off |
| `/proc/version` says WSL 1 | WSL 1 |
| mounting `procfs` refused | `/proc` masked (Docker's default): own without a PID namespace |

The remedy on Ubuntu is said once: an AppArmor profile granting `userns` to tarae's binary, else the sysctl (said
second: it weakens the whole system). tarae ships no profile.

## Showing it

*Built* (step 9). One value per run, `sandbox::Guarantees` (`guarantees.rs`, 9a): built where the run's keeper is
known (`Guarantees::landlock`, in `linux::confine`) or from its Seatbelt policy (`Guarantees::seatbelt`), never cached
by session. A session keeps only its last run's, to show it (`Session.sandbox`). Every place below says it in its
words (`line`, `sentence`, `shared_said`, `brief`), pinned by `sandbox_guarantees.txt` (§What each tier keeps apart).

- **Claude's report** (9b): the tier's sentence once a conversation, and again when it changes — what the run is
  kept from and what it shares with other sessions' commands, and what that means ("its localhost is the
  session's own"). Pinned by `sandbox_report.txt` per tier and `golden::the_report_says_the_tier_once`.
- **The run's environment** (9c): `TARAE_SANDBOX_STATUS` = `<tier>:<Landlock ABI>` (`own:6`, `host:4`), or `seatbelt`.
  Set after `runenv`'s floor, so the filter can't drop it; removed where a run has no sandbox
  (`golden::the_status_is_the_tier_and_the_abi`).
- **`tarae state`** (9d): each session's `sandbox` — tier, ABI, the keeper's namespaces, the network's way, what's
  kept and shared, the hint — and `--watch sandbox` when it changes; `ui_state.json` pins it (`uistate::V` 3: the tier's values changed when the tiers folded).
  Claude's `ui` answer carries it; the `ui:` line each request carries doesn't.
- **The sandbox panel** (9e, `:sandbox-blocked`, `space m s`): a header over the list (`Picker.head`) — `tier host
  Landlock ABI 4  loopback …  network …  shared …` as key hints, a fallback's reason in a few words faint below
  (`Guarantees::brief`, from `tier::brief`). Narrow, it folds whole items by `Guarantees::FOLD`; the tier always
  shows. `:procs`' keeper row says the same words.
- **How the runs would get namespaces** (9f, `tier::hint`): where Ubuntu's restriction is why and no profile is on
  tarae, the run's `Guarantees.hint` says "tarae's AppArmor profile would give own namespaces" (host becomes own;
  offline stays offline, with the session's namespaces), faint under the panel's why. The list's last row copies the
  step ("copy the step for own namespaces" — needs sudo, then restart tarae) through the system clipboard (`clipboard::copy`). `tarae
  state`'s `hint` has the whole of it. Not in Claude's report: Claude can't take the step (`sudo`, outside the
  sandbox, the machine's own policy); its `ui` answer has the hint when it's asked why.

## Steps

The history below (Steps, Measured, the Appendix) keeps the tiers' letters of before 2026-10-17: A+ and A are own with
and without a PID namespace, B+ and B offline in the session's namespaces, C host, D offline without namespaces.

The roadmap's numbers ([roadmap.md](../roadmap.md#the-linux-sandbox-keeper)). Each landed on its own, its tests each
seen failing once with the change broken — how is in the appendix, §How each test was seen failing; the story is in
the step's commit message. "Ran" is where the whole suite passed.

1. **Landlock's network rights** (D4). A run's own connect, bind and addressed send are the kernel's to refuse:
   TCP from ABI 4, unix paths from 9, UDP from 10 (Linux 7.2's `include/uapi/linux/landlock.h`). tarae's call on
   the run's socket goes through. ABI 11's own addition isn't handled: the rulesets ask for the rights up to 10.
   - Tests: `landlock::tests::a_runs_own_{connect,unix_connect,udp_send}_is_refused_by_the_kernel`; the golden
     `sandbox_landlock.txt` for ABIs 3, 4, 6, 9 and 10.
   - Ran (2026-10-06): colima `tarae-test` (6.8, ABI 4), the unix and UDP tests skipping themselves; GCP Ubuntu
     26.10 (7.3.0-1002-gcp x86_64, **ABI 11**), none skipped, `cargo xtask linux test sandbox` 111 passing.
2. **One keeper per session** (C, D). `channel.rs` (the protocol), `keeper.rs` (both sides; `--sandbox-keeper` first
   thing in `main`), the supervisor behind a few messages: in, the policy at its run's start and its listener; out,
   `Refused` on a channel (`supervisor::Served`, `sandbox::told`). The keeper is non-dumpable, a child subreaper, and
   ends with its channel; a command holds only its stdio and no capability; a killed keeper fails its runs closed and
   is remade.
   - Tests (`sandbox::keeper::tests`, `sandbox::channel::tests`): `the_channel_parser_never_panics` (200,000
     mutations of every kind's record, each a message or a refusal), `the_keeper_is_not_dumpable`,
     `a_command_holds_only_its_stdio`, `a_command_holds_no_capability`, `leftovers_are_the_keepers_children`,
     `a_killed_keeper_fails_closed_and_is_remade`.
   - As an ordinary user, the dumpability tests run in a process of their own (uid 65534 where the container makes
     it root): root with `CAP_SYS_PTRACE` reads any process, so as root they couldn't fail.
   - Ran: both VMs (121).
3. **Namespaces by tier** (A+ … B). Maps, mounts, `lo`, capabilities dropped, the session's device handles, a run's
   pid from its pidfd, plain mode's own network, the probe and its reasons (`tier.rs`, §Tiers).
   - Tests: `a_run_keeps_the_users_own_ids` (D5), `no_pid_namespace_without_its_own_proc` (D12),
     `a_run_sees_only_its_sessions_processes`, `the_session_mounts_keep_landlocks_rules`,
     `a_project_in_dev_shm_shares_dev_shm`, `a_pty_comes_from_the_sessions_devpts`,
     `a_command_cant_signal_the_keeper`, `tier::tests::the_probe_says_why`. Every step-2 test runs in the probe's
     tier (`keeper::outer_pid` and `seen_by_runs` translate the pids a run prints).
   - Ran (2026-10-06, 166 tests each): Debian 12 (6.1, ABI 2) A+, A and C; colima (6.8, ABI 4) C under Docker's
     seccomp and under Ubuntu's restriction; GCP Ubuntu 26.10 (7.3, ABI 11) C both ways, and A+ with the restriction
     lifted by the user for the run.
4. **Routing every socket** (D2, D-c–D-f). `route.rs` (one table, §Sockets and routing), `seccomp::Mode::Routed`,
   `RunNet::Routed` and the channel's `net`, the swap, the session's `lo` mirroring tarae's address families
   (`Families`), the supervisor's counters (one log line per served run: calls by kind, time in tarae).
   - Tests: `route::tests::the_route_table` (every row, both tiers; the metadata, NAT64 and 6to4 rows),
     `two_runs_share_the_sessions_loopback`, `the_users_loopback_isnt_reached`, `a_swapped_socket_is_taraes_before_the_run_holds_it`,
     `a_socket_from_outside_the_session_is_refused`, `a_run_makes_tcp_and_udp_sockets_only`.
   - In a netns a closed loopback port, a datagram there and an abstract name are the session's own: tests expect
     `ECONNREFUSED` there, not a rule's `EPERM`. A test reaches "beyond" only at a listener of its own on the
     container's address, with a policy that has no own addresses: a target truly beyond would send packets off
     the machine.
   - Ran (2026-10-07): colima C and Debian 12 `--userns` A+ (1637 + 43 e2e each); GCP Ubuntu 26.10 A+ with
     `TARAE_EXPECT_TIER=A+`.
5. **The egress proxy** (D17, D20, D23; §The egress proxy, `proxy.rs`). One proxy, threads in tarae: connections go
   out by host name, read from the CONNECT line or the TLS SNI, never decrypted; plain HTTP too. A host the rules
   don't name waits for the user's card (`llm.sandbox-hosts`); through tarae's own upstream proxy where it has one
   (`llm.sandbox-upstream`).
   - Tests: `the_connect_parser_never_panics`, `the_sni_parser_never_panics`, `it_connects_to_the_address_it_checked`,
     `sni_must_match_the_connect_host`, `the_proxy_knows_only_what_a_message_carries` (D23). The "outside" is
     always the test's own listeners on this machine.
6. **The transparent proxy** (6c–6h) — 6c–6e+ and 6g built, then **removed 2026-10-17** (`notproxy`):
   in the session's netns a program that ignores the proxy variable already fails closed, so taking its connections
   (nftables TPROXY rules, `SO_MARK` per run, tarae answering port 53 with stand-in addresses, a cgroup per run)
   gained no security, cost about 0.5 ms a connection and tests only a cloud VM could run, and reset connections
   where the proxy gives worded 403s; its first reason, Gradle ignoring the proxy, is the java pack's proxy file.
   srt has no transparent proxy either. Every tier goes out by the proxy's variable and the pair (step 5).
   - **The 5.14 floor** (6f): the swap's probe and `Out` went; tier C goes out by the proxy's variable and the pair.
   - **By copies** (6f′): built, then removed (D-g) — where `pidfd_getfd` is refused the session is offline.
   - **tarae's AppArmor profile** (6h, `apparmor.rs`, `packaging/apparmor/tarae`): lifts Ubuntu's restriction for
     tarae's binary alone; the `.deb` ships it for `/usr/bin/tarae`. tarae never writes it.
7. **Sessions kept apart** (D13, D14). Two sessions side by side, from a run of one at what the other's runs have:
   `keeper::tests::two_sessions_cant_reach_each_others_processes`, `…_sockets`, `two_sessions_cant_write_each_others_files`
   in every tier `cargo xtask linux` reaches, and `apart.rs`'s same three on macOS. What they found is fixed:
   - a run's own folder (`RunDir`) is in tarae's cache (`runs/<instance>/<n>`), out of every place another run may
     write;
   - on macOS a run signals and inspects only its own processes, and opens no terminal unless the project allows it;
     on Linux, where a session has no terminals of its own, a run reaches only those it makes;
   - a run can't read a file named like a key in the project unless that file is granted (Linux A+ … B: a mount in
     the run's own namespace; C, D and root: its folders' rules; macOS: the profile), and Claude's reads of one ask;
   - a session's keeper lives across a new conversation and ends with its policy (`a_new_conversation_keeps_the_keeper`,
     `a_key_change_ends_the_keepers_daemons`, `a_new_conversation_after_a_grant_ends_the_keepers_daemons`);
   - one log, each sandbox line naming its session, a busy one unable to push a quiet one's out;
   - many sessions measured (§Measured): the keepers dominate the memory, descriptors were the limit, so the editor
     raises its own and gives what it starts the old one back (`nofile.rs`).
8. **The loopback rule** (D18; §Where sessions share a loopback, `owners.rs`). Tier C: a run reaches no other session's socket on this machine's loopback, nor binds onto its port — refused as
   the kernel refuses (`ECONNREFUSED`, `EADDRINUSE`) and said by its port.
   - Tests: `two_sessions_cant_reach_each_others_sockets` (refused now in C), `a_bind_onto_another_sessions_port_is_refused`,
     `a_session_reaches_its_own_listener`, `the_users_listener_is_reached_where_opened`,
     `an_ended_sessions_sockets_leave_the_table`.
   - Ran: Ubuntu 24.04 and colima.
9. **Showing the tier** (D7, 9a–9f; §Showing it). `Guarantees`, then Claude's report, `TARAE_SANDBOX_STATUS`,
   `tarae state`, the panel's header and `:procs`, the profile's hint. Pinned by `sandbox_guarantees.txt`,
   `sandbox_report.txt`, `ui_state.json` and the panel's snapshots.
10. **Docs**: this document, [claude-integration.md](../claude-integration.md#on-linux), CLAUDE.md, CONTRIBUTING; the
    tier table pinned by `the_docs_tier_table_is_the_codes`.

Later, each its own step: `:sandbox-expose <port>` (D3); the supervisor and the proxy as one confined process per
session (D21). The roadmap's "Sandbox validation" comes next: a threat model, fuzzing, a must-be-refused suite on
several kernels, Yama 2, a speed comparison.

## Measured

All on colima `tarae-test`: kernel 6.8.0-117 (Landlock ABI 4), Ubuntu's VM with
`kernel.apparmor_restrict_unprivileged_userns` set to 0 by the user for the run, a `rust:1.99` container with
`seccomp=unconfined`, `apparmor=unconfined`, `systempaths=unconfined`; OpenJDK 21.0.12.1, Gradle 8.14.3, Kotlin plugin
2.0.21. Everything measured ran as uid 1000 with no capabilities. util-linux's `unshare`/`nsenter` stood in for tarae's
code, and a shell PID 1 reading commands from a FIFO stood in for the keeper. Script: `target/sb/nsholder-measure.sh`
(2026-10-06), whose method is the earlier root-run nsmeasure's (the same two projects, warm-up, `gradle --offline
--console=plain <task> --rerun`, daemons reset between pairs, medians of 3 pairs, costs as 20 samples).

**Gradle (Java `test`) and Kotlin (`compileKotlin`), two runs in a row**, ms, medians of 3 pairs:

| Mode | Java run 1 → run 2 | Kotlin run 1 → run 2 | Daemons after run 2 |
|---|---|---|---|
| no namespace | 2779 → **559** | 4765 → **574** | reused |
| a held user+net namespace, joined | 2974 → **604** | 5288 → **574** | reused, in the held netns |
| a held user+net+ipc+pid+mnt namespace, joined | 2868 → **550** | 4779 → **572** | reused, their parent the namespace's PID 1 |
| **the keeper stand-in: each command forked inside** | 2736 → **575** | 4747 → **563** | reused, their parent the keeper |
| *nsmeasure, root*: a fresh netns per command | 2781 → **10309** | 5641 → **9123** | not reused: unreachable, replaced |

Spread between pairs ±40 ms. A daemon kept by its session costs nothing on run 2; a per-command namespace costs
≈ +7.5 s over a cold start.

**Costs**, ms, median / min / max of 20:

| | |
|---|---|
| `/usr/bin/true`, fork + exec | 0.59 / 0.56 / 0.69 |
| make user+net+ipc+pid+mnt, `/proc` mounted, `lo` up | 1.45 / 1.37 / 2.28 |
| join all five (`nsenter`) and run `true` | 0.94 / 0.90 / 1.09 |
| fork `true` from the keeper stand-in | 0.90 / 0.86 / 1.14 |

A session's namespaces cost ≈ 1.5 ms once; a run inside ≈ 0.3 ms over a plain fork and exec. The stand-in's 0.90 ms
is a shell's (FIFO writes and reads, a subshell, two execs), not tarae's keeper's.

**What a run inside sees**: 6 processes in `/proc` against the container's 33 (the measuring script's own absent);
`/proc/net/tcp` holds its header only; `ip -o link` shows `lo` alone. Joined to user+net only, it saw all 34.

**A dead keeper**: with one Gradle daemon in the namespace, SIGKILL to its PID 1 took the daemon with it at once; a
later join's fork failed with `ENOMEM` (pid_namespaces(7)) though a descriptor of the namespace was still held.

**Where it can't**: with Ubuntu's restriction on (its default), an unprivileged process can't write its id map
(`EPERM`): tier C on our own test VM. Under Docker's masked `/proc` (no `systempaths=unconfined`), user and network
namespaces work but a fresh `/proc` can't be mounted (`EPERM`): tier A.

**What a session keeps warm** (RSS, measured earlier with the kept Gradle homes): the Java project's Gradle daemon
428–429 MB; the Kotlin project's Gradle daemon 676–693 MB plus its Kotlin daemon 446 MB. Per session, as today.

### Step 2, the keeper without namespaces (2026-10-06)

- **How**: `cargo xtask linux test --profile perf sandbox::keeper::tests::perf_`, optimized. The tests
  `perf_a_run_through_the_keeper` and `perf_a_keepers_birth_and_memory` stay as the recurring numbers.
- **The confining thread's numbers** were taken in the same build, before that path went.
- **The test process** (tarae's stand-in) held 38–48 MB resident.
- **The keepers** are the test binary run again, which is bigger than tarae's own: their Rss is an upper bound.

| | colima `tarae-test`: Linux 6.8, aarch64, ABI 4 | GCP VM: Linux 7.3.0, x86_64, ABI 11 |
|---|---|---|
| **M1**: `true`, asked to its exit status, median of 50 — through the keeper, plain | 0.43 ms | 2.95 ms |
| … served (the handoff and a supervisor) | 0.48 ms | 3.20 ms |
| … through the confining thread + `posix_spawn` it replaced, plain / served | 0.33 / 0.34 ms | 1.93 / 1.95 ms |
| **A keeper's birth** to `Ready`, median of 20 (once per session, on a worker) | 1.32 ms | 6.82 ms |
| **M3**: one idle keeper's own Rss / Pss | 6.9 MB / 4.2 MB | 9.6 MB / 4.8 MB |
| … 4 at once, Pss in all | 12.3 MB | 15.8 MB |
| … 8 at once, Pss in all | 21.6 MB | 25.4 MB |

- **A run costs the keeper 0.1 ms on colima and about 1 ms on the GCP VM**, which is slower at everything here.
  That's the keeper's `fork` of itself, where `posix_spawn` didn't copy, plus the request and answer.
- **An idle keeper shares most of its pages**: Pss per keeper falls from 4.2 to 2.7 MB with 8 of them.

### Step 3, the keeper in namespaces (2026-10-06)

- **How**: `cargo xtask linux --userns test --profile perf sandbox::keeper::tests::perf_`, optimized, A+ beside C in
  one process (`measured_in`).
- **M4 and M8**: `target/sb/m4-keeper.sh`, the setup from the earlier `nsholder-measure.sh`, through tarae's real run
  path (`sandbox::policy` → `spawn`), A+ then C (`TARAE_TEST_NO_NS`).
- Details in `target/sb/step3-notes.md`.

| | Debian 12, 6.1, ABI 2, x86_64: C · A+ | Ubuntu 26.10, 7.3, ABI 11, x86_64: C · A+ |
|---|---|---|
| **M1**, plain / served | 2.11 / 2.17 · 2.09 / 2.27 ms | 2.22 / 2.45 · 2.47 / 2.52 ms |
| **A keeper's birth** to `Ready` | 6.81 · 9.94 ms | 6.33 · 8.80 ms |
| **M3**, idle keepers' Pss, 1 / 4 / 8 | 4.7 / 13.4 / 23.2 · 4.8 / 13.4 / 23.1 MB | 7.7 / 15.9 / 27.0 · 6.2 / 16.0 / 26.5 MB |
| … the kernel's, 8 keepers (`Slab`) | ≈ 0 · +216 KB | 0 · −4 KB (noise) |

- **A run in the session's namespaces costs at most about 0.25 ms more; a birth about +3 ms, once.**
- **The namespaces don't change a keeper's own memory.** The kernel's share is below what `/proc/meminfo` tells apart.

**M4, Debian**, run 1 → run 2 in ms, three pairs (A+ here without a netns: the policy opens localhost, D-a):

| | A+ | C |
|---|---|---|
| Java `test` | 10105 → 2078 · 8861 → 2040 · 9476 → 2065 | 9697 → 1823 · 8861 → 1834 · 8402 → 1787 |
| Kotlin `compileKotlin` | 51438 → 2193 · 18182 → 2049 · 15155 → 2048 | 52882 → 2174 · 14958 → 1879 · 14887 → 2002 |

- **The daemons are kept by the session's keeper in both tiers**: reused on run 2, their parent the keeper (Kotlin's,
  the Gradle daemon), ended with it.
- **A+ adds about 0.25 s to Java's run 2** (both runs of the script); step 4's cut found where (below).
- ~~Run 2 through tarae's real path is 1.8–2.0 s against the stand-ins' 0.55 s.~~ **Corrected in step 4: that
  compared two machines.** The 0.55 s were colima's (aarch64); on this Debian VM the stand-ins themselves take
  1.81–1.82 s (Java) and 1.93–1.99 s (Kotlin), and tarae's real path in C matches them (1.79 s). Not a cost of
  tarae's path.

**M8, Debian**: `git status`/`log`, `cargo build --offline`, `npm ci --offline`, `mvn -o package` and `gradle --offline
test` all work in A+ as in C. **No tool refused root's files showing as `nobody`.**
- `ls /` is refused in both tiers (`/` is granted entry by entry around home).
- `python3 -m venv` fails in both: the image has no `ensurepip`.
- One `cargo build` in A+ failed once, its words lost, and built the next time: unexplained.

### Step 4, routing and the egress swap (2026-10-07)

- **How**: `target/sb/m4-before.sh`, `m4-after.sh` (M4, Debian), `m5-lookups.sh` (M5, Debian and Ubuntu 26.10 with
  `--network host`), `m5b-addrconfig.sh`, `m5c-cargo.sh`. Optimized test binaries; uid 1000, no capabilities.
  Details: `target/sb/step4-notes.md`.

**M4: Java's run 2 on Debian, medians of three pairs, ms** (the supervisor's calls in run 2 from its counters):

| | Before step 4 | After |
|---|---|---|
| the stand-ins on this VM: plain `sh -c` · a FIFO keeper in namespaces | 1823 · 1813 | — |
| C, the real path | 1793 (23 sockets, 6 connects, ≈ 4 ms in tarae) | 1869 (the same) |
| A+, `sandbox-localhost` on | 2032 (no netns: D-a) | 2100 (routed: 6 connects look up the session's listeners, 9 ms in tarae) |
| A+, the policy closed | — | 2063 (routed, no look-up, ≈ 3 ms in tarae) |
| A+, Landlock granting all of `/` | 1811 | — |
| A+ without the session's device grants | — | 2064 |
| **A**: no fresh `/proc` (no PID namespace) | — | **1889** (without device grants: 1885) |
| A+, unserved (`Own`) · `gradle` without `sh -c` | 2096 · 2078 | — |

- **The routing costs nothing measurable** (2.10 against 2.03 s, inside the pairs' spread). With the policy closed,
  Gradle needs no `sandbox-localhost`: its daemon is the session's.
- **The supervisor isn't a cost**: about 31 calls and 4 ms per run 2; unserved is no faster.
- **A+'s +0.2 s is the fresh `/proc` under Landlock's rules**: it goes in A (no fresh `/proc`) and with Landlock
  granting all of `/`; the device grants don't matter.

**M5: name lookups with the network open** (`getent ahosts`/`ahostsv4`, Python with and without `AI_ADDRCONFIG`,
curl, Node, Java with an HTTPS GET, `cargo fetch`; A+ and C beside unsandboxed, on Debian through GCP's resolver
`169.254.169.254` — D-c's "out" — and on 26.10 through `127.0.0.53` — D-c's "this machine"):
- Every lookup and connection worked in both tiers: 9–15 connects per client, 3–5 ms in tarae, times as C's.
- **Except an IPv4-only lookup with `AI_ADDRCONFIG`** (`getent ahostsv4`), empty inside the netns: with only `lo`,
  glibc sees no IPv4. Fixed by the session's `lo` mirroring tarae's own families (§Its birth: `SESSION_V4`,
  `SESSION_V6`), the IPv4 half measured first in a stand-in netns; an `AI_ADDRCONFIG` lookup answers inside as it
  would outside.
- `cargo fetch` failed in both tiers with a `~/.cargo` holding no `registry/` yet: the open network's grant on
  cargo's caches covers only the folders there when the policy is made (Landlock can't grant a path that isn't
  there). It predates step 4 (open items).

### Many sessions, idle (step 7, 2026-10-11)

- **How**: `cargo xtask linux [--env TARAE_EXPECT_TIER=A+ --userns] test --profile perf --bin tarae -- --exact
  sandbox::keeper::tests::perf_keepers_memory_per_session --nocapture`, optimized, run as root in the container.
  `test --as-user` builds at opt-level 0, where every `perf_` test skips itself.
- **What a session is here**: a keeper of its own, left idle. Then each keeper also keeps one idle served run: the
  test binary, holding a plain HTTP connection through the proxy to a listener of the test's that answers and
  keeps it open. That stands in for a daemon that went out once: its supervisor, the proxy's thread and its relay.
- **What's read**: the keepers' Pss is each keeper's own `smaps_rollup`, as it says it (`usage`). tarae's Pss and Rss
  are the test process's, grown per session, first for the keepers, then for the runs. Threads are counted by
  name, descriptors by kind. The runs' own processes count for neither. The first run's row also holds the
  proxy's one-time start (+4–5 MB Pss), so the per-session numbers are from 8 sessions up.
- colima twice (the same to ±0.1 MB); Debian 12 once, C beside A+ (netns) in one process.

| | colima `tarae-test`: 6.8, aarch64, ABI 4 — C | Debian 12: 6.1, x86_64, ABI 2 — C · A+ |
|---|---|---|
| **Keepers' Pss in all**, idle, 1 / 8 / 16 / 32 | 4.8 / 24.1 / 44.0 / 83.1 MB | 5.0 / 25.3 / 45.5 / 85.1 · the same |
| … each keeping a run | 4.8 / 23.7 / 43.9 / *stopped at 19 runs* | 5.0 / 24.8 / 45.2 / 85.7 · 4.9 / 24.7 / 45.3 / 85.8 MB |
| … one keeper's, at 32 | 2.6–2.7 MB | 2.6–2.7 · 2.6–2.7 MB |
| **tarae's Pss / Rss per session** for the keepers, 8 / 16 / 32 | +0.5 / +1.0 · +0.2 / +0.5 · +0.5 / +0.6 MB | +0.7 / +1.3 · +0.6 / +0.9 · +1.1 / +1.3 MB · −0.3 / +0.2 · +0.3 / +0.7 · +0.3 / +0.5 MB |
| … for the runs, 8 / 16 / 32 | −48 / +80 · +4 / +65 KB · — | −115 / +71 · +84 / +174 · −2 / +42 KB · −414 / −233 · −67 / +23 · −10 / +34 KB |
| **tarae's threads per session** | keeper 2 (`tarae-keeper`, `tarae-kwait`) · run 3 (`tarae-serve`, `tarae-ptys`, `tarae-proxy-conn`) | the same · run 2 (no `tarae-ptys`) |
| **tarae's descriptors per session** | keeper 5 · run 44 (32 ptys, 5 pipes, 4 sockets, ruleset, pidfd, seccomp notify) | the same · run 12 (no ptys) |
| … open at 32 sessions with their runs | — (1002 at 19: the limit is 1024) | 1574 · 550 |

- **The keepers dominate the memory**: 2.6–2.7 MB of Pss each at 32, 83–86 MB in all. tarae's side is at most
  about 1.3 MB of Rss per session, for a keeper's two threads. A held run's threads add at most 0.2 MB. Those
  numbers are noisy (the allocator's arenas): they move by hundreds of KB between counts, both ways.
- **Nothing grows worse than linearly.** A keeper's own Pss falls as there are more of them (4.8 → 2.6 MB: their
  pages are shared). tarae's per session doesn't rise with the count. Keeping a run changes no keeper's Pss.
  The threads grow by a fixed 2 per keeper and 3 (C) or 2 (A+) per held run.
- **Descriptors are the limit, not memory.** In C a session with a held served run costs tarae 49 descriptors.
  32 of them are the run's pty pool (3b: `ptys::POOL` = 16, a master and an `O_PATH` other end each). In A+ it's
  17, with no pool. colima's soft `RLIMIT_NOFILE` is 1024, systemd's default too. There the 20th session's run
  couldn't be made: `EMFILE` making its rules, both times. Debian's container allowed 1048576 and reached 32.
  tarae never raised its soft limit, and its servers, files and terminal took from the same 1024.

### Many sessions, the descriptor limit raised (step 7, 2026-10-13)

- **What changed**: the editor raises its soft `RLIMIT_NOFILE` to the hard limit in `main`, past the arguments,
  before any thread runs (macOS: to `kern.maxfilesperproc` at most; a subcommand like `tarae state` raises and
  pins nothing), and what it starts gets the old one back (`nofile.rs`). A pool pty's `O_PATH`
  other end is closed once the run's rules are made: a Landlock rule keeps its own reference to the inode, and
  tarae's master keeps the pty that inode. Broken once, closed before the rules, every pool pty is refused
  (`EACCES`). `ptys::POOL` stays 16: nothing measured says fewer is enough, and tarae's own e2e suite holds a pty
  per test thread.
- **How**: as above, colima, C. The measurement now runs in a process of its own, raised as `main` raises it
  (`TARAE_TEST_NO_RAISE=1` runs it without).
- **Per session in C**: keeper 5 descriptors (the same), a held served run **28** (was 44): 16 ptys (was 32),
  5 pipes, 4 sockets, ruleset, pidfd, seccomp notify.
- **Reached 32 sessions** with a run each: 1062 descriptors open, under 524288 (1024 → 524288 raised). Without the
  raise the pool change alone gets 30 runs, then `EMFILE` making session 31's rules (it was 19).
- **Memory unchanged**: keepers' Pss 4.2 / 23.7 / 43.7 / 82.9 MB at 1 / 8 / 16 / 32 sessions, 2.6 MB each at 32.
- **What tarae starts** gets the old limit as it's made. A fork's child (`pre_exec`: the terminal's shell, debug
  adapters, plugins' programs) sets it itself. The keeper sets it at its birth, so its runs (every sandboxed run on
  Linux) inherit it, with no shim and no rule for one. A posix_spawn child that runs the project's code or the
  user's commands (tasks, tests, Claude's runs outside the keeper, `:sh`, shell pipes, language servers, a debug
  build) goes through tarae's own binary (`tarae --nofile N <path> <argv0> args…`: the program sees the
  `argv[0]` it was given). On Linux that's `/proc/self/exe`. On macOS it's a hard link to the binary made at
  start (`nofile::pin`), so a build replacing the file or a `cargo clean` never changes which shim runs. It lives
  in `shims/<pid>` in tarae's state folder, not its cache, whose run homes runs write. No sandboxed run reads
  there, and none may write there (`nofile::tests`, against `sandbox::write_places`). It's made through no link:
  the folder 0700 and this user's, opened without following one, and the pin linked and renamed inside it. Its
  (device, inode) is checked before each start: if it changed behind tarae's back, there's no shim from then on
  (the raised limit), said in the log once. Between that check and the shim's exec, nothing inside the sandbox
  can swap it. Each pin has a lock its tarae holds; at start, on a worker, pins whose lock is free (a crashed
  tarae's, each keeping an old build alive) are removed. Where no hard link can be made (the state folder on
  another volume), there's no pin: the binary itself is checked the same way, so after a rebuild what tarae
  posix_spawns keeps the raised limit. That's said once in the log. That costs **+0.29–0.36 ms** per start on colima (two runs: 0.17 → 0.47 and
  0.19 → 0.55 ms, `true`, perf profile, `perf_e2e_nofile_shim_start`), **+2.1–2.3 ms** on macOS (inside Seatbelt; any extra exec costs ~1 ms
  there). macOS gets the shim too: there `FD_SET` skips a descriptor past 1024 and `select` refuses such an
  `nfds` (measured, `nofile::tests`), but a program built with an SDK older than macOS 11, or one setting the bits
  itself, still writes past its set. Left raised: git (tarae's own, started often — it sizes its open packs by the
  limit), claude (started at 256, node's child sees 1048575 and Bun's unlimited: the runtime raises its own), and
  tarae's short queries of fixed tools (a pack's `ask`, the container engine's CLI, the clipboard, the log
  readers), which open a handful of descriptors and end.

## Open items

- ~~**macOS: this machine's own addresses beyond loopback.**~~ Closed by the egress proxy (step 5): a run reaches
  the network only through it, and `Egress::refuses` refuses this machine's own addresses on both systems (G7 in
  the threat model); the risky ports at other hosts too, unless a host rule names the port (audit F3, 2026-10-17).
- **Linux: own addresses read once a policy.** An address that comes up during a run (a bridge, a new lease) is
  beyond this machine to that run until its next policy.
- **The fresh `/proc`'s cost under Landlock** (+0.2 s on a Gradle run 2 with a PID namespace, step 4's M4): a
  micro-benchmark — `/proc/self/stat` read 100,000 times under the same rules, without a PID namespace and with one — would say whether it's per lookup,
  and what in Landlock's walk costs more on the fresh procfs instance. On the roadmap.
- **A tool's cache folders that aren't there yet** (`cargo fetch` on a fresh `~/.cargo`, step 4's M5): with the
  network open their grant covers only what exists when the policy is made. The option chosen: tarae makes the
  pack-declared cache folders before the run (as `linux::Made` does, through no link), and the report names the
  refused folder in both cases — today it does only when `~/.cargo` itself is missing (under an existing
  `~/.cargo` the policy calls the path writable, so `said_in_output` doesn't count it). A small follow-up, queued
  by the user.

## Appendix: How each test was seen failing

The test records §Steps condensed, word for word as this document had them while each step was built, grouped by
the roadmap's step (the document's own numbers then, where they differ, in brackets). The commit messages hold few
of them, so this is their copy.

### Step 1, Landlock's network rights

- Tests: `landlock::tests::a_runs_own_connect_is_refused_by_the_kernel` (TCP connect and bind refused; tarae's
  connect on the run's socket reaches the listener), `a_runs_own_unix_connect_is_refused_by_the_kernel` (from 9),
  `a_runs_own_udp_send_is_refused_by_the_kernel` (from 10: a send to an address and a bind refused; a plain send
  on a socket connected for it arrives), the supervisor's run tests unchanged, the golden `sandbox_landlock.txt`
  for ABIs 3, 4, 6, 9 and 10.
- Broken once: no network right handled (TCP test); `RESOLVE_UNIX` granted, and UDP not handled (golden).
- Ran (2026-10-06): colima `tarae-test`, Linux 6.8, Landlock ABI 4 — every test, the unix and UDP ones skipping
  themselves; a GCP VM on Ubuntu 26.10 daily, Linux 7.3.0-1002-gcp x86_64, which reports **ABI 11** — every
  `landlock::tests` test ran, none skipped (TCP, unix paths, UDP, signal scopes), and `cargo xtask linux test
  sandbox` passed (111), every supervisor run test with `RESOLVE_UNIX` handled and granted nowhere. ABI 11's
  own addition isn't handled: tarae's rulesets ask for the rights up to 10.

### Step 2, one keeper per session

- Tests (`sandbox::keeper::tests`, `sandbox::channel::tests`), each broken once:
  - `the_channel_parser_never_panics`: the protocol module's one fuzz loop, 200,000 mutations of every kind's
    record, each answered with a message (that very record) or a refusal. Broken by reading without its bounds
    check: a panic.
  - `a_message_and_its_descriptors_cross_the_socket`.
  - `the_keeper_ends_with_its_channel`. Broken by ignoring EOF.
  - `the_keeper_is_not_dumpable`, as an ordinary user. Broken by dropping `PR_SET_DUMPABLE` 0.
  - `a_command_holds_only_its_stdio`. Broken by leaving the channel open in the child.
  - `a_command_holds_no_capability`, step 3's test, here for C/D. Broken by not dropping capabilities: root's set
    showed.
  - `exit_status_and_cancel_go_through_the_keeper`. Broken by ignoring `Signal`.
  - `tarae_takes_the_listener_of_a_keeper_child`, as an ordinary user: a port the policy opens is reached, port 9
    refused, and the refusal comes out through `Refused`. Broken by leaving the child non-dumpable: "handing its
    calls to tarae: Operation not permitted".
  - `leftovers_are_the_keepers_children` (a run forks `env -i setsid sleep`). Broken by no
    `PR_SET_CHILD_SUBREAPER`: its parent was init.
  - `the_sessions_keeper_is_known` (`:procs`). Broken by leaving the line out.
  - `a_killed_keeper_fails_closed_and_is_remade`. Broken by leaving its runs' waits unanswered.
  - `a_keeper_says_what_it_uses`.
- **As an ordinary user, the dumpability tests run in a process of their own** (uid 65534 when the container makes
  it root). Root with `CAP_SYS_PTRACE` reads and takes from any process, and a test keeper's libtest main thread
  keeps root's capabilities, so as root these tests couldn't fail.
- **Every Linux sandbox test passes on both VMs (121), with one changed**:
  `supervisor::tests::a_runs_detached_child_is_served_after_its_run`. A detached child's parent is now the
  session's keeper, not tarae. It's served, reaped and ended whether tarae itself is a subreaper or not, with
  `CAP_SYS_PTRACE` dropped too.

### Step 3, namespaces by tier

- **Tests**, each broken once (`target/sb/step3-notes.md`):
  - `a_command_holds_no_capability`, broken by no bounding-set drop;
  - `a_run_keeps_the_users_own_ids` (D5, as uid 1000), by a root map;
  - `no_pid_namespace_without_its_own_proc` (D12, a locked overmount of the scene's own);
  - `a_run_sees_only_its_sessions_processes` (pids, `lo`, SysV IPC);
  - `the_session_mounts_keep_landlocks_rules`, by granting only this system's roots;
  - `a_project_in_dev_shm_shares_dev_shm`;
  - `a_pty_comes_from_the_sessions_devpts`;
  - `a_command_cant_signal_the_keeper`, by a keeper with a TERM handler;
  - `a_runs_pid_comes_from_its_pidfd`;
  - `an_open_policys_keeper_has_no_netns_yet` (since step 4:
    `an_opened_policys_keeper_has_its_netns_where_the_swap_works` — since 6f `an_opened_policys_keeper_has_its_netns`);
  - `in_plain_mode_a_run_reaches_only_its_own_loopback`;
  - `tier::tests::the_probe_says_why` and `the_probe_here`.
- **Every step-2 test runs in the probe's tier.** The test helpers `keeper::outer_pid` and `seen_by_runs` translate
  the pids a run prints.
- **The tier tests reach a real A+** with `cargo xtask linux --userns` (the container's seccomp, AppArmor and
  masked paths lifted; `--userns=masked` keeps `/proc` masked, tier A). `--env TARAE_EXPECT_TIER=A+` makes a
  machine's expected tier fail rather than skip.
- **Ran (2026-10-06), 166 tests each, all passing:**
  - Debian 12 (Linux 6.1, ABI 2): A+, A and C.
  - colima (6.8, ABI 4): C under Docker's seccomp and under Ubuntu's restriction.
  - GCP Ubuntu 26.10 (7.3, ABI 11): C both ways, and A+ with the restriction lifted by the user for the run.

### Step 4, routing every socket

- **Tests**, each seen failing once with its break:

  | Test | Broken by |
  |---|---|
  | `route::tests::the_route_table` (every row, both tiers; GCP's resolver: port 53 out, its other ports never) | loopback always to this machine; a resolver matched by address alone |
  | `route::tests::resolvers_are_read_as_glibc_reads_them` | every line's second word taken |
  | `two_runs_share_the_sessions_loopback` (closed, and with `localhost` on: the session first) | loopback to this machine |
  | `the_users_loopback_isnt_reached` | the same: the user's own listener was reached |
  | `an_opened_port_reaches_this_machine` (and the session's own listener on that port first) | opened ports ignored |
  | `a_swapped_socket_keeps_nonblocking` (`EINPROGRESS`, `O_NONBLOCK`, close-on-exec, options, no device) | `O_NONBLOCK` not carried |
  | `a_bound_socket_isnt_swapped` (a port of its own refused; `0.0.0.0:0` goes out) | the bound check skipped |
  | `a_resolvers_port_reaches_this_machine` (D-c, a stand-in resolver on its own port) | the resolvers row dropped |
  | `the_never_reached_are_refused` (D-d; multicast, so a break sends nothing) | D-d's check removed |
  | `in_tier_c_the_network_open_goes_out_too` (D-e) | C's out-bound connect left on its `lo`-bound socket |
  | `a_routed_runs_own_connect_is_refused_by_the_kernel` (ABI ≥ 4) | `Routed`'s rules leaving TCP unhandled |
  | `the_swap_is_probed` (gone in 6f with the probe) | answering without `SETFD` |
  | `the_families_are_what_glibc_counts` (the decision, pure) | IPv4 never counted |
  | `an_ipv4_lookup_finds_an_address_in_the_session` | no `SESSION_V4` on `lo`: `getaddrinfo -2`; IPv4 never counted |
  | `an_ipv6_lookup_finds_an_address_where_tarae_has_ipv6` (the family forced at the birth: the test containers have no IPv6; without it, nothing found) | no `SESSION_V6` on `lo`: `getaddrinfo -2` |
  | *After the review (2026-10-07):* | |
  | `seccomp::tests::the_filter_knows_only_the_native_calls` (TCP/UDP by protocol in `Routed` and `OwnNetwork`; `listen` to tarae) and `a_run_makes_tcp_and_udp_sockets_only` (SCTP, MPTCP, UDP-Lite, ICMP refused in the session) | the protocol checks always allowing: SCTP and MPTCP sockets made on Debian (`0 0 0 0 0 13`) |
  | `route::tests::this_machines_own_addresses_are_this_machine` and `this_machines_own_address_is_reached_only_where_opened` (the addresses as `Here::now` reads them; C and the netns: refused, then reached once opened) | own addresses never asked: a run reached a listener on the container's own address with the network open |
  | `a_socket_put_out_of_the_session_isnt_listened_on` (C and the netns; after a refused connect, and after `connect(AF_UNSPEC)` undid one made) | the swapped check in `listen` skipped: `listen=0` |
  | `a_swapped_socket_is_taraes_before_the_run_holds_it` (a second thread connects the same number to loopback while the supervisor pauses right after `add_fd` — `Policy.swap_pause`, test-only) | the swap recorded after `add_fd`: the second connect was made on tarae's socket and reached the user's loopback listener (`lo=0 out=106`) |
  | `route::tests::unread_own_addresses_only_refuse` (own addresses unread: an opened port on any address refused, nothing out) | an unread list counted as "mine": `93.184.216.34:3000` became `OwnAddress` |
  | `without_the_swap_nothing_goes_beyond_loopback` (gone in 6f: no kernel at or above the 5.14 floor lacks the swap) (tier C, the swap forced missing — `Policy.test_swap_missing`: out refused and said, the loopback port reached, the report loopback only) | the swap's check skipped: the connection arrived out |
  | `a_socket_from_outside_the_session_is_refused` (two TCP sockets and a unix one made outside, handed over a unix socket in the project: connect to loopback, listen, abstract name) | the netns check always passing: `connect=0 listen=0 abstract=0` — the user's listener and an abstract name of this machine's reached |
  | `route::tests::the_route_table`'s metadata rows (`route::METADATA`: Alibaba's, EKS Pod Identity's, Google's IPv6; NAT64 and mapped forms; Alibaba's neighbours out) | the non-link-local list dropped |
  | `linux::tests::the_report_says_how_linux_differs`'s unread case ("connect to nothing but this machine's loopback … llm.sandbox-network notwithstanding") | the report's unread case skipped |
  | `route::tests::the_route_table`'s NAT64 and 6to4 rows (and `64:ff9b:1::/48`, the local-use prefix whose embedding varies, pinned as not covered) | the carried IPv4 address not checked |
  | `a_mirror_that_fails_leaves_the_birth_going` (both mirrors on a device that isn't there: born, `lo_missing` said, IPv6 lookup finds nothing) | the IPv6 mirror's failure fatal again: `BirthError { step: Lo, errno: 19 }` |

- **Changed for the routing**: in a netns a closed loopback port, a datagram there and an abstract name are the
  session's own, so `this_machines_ports_as_the_policy_says` and `unix_sockets_where_macos_allows_them` expect
  `ECONNREFUSED`/sent there, not a rule's `EPERM`; `gradle_builds_in_the_sandbox` builds with this machine's ports
  closed (its daemon is the session's); the scenes that needed a refusal in every tier use `192.0.2.1:9` with the
  network closed. The swap's tests reach a listener of theirs on the container's own address with a policy made
  with no own addresses (`own_addresses: Some([])`), so the routing takes it as beyond: a target truly beyond
  would send packets off the machine, which no test does.
- **Ran (2026-10-07)**: colima C (6.8, ABI 4) and Debian 12 `--userns` A+ (6.1, ABI 2) full suites; GCP Ubuntu
  26.10 A+ (7.3, ABI 11, the restriction lifted for the run) full suite with `TARAE_EXPECT_TIER=A+`; macOS
  clippy. After the review's four findings: colima C and Debian A+ full suites again (1637 + 43 e2e each).

### Step 5, the egress proxy

- Tests: `the_connect_parser_never_panics`, `the_sni_parser_never_panics`, `connect_heads_over_the_limit_are_refused`,
  `a_slow_client_is_cut_at_the_time_limit`, `loopback_link_local_and_metadata_are_refused` (a stand-in resolver
  answering `127.0.0.1`, `169.254.169.254`, `fe80::1`), `it_connects_to_the_address_it_checked`,
  `sni_must_match_the_connect_host`, `plain_http_through_the_proxy_is_405`, `a_refusal_lands_in_the_sessions_report`,
  `two_sessions_rules_stay_apart`, `the_proxy_knows_only_what_a_message_carries` (D23: the proxy runs against a
  stand-in supervisor that only writes messages to its channel — a connection whose rules allow a name is
  relayed, the same name under another message's rules is refused, and `Refused` comes back on the channel) — the
  "outside" is always the test's own listeners on this machine.
- Break once: connect by name instead of the checked address.

### Step 7, sessions kept apart (then "6. Per session", as planned)

Written before it was built; its tests became step 7's `two_sessions_cant_reach_each_others_processes`, `…_sockets`
and `two_sessions_cant_write_each_others_files`, whose breaks this document never recorded.

- Tests: `two_sessions_cant_reach_each_other` (A+: loopback, abstract sockets, pids, IPC, pty, `/dev/shm`),
  `a_new_conversation_keeps_the_keeper`, `a_key_change_ends_the_keepers_daemons`.
- Break once: one keeper for two sessions.

### Step 8, the loopback rule (then "7.")

- Tests: `two_sessions_cant_reach_each_others_sockets` (with `llm.sandbox-localhost` on),
  `a_session_reaches_its_own_listener`, `the_users_listener_is_reached_where_opened`,
  `an_ended_sessions_sockets_leave_the_table`.
- Broken once each: the owner lookup skipped; owners keyed by run; every unknown socket taken as another
  session's; no prune at the keeper's end.
