//! One language server for every checkout of a repository (user decision 2026-10-04; measured in
//! `target/measure/ra-{measure,share,swap,open}-2026-10-04.md`).
//!
//! - Every checkout of the same repository (git's common folder — a session's worktree is one) on the same
//!   toolchain is a workspace folder of one server (a pack's `share`, else `editor.lsp-share`: rust-analyzer;
//!   its toolchain files, checks setting and reload progress are its pack's too). Four checkouts of
//!   this repository took ≈1.5 GB that way against ≈3.4 GB with a server each, and every session's server
//!   stays warm.
//! - Its folders change by a **swap**: a new server B for the new list loads beside the one in use, A. B is
//!   told every open file of its folders and every change (a shadow link: asked nothing, its diagnostics
//!   unheard, its checks off). The new folders' files are B's from its start. Once B is quiescent and agrees
//!   with A (the same definition at a cursor in each old folder), every file moves to B, A answers what it
//!   was asked and goes, and B's checks come on. Nobody waits: the others' slowest answer was 58–174 ms,
//!   against 10–12 s with no answers in a restart. Folders that come or go close together are one swap
//!   (`GATHER`). A folder whose toolchain changes leaves for a server of its own (`share_refetch`).
//! - Two servers side by side need room: B's peak is up to 1.7 × A. With memory pressure up, or less than
//!   twice that free, the swap is a restart instead (`decide`). Pressure rising during a swap ends it: B
//!   takes over at once, loaded or not. A server that never finished loading is restarted, not swapped.
//!
//! What the user sees: the status line's server part (`share_said`), `:procs` (`share_procs`), and a line
//! in the log for each swap or restart, saying why.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::document::{DocId, Document};
use crate::editor::Editor;
use crate::lsp::{self, Client, ClientId, MakeOptions};
use crate::lsp_editor::Kind;

const fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// How long a change of folders waits for more — sessions made (or ended) close together are one swap.
const GATHER: Duration = ms(if cfg!(test) { 30 } else { 1500 });
/// How soon B is asked again whether it agrees with A, and how many rounds before it takes over anyway.
const AGREE_AGAIN: Duration = ms(if cfg!(test) { 20 } else { 1000 });
const AGREE_TRIES: u8 = 10;
/// How long A gets to answer what it was asked once B has taken over, and then to answer its shutdown.
const A_FINISH: Duration = ms(if cfg!(test) { 300 } else { 3000 });
/// How often memory is read while two servers run side by side.
const WATCH_EVERY: Duration = ms(if cfg!(test) { 20 } else { 1000 });
const GB: u64 = 1 << 30;

// ── What shares ─────────────────────────────────────────────────────────

/// What a server is shared by: the repository (git's common folder, a real path) and the toolchain (the words
/// of the nearest of its pack's `share.toolchain` files — rust-analyzer's `rust-toolchain.toml` — empty for
/// none).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    pub repo: PathBuf,
    pub toolchain: String,
}

/// `root`'s key for a server whose toolchain is said in the files `toolchain`. None when it isn't in a git
/// checkout (its own server, as before). Disk: run on a worker.
pub fn key_of(root: &Path, toolchain: &[String]) -> Option<Key> {
    Some(Key { repo: common_dir(root)?, toolchain: toolchain_of(root, toolchain) })
}

/// The files that say server `c`'s toolchain (its pack's `share.toolchain`).
fn toolchain_files(c: &Client) -> Vec<String> {
    c.traits.share.as_ref().map(|s| s.toolchain.clone()).unwrap_or_default()
}

/// Git's common folder for the checkout `root` is in: its `.git` folder, or for a worktree (`.git` a file
/// naming its own folder) the one that folder's `commondir` names.
fn common_dir(root: &Path) -> Option<PathBuf> {
    let dot = root.ancestors().map(|d| d.join(".git")).find(|g| g.exists())?;
    let dir = if dot.is_dir() {
        dot
    } else {
        let text = std::fs::read_to_string(&dot).ok()?;
        let p = PathBuf::from(text.trim().strip_prefix("gitdir:")?.trim());
        if p.is_absolute() { p } else { dot.parent()?.join(p) }
    };
    let common = match std::fs::read_to_string(dir.join("commondir")) {
        Ok(c) if Path::new(c.trim()).is_absolute() => PathBuf::from(c.trim()),
        Ok(c) => dir.join(c.trim()),
        Err(_) => dir,
    };
    std::fs::canonicalize(common).ok()
}

/// The nearest of the toolchain files `names` up from `root` (the first of them in a folder), its words —
/// empty if none.
fn toolchain_of(root: &Path, names: &[String]) -> String {
    if names.is_empty() {
        return String::new();
    }
    for d in root.ancestors() {
        for name in names {
            if let Ok(t) = std::fs::read_to_string(d.join(name)) {
                return t.split_whitespace().collect::<Vec<_>>().join(" ");
            }
        }
    }
    String::new()
}

/// Whether `path` is another checkout's, in an answer of a server serving `roots` to the checkout at `mine`:
/// under one of its folders that isn't the asker's (a path under none — the standard library, a
/// dependency — is everyone's). Only `workspace/symbol` mixes checkouts (measured: nothing else did).
pub fn foreign(roots: &[PathBuf], mine: &Path, path: &Path) -> bool {
    if roots.len() < 2 {
        return false;
    }
    let own = |r: &PathBuf| r.starts_with(mine) || mine.starts_with(r);
    roots.iter().any(|r| !own(r) && path.starts_with(r))
        && !roots.iter().any(|r| own(r) && path.starts_with(r))
}

/// The server's options with its checks (`checks`, its pack's `share.checks`: rust-analyzer's `checkOnSave`)
/// off — a swap's new server, a restarted one: measured, a start with checks on checks every folder at once.
pub fn checks_held(inner: MakeOptions, checks: String) -> MakeOptions {
    Box::new(move || {
        let mut o = inner();
        lsp::put(&mut o.init, &checks, json!(false));
        o
    })
}

/// A shared server's answer to `workspace/configuration`: the options it started with, its checks as they
/// are now. rust-analyzer takes the answer as its whole settings, so answering `checkOnSave` alone would
/// drop the rest (the left-out folders). None for one that isn't shared, or whose checks can't be held
/// (answered as before).
pub fn settings(c: &Client) -> Option<Value> {
    c.share.as_ref()?;
    let checks = c.traits.share.as_ref()?.checks.as_deref()?;
    let mut v = c.init_sent.get().cloned().unwrap_or(Value::Null);
    lsp::put(&mut v, checks, json!(!c.checks_off));
    Some(v)
}

/// Whether B's definition agrees with A's: the same places, or A had none (nothing to compare).
pub fn agree(a: &Value, b: &Value) -> bool {
    let a = lsp::location_uris(a);
    a.is_empty() || a == lsp::location_uris(b)
}

// ── Room for two ─────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pressure {
    Normal,
    Warn,
    Critical,
    /// Not said (Linux without PSI) — the free memory decides alone.
    Unknown,
}

/// The machine's memory as a swap reads it: the kernel's pressure, and what can be had without pushing
/// others out (macOS: free + file-backed + purgeable pages; Linux: `MemAvailable`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Room {
    pub pressure: Pressure,
    pub headroom: Option<u64>,
}

/// How memory is read — `room`, or a test's stand-in.
pub type ReadRoom = fn() -> Room;

/// What the new server is expected to need at its peak: 1.7 × the one in use, at least 1.5 GB (measured:
/// B's peak 1.1–1.7 × A, 1.4–2.5 GB for four checkouts of this repository).
pub fn need(a_footprint: Option<u64>) -> u64 {
    (a_footprint.unwrap_or(0) * 17 / 10).max(3 * GB / 2)
}

fn gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / GB as f64)
}

/// Swap (Ok: why there's room) or restart (Err: why not). Swap only with the pressure normal (or not said)
/// and twice the need free: no reading followed a 1–2 GB change within ±1 GB (others' pages compressed, other
/// programs moving), so the margin is wide.
pub fn decide(room: Room, a_footprint: Option<u64>) -> Result<String, String> {
    let need = need(a_footprint);
    match room.pressure {
        Pressure::Warn => return Err("memory pressure is up".into()),
        Pressure::Critical => return Err("memory pressure is critical".into()),
        Pressure::Normal | Pressure::Unknown => {}
    }
    match room.headroom {
        Some(h) if h >= 2 * need => {
            Ok(format!("{} free, room for the {} a second one needs", gb(h), gb(need)))
        }
        Some(h) => Err(format!("{} free, under twice the {} a second one needs", gb(h), gb(need))),
        None => Err("free memory unknown".into()),
    }
}

/// The machine's memory now (a few µs — still read on a worker, as every system read is).
#[cfg(target_os = "macos")]
pub fn room() -> Room {
    let pressure = match sysctl_int("kern.memorystatus_vm_pressure_level") {
        Some(1) => Pressure::Normal,
        Some(2) => Pressure::Warn,
        Some(4) => Pressure::Critical,
        _ => Pressure::Unknown,
    };
    Room { pressure, headroom: headroom() }
}

#[cfg(target_os = "macos")]
fn sysctl_int(name: &str) -> Option<i64> {
    let c = std::ffi::CString::new(name).ok()?;
    let mut v: i64 = 0;
    let mut size = std::mem::size_of::<i64>();
    // SAFETY: an 8-byte buffer and its size; a 4-byte value lands in its low half (little endian).
    let r =
        unsafe { libc::sysctlbyname(c.as_ptr(), (&raw mut v).cast(), &mut size, std::ptr::null_mut(), 0) };
    (r == 0).then_some(if size == 4 { v & 0xffff_ffff } else { v })
}

/// Free + file-backed + purgeable pages, in bytes.
#[cfg(target_os = "macos")]
#[allow(deprecated)]
fn headroom() -> Option<u64> {
    // (one send right to the host, kept — each call of `mach_host_self` would make another)
    static HOST: std::sync::OnceLock<libc::mach_port_t> = std::sync::OnceLock::new();
    // SAFETY: returns the host port; no arguments.
    let host = *HOST.get_or_init(|| unsafe { libc::mach_host_self() });
    // SAFETY: a plain out-struct and its size in integers; the call only fills them in.
    unsafe {
        let mut st: libc::vm_statistics64 = std::mem::zeroed();
        let mut count = libc::HOST_VM_INFO64_COUNT;
        if libc::host_statistics64(host, libc::HOST_VM_INFO64, (&raw mut st).cast(), &mut count) != 0 {
            return None;
        }
        let pages = st.free_count as u64 + st.external_page_count as u64 + st.purgeable_count as u64;
        Some(pages * libc::vm_page_size as u64)
    }
}

