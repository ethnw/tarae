# The sandbox keeper: one per session, its namespaces, one egress proxy (Linux)

Design, 2026-10-06. Status: **decided** (D2–D23 below). Steps 1 and 2 are built (§Steps): every Linux session's
sandboxed runs are forked by its keeper, without namespaces yet (`src/sandbox/keeper.rs`, `src/sandbox/channel.rs`).
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
| D13 | The boundary is the session. A new conversation, a session behind and the main checkout's session each keep their keeper |
| D14 | Any policy-key change ends the keeper; its daemons start cold once |
| D15 | The keeper is born by `clone3` from tarae, with raw system calls until its exec |
| D16 | `/tmp` stays shared |
| D18 | Another session's loopback listener is refused, even with `llm.sandbox-localhost` |
| D21 | Later, its own step: the supervisor and the proxy as one confined process per session |
| D23 | The supervisor and the proxy talk only over a Rust channel (`mpsc`) of typed, owned messages. **Supervisor → proxy**: the traffic end (`OwnedFd`), the session's id, and the connection's rules as a read-only snapshot (an `Arc`, never mutated). **Proxy → supervisor**: `Refused` and log lines. No socketpair between them, and no shared mutable state (no `Mutex` or other cell both sides write). The run's traffic socketpair stays. **Why not a socket**: in one process a socket isolates nothing; the channel keeps the same discipline at no serialization cost. If D21 is ever done, these messages move onto the keeper channel's wire format then (`channel.rs`). The keeper's own channel stays `SOCK_SEQPACKET` + `SCM_RIGHTS`: the keeper is another process |

## The shape

```
 tarae                                                     │ per session
 ─────                                                     │ ───────────
 the egress proxy: one, threads ◀── mpsc: traffic end ─┐   │ the KEEPER (a subprocess):
 the supervisor: threads, state per session ───────────┘   │   with user namespaces: PID 1 of user+net+ipc+pid+mnt
   decides each address, makes each allowed call           │   without: a fork server and child subreaper
 ── request channel (socketpair, close-on-exec) ──────────▶│   forks every run; parent or reaper of what runs leave
 ◀── each run's pidfd · status · its listener by pidfd_getfd│     └─ run: Landlock + seccomp of its own, then exec
```

| | Proxy | Supervisor | Keeper |
|---|---|---|---|
| **Where** | threads in tarae | threads in tarae | a process: `tarae --sandbox-keeper` |
| **How many** | one for all sessions | one set per session (its policy, grants, report, log) | one per session, in every tier |
| **Does** | parses CONNECT and the TLS ClientHello's SNI, resolves the name itself, checks every address, connects to the very address it checked, relays | takes each run's seccomp listener, reads the address, decides (`may_reach`, the route), and makes the call itself on the run's socket. Never `CONTINUE` | forks each run, reaps it, reports its status; owns what runs leave |
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

| Tier | Birth | What it is |
|---|---|---|
| with user namespaces | `clone3(CLONE_NEWUSER \| NEWNET \| NEWIPC \| NEWPID \| NEWNS \| CLONE_PIDFD)` | PID 1 of the session's namespaces |
| … where `/proc` can't be mounted (Docker's masked `/proc`) | the same without `CLONE_NEWPID` | the namespaces' fork server, a child subreaper |
| without user namespaces (*built*) | `clone3(CLONE_PIDFD)`: no namespaces — or `clone(2)` with the same flag where a filter answers `clone3` with `ENOSYS` (Docker's default profile does, so that libcs fall back) | a plain fork server and child subreaper (`PR_SET_CHILD_SUBREAPER`) |

Until its exec the child is a copy of a multithreaded process, so it makes raw system calls only:

1. It waits on a pipe while tarae writes `setgroups` = `deny`, `uid_map` and `gid_map`: the user's own ids (D5).
2. Mount propagation to slave; a fresh `procfs` on `/proc`; a new `devpts` instance with its `ptmx` bound over
   `/dev/ptmx`; a `tmpfs` on `/dev/shm` (D10).
3. `lo` up (`ioctl(SIOCSIFFLAGS)`).
4. Every capability dropped: each bounding-set bit, then `capset` to zero, no ambient set.
5. stdio to `/dev/null`, its channel to fd 3, and `SIGCHLD` blocked, so every thread of the new process starts with it
   blocked.
6. `execveat` of tarae's own binary (an `O_PATH` descriptor of `/proc/self/exe`, opened once: the binary running, not
   whatever replaced it on disk) as `tarae --sandbox-keeper`, with an **empty environment**.

