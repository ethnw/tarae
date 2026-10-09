//! A repository's own git config that runs commands (`target/sb/gittrust.md`): found when the repository is
//! resolved, held on every run of tarae's git until the user trusts it on a card (`What::GitTrust`). Only the
//! repository's own scopes — `local`, `worktree` and what they include — and the hooks in its git folder: the
//! user's `global`/`system` config is theirs, never held, and its value is what a held key gets back. The
//! files a value runs — a script it names by its path, the hooks of a hooks folder outside the work tree —
//! are part of what's trusted, by their text (`cover`): one edited after a `y` is held and asked about again.
//! A hooks folder inside the work tree is the hooks card's (`hooks.rs`); here, only the setting.
//!
//! How each key is held is measured, not assumed (`target/sb/gittrust-probe*.sh`, git 2.54): an empty `-c`
//! value isn't a general "off" — for a textconv, a diff command, a merge driver, `sshCommand` or a gpg program
//! git then fails ("cannot run"), and for `gitProxy` and `uploadpack` the repository's own value still wins.

use std::path::{Path, PathBuf};

/// How a command-running key is held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Off {
    /// `-c key=value` — the user's own value instead, when `global`/`system` has the key.
    Value(&'static str),
    /// A filter's command: `-c key=` and `-c filter.<driver>.required=false` (else a required filter fails) —
    /// the user's own instead (an LFS set up globally keeps working).
    Filter,
    /// A list (credential helpers): `-c key=` empties it; the user's own again after, in order.
    Reset,
    /// `protocol.<name>.allow`: the user's own value, else their `protocol.allow`, else git's default for that
    /// protocol.
    Protocol,
    /// `protocol.allow`, what every protocol not named on its own falls back to: the user's own value, else
    /// git's own table put back (`PROTOCOLS`, each but those the user's config names) — a `protocol.allow`
    /// of any value would stand in for that table, and `user` opens `ext::` to tarae's runs.
    Protocols,
    /// A flag right after the subcommands (`on`) that take it — config can't hold it.
    Flag(&'static str, &'static [&'static str]),
    /// An environment variable git reads over its config.
    Env(&'static str, &'static str),
}

/// A key of git's that names a command, how it's held, and when it runs (said on the card).
pub struct Guard {
    /// The key; `*` stands for any subsection (`filter.*.clean`).
    pub key: &'static str,
    pub off: Off,
    pub runs: &'static str,
}

/// Subcommands that take `--no-textconv`.
const TEXTCONV: &[&str] =
    &["blame", "diff", "diff-files", "diff-index", "diff-tree", "log", "show", "whatchanged"];
/// Subcommands that take `--no-ext-diff`.
const EXT_DIFF: &[&str] = &["diff", "diff-files", "diff-index", "diff-tree", "log", "show", "whatchanged"];

/// Every key of git's that names a command tarae's git can reach (`gittrust.md` — each with a test).
pub const GUARDED: &[Guard] = &[
    Guard {
        key: "core.fsmonitor",
        off: Off::Value("false"),
        runs: "on every status: the git tab, a review, Claude's runs",
    },
    Guard { key: "filter.*.clean", off: Off::Filter, runs: "when a file is stored: staging, Claude's runs" },
    Guard {
        key: "filter.*.smudge",
        off: Off::Filter,
        runs: "when a file is written out: discard, switch, stash",
    },
    Guard { key: "filter.*.process", off: Off::Filter, runs: "when a file is stored or written out" },
    Guard {
        key: "diff.*.textconv",
        off: Off::Flag("--no-textconv", TEXTCONV),
        runs: "on blame and a commit's diff",
    },
    Guard { key: "diff.*.command", off: Off::Flag("--no-ext-diff", EXT_DIFF), runs: "on a commit's diff" },
    Guard { key: "diff.external", off: Off::Flag("--no-ext-diff", EXT_DIFF), runs: "on a commit's diff" },
    Guard {
        key: "merge.*.driver",
        off: Off::Value("git merge-file -L %X -L %S -L %Y --marker-size=%L %A %O %B"),
        runs: "on a merge: a rebase, a stash taken back",
    },
    Guard { key: "core.hookspath", off: Off::Value("/dev/null"), runs: "as hooks: commit, push, switch …" },
    Guard { key: "core.sshcommand", off: Off::Value("ssh"), runs: "on push, pull and fetch" },
    Guard {
        key: "core.gitproxy",
        off: Off::Env("GIT_PROXY_COMMAND", "false"),
        runs: "on push, pull and fetch",
    },
    Guard { key: "core.askpass", off: Off::Value(""), runs: "when a remote asks for a password" },
    Guard { key: "credential.helper", off: Off::Reset, runs: "when a remote asks for a password" },
    Guard { key: "credential.*.helper", off: Off::Reset, runs: "when a remote asks for a password" },
    Guard {
        key: "remote.*.uploadpack",
        off: Off::Flag("--upload-pack=git-upload-pack", &["fetch", "ls-remote", "pull"]),
        runs: "on pull and fetch",
    },
    Guard {
        key: "remote.*.receivepack",
        off: Off::Flag("--receive-pack=git-receive-pack", &["push"]),
        runs: "on push",
    },
    Guard { key: "remote.*.vcs", off: Off::Value(""), runs: "on push, pull and fetch" },
    Guard {
        key: "protocol.allow",
        off: Off::Protocols,
        runs: "on push, pull and fetch (ext:: runs anything)",
    },
    Guard {
        key: "protocol.*.allow",
        off: Off::Protocol,
        runs: "on push, pull and fetch (ext:: runs anything)",
    },
    Guard { key: "core.alternaterefscommand", off: Off::Value(""), runs: "on push and fetch" },
    Guard { key: "gpg.program", off: Off::Value("gpg"), runs: "when a commit is signed" },
    Guard { key: "gpg.openpgp.program", off: Off::Value("gpg"), runs: "when a commit is signed" },
    Guard { key: "gpg.x509.program", off: Off::Value("gpgsm"), runs: "when a commit is signed" },
    Guard { key: "gpg.ssh.program", off: Off::Value("ssh-keygen"), runs: "when a commit is signed" },
    Guard { key: "gpg.ssh.defaultkeycommand", off: Off::Value(""), runs: "when a commit is signed" },
];

/// What the card says of the hooks in the git folder.
pub const HOOKS_RUN: &str = "as hooks: commit, push, switch …";

/// The guard for `key` exactly as git lists it: section and name lowercase, a subsection as written — and
/// subsections differ by case (`filter.Lfs.clean` and `filter.lfs.clean` are two keys, each its own driver).
pub fn guard(key: &str) -> Option<&'static Guard> {
    GUARDED.iter().find(|g| match g.key.split_once(".*.") {
        Some((section, name)) => {
            let (head, tail) = (format!("{section}."), format!(".{name}"));
            key.len() > head.len() + tail.len() && key.starts_with(&head) && key.ends_with(&tail)
        }
        None => key == g.key,
    })
}

/// git's own table for the protocols it names (`transport.c`, with no `protocol.allow` set) — any other
/// protocol is `user`.
const PROTOCOLS: &[(&str, &str)] = &[
    ("ext", "never"),
    ("file", "user"),
    ("git", "always"),
    ("http", "always"),
    ("https", "always"),
    ("ssh", "always"),
];

/// git's own default for `protocol.<name>.allow`.
fn protocol_default(key: &str) -> &'static str {
    let name = key.strip_prefix("protocol.").and_then(|k| k.strip_suffix(".allow")).unwrap_or_default();
    let name = name.to_ascii_lowercase();
    PROTOCOLS.iter().find(|(p, _)| *p == name).map_or("user", |(_, d)| d)
}

