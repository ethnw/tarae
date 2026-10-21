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
outside the project ask you first. The `claude` program itself runs as you, as it does in any editor or terminal:
what keeps it is that everything it does through its tools goes through tarae — no shell, hooks, plugins, MCP
servers, skills or auto memory (§2). What's not kept: claude's own code reads and writes what you can, runs git in
the project, reaches Anthropic's servers and reads its login (§2 lists what it touched, measured); a command can
read what's outside your home folder and
the temp folders; with the network on it can send what it read to the language packs' registries (they take
uploads) and to any host you said yes to — to any host at all with `llm.sandbox-hosts = "*"`; and on macOS it can read your other processes' command
lines and starting environments.

## Who and what this defends against

| | In scope | Why it's a risk |
|---|---|---|
| **The model, misled** | Prompt injection in what Claude reads: a repository's files, a dependency's README, a test's output, a web page a command fetched | Claude acts on text it didn't write. Assume any turn may try what a hostile author wants |
| **A hostile repository** | What arrives with `git clone`/`pull`: build scripts, `.cargo/config.toml`, git hooks, `.claude/settings.json`, `.tarae.toml`, `CLAUDE.md` — and a checkout that came whole (an archive with its `.git`) | Opening it, building it or chatting in it runs or loads those |
| **A bug in Claude Code's permission path** | A tool call that skips `can_use_tool`, an update that changes what a flag means | tarae relies on claude asking. Out of scope beyond that: such a bug acts as you, as in any tool that runs claude (§2) |

Out of scope: another local user or root; malware already running as you; a hostile kernel or OS; a tampered tarae
binary or toolchain; what *you* run (your terminal, your tasks, `:sh`, preflight after its card) — tarae doesn't
sandbox you. Plugins have their own runtime and permissions ([plugins](plugins.md)); not covered here.

## The pieces, and who trusts whom

```
 you ──▶ tarae (trusted: your process, not confined)
          ├─ the claude process  (as you, like any program of yours; network open)
          │     tools: tarae's own (in tarae) — none of Claude Code's (but Edit/Write when enabled)
          │     starts by itself: git, security (its login), sh (system facts)
          ├─ Claude's runs        (sandboxed: Seatbelt on macOS; keeper + Landlock + seccomp + supervisor on Linux)
          ├─ tarae's own git, language servers, debug adapters (as you; git's repository commands held)
          └─ your terminal, tasks, preflight (as you: yours)
```

| Claude wants to… | It goes through | Kept by |
|---|---|---|
| read a file in the project | tarae's `read`/`search` → `reads::Gate` (an image or a PDF's pages as pictures: `media.rs`) | key-named files ask |
| read outside the project | the same → a card (`y`/`a`/`A`/`n`) | home, above it, key folders: once only |
| change a file | tarae's `edit`/`rename`/`replace` (or Edit/Write when enabled) → the review | you |
| run a command | tarae's `run`/`run_tests` → the sandbox | the kernel, and tarae's supervisor and proxy |
| run a command outside the sandbox | `outside_sandbox` + a reason → a card, `y`/`n` only | you, every time |
| see the screen | `screen` → a card | you |
| use git | tarae's `git` tool: reads only, an option allowlist | `gittool.rs` |
| commit | tarae's `commit` tool → a card with the message and the diff of the files it names; no hooks, no push | you |

---

## 1. Claude's commands (runs)

