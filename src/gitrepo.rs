//! The one way tarae runs git. A repository is resolved once from a root tarae trusts — a session's root, a
//! checkout it made, the top git named for one — and kept; every git after runs on it with its git folder and
//! work tree given (`--git-dir`, `--work-tree`), never found again by discovery from where the call starts.
//! Discovery from a file's folder took a subfolder holding `HEAD`, `objects/` and `refs/` for a repository,
//! with that folder's `config` (`target/sb/gitescape.md`); a file's git (the gutter, blame, staging, `:diff`)
//! goes through `for_file` instead: the session's repository, or the submodule of it the file is in as the
//! repository records it. `-c safe.bareRepository=explicit` on every run is the second guard: a bare
//! repository only when named, also for git's own discovery (the resolving, a hook's git).
//!
//! Two forms of a run, for one reason: git exports what `--git-dir`/`--work-tree` name as `GIT_DIR` and
//! `GIT_WORK_TREE` to everything it starts. tarae's own git (`Repo::command`: reads, `NO_HOOKS` steps, the
//! rebase) runs no hooks, so it names the repository. The git-tab actions that run the user's own hooks —
//! commit, pull, push, fetch, switch, stash, branch work (`hooked`, `Repo::hooked_command`) — run from the
//! work tree's top with nothing named, as from a terminal: a hook that runs git in another repository (a nested
//! project's lint-staged, a submodule) would otherwise act on this one. There the top's own `.git` (a
//! protected name — nothing a run of Claude's makes) is what git finds first, `GIT_CEILING_DIRECTORIES` stops
//! the climb above the top, and a bare look-alike is refused.
//!
//! A file in no session's repository (`for_file_in`): its own repository by discovery from its folder — only
//! where no sandboxed run can write (`sandbox::write_places`, the policy's own list), the climb stopped at
//! those places; inside one, none (`NoGit::Writable`, said once). Runs not sandboxed (`llm.sandbox` off, no
//! sandbox here): a run can write anywhere and run git itself, so nothing is held back — discovery from the
//! file's folder wherever it is, still refusing a bare look-alike.
//!
//! A repository's own config can name commands git runs (fsmonitor, filters, textconv, merge drivers, ssh,
//! credential helpers, gpg …) and its git folder can hold hooks: found when it's resolved (`held.rs`), and
//! held on every run — settings (`Hold::settings`), flags, environment — until the user trusts them on a
//! card (`What::GitTrust`, answers kept as `.tarae.toml`'s are: `trust.rs`); a resolve that finds them
//! unanswered puts the card up, whichever git call it was for (`set_asker`). The user's own config —
//! `global`, `system`, what tarae's environment gives — is never held.
//!
//! Not here: the grammar download's git (`grammar.rs`) — a repository tarae makes itself in its data folder,
//! from a URL in the packs, outside any project.

mod held;

pub use held::{Hold, Local};

/// When a found key (or `hook <name>`) runs — said on the card.
pub fn runs(key: &str) -> &'static str {
    match key.starts_with("hook ") {
        true => held::HOOKS_RUN,
        false => held::guard(key).map_or("", |g| g.runs),
    }
}

use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use crate::trust::{Digest, State};

/// On every git tarae runs: a bare repository is used only when named.
const SAFE: [&str; 2] = ["-c", "safe.bareRepository=explicit"];

/// git's hooks turned off — for what tarae itself runs in a checkout, outside the sandbox (session making,
/// ending and bringing, a review's fetch, the rebase — `rebase::git_line`): a `core.hooksPath` inside a
/// checkout is a folder Claude can write (review, 2026-10-04). The user's own git-tab actions keep their hooks.
pub(crate) const NO_HOOKS: [&str; 2] = ["-c", "core.hooksPath=/dev/null"];

/// Submodules inside submodules followed at most this deep.
const DEPTH_MAX: usize = 8;

/// A repository's config listed at most this often in one resolve (`read_local`: until it names no file not
/// stamped first).
const LISTINGS_MAX: usize = 4;

/// A repository git runs on, its folders as real paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repo {
    /// The checkout's git folder (a linked checkout's own, under the common one).
    pub git_dir: PathBuf,
    /// The repository's git folder, shared by its checkouts.
    pub common_dir: PathBuf,
    /// The checkout's top — None: a git folder alone (`git_folder`).
    pub work_tree: Option<PathBuf>,
    /// Where its git runs: the root it was resolved from (the work tree, or a folder in it — a session started
    /// in a subfolder keeps `-- .` and `--relative` to that folder), else the git folder.
    pub dir: PathBuf,
    /// Its own config that runs commands, and its git folder's hooks (`held.rs`).
    pub local: Arc<Local>,
    /// What every run puts in while those are held — None: none found, or trusted.
    pub held: Option<Arc<Hold>>,
    /// Not answered yet for what's found now: the card asks (`What::GitTrust`).
    pub asks: bool,
    /// The files that decided `local` and their stamps (`Kept::get` resolves it again when one changed).
    pub stamps: Vec<(PathBuf, Stamp)>,
    /// Resolved within `RACY` of a change to one of those files: not kept (`Kept::get`) — a change after
    /// its stamp could leave the stamp as it was.
    pub racy: bool,
    /// Its submodules checked out — each resolved as a repository of its own (`Repo::submodules`).
    pub subs: Vec<Sub>,
    /// Its submodules' git folders in its own `modules/` that aren't checked out now: a fetch fetches into
    /// them, a checkout of a commit that has them checks them out — read as the checked-out ones are.
    pub dormant: Vec<Arc<Repo>>,
}

/// A submodule checked out in a repository's work tree: a gitlink in its index with a `.git` there — what
/// git looks into from the repository around it (a status runs one in it; a fetch, push or checkout may
/// recurse), its own config and all. While its commands are held (`held`, or held in one below it), every
/// run of the repository around it keeps out of it (`Hold::keep_out`); its changes don't show until then.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sub {
    /// From the work tree's top.
    pub rel: String,
    pub repo: Arc<Repo>,
}

/// A file's change times (`ctime`, seconds and nanoseconds), its own and — a link — what it leads to's: what
/// its text, its mode and a file put in its place all move (a `chmod +x` leaves the modification time as
/// it was). None: not there.
pub type Stamp = Option<[(i64, i64); 2]>;

fn stamp(p: &Path) -> Stamp {
    use std::os::unix::fs::MetadataExt as _;
    let own = std::fs::symlink_metadata(p).ok()?;
    let to = std::fs::metadata(p).unwrap_or_else(|_| own.clone());
    let s = [(own.ctime(), own.ctime_nsec()), (to.ctime(), to.ctime_nsec())];
    #[cfg(test)]
    let s = coarse(p, s);
    Some(s)
}

