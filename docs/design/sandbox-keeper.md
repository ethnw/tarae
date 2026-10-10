# The sandbox keeper: one per session, its namespaces, one egress proxy (Linux)

Design, 2026-10-06. Status: **decided** (D2–D23 below). Steps 1–4 are built (§Steps): every Linux session's
sandboxed runs are forked by its keeper, born in the session's namespaces where user namespaces work
(`src/sandbox/keeper.rs`, `src/sandbox/tier.rs`, `src/sandbox/channel.rs`); in the session's netns a run makes its
own sockets and tarae routes every call that names an address (`src/sandbox/route.rs`, `supervisor.rs`).
macOS is unchanged: Seatbelt, as `src/sandbox.rs` has it.

## What it's for

Today's Linux sandbox (`landlock.rs`, `seccomp.rs`, `sandbox/supervisor.rs`) confines each command's files and decides
each address it reaches. It leaves four things open:

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
| D12 | Where `/proc` can't be mounted there is no PID namespace (tier A), never the host's `/proc` |
| D13 | The boundary is the session. A new conversation, a session behind and the main checkout's session each keep their keeper — a new conversation unless the last one granted writes or ports: those are in the policy key (D14) |
| D14 | Any policy-key change ends the keeper; its daemons start cold once |
| D15 | The keeper is born by `clone3` from tarae, with raw system calls until its exec |
| D16 | `/tmp` stays shared |
| D18 | Another session's loopback listener is refused, even with `llm.sandbox-localhost` |
| D21 | Later, its own step: the supervisor and the proxy as one confined process per session |
| D23 | The supervisor and the proxy talk only over a Rust channel (`mpsc`) of typed, owned messages. **Supervisor → proxy**: the traffic end (`OwnedFd`), the session's id, and the connection's rules as a read-only snapshot (an `Arc`, never mutated). **Proxy → supervisor**: `Refused` and log lines. No socketpair between them, and no shared mutable state (no `Mutex` or other cell both sides write). The run's traffic socketpair stays. **Why not a socket**: in one process a socket isolates nothing; the channel keeps the same discipline at no serialization cost. If D21 is ever done, these messages move onto the keeper channel's wire format then (`channel.rs`). The keeper's own channel stays `SOCK_SEQPACKET` + `SCM_RIGHTS`: the keeper is another process |
| D-c | ~~With the network open, the resolvers `resolv.conf` names are reached on port 53 only~~ (2026-10-07) — superseded: through the proxy names are the proxy's (step 5c), in a TPROXY session the session's (6e); 6f took the route's last of it out |
| D-d | Link-local addresses (the cloud metadata servers among them), multicast and broadcast, and their IPv4-mapped and carried forms, are never reached (2026-10-07; D-c's port-53 exception gone in 6f) |
| D-e | ~~Tier C goes out by the same swap~~ (2026-10-07) — superseded: tier C goes out by the proxy's variable and the pair (step 5; kept in 6f, F2) |
| D-f | Abstract unix names are the session's own in its netns (bound and reached there); refused in C (2026-10-07) |

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
| **Holds** | the session's rules per socket | the run's listener; descriptors it takes with `pidfd_getfd`, or by copies its own copy of every socket the run has (§By copies) | stdio from `/dev/null`, its channel's end; no capability |

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

| Property | A+ / B+ | A / B | C / D | Test |
|---|---|---|---|---|
| non-dumpable | yes | yes | yes | `the_keeper_is_not_dumpable` |
| no capability in what it forks | dropped before exec | dropped before exec | dropped at its start (it has some only as root) | `a_command_holds_no_capability` |
| unreachable by its runs' signals | PID 1, and Landlock ABI 6 | Landlock ABI 6; below it a run can kill it | as A / B | `a_command_cant_signal_the_keeper` (from ABI 6, and below it in A+/B+); else `a_killed_keeper_fails_closed_and_is_remade` |
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
`net`): `Routed` in a netns tier served, `Served` in C, `Own` and `Plain` unserved. In `Routed` the run makes its own
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
| a question to port 53, network open, in a TPROXY session (6e) | **the session's**: answered by tarae (port 53 opened by the user, or a run's own server at that address: this machine's / the session's) | `Route::Redirected`: the run's own socket, marked |
| a resolver `resolv.conf` names, its port 53, network open, through the proxy (step 5c) | refused: `EPERM`, `network-direct`, said as a lookup | names are the proxy's to look up. Read once into `Policy.resolvers` (`route::resolvers`, glibc's three) |
| link-local (with `169.254.169.254`), the cloud metadata servers that aren't (Alibaba Cloud's `100.100.100.200`; AWS's `fd00:ec2::254` and EKS Pod Identity's `fd00:ec2::23`, Google Cloud's `fd20:ce::254`), multicast, broadcast — and those IPv4 ones carried in NAT64's `64:ff9b::/96` or 6to4's `2002::/16` — network open (D-d) | refused: `EPERM`, `network-never` | before any connect |
| the proxy's reserved address (`192.0.2.1:3128`), network open, not in a TPROXY session | **the proxy** (`Route::Proxy`) | the pair (§The egress proxy). In a TPROXY session it's the session's own address, a closed port there (6f) |
| anything else beyond, network open, a TCP connect in a TPROXY session (6d) | **the session's network** (`Route::Redirected`) | the run's own socket, marked; a planted socket takes it, the proxy decides by its host |
| anything else beyond, network open, otherwise | refused: `EPERM`, `network-direct` | only the proxy and the session's network go out (6f took `Out` — a socket of tarae's connected beyond, unreachable since step 5 — away) |
| anything else, network closed | refused: `EPERM`, `network-outbound` | as before |