/// An include's key: `include.path`, `includeIf.<condition>.path` (as listed).
fn include(key: &str) -> bool {
    key == "include.path" || (key.starts_with("includeif.") && key.ends_with(".path") && key.len() > 15)
}

/// What the resolve lists: the guarded keys and the includes, every scope, nothing else of the user's config
/// (tokens in `url.*.insteadOf`, `http.extraHeader` … never read) — one `git config --get-regexp` (ERE,
/// matched against the key as listed). Lists what `--list` does for those keys
/// (`the_narrow_listing_is_the_full_one_cut_to_the_guarded_keys`).
pub fn listing_args() -> Vec<String> {
    let keys: Vec<String> = GUARDED
        .iter()
        .map(|g| g.key)
        .chain(["include.path", "includeif.*.path"])
        .map(|k| k.split(".*.").map(|p| p.replace('.', "\\.")).collect::<Vec<_>>().join("\\..+\\."))
        .collect();
    ["config", "--show-scope", "--show-origin", "--includes", "-z", "--get-regexp"]
        .into_iter()
        .map(String::from)
        .chain([format!("^({})$", keys.join("|"))])
        .collect()
}

/// The file an include names, resolved as git does: `~/` from home, a relative path from the folder of the
/// config file that includes it. None: one it can't name (`~user/`, `%(prefix)/` — outside every place a
/// run can write).
fn include_target(origin: &Path, value: &str) -> Option<PathBuf> {
    if let Some(rest) = value.strip_prefix("~/") {
        return crate::allow::home().map(|h| h.join(rest));
    }
    if value.starts_with('~') || value.starts_with("%(") || value.is_empty() {
        return None;
    }
    Some(origin.parent().unwrap_or(Path::new("")).join(value))
}