/// How soon after a change to what decides a repository its resolve isn't kept (`Repo::racy`) — git's
/// "racy" files: a file's time moves coarsely (Linux's by the kernel's tick, a few ms; some file systems'
/// by the second), so a change in the tick its stamp was taken in leaves the stamp as it was — a resolve
/// that read the file before the change, finishing after it, would be kept with what it read (seen on
/// Linux: a worker's resolve between a hook's write and its `chmod`).
const RACY: std::time::Duration = std::time::Duration::from_secs(1);

/// Whether `s` was changed within `RACY` before `at`.
fn racy(s: &Stamp, at: std::time::SystemTime) -> bool {
    let changed = |(sec, ns): (i64, i64)| {
        std::time::UNIX_EPOCH + std::time::Duration::new(sec.max(0) as u64, ns.clamp(0, 999_999_999) as u32)
    };
    s.is_some_and(|times| times.into_iter().any(|t| changed(t) + RACY > at))
}

/// Tests: stamps of files under these folders in whole seconds — a file system whose times move by the
/// second (Linux's move by the tick): a change in the same second leaves a stamp as it was.
#[cfg(test)]
static COARSE: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

#[cfg(test)]
pub fn coarse_times_under(under: &Path) {
    COARSE.lock().unwrap_or_else(|e| e.into_inner()).push(under.to_path_buf());
}

#[cfg(test)]
fn coarse(p: &Path, s: [(i64, i64); 2]) -> [(i64, i64); 2] {
    match COARSE.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|u| p.starts_with(u)) {
        true => s.map(|(sec, _)| (sec, 0)),
        false => s,
    }
}

/// `files` with their stamps now.
fn stamped(files: impl IntoIterator<Item = PathBuf>) -> Vec<(PathBuf, Stamp)> {
    files
        .into_iter()
        .map(|f| {
            let t = stamp(&f);
            (f, t)
        })
        .collect()
}

/// Where answers about repositories' own commands are kept (`trust.rs`'s file) — set once at startup; never in
/// tests (nothing read: everything asks).
static TRUST_STORE: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Answers given since tarae started, newest last: (the repository's config file, what was found, trusted) —
/// before the store's file is written, and in tests.
static ANSWERS: Mutex<Vec<(PathBuf, Digest, bool)>> = Mutex::new(Vec::new());

/// The trust store's file (`trust::Store::path`) — `main` sets it.
pub fn set_trust_store(path: Option<PathBuf>) {
    *TRUST_STORE.lock().unwrap_or_else(|e| e.into_inner()) = path;
}

/// The answer for what a repository (its `config` file) runs now (`digest`).
fn trust_state(config: &Path, digest: Digest) -> State {
    // (being taken back: held from the `d` on — an answer since tarae started too)
    if crate::trust::is_taking(&crate::trust::place_of(config)) {
        return State::Ask;
    }
    let answers = ANSWERS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, _, t)) = answers.iter().rev().find(|(c, d, _)| c == config && *d == digest) {
        return if *t { State::Trusted } else { State::Ignored };
    }
    drop(answers);
    let store = TRUST_STORE.lock().unwrap_or_else(|e| e.into_inner()).clone();
    match store {
        Some(path) => crate::trust::Store::read(Some(&path)).state(config, digest),
        None => State::Ask,
    }
}

/// Tests: `trust_state`.
#[cfg(test)]
pub fn trust_state_of(config: &Path, digest: Digest) -> State {
    trust_state(config, digest)
}

/// The card answered for a repository (its `config` file): kept for this run of tarae (the editor writes the
/// store), and its kept entries resolved again — trusted, nothing held.
pub fn answered(config: &Path, digest: Digest, trusted: bool) {
    ANSWERS.lock().unwrap_or_else(|e| e.into_inner()).push((config.to_path_buf(), digest, trusted));
    repos().entries.retain(|(_, r, _)| !r.about(config));
    submodules().entries.retain(|(_, r, _)| !r.about(config));
}

/// What a repository's config file at `place` runs taken back (`:granted`'s `d`, on a worker — after the store's
/// write): its answers since tarae started forgotten and its kept entries resolved again — held, the card asks.
pub fn forget_trust(place: &crate::trust::Place) {
    trust_changed();
    let of = |c: &Path| crate::trust::place_of(c) == *place;
    ANSWERS.lock().unwrap_or_else(|e| e.into_inner()).retain(|(c, ..)| !of(c));
    repos().entries.retain(|(_, r, _)| !r.about_place(place));
    submodules().entries.retain(|(_, r, _)| !r.about_place(place));
}

/// Whether what a repository (its `config` file) runs (`digest`) was answered since tarae started — a resolve
/// that asked, told before the answer and landing after it, puts no card up again. Reads no file.
pub fn answered_since(config: &Path, digest: Digest) -> bool {
    ANSWERS.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|(c, d, _)| c == config && *d == digest)
}

/// Where a resolve says it found commands waiting for an answer: (repositories under this folder, the events
/// of the editor whose card goes up) — `main` sets `/`, a test its own folder.
static ASKERS: Mutex<Vec<(PathBuf, crate::event::Tx)>> = Mutex::new(Vec::new());

/// A resolve of a repository under `under` that finds its commands unanswered tells `tx` (`tell`): the card
/// goes up from whichever git call resolved it — a status, a snapshot after a run, a blame — not only when
/// a checkout is first watched.
pub fn set_asker(under: &Path, tx: crate::event::Tx) {
    let mut askers = ASKERS.lock().unwrap_or_else(|e| e.into_inner());
    askers.retain(|(u, _)| u != under);
    askers.push((under.to_path_buf(), tx));
}

/// `r` just resolved (not one kept): itself and each submodule whose commands wait for an answer, told —
/// `Editor::git_trust_told` puts its card up unless it's up already.
fn tell(r: &Repo) {
    let asking = r.asking();
    if asking.is_empty() {
        return;
    }
    let askers = ASKERS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(tx) = askers.iter().rev().find(|(u, _)| r.dir.starts_with(u)).map(|(_, tx)| tx.clone()) else {
        return;
    };
    drop(askers);
    for a in asking {
        let _ = tx.send(crate::event::Event::Job(Box::new(move |ed: &mut crate::editor::Editor| {
            ed.git_trust_told(&a)
        })));
    }
}

/// Tests: what runs right after a repository's config was listed (by its git folder), once — a change that
/// lands while it's resolved.
#[cfg(test)]
type AfterListing = (PathBuf, Box<dyn FnOnce() + Send>);

#[cfg(test)]
static AFTER_LISTING: Mutex<Vec<AfterListing>> = Mutex::new(Vec::new());