/// Linux: `MemAvailable`; the pressure from PSI where the kernel has it (`/proc/pressure/memory`), else
/// not said.
#[cfg(target_os = "linux")]
pub fn room() -> Room {
    let headroom = std::fs::read_to_string("/proc/meminfo").ok().and_then(|m| mem_available(&m));
    let pressure = std::fs::read_to_string("/proc/pressure/memory").map_or(Pressure::Unknown, |p| psi(&p));
    Room { pressure, headroom }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn room() -> Room {
    Room { pressure: Pressure::Unknown, headroom: None }
}

/// `MemAvailable` of `/proc/meminfo`, in bytes.
#[cfg(any(target_os = "linux", test))]
fn mem_available(meminfo: &str) -> Option<u64> {
    let line = meminfo.lines().find(|l| l.starts_with("MemAvailable:"))?;
    Some(line.split_whitespace().nth(1)?.parse::<u64>().ok()? * 1024)
}

/// PSI's memory pressure: tasks all stalled ≥ 5 % of the last 10 s = critical, some stalled ≥ 10 % = warn
/// (my thresholds — not measured; there's no Linux machine under pressure here to measure them on).
#[cfg(any(target_os = "linux", test))]
fn psi(text: &str) -> Pressure {
    let avg10 = |kind: &str| {
        text.lines()
            .find(|l| l.starts_with(kind))
            .and_then(|l| l.split_whitespace().find_map(|w| w.strip_prefix("avg10=")))
            .and_then(|v| v.parse::<f64>().ok())
    };
    match (avg10("some"), avg10("full")) {
        (_, Some(f)) if f >= 5.0 => Pressure::Critical,
        (Some(s), _) if s >= 10.0 => Pressure::Warn,
        (None, None) => Pressure::Unknown,
        _ => Pressure::Normal,
    }
}

// ── State ────────────────────────────────────────────────────────────────

/// A swap under way: B loading beside A.
pub struct Swap {
    pub a: ClientId,
    pub b: ClientId,
    pub started: Instant,
    /// What changed and why it's a swap (the log, `:procs`).
    pub why: String,
    /// The folders B brings in (their files are B's from its start).
    pub new: Vec<PathBuf>,
    /// The old folders where B agreed with A (or there was nothing to compare) · this round's answers
    /// (A's, B's) per folder · rounds asked.
    agreed: Vec<PathBuf>,
    answers: HashMap<PathBuf, (Option<Value>, Option<Value>)>,
    tries: u8,
    /// When B took over (A then answers what it was asked, and goes).
    pub switched: Option<Instant>,
}

#[derive(Default)]
pub struct Shares {
    /// Each folder's key, once read (None: not in a git checkout) · folders being read.
    keys: HashMap<PathBuf, Option<Key>>,
    reading: HashSet<PathBuf>,
    /// Files waiting for a server: their folder's key being read, or their folder joining at a swap.
    parked: Vec<DocId>,
    /// Folders to join each server at its next swap · when that's due.
    joining: HashMap<ClientId, Vec<PathBuf>>,
    due: HashMap<ClientId, Instant>,
    /// Servers whose next step (swap or restart) is being decided (memory read on a worker).
    deciding: HashSet<ClientId>,
    pub swaps: Vec<Swap>,
    /// Swapped-out servers told to shut down (their exit is no failure).
    leaving: HashSet<ClientId>,
    /// Restarted servers (and swaps cut short): their checks come on once they're quiescent.
    restarted: HashSet<ClientId>,
    /// How memory is read (tests: a stand-in).
    pub room: Option<ReadRoom>,
}

impl Shares {
    /// A swap's new server that hasn't taken over — asked nothing, its diagnostics unheard.
    pub fn incoming(&self, cid: ClientId) -> bool {
        self.swaps.iter().any(|s| s.b == cid && s.switched.is_none())
    }

    /// Being swapped out: told to shut down.
    pub fn leaving(&self, cid: ClientId) -> bool {
        self.leaving.contains(&cid)
    }

    /// In a swap, as A or B.
    pub fn swapping(&self, cid: ClientId) -> bool {
        self.swaps.iter().any(|s| s.a == cid || s.b == cid) || self.deciding.contains(&cid)
    }

    /// The server serving its folders now: not a swap's B yet, not an A it has replaced.
    fn in_use(&self, cid: ClientId) -> bool {
        !self.incoming(cid)
            && !self.leaving(cid)
            && !self.swaps.iter().any(|s| s.a == cid && s.switched.is_some())
    }
}

/// Folders by their names (`tarae, buildrs`).
fn names(roots: &[PathBuf]) -> String {
    let name =
        |r: &PathBuf| r.file_name().map_or_else(|| r.display().to_string(), |n| n.to_string_lossy().into());
    roots.iter().map(name).collect::<Vec<_>>().join(", ")
}

/// What a change of folders is: `+ buildrs − phase0` (for 3 checkouts).
fn change_said(have: &[PathBuf], want: &[PathBuf]) -> String {
    let added: Vec<PathBuf> = want.iter().filter(|r| !have.contains(r)).cloned().collect();
    let gone: Vec<PathBuf> = have.iter().filter(|r| !want.contains(r)).cloned().collect();
    let mut s = Vec::new();
    if !added.is_empty() {
        s.push(format!("+ {}", names(&added)));
    }
    if !gone.is_empty() {
        s.push(format!("− {}", names(&gone)));
    }
    format!("{} (for {} checkout{})", s.join(" "), want.len(), if want.len() == 1 { "" } else { "s" })
}

fn lang_of(d: &Document) -> Option<String> {
    Some(crate::syntax::detect(d.path.as_deref()?)?.name.clone())
}

impl Editor {
    fn read_room(&self) -> ReadRoom {
        self.lsp.share.room.unwrap_or(room)
    }

    /// The main server for document `id` (in `path`) of a kind that's shared: the one serving its folder;
    /// else none yet — its folder's key read first (a worker), or its folder joining a server of the same
    /// repository and toolchain at that one's next swap; else a new one, for its folder alone (a folder
    /// outside git: its own, as before).
    pub(crate) fn share_attach(
        &mut self,
        server: &lsp::ServerSpec,
        lang: &str,
        path: &Path,
        id: DocId,
    ) -> Option<ClientId> {
        let root = lsp::find_root_for(lang, path);
        let named = |c: &&Client| c.name == server.name;
        if let Some(c) = self
            .lsp
            .clients
            .iter()
            .filter(named)
            .find(|c| c.roots.contains(&root) && self.lsp.share.in_use(c.id))
        {
            return Some(c.id);
        }
        // (a swap bringing this folder in: its files are the new server's from its start)
        if let Some(s) = self.lsp.share.swaps.iter().find(|s| s.switched.is_none() && s.new.contains(&root)) {
            return Some(s.b);
        }
        let key = match self.lsp.share.keys.get(&root) {
            Some(k) => k.clone(),
            // (outside git: nothing to share — no wait for a worker)
            None if !root.ancestors().any(|d| d.join(".git").exists()) => {
                self.lsp.share.keys.insert(root.clone(), None);
                None
            }
            None => {
                let toolchain = lsp::traits(server).share.as_ref().map(|s| s.toolchain.clone());
                self.share_read_key(root, id, toolchain.unwrap_or_default());
                return None;
            }
        };
        let Some(key) = key else { return self.lsp_spawn(server, lang, path) };
        let same = self
            .lsp
            .clients
            .iter()
            .filter(named)
            .find(|c| c.share.as_ref() == Some(&key) && self.lsp.share.in_use(c.id))
            .map(|c| c.id);
        if let Some(a) = same {
            self.share_join(a, root, id);
            return None;
        }
        let cid = self.lsp_start(server, std::slice::from_ref(&root), false)?;
        if let Some(c) = self.client_mut(cid) {
            c.share = Some(key);
        }
        Some(cid)
    }

    fn park(&mut self, id: DocId) {
        if !self.lsp.share.parked.contains(&id) {
            self.lsp.share.parked.push(id);
        }
    }

    /// Folder `root`'s key is read on a worker (`toolchain`: the files that say the server's toolchain);
    /// document `id` gets its server then. (Keys are kept by folder: a second kind of shared server in one
    /// folder would take the first's — there's one kind today.)
    fn share_read_key(&mut self, root: PathBuf, id: DocId, toolchain: Vec<String>) {
        self.park(id);
        if !self.lsp.share.reading.insert(root.clone()) {
            return;
        }
        self.events.shared_jobs().spawn(move || {
            let key = key_of(&root, &toolchain);
            move |ed: &mut Editor| {
                ed.lsp.share.reading.remove(&root);
                ed.lsp.share.keys.insert(root, key);
                ed.share_unpark();
            }
        });
    }

    /// The parked files try again (a key read, a swap begun or ended).
    fn share_unpark(&mut self) {
        for id in std::mem::take(&mut self.lsp.share.parked) {
            if self.docs.iter().any(|d| d.id == id && d.lsp.client.is_none()) {
                self.attach_lsp(id);
            }
        }
    }

    /// Folder `root` joins server `a` at its next swap; document `id` waits for it.
    fn share_join(&mut self, a: ClientId, root: PathBuf, id: DocId) {
        self.park(id);
        let name = self.client(a).map(|c| c.name.clone()).unwrap_or_default();
        let list = self.lsp.share.joining.entry(a).or_default();
        if !list.contains(&root) {
            let joins = format!("{} joins the shared {name}", root.display());
            crate::log::Session::of(&root).line(&name, "share", &joins);
            list.push(root);
        }
        self.share_wake();
    }

    /// A session was made for checkout `root`: a project of a repository whose shared server is up joins it
    /// now — the swap runs while the session is set up and its Claude starts, not after its first file opens
    /// (that waited the whole 11–14 s of a new checkout's load from then). Disk on a worker.
    pub(crate) fn share_session(&mut self, root: &Path) {
        if !self.config.lsp.enabled {
            return;
        }
        // Each server shared now: what makes its project there (its languages' `needs-project`, else their
        // `roots` — rust-analyzer's: Cargo.toml, rust-project.json), and the files that say its toolchain
        let mut servers: Vec<(String, Vec<String>, Vec<String>)> = Vec::new();
        for c in &self.lsp.clients {
            if c.share.is_none()
                || !self.config.lsp.shares(&c.name, c.traits)
                || servers.iter().any(|s| s.0 == c.name)
            {
                continue;
            }
            let mut marks: Vec<String> = Vec::new();
            for (lang, _) in self.lsp.server_for.iter().filter(|(_, s)| **s == c.name) {
                marks.extend(
                    lsp::project_markers(lang).unwrap_or_else(|| lsp::markers_for(lang)).iter().cloned(),
                );
            }
            servers.push((c.name.clone(), marks, toolchain_files(c)));
        }
        if servers.is_empty() {
            return;
        }
        let root = root.to_path_buf();
        self.events.shared_jobs().spawn(move || {
            // (the folder its files' server would have: the checkout, when its project is at its top)
            let keys: Vec<(String, Key)> = servers
                .into_iter()
                .filter(|(_, marks, _)| marks.iter().any(|m| root.join(m).is_file()))
                .filter_map(|(name, _, files)| Some((name, key_of(&root, &files)?)))
                .collect();
            move |ed: &mut Editor| {
                for (name, key) in keys {
                    ed.share_session_key(root.clone(), &name, key);
                }
            }
        });
    }

    fn share_session_key(&mut self, root: PathBuf, server: &str, key: Key) {
        self.lsp.share.keys.insert(root.clone(), Some(key.clone()));
        let Some(c) = self
            .lsp
            .clients
            .iter()
            .find(|c| c.name == server && c.share.as_ref() == Some(&key) && self.lsp.share.in_use(c.id))
        else {
            return;
        };
        let (a, name) = (c.id, c.name.clone());
        let coming = self.lsp.share.swaps.iter().any(|s| s.new.contains(&root));
        if c.roots.contains(&root) || coming {
            return;
        }
        let list = self.lsp.share.joining.entry(a).or_default();
        if !list.contains(&root) {
            let msg = format!("{} joins the shared {name} as its session starts", root.display());
            crate::log::line(&name, "share", &msg);
            list.push(root);
        }
        self.share_wake();
    }

    /// `share_sync` again once a gathering is over.
    fn share_wake(&mut self) {
        self.events.shared_jobs().spawn(|| {
            std::thread::sleep(GATHER + ms(5));
            |ed: &mut Editor| ed.share_sync()
        });
    }

    /// Document `id` was given server `cid`: a swap under way from `cid` tells the new server too.
    pub(crate) fn share_shadow(&mut self, cid: ClientId, id: DocId, lang: &str) {
        let Some(b) = self.lsp.share.swaps.iter().find(|s| s.a == cid && s.switched.is_none()).map(|s| s.b)
        else {
            return;
        };
        let under = |ed: &Editor| {
            let path = ed.docs.iter().find(|d| d.id == id).and_then(|d| d.path.clone());
            path.is_some_and(|p| ed.client(b).is_some_and(|c| c.serves(&p)))
        };
        if under(self) {
            self.shadow_one(b, id, lang);
        }
    }

    fn shadow_one(&mut self, b: ClientId, id: DocId, lang: &str) {
        let u16 = self.client(b).is_some_and(|c| c.ready && c.encoding == lsp::Encoding::Utf16);
        let Some(doc) = self.docs.iter_mut().find(|d| d.id == id) else { return };
        doc.lsp.attach_shadow(b, u16);
        self.lsp_did_open(b, id, lang);
    }

    /// Every event: a shared server whose folders should change — a folder joining, a session ended (its
    /// files closed), a toolchain changed — swaps (or restarts) once the change has gathered for `GATHER`.
    pub(crate) fn share_sync(&mut self) {
        if self.lsp.share.parked.is_empty() && !self.lsp.clients.iter().any(|c| c.share.is_some()) {
            return;
        }
        let now = Instant::now();
        let ids: Vec<ClientId> = self
            .lsp
            .clients
            .iter()
            .filter(|c| c.share.is_some() && !self.lsp.share.swapping(c.id) && !self.lsp.share.leaving(c.id))
            .map(|c| c.id)
            .collect();
        for a in ids {
            let want = self.share_wanted(a);
            let have = self.client(a).map(|c| c.roots.clone()).unwrap_or_default();
            let same = want.len() == have.len() && want.iter().all(|r| have.contains(r));
            if same || want.is_empty() {
                self.lsp.share.due.remove(&a);
                continue;
            }
            let due = match self.lsp.share.due.get(&a) {
                Some(&d) => d,
                None => {
                    self.lsp.share.due.insert(a, now + GATHER);
                    self.share_wake();
                    continue;
                }
            };
            if now >= due {
                self.lsp.share.due.remove(&a);
                self.share_change(a, want);
            }
        }
    }

    /// The folders server `a` should serve: its own that are still wanted — a file of it open, or a
    /// session's checkout, and the same toolchain still — then the ones joining.
    fn share_wanted(&self, a: ClientId) -> Vec<PathBuf> {
        let Some(c) = self.client(a) else { return Vec::new() };
        let mut want: Vec<PathBuf> =
            c.roots
                .iter()
                .filter(|r| {
                    if self.lsp.share.keys.get(*r).is_some_and(|k| k.as_ref() != c.share.as_ref()) {
                        return false;
                    }
                    let open = self.docs.iter().any(|d| {
                        d.lsp.client == Some(a) && d.path.as_deref().is_some_and(|p| p.starts_with(r))
                    });
                    open || self.session_of(r).is_some()
                })
                .cloned()
                .collect();
        for r in self.lsp.share.joining.get(&a).into_iter().flatten() {
            if !want.contains(r) {
                want.push(r.clone());
            }
        }
        want
    }

    /// Server `a`'s folders become `want`: a restart if it never finished loading (nobody is served well
    /// yet), else swap or restart as memory allows — read on a worker.
    fn share_change(&mut self, a: ClientId, want: Vec<PathBuf>) {
        let Some(c) = self.client(a) else { return };
        let why = change_said(&c.roots, &want);
        // Only folders going (a session ended), its first one staying: told, nothing reloads from scratch
        // (#148 — a restart re-indexed every checkout left at 729% CPU)
        let leaves_only = want.iter().all(|r| c.roots.contains(r)) && want.first() == c.roots.first();
        if leaves_only && c.takes_folder_changes() {
            return self.share_leave(a, want, why);
        }
        if leaves_only {
            let msg = format!("{} doesn't take workspace folder changes — so not told", c.name);
            crate::log::line(&c.name, "share", &msg);
        }
        if !c.settled_once {
            return self.share_restart(a, want, format!("{why} — it was still loading, so a restart"));
        }
        let (pid, read) = (c.pid(), self.read_room());
        self.lsp.share.deciding.insert(a);
        self.events.shared_jobs().spawn(move || {
            let room = read();
            let footprint = pid.and_then(crate::procs::read).map(|o| o.footprint.unwrap_or(o.rss));
            move |ed: &mut Editor| ed.share_decided(a, want, why, room, footprint)
        });
    }

    /// Server `a` keeps only `want` (its folders less those gone): `workspace/didChangeWorkspaceFolders`
    /// with the rest removed — the same process, its index of the folders left kept.
    fn share_leave(&mut self, a: ClientId, want: Vec<PathBuf>, why: String) {
        let Some(c) = self.client_mut(a) else { return };
        let gone: Vec<Value> = c.roots.iter().filter(|r| !want.contains(r)).map(|r| lsp::folder(r)).collect();
        let event = json!({ "event": { "added": [], "removed": gone } });
        c.notify("workspace/didChangeWorkspaceFolders", event);
        c.roots = want;
        let name = c.name.clone();
        crate::log::line(&name, "share", &format!("{why} — told it the folders that went; no restart"));
        self.share_unpark();
    }

    fn share_decided(&mut self, a: ClientId, want: Vec<PathBuf>, why: String, room: Room, fp: Option<u64>) {
        self.lsp.share.deciding.remove(&a);
        if self.client(a).is_none() {
            return self.share_unpark();
        }
        match decide(room, fp) {
            Ok(mem) => self.swap_begin(a, want, format!("{why} · {mem}")),
            Err(mem) => self.share_restart(a, want, format!("{why} · a restart: {mem}")),
        }
    }

    /// Folders joining `from` join `to` now.
    fn share_move_joining(&mut self, from: ClientId, to: ClientId, taken: &[PathBuf]) {
        let Some(list) = self.lsp.share.joining.remove(&from) else { return };
        let rest: Vec<PathBuf> = list.into_iter().filter(|r| !taken.contains(r)).collect();
        if !rest.is_empty() {
            self.lsp.share.joining.entry(to).or_default().extend(rest);
        }
    }

    /// The swap: B for `want` beside A, told every open file of its folders and every change, asked
    /// nothing, its checks off; the new folders' files B's own.
    fn swap_begin(&mut self, a: ClientId, want: Vec<PathBuf>, why: String) {
        let Some(c) = self.client(a) else { return };
        let (spec, key, have) = (c.spec.clone(), c.share.clone(), c.roots.clone());
        let Some(b) = self.lsp_start(&spec, &want, true) else {
            crate::log::line(&spec.name, "swap", &format!("not started ({why}) — the one in use stays"));
            return;
        };
        if let Some(c) = self.client_mut(b) {
            c.share = key;
        }
        self.share_move_joining(a, b, &want);
        let new: Vec<PathBuf> = want.iter().filter(|r| !have.contains(r)).cloned().collect();
        let theirs: Vec<(DocId, String)> = self
            .docs
            .iter()
            .filter(|d| {
                d.lsp.client == Some(a)
                    && d.path.as_deref().is_some_and(|p| want.iter().any(|r| p.starts_with(r)))
            })
            .filter_map(|d| Some((d.id, lang_of(d)?)))
            .collect();
        for (id, lang) in theirs {
            self.shadow_one(b, id, &lang);
        }
        let msg = format!("swap begun: a new {} for {} — {why}", spec.name, names(&want));
        crate::log::line(&spec.name, "swap", &msg);
        self.lsp.share.swaps.push(Swap {
            a,
            b,
            started: Instant::now(),
            why,
            new,
            agreed: Vec::new(),
            answers: HashMap::new(),
            tries: 0,
            switched: None,
        });
        self.share_unpark();
        self.share_watch(b);
    }

    /// Server `cid` says it's quiescent: a restarted one's checks come on; a swap's B is asked whether it
    /// agrees with A.
    pub(crate) fn share_quiescent(&mut self, cid: ClientId) {
        if self.lsp.share.restarted.remove(&cid) {
            self.checks_on(cid);
        }
        if self.lsp.share.incoming(cid) {
            self.swap_check(cid);
        }
    }

    /// Its checks on again (rust-analyzer's `checkOnSave`: it asks for its settings again — `settings`;
    /// measured: no check starts, the next save checks).
    fn checks_on(&mut self, cid: ClientId) {
        let Some(c) = self.client_mut(cid) else { return };
        if !c.checks_off {
            return;
        }
        c.checks_off = false;
        c.notify("workspace/didChangeConfiguration", json!({ "settings": {} }));
        crate::log::line(&c.name.clone(), "share", "checks on save on again");
    }

    /// One round of "does B agree with A": the definition at a cursor in an open file of each old folder,
    /// asked of both.
    fn swap_check(&mut self, b: ClientId) {
        let Some(s) = self.lsp.share.swaps.iter().find(|s| s.b == b && s.switched.is_none()) else { return };
        let (a, agreed) = (s.a, s.agreed.clone());
        let a_roots = self.client(a).map(|c| c.roots.clone()).unwrap_or_default();
        let b_roots = self.client(b).map(|c| c.roots.clone()).unwrap_or_default();
        let olds: Vec<PathBuf> =
            a_roots.into_iter().filter(|r| b_roots.contains(r) && !agreed.contains(r)).collect();
        self.lsp_flush();
        let mut asked = Vec::new();
        let mut nothing = Vec::new();
        for root in olds {
            let doc = self.docs.iter().find(|d| {
                d.lsp.client == Some(a)
                    && d.lsp.shadows().any(|x| x == b)
                    && d.path.as_deref().is_some_and(|p| p.starts_with(&root))
            });
            let Some(doc) = doc.map(|d| d.id) else {
                nothing.push(root);
                continue;
            };
            let sent = [a, b].iter().all(|&cid| {
                let params = self.definition_at(doc, cid);
                let kind = Kind::SwapCheck { root: root.clone() };
                params.is_some_and(|p| {
                    self.lsp_send_for(doc, cid, kind, "textDocument/definition", p).is_some()
                })
            });
            if sent {
                asked.push(root);
            }
        }
        let Some(s) = self.lsp.share.swaps.iter_mut().find(|s| s.b == b) else { return };
        s.agreed.extend(nothing);
        s.tries += 1;
        s.answers = asked.into_iter().map(|r| (r, (None, None))).collect();
        if s.answers.is_empty() {
            self.swap_judge(b);
        }
    }

    fn definition_at(&self, id: DocId, cid: ClientId) -> Option<Value> {
        let doc = self.docs.iter().find(|d| d.id == id)?;
        let head = doc.selection().primary().cursor(&doc.text);
        let position = lsp::to_position(&doc.text, head, self.encoding_of(cid));
        Some(json!({ "textDocument": { "uri": lsp::uri(doc.path.as_deref()?) }, "position": position }))
    }

    /// One of a swap's two answers in folder `root` (None: an error — counted as nothing).
    pub(crate) fn swap_answer(&mut self, cid: ClientId, root: &Path, answer: Option<&Value>) {
        let Some(s) = self
            .lsp
            .share
            .swaps
            .iter_mut()
            .find(|s| (s.a == cid || s.b == cid) && s.switched.is_none() && s.answers.contains_key(root))
        else {
            return;
        };
        let v = answer.cloned().unwrap_or(Value::Null);
        let b = s.b;
        let Some(both) = s.answers.get_mut(root) else { return };
        if cid == s.a {
            both.0 = Some(v);
        } else {
            both.1 = Some(v);
        }
        if let (Some(x), Some(y)) = both.clone() {
            s.answers.remove(root);
            if agree(&x, &y) {
                s.agreed.push(root.to_path_buf());
            }
            if s.answers.is_empty() {
                self.swap_judge(b);
            }
        }
    }

    /// A round's answers are in: every old folder agreed → B takes over; else again (`AGREE_AGAIN`) while
    /// it's quiescent, and after `AGREE_TRIES` rounds it takes over anyway (said in the log).
    fn swap_judge(&mut self, b: ClientId) {
        let Some(s) = self.lsp.share.swaps.iter().find(|s| s.b == b && s.switched.is_none()) else { return };
        let a_roots = self.client(s.a).map(|c| c.roots.clone()).unwrap_or_default();
        let b_roots = self.client(b).map(|c| c.roots.clone()).unwrap_or_default();
        let all = a_roots.iter().filter(|r| b_roots.contains(r)).all(|r| s.agreed.contains(r));
        if all {
            return self.swap_switch(b, String::new());
        }
        if s.tries >= AGREE_TRIES {
            return self
                .swap_switch(b, format!(" — it didn't agree with the old one in {AGREE_TRIES} rounds"));
        }
        self.events.shared_jobs().spawn(move || {
            std::thread::sleep(AGREE_AGAIN);
            move |ed: &mut Editor| {
                if ed.client(b).is_some_and(|c| c.quiescent == Some(true)) {
                    ed.swap_check(b);
                }
            }
        });
    }

    /// B takes over: every file of A's in B's folders is B's (its link a plain one, its versions going on),
    /// what A found wrong stays (as B's, until B's own), B's checks come on; A answers what it was asked,
    /// then goes.
    fn swap_switch(&mut self, b: ClientId, note: String) {
        let Some(s) = self.lsp.share.swaps.iter_mut().find(|s| s.b == b && s.switched.is_none()) else {
            return;
        };
        s.switched = Some(Instant::now());
        let (a, took, why) = (s.a, s.started.elapsed(), s.why.clone());
        self.take_files(a, b);
        self.checks_on(b);
        self.share_move_joining(a, b, &[]);
        let name = self.client(b).map(|c| c.name.clone()).unwrap_or_default();
        let secs = crate::term::duration(took.as_secs_f32());
        crate::log::line(&name, "swap", &format!("switched to the new {name} after {secs}{note} — {why}"));
        self.swap_finish_a(a, Instant::now());
    }

    /// The files of A's that B has (as a shadow) become B's, A's diagnostics in them B's.
    fn take_files(&mut self, a: ClientId, b: ClientId) {
        let roots = self.client(b).map(|c| c.roots.clone()).unwrap_or_default();
        for d in &mut self.docs {
            if d.lsp.link(b).is_none() {
                continue;
            }
            d.lsp.take_over(a, b);
            if d.lsp.client.is_none() {
                d.lsp.client = Some(b);
            }
            for x in d.lsp.diagnostics.iter_mut().filter(|x| x.source == a) {
                x.source = b;
            }
        }
        for (path, list) in self.lsp.elsewhere.iter_mut() {
            if roots.iter().any(|r| path.starts_with(r)) {
                for x in list.iter_mut().filter(|x| x.0 == a) {
                    x.0 = b;
                }
            }
        }
        let published: Vec<(ClientId, PathBuf)> = self
            .lsp
            .published
            .keys()
            .filter(|(c, p)| *c == a && roots.iter().any(|r| p.starts_with(r)))
            .cloned()
            .collect();
        for (c, p) in published {
            if let Some(t) = self.lsp.published.remove(&(c, p.clone())) {
                self.lsp.published.insert((b, p), t);
            }
        }
    }

    /// A answers what it was asked (up to `A_FINISH`), then is told to shut down — and is gone at its answer,
    /// or after `A_FINISH` more.
    fn swap_finish_a(&mut self, a: ClientId, since: Instant) {
        let asked = self.lsp.pending.iter().any(|((c, _), p)| *c == a && !matches!(p.kind, Kind::Shutdown));
        if asked && since.elapsed() < A_FINISH {
            self.events.shared_jobs().spawn(move || {
                std::thread::sleep(ms(20));
                move |ed: &mut Editor| ed.swap_finish_a(a, since)
            });
            return;
        }
        self.lsp.share.leaving.insert(a);
        if self.lsp_send(a, Kind::Shutdown, "shutdown", Value::Null).is_none() {
            return self.share_drop(a);
        }
        self.events.shared_jobs().spawn(move || {
            std::thread::sleep(A_FINISH);
            move |ed: &mut Editor| {
                if ed.lsp.share.leaving(a) {
                    ed.share_drop(a);
                }
            }
        });
    }

    /// A swapped-out (or restarted) server goes: told to exit, stopped; files still its own (a folder that
    /// left it) get a server again.
    pub(crate) fn share_drop(&mut self, a: ClientId) {
        self.lsp.share.leaving.remove(&a);
        let orphans: Vec<DocId> =
            self.docs.iter().filter(|d| d.lsp.client == Some(a)).map(|d| d.id).collect();
        if let Some(c) = self.client_mut(a) {
            c.notify("exit", Value::Null);
        }
        let root = self.client(a).map(|c| c.root.display().to_string()).unwrap_or_default();
        if let Some(name) = self.lsp_stopped(a) {
            crate::log::line(&name, "stop", &format!("{name} for {root} stopped — replaced"));
        }
        for id in orphans {
            self.attach_lsp(id);
        }
        // (nothing of it is on screen)
        self.unchanged = true;
    }

    /// Server `a` goes at once and B for `want` starts in its place (checks off until it's loaded): everyone
    /// waits for B to load — the path when memory is short, or A was still loading itself.
    fn share_restart(&mut self, a: ClientId, want: Vec<PathBuf>, why: String) {
        let Some(c) = self.client(a) else { return };
        let (spec, key) = (c.spec.clone(), c.share.clone());
        let Some(b) = self.lsp_start(&spec, &want, true) else {
            crate::log::line(&spec.name, "restart", &format!("not started ({why}) — the one in use stays"));
            return;
        };
        if let Some(c) = self.client_mut(b) {
            c.share = key;
        }
        self.lsp.share.restarted.insert(b);
        self.share_move_joining(a, b, &want);
        let theirs: Vec<(DocId, String)> = self
            .docs
            .iter()
            .filter(|d| {
                d.lsp.client == Some(a)
                    && d.path.as_deref().is_some_and(|p| want.iter().any(|r| p.starts_with(r)))
            })
            .filter_map(|d| Some((d.id, lang_of(d)?)))
            .collect();
        for (id, lang) in &theirs {
            self.shadow_one(b, *id, lang);
        }
        self.take_files(a, b);
        let msg = format!("restart: a new {} for {} — {why}", spec.name, names(&want));
        crate::log::line(&spec.name, "restart", &msg);
        let orphans: Vec<DocId> =
            self.docs.iter().filter(|d| d.lsp.client == Some(a)).map(|d| d.id).collect();
        self.lsp_stopped(a);
        for id in orphans {
            self.attach_lsp(id);
        }
        self.share_unpark();
    }

    /// While two servers run side by side: memory read every `WATCH_EVERY`; pressure above normal ends the
    /// swap — B takes over now if it's ready and agreed, else at once all the same, A gone (a restart into
    /// the B already loading — dropping it would only load it again).
    fn share_watch(&mut self, b: ClientId) {
        let read = self.read_room();
        self.events.shared_jobs().spawn(move || {
            std::thread::sleep(WATCH_EVERY);
            let room = read();
            move |ed: &mut Editor| ed.share_watched(b, room)
        });
    }

    fn share_watched(&mut self, b: ClientId, room: Room) {
        let Some(s) = self.lsp.share.swaps.iter().find(|s| s.b == b && s.switched.is_none()) else { return };
        if !matches!(room.pressure, Pressure::Warn | Pressure::Critical) {
            // (nothing of it is on screen)
            self.unchanged = true;
            return self.share_watch(b);
        }
        let a = s.a;
        let why = format!(
            "memory pressure {} during the swap",
            if room.pressure == Pressure::Warn { "up" } else { "critical" }
        );
        // (ready: quiescent, and agreed with A in every folder they share)
        let ready = self.client(b).is_some_and(|c| c.quiescent == Some(true)) && {
            let a_roots = self.client(a).map(|c| c.roots.clone()).unwrap_or_default();
            let b_roots = self.client(b).map(|c| c.roots.clone()).unwrap_or_default();
            a_roots.iter().filter(|r| b_roots.contains(r)).all(|r| s.agreed.contains(r))
        };
        if ready {
            return self.swap_switch(b, format!(" — {why}"));
        }
        let name = self.client(b).map(|c| c.name.clone()).unwrap_or_default();
        crate::log::line(
            &name,
            "swap",
            &format!("{why}: the old one stops, the new one takes over as it loads"),
        );
        let orphans: Vec<DocId> = self
            .docs
            .iter()
            .filter(|d| d.lsp.client == Some(a) && d.lsp.link(b).is_none())
            .map(|d| d.id)
            .collect();
        self.lsp_stopped(a);
        for id in orphans {
            self.attach_lsp(id);
        }
    }

    /// Server `cid` is gone (`lsp_stopped`): what waited on it. A swap's A gone before B took over → B takes
    /// over now, loaded or not; a swap's B gone → A goes on, and the folders it was to bring in join A again.
    pub(crate) fn share_forget(&mut self, cid: ClientId) {
        let s = &mut self.lsp.share;
        s.deciding.remove(&cid);
        s.due.remove(&cid);
        s.restarted.remove(&cid);
        s.leaving.remove(&cid);
        let joining = s.joining.remove(&cid).unwrap_or_default();
        if let Some(i) = s.swaps.iter().position(|w| w.a == cid) {
            let w = s.swaps.remove(i);
            if w.switched.is_none() {
                s.restarted.insert(w.b);
                for d in &mut self.docs {
                    if d.lsp.shadows().any(|x| x == w.b) {
                        d.lsp.take_over(cid, w.b);
                        if d.lsp.client.is_none() {
                            d.lsp.client = Some(w.b);
                        }
                    }
                }
                let name = self.client(w.b).map(|c| c.name.clone()).unwrap_or_default();
                crate::log::line(
                    &name,
                    "swap",
                    "the old one went before the swap ended — the new one takes over as it loads",
                );
            }
            if !joining.is_empty() {
                self.lsp.share.joining.entry(w.b).or_default().extend(joining);
            }
        } else if let Some(i) = s.swaps.iter().position(|w| w.b == cid) {
            let w = s.swaps.remove(i);
            if w.switched.is_none() {
                let list = s.joining.entry(w.a).or_default();
                list.extend(w.new.into_iter().filter(|r| !list.contains(r)).collect::<Vec<_>>());
                let name = self.client(w.a).map(|c| c.name.clone()).unwrap_or_default();
                crate::log::line(&name, "swap", "the new one exited before it took over — the old one stays");
            }
        }
        self.share_unpark();
    }

    /// Server `cid` began loading its workspaces again (`Fetching`): its folders' keys are read again — one
    /// whose toolchain changed leaves it for a server of its own (`share_wanted` leaves it out; measured: a
    /// second toolchain added 0.5 GB to the shared one that stayed after the folder changed back).
    pub(crate) fn share_refetch(&mut self, cid: ClientId) {
        let Some(c) = self.client(cid) else { return };
        let (roots, toolchain) = (c.roots.clone(), toolchain_files(c));
        self.events.shared_jobs().spawn(move || {
            let keys: Vec<(PathBuf, Option<Key>, bool)> = roots
                .into_iter()
                .map(|r| {
                    let (k, there) = (key_of(&r, &toolchain), r.exists());
                    (r, k, there)
                })
                .collect();
            move |ed: &mut Editor| ed.share_keys_read(cid, keys)
        });
    }

    /// (a folder gone — its session's checkout removed — leaves too: measured live, rust-analyzer re-fetches
    /// when it goes, and says "Failed to load workspaces" until the swap without it)
    fn share_keys_read(&mut self, cid: ClientId, keys: Vec<(PathBuf, Option<Key>, bool)>) {
        let Some(c) = self.client(cid) else { return };
        let (key, single, name) = (c.share.clone(), c.roots.len() == 1, c.name.clone());
        for (r, k, there) in keys {
            if k != key {
                if single {
                    // (its only folder: it stays — the server reloads it itself)
                    if let (Some(c), Some(_)) = (self.client_mut(cid), &k) {
                        c.share = k.clone();
                    }
                } else {
                    let msg = match there {
                        true => format!(
                            "{}'s toolchain changed — it leaves the shared {name} for one of its own",
                            r.display()
                        ),
                        false => format!("{} is gone — it leaves the shared {name}", r.display()),
                    };
                    crate::log::line(&name, "share", &msg);
                }
            }
            self.lsp.share.keys.insert(r, k);
        }
        self.unchanged = true;
    }

    /// The status line's server part while a document's server is being arranged: its folder waiting to
    /// join, or its server being swapped (`swap 7.2s · Indexing 40%`).
    pub fn share_said(&self, doc: &Document) -> Option<String> {
        if self.lsp.share.parked.contains(&doc.id) {
            let lang = lang_of(doc)?;
            let name = self.lsp.server_for.get(&lang)?;
            return Some(format!("{name}: joining the shared one"));
        }
        let main = doc.lsp.client?;
        let s = self.lsp.share.swaps.iter().find(|s| s.switched.is_none() && (s.a == main || s.b == main))?;
        let b = self.client(s.b)?;
        let doing = match b.progress.values().next() {
            Some(p) => p.text(),
            None if !b.ready => "starting".into(),
            None => "checking it agrees".into(),
        };
        Some(format!(
            "{}: swap {} · {doing}",
            b.name,
            crate::term::duration(s.started.elapsed().as_secs_f32())
        ))
    }

    /// What `:procs` says a shared server is doing — None for one folder's own.
    pub(crate) fn share_procs(&self, c: &Client) -> Option<String> {
        let n = c.roots.len();
        let of = format!("{n} checkout{}: {}", if n == 1 { "" } else { "s" }, names(&c.roots));
        let s = self.lsp.share.swaps.iter().find(|s| s.a == c.id || s.b == c.id);
        Some(match s {
            Some(s) if s.b == c.id && s.switched.is_none() => {
                let secs = crate::term::duration(s.started.elapsed().as_secs_f32());
                format!("swapping in for {of} — {secs} so far · {}", s.why)
            }
            Some(s) if s.a == c.id && s.switched.is_some() => {
                format!("swapped out ({of}) — answering, then going")
            }
            Some(s) if s.a == c.id => format!("{of} — being swapped"),
            _ if self.lsp.share.leaving(c.id) => format!("swapped out ({of}) — going"),
            _ if n > 1 => format!("shared by {of}"),
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU8, Ordering};

    fn plenty() -> Room {
        Room { pressure: Pressure::Normal, headroom: Some(64 * GB) }
    }

    /// rust-analyzer's traits, as the rust pack says them — the tests here run on that data.
    fn ra() -> &'static lsp::ServerTraits {
        &crate::languages::get().traits["rust-analyzer"]
    }

    fn tight() -> Room {
        Room { pressure: Pressure::Normal, headroom: Some(GB) }
    }

    /// A repository `main` and worktrees of it (`.git` files naming their folder, and its `commondir`, as git
    /// makes them — no git run), each a cargo project with `src/main.rs`; the stand-in servers (`cat`) write
    /// what they're sent to `srv-<pid>.log`. `main` is the session's checkout.
    fn fixture(name: &str, worktrees: &[&str], room: ReadRoom) -> (PathBuf, Editor) {
        let t = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("tarae-share-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(t.join("main/.git")).unwrap();
        for c in std::iter::once(&"main").chain(worktrees) {
            std::fs::create_dir_all(t.join(c).join("src")).unwrap();
            std::fs::write(t.join(c).join("src/main.rs"), "fn main() {\n    let x = 1;\n}\n").unwrap();
            std::fs::write(t.join(c).join("Cargo.toml"), "[package]\nname = \"t\"\n").unwrap();
        }
        for w in worktrees {
            let dir = t.join("main/.git/worktrees").join(w);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("commondir"), "../..\n").unwrap();
            std::fs::write(t.join(w).join(".git"), format!("gitdir: {}\n", dir.display())).unwrap();
        }
        let cfg = format!(
            "[lsp.rust-analyzer]\ncommand = \"sh\"\nargs = [\"-c\", \"cat > '{0}/srv-'$$'.log'\"]\n\
             [lang.rust]\nlsp = [\"rust-analyzer\"]\n",
            t.display()
        );
        let (config, w) = crate::config::parse(&cfg);
        assert!(w.is_empty(), "{w:?}");
        let mut ed = Editor::new(config);
        ed.session.root = t.join("main");
        ed.lsp.share.room = Some(room);
        (t, ed)
    }

    fn settle(ed: &mut Editor, ms: u64, done: impl Fn(&Editor) -> bool) -> bool {
        let end = Instant::now() + Duration::from_millis(ms);
        while !done(ed) && Instant::now() < end {
            if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(10)) {
                ed.handle_event(ev);
            }
        }
        done(ed)
    }

    /// Opens `file`: its document and its main server, once it has one (a folder's key is read first).
    fn open(ed: &mut Editor, file: &Path) -> (DocId, Option<ClientId>) {
        ed.open(file).unwrap();
        let id = ed.doc().id;
        let has = |ed: &Editor| ed.docs.iter().find(|d| d.id == id).is_some_and(|d| d.lsp.client.is_some());
        settle(ed, 3000, has);
        (id, main_of(ed, id))
    }

    fn main_of(ed: &Editor, id: DocId) -> Option<ClientId> {
        ed.docs.iter().find(|d| d.id == id).and_then(|d| d.lsp.client)
    }

    /// Server `cid` answers `initialize` (UTF-8).
    fn up(ed: &mut Editor, cid: ClientId) {
        let init = json!({ "jsonrpc": "2.0", "id": 0, "result": { "capabilities": { "positionEncoding": "utf-8" } } });
        ed.on_lsp_message(cid, init);
    }

    fn quiescent(ed: &mut Editor, cid: ClientId) {
        let status = json!({ "jsonrpc": "2.0", "method": "experimental/serverStatus",
            "params": { "health": "ok", "quiescent": true } });
        ed.on_lsp_message(cid, status);
    }

    /// What server `cid` was sent, once every piece of `want` is in it (10 s at most).
    fn sent(t: &Path, ed: &Editor, cid: ClientId, want: &[&str]) -> String {
        let pid = ed.client(cid).and_then(Client::pid).expect("its process");
        let log = t.join(format!("srv-{pid}.log"));
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            let out = std::fs::read_to_string(&log).unwrap_or_default();
            if want.iter().all(|w| out.contains(w)) || Instant::now() >= end {
                return out;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The requests of a kind waiting on server `cid`.
    fn asked(ed: &Editor, cid: ClientId, kind: fn(&Kind) -> bool) -> Vec<u64> {
        ed.lsp
            .pending
            .iter()
            .filter(|((c, _), p)| *c == cid && kind(&p.kind))
            .map(|((_, id), _)| *id)
            .collect()
    }

    fn answer(ed: &mut Editor, cid: ClientId, id: u64, result: Value) {
        ed.on_lsp_message(cid, json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    fn is_check(k: &Kind) -> bool {
        matches!(k, Kind::SwapCheck { .. })
    }

    /// The swap's comparison round: A and B answer `a` and `b` to each definition asked.
    fn round(ed: &mut Editor, a: ClientId, b: ClientId, ra: &Value, rb: &Value) {
        settle(ed, 2000, |ed| !asked(ed, a, is_check).is_empty() && !asked(ed, b, is_check).is_empty());
        for id in asked(ed, a, is_check) {
            answer(ed, a, id, ra.clone());
        }
        for id in asked(ed, b, is_check) {
            answer(ed, b, id, rb.clone());
        }
    }

    fn the_swap(ed: &Editor) -> Option<(ClientId, ClientId)> {
        ed.lsp.share.swaps.iter().find(|s| s.switched.is_none()).map(|s| (s.a, s.b))
    }

    /// A's shutdown answered: it's gone, quietly.
    fn shut(ed: &mut Editor, a: ClientId) {
        settle(ed, 3000, |ed| !asked(ed, a, |k| matches!(k, Kind::Shutdown)).is_empty());
        let id = asked(ed, a, |k| matches!(k, Kind::Shutdown)).pop().expect("told to shut down");
        answer(ed, a, id, Value::Null);
        assert!(ed.client(a).is_none(), "gone at its answer");
        assert!(!ed.lsp.failed.contains_key("rust-analyzer"), "a swapped-out server isn't a failure");
        assert!(!ed.toasts.iter().any(|t| t.text.contains("exited")), "nothing said about it on screen");
    }

    /// The recipe end to end, with stand-in servers: a worktree's first file joins the repository's server
    /// by a swap — B for both folders, its checks off, told every open file (A's as a shadow: asked nothing,
    /// its diagnostics unheard) and every change, the new folder's file B's own from the start — while A goes
    /// on serving its folder. Once B is quiescent and agrees with A, every file is B's, what A found wrong
    /// stays, B's checks come on, A is shut down — quietly.
    #[test]
    fn a_new_checkout_joins_by_a_swap_and_nobody_waits() {
        let (t, mut ed) = fixture("swap", &["wt1"], plenty);
        let (main_file, wt_file) = (t.join("main/src/main.rs"), t.join("wt1/src/main.rs"));
        let (main, a) = open(&mut ed, &main_file);
        let a = a.expect("the repository's server");
        assert_eq!(ed.client(a).unwrap().roots, [t.join("main")]);
        assert!(ed.client(a).unwrap().share.is_some(), "shared by the repository");
        up(&mut ed, a);
        quiescent(&mut ed, a);
        // A diagnostic of A's in main's file
        let diag = |line: u64| json!({ "range": { "start": { "line": line, "character": 4 }, "end": { "line": line, "character": 5 } }, "message": "unused", "severity": 2 });
        let publish = |path: &Path, d: Value| json!({ "jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": { "uri": lsp::uri(path), "diagnostics": [d] } });
        ed.on_lsp_message(a, publish(&main_file, diag(1)));

        // The worktree's file: its folder joins by a swap
        ed.open(&wt_file).unwrap();
        let wt = ed.doc().id;
        assert!(settle(&mut ed, 3000, |ed| the_swap(ed).is_some()), "a swap began");
        let (sa, b) = the_swap(&ed).unwrap();
        assert_eq!(sa, a);
        assert_eq!(ed.client(b).unwrap().roots, [t.join("main"), t.join("wt1")], "B for both folders");
        assert!(
            settle(&mut ed, 2000, |ed| main_of(ed, wt) == Some(b)),
            "the new folder's file is B's from the start"
        );
        assert_eq!(main_of(&ed, main), Some(a), "main's file still A's");
        let doc = |ed: &Editor, id: DocId| {
            ed.docs.iter().find(|d| d.id == id).unwrap().lsp.shadows().collect::<Vec<_>>()
        };
        assert_eq!(doc(&ed, main), [b], "B told main's file as a shadow");
        let said = ed.share_said(ed.docs.iter().find(|d| d.id == main).unwrap()).unwrap();
        assert!(said.starts_with("rust-analyzer: swap"), "the status line says it: {said}");
        let procs = ed.share_procs(ed.client(b).unwrap()).unwrap();
        assert!(procs.starts_with("swapping in for 2 checkouts: main, wt1"), "{procs}");

        up(&mut ed, b);
        let out = sent(&t, &ed, b, &["initialize", "wt1/src/main.rs", "main/src/main.rs"]);
        assert!(out.contains(r#""checkOnSave":false"#), "B's checks off: {out}");
        assert!(
            out.contains(r#""serverStatusNotification":true"#),
            "asked to say when it's quiescent: {out}"
        );
        assert!(out.contains(&format!(r#""uri":"{}""#, lsp::uri(&t.join("wt1")))), "both folders");
        assert_eq!(out.matches("textDocument/didOpen").count(), 2, "every open file: {out}");
        // Its settings answer says checks off too (rust-analyzer takes it as its whole settings)
        ed.on_lsp_message(b, json!({ "jsonrpc": "2.0", "id": 900, "method": "workspace/configuration", "params": { "items": [{ "section": "rust-analyzer" }] } }));
        let out = sent(&t, &ed, b, &[r#""id":900"#]);
        assert!(out.contains(r#""result":[{"checkOnSave":false}]"#), "{out}");

        // A change goes to both; B's diagnostics aren't heard yet
        ed.open(&main_file).unwrap();
        for k in ["i", "y", "<esc>"] {
            ed.handle_key(k.trim_matches(['<', '>']).parse().unwrap());
        }
        ed.lsp_flush();
        assert!(sent(&t, &ed, b, &["didChange"]).contains("didChange"), "B got the change");
        assert!(sent(&t, &ed, a, &["didChange"]).contains("didChange"), "A got it too");
        ed.on_lsp_message(b, publish(&main_file, diag(0)));
        let diags = |ed: &Editor| ed.docs.iter().find(|d| d.id == main).unwrap().lsp.diagnostics.clone();
        assert_eq!(diags(&ed).len(), 1, "B isn't heard before it takes over");
        assert_eq!(diags(&ed)[0].source, a);

        // Quiescent: asked whether it agrees — not yet (null), then again, and yes
        let place = json!([{ "uri": lsp::uri(&main_file), "range": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 7 } } }]);
        quiescent(&mut ed, b);
        round(&mut ed, a, b, &place, &Value::Null);
        assert_eq!(main_of(&ed, main), Some(a), "B hasn't agreed: A stays");
        round(&mut ed, a, b, &place, &place);
        assert_eq!(main_of(&ed, main), Some(b), "B took over");
        assert!(doc(&ed, main).is_empty(), "no shadow left");
        assert_eq!(diags(&ed).len(), 1, "what A found stays");
        assert_eq!(diags(&ed)[0].source, b, "as B's");
        assert!(!ed.client(b).unwrap().checks_off);
        assert!(
            sent(&t, &ed, b, &["didChangeConfiguration"]).contains("didChangeConfiguration"),
            "checks on"
        );
        ed.on_lsp_message(b, json!({ "jsonrpc": "2.0", "id": 901, "method": "workspace/configuration", "params": { "items": [{ "section": "rust-analyzer" }] } }));
        assert!(sent(&t, &ed, b, &[r#""id":901"#]).contains(r#""result":[{"checkOnSave":true}]"#));
        // A answers what it was asked, then goes
        shut(&mut ed, a);
        let procs = ed.share_procs(ed.client(b).unwrap()).unwrap();
        assert_eq!(procs, "shared by 2 checkouts: main, wt1");
        ed.on_lsp_message(b, json!({ "jsonrpc": "2.0", "id": 902, "method": "workspace/workspaceFolders" }));
        assert!(sent(&t, &ed, b, &[r#""id":902"#]).contains(&lsp::uri(&t.join("wt1"))), "its folders, both");
        let _ = std::fs::remove_dir_all(&t);
    }

    /// A session made for a checkout of the repository joins the shared server as it's made — before any
    /// file of it opens; its first file then goes to the new server.
    #[test]
    fn a_session_made_joins_before_its_first_file() {
        let (t, mut ed) = fixture("session", &["wt1"], plenty);
        let (_, a) = open(&mut ed, &t.join("main/src/main.rs"));
        let a = a.unwrap();
        up(&mut ed, a);
        quiescent(&mut ed, a);
        let id = ed.session_add(t.join("wt1"));
        assert!(settle(&mut ed, 3000, |ed| the_swap(ed).is_some()), "a swap began with no file of it open");
        let (_, b) = the_swap(&ed).unwrap();
        assert_eq!(ed.client(b).unwrap().roots, [t.join("main"), t.join("wt1")]);
        ed.session_switch(id);
        let (_, got) = open(&mut ed, &t.join("wt1/src/main.rs"));
        assert_eq!(got, Some(b), "its first file: the new server's");
        ed.sessions.clear();
        let _ = std::fs::remove_dir_all(&t);
    }

    /// Two checkouts' first files close together are one swap; a checkout no longer wanted (its file
    /// closed, no session for it) leaves by another swap, with the shorter list.
    #[test]
    fn checkouts_close_together_are_one_swap_and_one_that_ends_leaves_by_another() {
        let (t, mut ed) = fixture("batch", &["wt1", "wt2"], plenty);
        let (_, a) = open(&mut ed, &t.join("main/src/main.rs"));
        let a = a.unwrap();
        up(&mut ed, a);
        quiescent(&mut ed, a);
        ed.open(&t.join("wt1/src/main.rs")).unwrap();
        ed.open(&t.join("wt2/src/main.rs")).unwrap();
        let wt2 = ed.doc().id;
        assert!(settle(&mut ed, 3000, |ed| the_swap(ed).is_some()));
        let (_, b) = the_swap(&ed).unwrap();
        assert_eq!(
            ed.client(b).unwrap().roots.len(),
            3,
            "one swap for both: {:?}",
            ed.client(b).unwrap().roots
        );
        assert_eq!(ed.lsp.clients.len(), 2, "A and B, no third");
        up(&mut ed, b);
        quiescent(&mut ed, b);
        let place = json!([{ "uri": "file:///x.rs", "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } } }]);
        round(&mut ed, a, b, &place, &place);
        assert!(the_swap(&ed).is_none(), "switched");
        shut(&mut ed, a);
        // wt2's file closed — nothing keeps its checkout: a swap with the shorter list
        ed.close_doc(wt2);
        assert!(settle(&mut ed, 3000, |ed| the_swap(ed).is_some()), "a swap to leave it");
        let (sa, c) = the_swap(&ed).unwrap();
        assert_eq!(sa, b);
        assert_eq!(ed.client(c).unwrap().roots, [t.join("main"), t.join("wt1")]);
        let _ = std::fs::remove_dir_all(&t);
    }

    /// #148: a checkout leaving a server that takes folder changes (rust-analyzer says so in its `initialize`
    /// answer) is told it — the same server goes on for the rest, no swap, no restart. (One that doesn't
    /// take them swaps: the test above.)
    #[test]
    fn a_checkout_that_ends_leaves_by_a_folder_change_when_the_server_takes_them() {
        let (t, mut ed) = fixture("leave", &["wt1"], plenty);
        let takes = |ed: &mut Editor, cid: ClientId| {
            let caps = json!({ "positionEncoding": "utf-8",
                "workspace": { "workspaceFolders": { "supported": true, "changeNotifications": true } } });
            ed.on_lsp_message(cid, json!({ "jsonrpc": "2.0", "id": 0, "result": { "capabilities": caps } }));
            quiescent(ed, cid);
        };
        let (_, a) = open(&mut ed, &t.join("main/src/main.rs"));
        let a = a.unwrap();
        takes(&mut ed, a);
        ed.open(&t.join("wt1/src/main.rs")).unwrap();
        let wt = ed.doc().id;
        assert!(settle(&mut ed, 3000, |ed| the_swap(ed).is_some()), "joining is still a swap");
        let (_, b) = the_swap(&ed).unwrap();
        takes(&mut ed, b);
        let place = json!([{ "uri": "file:///x.rs", "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } } }]);
        round(&mut ed, a, b, &place, &place);
        shut(&mut ed, a);
        // wt1's file closed, no session for it: told, and the same server serves main
        ed.close_doc(wt);
        let left = |ed: &Editor| ed.client(b).is_some_and(|c| c.roots == [t.join("main")]);
        assert!(settle(&mut ed, 3000, left), "{:?}", ed.client(b).map(|c| c.roots.clone()));
        assert!(the_swap(&ed).is_none() && ed.lsp.clients.len() == 1, "no swap, no second server");
        let out = sent(&t, &ed, b, &["didChangeWorkspaceFolders"]);
        let told = out.lines().find(|l| l.contains("didChangeWorkspaceFolders")).unwrap_or_default();
        assert!(
            told.contains(&format!(r#""removed":[{{"name":"wt1","uri":"{}"}}]"#, lsp::uri(&t.join("wt1")))),
            "{told}"
        );
        assert!(told.contains(r#""added":[]"#), "{told}");
        let _ = std::fs::remove_dir_all(&t);
    }

    /// Without room for two the change is a restart: A goes at once, one server for every folder starts, its
    /// checks off until it's loaded. A server that never finished loading is restarted too.
    #[test]
    fn without_room_for_two_a_restart() {
        let (t, mut ed) = fixture("tight", &["wt1", "wt2"], tight);
        let (main, a) = open(&mut ed, &t.join("main/src/main.rs"));
        let a = a.unwrap();
        up(&mut ed, a);
        quiescent(&mut ed, a);
        let (wt, _) = (ed.open(&t.join("wt1/src/main.rs")).map(|_| ed.doc().id).unwrap(), ());
        assert!(settle(&mut ed, 3000, |ed| ed.client(a).is_none()), "A stopped at once");
        let b = main_of(&ed, main).expect("main's file has the new one");
        assert_eq!(main_of(&ed, wt), Some(b), "and the worktree's");
        assert_eq!(ed.client(b).unwrap().roots, [t.join("main"), t.join("wt1")]);
        assert!(ed.lsp.share.swaps.is_empty(), "no swap");
        assert!(ed.client(b).unwrap().checks_off, "checks off while it loads");
        up(&mut ed, b);
        assert!(sent(&t, &ed, b, &["checkOnSave"]).contains(r#""checkOnSave":false"#));
        quiescent(&mut ed, b);
        assert!(!ed.client(b).unwrap().checks_off, "on once it's loaded");
        assert!(sent(&t, &ed, b, &["didChangeConfiguration"]).contains("didChangeConfiguration"));
        // Room again: the next checkout is a swap
        ed.lsp.share.room = Some(plenty);
        let (wt2, _) = (ed.open(&t.join("wt2/src/main.rs")).map(|_| ed.doc().id).unwrap(), ());
        assert!(settle(&mut ed, 3000, |ed| main_of(ed, wt2).is_some()));
        let c = main_of(&ed, wt2).unwrap();
        assert_eq!(the_swap(&ed), Some((b, c)), "with room: a swap");
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn a_server_still_loading_is_restarted_with_the_new_list() {
        let (t, mut ed) = fixture("loading", &["wt1"], plenty);
        let (main, a) = open(&mut ed, &t.join("main/src/main.rs"));
        let a = a.unwrap();
        up(&mut ed, a);
        // (never quiescent)
        ed.open(&t.join("wt1/src/main.rs")).unwrap();
        assert!(settle(&mut ed, 3000, |ed| ed.client(a).is_none()), "restarted, not swapped");
        let b = main_of(&ed, main).unwrap();
        assert_eq!(ed.client(b).unwrap().roots.len(), 2);
        assert!(ed.lsp.share.swaps.is_empty());
        let _ = std::fs::remove_dir_all(&t);
    }

    static PRESSURE: AtomicU8 = AtomicU8::new(0);

    fn pressure_later() -> Room {
        match PRESSURE.load(Ordering::SeqCst) {
            0 => plenty(),
            _ => Room { pressure: Pressure::Warn, headroom: Some(64 * GB) },
        }
    }

    /// Memory pressure rising while two servers run side by side ends the swap: B (not yet ready) takes over
    /// at once and A stops — every file B's.
    #[test]
    fn pressure_during_a_swap_ends_it() {
        let (t, mut ed) = fixture("pressure", &["wt1"], pressure_later);
        let (main, a) = open(&mut ed, &t.join("main/src/main.rs"));
        let a = a.unwrap();
        up(&mut ed, a);
        quiescent(&mut ed, a);
        ed.open(&t.join("wt1/src/main.rs")).unwrap();
        assert!(settle(&mut ed, 3000, |ed| the_swap(ed).is_some()));
        let (_, b) = the_swap(&ed).unwrap();
        up(&mut ed, b);
        PRESSURE.store(1, Ordering::SeqCst);
        assert!(settle(&mut ed, 3000, |ed| ed.client(a).is_none()), "A stopped");
        assert_eq!(main_of(&ed, main), Some(b), "B took main's file");
        assert!(ed.lsp.share.swaps.is_empty());
        assert!(ed.client(b).unwrap().checks_off, "its checks wait until it's loaded");
        quiescent(&mut ed, b);
        assert!(!ed.client(b).unwrap().checks_off);
        let _ = std::fs::remove_dir_all(&t);
    }

    /// A checkout whose toolchain changes (seen when the server loads its workspaces again) leaves the shared
    /// server — a swap with the shorter list — and its file gets a server of its own.
    #[test]
    fn a_checkout_whose_toolchain_changes_gets_its_own() {
        let (t, mut ed) = fixture("toolchain", &["wt1"], plenty);
        let (main, a) = open(&mut ed, &t.join("main/src/main.rs"));
        let a = a.unwrap();
        up(&mut ed, a);
        // (still loading: restarted for both)
        let (wt, _) = (ed.open(&t.join("wt1/src/main.rs")).map(|_| ed.doc().id).unwrap(), ());
        assert!(settle(&mut ed, 3000, |ed| main_of(ed, wt).is_some() && ed.client(a).is_none()));
        let s = main_of(&ed, wt).unwrap();
        up(&mut ed, s);
        quiescent(&mut ed, s);
        std::fs::write(t.join("wt1/rust-toolchain.toml"), "[toolchain]\nchannel = \"1.98.0\"\n").unwrap();
        let fetching = json!({ "jsonrpc": "2.0", "method": "$/progress", "params": { "token": "rustAnalyzer/Fetching", "value": { "kind": "begin", "title": "Fetching" } } });
        ed.on_lsp_message(s, fetching);
        assert!(settle(&mut ed, 3000, |ed| the_swap(ed).is_some()), "a swap to let it go");
        let (_, b) = the_swap(&ed).unwrap();
        assert_eq!(ed.client(b).unwrap().roots, [t.join("main")]);
        up(&mut ed, b);
        quiescent(&mut ed, b);
        let place = json!([{ "uri": "file:///x.rs", "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } } }]);
        round(&mut ed, s, b, &place, &place);
        assert_eq!(main_of(&ed, main), Some(b));
        shut(&mut ed, s);
        let own = main_of(&ed, wt).expect("the worktree's file has a server again");
        assert!(own != b && own != s);
        assert_eq!(ed.client(own).unwrap().roots, [t.join("wt1")], "its own");
        let _ = std::fs::remove_dir_all(&t);
    }

    /// A new session's checkout joins as it's made (`a_session_made_joins_before_its_first_file`) only when
    /// it's a project at its top — by what the server's language's pack says makes one (rust's
    /// `needs-project`); one that isn't waits for a file of its own.
    #[test]
    fn a_new_sessions_checkout_joins_only_as_a_project() {
        let (t, mut ed) = fixture("session-project", &["wt1", "wt2"], plenty);
        let (_, a) = open(&mut ed, &t.join("main/src/main.rs"));
        let a = a.unwrap();
        up(&mut ed, a);
        quiescent(&mut ed, a);
        std::fs::remove_file(t.join("wt2/Cargo.toml")).unwrap();
        ed.share_session(&t.join("wt2"));
        ed.share_session(&t.join("wt1"));
        assert!(settle(&mut ed, 3000, |ed| the_swap(ed).is_some()), "a swap began");
        let (_, b) = the_swap(&ed).unwrap();
        assert_eq!(ed.client(b).unwrap().roots, [t.join("main"), t.join("wt1")], "wt1 joined, wt2 didn't");
        let _ = std::fs::remove_dir_all(&t);
    }

    /// `editor.lsp-share` unset: the servers whose pack says `share` (rust-analyzer) — and what `:config-show`
    /// says; set, exactly its list (`[]`: none).
    #[test]
    fn who_shares_is_the_packs_unless_set() {
        let (ra_spec, other) =
            (lsp::default_spec("rust-analyzer").unwrap(), lsp::default_spec("gopls").unwrap());
        let unset = crate::lsp::LspConfig::default();
        assert!(unset.shares("rust-analyzer", lsp::traits(&ra_spec)));
        assert!(!unset.shares("gopls", lsp::traits(&other)));
        assert_eq!(crate::lsp::shared_by_packs(), ["rust-analyzer"]);
        let (config, w) = crate::config::parse("[editor]\nlsp-share = []\n");
        assert!(w.is_empty(), "{w:?}");
        assert!(!config.editor.lsp.shares("rust-analyzer", lsp::traits(&ra_spec)), "turned off");
        let (config, _) = crate::config::parse("[editor]\nlsp-share = [\"gopls\"]\n");
        assert!(config.editor.lsp.shares("gopls", lsp::traits(&other)), "one whose pack doesn't say it");
        assert!(!config.editor.lsp.shares("rust-analyzer", lsp::traits(&ra_spec)));
        let shown = crate::settings::SETTINGS.iter().find(|s| s.path == "editor.lsp-share").unwrap();
        let default = crate::config::EditorConfig::default();
        assert_eq!((shown.get)(&default), toml::Value::Array(vec!["rust-analyzer".into()]));
    }

    #[test]
    fn swap_or_restart_by_pressure_and_twice_the_need() {
        let room = |pressure, headroom: Option<u64>| Room { pressure, headroom };
        // A at 1 GB: B needs 1.7 GB at its peak; twice that is 3.4 GB
        let a = Some(GB);
        assert_eq!(need(a), GB * 17 / 10);
        assert_eq!(need(None), 3 * GB / 2, "at least 1.5 GB");
        assert!(decide(room(Pressure::Normal, Some(4 * GB)), a).is_ok());
        assert!(
            decide(room(Pressure::Unknown, Some(4 * GB)), a).is_ok(),
            "no pressure said: the free memory decides"
        );
        let short = decide(room(Pressure::Normal, Some(3 * GB)), a).unwrap_err();
        assert!(short.contains("under twice"), "{short}");
        assert!(decide(room(Pressure::Warn, Some(64 * GB)), a).unwrap_err().contains("pressure"));
        assert!(decide(room(Pressure::Critical, Some(64 * GB)), a).is_err());
        assert!(decide(room(Pressure::Normal, None), a).is_err(), "free memory unknown: restart");
    }

    #[test]
    fn linux_memory_readings() {
        let info = "MemTotal:       16000000 kB\nMemFree:          500000 kB\nMemAvailable:    6000000 kB\n";
        assert_eq!(mem_available(info), Some(6_000_000 * 1024));
        let calm = "some avg10=0.50 avg60=0.10 avg300=0.00 total=1\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n";
        assert_eq!(psi(calm), Pressure::Normal);
        let some = "some avg10=12.00 avg60=3.00 avg300=1.00 total=1\nfull avg10=1.00 avg60=0.00 avg300=0.00 total=0\n";
        assert_eq!(psi(some), Pressure::Warn);
        let full = "some avg10=40.00 avg60=3.00 avg300=1.00 total=1\nfull avg10=9.00 avg60=0.00 avg300=0.00 total=0\n";
        assert_eq!(psi(full), Pressure::Critical);
        assert_eq!(psi(""), Pressure::Unknown);
    }

    /// The real readings answer on this machine (whatever they say).
    #[test]
    #[cfg(target_os = "macos")]
    fn this_machines_memory_reads() {
        let r = room();
        assert_ne!(r.pressure, Pressure::Unknown, "{r:?}");
        assert!(r.headroom.is_some_and(|h| h > 0), "{r:?}");
    }

    #[test]
    fn a_shared_servers_symbols_are_the_asking_checkouts() {
        let roots = [PathBuf::from("/w/main"), PathBuf::from("/w/s1")];
        let mine = Path::new("/w/s1");
        assert!(foreign(&roots, mine, Path::new("/w/main/src/a.rs")), "the other checkout's");
        assert!(!foreign(&roots, mine, Path::new("/w/s1/src/a.rs")), "its own");
        assert!(
            !foreign(&roots, mine, Path::new("/rustlib/src/core/lib.rs")),
            "the standard library: everyone's"
        );
        assert!(!foreign(&roots[..1], mine, Path::new("/w/main/src/a.rs")), "one folder: nothing to filter");
        // A session whose checkout holds the server's folder lower down
        assert!(!foreign(&roots, Path::new("/w"), Path::new("/w/main/src/a.rs")));
    }

    #[test]
    fn a_worktree_shares_its_repositorys_key_and_a_toolchain_tells_them_apart() {
        let t = std::env::temp_dir().join(format!("tarae-share-key-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        let (main, wt, clone) = (t.join("main"), t.join("wt"), t.join("clone"));
        std::fs::create_dir_all(main.join(".git/worktrees/wt")).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::create_dir_all(clone.join(".git")).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", main.join(".git/worktrees/wt").display()))
            .unwrap();
        std::fs::write(main.join(".git/worktrees/wt/commondir"), "../..\n").unwrap();
        // (the files the rust pack says rust-analyzer's toolchain is in)
        let files = &ra().share.as_ref().expect("the rust pack shares rust-analyzer").toolchain;
        let key = |r: &Path| key_of(r, files);
        let k = key(&main).unwrap();
        assert_eq!(key(&wt), Some(k.clone()), "a worktree: its repository's");
        assert_ne!(key(&clone), Some(k.clone()), "a clone is a repository of its own");
        assert_eq!(key(&t.join("nowhere")), None);
        std::fs::write(wt.join("rust-toolchain.toml"), "[toolchain]\nchannel = \"1.98.0\"\n").unwrap();
        let w = key(&wt).unwrap();
        assert_eq!(w.repo, k.repo);
        assert_ne!(w, k, "another toolchain: another server");
        std::fs::remove_file(wt.join("rust-toolchain.toml")).unwrap();
        std::fs::write(wt.join("rust-toolchain"), "1.98.0\n").unwrap();
        assert_ne!(key(&wt), Some(k.clone()), "the older file name too");
        assert_eq!(
            key_of(&wt, &[]),
            key_of(&main, &[]),
            "a server whose pack names none: by repository alone"
        );
        let _ = std::fs::remove_dir_all(&t);
    }
}