/// One entry of `git config --list --show-scope --show-origin --includes -z` (or `listing_args`').
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub scope: String,
    /// The file it's from (`file:` taken off), when it's a file.
    pub origin: Option<PathBuf>,
    pub key: String,
    /// None: a key written with no `=` (git reads it as true).
    pub value: Option<String>,
}

/// `git config --list --show-scope --show-origin --includes -z`'s output: scope NUL origin NUL key LF value NUL.
pub fn parse(out: &str) -> Vec<Entry> {
    let mut fields = out.split('\0');
    let mut entries = Vec::new();
    while let (Some(scope), Some(origin), Some(kv)) = (fields.next(), fields.next(), fields.next()) {
        if scope.is_empty() {
            break;
        }
        let (key, value) = match kv.split_once('\n') {
            Some((k, v)) => (k, Some(v.to_string())),
            None => (kv, None),
        };
        let origin = origin.strip_prefix("file:").map(PathBuf::from);
        entries.push(Entry { scope: scope.to_string(), origin, key: key.to_string(), value });
    }
    entries
}

/// What a run puts in while a repository's commands are held.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Hold {
    /// (key, value), in order — given to git as `settings` says, never as `-c key=value`: `-c` splits at the
    /// first `=`, and a subsection can hold one (`filter.x=y.clean=` set `filter.x`, and the repository's own
    /// `x=y` filter ran).
    pub config: Vec<(String, String)>,
    /// A flag, and the subcommands it goes after.
    pub flags: Vec<(&'static str, &'static [&'static str])>,
    pub env: Vec<(&'static str, &'static str)>,
    /// Pathspecs `add` takes after its own (`keep_out`).
    pub exclude: Vec<String>,
}

/// Subcommands that look at the work tree's submodules and take `--ignore-submodules`.
const WORK_DIFF: &[&str] = &["status", "diff", "diff-files", "diff-index"];

/// The start of the environment variables `Hold::settings` puts the values in (`TARAE_GIT_HELD_0` …).
pub const VAR: &str = "TARAE_GIT_HELD_";

impl Hold {
    /// Whether `key` is put in already.
    fn has(&self, key: &str) -> bool {
        self.config.iter().any(|(k, _)| k == key)
    }

    /// What of it goes on a shell line made before its subcommand (`Repo::shell_line`, the rebase's): every
    /// setting and variable — the network's too: the rebase never fetches (`NO_LAZY_FETCH`), and held all the
    /// same — but no flags (they go after a subcommand) and no `add` pathspecs.
    pub fn for_line(&self) -> Hold {
        Hold { config: self.config.clone(), env: self.env.clone(), ..Hold::default() }
    }

    /// `config` as git takes it whatever a key holds: each an argument `--config-env=<key>=<variable>` and
    /// that variable — git splits the argument at its last `=`, and a variable's name holds none. The user's
    /// own `GIT_CONFIG_COUNT`/`GIT_CONFIG_PARAMETERS` stay as they are: git reads those first and these after
    /// (measured, git 2.54: `the_users_own_git_config_environment_stays`). (argument, variable, value).
    pub fn settings(&self) -> Vec<(String, String, &str)> {
        let vars = self.config.iter().enumerate();
        vars.map(|(i, (k, v))| (format!("--config-env={k}={VAR}{i}"), format!("{VAR}{i}"), v.as_str()))
            .collect()
    }

    /// Kept out of the submodules at `rels` (from the top), whose own commands are held (`gitrepo::Sub`): git
    /// would run a status in each — its fsmonitor, its filters — to say whether it's dirty, and fetch, push
    /// or check one out when the user's config says to recurse. `names`: every submodule `.gitmodules`
    /// names, whose own `ignore` there (`none`) would win over `diff.ignoreSubmodules`. Measured (git 2.54,
    /// `held::tests::a_held_submodule_is_kept_out_of_the_parents_runs`): the flag where a subcommand takes
    /// it, the config where it doesn't (commit, stash), and `add` — which takes neither — not given them. (A
    /// submodule's name, from `.gitmodules`, can hold a `=`: `settings`.)
    pub fn keep_out(&mut self, names: &[String], rels: &[String]) {
        for (k, v) in [
            ("diff.ignoreSubmodules", "dirty"),
            ("status.submoduleSummary", "false"),
            ("diff.submodule", "short"),
            ("submodule.recurse", "false"),
            ("fetch.recurseSubmodules", "false"),
            ("push.recurseSubmodules", "no"),
        ] {
            self.config.push((k.into(), v.into()));
        }
        self.flags.push(("--ignore-submodules=dirty", WORK_DIFF));
        self.flags.push(("--no-recurse-submodules", &["fetch", "pull", "push"]));
        self.config.extend(names.iter().map(|n| (format!("submodule.{n}.ignore"), "dirty".into())));
        self.exclude = rels.iter().map(|r| format!(":(exclude,top,literal){r}")).collect();
    }

    /// `args` (a run's, `-c` pairs first) with the flags put right after its subcommand — and `add`'s
    /// pathspecs ending in `exclude`.
    pub fn args<'a>(&'a self, args: &[&'a str]) -> Vec<&'a str> {
        let mut i = 0;
        while i + 1 < args.len() && args[i] == "-c" {
            i += 2;
        }
        let mut out: Vec<&'a str> = args.iter().copied().take(i + 1).collect();
        if let Some(sub) = args.get(i) {
            for (flag, on) in &self.flags {
                if on.contains(sub) && !out.contains(flag) {
                    out.push(flag);
                }
            }
        }
        out.extend(args.iter().skip(i + 1).copied());
        if args.get(i) == Some(&"add") && !self.exclude.is_empty() {
            let rest = &args[i + 1..];
            let named = match rest.iter().position(|a| *a == "--") {
                Some(dash) => dash + 1 < rest.len(),
                None => rest.iter().any(|a| !a.starts_with('-')),
            };
            if !rest.contains(&"--") {
                out.push("--");
            }
            // (no pathspec is the whole tree: said, so the exclusions alone don't stand for `.` from `dir`)
            if !named {
                out.push(":/");
            }
            out.extend(self.exclude.iter().map(String::as_str));
        }
        out
    }
}

/// A repository's command-running config and hooks, as found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Local {
    /// What the card shows, in order: (key, value) as git lists them — a hook as (`hook <name>`, its file).
    pub found: Vec<(String, String)>,
    /// The files what's found runs (`cover`: a script a value names, a hooks folder's hooks, the git folder's
    /// hooks), each with its text's digest — None: not there (yet). Part of what an answer is about: an edit
    /// to one, or one made, asks again.
    pub covers: Vec<(PathBuf, Option<crate::trust::Digest>)>,
    /// The files that decide them, kept by their times (a change resolves the repository again): every file
    /// of the repository's own the listing names, every include's file — there or not yet, its condition met
    /// or not — `HEAD` while an `onbranch:` include is listed, every file in the hooks folder but the samples
    /// (executable or not).
    pub files: Vec<PathBuf>,
    /// Those of `files` stamped here before they were read — the hooks (`gitrepo::read_local` stamps the
    /// rest before the listing).
    pub stamped: Vec<(PathBuf, super::Stamp)>,
    pub hold: Hold,
}

impl Local {
    /// What an answer is about (`trust::hash`): what's found, and the text of each file it runs.
    pub fn digest(&self) -> crate::trust::Digest {
        let files = self.covers.iter().map(|(p, d)| {
            let text =
                d.map_or_else(|| "not there".into(), |d| d.iter().map(|b| format!("{b:02x}")).collect());
            (format!("file {}", p.to_string_lossy()), text)
        });
        crate::trust::hash(&self.found.iter().cloned().chain(files).collect::<Vec<_>>())
    }

    /// A value too long for the card: never trusted (only `n`).
    pub fn showable(&self) -> bool {
        self.found.iter().all(|(_, v)| v.chars().count() <= crate::trust::SHOWN_MAX)
    }
}

/// The scopes that are the repository's own; every other one — `global`, `system`, `command` (the
/// environment tarae runs in: `GIT_CONFIG_COUNT`, `GIT_CONFIG_PARAMETERS`; tarae's own `-c` on the listing
/// names no guarded key) and git's built-in config (Apple's git lists Xcode's `git-core/gitconfig` as
/// `unknown`, with `credential.helper=osxkeychain` — measured) — is the user's.
fn own(scope: &str) -> bool {
    scope == "local" || scope == "worktree"
}

fn users(scope: &str) -> bool {
    !own(scope)
}

/// From the listing (`parse`), the repository's hooks folder (`hooks`, the common one) and the checkout's
/// `HEAD`: what's found, and how a run holds it. Keys are compared exactly as listed.
pub fn find(entries: &[Entry], hooks: &Path, head: &Path) -> Local {
    let own = |e: &&Entry| own(&e.scope);
    let users = |key: &str| -> Vec<&str> {
        entries
            .iter()
            .filter(|e| users(&e.scope) && e.key == key)
            .map(|e| e.value.as_deref().unwrap_or("true"))
            .collect()
    };
    let mut local = Local::default();
    let watch = |files: &mut Vec<PathBuf>, f: PathBuf| {
        if !files.contains(&f) {
            files.push(f);
        }
    };
    let mut held: Vec<&str> = Vec::new();
    for e in entries.iter().filter(own) {
        if let Some(o) = &e.origin {
            watch(&mut local.files, o.clone());
        }
        if include(&e.key) {
            if let (Some(o), Some(v)) = (&e.origin, &e.value)
                && let Some(target) = include_target(o, v)
            {
                watch(&mut local.files, target);
            }
            if e.key.starts_with("includeif.onbranch:") {
                watch(&mut local.files, head.to_path_buf());
            }
            continue;
        }
        let Some(g) = guard(&e.key) else { continue };
        local.found.push((e.key.clone(), e.value.clone().unwrap_or_else(|| "true".into())));
        if held.contains(&e.key.as_str()) {
            continue;
        }
        held.push(&e.key);
        let theirs = users(&e.key);
        let h = &mut local.hold;
        let set = |h: &mut Hold, v: &str| h.config.push((e.key.clone(), v.into()));
        match g.off {
            Off::Value(off) => set(h, theirs.last().copied().unwrap_or(off)),
            Off::Filter => match theirs.last() {
                Some(v) => set(h, v),
                None => {
                    set(h, "");
                    let driver = e.key.rsplit_once('.').map_or("", |(d, _)| d);
                    h.config.push((format!("{driver}.required"), "false".into()));
                }
            },
            Off::Reset => {
                set(h, "");
                for v in &theirs {
                    set(h, v);
                }
            }
            // (a protocol's key once: `Protocols` may have put its default in already — the same value)
            Off::Protocol if !h.has(&e.key) => {
                let fallback = users("protocol.allow").last().copied();
                set(h, theirs.last().copied().or(fallback).unwrap_or(protocol_default(&e.key)))
            }
            Off::Protocol => {}
            Off::Protocols => match theirs.last() {
                Some(v) => set(h, v),
                None => {
                    set(h, "user");
                    for (name, default) in PROTOCOLS {
                        let key = format!("protocol.{name}.allow");
                        if users(&key).is_empty() && !h.has(&key) {
                            h.config.push((key, default.to_string()));
                        }
                    }
                }
            },
            Off::Flag(flag, on) => {
                if !h.flags.iter().any(|(f, _)| *f == flag) {
                    h.flags.push((flag, on));
                }
            }
            Off::Env(var, value) => {
                if !h.env.iter().any(|(v, _)| *v == var) {
                    h.env.push((var, value));
                }
            }
        }
    }
    // Hooks in the git folder — used only while no scope names a hooks folder of its own
    if !entries.iter().any(|e| e.key == "core.hookspath") {
        let mut names: Vec<(String, PathBuf)> = std::fs::read_dir(hooks)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
            .filter(|(name, _)| !name.ends_with(".sample"))
            .collect();
        crate::sorting::sort(&mut names);
        // (every one watched, executable or not: a `chmod +x` later is a hook that runs — stamped before it's
        // looked at)
        for (_, path) in &names {
            watch(&mut local.files, path.clone());
            local.stamped.push((path.clone(), super::stamp(path)));
        }
        names.retain(|(_, path)| {
            use std::os::unix::fs::PermissionsExt as _;
            // (as git runs it: a link followed — `DirEntry::metadata` wouldn't)
            std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        });
        if !names.is_empty() {
            for (name, path) in names {
                // (its text's digest whole in `covers`, the start of it on the card: a hook changed since is
                // asked about again)
                let sum = text_digest(&path);
                local.covers.push((path.clone(), sum));
                let short: String =
                    sum.unwrap_or_default().iter().take(6).map(|b| format!("{b:02x}")).collect();
                // (a link: where it leads, what runs)
                let to = std::fs::read_link(&path).map(|t| format!(" → {}", t.display())).unwrap_or_default();
                local.found.push((format!("hook {name}"), format!("{}{to} · {short}", path.display())));
            }
            local.hold.config.push(("core.hooksPath".into(), "/dev/null".into()));
        }
    }
    local
}

/// A file bigger than this (a program a value names by its path, say) isn't read for its digest: what
/// stands for its text is the file itself as the system keeps it — its inode, size and change time, which
/// any write moves and no one can set back — read again on every resolve otherwise.
const READ_MAX: u64 = 8 << 20;

/// The digest of the file at `path` (a link followed, as git runs it) — None: no file there. Opened without
/// waiting (a fifo put there would hold the resolve up forever), then looked at through what was opened.
fn text_digest(path: &Path) -> Option<crate::trust::Digest> {
    use std::io::Read as _;
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
    let mut f = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(path).ok()?;
    let m = f.metadata().ok()?;
    if !m.is_file() {
        return None;
    }
    if m.len() > READ_MAX {
        let it = format!("file {} {} {} {}.{}", m.dev(), m.ino(), m.len(), m.ctime(), m.ctime_nsec());
        return Some(crate::sha256::digest(it.as_bytes()));
    }
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes).ok()?;
    Some(crate::sha256::digest(&bytes))
}