#[cfg(test)]
pub fn after_listing(git_dir: &Path, f: impl FnOnce() + Send + 'static) {
    AFTER_LISTING.lock().unwrap_or_else(|e| e.into_inner()).push((git_dir.to_path_buf(), Box::new(f)));
}

#[cfg(test)]
fn after_listing_run(git_dir: &Path) {
    let f = {
        let mut all = AFTER_LISTING.lock().unwrap_or_else(|e| e.into_inner());
        all.iter().position(|(d, _)| d == git_dir).map(|i| all.remove(i).1)
    };
    if let Some(f) = f {
        f();
    }
}

/// Roots kept at most (the least lately used goes first).
const KEPT_MAX: usize = 64;

/// Each root's repository once resolved, by the root's real path, the last used at the end. A failure isn't
/// kept: a `git init` there later is seen.
struct Kept {
    cap: usize,
    entries: Vec<(PathBuf, Arc<Repo>, u64)>,
    /// What a resolve's trust answers are as of (`trust_gen`; a test's own).
    epoch_now: fn() -> u64,
}

/// Bumped as a repository's trust is taken back (`:granted`'s `d` — on the main thread, before the worker that
/// forgets its answers): a resolve under way across it isn't kept, and one kept from before isn't used.
static TRUST_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn trust_gen() -> u64 {
    TRUST_GEN.load(std::sync::atomic::Ordering::SeqCst)
}

/// A repository's trust taken back: what's kept resolved is resolved again, and a resolve under way isn't kept.
pub fn trust_changed() {
    TRUST_GEN.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

impl Kept {
    /// `root`'s repository while it still holds: its git folder there, and no `.git` of the root's own that
    /// isn't it (a folder first resolved inside an outer repository, `git init`ed since) — else dropped.
    fn get(&mut self, root: &Path) -> Option<Arc<Repo>> {
        let i = self.entries.iter().position(|(k, ..)| k == root)?;
        let (r, epoch) = (&self.entries[i].1, self.entries[i].2);
        let own = std::fs::symlink_metadata(root.join(".git")).is_ok();
        let changed = r.racy || r.stamps.iter().any(|(p, t)| stamp(p) != *t) || epoch != (self.epoch_now)();
        if !r.git_dir.is_dir() || (own && r.work_tree.as_deref() != Some(root)) || changed {
            self.entries.remove(i);
            return None;
        }
        let e = self.entries.remove(i);
        let r = e.1.clone();
        self.entries.push(e);
        Some(r)
    }

    /// `r`, resolved from `epoch` on, kept for `root` — unless a trust was taken back meanwhile (false: resolve it
    /// again, it may have read an answer forgotten since).
    fn put(&mut self, root: PathBuf, r: Arc<Repo>, epoch: u64) -> bool {
        if epoch != (self.epoch_now)() {
            return false;
        }
        self.entries.retain(|(k, ..)| *k != root);
        self.entries.push((root, r, epoch));
        let over = self.entries.len().saturating_sub(self.cap);
        self.entries.drain(..over);
        true
    }
}

static REPOS: Mutex<Kept> = Mutex::new(Kept { cap: KEPT_MAX, entries: Vec::new(), epoch_now: trust_gen });

fn repos() -> std::sync::MutexGuard<'static, Kept> {
    REPOS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Submodules once resolved (`submodule`), by their top's real path — apart from the roots: one is taken
/// only as the repository around it records it.
static SUBMODULES: Mutex<Kept> =
    Mutex::new(Kept { cap: KEPT_MAX, entries: Vec::new(), epoch_now: trust_gen });

fn submodules() -> std::sync::MutexGuard<'static, Kept> {
    SUBMODULES.lock().unwrap_or_else(|e| e.into_inner())
}

/// The repository of `root` — a folder tarae trusts, never a file's folder (`for_file`) — resolved the first
/// time and kept (`Kept`). Runs git the first time: on a worker.
pub fn repo(root: &Path) -> Result<Arc<Repo>, String> {
    let real = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    if let Some(r) = repos().get(&real) {
        return Ok(r);
    }
    loop {
        let epoch = trust_gen();
        let r = Arc::new(resolve_with(&real, None)?);
        if repos().put(real.clone(), r.clone(), epoch) {
            tell(&r);
            return Ok(r);
        }
    }
}

/// `repo` when it was resolved already — for the main loop, which doesn't run git: `root` as given (a
/// session's root is its real path), else not known yet (`repo` on a worker).
pub fn cached(root: &Path) -> Option<Arc<Repo>> {
    repos().get(root)
}

/// Tests: `root`'s repository not kept any more (the next `repo` resolves it again).
#[cfg(test)]
pub fn forget(root: &Path) {
    let real = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    repos().entries.retain(|(k, ..)| *k != real);
}

/// The one discovery: from `root`, bare-looking folders refused — and with `ceilings`, never climbing into
/// one of them (`GIT_CEILING_DIRECTORIES`).
fn resolve_with(root: &Path, ceilings: Option<&std::ffi::OsStr>) -> Result<Repo, String> {
    let args = [
        "rev-parse",
        "--path-format=absolute",
        "--git-dir",
        "--git-common-dir",
        "--is-inside-work-tree",
        "--show-cdup",
    ];
    #[cfg(test)]
    not_in_this_repository(root, &args);
    let mut cmd = base(root);
    if let Some(c) = ceilings {
        cmd.env("GIT_CEILING_DIRECTORIES", c);
    }
    cmd.args(SAFE).args(args);
    let out = String::from_utf8_lossy(&finish(&cmd, None)?).into_owned();
    let mut lines = out.lines();
    let (Some(git_dir), Some(common), Some(inside)) = (lines.next(), lines.next(), lines.next()) else {
        return Err(format!("git named no repository for {}", root.display()));
    };
    let real = |p: PathBuf| std::fs::canonicalize(&p).unwrap_or(p);
    let (git_dir, common_dir, here) = (real(git_dir.into()), real(common.into()), real(root.to_path_buf()));
    // (`--show-cdup`: the way up from `root` to the top, empty at the top)
    let work_tree = (inside == "true").then(|| real(here.join(lines.next().unwrap_or_default())));
    let dir = match &work_tree {
        Some(top) if here.starts_with(top) => here,
        Some(top) => top.clone(),
        None => git_dir.clone(),
    };
    Ok(Repo::read_local(git_dir, common_dir, work_tree, dir, 0))
}