One sandbox per command on macOS (Seatbelt, `sandbox-exec`): denied by default, as Anthropic's sandbox-runtime's
profile is (2026-10-17) — what every run may ask the system besides files and the network is one table, `BASE` in
`src/sandbox.rs`, each row with the tool that needed it (measured: compat's stacks, tarae's own tests). On Linux each session has a *keeper*: a process
that forks every command, born in the session's own namespaces where the system allows; each command gets
Landlock and seccomp of its own, and tarae decides every call that names an address. How much of that a session
gets is its **tier** (own, host or offline, §1.8). The rules are the same on both systems; the kernel features differ.

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
- A file's attributes. macOS: its mode, times, owner, flags, extended attributes and ACLs change only where a run
  writes, by path, through a descriptor and by its `/dev/fd` name. Linux: in tiers own and host tarae serves a file's
  mode, times and extended attribute calls (ACLs too — `install -m`, `cp --preserve=all`), allowing them only where
  the run may write — or through a file it holds open for writing, which Landlock let it write when it opened it (a
  protected name it made where none was too: §1.2's card asks about that one); a path through `/proc`, `/dev/fd` or a
  magic link is refused, never resolved for it. The `security.` and `trusted.` attributes (a program's capabilities,
  a security module's labels) and every other `system.` one never change, as root too, the project's neither; nor a
  file's owner or flags; an owner call that changes nothing (`rsync -a`, `cp -a`) is let in tiers own and host where
  a mode would be ([D-h](design/sandbox-keeper.md#decisions)); what the kernel would refuse anyway with your
  credentials (`user.` on a file you may not write, `trusted.` without the capability) gets its own answer, unsaid.
  In tier offline extended attributes are refused.
  test: `sandbox::tests::a_run_changes_no_attribute_of_a_file_outside_its_write_places` (shown, macOS/Linux) ·
  `sandbox::keeper::tests::a_runs_raw_mode_and_times_calls_are_served_or_refused` ·
  `sandbox::keeper::tests::a_runs_xattr_calls_are_served_or_refused` ·
  `compat::tests::a_protected_name_is_refused_with_its_cause_or_found_made` (a made name's times, Linux) ·
  `sandbox::keeper::tests::a_file_outside_write_places_is_never_changed_by_a_link_race` ·
  `sandbox::tests::a_probe_runs_through_the_sessions_keeper` ·
  `sandbox::keeper::tests::dev_shm_is_a_runs_to_change_only_where_it_has_its_own` ·
  `sandbox::policy::tests::writes_allow_checks_dev_shm_against_kept_and_caches` ·
  `seccomp::tests::owner_xattr_and_flag_calls_are_refused_on_a_filtered_thread` (shown, Linux) ·
  `seccomp::tests::a_files_attributes_are_refused_or_go_to_tarae`,
  `seccomp::tests::the_filter_names_every_attribute_call_the_kernel_has` (rule)

Not kept:
- **The temp folders are shared** (`/tmp`, `$TMPDIR`; [D16](design/sandbox-keeper.md#decisions)). Any run, any
  session, and your own programs write there. Don't keep anything there a run mustn't change. Unix sockets there
  are closed (§1.9).
- **Source files in the project.** A run can change any file you'll later run yourself (`src/main.rs`, a test).
  That's the job. tarae records what each run changed with its turn (`snapshot.rs`) and you see it in the review,
  but nothing stops you running it unread.
- **Linux, Landlock ABI 2**: truncating isn't controlled — a run could empty any file of yours. tarae asks before
  every run at ABI 2 and says why. Below ABI 2: no sandbox, every command asks (§1.12).
- **Below a pack's tools' output** (`target/`, `node_modules/`, `.venv/`) a run writes freely — what runs from
  there later is §1.2's (G28).
- **Linux, tier offline: a file's mode and times.** Landlock has no right for them and nothing serves its calls here
  (user decision 2026-10-18: offline stays a full tier): a run can change the mode and times of any file of yours it can
  name (`chmod 644` on a key, `chmod +x` on a hook, times backdated), never what's in it, nor read it. Said in the
  report, `tarae state` and the state line (G29).

### 1.2 What runs later — the protected names

A file that runs or configures a build *without you reading it* is a way out of the sandbox: the next build you
run, outside it, runs it. So these names are never made, changed, moved, removed or hard-linked by a run, anywhere
in the project, in any case: `.git`, `.tarae.toml`, `.tarae.local.toml`, `.envrc`, `.github`, `.claude`, `.vscode`,
and every language pack's `protect` list (`build.rs`, `Cargo.toml`, `.cargo`, `Makefile`, `package.json`,
`pom.xml`, `gradlew`, `__pycache__` …), plus `llm.sandbox-protect`. Below a tool's output folder (`target/`,
`node_modules/`, `.venv/`) they're output and written as usual.

Kept:
- The names, by name, wherever they are, whatever the project's languages.
  test: `sandbox::tests::what_a_run_keeps_off_by_name` · `sandbox::tests::the_protected_names_are_the_packs_and_the_cores` ·
  `sandbox::tests::a_packs_names_are_kept_in_a_project_of_another_language` ·
  `sandbox::linux::tests::protected_names_are_found_where_they_are` ·
  `sandbox::linux::tests::a_project_inside_a_writable_place_keeps_its_protected_names`
- Python's bytecode beside its source (`__pycache__`, what your own `pytest` imports) is a protected name: a run's
  Python writes its own under `PYTHONPYCACHEPREFIX` in the run's folder; a virtualenv's is tools' output.
  test: `sandbox::tests::bytecode_beside_its_source_is_kept_and_a_virtualenvs_isnt` ·
  `sandbox::tests::what_runs_later_stays_as_it_is_under_the_real_profile` (every name, macOS)
- Folders that hold what runs later outside the project — toolchain homes, folders on `PATH`, shared caches — are
  never written and never offered on a grant card.
  test: `sandbox::tests::what_runs_later_is_never_an_asked_folder` · `allow::tests::which_folders_a_denied_write_may_open`
- Claude changes a protected file only through an edit you review, with a warning — even with review at the end.
  test: `edits::tests::review_at_the_end_still_asks_for_a_protected_file_with_its_warning` ·
  `refactor::tests::a_protected_file_in_a_change_set_is_reviewed_with_its_warning`

Not kept:
- **A protected name made where none is yet, or moved in inside a folder.** Linux: Landlock only grants. Where a
  run has a mount namespace of its own (tier own), each protected name is bound read-only over itself and every
  folder takes new files, so a new `build.rs` beside a `Cargo.toml` is made; each folder between the project and a
  protected name is bound over itself too, so it can't be moved away (or removed) with the name inside it and the
  name made anew — a file moved between such a folder and the rest of the project goes as a copy (`EXDEV`)
  (test: `sandbox::tests::protected_names_are_sealed_and_the_root_takes_new_files`); elsewhere a folder holding a protected
  name is granted entry by entry, but a folder with none can take a new `Makefile`. While names a run made wait
  for their card, the checkout's language servers are paused (rust-analyzer's check would run a new `build.rs`);
  a server's own file watch may still see one while the run goes. macOS:
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
- **What runs from tools' output** (G28). Below a pack's `outputs` (`target/`, `node_modules/`, `.venv/`) a run
  writes freely, so what your own builds, tests and tools load from there later can be planted: a test runner in
  `node_modules/.bin`, a virtualenv's `python`, a proc macro's library in `target/` (rust-analyzer loads it as
  you), a build script cargo runs again without rebuilding it. sandbox-runtime and Codex leave them writable the
  same way (their whole working folder is), and so does tarae (user decision 2026-10-17: no separate output folder
  for runs). What tarae does: `__pycache__` beside the source is protected (above); and after a run that isn't a
  build or a check (the packs' `checks` and `builders`), the programs (an executable bit), bytecode (`.pyc`) and
  libraries (`.so`, `.dylib`) it changed below an ignored output folder are named in Claude's report — by name and
  modification time, nothing read, one line a folder with its first 3, past 200,000 entries said. Said, not asked:
  no card, and a build's or a check's run isn't looked at (it writes there by design); a file copied in with its
  old time isn't seen.
  test: `snapshot::tests::code_a_run_changed_below_tools_output_is_named` ·
  `tools::tests::what_builds_is_the_checks_and_the_builders`
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
- **A file named like a key outside home, in no place runs close them** (`/opt/app/.env`): Claude's Read asks for
  it on its own card, but a run reads it, and what a run prints reaches Claude just the same — the gap is the
  runs'. Closing those names to runs outside home too (with the CA bundles and installs read, as in the places
  they close) is its own piece, planned after frictions29.
  test: `sandbox::tests::every_user_of_the_key_rule_gives_one_answer` (the one row where they part)
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
- A file named like a key is closed where another program or session could have put it, and read where only this
  run writes, one rule for runs and Claude's Read (frictions29: a key made in a granted folder after the grant was
  read unasked). Closed: your read grants and the folders you opened for writing, the other checkouts (a nested
  worktree's main checkout too), the temp folders — `/tmp`, `$TMPDIR` and its siblings, which every program of
  yours writes — and beside the project when one holds it, as in the project; there Claude's Read asks on the key's
  own card (`y` once) and a run can't read it, even one it made (its report says to make it under its own
  `$TMPDIR`; a JVM's `java.io.tmpdir` is the user's, `-Djava.io.tmpdir=$TMPDIR` moves it). Read: a run's own
  folder (its `TMPDIR`), and nowhere wider closes them, so a toolchain's folders in home stay readable — Claude's
  Read too. Outside home Claude's Read asks for one wherever it is; runs read one there that's in no place they
  close ("not kept" in §1.3). tarae's own tools (search, read, the message's context) still leave out every file named like
  a key outside the project by its name. Linux's runs close them in the project only (Landlock only grants);
  Claude's Read goes by the rule as designed there too. Inside those places a toolchain's own files read: an
  install the packs name (pyenv's, conda's, rbenv's, asdf's and mise's installs) and what's below an install's
  packages as its layout has them (`lib/python3*/site-packages`, `lib/ruby/gems`, `lib/ruby/3.*` — never a bare
  `gems`, a monorepo's own) as below tools' output; CA bundles by a `ca` name (`cacert.pem`, `ca-bundle.pem`,
  `tls-ca-bundle.pem`) outside the project — in it they're closed as any `*.pem` — and not `cert.pem`, often a
  certificate and its key in one file. One rule says it (`Policy::key_rule`): the profile, the report, the run's panel and
  `also` go by it, so a refused key is offered as that one file (`y` once), never as its folder, whose read would
  close it again.
  test: `reads::tests::a_key_named_file_outside_the_project_asks_for_itself` ·
  `sandbox::tests::every_user_of_the_key_rule_gives_one_answer` ·
  `sandbox::tests::files_named_like_keys_are_closed_where_a_grant_or_a_default_opened_outside` ·
  `sandbox::tests::keys_close_in_a_main_checkout_above_and_a_folder_opened_for_writing_ca_bundles_read` ·
  `sandbox::tests::a_key_in_a_granted_place_outside_is_closed_and_a_ca_bundle_reads` (macOS)
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
  test: `sandbox::tests::what_the_default_refuses_and_the_base_lets_through` (a look-up of each, refused)
- macOS: no service, system setting or preference outside `BASE` — every mach service but the user's name and
  groups, logging and notifications; sysctls but the machine's and the system's facts; no IOKit; preferences only the
  global domain's plists (cfprefsd is out of reach: it hands out any domain the profile lets it). Trust's service
  only where the network reaches a host as the run starts (G26).
  test: `sandbox::tests::what_the_default_refuses_and_the_base_lets_through` ·
  `sandbox::tests::every_base_rule_says_why_and_names_no_wildcard` · `sandbox::tests::trustd_only_where_the_network_goes`
- macOS: your other processes' starting environments and command lines aren't read (`KERN_PROCARGS2`): under the
  deny-default profile `process-info-pidinfo`'s deny keeps them, a run's own still read. (Linux: environments
  never — test: `landlock::tests::a_confined_run_reads_no_environment_outside_it`; command lines of the processes it
  sees, §1.9.)
  test: `sandbox::apart::two_sessions_cant_reach_each_others_processes`

Not kept:
- **Linux: a file named like a key in a place outside the project a run reads** — a folder you granted, another
  checkout, the temp folders: Landlock only grants, so a name can't be closed inside a granted folder; in the
  project its folders are granted entry by entry around each key (`linux::there`), outside they aren't yet.
  Claude's own reads ask there as on macOS.
- **A token inside a file that isn't named like a key** and isn't a known tool's settings: `config/prod.yml`
  with a password in it is an ordinary project file.
- **cargo's `credentials.toml`** isn't read, but cargo takes no copy without it: the report says so in cargo's
  pack's words.
  test: `tokens::tests::cargos_tokens_are_told_once`

### 1.5 The network

Closed by default: **nothing beyond this machine unless you say yes to a host on its card, for a run, a
conversation or a session at most, through tarae's proxy** (piece C) — "always" is `llm.sandbox-network`. Three
settings open more, all in your config (a project's `.tarae.toml` only once trusted): `llm.sandbox-ports` (ports
on this machine), `llm.sandbox-localhost` (every port on this machine), `llm.sandbox-network` (beyond this machine,
through tarae's proxy: the packs' registries unasked by default, `llm.sandbox-hosts`' list instead, or every host
with `"*"`; any other host a card).

Kept:
- **Closed**: no port on this machine; beyond it only a host you said yes to.
  test: `sandbox::tests::local_ports_open_only_where_asked` (macOS) · `sandbox::route::tests::the_route_table` (rule) ·
  `sandbox::keeper::tests::a_routed_runs_own_connect_is_refused_by_the_kernel` ·
  `sandbox::keeper::tests::the_users_loopback_isnt_reached` · `sandbox::keeper::tests::in_plain_mode_a_run_reaches_only_its_own_loopback`
- **Closed is an empty list**: with `llm.sandbox-network` off a run goes through the proxy with no host named —
  `llm.sandbox-hosts` and the project's kept (`A`) hosts don't apply, a host's grant lasts the session at most,
  and the card offers no `A`.
  test: `sandbox::tests::a_closed_network_goes_through_the_proxy_with_an_empty_list` ·
  `caps::tests::what_may_be_kept_and_for_how_long` · `hosts::tests::the_closed_network_asks_each_host_for_a_session_at_most` ·
  `sandbox::proxy::macos_tests::with_the_network_closed_a_host_waits_and_yes_lets_it_through`
- **Before your yes nothing leaves**: the name stays in the request — no lookup; a name a run makes up reaches
  only your screen; at most `Limits::questions` (8) wait at once.
  A conversation's yes (`a`) is taken back when it ends.
  test: `hosts::tests::a_chat_answer_is_taken_back_when_the_conversation_ends` ·
  `sandbox::proxy::tests::a_registrys_suggestion_answered_lets_its_names_through`
- **The caches stay read-only** but for a run that starts under a yes for their pack's registry (Rust's, Go's);
  a yes mid-run opens them from the next run on. A look-alike of a registry isn't called one.
  test: `sandbox::tests::a_registry_grant_opens_its_packs_caches_for_the_run` ·
  `hosts::tests::the_closed_network_asks_each_host_for_a_session_at_most`
- **Ports**: only those you open; on Linux this machine's other addresses count as this machine.
  test: `sandbox::keeper::tests::an_opened_port_reaches_this_machine` ·
  `sandbox::keeper::tests::this_machines_own_address_is_reached_only_where_opened` ·
  `sandbox::route::tests::this_machines_own_addresses_are_this_machine`
- **Beyond, only through tarae's proxy.** On macOS the run gets `HTTPS_PROXY` and may connect only to its own
  proxy port, with a token of its own. On Linux, in every tier, the variable names the proxy's reserved address,
  which the supervisor hands to the proxy; a tool that ignores it, or raw TCP (ssh, a database client), can't go
  out — no card opens it, only a run outside the sandbox.
  test: `sandbox::proxy::macos_tests::a_run_goes_out_only_through_its_proxy` ·
  `sandbox::proxy::macos_tests::a_run_reaches_only_its_own_proxy_port` ·
  `sandbox::proxy::macos_tests::a_runs_token_is_its_own_and_never_passed_through` ·
  `sandbox::route::tests::the_proxy_is_the_only_way_out` · `sandbox::keeper::tests::a_direct_connect_is_refused_with_the_proxy` ·
  `sandbox::keeper::tests::a_proxied_connect_holds_a_tcp_socket` ·
  `sandbox::tests::a_connection_past_the_proxy_is_said_to_open_only_outside`
- **Hosts by name.** The proxy reads the host from `CONNECT` and the TLS server name (they must match), resolves
  it itself, checks every address, and connects to the address it checked. With the network on, a host
  `llm.sandbox-hosts` doesn't name — by default every host but the packs' registries — waits on a card.
  test: `sandbox::tests::the_open_networks_default_is_the_packs_registries` ·
  `sandbox::proxy::tests::with_the_network_open_a_registry_passes_and_another_host_waits` ·
  `settings::tests::sandbox_hosts_reads_the_default_star_and_a_list`
  test: `sandbox::proxy::tests::sni_must_match_the_connect_host` · `sandbox::proxy::tests::plain_http_is_checked_by_host` ·
  `sandbox::proxy::parse::tests::a_name_is_never_an_address_in_another_form` ·
  `sandbox::proxy::tests::an_unknown_host_is_held_and_asked` · `sandbox::proxy::macos_tests::an_unknown_host_waits_for_the_answer` ·
  `sandbox::proxy::tests::a_pipelined_request_never_reaches_the_checked_address`
- **Never reached**: link-local (cloud metadata), multicast, broadcast, in any carried form; metadata names by name.
  test: `sandbox::proxy::tests::the_never_reached_are_refused_by_the_proxy` ·
  `sandbox::keeper::tests::the_never_reached_are_refused` · `sandbox::proxy::tests::upstream::metadata_names_are_refused_by_name`
- **This machine's own addresses beyond loopback** (its LAN and bridge addresses) aren't reached through the
  proxy by any name that leads there — every address, when they couldn't be read (`Egress::refuses`,
  `src/sandbox/proxy/egress.rs`; G7, closed).
  test: `sandbox::proxy::tests::the_never_reached_are_refused_by_the_proxy` ·
  `sandbox::proxy::macos_tests::a_name_leading_to_an_own_address_is_refused` ·
  `sandbox::proxy::macos_tests::a_run_whose_own_addresses_are_unread_reaches_nothing`
- **Risky ports** (Docker's API, Jupyter, DevTools, Node's inspector, Redis, etcd, kube, kubelet) aren't reached
  through the proxy at any host unless a rule of `llm.sandbox-hosts` names that host with the port
  (`docker.example:2375`) — never by `"*"`, a rule naming the host alone, or a card's yes.
  test: `sandbox::tests::the_docs_name_every_risky_port` (the list) ·
  `sandbox::proxy::tests::a_risky_port_is_reached_only_where_a_rule_names_it`
- **Your upstream proxy** (`llm.sandbox-upstream = "env"`): its host and credentials never reach a run.
  test: `sandbox::tests::runs_never_see_the_upstream` · `sandbox::proxy::tests::upstream::upstream_credentials_go_only_to_the_upstream`
- Each session's host rules are its own.
  test: `sandbox::proxy::tests::two_sessions_rules_stay_apart`

Not kept:
- **A host you said yes to is a channel**: what the run read can be sent there (a registry takes uploads; a code
  host takes anything) — every host card says so.
- **Under a yes for a pack's registry, G4 is open for those runs**: a build script can put code in the toolchain's
  caches (`~/.cargo/registry/src`) that your own builds then run — the card says so (Rust and Go: tarae's own fetch,
  piece C3, is the way that keeps them closed).
- A run waits up to `Limits::hold` (20 s) on a card (availability only).
- **With the network on, a run can send anything it read to the packs' registries** (`llm.sandbox-hosts`' default,
  every built-in pack's whatever the project's languages: crates.io, npm, PyPI, Go's proxy, Maven Central, Gradle's —
  each takes uploads) and to a host you said yes to; every other host is a card. **With `llm.sandbox-hosts = "*"`
  (`"any"` reads the same; the default before C1b), to any host** — the read rules above are then the only thing
  between your project and the internet. A list is that list alone: the registries aren't added to it.
- **`llm.sandbox-localhost` opens the risky ports too.** Neither OS can say "every port but these" once it's on.
  On macOS it also opens other runs' proxy ports, which then only their tokens guard.
- **Behind an upstream proxy**, the addresses a name leads to are the upstream's to look up: tarae checks the host
  and server name, not the address.
- **DNS as a channel**: a tool that does its own lookups gets no resolver
  (test: `sandbox::keeper::tests::under_the_proxy_a_resolver_isnt_reached`). Names asked through the proxy still
  leave as lookups tarae makes — a name can carry data.

### 1.6 Other tarae sessions

What a run is kept from and shares with other sessions' commands, per tier, at Landlock ABI 6, and on macOS. This
table is the code's — a test fails when they differ.

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

Not kept — said, not closed: what each tier's "Shared" column lists. Without the session's namespaces shared memory
(SysV, POSIX) is reachable, and on macOS POSIX's (System V's is refused there, a run's own too: G27): a run can read
and write what another session's commands — or your other programs — keep there. On macOS a loopback port it may
reach is reached whoever listens there: Seatbelt can't tell whose a listener is.

### 1.7 Git

Kept:
- `.git` is a protected name: a run can't write hooks, config, refs or the index. Runs get
  `GIT_OPTIONAL_LOCKS=0` so a read-only `git status` works.
  test: `sandbox::tests::what_a_run_keeps_off_by_name` · `sandbox::tests::git_status_leaves_the_index_in_a_sandboxed_run` ·
  `editor::tests::a_claude_run_cant_leave_git_without_its_refs`
- Claude's `git` tool runs reads only, from an option allowlist, key files kept out on git's side.
  test: `gittool::tests::only_reads_pass_and_only_the_options_listed`
- Claude's `commit` commits only on your `y`, and only the paths it names, as the card showed them. What else is
  staged stays staged. No hook runs, not even in a trusted repository, and nothing is pushed or amended. A file
  of the checkout's hooks folder is in the warning colour (the one git uses, or the one the repository's config
  names while it waits for trust): once committed, it runs on your git-tab actions with no hooks card.
  test: `committool::tests::only_the_named_paths_go_in_and_what_else_is_staged_stays` ·
  `committool::tests::what_cant_be_committed_refuses_the_call_named` · `committool::tests::a_hook_is_warned_about`
- Git that tarae runs itself never runs a repository's own commands (hooks, `core.fsmonitor`, filters,
  `diff.external`, `core.sshCommand`, credential helpers, …) until you trust them on a card — one test per key.
  test: `gitrepo::held::tests::fsmonitor_is_held_on_status_and_snapshots` ·
  `gitrepo::held::tests::a_hooks_path_is_held_on_the_git_tabs_commit` · `gitrepo::held::tests::a_credential_helper_is_held` ·
  `gitrepo::held::tests::a_submodules_own_commands_are_held`