Steps 1–4 are the namespaced tiers'. **A keeper that can't start fails the run with the reason. It never falls back to
running unsandboxed.** The confining thread + `posix_spawn` path is retired.

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
| `Run { id, program, argv, envp, cwd, comm, served }` + `SCM_RIGHTS` | `program` resolved by tarae (`spawn::resolve`); `envp` the run's environment as `spawn::environment` makes it; `comm` the name the run gives itself (≤ 15 bytes). The descriptors: the Landlock ruleset tarae built (`linux::confine`), stdin (`/dev/null`, tarae's), stdout, stderr, and served, the run's handoff socket. A malformed request gets `Failed`, never a crash. `pty` comes with step 3 |
| `Signal { id, sig }` | cancel: the keeper signals the run's process group, only while it hasn't reaped it (`dap::term_group` asks here: once the keeper has reaped a run, a kill from tarae could reach a reused pid) |
| `AskUsage` | its own Rss and Pss (M3): its `smaps_rollup` is unreadable to tarae |
| `End`, or EOF | end every run and leftover, then exit |

| keeper → tarae | |
|---|---|
| `Ready { pid }` | once, at its start |
| `Started { id, pid }` + `SCM_RIGHTS` | the child's pidfd. The main loop waits for it (≤ 1 s, as it waited for `posix_spawn`) |
| `Exited { id, status }` | the keeper is the child's parent and reaps it (`spawn::wait` asks here: a keeper's run isn't tarae's child) |
| `Failed { id, errno, step }` | the child failed before exec (`chdir`, Landlock, seccomp, the handoff, exec), also said in its stderr; it exits 127 |
| `Reaped { n }` | orphans it reaped, as a count (the log) |
| `Usage { rss_kb, pss_kb }` | the answer to `AskUsage` |

**The forked child**, step by step:
1. Closes the channel.
2. Sets `PR_SET_DUMPABLE` 1 again, for the handoff below. Without a user namespace, `pidfd_getfd` needs ptrace access
   to the child, which a non-dumpable process refuses unless the caller has `CAP_SYS_PTRACE`. With one (step 3),
   tarae's capabilities in the namespace cover it. A dumpable run is what every run was before, and what exec makes an
   ordinary program anyway. Landlock's ptrace rule keeps other confined runs off it.
3. `setsid` (and the controlling terminal with `pty`); `dup2`s its stdio; `chdir(cwd)`.
4. `PR_SET_NAME` = `comm`, which Landlock's audit names beside the run's own pid.
5. Unblocks every signal.
6. `landlock_restrict_self(ruleset)`; then seccomp (`seccomp::apply`).
7. Served: writes its listener's number on the handoff socket. tarae (a thread of its own, not the main loop) runs
   `pidfd_getfd`, hands the listener to `supervisor::serve`, and writes one byte back.
8. Execs.

Each step's failure is written on a close-on-exec pipe the keeper reads.

tarae uses the pidfd for `pidfd_getfd`. The run's pid comes with `Started`. Yama 1 holds: tarae → keeper → run, and
what runs leave stays below the keeper.

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

In Plain mode inside the session's namespaces (step 3 on) the run makes its own TCP and UDP calls, so those rights are
left unhandled there; the netns, with only `lo`, is the lock. Plain mode without namespaces refuses internet sockets
outright, as today.

## Sockets and routing (tiers with a network namespace)

The run makes its own internet sockets; they belong to the session's netns. `connect`, `bind`, `sendto` with an
address, `sendmsg` and `sendmmsg` still go to the supervisor, which decides with `may_reach` and a route:

| Destination | Route | How |
|---|---|---|
| loopback: a port the session listens on, or one not opened to this machine | **the session** | tarae makes the call on the run's own socket (`pidfd_getfd`); it stays in the session's netns |
| loopback: a port opened by `llm.sandbox-ports`/`-localhost` that nothing in the session listens on (D2) | **this machine** | egress |
| anything else, network open | **out** | egress, through the proxy |
| anything else, network closed | refused | as today |