impl Repo {
    /// A repository's folders with its own command-running config read (`held::find`: one `git config
    /// --get-regexp` of the guarded keys and the includes, which runs nothing — measured), held unless
    /// trusted for exactly that — and with a work tree, its submodules (`submodules`, `depth` deep already),
    /// kept out of while one's held.
    fn read_local(
        git_dir: PathBuf,
        common_dir: PathBuf,
        work_tree: Option<PathBuf>,
        dir: PathBuf,
        depth: usize,
    ) -> Repo {
        let mut r = Repo {
            git_dir,
            common_dir,
            work_tree,
            dir,
            local: Arc::default(),
            held: None,
            asks: false,
            stamps: Vec::new(),
            racy: false,
            subs: Vec::new(),
            dormant: Vec::new(),
        };
        let at = std::time::SystemTime::now();
        // (every file stamped before it's read, as `submodule`'s: a change while it's read — a run writing the
        // config, an include, a hook — is a change the next run sees. What the listing names besides — an
        // include's file, `HEAD` for an `onbranch:` — is known only after it: listed again with those stamped
        // first, until it names nothing new; the hooks `find` reads, stamped there before each is looked at)
        let mut known = vec![r.config_file(), r.git_dir.join("config.worktree"), r.common_dir.join("hooks")];
        let mut listings = 0;
        let (local, mut stamps) = loop {
            let mut stamps = stamped(known.clone());
            let local = r.listing();
            listings += 1;
            stamps.extend(local.stamped.iter().cloned());
            let new: Vec<PathBuf> =
                local.files.iter().filter(|f| !stamps.iter().any(|(p, _)| p == *f)).cloned().collect();
            if new.is_empty() {
                break (local, stamps);
            }
            // (an include chain changing as it's read: what it names now, stamped after)
            if listings == LISTINGS_MAX {
                stamps.extend(stamped(new));
                break (local, stamps);
            }
            known.extend(new);
        };
        if !local.found.is_empty() {
            let state = trust_state(&r.config_file(), local.digest());
            let trusted = state == State::Trusted && local.showable();
            r.held = (!trusted).then(|| Arc::new(local.hold.clone()));
            r.asks = state == State::Ask;
        }
        r.local = Arc::new(local);
        // (a submodule not checked out has no work tree, but its `modules/` the git folders of its own
        // submodules: a recursive checkout that brings it back checks those out too)
        if (r.work_tree.is_some() || depth > 0) && depth < DEPTH_MAX {
            let (subs, watched) = r.submodules(depth);
            let (dormant, folders) = r.dormant(&subs, depth);
            stamps.extend(watched.into_iter().chain(folders));
            (r.subs, r.dormant) = (subs, dormant);
            let out: Vec<String> =
                r.subs.iter().filter(|s| s.repo.held.is_some()).map(|s| s.rel.clone()).collect();
            if !out.is_empty() || r.dormant.iter().any(|d| d.held.is_some()) {
                let mut h = r.held.as_deref().cloned().unwrap_or_default();
                // (one not checked out runs nothing itself: held, it's what the repository around it keeps
                // out of — no `.gitmodules` of its own to read)
                let names = if r.work_tree.is_some() { r.gitmodules_names() } else { Vec::new() };
                h.keep_out(&names, &out);
                r.held = Some(Arc::new(h));
            }
        }
        let below: Vec<(PathBuf, Stamp)> = r.below().flat_map(|b| b.stamps.clone()).collect();
        stamps.extend(below);
        r.racy = stamps.iter().any(|(_, s)| racy(s, at));
        r.stamps = stamps;
        r
    }

    /// Its own command-running config as git lists it (`held::find`: one `git config --get-regexp` of the
    /// guarded keys and the includes) and its git folder's hooks.
    fn listing(&self) -> Local {
        let args = held::listing_args();
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        #[cfg(test)]
        not_in_this_repository(&self.dir, &args);
        let mut cmd = base(&self.dir);
        cmd.arg("--git-dir").arg(&self.git_dir);
        // (a git folder alone: a submodule's not checked out names a work tree that isn't there — git stops
        // before reading its config, measured — so one that is)
        if self.work_tree.is_none() {
            cmd.arg("--work-tree").arg(&self.git_dir);
        }
        cmd.args(SAFE).args(&args);
        // (`--get-regexp` exits 1 when no key matches: what it printed either way. No listing: nothing found —
        // and nothing that can run, since git can't read it either)
        let listing =
            ran(&cmd, None).map(|r| String::from_utf8_lossy(&r.stdout).into_owned()).unwrap_or_default();
        #[cfg(test)]
        after_listing_run(&self.git_dir);
        // (the files named as git read them: with `--git-dir` given absolute, absolute — a run that found its
        // repository says `file:.git/config`, measured; `the_users_own_values_come_back` holds this)
        let mut local =
            held::find(&held::parse(&listing), &self.common_dir.join("hooks"), &self.git_dir.join("HEAD"));
        // (git runs its commands from the work tree's top — it goes there first — else in the git folder)
        let runs_in = self.work_tree.as_deref().unwrap_or(&self.git_dir);
        // (whose hooks card looks at a hooks folder: the checkout's — the git folder named alone, its main
        // checkout's, around its `.git`: it agrees with that checkout, and runs no hooks itself)
        let main = (self.git_dir == self.common_dir && self.git_dir.file_name() == Some(".git".as_ref()))
            .then(|| self.git_dir.parent())
            .flatten();
        let checkout = self.work_tree.as_deref().or(main);
        held::cover(&mut local, runs_in, checkout, &[&self.git_dir, &self.common_dir]);
        local
    }

    /// Its submodules' repositories, checked out or not (one level).
    fn below(&self) -> impl Iterator<Item = &Arc<Repo>> {
        self.subs.iter().map(|s| &s.repo).chain(&self.dormant)
    }