/// A value's words, as a shell or git splits them closely enough to find the paths in it (`--file=x/y` →
/// `x/y`) — a word too many is only a file more covered.
fn words(value: &str) -> impl Iterator<Item = &str> {
    let apart = |c: char| c.is_whitespace() || "'\"`;|&()<>={}!,".contains(c);
    value.split(apart).filter(|w| {
        !w.is_empty() && !w.starts_with(['-', '~', '%', '$']) && !w.contains("://") && !w.contains('*')
    })
}

/// The files `local.found`'s values run, covered (`Local::covers`) and watched (`files`, stamped before
/// each is read): every word that's a path — a relative one from `runs_in`, where git runs it (the work
/// tree's top, else the git folder) — to a file, or to nothing yet (made later, it asks again). An absolute
/// one wherever it leads: a temp folder is a place a run writes too, and one config's checkouts and its git
/// folder alone then agree on it — a relative path is each checkout's own file. A bare word is looked up on
/// `PATH` (by `sh`, or git's own exec), not here — but `core.hooksPath`'s is a folder from where git runs:
/// the folder, and every hook in it (the executable ones covered; the rest watched, as the git folder's are —
/// a `chmod +x` later is a hook that runs). Unless that folder is in `work_tree` (and not under one of
/// `git_dirs`): its hooks are the hooks card's (`hooks::unseen` — every one not as the base has it, shown
/// before a git-tab action runs it), and the setting alone is what's trusted here — one card for one edit.
pub fn cover(local: &mut Local, runs_in: &Path, work_tree: Option<&Path>, git_dirs: &[&Path]) {
    let the_hooks_cards = |folder: &Path| {
        let real = crate::paths::real(folder);
        real.is_some_and(|r| {
            work_tree.is_some_and(|t| r.starts_with(t)) && !git_dirs.iter().any(|g| r.starts_with(g))
        })
    };
    use std::os::unix::fs::PermissionsExt as _;
    // (stamped before it's read)
    fn watch(local: &mut Local, path: &Path) {
        if !local.files.iter().any(|f| f == path) {
            local.files.push(path.to_path_buf());
            local.stamped.push((path.to_path_buf(), super::stamp(path)));
        }
    }
    fn add(local: &mut Local, path: PathBuf) {
        if !local.covers.iter().any(|(p, _)| *p == path) {
            let sum = text_digest(&path);
            local.covers.push((path, sum));
        }
    }
    let values: Vec<(String, String)> =
        local.found.iter().filter(|(k, _)| !k.starts_with("hook ")).cloned().collect();
    for (key, value) in values {
        let hooks = key == "core.hookspath";
        for word in words(&value) {
            let named = Path::new(word);
            if !hooks && !word.contains('/') {
                continue;
            }
            let path = runs_in.join(named);
            if hooks && the_hooks_cards(&path) {
                continue;
            }
            watch(local, &path);
            if !path.is_dir() {
                add(local, path);
                continue;
            }
            if !hooks {
                continue;
            }
            let mut each: Vec<PathBuf> = std::fs::read_dir(&path)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| !p.to_string_lossy().ends_with(".sample"))
                .collect();
            crate::sorting::sort(&mut each);
            for hook in each {
                watch(local, &hook);
                if std::fs::metadata(&hook).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                {
                    add(local, hook);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