**Egress**: tarae makes a socket in its own namespace, copies what the run set (`O_NONBLOCK`, close-on-exec, the
options it can read back) and puts it in place of the run's at the same descriptor number (`SECCOMP_IOCTL_NOTIF_ADDFD`
with `SECCOMP_ADDFD_FLAG_SETFD`, Linux 5.14). A non-blocking connect answers `EINPROGRESS` as the run's own would. A
socket the run bound itself is refused with `EADDRNOTAVAIL`: its source address can't move namespaces.

Unix sockets by path cross namespaces, so the supervisor's path rule (the session's project and `RunDir`) and ABI 9
stay their guard. Abstract names are per netns: a run reaches only its session's.

**Where sessions share a loopback** (tiers C and A without a netns of their own), D18's rule: tarae makes every
internet socket in served mode, so one table holds each socket's inode against its session; a loopback listener is
looked up by inode (`/proc/net/{tcp,tcp6,udp,udp6}`) and refused if it is another session's.

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
only TLS and CONNECT go out — plain `http://` and raw TCP beyond this machine (ssh) are refused unless a session rule
names the address and port, and the report says which.

## Tiers

| Tier | The keeper's namespaces | Supervisor | Leftovers owned by |
|---|---|---|---|
| **A+** | user, net, ipc, pid, mnt (`/proc`, `devpts`, `/dev/shm`) | serving | the keeper's PID namespace: one kill |
| **A** | user, net, ipc, mnt (`devpts`, `/dev/shm`); no fresh `/proc` | serving | the keeper's tree |
| **B+ / B** | as A+ / A | plain (Yama 2/3, inside another listener) | as A+ / A |
| **C** | none (no unprivileged user namespaces) | serving | the keeper's tree |
| **D** | none | plain | the keeper's tree |

The file guarantees (Landlock ABI ≥ 3 full, 2 partial, < 2 none) are orthogonal (`linux::availability`).

**What C/D still keep apart between sessions**: writes (Landlock), unix sockets by path (the supervisor, ABI 9),
loopback listeners (D18), abstract sockets and signals from ABI 6, the environment (`runenv.rs`), and leftovers (the
keeper's tree). **What they don't**: one process view (`cmdline`, `status`, `/proc/<pid>/net`, and `setpriority`,
`sched_setaffinity`, `prlimit64` on others' processes — no Landlock hook covers those), one SysV IPC, the user's ptys
and `/dev/shm` (`DEVICES` grants them), and below ABI 6 signals. Reads of the repository's other checkouts and `/tmp`
are shared in every tier (D16).

**The probe**, once on a worker: Landlock's ABI and the seccomp architecture as today, then a keeper made for real and
a `true` run through it under a time limit — a timeout is no namespaces, not success — and whether `ADDFD`'s `SETFD`
is taken. What failure says:

| Seen | Why |
|---|---|
| the id map write refused, `apparmor_restrict_unprivileged_userns` = 1 | Ubuntu 23.10+/24.04's restriction: the new namespace has no capability |
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
3. **Namespaces at the keeper's birth** (A+/A/B+/B): maps, mounts, `lo`, capabilities dropped, the probe, the tiers.
   - Tests: `a_command_holds_no_capability` (C/D's built in step 2), `no_pid_namespace_without_its_own_proc`,
     `a_run_sees_only_its_sessions_processes`, `the_session_mounts_keep_landlocks_rules` (a pty, a POSIX shm file and a
     `/proc/self` read under the run's ruleset), `a_pty_comes_from_the_sessions_devpts`,
     `a_command_cant_signal_the_keeper`, the probe's classification from stand-in errnos.
   - Break once: skip the bounding-set drop.
4. **Sockets inside the session, routing and the egress swap** (D2).
   - Tests: `route` as a pure function over a table of destinations, `two_runs_share_the_sessions_loopback`,
     `the_users_loopback_isnt_reached`, `an_opened_port_reaches_this_machine`, `a_swapped_socket_keeps_nonblocking`,
     `a_bound_socket_isnt_swapped`, M5.
   - Break once: route loopback always to this machine.
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

**Still to measure**: M3 with namespaces (step 3); M4, the Gradle and
Kotlin pairs through the real keeper in A+ and C (step 3); M5, name lookups (`AI_ADDRCONFIG`) inside a session's netns
with the network open (step 4); M8, tools that look at file owners inside the user namespace, where root's files show
as `nobody` (step 3).