    /// The git folders in its `modules/` (the checkout's own: a linked checkout keeps its submodules' there)
    /// that none of `subs` uses — a folder holding `HEAD` and `config` is one, any other is looked into (a
    /// submodule's name can hold a `/`) — and the folders looked into, to watch for one coming (stamped before
    /// each is read).
    fn dormant(&self, subs: &[Sub], depth: usize) -> (Vec<Arc<Repo>>, Vec<(PathBuf, Stamp)>) {
        let (mut found, mut folders) = (Vec::new(), Vec::new());
        let mut todo = vec![self.git_dir.join("modules")];
        while let Some(dir) = todo.pop() {
            folders.push((dir.clone(), stamp(&dir)));
            for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let p = e.path();
                if !e.file_type().is_ok_and(|t| t.is_dir()) {
                    continue;
                }
                if !(p.join("HEAD").is_file() && p.join("config").is_file()) {
                    todo.push(p);
                    continue;
                }
                let Ok(p) = std::fs::canonicalize(&p) else { continue };
                if subs.iter().any(|s| s.repo.git_dir == p) {
                    continue;
                }
                found.push(Arc::new(Repo::read_local(p.clone(), p.clone(), None, p, depth + 1)));
            }
        }
        (found, folders)
    }

    /// Its submodules checked out (`Sub`): the gitlinks in its index (`ls-files --stage`, which runs nothing)
    /// with a `.git` there — a folder, or a file naming one — each read as a repository of its own; and the
    /// files that decide them, to watch: `HEAD` (a switch to a commit that has others), `.gitmodules` (what
    /// adding one, or a pull that brings one, changes), each gitlink's `.git` (there or not yet), and — in
    /// `dormant` — `modules/` (where a new one's git folder goes) — each stamped before it's read. Not the
    /// index: every stage and commit writes it, and each would resolve the repository again (25–45 ms of git,
    /// measured) for what those already catch — a gitlink comes with `.gitmodules`, and is looked into only
    /// once its `.git` is there.
    fn submodules(&self, depth: usize) -> (Vec<Sub>, Vec<(PathBuf, Stamp)>) {
        let Some(top) = self.work_tree.clone() else { return (Vec::new(), Vec::new()) };
        let mut watched = stamped([self.git_dir.join("HEAD"), top.join(".gitmodules")]);
        let listed =
            self.output(&["ls-files", "--stage", "-z", "--full-name", "--", ":/"], None).unwrap_or_default();
        let mut subs = Vec::new();
        // (`160000 <id> <stage>\t<path>`)
        for e in listed.split(|&b| b == 0) {
            let Some(rel) = e.strip_prefix(b"160000 ").and_then(|e| e.splitn(2, |&b| b == b'\t').nth(1))
            else {
                continue;
            };
            let Ok(rel) = std::str::from_utf8(rel) else { continue };
            let (at, dot_git) = (top.join(rel), top.join(rel).join(".git"));
            watched.push((dot_git.clone(), stamp(&dot_git)));
            let git_dir = match std::fs::symlink_metadata(&dot_git) {
                Ok(m) if m.is_dir() => dot_git,
                Ok(_) => {
                    let named = std::fs::read_to_string(&dot_git).unwrap_or_default();
                    let Some(to) = named.lines().find_map(|l| l.strip_prefix("gitdir:")) else { continue };
                    at.join(to.trim())
                }
                Err(_) => continue,
            };
            let (Ok(git_dir), Ok(sub_top)) = (std::fs::canonicalize(&git_dir), std::fs::canonicalize(&at))
            else {
                continue;
            };
            // (a linked checkout's git folder names its common one)
            let common = std::fs::read_to_string(git_dir.join("commondir"))
                .ok()
                .and_then(|c| std::fs::canonicalize(git_dir.join(c.trim())).ok())
                .unwrap_or_else(|| git_dir.clone());
            let repo = Repo::read_local(git_dir, common, Some(sub_top.clone()), sub_top, depth + 1);
            subs.push(Sub { rel: rel.to_string(), repo: Arc::new(repo) });
        }
        (subs, watched)
    }

    /// The submodules' names `.gitmodules` gives as git reads it: the work tree's file, else the index's,
    /// else HEAD's (`git config`, which runs nothing).
    fn gitmodules_names(&self) -> Vec<String> {
        let top = self.work_tree.clone().unwrap_or_default();
        let file = top.join(".gitmodules");
        let from: Vec<Vec<String>> = match std::fs::symlink_metadata(&file) {
            Ok(_) => vec![vec!["--file".into(), file.to_string_lossy().into_owned()]],
            Err(_) => [":.gitmodules", "HEAD:.gitmodules"].map(|b| vec!["--blob".into(), b.into()]).into(),
        };
        for source in from {
            let mut args = vec!["config", "-z"];
            args.extend(source.iter().map(String::as_str));
            args.extend(["--get-regexp", r"^submodule\..+\.path$"]);
            let Ok(r) = ran(&self.command(&args), None) else { continue };
            if !r.ok && r.stdout.is_empty() && !r.stderr.is_empty() {
                continue; // (not there)
            }
            // (`key LF value NUL`; the name as written, between `submodule.` and `.path`)
            return String::from_utf8_lossy(&r.stdout)
                .split('\0')
                .filter_map(|e| e.split_once('\n').map_or(Some(e), |(k, _)| Some(k)))
                .filter_map(|k| k.strip_prefix("submodule.")?.strip_suffix(".path"))
                .map(str::to_string)
                .collect();
        }
        Vec::new()
    }

    /// Whether `config` is its own config file, or one of its submodules' (any depth, checked out or not).
    fn about(&self, config: &Path) -> bool {
        self.config_file() == config || self.below().any(|b| b.about(config))
    }

    /// `about` for a trust answer's place (`trust::place_of` its config file's).
    fn about_place(&self, place: &crate::trust::Place) -> bool {
        crate::trust::place_of(&self.config_file()) == *place || self.below().any(|b| b.about_place(place))
    }

    /// Itself and its submodules, any depth, checked out or not — the ones whose own commands wait for a
    /// card (`asks`).
    pub fn asking(&self) -> Vec<Arc<Repo>> {
        let mut out: Vec<Arc<Repo>> = Vec::new();
        if self.asks {
            out.push(Arc::new(self.clone()));
        }
        for b in self.below() {
            out.extend(b.asking());
        }
        out
    }

    /// The file answers about its own commands are kept by: its common folder's `config`.
    pub fn config_file(&self) -> PathBuf {
        self.common_dir.join("config")
    }

    /// `args` on `cmd` — with what's held put in first (`-c`, the environment) and its flags after the
    /// subcommand (`Hold::args`).
    fn hold_on(&self, cmd: &mut Command, args: &[&str]) {
        let Some(h) = self.held.as_deref() else {
            cmd.args(args);
            return;
        };
        for (arg, var, value) in h.settings() {
            cmd.arg(arg).env(var, value);
        }
        for (var, value) in &h.env {
            cmd.env(var, value);
        }
        cmd.args(h.args(args));
    }
}

/// git in `dir`: no prompt, no optional lock (a read — status, diff — never takes the index lock: a commit
/// or stage running then would fail on it).
fn base(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).env("GIT_TERMINAL_PROMPT", "0").env("GIT_OPTIONAL_LOCKS", "0");
    // (tests never read the machine's own config — it's the user's, and can hold tokens: a stand-in instead —
    // nor its own ignore and attributes, read from `$XDG_CONFIG_HOME/git` or `~/.config/git` with no config at
    // all: under `/dev/null` they can't be)
    #[cfg(test)]
    cmd.env("GIT_CONFIG_GLOBAL", test_global(dir))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("XDG_CONFIG_HOME", "/dev/null");
    #[cfg(test)]
    for (k, v) in test_env(dir) {
        cmd.env(k, v);
    }
    cmd
}