In tier C (`Served`, no netns) the same table without the session: loopback is this machine's where opened, else
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
so a run reaches only its session's (D-f; this machine's listener on the same name isn't reached); in C they stay
refused.

**Where sessions share a loopback** (tiers C and A without a netns of their own), D18's rule: tarae makes every
internet socket in served mode, so one table holds each socket's inode against its session; a loopback listener is
looked up by inode (`/proc/net/{tcp,tcp6,udp,udp6}`) and refused if it is another session's.

### By copies (*built*, 6f′, `copies.rs`, `target/sb/step6fprime-design.md`)

Where `pidfd_getfd` is refused (Docker's default profile, a container without `--cap-add SYS_PTRACE`), the supervisor
can't take a run's socket to make a call on it. The probe (`supervisor::serving`) then answers `Serving::Copies`, not
"can't serve": the session is born without a netns (every socket is tarae's; Docker refuses new namespaces anyway) and
its runs are `RunNet::Copied` — tier C, said "served by tarae's own copies of their sockets".
- **tarae makes every socket the run has** (`seccomp::Mode::Copied`): internet ones as in C, and also unix stream and
  netlink route ones, `socketpair` and `accept`/`accept4`. Each is kept as a copy by inode before the run holds it
  (`Copies::keep`); a call on descriptor `n` finds it by the inode `readlink /proc/<tid>/fd/<n>` names
  (`socket:[…]`, `PTRACE_MODE_READ` only). Netlink keeps its routing-only rule.
- **A socket tarae has no copy of** (handed in from outside the sandbox) is refused, `EPERM` and `network-bound`,
  said "made outside tarae's sandbox … tarae serves only the sockets it made".
- **A descriptor passed with `SCM_RIGHTS`**: a socket by its copy; a file or pipe opened again through
  `/proc/<tid>/fd/<n>` with its `fdinfo` access flags (an open of its own: its offset isn't the run's); an eventfd,
  epoll or pidfd (`anon_inode:`) can't be, and is refused, said.
- **`accept`** is tarae's, on its copy of the listener: it waits as the run would (the same open socket's
  `O_NONBLOCK`), up to the listener's `SO_RCVTIMEO` (`EAGAIN` after, as the kernel's), in 100 ms slices that look
  for a signal the caller doesn't block. The connection's copy is kept and its peer's address written back. A
  waiting accept holds one supervisor thread, as every slow call does.
