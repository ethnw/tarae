# tarae's threat model

*For someone deciding whether to trust tarae with an AI agent, and for a security reviewer, who should start here.
How the Linux sandbox is built: [the keeper's design](design/sandbox-keeper.md). What each rule does, in the user's
words: [Claude › The sandbox](claude-integration.md#the-sandbox).*

Every claim that something is **kept** names the test that shows it, by its module path — a test of tarae's
(`every_claim_names_a_test_that_exists`) fails when one is gone or isn't where the path says. A test marked **(rule)** pins the
rules tarae hands the kernel, not the kernel holding them; **(shown)** runs a command and sees it refused. Every
thing **not kept** says why, and where it's planned, when.

## In one paragraph

Claude never runs a command as you. Its commands run in a sandbox: they write only in the project (minus the
files that run later), read in your home only what toolchains need, never read keys, and reach no network unless
you turn it on — then only through tarae, which checks every host. Its edits come to you as a review. Its reads
outside the project ask you first. The `claude` program itself runs as you, but in a sandbox of its own (macOS:
Seatbelt; Linux: Landlock): it reads no key folders or files named like keys and writes only its own folders and
temp — not the project, not what runs later (§2). What's not kept: the `claude` program still runs git in the
project and reaches Anthropic's servers, on macOS reads its login from your keychain, and on Linux below Landlock
ABI 3 runs unconfined (§2 lists what it touched, measured); a command can read what's outside your home folder and
the temp folders; with the network on
and no host list, it can send what it read to any host; and on macOS it can read your other processes' command
lines and starting environments.

## Who and what this defends against

| | In scope | Why it's a risk |
|---|---|---|
| **The model, misled** | Prompt injection in what Claude reads: a repository's files, a dependency's README, a test's output, a web page a command fetched | Claude acts on text it didn't write. Assume any turn may try what a hostile author wants |
| **A hostile repository** | What arrives with `git clone`/`pull`: build scripts, `.cargo/config.toml`, git hooks, `.claude/settings.json`, `.tarae.toml`, `CLAUDE.md` — and a checkout that came whole (an archive with its `.git`) | Opening it, building it or chatting in it runs or loads those |
| **A bug in Claude Code's permission path** | A tool call that skips `can_use_tool`, an update that changes what a flag means | tarae relies on claude asking. Partly in scope: see §2 |

Out of scope: another local user or root; malware already running as you; a hostile kernel or OS; a tampered tarae
binary or toolchain; what *you* run (your terminal, your tasks, `:sh`, preflight after its card) — tarae doesn't
sandbox you. Plugins have their own runtime and permissions ([plugins](plugins.md)); not covered here.

## The pieces, and who trusts whom

```
 you ──▶ tarae (trusted: your process, not confined)
          ├─ the claude process  (as you — a files-only sandbox of its own: Seatbelt / Landlock; network open)
          │     tools: Claude Code's Read (each call asks tarae) · tarae's own tools (in tarae)
          │     starts by itself: git, security (its login), sh (system facts)
          ├─ Claude's runs        (sandboxed: Seatbelt on macOS; keeper + Landlock + seccomp + supervisor on Linux)
          ├─ tarae's own git, language servers, debug adapters (as you; git's repository commands held)
          └─ your terminal, tasks, preflight (as you: yours)
```

| Claude wants to… | It goes through | Kept by |
|---|---|---|
| read a file in the project | Claude Code's Read → `can_use_tool` → `reads.rs` | key-named files ask |
| read outside the project | the same → a card (`y`/`a`/`A`/`n`) | home, above it, key folders: once only |
| change a file | tarae's `edit`/`rename`/`replace` (or Edit/Write when enabled) → the review | you |
| run a command | tarae's `run`/`run_tests` → the sandbox | the kernel, and tarae's supervisor and proxy |
| run a command outside the sandbox | `outside_sandbox` + a reason → a card, `y`/`n` only | you, every time |
| see the screen | `screen` → a card | you |
| use git | tarae's `git` tool: reads only, an option allowlist | `gittool.rs` |

---

## 1. Claude's commands (runs)

One sandbox per command on macOS (Seatbelt, `sandbox-exec`). On Linux each session has a *keeper*: a process
that forks every command, born in the session's own namespaces where the system allows; each command gets
Landlock and seccomp of its own, and tarae decides every call that names an address. How much of that a session
gets is its **tier** (A+ … D, §1.8). The rules are the same on both systems; the kernel features differ.

### 1.1 Files it writes

Kept:
- Writes only in the project, its own run folder, the temp folders, and the toolchains' own folders.
  test: `sandbox::tests::seatbelt_keeps_writes_in_the_project` (shown, macOS) ·
  `sandbox::linux::tests::a_run_keeps_to_its_policy` (shown, Linux) · `sandbox::golden::the_policy_is_pinned`,
  `sandbox::golden::landlocks_rules_are_pinned` (rule)
- A run writes only its own run folder, not another run's or another session's.
  test: `sandbox::tests::a_run_writes_only_its_own_run_folder` ·
  `sandbox::tests::runs_folders_where_every_run_writes_are_refused`
- Another session's checkout isn't written.
  test: `sandbox::apart::two_sessions_cant_write_each_others_files` (macOS) ·
  `sandbox::keeper::tests::two_sessions_cant_write_each_others_files` (Linux)
- A cargo/Go folder a checkout's config names opens only when it's that checkout's own.
  test: `sandbox::tests::a_cargo_dir_from_the_checkout_opens_only_its_own` ·
  `sandbox::tests::a_build_folder_the_policy_refuses_is_never_made`
- Shared caches (cargo's `registry`/`git`, Go's module cache) are read-only while the network is closed — what
  your own builds pick up later can't be planted offline.
  test: `sandbox::tests::an_asked_cache_is_kept_as_caches_are` · `sandbox::golden::the_policy_is_pinned` (rule)

Not kept:
- **The temp folders are shared** (`/tmp`, `$TMPDIR`; [D16](design/sandbox-keeper.md#decisions)). Any run, any
  session, and your own programs write there. Don't keep anything there a run mustn't change. Unix sockets there
  are closed (§1.9).
- **Source files in the project.** A run can change any file you'll later run yourself (`src/main.rs`, a test).
  That's the job. tarae records what each run changed with its turn (`snapshot.rs`) and you see it in the review,
  but nothing stops you running it unread.
- **Linux, Landlock ABI 2**: truncating isn't controlled — a run could empty any file of yours. tarae asks before
  every run at ABI 2 and says why. Below ABI 2: no sandbox, every command asks (§1.12).

### 1.2 What runs later — the protected names

A file that runs or configures a build *without you reading it* is a way out of the sandbox: the next build you
run, outside it, runs it. So these names are never made, changed, moved, removed or hard-linked by a run, anywhere
in the project, in any case: `.git`, `.tarae.toml`, `.tarae.local.toml`, `.envrc`, `.github`, `.claude`, `.vscode`,
and every language pack's `protect` list (`build.rs`, `Cargo.toml`, `.cargo`, `Makefile`, `package.json`,
`pom.xml`, `gradlew`, …), plus `llm.sandbox-protect`. Below a tool's output folder (`target/`,
`node_modules/`) they're output and written as usual.

Kept:
- The names, by name, wherever they are, whatever the project's languages.
  test: `sandbox::tests::what_a_run_keeps_off_by_name` · `sandbox::tests::the_protected_names_are_the_packs_and_the_cores` ·
  `sandbox::tests::a_packs_names_are_kept_in_a_project_of_another_language` ·
  `sandbox::linux::tests::protected_names_are_found_where_they_are` ·
  `sandbox::linux::tests::a_project_inside_a_writable_place_keeps_its_protected_names`
- Folders that hold what runs later outside the project — toolchain homes, folders on `PATH`, shared caches — are
  never written and never offered on a grant card.
  test: `sandbox::tests::what_runs_later_is_never_an_asked_folder` · `allow::tests::which_folders_a_denied_write_may_open`
- Claude changes a protected file only through an edit you review, with a warning — even with review at the end.
  test: `edits::tests::review_at_the_end_still_asks_for_a_protected_file_with_its_warning` ·
  `refactor::tests::a_protected_file_in_a_change_set_is_reviewed_with_its_warning`

Not kept:
- **A protected name made where none is yet, or moved in inside a folder.** Linux: Landlock only grants; a folder
  holding a protected name is granted entry by entry, but a folder with none can take a new `Makefile`. macOS:
  Seatbelt checks a rename's two paths, not what's in a folder — a run makes `x/` in temp holding `build.rs`, then
  moves it into the project as `crates/x` (G23). Neither is refused; both are **seen after the run and asked about
  before anything runs them**. tarae compares the checkout as the run found it and left it: new files in its tree,
  new ignored files and the names inside a new ignored folder (names only — none read), past 5,000 new files at once
  their names (the tree keeps none of them), links — one named like a protected name, or one leading to a folder
  anywhere in the project (by its own name; the folder isn't walked) — and new repositories (by their `.git`).
  In or below a tool's output folder nothing is looked at, as nothing there is kept — a first build's new
  `target/` isn't walked or named. What it finds is said in Claude's
  report, and before what runs it next in that checkout — Claude's next command or test, your task (your build),
  your preflight — one card asks: `y` keeps them and runs it, `r` removes them first (only those entries, through
  no link: a link goes itself, never what it leads to; a folder isn't removed), `v` shows what each holds, `n`
  doesn't run it. One you kept isn't asked about again until a run makes it holding something else (path and
  content); one past what tarae reads to tell (a file over 1 MiB, a folder of over 10,000 names — a sparse
  100 GB file costs a run no disk) or one it can't read (a FIFO, a file of mode 000) is never kept: it's asked
  about each time.
  test (the catch, not the block): `snapshot::tests::protected_names_a_run_made_are_found` ·
  `snapshot::tests::past_new_max_the_new_names_are_still_looked_at` ·
  `snapshot::tests::the_names_in_a_new_ignored_folder_are_looked_at` ·
  `snapshot::tests::a_new_output_folder_is_neither_walked_nor_named` ·
  `snapshot::tests::a_made_name_past_the_cap_isnt_read_and_isnt_kept` ·
  `snapshot::tests::a_made_name_that_cant_be_read_isnt_kept` ·
  `snapshot::tests::r_leaves_a_folder_and_a_file_changed_since_it_was_asked` ·
  `snapshot::tests::what_r_couldnt_remove_is_asked_again_before_the_run` ·
  `snapshot::tests::links_and_repositories_a_run_made_are_looked_at` ·
  `snapshot::tests::a_protected_name_a_run_made_is_asked_about_before_the_next_run` ·
  `snapshot::tests::your_task_and_preflight_wait_on_what_claudes_run_made` ·
  `snapshot::tests::the_cards_answers_view_remove_and_keep_by_content`

  What the look doesn't see: names new inside an ignored folder that was there before the run (git lists an
  ignored folder whole, so it's the same entry before and after); what a process left running makes after the
  run's look (the next run finds it there already — §1.10); a project that isn't a git checkout (no tree, nothing
  compared — the log says why). And what doesn't wait on the card: your test runs, `:restart`'s build, your
  terminal, and rust-analyzer (G13) run what's there unasked.

  Other tools, read from their sources (2026-10-15): none of sandbox-runtime, Codex or Gemini CLI handles a folder
  moved in under a plain name. Codex's Linux sandbox watches a missing *top-level* protected name with inotify
  (`IN_CREATE | IN_MOVED_TO`), deletes what appears and fails the run — top-level names only. sandbox-runtime and
  Codex deny unlink and create on a protected path's ancestors, as the claude profile does for `~/.claude` (§2).
- **Names no pack knows.** A tool that runs a file by another name (`Taskfile.yml`, `noxfile.py`,
  `.pre-commit-config.yaml`) is protected only once a pack lists it or you add it to `llm.sandbox-protect`.
- **What you accept in a review.** A `build.rs` you accept runs at your next build — and rust-analyzer runs it as
  soon as it loads the project (§3).

### 1.3 Reads in your home folder

Kept:
- In home a run reads only the project, the folders it may write, the repository's other checkouts, places you
  granted, and what the toolchains read to work (`~/.cargo` minus credentials, `~/.rustup`, `~/.gitconfig`, …). The
  list was measured; it's pack data.
  test: `sandbox::golden::the_policy_is_pinned` · `sandbox::golden::landlocks_rules_are_pinned` (rule) ·
  `sandbox::linux::tests::a_run_keeps_to_its_policy` (shown, Linux)
- tarae's own state folder (chats, trust answers, grants) is never read.
  test: `sandbox::golden::the_policy_is_pinned` (rule: `$STATE` in the denied list)

Not kept:
- **Outside home a run reads anything your user can** — `/etc`, `/opt`, `/Volumes`, other users' world-readable
  files. Programs need `/usr`, `/opt`, `/Applications`; tarae doesn't list them. Keep secrets in home.
- **Another checkout of the same repository is readable unasked** — sessions of one repository read each other's
  code as a matter of course. Its *writes* are kept.
- **A file's metadata** (that it exists, its size and time) stays readable everywhere: every program looks.

### 1.4 Keys and tokens

Kept:
- Files named like keys (`.env`, `*.pem`, `*.key`, `id_rsa*`, `.netrc`, `.npmrc`, …) aren't read, in the project
  too, unless you open one from the run's panel. Key folders in home (`~/.ssh`, `~/.aws`, `~/.gnupg`, `~/.kube`,
  `~/.docker`, `~/.config/gh`, `~/.claude`, `~/.claude.json`, `~/Library/Keychains`, …) are denied even inside a
  grant.
  test: `sandbox::tests::a_run_cant_read_a_key_named_file` · `sandbox::tests::files_named_like_keys_are_closed_but_the_exceptions` ·
  `sandbox::tests::a_key_inside_a_protected_folder_is_closed` · `sandbox::linux::tests::keys_are_found_where_they_are`
- Linux: no way around it through a link, a rename, another mount or another process's root.
  test: `sandbox::tests::a_hard_link_to_a_key_made_earlier_is_closed_too` ·
  `sandbox::tests::a_key_reached_through_another_mount_is_closed_there` ·
  `sandbox::tests::a_run_reaches_no_key_through_another_process_root` ·
  `sandbox::tests::a_key_swapped_after_the_walk_stops_its_run` · `sandbox::tests::a_run_holds_no_descriptor_but_its_stdio`
- Settings a tool reads that hold a token (git's `extraHeader`, Go's env, pip's index URL, `~/.npmrc`) are read
  from a copy without the token lines.
  test: `sandbox::tests::settings_holding_a_token_are_read_from_copies` ·
  `sandbox::tests::tokens_stay_out_of_runs_under_the_real_profile` (macOS) ·
  `tokens::tests::copies_leave_out_the_tokens_and_hide_their_files`
- The environment starts from a floor (`PATH`, `HOME`, the locale, the toolchains' own); token-like variables only
  by their whole name in `llm.sandbox-env`.
  test: `sandbox::runenv::tests::the_runs_environment_is_the_floor_the_packs_and_yours` ·
  `sandbox::runenv::tests::credentials_in_passed_families_and_options_stay_out` ·
  `sandbox::tests::a_fake_token_never_reaches_a_sandboxed_run`
- macOS: the keychain and the clipboard aren't reached (`com.apple.SecurityServer`, `securityd`, the pasteboard).
  test: `sandbox::golden::the_policy_is_pinned` (rule only — no test runs a keychain lookup under the profile)

Not kept:
- **macOS: your other processes' starting environments and command lines.** A run can read them for any process
  of yours: tarae's, your shell's, another session's runs. No Seatbelt rule closes it (measured on macOS 27). A
  token you export in your shell is within reach. Claude is told once a conversation, with the names left out, to
  ask you rather than read them. Keep tokens in files tarae never reads, or the keychain.
  (Linux: environments never — test: `landlock::tests::a_confined_run_reads_no_environment_outside_it`; command
  lines of the processes it sees, §1.9.)
- **A token inside a file that isn't named like a key** and isn't a known tool's settings: `config/prod.yml`
  with a password in it is an ordinary project file.
- **cargo's `credentials.toml`** isn't read, but cargo takes no copy without it: the report says so in cargo's
  pack's words.
  test: `tokens::tests::cargos_tokens_are_told_once`

### 1.5 The network

Closed by default. Three settings open it, all in your config (a project's `.tarae.toml` only once trusted):
`llm.sandbox-ports` (ports on this machine), `llm.sandbox-localhost` (every port on this machine),
`llm.sandbox-network` (beyond this machine, through tarae's proxy).

Kept:
- **Closed**: nothing beyond this machine, no port on it.
  test: `sandbox::tests::local_ports_open_only_where_asked` (macOS) · `sandbox::route::tests::the_route_table` (rule) ·
  `sandbox::keeper::tests::a_routed_runs_own_connect_is_refused_by_the_kernel` ·
  `sandbox::keeper::tests::the_users_loopback_isnt_reached` · `sandbox::keeper::tests::in_plain_mode_a_run_reaches_only_its_own_loopback`
- **Ports**: only those you open; on Linux this machine's other addresses count as this machine.
  test: `sandbox::keeper::tests::an_opened_port_reaches_this_machine` ·
  `sandbox::keeper::tests::this_machines_own_address_is_reached_only_where_opened` ·
  `sandbox::route::tests::this_machines_own_addresses_are_this_machine`
- **Beyond, only through tarae's proxy.** On macOS the run gets `HTTPS_PROXY` and may connect only to its own
  proxy port, with a token of its own. On Linux, where the session has its own network (TPROXY), there's no
  variable: every connection and every name lookup is taken by tarae; elsewhere the variable, and a tool that
  ignores it can't go out.
  test: `sandbox::proxy::macos_tests::a_run_goes_out_only_through_its_proxy` ·
  `sandbox::proxy::macos_tests::a_run_reaches_only_its_own_proxy_port` ·
  `sandbox::proxy::macos_tests::a_runs_token_is_its_own_and_never_passed_through` ·
  `sandbox::route::tests::the_proxy_is_the_only_way_out` · `sandbox::keeper::tests::a_direct_connect_is_refused_with_the_proxy` ·
  `sandbox::keeper::tests::a_runs_connection_beyond_reaches_its_host_through_the_planted_socket` ·
  `sandbox::keeper::tests::a_run_has_no_proxy_variable_where_its_keeper_has_the_network`
- **Hosts by name.** The proxy reads the host from `CONNECT` and the TLS server name (they must match), resolves
  it itself, checks every address, and connects to the address it checked. With `llm.sandbox-hosts` a list, an
  unknown host waits on a card.
  test: `sandbox::proxy::tests::sni_must_match_the_connect_host` · `sandbox::proxy::tests::plain_http_is_checked_by_host` ·
  `sandbox::proxy::parse::tests::a_name_is_never_an_address_in_another_form` ·
  `sandbox::proxy::tests::an_unknown_host_is_held_and_asked` · `sandbox::proxy::macos_tests::an_unknown_host_waits_for_the_answer` ·
  `sandbox::proxy::tests::a_pipelined_request_never_reaches_the_checked_address`
- **Never reached**: link-local (cloud metadata), multicast, broadcast, in any carried form; metadata names by name.
  test: `sandbox::proxy::tests::the_never_reached_are_refused_by_the_proxy` ·
  `sandbox::keeper::tests::the_never_reached_are_refused` · `sandbox::proxy::tests::upstream::metadata_names_are_refused_by_name`
- **Risky ports** (Docker's API, Jupyter, DevTools, Node's inspector, Redis, etcd, kube, kubelet) aren't reached
  with the network open unless listed.
  test: `sandbox::tests::the_docs_name_every_risky_port` (the list) · `sandbox::golden::the_policy_is_pinned` (rule)
- **Your upstream proxy** (`llm.sandbox-upstream = "env"`): its host and credentials never reach a run.
  test: `sandbox::tests::runs_never_see_the_upstream` · `sandbox::proxy::tests::upstream::upstream_credentials_go_only_to_the_upstream`
- Each session's host rules are its own.
  test: `sandbox::proxy::tests::two_sessions_rules_stay_apart`

Not kept:
- **With the network on and `llm.sandbox-hosts = "any"` (the default), a run can send anything it read to any
  host.** The read rules above are then the only thing between your project and the internet. A host list makes
  every other host a card.
- **`llm.sandbox-localhost` opens the risky ports too.** Neither OS can say "every port but these" once it's on.
  On macOS it also opens other runs' proxy ports, which then only their tokens guard.
- **macOS: this machine's LAN and bridge addresses** are reached as the network is (Seatbelt names ports by
  `localhost` only), so the risky ports there are reached with the network on. An open item in the design doc.
- **Behind an upstream proxy**, the addresses a name leads to are the upstream's to look up: tarae checks the host
  and server name, not the address.
- **DNS as a channel**: where the session answers names (TPROXY), a run's questions are tarae's; with the
  variable, a tool that does its own lookups gets no resolver
  (test: `sandbox::keeper::tests::under_the_proxy_a_resolver_isnt_reached`). Names asked through the proxy still
  leave as lookups tarae makes — a name can carry data.

### 1.6 Other tarae sessions

What a run is kept from and shares with other sessions' commands, per tier, at Landlock ABI 6, and on macOS. This
table is the code's — a test fails when they differ.

<!-- tiers: from src/snapshots/sandbox_guarantees.txt, ABI 6 and Seatbelt — the_docs_tier_table_is_the_codes -->
| Tier, and what differs | Network | Kept apart from other sessions' commands | Shared with them |
|---|---|---|---|
| A+, TPROXY | TPROXY | files (but the temp folders), processes, signals, System V IPC, /dev/shm, terminals, loopback (its own) | nothing |
| A+, tarae's proxy | tarae's proxy | files (but the temp folders), processes, signals, System V IPC, /dev/shm, terminals, loopback (its own) | nothing |
| A+ by copies: the session's user namespace, no network one | tarae's proxy, by copies | files (but the temp folders), processes, signals, System V IPC, /dev/shm, terminals, loopback (this machine's, other sessions' sockets refused) | nothing |
| A+, the project in /dev/shm | TPROXY | files (but the temp folders), processes, signals, System V IPC, terminals, loopback (its own) | /dev/shm |
| A+, its lo without an IPv6 address | TPROXY | files (but the temp folders), processes, signals, System V IPC, /dev/shm, terminals, loopback (its own, without an IPv6 address) | nothing |
| A+, the network closed | closed | files (but the temp folders), processes, signals, System V IPC, /dev/shm, terminals, loopback (its own) | nothing |
| A, TPROXY | TPROXY | files (but the temp folders), signals, System V IPC, /dev/shm, terminals, loopback (its own) | processes (command lines, priority) |
| A, tarae's proxy | tarae's proxy | files (but the temp folders), signals, System V IPC, /dev/shm, terminals, loopback (its own) | processes (command lines, priority) |
| A by copies | tarae's proxy, by copies | files (but the temp folders), signals, System V IPC, /dev/shm, terminals, loopback (this machine's, other sessions' sockets refused) | processes (command lines, priority) |
| B+ | no socket | files (but the temp folders), processes, signals, System V IPC, /dev/shm, terminals, loopback (its own) | nothing |
| B | no socket | files (but the temp folders), signals, System V IPC, /dev/shm, terminals, loopback (its own) | processes (command lines, priority) |
| C, tarae's proxy | tarae's proxy | files (but the temp folders), signals, terminals (only those it makes), loopback (this machine's, other sessions' sockets refused) | processes (command lines, priority), System V IPC, /dev/shm |
| C by copies | tarae's proxy, by copies | files (but the temp folders), signals, terminals (only those it makes), loopback (this machine's, other sessions' sockets refused) | processes (command lines, priority), System V IPC, /dev/shm |
| C, Ubuntu's restriction, no profile | tarae's proxy | files (but the temp folders), signals, terminals (only those it makes), loopback (this machine's, other sessions' sockets refused) | processes (command lines, priority), System V IPC, /dev/shm |
| D | no socket | files (but the temp folders), signals, loopback (no socket) | processes (command lines, priority), System V IPC, /dev/shm, terminals |
| D, Ubuntu's restriction, no profile | no socket | files (but the temp folders), signals, loopback (no socket) | processes (command lines, priority), System V IPC, /dev/shm, terminals |
| Seatbelt, no port open, the network closed | closed | files (but the temp folders), signals, loopback (none of its ports open) | processes (command lines, starting environments, priority), System V shared memory, POSIX shared memory |
| Seatbelt, ports open, the network closed | closed | files (but the temp folders), signals | processes (command lines, starting environments, priority), System V shared memory, POSIX shared memory, loopback (the ports it opens, whoever's) |
| Seatbelt, no port open, the network open | tarae's proxy | files (but the temp folders), signals, loopback (none of its ports open) | processes (command lines, starting environments, priority), System V shared memory, POSIX shared memory |
| Seatbelt, ports and the network open | tarae's proxy | files (but the temp folders), signals | processes (command lines, starting environments, priority), System V shared memory, POSIX shared memory, loopback (the ports it opens, whoever's) |
<!-- /tiers -->

Every "kept apart" cell is shown by these (each runs two sessions with stand-ins and tries each channel):
- test: `sandbox::keeper::tests::two_sessions_cant_reach_each_others_processes` ·
  `sandbox::keeper::tests::two_sessions_cant_reach_each_others_sockets` ·
  `sandbox::keeper::tests::two_sessions_cant_write_each_others_files` ·
  `sandbox::keeper::tests::a_run_opens_only_the_ptys_it_makes` ·
  `sandbox::keeper::tests::a_bind_onto_another_sessions_port_is_refused` ·
  `sandbox::route::tests::a_shared_loopbacks_socket_of_another_session_is_refused` (Linux, in `cargo xtask linux`)
- test: `sandbox::apart::two_sessions_cant_reach_each_others_processes` ·
  `sandbox::apart::two_sessions_cant_reach_each_others_sockets` · `sandbox::apart::two_sessions_cant_write_each_others_files` (macOS)
- The words are one value per run: test: `sandbox::golden::the_guarantees_say_each_tier`

Below Landlock ABI 6 a run can signal its own session's other commands; at ABI 2 files are shared too.

Not kept — said, not closed: what each tier's "Shared" column lists. In C and D, and always on macOS, shared memory
(SysV, POSIX) is reachable: a run can read and write what another session's commands — or your other programs —
keep there. On macOS a loopback port it may reach is reached whoever listens there: Seatbelt can't tell whose a
listener is.

### 1.7 Git

Kept:
- `.git` is a protected name: a run can't write hooks, config, refs or the index. Runs get
  `GIT_OPTIONAL_LOCKS=0` so a read-only `git status` works.
  test: `sandbox::tests::what_a_run_keeps_off_by_name` · `sandbox::tests::git_status_leaves_the_index_in_a_sandboxed_run` ·
  `editor::tests::a_claude_run_cant_leave_git_without_its_refs`
- Claude's `git` tool runs reads only, from an option allowlist, key files kept out on git's side.
  test: `gittool::tests::only_reads_pass_and_only_the_options_listed`
- Git that tarae runs itself never runs a repository's own commands (hooks, `core.fsmonitor`, filters,
  `diff.external`, `core.sshCommand`, credential helpers, …) until you trust them on a card — one test per key.
  test: `gitrepo::held::tests::fsmonitor_is_held_on_status_and_snapshots` ·
  `gitrepo::held::tests::a_hooks_path_is_held_on_the_git_tabs_commit` · `gitrepo::held::tests::a_credential_helper_is_held` ·
  `gitrepo::held::tests::a_submodules_own_commands_are_held`
- Your own git-tab actions ask before running a hook in the checkout that changed since the base.
  test: `worktree::tests::the_users_git_asks_before_hooks_a_claude_could_have_written` ·
  `worktree::tests::hooks_in_the_checkout_never_run_in_taraes_rebase`

Not kept:
- **Git you run yourself** (your terminal, your tasks) reads the repository's config as git always does.
- **Git the claude process runs** (§2) reads it too.
- **A hooks folder outside the checkout** (`core.hooksPath` to a folder elsewhere) is trusted as it was when you
  answered; tarae checks its digest, not each run.

### 1.8 Tiers on Linux, in short

| Tier | When | What it means |
|---|---|---|
| A+ | unprivileged user namespaces work, `/proc` can be mounted | the session's own processes, loopback, IPC, `/dev/shm`, terminals |
| A | the same, `/proc` masked (Docker's default) | as A+, but processes are shared (command lines, priority) |
| B+, B | as A+, A, but tarae can't serve a run's sockets (Yama 2/3, inside another seccomp listener) | no socket at all |
| C | no user namespaces (Ubuntu's restriction without tarae's profile; a container) | files and signals kept; processes, SysV IPC, `/dev/shm` shared |
| D | as C, and no sockets | as C, no network |

The tier is said to Claude once a conversation (test: `sandbox::golden::the_report_says_the_tier_once`), to the
run as `TARAE_SANDBOX_STATUS` (test: `sandbox::golden::the_status_is_the_tier_and_the_abi`), and to you (the
sandbox panel, `:procs`, `tarae state`).

### 1.9 Your processes, and tarae itself

Kept:
- **Signals**: only to the run's own processes. macOS always; Linux from Landlock ABI 6.
  test: `sandbox::tests::a_run_signals_and_looks_into_only_its_own_processes` (macOS) ·
  `landlock::tests::a_confined_run_signals_nothing_outside_it` (Linux)
- **Looking into other processes** (macOS `lsof`-style details; Linux `/proc/<pid>/environ` and other details
  outside the session).
  test: `sandbox::tests::a_run_signals_and_looks_into_only_its_own_processes` ·
  `sandbox::keeper::tests::a_run_reads_no_details_outside_its_session` · `sandbox::keeper::tests::a_run_sees_only_its_sessions_processes` (A+)
- **Other terminals**: a run can't open your tty or another pty.
  test: `sandbox::tests::a_run_cant_open_another_terminal` (macOS) · `sandbox::keeper::tests::a_run_opens_only_the_ptys_it_makes`
- **The keeper** (Linux): a run can't signal or trace it; it isn't dumpable; a run holds no capability and no
  descriptor but its stdio; makes no user namespace.
  test: `sandbox::keeper::tests::a_command_cant_signal_the_keeper` · `sandbox::keeper::tests::the_keeper_is_not_dumpable` ·
  `sandbox::keeper::tests::a_command_holds_no_capability` · `sandbox::keeper::tests::a_command_holds_only_its_stdio` ·
  `sandbox::keeper::tests::a_run_makes_no_user_namespace_under_the_profile`
- **tarae's UI socket** (which could drive the editor): refused to Claude's processes, and out of a run's reach.
  test: `sandbox::tests::claudes_runs_cant_reach_the_ui_state_socket` ·
  `uistate::tests::the_socket_answers_snapshots_and_refuses_claudes_processes`
- **tarae's state** (trust answers, grants, chats) isn't read or written (§1.3).
- **Unix sockets** only in the project and the run's own folder — not tmux's, zellij's, ssh-agent's.
  test: `sandbox::tests::unix_sockets_only_in_the_run_and_the_project` · `sandbox::tests::the_multiplexers_socket_folders`
- **Control messages** (Linux, a `sendmsg`/`sendmmsg` tarae makes for a run): only descriptors go (`SCM_RIGHTS`,
  at most `SCM_MAX_FD` 253 a message); every other type — credentials included — is refused (`EPERM`) and named in
  the run's report; what reaches `sendmsg` is a buffer tarae built, nothing of the run's bytes. A run's own
  `setsockopt` runs with its own rights.
  test: `sandbox::supervisor::wire::tests::a_control_message_but_descriptors_is_refused_by_its_type` ·
  `sandbox::supervisor::wire::tests::past_scm_max_fd_descriptors_none_more_is_taken` ·
  `sandbox::supervisor::wire::tests::only_a_buffer_tarae_built_goes_out` ·
  `sandbox::supervisor::tests::a_runs_message_passes_descriptors_and_no_other_control`
- **No Apple Events, no LaunchServices** (macOS: a run can't ask another app to act).
  test: `sandbox::golden::the_policy_is_pinned` (rule only)

Not kept:
- **Command lines and priority**: visible and lowerable (renice) where processes are shared — macOS, and Linux
  tiers A, B, C, D. macOS: starting environments too (§1.4).
- **tarae parses what a run sends** — the supervisor reads a run's call arguments from its memory, the proxy
  parses its requests — inside tarae's own process. A bug there is a bug in tarae: a panic ends the editor
  (`panic = "abort"`), a memory-safety bug would own it. Rust, with seeded parser loops
  (test: `sandbox::channel::tests::the_channel_parser_never_panics`, `sandbox::proxy::parse::tests::the_connect_parser_never_panics`,
  `sandbox::proxy::parse::tests::the_sni_parser_never_panics`); the supervisor's memory readers aren't fuzzed yet
  ([roadmap](roadmap.md): fuzzing). Moving them into a confined process of their own is
  [D21](design/sandbox-keeper.md#decisions), later. **A reviewer's first target.**
- **A denial's words are partly the run's** (macOS, V2-2): the log's line `Sandbox: <name>(<pid>) deny(1) <op>
  <target>` holds two things the run picks, its program's name and the target's path, and nothing marks where
  either ends. A program named `a(1) deny(1) file-read-data /x ` makes its own denial read with that operation and
  target, so the run's report and `space m s`'s choices name those. No parsing can tell the two apart; it gains a
  run nothing an attempt at the access wouldn't, since the denial it fakes is one it could cause (user decision
  2026-10-16: written down, not fixed). What's read is always in the line's message, never from elsewhere.
  test: `sandbox::loops::a_denial_read_from_the_log_is_in_its_message`
- **The keeper's birth** (`clone3` with raw system calls until exec, id maps written from outside) is the other
  part of Linux's sandbox not covered by Landlock or seccomp. A reviewer's second target.

### 1.10 What a run leaves running

Kept:
- Linux: whatever a run leaves (a daemon) is the keeper's child; the session ends it, and in A+ the PID namespace
  ends it in one kill. It stays confined: everything a run starts inherits its Landlock and seccomp.
  test: `sandbox::keeper::tests::leftovers_are_the_keepers_children` · `sandbox::keeper::tests::the_keeper_ends_with_its_channel` ·
  `sandbox::keeper::tests::keepers_end_together_not_one_after_another`
- It stays listed under its session in `:procs`.
  test: `procs::tests::a_claude_run_left_running_stays_under_its_session`

Not kept:
- **macOS**: there's no subreaper. A process a run leaves goes to `launchd`; tarae knows it by the tag in its
  environment, and one started with `env -i` drops the tag — it's then neither listed nor ended. It stays inside
  the sandbox (Seatbelt is inherited), so it can do no more than the run could, for as long as it lives.

### 1.11 Grants you give, and what each gives away

| You answer | It gives |
|---|---|
| `y`/`a`/`A` on a blocked write (`space m s`) | that folder writable for runs (never home, above it, keys, folders of programs, shared caches, toolchain homes) |
| a port or `llm.sandbox-localhost` | those ports (the risky ones too, with localhost) |
| `llm.sandbox-network` | the internet through the proxy; every host unless `llm.sandbox-hosts` lists them |
| `y` on a host card | that host for the run, session, or always |
| `y` on a run-outside card | one command as you, unconfined, in full — the card shows what changed since the last `y` (`sinceyes.rs`) |
| `y`/`a` on a no-sandbox card | commands as you, unconfined |
| `llm.sandbox = false` | every command of Claude's as you, unconfined, **unasked** — only the chat's title says `› no sandbox` |
| `llm.claude-sandbox = false` | the claude program itself as you, unconfined — on macOS the chat says so; on Linux it's the default (§2, G25); only your own config sets it |

test: `allow::tests::which_folders_a_denied_write_may_open` · `allow::tests::home_above_it_and_key_folders_are_never_kept` ·
`editor::tests::claude_asks_to_run_outside_the_sandbox_with_a_reason` · `tools::tests::claude_asks_for_one_socket_not_the_sandbox_off`

Each of these settings is yours: a project's `.tarae.toml` sets one only once you trust it.
test: `trust::tests::every_sandbox_setting_waits_for_trust`

### 1.12 When there's no sandbox

No Seatbelt (inside another sandbox), Landlock missing or ABI 1: every command of Claude's asks first, saying why;
`y` this one, `a` this conversation.
test: `editor::tests::without_a_sandbox_claudes_commands_ask` · `sandbox::tests::a_run_outside_the_sandbox_says_it_had_no_user_namespace_where_refused`

The exception is yours to make: **`llm.sandbox = false` turns the sandbox off without a card** — every command of
Claude's then runs as you, unconfined, and only the chat's title (`› no sandbox`) and Claude's own report say so.
A project can set it only once you trust it (§1.11).
test: `editor::tests::the_sandbox_turned_off_is_said` · `trust::tests::every_sandbox_setting_waits_for_trust`

---

## 2. The claude process

`claude -p` runs as you. Its cwd is the session's checkout. What it may *do* through its tools is narrowed by its
flags and by tarae answering its permission requests; what its own code does, on macOS by a sandbox of its own
(`llm.claude-sandbox`, on unless you turn it off: a Seatbelt profile), files only — its network, the programs it
starts and the system's services as they are. **On Linux it's not confined by default** (user decision
2026-10-16): it runs as you, with your own `~/.claude` and login, as Claude Code's and Codex's own sandboxes leave
their agent — they confine the commands, and so does tarae: Claude's runs stay in the sandbox (§1) and its edits
reviewed. Why: Landlock only grants, and claude refreshes its login in `~/.claude/.credentials.json` — a write
there needs all of `~/.claude` open (your settings, `CLAUDE.md`, hooks), so a confined claude needs a folder and a
login of its own, a second login on a machine that's logged in; copying the login is ruled out (refresh tokens
rotate, and two copies fight). What that leaves: **a hostile or buggy claude program can write what you can** —
your shell's startup files, `~/.local/bin`, your Claude Code's settings and hooks, the project's protected names —
and read your keys (G25). Opt-in, `llm.claude-sandbox = true` (your own config only): Landlock rules from the same
pieces as macOS's, its folder tarae's own, apart from your `~/.claude` (below).

### What it touches

Measured on macOS (claude 2.1.296, macOS 27, 2026-10-14): one short chat with tarae's flags — a Read in the
project, an edit, a Read outside it after a `y` — under a Seatbelt profile that refuses nothing and reports
everything. macOS throttles repeated reports, so a path missing below isn't proof it wasn't touched. On Linux
(claude 2.1.296 under `strace`, in a container, against a stand-in for Anthropic's API and with no login) it touched
the same kinds of places; there its login is a file in its folder, not the keychain.

| | What it touched |
|---|---|
| **Programs it started** | `git`, seven times, in the checkout (the status it tells the model) · `/usr/bin/security`, twice (its login, in the keychain) · `sh` and `bash` running `uname`, `ps`, `ioreg` (facts about the machine) |
| **Writes in home** | `~/.claude.json` (with its lock, temp copies and `~/.claude/backups/`) · `~/.claude/projects/<the checkout>/` (the conversation, `memory/`) · `~/.claude/sessions/` · `~/.claude/plugins/` (locks, in-use marks of your plugins, synced lists) |
| **Writes elsewhere** | `/tmp/claude-<uid>/` (a file-watch probe) · a unix socket it binds in `/tmp/cc-socks/` · project files only through Edit or Write, after tarae's answer |
| **Reads in home** | the list of `~` and `~/.config` · `~/.claude/` (settings, plugins, skills, caches, state) · `~/.claude.json` · `~/.gitconfig`, `~/.config/git/ignore` (git) · `~/.local/bin`, `~/.local/share/claude/versions/`, `~/.local/state/claude/locks/` (its own program) · `~/Library/Keychains/login.keychain-db` (`security`) |
| **Reads elsewhere** | the checkout and its `.git` · what Read was let read outside it · `/Library/Keychains/System.keychain` · `/etc/ssl/cert.pem` · time zones · the Xcode tools' git |
| **Network** | port 443 (Anthropic) · the system's name lookups · its own unix socket |
| **System services** | the keychain (`com.apple.SecurityServer`, `com.apple.securityd.xpc`) · `trustd` (certificates) · FSEvents · preferences, logging, notifications, directory services, `analyticsd` |

It read nothing in the key folders of §1.4 but the keychain, and nothing outside the project that Read hadn't
asked about. A `CLAUDE.md` `@import` of a file outside the project wasn't read: its words didn't reach the model
(one run, this version).

Kept:
- **macOS: its own sandbox, files only.** It reads none of the places that hold keys (§1.4's list, but
  `~/Library/Keychains` — its login), no file named like a key in home or the project (but its own), nothing of
  tarae's state but the log. It writes only its own (`~/.claude/`, `~/.claude.json` and the lock, temp copy and
  backups beside it, `~/.local/state/claude/`), the temp folders and the devices — and in `~/.claude/` not what your
  own Claude Code runs or obeys later: `settings.json`, `settings.local.json`, `CLAUDE.md`, `commands/`, `agents/`,
  `skills/`, `hooks/`, `output-styles/`, `local/` (the local installer's claude program), `ide/` (the locks a
  later claude connects by), `shell-snapshots/`, `plugins/` (but the in-use marks, locks, synced marketplace list
  and its round's time claude writes there — measured, by name), and in `projects/` every project's conversations
  but this one's (a resumed conversation is obeyed as history) and this one's auto-memory (`memory/`). Nor
  `~/.claude`, `projects/` or this project's folder themselves: none is moved aside, removed or made again (a
  folder renamed whole carries what's in it past the denies on its children — settings, a memory, another
  project's conversations filled in elsewhere); tarae makes this project's folder before claude starts, through no
  link. Its own cleanup would delete other projects' old conversations and old shell snapshots at each start (measured
  with 2.1.296): the chat's `--settings` sets `cleanupPeriodDays` to 36500 where claude loads your user settings, so
  it tries none (test: `chat::tests::the_chat_keeps_claude_from_cleaning_up_transcripts`); one refused all the same is
  logged once a process, never said in the chat (`sandbox::claude::tests::claudes_own_cleanup_is_told_apart`). Your
  own Claude Code, unconfined, cleans them as your settings say. With the default edit tools not
  the project (tarae's `edit` writes it), with Edit and Write (`llm.edit-tools` "both"/"native") the project but
  `.git` and the protected names (§1.2) — and a project that holds home (home itself or above) is writable only
  outside home: home's deny stays over it. So a bug in its permission path can't write `~/.config/tarae`, your
  shell's startup files, `~/.local/bin` or a hook, nor read `~/.ssh`. The deny lists are the runs' own. A Read the profile
  would refuse isn't asked on a card: it's answered at once, and you can still send the file yourself (`C-o`).
  What the profile refuses claude is said in the chat and tarae's log — from the kernel's reports, and from
  claude's own tool results (`EPERM …`) where the profile's rules refuse that path: the kernel's report didn't come
  for claude's refused Edit in the live check. The reviewer's claude runs under the same profile, the project
  read-only; its refusals go to the log. Started inside another sandbox (Seatbelt doesn't nest) or with
  `llm.claude-sandbox = false` — a setting only your own config sets — it's unconfined and the chat says so.
  test: `sandbox::claude::tests::claude_cant_read_a_key` · `sandbox::golden::the_claude_profile_is_pinned` ·
  `sandbox::claude::tests::claudes_own_folder_keeps_what_your_claude_runs_later` ·
  `sandbox::claude::tests::a_project_holding_home_opens_no_home` ·
  `chat::tests::a_refusal_claudes_tool_result_says_is_said_in_the_chat` ·
  `reviewer::tests::the_reviewers_claude_runs_under_its_own_profile` ·
  `sandbox::claude::tests::a_read_claudes_profile_refuses_is_said_not_asked` ·
  `sandbox::claude::tests::a_refusal_claudes_tool_says_is_the_profiles_where_its_rules_refuse` ·
  `chat::tests::a_claude_refusal_is_said_once` · `chat::tests::claude_unconfined_by_the_setting_is_said` ·
  `trust::tests::claudes_own_sandbox_is_risky_and_only_yours`
- **Linux, by default: claude unconfined, on your own folder.** No `CLAUDE_CONFIG_DIR` of tarae's (yours passed on as
  it is), `--setting-sources` with `user` as on macOS, no Landlock rules, nothing made in tarae's state; your
  Claude settings are a hold-back's baseline as on macOS; `:claude-login` is your claude's own `claude auth login`,
  and a claude with no login says Claude Code's own words (`/login`). The chat doesn't say it's unconfined in every
  chat — it's the default, said here and in the settings. A conversation started confined (in tarae's folder)
  continues as a new one after the switch, said once; `~/.local/state/tarae/claude` stays, used again once
  confinement is on.
  test: `claudehome::tests::claude_is_confined_by_default_on_macos_only` ·
  `claudehome::tests::on_linux_unconfined_every_claude_has_your_folder_and_settings` ·
  `chat::tests::claude_unconfined_by_the_setting_is_said` · `settings::tests::settings_reference_is_up_to_date`
- **Linux, opt-in (`llm.claude-sandbox = true`): its folder is tarae's own.** Every claude tarae starts (the chat,
  the reviewer, an ask, a commit draft, the prewarm) gets `CLAUDE_CONFIG_DIR` = `~/.local/state/tarae/claude` (`$XDG_STATE_HOME/tarae/claude`). tarae makes it 0700
  through no link: a link planted there is refused and claude doesn't start. Measured: with the variable,
  `.claude.json`, its lock and backups, the conversations and the login all live there; outside it claude writes
  only `~/.local/state/claude/locks/` (also made by tarae first) and temp. It loads no user settings
  (`--setting-sources project,local`): its user settings would be that folder, which it writes itself. What changes
  for you: **one login for tarae** (`:claude-login` runs Claude Code's own `claude auth login` with the variable, in
  the terminal panel; the chat says why once when claude answers "Not logged in": Landlock can't let it write your
  login without all of `~/.claude`). **Its conversations are apart
  from yours**: your `claude --resume` doesn't list tarae's, nor tarae yours, and a chat started before the switch
  continues as a new conversation, said once (a message you'd sent goes to the new one). **Your user settings,
  `CLAUDE.md`, plugins and user-scope MCP servers don't apply** in tarae's claude. Runs never read the folder (it's
  in tarae's state, kept from them), so its login is kept from Claude's commands. Your `~/.claude` and
  `~/.claude.json` are closed to it by its profile (next). macOS is unchanged.
  test: `claudehome::tests::claudes_folder_is_made_through_no_link` ·
  `claudehome::tests::every_confined_claude_on_linux_has_taraes_folder_and_no_user_settings` ·
  `claudehome::tests::claude_login_is_claudes_own_login_in_taraes_folder` ·
  `claudehome::tests::no_login_says_claude_login_once` · `editor::tests::a_conversation_continues_after_a_restart`
- **Linux, opt-in: its own sandbox, Landlock, files only.** The macOS profile's pieces (the runs' kept places, the
  protected names, the files named like keys, the temp folders, the devices) made into Landlock rules as the
  runs' are, on a worker; applied on a thread of tarae's named for it, which then starts claude (`no_new_privs`
  with it; nothing else of tarae's confined). Reads: everything but home's kept places — §1.4's key folders, your
  `~/.claude` and `~/.claude.json`, tarae's state (but claude's folder, which is in it, and the log) — the
  terminals, kept places outside home (a moved state or toolchain home), the files named like keys at home's top
  and anywhere in the project. Home-wide, not the runs' narrow home: rules can't be added to a running process, so
  a Read you allow on a card must already reach the file; one the rules close is answered at once. Writes: its
  folder and `~/.local/state/claude/locks/` whole, the temp folders and the devices but the terminals — claude isn't
  setsid, so `/dev/tty` is tarae's own terminal (a write lands on your screen, `TIOCSTI` types keys into tarae).
  The project: none with the default edit tools; with Edit and Write as runs have it (§1.2), entry by entry around
  the protected names — a folder holding one takes no new entry, the project's top included (`.git`): a new file
  there is refused, and so is the temp file claude's Edit and Write make beside the one they change, after which
  claude 2.1.296 writes a file already there in place (measured, run C: the edit landed, the new file wasn't
  made); the chat says so once, tarae's `edit` makes new files there. Files only (decision 17): no network rights and no unix-socket rules, so
  **claude reaches any unix socket whose path it can find** — your session's D-Bus (where `systemd-run --user`
  runs anything unconfined), tmux's sockets, the SSH and GPG agents under `$XDG_RUNTIME_DIR`. Against a hostile
  claude program the Linux confinement is no wall; it holds against claude's file bugs. The chat's claude has no
  Bash tool, so only claude's own code could go that way (G24). Signals and abstract sockets outside its domain
  closed from ABI 6. Each claude's rules are made on a worker as it starts, from what's there then. Below Landlock ABI 3, or with a claude program in a place the rules keep (the old
  local install in `~/.claude/local`), claude runs unconfined and the chat says why. Its refusals: claude's tool
  results in Landlock's words (`EACCES …`, `EXDEV …` for a move), and from ABI 7 Landlock's audit records of its
  own domain. Measured live (claude 2.1.296 through `tarae --claude-confined`, `target/sb/v1b-linux.sh` run C,
  against a stand-in API; kernel 6.8, Landlock ABI 4, 2026-10-15): the conversation finished as unconfined; a
  planted key's Read came back `EACCES`; a Write over `~/.bashrc` was refused (its temp file and the in-place
  fallback both `EACCES`) and the file stayed as it was; a new file in the project's top wasn't made, an edit of
  one there landed in place; a marker in a `CLAUDE.md` in its folder reached none of the 8 requests (the
  project's, the control, all 8); claude sent no signal.
  test: `sandbox::claude::linux::tests::claudes_grants_read_all_but_whats_kept_and_write_its_own` ·
  `sandbox::claude::linux::tests::claudes_rules_in_rust_are_its_grants` ·
  `sandbox::golden::claudes_linux_grants_are_pinned` ·
  `sandbox::claude::linux::tests::claudes_landlock_rules_reach_nothing_kept` ·
  `sandbox::claude::linux::tests::a_stand_in_claude_does_what_claude_does_and_no_more` ·
  `sandbox::claude::linux::tests::claude_is_started_confined_by_its_tagged_thread` ·
  `sandbox::claude::linux::tests::claudes_watch_hears_its_own_domain` ·
  `sandbox::claude::linux::tests::a_refusal_in_landlocks_words_is_said` ·
  `sandbox::claude::linux::tests::a_claude_program_its_rules_keep_is_told`
- **It doesn't update itself** from tarae: its updater writes a folder of programs on your `PATH`, closed to it;
  every claude tarae starts gets `DISABLE_UPDATES`. `:claude-update` runs `claude update` as your own task, outside
  the sandbox, without `DISABLE_UPDATES` or `DISABLE_AUTOUPDATER` (a tarae started with one passed it on), and
  says what came in the versions folder ("Installation complete" with nothing new: a warning); the chat says once when the claude it runs was installed over two weeks ago (without the network
  tarae can tell its age, not whether a newer one exists).
  test: `llm::tests::every_claude_tarae_starts_has_its_updates_off` ·
  `claudeupdate::tests::an_old_claude_is_said_with_how_to_update_it` ·
  `claudeupdate::tests::claude_update_runs_as_your_own_task`
- **It reads no auto memory**: every claude tarae starts gets `CLAUDE_CODE_DISABLE_AUTO_MEMORY=1`, which outranks
  the setting `autoMemoryEnabled` (a project's settings can't turn it back on), and never `CLAUDE_MEMORY_STORES`
  (team memory). The memory folder is the repository's, shared by every checkout; what's there is another tool's
  notes, and memory a tarae claude wrote would be obeyed later by your own Claude Code.
  test: `llm::tests::every_claude_tarae_starts_has_its_memory_off`
- **Its tools**: Claude Code's `Read` only (plus `Edit`, `Write` with `llm.edit-tools` "both"/"native"), and
  tarae's own tools in-process. No Bash, no WebFetch or WebSearch, no Grep or Glob, no Task.
  test: `chat::tests::each_edit_arm_says_its_own_tools`
- **Only tarae's MCP server** (`--strict-mcp-config`: a project's `.mcp.json` doesn't connect); `Skill` denied;
  hooks off (`disableAllHooks`).
  test: `claudetrust::tests::the_chat_asks_for_only_tarae_s_own_mcp_servers` ·
  `claudetrust::tests::a_project_skill_cant_pre_approve_tools_in_the_chat` ·
  `claudetrust::tests::chat_settings_l6_checks_are_what_tarae_passes`
- **Every Read asks tarae** (Read isn't in `--allowedTools`; claude sends `can_use_tool`). Inside the project it's
  allowed, but a key-named file; outside, a card says what's read; home, above it and key folders only once.
  test: `editor::tests::claudes_reads_outside_the_project_ask_first` · `reads::tests::inside_or_allowed_goes_without_asking` ·
  `chat::tests::a_key_named_file_reaches_claude_only_when_the_user_lets_it` · `reads::tests::a_link_to_a_key_file_is_the_key_file`
- **Every change is reviewed**, or with review at the end allowed unseen and shown after — never for a protected
  name, `.git`, outside the project, or a buffer you changed. Moves and deletes always ask. tarae writes through
  descriptors, never a path re-walked after its check.
  test: `edits::tests::an_edit_to_a_protected_file_warns_on_its_review` · `edittool::tests::a_lone_delete_is_never_written_unseen` ·
  `saver::tests::a_save_claude_caused_never_goes_through_a_link_swapped_in`
- **Every command** Claude asks for goes through tarae's `run` — §1.
- **A repository's Claude settings are held until you trust them**: helper commands, `env`, plugins, `allow` rules,
  `additionalDirectories`. claude doesn't start until the card is answered; a change while it runs stops it.
  test: `claudetrust::tests::the_chats_claude_starts_only_as_the_card_answers` ·
  `claudetrust::tests::what_a_repository_runs_is_held_and_hooks_only_shown` ·
  `claudetrust::tests::settings_made_while_claude_runs_stop_it_and_ask` ·
  `claudetrust::tests::every_command_key_is_held_up_front_but_trusted_or_yours`
- **Its environment**: API keys dropped unless you keep them; a parent Claude Code session's variables dropped.
  test: `llm::tests::what_the_process_itself_holds_never_reaches_claude` ·
  `llm::tests::keys_go_to_claude_only_when_you_keep_them_outside_claude_code`
- **Your SSH agent**: no claude tarae starts gets `SSH_AUTH_SOCK` (its git only reads status, it never needs the
  agent), and on macOS its profile refuses connecting to the socket that variable named when tarae started (by
  its real path: through a link too) and to launchd's agent sockets, `com.apple.launchd.*/Listeners` in
  `/private/var/run/` (macOS 27) or `/private/tmp/` (older) — measured with `ssh-add -l`, which reached each
  outside the profile; live on 27.0.1, your agent refused under it (by the socket the variable named). In
  `/private/var/run/` (root's: no test can put a stand-in there) Seatbelt's own answer to the launchd rule is only
  the live check's, which probes launchd's agent under the profile without the variable's socket; the test reads
  the rule's regex with Rust's `regex`, not Seatbelt's.
  test: `llm::tests::every_claude_tarae_starts_has_no_ssh_agent` ·
  `sandbox::claude::tests::claude_cant_reach_your_ssh_agent` ·
  `sandbox::claude::tests::launchds_agent_sockets_are_refused_in_both_places`
- **Where tarae's AppArmor profile lends user namespaces**, what starts for Claude doesn't get them.
  test: `spawn::tests::only_claudes_and_plugins_processes_are_kept_from_user_namespaces` ·
  `seccomp::tests::what_starts_outside_for_claude_makes_no_user_namespace`

Not kept:
- **claude's own code, within its profile.** A bug in its permission path (a tool that skips `can_use_tool`, a flag
  whose meaning changes in an update), or a feature that reads by itself, reads what the profile leaves open —
  everything but key folders, files named like keys and tarae's state — and writes its own folders and temp.
- **What Landlock can't say** (Linux, said once in the chat): a file named like a key deeper in home than its top
  (macOS's profile refuses them anywhere in home; claude's Read still asks tarae); a name inside a granted folder
  (claude's folder is all or nothing — why its user settings source is off); a unix socket's connect (next); what's
  made after claude started — in home closed to it, a key or protected name in the project open to it — until its
  next start, whose rules are made then; home's own listing. Below ABI 3, or a claude program the rules keep:
  unconfined, said.
- **Unix sockets: no wall against a hostile claude program** (G24). Both profiles are files only (decision 17).
  On Linux claude reaches any unix socket whose path it can find: your session's D-Bus (`systemd-run --user`
  runs anything unconfined), tmux's sockets (a tmux server runs commands as you), the SSH and GPG agents under
  `$XDG_RUNTIME_DIR` (they sign and decrypt for it). Through the first two its own code gets out of the rules
  whole. On macOS the same holds but for the agent sockets its profile knows (below). The chat's claude has no
  Bash tool: this is claude's own code, not the model's reach. Said once in the chat on Linux. Deferred (user
  decision 2026-10-15): at V3, on a 7.2+ kernel, Landlock's `RESOLVE_UNIX` (ABI 9) granted only beneath
  `/tmp/cc-socks/`; below ABI 9 the gap stays, said.
- **Your keychain, through `security`.** claude reads its login with `/usr/bin/security`; anything else that tool
  may read without a prompt is within the same reach.
- **Its own git reads the repository's config.** tarae holds a repository's commands for its own git (§1.7), not
  for the git claude starts. Measured (claude 2.1.296, macOS): a `core.fsmonitor` set in the checkout's
  `.git/config` to a stand-in did **not** run under claude's seven git calls — what claude asks git doesn't reach
  the fsmonitor — while a plain `git status` in the same repository runs it (the control). Another version, or another git command of claude's, could; a clone doesn't bring `.git/config`
  and Claude's runs can't write `.git`, but a checkout that came whole (an archive with its `.git`) can. On macOS
  what such a command does is confined as claude is (it inherits the profile).
- **Your own Claude settings apply** (macOS). `--setting-sources user,…` loads `~/.claude/settings.json`: an
  `allow` rule of yours for Read would let reads go unasked (only a project's are held; not measured), and your
  plugins load (measured: marked in use). On Linux they don't: the user source is off.
- **A settings file in claude's temp folder.** claude 2.1.296 looks up `/tmp/claude-<uid>/claude-settings-<hash>.json`
  as it starts (on Linux it stats and resolves it — `openat O_PATH`, `readlink` — but never reads it). Measured
  (Linux, two runs; macOS, one turn): one planted there — `"permissions": {"allow": ["Edit", "Write"]}` and a model
  of its own — wasn't applied: Edit and Write still asked, and the API was asked for tarae's model. On macOS the
  lookup itself isn't confirmed (Seatbelt doesn't report claude's lookups). What's left: another version could
  read it, and Claude's runs can write that folder (temp is theirs).
- **The `env` window**: a variable a `git pull` adds to `.claude/settings.json` can reach claude's next request
  before tarae's watch stops it (at most about half a second).
- **The network**: claude reaches whatever it reaches. tarae doesn't proxy it: it must reach Anthropic, or your
  gateway or proxy. What it read can leave that way.
- **The ask prompt's and commit drafts' claudes** run unconfined: they're started with no tools at all
  (`--tools ""`) — what they could do is claude's own code alone.
- **A refusal claude doesn't put in a tool result goes unseen.** The kernel reported none of claude's own
  refusals live (only a probe's), so the chat sees a refusal only when a tool's result says it (`EPERM …`); one in
  claude's own work — its state, its caches — isn't said anywhere. The profile still holds. That's why `~/.claude/`
  is closed by a list of what runs later rather than opened by a list of what claude writes: an allowlist would
  refuse a new place a claude update writes, unseen. Planned: the allowlist, once those refusals can be seen.
- **Your user-scope MCP servers.** `~/.claude.json` holds them — commands your own claude starts — and claude must
  write that file (its state, counters, project entries): a bug could add a server there. tarae's chat runs with
  `--strict-mcp-config` and starts none of them; your own claude, outside tarae, would.
- **An SSH agent at another socket.** Without the variable claude has to find a path on its own; on macOS an agent
  whose socket `SSH_AUTH_SOCK` didn't name when tarae started (one started later, a second agent of another
  program) is still reachable — the profile refuses only those it knows. **On Linux claude isn't given
  `SSH_AUTH_SOCK`, but nothing stops it reaching the agent**: its rules refuse no socket, and an agent's it finds
  (`$XDG_RUNTIME_DIR/ssh-agent.socket`, gnome-keyring's, gpg-agent's) is reached (G24).
- **Claude Code's own memory folder** (`~/.claude/projects/<project>/memory/`, shared by every checkout of a
  repository) is inside claude's own folder: no claude tarae starts loads it (auto memory is off — above), but its
  own code could still read it. On macOS the profile keeps claude from writing it (a new project's folder isn't
  even made); on Linux it's in claude's folder, tarae's, written whole — your own Claude Code never reads it.

Planned: on Linux, the scopes (ABI 6) and the audit (ABI 7) checked live on a 6.12+ kernel — colima's, where the
live check runs, is ABI 4.

## 3. What tarae runs as you, outside the sandbox

Not Claude's, but a hostile repository reaches them:
- **Language servers** run the project's code as you: rust-analyzer runs `build.rs` and proc macros when it loads
  the project. **Not tied to trust yet.** Opening a hostile Rust repository in tarae runs its build script, as it
  does in any editor with rust-analyzer.
- **`.tarae.toml`'s risky part** (tasks, server commands, `:` bindings, settings that run something or loosen a
  guard, `[[attach]]` before-commands) waits for a trust card, keyed by real path and hash.
  test: `trust::tests::too_long_to_show_is_never_trusted` · `config::tests::the_users_own_file_is_never_held_back`
- **Preflight's pack defaults** run the project's tests as you after a card that says so.
- **Git** — §1.7.

## 4. Gaps, in one list

| | Gap | Why | Plan |
|---|---|---|---|
| G1 | The `claude` process confined (macOS's default; Linux opt-in, else G25) reads all but keys and tarae's state, and the network is open; on Linux it reaches any unix socket whose path it can find — the session D-Bus (`systemd-run --user`: anything unconfined), tmux's, the agents under `$XDG_RUNTIME_DIR` — so its confinement is no wall against a hostile claude program, only against its file bugs (G24); on Linux a file named like a key deeper in home than its top too, and below Landlock ABI 3 it's unconfined | it needs your login and Anthropic; Landlock only grants, by folder | a files-only profile on both (§2): Seatbelt; Landlock with claude's folder tarae's (its `~/.claude.json` write needed home writable) |
| G2 | Your own Claude `allow` rules and plugins apply in the chat | they're yours; claude reads them itself | said |
| G3 | Reads outside home are open | programs need `/usr`, `/opt`; no list | said |
| G4 | Network on + hosts "any" = exfiltration to any host | the default favours builds that fetch | said; a host list is one setting |
| G5 | macOS: other processes' environments and command lines | no Seatbelt rule | said to Claude and you |
| G6 | macOS: shared memory, renice, loopback whoever's | no Seatbelt rule | said |
| G7 | macOS: LAN addresses' risky ports with the network on | Seatbelt names ports by `localhost` only | open; the proxy could check |
| G8 | macOS: a leftover without its tag isn't ended | no subreaper | said; stays sandboxed |
| G9 | Linux C/D: processes, SysV IPC, `/dev/shm` shared; A/B: processes | no namespaces there | said; the AppArmor profile raises C to A+ |
| G10 | Linux: a new protected name in a folder without one | Landlock only grants | caught after the run, asked before anything runs it — Claude's next run, your task, your preflight (§1.2) |
| G11 | Linux ABI 2: truncation | kernel | every run asks |
| G12 | The supervisor and proxy parse hostile input inside tarae | threads (D19) | fuzzing; a confined process (D21); a review |
| G13 | rust-analyzer runs `build.rs`/proc macros untrusted | not tied to trust | later |
| G14 | The temp folders are shared | D16 | said |
| G15 | Keychain/Apple Events/LaunchServices denials are pinned, not shown | no behaviour test | the must-be-refused suite |
| G16 | Landlock rules are shown on the kernels CI and the test machines have | kernels differ | the suite on several kernels |
| G17 | claude's own git reads a repository's config (`core.fsmonitor` …) | it's claude's, not tarae's | measured: its git didn't run the fsmonitor that a plain `git status` there runs (2.1.296); on macOS what it would run is confined as claude is; measured again by the live check under the profile |
| G18 | `llm.sandbox = false` runs Claude's commands unconfined with no card | your choice, said in the title | said; a project needs your trust to set it |
| G19 | claude writes `~/.claude.json`, which holds your user-scope MCP servers (commands your own claude starts) | claude's state lives in it | said; the chat runs `--strict-mcp-config` |
| G20 | claude inherited `SSH_AUTH_SOCK`: the agent signed with your keys for it | unix sockets stay open (files-only profile) | narrowed (on Linux an agent socket claude finds is still reached — G1): no claude tarae starts gets `SSH_AUTH_SOCK`, and on macOS its profile refuses connecting to the socket it named when tarae started (real path) and to launchd's agent sockets in `/private/var/run/` (macOS 27) and `/private/tmp/` (older) (test: `llm::tests::every_claude_tarae_starts_has_no_ssh_agent` · `sandbox::claude::tests::claude_cant_reach_your_ssh_agent` · `sandbox::claude::tests::launchds_agent_sockets_are_refused_in_both_places`; live: `ssh-add -l` refused under the profile on 27.0.1; `/private/var/run/` is checked under Seatbelt only by the live check — root's folder, no test stand-in); left: on Linux no profile yet (the variable is still removed); on macOS an agent at a socket the variable didn't name at start |
| G21 | Claude Code's auto-memory (`~/.claude/projects/<project>/memory/`) was read unasked, shared by every checkout | inside claude's own folder | closed: tarae starts every claude with auto memory off (test: `llm::tests::every_claude_tarae_starts_has_its_memory_off`); left: on Linux with claude confined its folder is tarae's, and the memory folder in it is claude's to read (your `~/.claude` closed to it) — nothing reads it now |
| G23 | macOS: a folder renamed into the project carries protected names past their deny — Seatbelt checks a rename's two paths, not what's in it: a run (or claude with Edit and Write) makes `x/` in temp holding `build.rs`/`Cargo.toml`/`.cargo/`, then moves it to the project as `crates/x` | no Seatbelt rule for a folder's contents on rename | caught after the run, asked before anything runs it, as G10 (§1.2: new files, ignored folders' names, names past 5,000 new files, links, repositories); left: an ignored folder that was there before, what a leftover makes later, no git checkout, your tests and `:restart`'s build |
| G22 | A refusal of claude's own work (not a tool's) goes unseen; `~/.claude/` is a denylist | the kernel didn't report claude's refusals (V1d, live) | an allowlist of what claude writes, once its refusals can be seen — and with it a measurement of `shell-snapshots/` (kept closed unmeasured: whether a later claude sources one it didn't make can't be told from here) |
| G24 | Linux, claude confined (opt-in; by default it isn't — G25): claude reaches any unix socket whose path it can find — the session D-Bus (`systemd-run --user` runs anything unconfined), tmux's sockets, the SSH and GPG agents under `$XDG_RUNTIME_DIR` (G20 there: the variable left out, the agent still reached). Against a hostile claude program its confinement is no wall; it holds against claude's file bugs. The chat's claude has no Bash tool: only claude's own code could | its rules are files only (decision 17) | said (the chat, once); deferred (user decision 2026-10-15): at V3, on a 7.2+ kernel, Landlock's right to connect to a unix socket by its path (`RESOLVE_UNIX`, ABI 9 — `landlock.rs` reads it from Linux 7.2's uapi header) granted only beneath `/tmp/cc-socks/`, where claude binds its own (measured); below ABI 9 the gap stays, said. Not taken: seccomp (a plain filter can't see a connect's path, a pointer, and denying `socket(AF_UNIX)` outright refuses that bind too — it needs the runs' supervisor deciding each address instead); a mount and user namespace showing claude an empty `$XDG_RUNTIME_DIR` and a `/tmp` of its own (needs user namespaces, tiers A+/A; abstract sockets stay to the ABI 6 scope) |
| G25 | Linux, by default: the `claude` process isn't confined (`llm.claude-sandbox` off) — a hostile or buggy claude program can write what you can (your shell's startup files, `~/.local/bin`, your Claude Code's settings and hooks, the project's protected names) and read your keys | Landlock only grants: claude refreshing its login in `~/.claude/.credentials.json` needs all of `~/.claude` open, so confined it needs a login of its own — a second login (user decision 2026-10-16); a copied login fights its original (refresh tokens rotate) | said (here, the settings, the docs); opt-in `llm.claude-sandbox = true` (a login of tarae's own); Claude's commands stay sandboxed and its edits reviewed, as Claude Code's and Codex's own sandboxes do |

## 5. For a reviewer: where to start

- **The supervisor** — `src/sandbox/supervisor.rs`: reads a run's arguments from `/proc/<tid>/mem` after
  `SECCOMP_IOCTL_NOTIF_ID_VALID`, decides on tarae's copy (`route.rs`), makes the call on the run's socket
  (`pidfd_getfd`), never `CONTINUE`. By copies: `copies.rs`. Unix paths: `openat2`, no links.
- **The proxy** — `src/sandbox/proxy.rs`, `proxy/parse`, `proxy/egress`, `proxy/upstream`, `redirect.rs` (TPROXY).
- **The keeper's birth** — `src/sandbox/keeper.rs` (`clone3`, raw calls until exec), `channel.rs` (its request
  channel), `tier.rs` (the probe).
- How each test was seen failing: the design doc's
  [appendix](design/sandbox-keeper.md#appendix-how-each-test-was-seen-failing).