- A repository's trust parts waiting together (its `.tarae.toml`, its Claude settings, its git's commands) are one
  card; each answer is kept as its own card's is, so one `y` is never a later change's. A part joining the card
  settles it again before `y` counts, and a git value too long to show is held back on `y`.
  test: `repotrust::tests::yes_trusts_each_part_in_its_own_store_and_a_change_asks_its_part_alone` ·
  `repotrust::tests::a_part_joining_settles_the_card_again` ·
  `repotrust::tests::a_git_value_too_long_to_show_is_held_back_on_yes`
- Your own git-tab actions ask before running a hook in the checkout that changed since the base. A `y` is
  kept for that file's content (its SHA-256, with the trust answers in `trust.json`, shared by the repository's
  checkouts): a hook rewritten since asks again, byte for byte; `n` keeps nothing. What the look can't see is
  asked about too, with only `n` (so never kept): a file it can't read or that isn't a plain one (read without
  waiting on a FIFO), a folder it can't list or past 3 deep, the files past 256. srt asks nothing here: it
  denies writes to `.git/hooks` and has no rule for a `core.hooksPath` inside the checkout.
  test: `worktree::tests::the_users_git_asks_before_hooks_a_claude_could_have_written` ·
  `worktree::tests::hooks_in_the_checkout_never_run_in_taraes_rebase` ·
  `hooks::tests::a_hook_it_cant_read_is_asked_about` · `hooks::tests::past_what_is_looked_at_is_asked_about_whole`