/// Tests: the environment tarae runs in, as git sees it, for runs in folders under a test's own (`env_for`).
#[cfg(test)]
type Env = Vec<(String, String)>;

#[cfg(test)]
static ENVS: Mutex<Vec<(PathBuf, Env)>> = Mutex::new(Vec::new());

#[cfg(test)]
fn test_env(dir: &Path) -> Env {
    let envs = ENVS.lock().unwrap_or_else(|e| e.into_inner());
    envs.iter().find(|(u, _)| dir.starts_with(u)).map(|(_, v)| v.clone()).unwrap_or_default()
}

#[cfg(test)]
pub fn env_for(under: &Path, vars: &[(&str, &str)]) {
    let vars = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    ENVS.lock().unwrap_or_else(|e| e.into_inner()).push((under.to_path_buf(), vars));
}

/// Tests: the user-level config git reads for runs in folders under a test's own (`GIT_CONFIG_GLOBAL` on
/// those runs only — never the test binary's environment — and no system config, nor git's built-in one).
#[cfg(test)]
static GLOBALS: Mutex<Vec<(PathBuf, PathBuf)>> = Mutex::new(Vec::new());

/// Tests: the stand-in user config for a run in `dir` — a test's own (`global_config_for`), else an empty
/// file of the test binary's in the temp folder.
#[cfg(test)]
fn test_global(dir: &Path) -> PathBuf {
    static EMPTY: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    let own =
        GLOBALS.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(u, _)| dir.starts_with(u)).cloned();
    own.map(|(_, f)| f).unwrap_or_else(|| {
        EMPTY
            .get_or_init(|| {
                let f = std::env::temp_dir().join(format!("tarae-test-gitconfig-{}", std::process::id()));
                std::fs::write(&f, "").ok();
                f
            })
            .clone()
    })
}

#[cfg(test)]
pub fn global_config_for(under: &Path, file: &Path) {
    GLOBALS.lock().unwrap_or_else(|e| e.into_inner()).push((under.to_path_buf(), file.to_path_buf()));
}

/// What a git run gave.
pub struct Ran {
    pub ok: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Runs `cmd` (a `Repo::command`) with no terminal of its own — ssh or gpg asking for a passphrase can't
/// take ours, it fails instead — and without forking the editor (`spawn::setsid`: a fork costs with the
/// editor's memory), `input` on its stdin, its stdin `/dev/null` else. On a worker.
pub fn ran(cmd: &Command, input: Option<&[u8]>) -> Result<Ran, String> {
    use std::io::{Read as _, Write as _};
    let said = |e: std::io::Error| format!("git: {e}");
    let (s, feed) = match input {
        Some(bytes) => {
            let (s, mut w) = crate::spawn::setsid_fed(cmd).map_err(said)?;
            let bytes = bytes.to_vec();
            // (from a thread of its own: a big answer can't block the asking)
            let feed = crate::event::thread("tarae-git-feed", move || {
                let _ = w.write_all(&bytes); // (then closed: the end of its input)
            });
            (s, Some(feed))
        }
        None => (crate::spawn::setsid_git(cmd).map_err(said)?, None),
    };
    // (stderr read beside stdout: a full pipe of warnings can't hold git up)
    let errs = s.stderr.map(|mut e| {
        crate::event::thread("tarae-git-err", move || {
            let mut v = Vec::new();
            let _ = e.read_to_end(&mut v);
            v
        })
    });
    let mut stdout = Vec::new();
    let read = s.stdout.map_or(Ok(0), |mut o| o.read_to_end(&mut stdout));
    let stderr = errs.and_then(|t| t.join().ok()).unwrap_or_default();
    let status = crate::spawn::wait(s.pid).map_err(said)?;
    if let Some(f) = feed {
        let _ = f.join();
    }
    read.map_err(said)?;
    Ok(Ran { ok: status.success(), stdout, stderr })
}

/// `ran`: its stdout, else the first line it said why.
pub fn finish(cmd: &Command, input: Option<&[u8]>) -> Result<Vec<u8>, String> {
    let r = ran(cmd, input)?;
    if !r.ok {
        let err = String::from_utf8_lossy(&r.stderr);
        return Err(err.lines().find(|l| !l.trim().is_empty()).unwrap_or("git failed").to_string());
    }
    Ok(r.stdout)
}

impl Repo {
    /// A git folder named — the common one a checkout's `repo` gave, for what outlives the checkout (its
    /// removal, its branch's): no work tree, nothing discovered; its own config read and held as a checkout's
    /// (runs git: on a worker).
    pub fn git_folder(dir: &Path) -> Repo {
        let d = dir.to_path_buf();
        let r = Repo::read_local(d.clone(), d.clone(), None, d, 0);
        tell(&r);
        r
    }

    /// git on this repository: its folders given, run in `dir` — to run with `ran` or `finish` (an
    /// environment variable added first, say).
    pub fn command(&self, args: &[&str]) -> Command {
        #[cfg(test)]
        not_in_this_repository(&self.dir, args);
        let mut cmd = base(&self.dir);
        cmd.arg("--git-dir").arg(&self.git_dir);
        if let Some(top) = &self.work_tree {
            cmd.arg("--work-tree").arg(top);
        }
        cmd.args(SAFE);
        self.hold_on(&mut cmd, args);
        cmd
    }

    /// git for an action that runs the user's own hooks (`hooked`): from the work tree's top, the repository
    /// found there — the top's own `.git` comes first — not named; the climb stopped at the top
    /// (`GIT_CEILING_DIRECTORIES` = its parent), a bare look-alike refused. No `GIT_DIR` or `GIT_WORK_TREE`
    /// from tarae's own environment either.
    pub fn hooked_command(&self, args: &[&str]) -> Result<Command, String> {
        let top = self.work_tree.as_deref().ok_or("not a checkout")?;
        #[cfg(test)]
        not_in_this_repository(top, args);
        let mut cmd = base(top);
        if let Some(parent) = top.parent() {
            cmd.env("GIT_CEILING_DIRECTORIES", parent);
        }
        cmd.env_remove("GIT_DIR").env_remove("GIT_WORK_TREE").args(SAFE);
        self.hold_on(&mut cmd, args);
        Ok(cmd)
    }

    /// Its stdout as it came.
    pub fn output(&self, args: &[&str], input: Option<&str>) -> Result<Vec<u8>, String> {
        finish(&self.command(args), input.map(str::as_bytes))
    }

    /// Its stdout as text.
    pub fn run(&self, args: &[&str], input: Option<&str>) -> Result<String, String> {
        self.output(args, input).map(|b| String::from_utf8_lossy(&b).into_owned())
    }