- **A signal during a served accept** ends it as the kernel's would where tarae can tell: one sent to the accepting
  thread, or to a process of one thread, is answered `ERESTARTSYS` — the kernel then restarts the call after a
  handler with `SA_RESTART` (tarae serves it anew) and fails it `EINTR` after one without; on a listener with its
  own `SO_RCVTIMEO`, `EINTR` (the kernel's `sock_intr_errno`). Where it can't tell — a signal sent to a process with
  other threads, which the kernel may have given to another — it waits one slice more; still pending, `EINTR`
  whatever the handler (a spurious one after `SA_RESTART`: `ERESTARTSYS` reaching a thread with no signal to take
  would be error 512); gone by then — another thread took it — the accept goes on, as the kernel's.
  `by_copies_a_signal_ends_an_accept_as_the_kernels_would` pins the restart, `EINTR`, the timeout and a signal another
  thread takes live; `a_process_signal_no_other_thread_takes_ends_the_wait_with_eintr` the last case.

**The copies' cost: a late close.** While tarae holds a copy, the run's `close()` sends no FIN and a bound port stays
bound. Copies are collected by scanning: while any is kept, a thread reads the descriptors of every process below the
keeper (`/proc/<pid>/task/*/children`, else every process's parent), and a copy none held in `STRIKES` (2) scans in a
row goes; a copy kept during a scan isn't struck by it. Never early:
- a socket a served `sendmsg` passed is held by no process while the message waits in the receiver's queue, however
  long; its copy is kept while the socket the message went on has anything unread (`SIOCOUTQ` — the kernel frees it
  when the receiver reads the message, or drops it), up to 30 s (`IN_FLIGHT_MAX`), then collected as any;
- a scan that can't read a process's descriptors or children (one that made itself non-dumpable: ssh-agent,
  gpg-agent) strikes nothing — every copy waits until that process is gone. One that ended meanwhile holds nothing.

Scans are paced by what the run does: 50 ms (`INTERVAL`) after a served call, a copy kept or a strike, doubling to
1 s (`QUIET_MAX`) while nothing happens. So a close's FIN comes ≈ 100 ms late while the run is busy, up to ≈ 1 s after
a quiet stretch. A port's release is lazy too: a bind that finds its port taken has the copies collected at once
(`collect_now`, two scans) and, where a copy just went (that collection's, or the scanner's a moment before), tries
again for up to 200 ms (`RELEASE_WAIT`, `Copies::bind_again`). The socket goes only with its last reference, and
tarae's copy isn't always that: any process tarae starts holds every descriptor of tarae's from its fork until its
exec is through. Seen under the test suite's load: the port free 63 µs after the copy went. Where no copy went, the
port is someone else's and the bind fails at once.

Measured on a Gradle build and its idle daemon (≈ 300–460 descriptors, a test-profile build): a scan takes 3–11 ms.
At a fixed 50 ms that was 10–22% of a core, idle included; paced, 1.3% and 5.6% in the two builds and 0.8% idle
(`measure_copies_scan`; the numbers in the design note).

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

**Raw TCP and a name's records, in a TPROXY session (6e+, `target/sb/step6eplus-design.md`).** A connection to an
address of the session's names whose first bytes are neither a ClientHello nor what can start an origin-form request
(`parse::origin`'s `may_start`: ssh's banner, a database's startup), or that sends nothing within W
(`Limits::silent`, 1 s after M14: a server that speaks first), is raw: a rule naming that port lets it through
(`Egress::allows_raw` — a rule without a port, or `any`, doesn't), else the user's raw answers, else the raw card
(`What::HostRaw`), held as the host card holds a connection. Let through, it's connected by the name as a tunnel is and
spliced, what it sent meanwhile first. Raw to an address no name was given stays refused. A run's SRV or TXT question is
looked up by tarae itself (`records.rs`: `resolv.conf`'s nameservers, UDP then TCP where cut short; the response read
by `dns::response`, names compressed, bounded) for the name it's about (an SRV owner's without its `_` labels): at once
where tarae already looks that name up — the rules name it (or `any`), or the user said yes to it on a host or raw
card (E1: its records show the DNS's operators nothing new; every host they name is still asked about at connect) —
else held — no answer, the client asks again — while the lookup card (`What::Lookup`) asks. Through an upstream proxy
no lookup is made. Host and raw answers are kept apart (`proxy::Kind`): a yes for a host over TLS isn't one for raw
bytes to it, nor the other way round.

**Named limits**: domain fronting (SNI and the inner `Host` can differ); Encrypted Client Hello hides the real SNI;
through the proxy's address only TLS (CONNECT) and plain `http://` go out; raw TCP is only a TPROXY session's, and what
passes on it isn't checked — only that it goes to the name the run looked up; to another machine by its address, raw
or not, nothing goes (no rule names an address).

## Tiers

| Tier | The keeper's namespaces | Supervisor | Beyond this machine, the network open (6f) | Leftovers owned by |
|---|---|---|---|---|
| **A+** | user, net, ipc, pid, mnt (`/proc`, `devpts`, `/dev/shm`) | serving | with TPROXY (`tier::Probed.redirect`): the session's network — every TCP connect and every name asked taken by tarae, no proxy variable (6d, 6e, 6e′); without it: the proxy's variable and the pair, its own loopback kept (6f's F3) | the keeper's PID namespace: one kill |
| **A** | user, net, ipc, mnt (`devpts`, `/dev/shm`); no fresh `/proc` | serving | as A+ | the keeper's tree |
| **B+ / B** | as A+ / A | plain (Yama 2/3, inside another listener) | nothing: no socket at all but netlink | as A+ / A |
| **C** | none (no unprivileged user namespaces; or served by copies — Docker's default profile, §By copies) | serving (by copies: every socket tarae's) | the proxy's variable and the pair (6f's F2) | the keeper's tree |
| **D** | none | plain (Yama 2/3, inside another listener) | nothing: no socket at all but netlink | the keeper's tree |

In every served tier opened ports and this machine's own addresses go by the swap (§Sockets and routing) until connect
leaves the supervisor (Landlock ABI 9's step, D2). The file guarantees (Landlock ABI ≥ 3 full, 2 partial, < 2 none)
are orthogonal (`linux::availability`).

**What C/D still keep apart between sessions**: writes (Landlock), unix sockets by path (the supervisor, ABI 9),
loopback listeners (D18), abstract sockets and signals from ABI 6, the environment (`runenv.rs`), and leftovers (the
keeper's tree). **What they don't**: one process view (`cmdline`, `status`, `/proc/<pid>/net`, and `setpriority`,
`sched_setaffinity`, `prlimit64` on others' processes — no Landlock hook covers those), one SysV IPC, the user's ptys
and `/dev/shm` (`DEVICES` grants them), and below ABI 6 signals. Reads of the repository's other checkouts and `/tmp`
are shared in every tier (D16).

**The probe**, once on a worker: Landlock's ABI and the seccomp architecture as today, then a keeper made for real and
a `true` run through it under a time limit — a timeout is no namespaces, not success — and, with a netns, whether
the session's network lands (TPROXY). What failure says:

| Seen | Why |
|---|---|
| the first mount refused (`EPERM`), `apparmor_restrict_unprivileged_userns` = 1 | Ubuntu 23.10+/24.04's restriction: the new namespace has no capability. tarae writes the id maps from outside, as the namespace's owner, so that part works; measured on colima and Ubuntu 26.10 (`refused at making its mounts its own`) |
| the id map write refused (`EACCES`) while tarae is non-dumpable | its ids changed after it started: its children's `/proc` files are root's |
| a mount refused under an AppArmor profile other than `unconfined` | the profile (Docker's `docker-default`) denies mounting |
| `clone3`/`unshare` EPERM or ENOSYS in a container | Docker's default seccomp profile |
| ENOSPC, `user.max_user_namespaces` = 0, `kernel.unprivileged_userns_clone` = 0 | user namespaces turned off |
| `/proc/version` says WSL 1 | WSL 1 |
| mounting `procfs` refused | `/proc` masked (Docker's default): tier A, not A+ |

The remedy on Ubuntu is said once: an AppArmor profile granting `userns` to tarae's binary, else the sysctl (said
second: it weakens the whole system). tarae ships no profile.

## Showing it

One value, `sandbox::Guarantees`, said in four places, each true for that run: a sentence in the report Claude reads
("its localhost is the session's own — what it leaves listening its later runs reach, nothing of yours"); the state
line of `:sandbox-blocked`'s panel (`Landlock 8 · files full · network: session + tarae · kernel locks TCP UDP unix ·
signals scoped`, a fallback's reason faint below); `tarae state`'s `sandbox` object (`uistate::V` up, `--watch
sandbox`); `TARAE_SANDBOX_STATUS` = tier and ABI (`A+:8`, `C:6`) in the run's environment.

## Steps

Each lands on its own. The test that pins it is seen failing once with the change broken.

1. **Landlock's network rights.** *Built*: TCP (ABI 4), unix paths (ABI 9), UDP (ABI 10, shipped in Linux 7.2; the
   numbers from the v7.2 tag's `include/uapi/linux/landlock.h`,
   https://raw.githubusercontent.com/torvalds/linux/v7.2/include/uapi/linux/landlock.h).
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
2. **The keeper without namespaces, for every session; the supervisor behind its few messages** (tiers C/D).
   - *Built*:
     - `channel.rs` (the protocol);
     - `keeper.rs` (both sides; `--sandbox-keeper` first thing in `main`);
     - the supervisor in, the policy at its run's start and its listener; out, `Refused` on a channel
       (`supervisor::Served`, `sandbox::told`);
     - the confining thread + `posix_spawn` path removed (`landlock::confined` stays for its own tests).
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
3. **Namespaces at the keeper's birth** (A+/A/B+/B). *Built*: maps, mounts, `lo`, capabilities dropped, the
   session's device handles, a run's pid from its pidfd (held), plain mode's own network, the probe and its reasons
   (`tier.rs`), the tier said in the log and `:procs` (the full showing is step 8).
   - **Two interim rules**: D-a, no netns for a policy that opens anything, until step 4; and no private `/dev/shm`
     for a project in it.
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
4. **Sockets inside the session, routing and the egress swap** (D2). *Built* (2026-10-07; plan and measurements
   `target/sb/step4-plan.md`, `step4-notes.md`): `route.rs`, `seccomp::Mode::Routed`, `RunNet::Routed` and the
   channel's `net`, the swap and its probe, D-a only where the swap is missing, D-c–D-f as decided (§Sockets and
   routing), the session's `lo` mirroring tarae's address families (`Families`), the supervisor's counters (one log line per served run: calls by
   kind, time in tarae).
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
5. **The egress proxy.**
   - Tests: `the_connect_parser_never_panics`, `the_sni_parser_never_panics`, `connect_heads_over_the_limit_are_refused`,
     `a_slow_client_is_cut_at_the_time_limit`, `loopback_link_local_and_metadata_are_refused` (a stand-in resolver
     answering `127.0.0.1`, `169.254.169.254`, `fe80::1`), `it_connects_to_the_address_it_checked`,
     `sni_must_match_the_connect_host`, `plain_http_through_the_proxy_is_405`, `a_refusal_lands_in_the_sessions_report`,
     `two_sessions_rules_stay_apart`, `the_proxy_knows_only_what_a_message_carries` (D23: the proxy runs against a
     stand-in supervisor that only writes messages to its channel — a connection whose rules allow a name is
     relayed, the same name under another message's rules is refused, and `Refused` comes back on the channel) — the
     "outside" is always the test's own listeners on this machine.
   - Break once: connect by name instead of the checked address.
6. **Per session** (D13, D14).
   - Tests: `two_sessions_cant_reach_each_other` (A+: loopback, abstract sockets, pids, IPC, pty, `/dev/shm`),
     `a_new_conversation_keeps_the_keeper`, `a_key_change_ends_the_keepers_daemons`.
   - Break once: one keeper for two sessions.
7. **The loopback rule where sessions share a loopback** (D18, tiers C and A).
   - Tests: `a_session_cant_reach_anothers_listener` (with `llm.sandbox-localhost` on), `a_session_reaches_its_own_listener`.
   - Break once: skip the owner lookup.
8. **Showing it** (D7). Tests: golden `sandbox_report.txt` per tier, the `ui_state.json` snapshot; a screenshot first.
9. **Docs and CLAUDE.md.**
10. **Later, each its own step**: `:sandbox-expose <port>` (D3); the supervisor and the proxy as one confined process
    per session (D21).

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

## Open items

- **macOS: this machine's own addresses beyond loopback.** Seatbelt names a port by `localhost` only, so with
  `llm.sandbox-network` on a command reaches the risky ports (Docker's 2375, Jupyter's 8888…) on this machine's
  LAN or bridge address as the network. Linux takes those addresses as this machine (§Sockets and routing); macOS
  can't express it — its report and `llm.sandbox-network`'s description say so. A way would be the egress proxy
  (step 5) checking the addresses it connects to, on macOS too.
- **Linux: own addresses read once a policy.** An address that comes up during a run (a bridge, a new lease) is
  beyond this machine to that run until its next policy.
- **The fresh `/proc`'s cost under Landlock** (A+ +0.2 s on a Gradle run 2, step 4's M4): a micro-benchmark —
  `/proc/self/stat` read 100,000 times under the same rules, in A against A+ — would say whether it's per lookup,
  and what in Landlock's walk costs more on the fresh procfs instance. On the roadmap.
- **A tool's cache folders that aren't there yet** (`cargo fetch` on a fresh `~/.cargo`, step 4's M5): with the
  network open their grant covers only what exists when the policy is made. The option chosen: tarae makes the
  pack-declared cache folders before the run (as `linux::Made` does, through no link), and the report names the
  refused folder in both cases — today it does only when `~/.cargo` itself is missing (under an existing
  `~/.cargo` the policy calls the path writable, so `said_in_output` doesn't count it). A small follow-up, queued
  by the user.