Not kept:
- **Git you run yourself** (your terminal, your tasks) reads the repository's config as git always does.
- **Git the claude process runs** (§2) reads it too.
- **A hooks folder outside the checkout** (`core.hooksPath` to a folder elsewhere) is trusted as it was when you
  answered; tarae checks its digest, not each run.

### 1.8 Tiers on Linux, in short

| Tier | When | What it means |
|---|---|---|
| own | unprivileged user namespaces work, and tarae serves a run's sockets (at Yama's `ptrace_scope` 2 too) | the session's own processes, loopback, IPC, `/dev/shm`, terminals; where `/proc` is masked (Docker's default) no PID namespace: processes shared (command lines, priority) |
| host | no user namespaces (Ubuntu's restriction without tarae's profile; a container), sockets served | files and signals kept; loopback this machine's, other sessions' sockets refused; processes, SysV IPC, `/dev/shm` shared |
| offline | tarae can't serve a run's sockets (Yama 3, Yama 2 without the session's namespaces, inside another seccomp listener, `pidfd_getfd` refused), with the session's namespaces or none | no network; with the namespaces its own loopback, IPC, `/dev/shm`, terminals; without, no socket at all and those shared |

Yama's `ptrace_scope` 2 asks for `CAP_SYS_PTRACE` in a run's user namespace, and tarae holds every capability in
the session's, which it made: there it serves a run's sockets (tier own; the tier probe tries it on a run of its own
keeper's, test: `sandbox::tier::tests::at_yama_2_the_probe_serves_the_sessions_namespaces` in `cargo xtask linux` on a
scope 2 machine). Without the session's namespaces (host) it can't, and the session is offline, said with the way
out: user namespaces. **tarae gets no capability for it, and no privileged helper does it** (user decision
2026-10-18): `CAP_SYS_PTRACE` in this system's namespace would let tarae — and whatever took it over — read and
write every process here, root's too, which is what scope 2 is set to stop. Only if a user asks, and then as a piece
of its own.

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
  `sandbox::keeper::tests::a_run_reads_no_details_outside_its_session` · `sandbox::keeper::tests::a_run_sees_only_its_sessions_processes` (with a PID namespace)
- **Other terminals**: a run can't open your tty or another pty. On macOS it can't make one either; the ptys
  tarae's pty broker hands in (terminals A, 2026-10-19) are new pairs tarae made, at most 128 a run open at once
  (past it an ask is refused and said: the Mac has ~511; one closed, both ends, frees its place), and a run's
  profile opens only ioctl on terminals (`^/dev/ttys[0-9]+$`, `/dev/ptmx`), **only the commands a program on a
  pty uses** (`policy::TTY_IOCTLS`: its size, its modes, its line discipline read (`stty`), its process group,
  the bytes waiting, close-on-exec — Seatbelt narrows `file-ioctl` by `ioctl-command`, measured; anything else,
  setting a line discipline (`TIOCSETD`) say, refused). An ioctl needs a
  descriptor, and a run starts with none of yours (no controlling terminal, stdio pipes). **What that leaves:** a
  descriptor could come in over a unix socket in the project or the run's own folder, if a process of yours
  passed one of your terminals' there: the run could then change its modes or size (raw mode needs the termios
  setters, so they stay — tarae's own e2e and shots set raw mode on the pty), not type into it: `TIOCSTI` isn't in
  the list, and macOS refuses it anyway (`hid-control`, measured, on a run's own controlling terminal too). A run
  with **a socket of yours granted** (`llm.sandbox-sockets`, a card's grant — docker's, colima's) is where that's
  likeliest, so such a run gets no ioctl on terminals and no broker: it asks the terminals card as before.
  test: `sandbox::tests::a_run_cant_open_another_terminal` (macOS) ·
  `sandbox::ptybroker::tests::a_run_without_the_grant_uses_the_ptys_its_broker_hands_in` (macOS) ·
  `sandbox::keeper::tests::a_run_opens_only_the_ptys_it_makes`
- **The keeper** (Linux): a run can't trace it; from Landlock ABI 6, or inside the session's PID namespace
  (as PID 1), a run can't signal it. Below ABI 6 without one, a run can kill it, and its runs fail closed
  (they fail, what they left ends, tarae says so); it isn't dumpable; a run holds no capability and no
  descriptor but its stdio; makes no user namespace.
  test: `sandbox::keeper::tests::a_command_cant_signal_the_keeper` · `sandbox::keeper::tests::the_keeper_is_not_dumpable` ·
  `sandbox::keeper::tests::a_command_holds_no_capability` · `sandbox::keeper::tests::a_command_holds_only_its_stdio` ·
  `sandbox::keeper::tests::a_run_makes_no_user_namespace_under_the_profile` ·
  `sandbox::keeper::tests::a_killed_keeper_fails_closed_and_is_remade`
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
- **Your other programs can trace a session's runs** (Linux): in the session's namespaces at Yama's `ptrace_scope`
  0, 1 and 2 — Yama asks for `CAP_SYS_PTRACE` in a run's user namespace (at 1 as one way past its descendants-only
  rule, at 2 as the only way), and any process of yours owns that namespace as tarae does, so it reaches a run there
  as tarae's supervisor does; without them (host) at 0 any process of yours, at 1 only tarae and the processes it
  descends from (your shell); at 3 none. True of every user namespace a program of yours makes; tarae opens nothing more (the rule:
  `sandbox::supervisor::tests::reaches_answers_as_yama_does`).
- **Command lines and priority**: visible and lowerable (renice) where processes are shared — Linux without a PID
  namespace of the session's (host, and own or offline where `/proc` is masked). (macOS: neither — the profile's
  default refuses `setpriority`, a run's own too, and §1.4.)
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
- Linux: whatever a run leaves (a daemon) is the keeper's child; the session ends it, and with a PID namespace of the
  session's that namespace ends it in one kill. It stays confined: everything a run starts inherits its Landlock and seccomp.
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
| `llm.sandbox-network` | the internet through the proxy: the packs' registries unasked (`llm.sandbox-hosts`' default), others a card; every host with `"*"`, a list's hosts alone with a list |
| `y`/`a`/`s` on a host card, the network closed | that host — and the registries the card lists with it — for the run, conversation or session; a registry's pack's caches writable from the next run under it |
| `y`/`a`/`s` on Claude's `also: {network}` card (or `:sandbox-blocked`'s host) | the same, before the run: that host and its pack's other registries, for the run it opens, the conversation or the session; a `y` also lifts a no you gave that host this session; a registry's caches writable from that run, the network closed. Never an address, `localhost` or a cloud metadata server's name |
| `A` on a host card, the network open (by default, or with a list) | that host, always, in this project |
| `y` on a run-outside card | one command as you, unconfined, in full — the card shows what changed since the last `y` (`sinceyes.rs`) |
| `y`/`a`/`A` on Claude's `also: {read}` card (or `:sandbox-blocked`'s) | that folder and everything in it readable for runs — `A` in this project always; home, above it and places holding keys only once |
| `y` on a key file's card | that one file readable for the run, once only: "Its commands read what's in it — a key or a token, by its name — and what they print reaches Claude." |
| `y`/`a`/`A` on Claude's `also: {env}` card | that variable of yours passed to runs — `A` keeps it for every later run in this project; a name like a secret's for the session at most: "Its commands read its value, and what they print reaches Claude." |
| `y`/`a`/`A` on Claude's `also: {terminals}` card (macOS) | every open terminal (`/dev/ttys*`), yours too — `A` in this project always: "macOS's sandbox can't tell a terminal a command made from yours: its commands could read what you type in your shells, tarae's and other sessions', and type into them." |
| `y`/`a`/`A` on a read card (Claude's Read outside the project) | Claude reads there — this once, this conversation, or always (`$state/sandbox.json`); home, above it and key folders only once |
| `y`/`s` on a no-sandbox card | commands as you, unconfined — `s` its checks and reads unasked until the session ends, in every chat of it, never kept past a restart |
| `llm.sandbox = false` | every command of Claude's as you, unconfined, **unasked** — only the chat's title says `› no sandbox` |

test: `allow::tests::which_folders_a_denied_write_may_open` · `allow::tests::home_above_it_and_key_folders_are_never_kept` ·
`tools::tests::also_asks_for_localhost_a_variable_and_the_terminals` ·
`editor::tests::claude_asks_to_run_outside_the_sandbox_with_a_reason` · `tools::tests::claude_asks_for_one_socket_not_the_sandbox_off`

Each of these settings is yours: a project's `.tarae.toml` sets one only once you trust it.
test: `trust::tests::every_sandbox_setting_waits_for_trust`

### 1.12 When there's no sandbox

No Seatbelt (inside another sandbox), Landlock missing or ABI 1: Claude's first command in a session asks first,
saying why, how the session's commands go until it ends — `y` ask before each, `s` checks and reads unasked in
every chat of the session (what would ask anyway still asks), `n` none (Claude told once). What `s` gives is what
`a` gave a conversation, spanning the session's chats; it's never kept: a restart asks again. A partial sandbox
(Landlock ABI 2, G11) asks the same, in its own words, its commands still in it as far as it goes.
test: `editor::tests::without_a_sandbox_the_session_asks_once` · `editor::tests::a_partial_sandbox_asks_once_a_session` · `sandbox::tests::a_run_outside_the_sandbox_says_it_had_no_user_namespace_where_refused`

The exception is yours to make: **`llm.sandbox = false` turns the sandbox off without a card** — every command of
Claude's then runs as you, unconfined, and only the chat's title (`› no sandbox`) and Claude's own report say so.
A project can set it only once you trust it (§1.11).
test: `editor::tests::the_sandbox_turned_off_is_said` · `trust::tests::every_sandbox_setting_waits_for_trust`

---

## 2. The claude process

`claude -p` runs as you, as every other tool runs it. Its cwd is the session's checkout. What it may *do* through
its tools is narrowed by its flags and by tarae answering its permission requests; what its own code does isn't
narrowed at all.

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
- **No sandbox of its own, on any OS** (user decision 2026-10-17). claude runs as you, as Claude Code itself, Zed,
  Cursor and Codex run it: Anthropic suggests wrapping the whole process (its sandbox runtime) only for unattended
  `--dangerously-skip-permissions` runs, which tarae never makes. What a steered claude can do goes through tarae's
  tools — the bullets below — so a process sandbox added little, and its cost was large: on macOS (V1d, 2026-10-14)
  claude's login broke every few hours, and on Linux (claudeown, 2026-10-16) Landlock had to grant `~/.claude` whole
  anyway, which holds what your own Claude Code runs later; each claude update needed the rules measured again.
  Both were removed. Your login, settings, `CLAUDE.md` and conversations are claude's as they are in your own
  CLI, and your Claude settings a hold-back's baseline; claude cleans its old conversations as your
  `cleanupPeriodDays` says, as your own Claude Code does — a project's is held (below).
  test: `llm::tests::every_claude_starts_as_its_own_program` ·
  `claudetrust::tests::a_chat_starts_held_back_while_its_card_waits` ·
  `claudetrust::tests::your_claude_settings_are_the_baseline_where_claude_loads_them`
- **It doesn't update itself** from tarae: you update it (decision A), never a program swapped under a chat that's
  running; every claude tarae starts gets `DISABLE_UPDATES`. `:claude-update` runs `claude update` as your own task, outside
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
- **Its tools**: tarae's own, in-process — none of Claude Code's (`--tools ""`), but `Edit` and `Write` with
  `llm.edit-tools` "both"/"native". No Read (tarae's `read` reads images too), no Bash, no
  WebFetch or WebSearch, no Grep or Glob, no Task.
  test: `chat::tests::each_edit_arm_says_its_own_tools`
- **Only tarae's MCP server** (`--strict-mcp-config`: a project's `.mcp.json` doesn't connect); `Skill` denied;
  hooks off (`disableAllHooks`).
  test: `claudetrust::tests::the_chat_asks_for_only_tarae_s_own_mcp_servers` ·
  `claudetrust::tests::a_project_skill_cant_pre_approve_tools_in_the_chat` ·
  `claudetrust::tests::chat_settings_l6_checks_are_what_tarae_passes`
- **Every read is tarae's** (`read`, `search`: `reads::Gate`, the rule Claude Code's Read had through
  `can_use_tool`). Inside the project it's allowed, but a key-named file — a picture named like one too;
  outside, a card says what's read; home, above it and key folders only once. An image's bytes — anyone's: a
  cloned repository's, one Claude downloaded — are parsed only by memory-safe code, in-process (the `image`
  crate, on a worker), never by an outside program, and a picture past the decoding limits (256 MB decoded,
  20,000 pixels a side) is refused before it's decoded — a decompression bomb, said; a PDF isn't parsed at all
  (its pages aren't read yet).
  Claude's `note` pins on the project's files only (a note's lines reach Claude on your reply): a path outside
  gets one answer whether or not it's there.
  test: `editor::tests::claudes_reads_outside_the_project_ask_first` · `reads::tests::inside_or_allowed_goes_without_asking` ·
  `chat::tests::a_key_named_file_reaches_claude_only_when_the_user_lets_it` · `reads::tests::a_link_to_a_key_file_is_the_key_file` ·
  `notes::tests::a_note_outside_the_project_is_refused_alike` · `readtool::tests::what_read_would_ask_about_asks` ·
  `media::tests::a_decompression_bomb_is_refused_said` ·
  `readtool::tests::a_key_named_image_asks_first`
- **`references` and `definition` read a place's line by the opened file's real path**: inside the project, or
  outside where Read would go unasked; else `(outside the project — not read)` — a link in the project leading out
  isn't inside, an open buffer of such a file neither.
  test: `nav::tests::a_place_through_a_link_out_of_the_project_isnt_read`
- **Claude's `procs` tool doesn't read what you typed**: a process under your terminal or one of your tasks is
  named by its program alone — no arguments, no folder, nor the terminal's title or the task's command; every
  other command line goes with what looks like a token taken out (`tokens::redacted` — a heuristic).
  test: `procs::tests::the_tool_says_the_users_processes_by_their_program_alone` ·
  `procs::tests::claudes_procs_tool_says_what_procs_shows`
- **Every change is reviewed**, or with review at the end allowed unseen and shown after — never for a protected
  name, `.git`, outside the project, or a buffer you changed. Moves and deletes always ask. tarae writes through
  descriptors, never a path re-walked after its check — `space u` too, a file that isn't text included, and in
  the project only.
  test: `edits::tests::an_edit_to_a_protected_file_warns_on_its_review` · `edittool::tests::a_lone_delete_is_never_written_unseen` ·
  `saver::tests::a_save_claude_caused_never_goes_through_a_link_swapped_in` ·
  `checkpoint::tests::putting_a_file_back_never_goes_through_a_link_swapped_in` ·
  `snapshot::tests::putting_a_blob_back_never_goes_through_a_link_swapped_in`
- **Every command** Claude asks for goes through tarae's `run` — §1.
- **A repository's Claude settings are held until you trust them**: helper commands, `env`, plugins, `allow` rules,
  `additionalDirectories`, `cleanupPeriodDays` (when claude deletes your conversations — every project's; `0`
  is all of them: held back, yours applies, else Claude Code's default, 30 days). While the card waits claude runs with all of them held back, as `n` leaves it — the
  chat doesn't wait for the answer; `y` starts it again with them. A change while it runs starts it again at once
  held back (mid-turn too), the card up again.
  test: `claudetrust::tests::a_chat_starts_held_back_while_its_card_waits` ·
  `claudetrust::tests::a_change_mid_chat_restarts_it_held_back_at_once` ·
  `claudetrust::tests::what_a_repository_runs_is_held_and_hooks_only_shown` ·
  `claudetrust::tests::settings_made_while_claude_runs_restart_it_held_back_and_ask` ·
  `claudetrust::tests::every_command_key_is_held_up_front_but_trusted_or_yours` ·
  `claudetrust::tests::a_projects_cleanup_period_is_held_to_yours`
- **Its environment**: API keys dropped unless you keep them; a parent Claude Code session's variables dropped.
  test: `llm::tests::what_the_process_itself_holds_never_reaches_claude` ·
  `llm::tests::keys_go_to_claude_only_when_you_keep_them_outside_claude_code`
- **Your SSH agent**: no claude tarae starts gets `SSH_AUTH_SOCK` (its git only reads status, it never needs the
  agent). A socket it finds on its own is reached (G20).
  test: `llm::tests::every_claude_tarae_starts_has_no_ssh_agent`
- **Where tarae's AppArmor profile lends user namespaces**, what starts for Claude doesn't get them.
  test: `spawn::tests::only_claudes_and_plugins_processes_are_kept_from_user_namespaces` ·
  `seccomp::tests::what_starts_outside_for_claude_makes_no_user_namespace`

Not kept:
- **claude's own code: what you can do** (G1). A bug in its permission path (a tool that skips `can_use_tool`, a
  flag whose meaning changes in an update), or a feature that acts by itself, reads and writes what you can —
  your keys, what runs later, any unix socket whose path it finds. The chat's claude has no Bash tool: this is
  claude's own code, not the model's reach — as in every tool that runs claude.
- **Your keychain, through `security`.** claude reads its login with `/usr/bin/security`; anything else that tool
  may read without a prompt is within the same reach.
- **Its own git reads the repository's config.** tarae holds a repository's commands for its own git (§1.7), not
  for the git claude starts. Measured (claude 2.1.296, macOS): a `core.fsmonitor` set in the checkout's
  `.git/config` to a stand-in did **not** run under claude's seven git calls — what claude asks git doesn't reach
  the fsmonitor — while a plain `git status` in the same repository runs it (the control). Another version, or another git command of claude's, could; a clone doesn't bring `.git/config`
  and Claude's runs can't write `.git`, but a checkout that came whole (an archive with its `.git`) can.
- **Your own Claude settings apply.** `--setting-sources user,…` loads `~/.claude/settings.json`: an `allow` rule
  of yours for Read would let reads go unasked (only a project's are held; not measured), and your plugins load
  (measured: marked in use).
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
- **The ask prompt's and commit drafts' claudes** are started with no tools at all (`--tools ""`) — what they
  could do is claude's own code alone.
- **Your user-scope MCP servers.** `~/.claude.json` holds them — commands your own claude starts — and claude must
  write that file (its state, counters, project entries): a bug could add a server there. tarae's chat runs with
  `--strict-mcp-config` and starts none of them; your own claude, outside tarae, would.
- **An SSH agent it finds.** Without the variable claude has to find a path on its own (launchd's socket on macOS,
  `$XDG_RUNTIME_DIR` on Linux); one it finds is reached (G20).
- **Claude Code's own memory folder** (`~/.claude/projects/<project>/memory/`, shared by every checkout of a
  repository) is inside claude's own folder: no claude tarae starts loads it (auto memory is off — above), but its
  own code could still read or write it, as your own Claude Code's could.

Planned: on Linux, the scopes (ABI 6) and the audit (ABI 7) checked live on a 6.12+ kernel — colima's, where the
live check runs, is ABI 4.

## 3. What tarae runs as you, outside the sandbox

Not Claude's, but a hostile repository reaches them:
- **Language servers** run the project's code as you: rust-analyzer runs `build.rs` and proc macros when it loads
  the project. **Not tied to trust yet.** Opening a hostile Rust repository in tarae runs its build script, as it
  does in any editor with rust-analyzer. Turning servers off (`editor.lsp = false`) is yours to keep: a project's
  `editor.lsp`, and a `[lang.*]` `lsp`/`also` naming a server the language doesn't list, wait for trust.
  test: `config::tests::what_starts_a_language_server_waits_for_trust` ·
  `trust::tests::every_setting_that_starts_a_program_waits_for_trust`
- **`.tarae.toml`'s risky part** (tasks, server commands, `:` bindings, settings that run something or loosen a
  guard, `[[attach]]` before-commands) waits for a trust card, keyed by real path and hash.
  test: `trust::tests::too_long_to_show_is_never_trusted` · `config::tests::the_users_own_file_is_never_held_back`
- **Preflight's pack defaults** run the project's tests as you after a card that says so.
- **Git** — §1.7.

## 4. Gaps, in one list

| | Gap | Why | Plan |
|---|---|---|---|
| G1 | The `claude` process runs as you: its own code reads and writes what you can, reaches any unix socket it finds, and the network is open | as every tool runs it (user decision 2026-10-17: the process sandbox — V1d, claudeown — removed); it needs your login and Anthropic | what protects is the tool gating (§2: reads outside the project asked, edits reviewed, no shell, hooks/plugins/MCP/Skill/auto memory off) and the runs' sandbox (§1) |
| G2 | Your own Claude `allow` rules and plugins apply in the chat | they're yours; claude reads them itself | said |
| G3 | Reads outside home are open | programs need `/usr`, `/opt`; no list | said |
| G4 | Network on = exfiltration to the packs' registries and the hosts you said yes to; with hosts `"*"`, to any host | registries take uploads; `"*"` favours builds that fetch from anywhere | said; narrowed by C1b (the default was any host); a list narrows more |
| G5 | macOS: other processes' environments and command lines | no Seatbelt rule under `(allow default)` | closed: the deny-default profile (2026-10-17) keeps them (test: `sandbox::apart::two_sessions_cant_reach_each_others_processes`) |
| G6 | macOS: POSIX shared memory, loopback whoever's | no rule names who made a POSIX name (srt allows it too); Seatbelt names ports by `localhost` only | said; System V's and renice closed by the deny-default profile (G27) |
| G7 | macOS: LAN addresses' risky ports with the network on | Seatbelt names ports by `localhost` only | closed: the proxy refuses this machine's own addresses (`src/sandbox/proxy/egress.rs`; test: `sandbox::proxy::macos_tests::a_name_leading_to_an_own_address_is_refused`), and risky ports at any host unless a rule names the port (test: `sandbox::proxy::tests::a_risky_port_is_reached_only_where_a_rule_names_it`) |
| G8 | macOS: a leftover without its tag isn't ended | no subreaper | said; stays sandboxed |
| G9 | Linux without namespaces: processes, SysV IPC, `/dev/shm` shared; without a PID namespace: processes | no namespaces there | said; the AppArmor profile gives own namespaces |
| G10 | Linux: a new protected name in a folder without one | Landlock only grants | caught after the run, asked before anything runs it — Claude's next run, your task, your preflight (§1.2) |
| G11 | Linux ABI 2: truncation | kernel | every run asks |
| G12 | The supervisor and proxy parse hostile input inside tarae | threads (D19) | fuzzing; a confined process (D21); a review |
| G13 | rust-analyzer runs `build.rs`/proc macros untrusted | not tied to trust | later; a project can't turn servers on or add one untrusted (§3) |
| G14 | The temp folders are shared | D16 | said |
| G15 | Apple Events/LaunchServices denials are pinned, not shown | no behaviour test | the must-be-refused suite; the keychain's and the clipboard's are tried (`sandbox::tests::what_the_default_refuses_and_the_base_lets_through`) |
| G16 | Landlock rules are shown on the kernels CI and the test machines have | kernels differ | the suite on several kernels |
| G17 | claude's own git reads a repository's config (`core.fsmonitor` …) | it's claude's, not tarae's | measured: its git didn't run the fsmonitor that a plain `git status` there runs (2.1.296) |
| G18 | `llm.sandbox = false` runs Claude's commands unconfined with no card | your choice, said in the title | said; a project needs your trust to set it |
| G19 | claude writes `~/.claude.json`, which holds your user-scope MCP servers (commands your own claude starts) | claude's state lives in it | said; the chat runs `--strict-mcp-config` |
| G20 | claude inherited `SSH_AUTH_SOCK`: the agent signed with your keys for it | claude runs as you (G1) | narrowed: no claude tarae starts gets `SSH_AUTH_SOCK` (test: `llm::tests::every_claude_tarae_starts_has_no_ssh_agent`); left: an agent socket claude finds on its own is reached |
| G21 | Claude Code's auto-memory (`~/.claude/projects/<project>/memory/`) was read unasked, shared by every checkout | inside claude's own folder | closed: tarae starts every claude with auto memory off (test: `llm::tests::every_claude_tarae_starts_has_its_memory_off`); left: claude's own code could still read it, as G1 |
| G23 | macOS: a folder renamed into the project carries protected names past their deny — Seatbelt checks a rename's two paths, not what's in it: a run makes `x/` in temp holding `build.rs`/`Cargo.toml`/`.cargo/`, then moves it to the project as `crates/x` | no Seatbelt rule for a folder's contents on rename | caught after the run, asked before anything runs it, as G10 (§1.2: new files, ignored folders' names, names past 5,000 new files, links, repositories); left: an ignored folder that was there before, what a leftover makes later, no git checkout, your tests and `:restart`'s build |
| G26 | macOS: where the network reaches a host as the run starts, trust's service (`com.apple.trustd.agent`) is reachable, and it fetches revocation lists and issuers itself, outside tarae's proxy | Go's TLS (and Security.framework's) asks it: without it `x509: OSStatus -26276`, measured | only then (as srt's `enableWeakerNetworkIsolation`), never with every host asked; left: a run can make it fetch a URL a certificate names |
| G27 | macOS: System V shared memory and `setpriority` are refused a run's own too, and a System V segment it makes can't be removed — `shmget` isn't checked, `shmctl(IPC_RMID)` is refused (measured: the probe's 4 KB segment stayed) | the deny-default profile, as srt's (it allows neither); a segment's key doesn't say whose it is; no rule refuses the making (XNU asks the policy nothing then) | said; a tool that needs either (a SysV user, `renice`) fails in the sandbox — a `BASE` row only with a measured need; what was made while a run ran and nothing attached (32 fill the Mac until a restart) its report names with the `ipcrm` that frees them — never removed: nothing ties a segment to the run (its maker may be gone, or the user's own program), so the user decides (`sandbox/sysv.rs`, frictions 56) |
| G28 | Below a pack's tools' output (`target/`, `node_modules/`, `.venv/`) a run writes freely: what your own builds, tests and tools load from there later can be planted | builds write there; srt and Codex leave it writable too (user decision 2026-10-17: no separate output folder for runs) | narrowed: `__pycache__/` beside the source protected; the programs, bytecode and libraries a run that isn't a build or a check changed there are named in Claude's report (§1.2; tests: `snapshot::tests::code_a_run_changed_below_tools_output_is_named`, `sandbox::tests::bytecode_beside_its_source_is_kept_and_a_virtualenvs_isnt`) |
| G29 | Linux: a file's mode and times change outside the write places, in tier offline (protected names in the project too); in tiers own and host tarae serves them where writes are allowed; in tier offline a run copies no extended attributes or ACLs (`install -m`, `cp --preserve=all` fail there) | Landlock has no right for a file's attributes; in tier offline nothing serves them | said in tier offline; in own and host tarae serves mode, times and extended attributes (2026-10-20, D2 B); owner, flags, `security.` and `trusted.` refused in every tier (D-h); closed where a Landlock ABI brings an attribute right |

## 5. For a reviewer: where to start

- **The supervisor** — `src/sandbox/supervisor.rs`: reads a run's arguments from `/proc/<tid>/mem` after
  `SECCOMP_IOCTL_NOTIF_ID_VALID`, decides on tarae's copy (`route.rs`), makes the call on the run's socket
  (`pidfd_getfd`), never `CONTINUE`. Unix paths: `openat2`, no links.
- **The proxy** — `src/sandbox/proxy.rs`, `proxy/parse`, `proxy/egress`, `proxy/upstream`, `supervisor.rs`'s `proxied` (a run's connect to the proxy's address).
- **The keeper's birth** — `src/sandbox/keeper.rs` (`clone3`, raw calls until exec), `channel.rs` (its request
  channel), `tier.rs` (the probe).
- How each test was seen failing: the design doc's
  [appendix](design/sandbox-keeper.md#appendix-how-each-test-was-seen-failing).