    /// `run` with `NO_HOOKS`.
    pub fn run_no_hooks(&self, args: &[&str]) -> Result<String, String> {
        let all: Vec<&str> = NO_HOOKS.iter().chain(args).copied().collect();
        self.run(&all, None)
    }

    /// The start of a shell line that runs git on it (the rebase's, in the task panel), and the environment
    /// it runs with: no fetch (`NO_LAZY_FETCH`), and what's held as on every run (`Hold::for_line`) — names in
    /// the line (`--config-env`), values only in the environment: the line is shown and logged, and a value
    /// can be the user's own put back (a credential helper's, say).
    pub fn shell_line(&self) -> Line {
        let q = |p: &Path| crate::packs::sh_quote(&p.to_string_lossy());
        let mut text = format!("git --git-dir {}", q(&self.git_dir));
        if let Some(top) = &self.work_tree {
            text += &format!(" --work-tree {}", q(top));
        }
        text = text + " " + &SAFE.join(" ");
        let mut env = vec![(NO_LAZY_FETCH.0.to_string(), NO_LAZY_FETCH.1.to_string())];
        if let Some(h) = self.held.as_deref().map(Hold::for_line) {
            for (arg, var, value) in h.settings() {
                text += &format!(" {}", crate::packs::sh_quote(&arg));
                env.push((var, value.to_string()));
            }
            env.extend(h.env.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        }
        Line { text, env }
    }
}

/// On the rebase's git (`Repo::shell_line`): a partial clone's missing object isn't fetched from its remote
/// on demand — the rebase reaches no remote (its credential helpers, ssh, a proxy) — it fails instead, in
/// git's own words, which the rebase's end says (git 2.45+).
pub const NO_LAZY_FETCH: (&str, &str) = ("GIT_NO_LAZY_FETCH", "1");

/// A shell line of git's and the environment it runs with (`Repo::shell_line`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub text: String,
    pub env: Vec<(String, String)>,
}

/// git on `root`'s repository (`repo`) — the call most of tarae makes.
pub(crate) fn git(root: &Path, args: &[&str], input: Option<&str>) -> Result<String, String> {
    repo(root)?.run(args, input)
}

/// git for a git-tab action that runs the user's own hooks — commit, amend, push, pull, fetch, switch, stash,
/// branch work (`hooks::with`'s): `Repo::hooked_command` — the hooks get no `GIT_DIR`/`GIT_WORK_TREE` (see the
/// module's doc).
pub(crate) fn hooked(root: &Path, args: &[&str]) -> Result<String, String> {
    let cmd = repo(root)?.hooked_command(args)?;
    finish(&cmd, None).map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// `git` with `NO_HOOKS`.
pub(crate) fn git_no_hooks(root: &Path, args: &[&str]) -> Result<String, String> {
    repo(root)?.run_no_hooks(args)
}

/// The repository a file's git runs on, and the file's path from its work tree (git runs at the top): `root`'s
/// (a session's root, never the file's folder), or the submodule of it the file is in. None: outside its
/// work tree, or no repository.
pub fn for_file(root: &Path, path: &Path) -> Option<(Arc<Repo>, String)> {
    let r = repo(root).ok()?;
    let top = r.work_tree.clone()?;
    let real;
    let rel = match path.strip_prefix(&top) {
        Ok(rel) => rel,
        Err(_) => {
            real = std::fs::canonicalize(path).ok()?;
            real.strip_prefix(&top).ok()?
        }
    };
    let at_top = if r.dir == top { r.clone() } else { Arc::new(Repo { dir: top, ..(*r).clone() }) };
    within(at_top, rel, 0)
}

/// Why a file gets no git.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoGit {
    /// In no repository tarae reads.
    None,
    /// In no session's repository, where a sandboxed run can write: git isn't read there. `looks`: something
    /// there passes for a repository (a `.git`; `HEAD`, `objects/` and `refs/`) — the user is told why.
    Writable { looks: bool },
}

/// What's said — the status line, once per file (the gutter), or as the error of what was asked — when git
/// isn't read for a file (`NoGit::Writable`).
pub const NOT_READ: &str =
    "not in this session's repository — tarae doesn't read a git folder from where commands can write";

impl NoGit {
    /// Why, in words.
    pub fn said(self) -> &'static str {
        match self {
            NoGit::None => "not in a git repository",
            NoGit::Writable { .. } => NOT_READ,
        }
    }
}

/// Where a sandboxed run may write (`sandbox::write_places`, each session's) — None: runs aren't sandboxed (one
/// can write anywhere and run git itself, so nothing is held back: a file's own repository is found from its
/// folder). Asked on a worker, only for a file in no session's repository.
pub type Writes = Arc<dyn Fn() -> Option<Vec<PathBuf>> + Send + Sync>;

/// Where a file's git is looked for — made on the main loop (`Editor::git_places`), used on a worker.
#[derive(Clone)]
pub struct Places {
    /// The sessions' roots, the one whose checkout holds the file first.
    pub roots: Vec<PathBuf>,
    pub writes: Writes,
}

impl Places {
    /// Tests: `root`'s repository — and runs not sandboxed.
    #[cfg(test)]
    pub fn only(root: &Path) -> Places {
        Places { roots: vec![root.to_path_buf()], writes: Arc::new(|| None) }
    }
}

/// The repository a file's git runs on, and the file's path from its top (`for_file`): a session's — else its
/// own, found from its folder (a bare look-alike refused), only when no sandboxed run can write that folder
/// (`Places::writes`) nor where its repository keeps its git folder; the climb stops at those places
/// (`GIT_CEILING_DIRECTORIES`). Runs not sandboxed: found from its folder wherever it is. On a worker.
pub fn for_file_in(places: &Places, path: &Path) -> Result<(Arc<Repo>, String), NoGit> {
    if let Some(found) = places.roots.iter().find_map(|root| for_file(root, path)) {
        return Ok(found);
    }
    let real = std::fs::canonicalize(path)
        .or_else(|_| {
            let (dir, name) = (path.parent().ok_or(())?, path.file_name().ok_or(())?);
            std::fs::canonicalize(dir).map(|d| d.join(name)).map_err(|_| ())
        })
        .map_err(|_| NoGit::None)?;
    let folder = real.parent().ok_or(NoGit::None)?;
    // (runs not sandboxed: one can write anywhere and run git itself — holding git back protects nothing)
    let Some(writes) = (places.writes)() else {
        return within_outside(&real, folder, &[]);
    };
    if let Some(w) = writes.iter().find(|w| folder.starts_with(w)) {
        return Err(NoGit::Writable { looks: looks_like_git(folder, w) });
    }
    within_outside(&real, folder, &writes)
}

/// `real` (in `folder`) in the repository around it, found from there (`outside`) — not one whose git folder
/// is in `writes`.
fn within_outside(real: &Path, folder: &Path, writes: &[PathBuf]) -> Result<(Arc<Repo>, String), NoGit> {
    let repo = outside(folder, writes).ok_or(NoGit::None)?;
    let writable = |p: &Path| writes.iter().any(|w| p.starts_with(w));
    if writable(&repo.git_dir) || writable(&repo.common_dir) {
        return Err(NoGit::Writable { looks: true });
    }
    let top = repo.work_tree.clone().ok_or(NoGit::None)?;
    let rel = real.strip_prefix(&top).map_err(|_| NoGit::None)?;
    let at_top = Arc::new(Repo { dir: top.clone(), ..(*repo).clone() });
    within(at_top, rel, 0).ok_or(NoGit::None)
}

/// The repository around `folder` (outside every write place), kept as a root's is (`Kept`); the climb
/// stopped at `writes` — none of them is looked in.
fn outside(folder: &Path, writes: &[PathBuf]) -> Option<Arc<Repo>> {
    if let Some(r) = repos().get(folder) {
        return Some(r);
    }
    let ceilings = std::env::join_paths(writes.iter().filter(|w| w.is_absolute())).ok()?;
    loop {
        let epoch = trust_gen();
        let r = Arc::new(resolve_with(folder, (!writes.is_empty()).then_some(ceilings.as_os_str())).ok()?);
        if repos().put(folder.to_path_buf(), r.clone(), epoch) {
            tell(&r);
            return Some(r);
        }
    }
}

/// Whether something from `folder` up to `top` passes for a git repository: a `.git`, or a git folder's own
/// layout (`HEAD`, `objects/`, `refs/`) — files only looked at, no git run.
fn looks_like_git(folder: &Path, top: &Path) -> bool {
    folder.ancestors().take_while(|d| d.starts_with(top)).any(|d| {
        std::fs::symlink_metadata(d.join(".git")).is_ok()
            || (d.join("HEAD").is_file() && d.join("objects").is_dir() && d.join("refs").is_dir())
    })
}

/// `rel` in `repo`'s work tree — down into a submodule on the way when there's one.
fn within(repo: Arc<Repo>, rel: &Path, depth: usize) -> Option<(Arc<Repo>, String)> {
    let top = repo.work_tree.clone()?;
    let parts: Vec<&std::ffi::OsStr> = rel
        .components()
        .map(|c| match c {
            Component::Normal(p) => Some(p),
            _ => None,
        })
        .collect::<Option<_>>()?;
    if depth < DEPTH_MAX {
        let mut at = top;
        for i in 0..parts.len().saturating_sub(1) {
            at.push(parts[i]);
            // (a folder with a `.git` in it: asked of the index — a stat each on the way down otherwise)
            if std::fs::symlink_metadata(at.join(".git")).is_ok() {
                let here: PathBuf = parts[..=i].iter().collect();
                if let Some(sub) = submodule(&repo, &at, here.to_str()?) {
                    let rest: PathBuf = parts[i + 1..].iter().collect();
                    return within(sub, &rest, depth + 1);
                }
            }
        }
    }
    Some((repo, rel.to_str()?.to_string()))
}

/// The submodule checked out at `at` (`rel` from `repo`'s top), as `repo` records it: a gitlink in its index
/// there, and a git folder under its own `modules/` — the one the checkout's `.git` names, taken only there.
/// Kept as a root's is (`SUBMODULES`), with what made it one stamped too: `repo`'s index and that `.git`.
fn submodule(repo: &Repo, at: &Path, rel: &str) -> Option<Arc<Repo>> {
    let top = std::fs::canonicalize(at).ok()?;
    if let Some(r) = submodules().get(&top) {
        return Some(r);
    }
    // (stamped before they're read: a change while they are is seen next time)
    let made = stamped([repo.git_dir.join("index"), at.join(".git")]);
    let spec = format!(":(top,literal){rel}");
    let listed = repo.run(&["ls-files", "--stage", "-z", "--", &spec], None).ok()?;
    // (`160000 <id> 0\t<path>`)
    let gitlink = listed
        .split('\0')
        .any(|e| e.starts_with("160000 ") && e.split_once('\t').is_some_and(|(_, p)| p == rel));
    if !gitlink {
        return None;
    }
    let named = std::fs::read_to_string(at.join(".git")).ok()?;
    let git_dir =
        std::fs::canonicalize(at.join(named.lines().find_map(|l| l.strip_prefix("gitdir:"))?.trim())).ok()?;
    let modules = std::fs::canonicalize(repo.common_dir.join("modules")).ok()?;
    if git_dir == modules || !git_dir.starts_with(&modules) || !git_dir.is_dir() {
        return None;
    }
    // (its own config, held by its own answer)
    let epoch = trust_gen();
    let mut sub = Repo::read_local(git_dir.clone(), git_dir.clone(), Some(top.clone()), top.clone(), 1);
    // (a trust taken back while it read: read again — it may have read an answer forgotten since)
    if epoch != trust_gen() {
        sub = Repo::read_local(git_dir.clone(), git_dir, Some(top.clone()), top.clone(), 1);
    }
    sub.stamps.extend(made);
    let sub = Arc::new(sub);
    submodules().put(top, sub.clone(), epoch);
    tell(&sub);
    Some(sub)
}

/// Tests only: whether git run in `dir` would work on tarae's own repository (the nearest `.git` up from it
/// is this crate's).
#[cfg(test)]
fn in_this_repository(dir: &Path) -> bool {
    let own = std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).unwrap();
    let dir = dir.ancestors().find_map(|d| std::fs::canonicalize(d).ok()).unwrap_or_default();
    dir.ancestors().find(|d| d.join(".git").exists()) == Some(own.as_path())
}

/// Tests only: no test runs git in tarae's own repository — even a read can write there (`git diff`
/// refreshes the index), and a sandboxed run reports it as a denial. One that does stops the whole run,
/// naming the command and where it came from (past libtest's capture: an abort loses what it captured).
#[cfg(test)]
pub(crate) fn not_in_this_repository(dir: &Path, args: &[&str]) {
    if in_this_repository(dir) {
        // (a job's worker is named after the test it ran for — `event::run`)
        let test = std::thread::current().name().unwrap_or("a test").to_string();
        let line = format!(
            "{test} ran `git {}` in tarae's own repository ({}) — give it a repository of its own in its \
             temp folder\n{}\n",
            args.join(" "),
            dir.display(),
            std::backtrace::Backtrace::force_capture()
        );
        let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
        std::process::abort();
    }
}

#[cfg(test)]
mod tests;
