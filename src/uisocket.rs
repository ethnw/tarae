//! The UI state's socket (`docs/design/ui-state.md`): `$state/ui/<pid>.sock`, one per tarae, on by default
//! — the folder 0700, the socket 0600 (bound under umask 077, then checked). A peer must be this user, and
//! is refused when it runs under one of Claude's processes or runs (Claude's runs can't reach it anyway:
//! Seatbelt lets them connect only to sockets in the project and their own `RunDir`). Snapshots — and a
//! shuttle's keys, below — one JSON line per request and per answer. Nobody asking, one thread sits in `accept` — no wakes.
//!
//! - `{"get":"state"}` → the state's JSON; `"format":"text"` → `{"text": …}`;
//! - `"after_input": N` holds the answer until `input` ≥ N and a frame after that (the keys sent were handled
//!   and drawn) — instead of sleeping after sending keys.
//! - `"chat": "s1"` (an id or a name, `""` the session in front) → that session's last turn instead
//!   (`uistate::ChatTurn`) — the user's level; Claude's never reads another session's.
//! - `"review": "s1"` → that session's reviewer, its verdict and findings whole (`uistate::ReviewReport`) —
//!   the user's level, as `chat`.
//! - `{"get":"features"}` → which features get used, every one with its count (`features::Report`, its file
//!   read on a worker); `"format":"text"` → `:features`' text. The user's level only.
//! - `{"get":"frame"}` → the frame drawn, as the terminal got it: `{"frame":{cols, rows, frames, input,
//!   bytes}}` (`after_input` as for the state) — what `cargo xtask shot --live` draws. The user's level: a frame
//!   shows every session's tabs and cards, and Claude's `screen` is a card.
//!
//! - `{"shuttle": {"name": …, "for": pid}}` → this connection carries a shuttle's steps (`shuttle.rs`): keys a
//!   program sends, each checked where it lands — the one thing here that acts, only with `ui.shuttle-keys` on in
//!   your own config and once you said `y` on its card.
//!
//! `tarae state [--json] [--pid N] [--after-input N] [--chat [S] | --review [S] | --frame FILE | --features]` is
//! the client (`cli`); `tarae keys` is a shuttle's (`keys_cli`).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

/// How long a snapshot is waited for, unless the request says (`timeout_ms`, at most `WAIT_MAX`).
pub(crate) const WAIT: Duration = Duration::from_secs(10);
const WAIT_MAX: Duration = Duration::from_secs(120);
/// `sun_path`'s room, its closing NUL included (macOS 104, Linux 108): a longer path is bound from its folder.
#[cfg(target_os = "macos")]
const SUN_PATH: usize = 104;
#[cfg(not(target_os = "macos"))]
const SUN_PATH: usize = 108;

/// This process's socket, once bound (`TARAE_UI` for the terminal panel and the user's tasks).
static BOUND: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// The socket of tarae `pid`.
pub fn path_for(pid: u32) -> Option<PathBuf> {
    crate::config::state_dir().map(|d| d.join("ui").join(format!("{pid}.sock")))
}

/// The socket this tarae listens on.
pub fn bound() -> Option<&'static Path> {
    BOUND.get().map(PathBuf::as_path)
}

/// Binds this process's socket. At startup, before any thread exists: the umask (and, for a path too long
/// for `sun_path`, the working folder) are the process's for a moment.
pub fn bind() -> Result<UnixListener, String> {
    let pid = std::process::id();
    let path = path_for(pid).ok_or("no state folder (neither XDG_STATE_HOME nor HOME)")?;
    let listener = bind_in(path.parent().expect("in ui/"), pid)?;
    let _ = BOUND.set(path);
    Ok(listener)
}

/// `<dir>/<pid>.sock`, bound: the folder made or closed (0700, this user's), a dead tarae's socket at that
/// path replaced (nothing else there is touched), bound under umask 077 and checked.
pub fn bind_in(dir: &Path, pid: u32) -> Result<UnixListener, String> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    let path = dir.join(format!("{pid}.sock"));
    let say = |what: &str, e: std::io::Error| format!("{what} {}: {e}", dir.display());
    if let Some(state) = dir.parent() {
        std::fs::create_dir_all(state).map_err(|e| say("can't make", e))?;
    }
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(say("can't make", e)),
        _ => {}
    }
    let meta = std::fs::symlink_metadata(dir).map_err(|e| say("can't read", e))?;
    // SAFETY: geteuid only reads.
    let me = unsafe { libc::geteuid() };
    if !meta.is_dir() || meta.uid() != me {
        return Err(format!("{} isn't a folder of this user's — no socket", dir.display()));
    }
    if meta.mode() & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| say("can't close", e))?;
    }
    // A socket at this pid's path is a dead process's (the pid is ours now) — the only one ever removed
    if let Ok(m) = std::fs::symlink_metadata(&path) {
        use std::os::unix::fs::FileTypeExt;
        if !m.file_type().is_socket() {
            return Err(format!("{} is there and isn't a socket — no socket", path.display()));
        }
        std::fs::remove_file(&path).map_err(|e| format!("can't remove the old {}: {e}", path.display()))?;
    }
    // SAFETY: umask only sets the process's mask (no thread is running yet); put back right after.
    let old = unsafe { libc::umask(0o077) };
    let listener = at(&path, |p: &Path| UnixListener::bind(p));
    // SAFETY: as above.
    unsafe { libc::umask(old) };
    let listener = listener.map_err(|e| format!("can't listen on {}: {e}", path.display()))?;
    // (bound 0700 — a socket's mode is what's left of 0777; read and write are all it needs)
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("can't close {}: {e}", path.display()))?;
    let mode = std::fs::metadata(&path).map(|m| m.mode() & 0o777).unwrap_or(0);
    if mode != 0o600 {
        return Err(format!("{} is {mode:o}, not 600 — no socket", path.display()));
    }
    Ok(listener)
}

/// `f` on `path` — from its folder when the path is too long for `sun_path` (single-threaded callers only:
/// the working folder is the process's).
fn at<T>(path: &Path, f: impl FnOnce(&Path) -> std::io::Result<T>) -> std::io::Result<T> {
    if path.as_os_str().len() < SUN_PATH {
        return f(path);
    }
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else { return f(path) };
    let back = std::env::current_dir()?;
    std::env::set_current_dir(dir)?;
    let r = f(Path::new(name));
    std::env::set_current_dir(back)?;
    r
}

/// The socket goes with this tarae (the process ending; `:restart`'s new binary binds it again).
pub fn remove() {
    if let Some(p) = bound() {
        let _ = std::fs::remove_file(p);
    }
}

/// Serves snapshots: a thread blocked in `accept`, one more per connection. Each request goes to the main
/// loop as a job; the answer is built after the next frame (`uistate::after_frame`).
pub fn serve(listener: UnixListener, tx: crate::event::Tx) {
    std::thread::Builder::new()
        .name("tarae-ui".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let tx = tx.clone();
                let _ =
                    std::thread::Builder::new().name("tarae-ui-peer".into()).spawn(move || peer(stream, tx));
            }
        })
        .ok();
}

/// One connection: this user's only; then its requests, an answer a line.
fn peer(stream: UnixStream, tx: crate::event::Tx) {
    let Ok(mut out) = stream.try_clone() else { return };
    let mut say = |v: Value| writeln!(out, "{v}").is_ok();
    let (uid, pid) = match peer_cred(&stream) {
        Ok(p) => p,
        Err(e) => {
            say(json!({ "error": format!("can't tell who's asking: {e}") }));
            return;
        }
    };
    // SAFETY: geteuid only reads.
    if uid != unsafe { libc::geteuid() } {
        say(json!({ "error": "refused: another user" }));
        return;
    }
    // (the peer and its parents — a process of Claude's among them is refused on the main loop, which
    // knows Claude's pids)
    let line = ancestors(pid);
    let mut lines = BufReader::new(&stream).lines();
    while let Some(req) = lines.next() {
        let Ok(req) = req else { return };
        if req.trim().is_empty() {
            continue;
        }
        // (a watch takes the connection over: events a line until either end goes; a shuttle, its steps)
        if let Ok(v) = serde_json::from_str::<Value>(&req) {
            if let Some(kinds) = v.get("watch") {
                drop(say);
                return watch(out, kinds, &line, &tx);
            }
            if let Some(hello) = v.get("shuttle") {
                drop(say);
                return shuttle(out, &mut lines, hello, &line, &tx);
            }
        }
        if !say(answer(&req, &line, &tx)) {
            return;
        }
    }
}

/// How long a hello waits for its answer — the person may be reading its card.
const HELLO_WAIT: Duration = Duration::from_secs(15 * 60);

/// `{"shuttle": {"name": …, "for": pid}}`: this connection carries a shuttle's steps (`shuttle.rs`). Its hello
/// answered — connected, or refused (a card may ask the person first) — then a request a line: a step (`keys` or
/// `text`, with `expect`), `{"resume": true}`, or a snapshot (`get`), each answered.
fn shuttle(
    mut out: UnixStream,
    lines: &mut std::io::Lines<BufReader<&UnixStream>>,
    hello: &Value,
    line: &[u32],
    tx: &crate::event::Tx,
) {
    let mut say = |v: &str| writeln!(out, "{v}").is_ok();
    // (no `for`: the grant is the connecting process's own, until it exits)
    let peer = line.first().copied().unwrap_or_default();
    let pid = match hello.get("for") {
        None | Some(Value::Null) => peer,
        Some(v) => match v.as_u64().and_then(|p| u32::try_from(p).ok()) {
            Some(p) => p,
            None => {
                say(&json!({ "error": "for: the pid of a process this one runs under, or none" }).to_string());
                return;
            }
        },
    };
    let Some(os) = crate::procs::read(pid) else {
        say(&json!({ "error": format!("process {pid} isn't there (or not this user's)") }).to_string());
        return;
    };
    let of = crate::shuttle::For { pid, started: os.started };
    let name = hello["name"].as_str().map_or_else(|| os.name.clone(), str::to_string);
    // (the program it runs, by its path — a name it picks can mislead; and this tarae with what runs it: never
    // a `for`)
    let exe_said = |p: u32| exe_of(p).map(|p| p.display().to_string()).unwrap_or_default();
    let exe = exe_said(pid);
    let above = ancestors(std::process::id());
    // (the program connecting, by its name and executable — the card names it beside a wider `for`)
    let peer_what = crate::procs::read(peer).map(|p| p.name).unwrap_or_default();
    let (peer_exe, ours) = (exe_said(peer), line.to_vec());
    let config = crate::shuttle::config_dirs();
    let h = crate::shuttle::Hello {
        name,
        of,
        what: os.name,
        exe,
        peer_what,
        peer_exe,
        line: ours,
        above,
        config,
    };
    type Asked = Box<dyn FnOnce(&mut crate::editor::Editor, crate::shuttle::Reply) + Send>;
    let ask = |job: Asked, wait: Duration| -> Result<String, String> {
        let (reply, got) = std::sync::mpsc::channel();
        let sent =
            tx.send(crate::event::Event::Job(Box::new(move |ed: &mut crate::editor::Editor| job(ed, reply))));
        sent.map_err(|_| "tarae is ending".to_string())?;
        got.recv_timeout(wait).map_err(|_| format!("no answer within {} s", wait.as_secs()))
    };
    let said = match ask(Box::new(move |ed, reply| crate::shuttle::hello(ed, h, reply)), HELLO_WAIT) {
        Ok(s) => s,
        Err(e) => {
            say(&json!({ "error": e }).to_string());
            return;
        }
    };
    let id = serde_json::from_str::<Value>(&said).ok().and_then(|v| v["id"].as_u64());
    if !say(&said) {
        return;
    }
    let Some(id) = id else { return };
    for req in lines {
        let Ok(req) = req else { return };
        if req.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&req) else {
            say(&json!({ "error": "not JSON — {\"keys\": …, \"expect\": {…}}" }).to_string());
            continue;
        };
        let answered = if v.get("get").is_some() {
            Ok(answer(&req, line, tx).to_string())
        } else if v["resume"] == true {
            ask(Box::new(move |ed, reply| _ = reply.send(crate::shuttle::resume(ed, id).to_string())), WAIT)
        } else {
            match crate::shuttle::Step::parse(id, &v) {
                Ok(step) => ask(Box::new(move |ed, reply| crate::shuttle::step(ed, step, reply)), WAIT * 2),
                Err(e) => Err(e),
            }
        };
        let said = answered.unwrap_or_else(|e| json!({ "error": e }).to_string());
        if !say(&said) {
            return;
        }
    }
}

/// The kinds of events a watcher may ask for (`{"watch":[…]}`; none: all).
const KINDS: [&str; 11] = [
    "keys", "cards", "sessions", "reviewer", "review", "turns", "runs", "toasts", "usage", "procs", "sandbox",
];

/// `{"watch":["keys","cards",…]}`: this connection is told what changes, a JSON line an event, from the batch
/// of events it's registered in on (`uistate/watch.rs`). It ends when the reader goes — or reads too slowly: then its
/// last line says so.
fn watch(mut out: UnixStream, kinds: &Value, line: &[u32], tx: &crate::event::Tx) {
    let kinds: Vec<String> = match kinds {
        Value::Array(a) => a.iter().filter_map(|k| k.as_str()).map(String::from).collect(),
        _ => Vec::new(),
    };
    if let Some(k) = kinds.iter().find(|k| !KINDS.contains(&k.as_str())) {
        let _ =
            writeln!(out, "{}", json!({ "error": format!("no events of kind {k:?} — {}", KINDS.join(" ")) }));
        return;
    }
    let (events, got) = std::sync::mpsc::sync_channel(crate::uistate::watch::UNREAD);
    let (ok, registered) = std::sync::mpsc::channel();
    let overflowed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (line, flag, asked) = (line.to_vec(), overflowed.clone(), kinds.clone());
    let job = move |ed: &mut crate::editor::Editor| {
        ed.drawn_for("a UI-state watch");
        let claude = ed.claude_pids();
        if let Some(p) = line.iter().find(|p| claude.contains(p)) {
            let _ = ok.send(Err(format!("refused: under a process of Claude's (pid {p})")));
            ed.unchanged = true;
            return;
        }
        let at = (ed.frames, ed.input_seq);
        ed.ui_watch_add(crate::uistate::watch::Watcher { tx: events, kinds: asked, overflowed: flag });
        let _ = ok.send(Ok(at));
        // (no frame for it: what it's told comes at the end of each batch of events, drawn or not)
        ed.unchanged = true;
    };
    if tx.send(crate::event::Event::Job(Box::new(job))).is_err() {
        return;
    }
    match registered.recv_timeout(WAIT) {
        Ok(Ok((frame, input))) => {
            let all = if kinds.is_empty() { KINDS.iter().map(|k| k.to_string()).collect() } else { kinds };
            let hello = json!({ "event": "watching", "kinds": all, "frame": frame, "input": input,
                                "said": "watching — events come as they happen" });
            if writeln!(out, "{hello}").is_err() {
                return;
            }
        }
        Ok(Err(e)) => {
            let _ = writeln!(out, "{}", json!({ "error": e }));
            return;
        }
        Err(_) => return,
    }
    // (until tarae lets this watcher go — the reader gone, or too slow — or ends)
    for ev in got.iter() {
        if writeln!(out, "{ev}").is_err() {
            return;
        }
    }
    if overflowed.load(std::sync::atomic::Ordering::Relaxed) {
        let e = format!(
            "let go: {} events unread — connect again and read a snapshot (frames tell what was missed)",
            crate::uistate::watch::UNREAD
        );
        let _ = writeln!(out, "{}", json!({ "event": "dropped", "said": e }));
    }
}

/// A request's answer.
fn answer(req: &str, line: &[u32], tx: &crate::event::Tx) -> Value {
    let Ok(req) = serde_json::from_str::<Value>(req) else {
        return json!({ "error": "not JSON — {\"get\":\"state\"}" });
    };
    use crate::uistate::Asked;
    let session = |k: &str| req[k].as_str().map(String::from);
    let what = match req["get"].as_str() {
        Some("frame") => Asked::Frame,
        Some("features") => Asked::Features,
        Some("state") => match (session("chat"), session("review")) {
            (Some(s), _) => Asked::Chat(s),
            (None, Some(s)) => Asked::Review(s),
            (None, None) => Asked::State,
        },
        _ => {
            return json!({ "error": "unknown request — {\"get\":\"state\"}, {\"get\":\"frame\"} or {\"get\":\"features\"}" });
        }
    };
    let text = req["format"] == "text" && what != Asked::Frame;
    let after_input = req["after_input"].as_u64().unwrap_or(0);
    let wait = req["timeout_ms"].as_u64().map_or(WAIT, Duration::from_millis).min(WAIT_MAX);
    let (reply, got) = std::sync::mpsc::channel();
    let line = line.to_vec();
    let until = std::time::Instant::now() + wait;
    let refused = reply.clone();
    let job = move |ed: &mut crate::editor::Editor| {
        ed.drawn_for("a UI-state ask");
        let claude = ed.claude_pids();
        if let Some(p) = line.iter().find(|p| claude.contains(p)) {
            let e = format!("refused: under a process of Claude's (pid {p}) — Claude has a view of its own");
            let _ = refused.send(json!({ "error": e }).to_string());
            // (nothing changed: no frame for it)
            ed.unchanged = true;
            return;
        }
        ed.ui_asks.push(crate::uistate::Ask { after_input, json: !text, reply, until, what });
    };
    if tx.send(crate::event::Event::Job(Box::new(job))).is_err() {
        return json!({ "error": "tarae is ending" });
    }
    match got.recv_timeout(wait) {
        Ok(s) if text => json!({ "text": s }),
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|_| json!({ "error": "a broken answer" })),
        Err(_) => {
            json!({ "error": format!("no frame within {} ms (input {after_input} not reached?)", wait.as_millis()) })
        }
    }
}

/// The peer's user and process.
#[cfg(target_os = "macos")]
fn peer_cred(s: &UnixStream) -> std::io::Result<(u32, u32)> {
    use std::os::fd::AsRawFd;
    let fd = s.as_raw_fd();
    let (mut uid, mut gid) = (0, 0);
    let mut pid: libc::pid_t = 0;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: out-pointers to live locals of the sizes given; the calls only fill them in.
    unsafe {
        if libc::getpeereid(fd, &mut uid, &mut gid) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::getsockopt(fd, libc::SOL_LOCAL, libc::LOCAL_PEERPID, (&raw mut pid).cast(), &mut len) != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok((uid, pid as u32))
}

/// The peer's user and process (`SO_PEERCRED`).
#[cfg(target_os = "linux")]
fn peer_cred(s: &UnixStream) -> std::io::Result<(u32, u32)> {
    use std::os::fd::AsRawFd;
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: an out-pointer to a live ucred of the size given; the call only fills it in.
    let r = unsafe {
        libc::getsockopt(s.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, (&raw mut cred).cast(), &mut len)
    };
    if r != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((cred.uid, cred.pid as u32))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn peer_cred(_s: &UnixStream) -> std::io::Result<(u32, u32)> {
    Err(std::io::Error::other("no peer credentials on this system"))
}

/// The executable process `pid` runs (macOS: `proc_pidpath`).
#[cfg(target_os = "macos")]
fn exe_of(pid: u32) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut buf = vec![0u8; 4096];
    // SAFETY: a live buffer of the size given; the call only fills it in, saying how much.
    let n = unsafe { libc::proc_pidpath(pid as libc::c_int, buf.as_mut_ptr().cast(), buf.len() as u32) };
    (n > 0).then(|| PathBuf::from(std::ffi::OsStr::from_bytes(&buf[..n as usize])))
}

/// The executable process `pid` runs (`/proc/<pid>/exe`).
#[cfg(not(target_os = "macos"))]
fn exe_of(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

/// `pid` and its parents, up to the first process, pid 1 (launchd, init) included — as far as the OS shows them.
fn ancestors(pid: u32) -> Vec<u32> {
    line_up(pid, |p| crate::procs::read(p).map(|os| os.ppid))
}

/// `pid` and its parents as `parent` tells them, up to pid 1 (kept: a peer whose parent it is was re-parented
/// there — `shuttle::hello_refused`).
pub(crate) fn line_up(pid: u32, parent: impl Fn(u32) -> Option<u32>) -> Vec<u32> {
    let mut v = vec![pid];
    while v.len() < 64 && v.last() != Some(&1) {
        match parent(*v.last().expect("one")) {
            Some(pp) if pp >= 1 && !v.contains(&pp) => v.push(pp),
            _ => break,
        }
    }
    v
}

// ── `tarae state` ────────────────────────────────────────────────────────

const USAGE: &str = "\
usage: tarae state [--json] [--pid PID] [--after-input N]
                   [--chat [SESSION] | --review [SESSION] | --frame FILE | --features | --watch [KINDS]]
  the UI state of a running tarae: where the keys go, the mode, the sessions, the cards, the panels,
  the runs, the toasts, what each session's Claude costs, its processes (:procs)
  --json             as JSON (the schema: docs/design/ui-state.md)
  --pid PID          that tarae (else $TARAE_UI's, or the one whose session holds this folder)
  --after-input N    once N keys and clicks were handled and drawn (read `input` first, add what you send)
  --chat [SESSION]   instead: a session's last turn as its chat shows it — by id (s1) or name, else the
                     one in front
  --review [SESSION] instead: a session's reviewer — its verdict and findings whole (space v r)
  --frame FILE       instead: the frame drawn, as the terminal got it, into FILE (`cargo xtask shot --live`
                     draws it as a picture)
  --features         instead: which features get used, and which never (:features; counted only with
                     features.count on)
  --watch [KINDS]    what changes, a line an event, until interrupted — KINDS: keys,cards,sessions,reviewer,
                     review,turns,runs,toasts,usage,procs,sandbox (all when left out)";

/// `tarae state`: a reader that stops early (`| head -3`) ends it quietly — exit 0, nothing said.
pub fn cli(args: &[String]) -> anyhow::Result<()> {
    match state(args) {
        Err(e) if e.is::<ReaderGone>() => Ok(()),
        r => r,
    }
}

/// Stdout's reader is gone (EPIPE): nothing more to print, nothing wrong.
#[derive(Debug)]
struct ReaderGone;

impl std::fmt::Display for ReaderGone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("stdout's reader is gone")
    }
}

impl std::error::Error for ReaderGone {}

/// `text` on stdout, flushed — a closed pipe is `ReaderGone` (`println!` would panic on it).
fn say(text: std::fmt::Arguments) -> anyhow::Result<()> {
    let mut out = std::io::stdout().lock();
    out.write_fmt(text).and_then(|()| out.flush()).map_err(|e| match e.kind() {
        std::io::ErrorKind::BrokenPipe => anyhow::Error::new(ReaderGone),
        _ => e.into(),
    })
}

fn state(args: &[String]) -> anyhow::Result<()> {
    let (mut json, mut pid, mut after, mut watching) = (false, None, 0u64, None::<Vec<String>>);
    let (mut chat, mut review, mut frame) = (None::<String>, None::<String>, None::<PathBuf>);
    let mut features = false;
    let mut it = args.iter().peekable();
    while let Some(a) = it.next() {
        let num = |v: Option<&String>| v.and_then(|v| v.parse::<u64>().ok());
        match a.as_str() {
            "--json" => json = true,
            "--features" => features = true,
            "--chat" => chat = Some(it.next_if(|s| !s.starts_with("--")).cloned().unwrap_or_default()),
            "--review" => review = Some(it.next_if(|s| !s.starts_with("--")).cloned().unwrap_or_default()),
            "--frame" => {
                frame = Some(
                    it.next().map(PathBuf::from).ok_or_else(|| anyhow::anyhow!("--frame FILE\n{USAGE}"))?,
                )
            }
            "--watch" => {
                let kinds = it.next_if(|k| !k.starts_with("--"));
                watching = Some(kinds.map(|k| k.split(',').map(String::from).collect()).unwrap_or_default());
            }
            "--pid" => {
                pid = Some(num(it.next()).ok_or_else(|| anyhow::anyhow!("--pid PID\n{USAGE}"))? as u32)
            }
            "--after-input" => {
                after = num(it.next()).ok_or_else(|| anyhow::anyhow!("--after-input N\n{USAGE}"))?
            }
            "-h" | "--help" => return say(format_args!("{USAGE}\n")),
            other => anyhow::bail!("unknown: {other}\n{USAGE}"),
        }
    }
    let path = match pid {
        Some(p) => path_for(p).ok_or_else(|| anyhow::anyhow!("no state folder"))?,
        None => find()?,
    };
    if let Some(kinds) = watching {
        let mut s = at(&path, |p: &Path| UnixStream::connect(p))
            .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        writeln!(s, "{}", json!({ "watch": kinds }))?;
        for line in BufReader::new(&s).lines() {
            let line = line?;
            let v: Value = serde_json::from_str(&line).unwrap_or_default();
            if let Some(e) = v["error"].as_str() {
                anyhow::bail!("{e}");
            }
            match json {
                true => say(format_args!("{line}\n"))?,
                false => {
                    say(format_args!("{:>6}  {}\n", v["frame"], v["said"].as_str().unwrap_or_default()))?
                }
            }
        }
        return Ok(());
    }
    let get = match (&frame, features) {
        (Some(_), _) => "frame",
        (None, true) => "features",
        (None, false) => "state",
    };
    let mut req = json!({ "get": get, "after_input": after });
    if !json {
        req["format"] = json!("text");
    }
    if let Some(which) = chat {
        req["chat"] = json!(which);
    }
    if let Some(which) = review {
        req["review"] = json!(which);
    }
    let v = ask(&path, &req).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    if let Some(e) = v["error"].as_str() {
        anyhow::bail!("{e}");
    }
    if let Some(file) = frame {
        let f = &v["frame"];
        std::fs::write(&file, f["bytes"].as_str().unwrap_or_default())
            .map_err(|e| anyhow::anyhow!("{}: {e}", file.display()))?;
        return say(format_args!("{}: {}×{}, frame {}\n", file.display(), f["cols"], f["rows"], f["frames"]));
    }
    match v["text"].as_str() {
        Some(t) if !json => say(format_args!("{t}")),
        _ => say(format_args!("{v}\n")),
    }
}

const KEYS_USAGE: &str = "\
usage: tarae keys [--pid PID] [--json] [--for PID] [--name NAME] [--expect-keys ID] [--expect 'ID F=V …']…
                  (KEYS… | --text TEXT | --text-file FILE | --resume)
  type into a running tarae over its socket (ui.shuttle-keys, your own config): each step is checked where
  it lands, as tarae handles it — refused, nothing sent, unless what's expected holds then
  KEYS               tarae's key notation, a word a key: y · space v m · C-q · esc · enter
  --expect-keys ID   where the keys must go: card#12, pane#4, chat, prompt … (`keys →` in tarae state)
  --expect 'ID F=V'  an element's fields as `tarae state --json` shows them: 'review:src/a.rs viewed=false';
                     card#12@3 is card#12 at rev 3
  --text TEXT        one paste, with your paste's rules (a : line opens the command line, never runs it)
  --text-file FILE   the paste from a file
  --resume           go on after your key paused it (once you've been idle 5 s)
  --for PID          the grant goes to PID instead of this command alone: a card asks you once, and every
                     program PID starts may type until it ends ($TARAE_SHUTTLE_FOR) — the program that
                     drives tarae (claude, a test runner), never your interactive shell. Without it, each
                     `tarae keys` is asked for on its own
  --name NAME        what the card and the status line call it ($TARAE_SHUTTLE_NAME, else its process's name)
  --json             the answer as JSON
  exits 1 when refused — the answer says why, and where the keys go now";

/// `tarae keys`: one shuttle step (`shuttle.rs`) — its hello, the step, the answer said.
pub fn keys_cli(args: &[String]) -> anyhow::Result<()> {
    let (mut json, mut pid, mut of, mut name) = (false, None::<u32>, None::<u32>, None::<String>);
    let (mut expect, mut keys, mut text, mut resume) = (serde_json::Map::new(), Vec::new(), None, false);
    let mut it = args.iter();
    let need =
        |v: Option<&String>, what: &str| v.cloned().ok_or_else(|| anyhow::anyhow!("{what}\n{KEYS_USAGE}"));
    let pid_of =
        |v: String, what: &str| v.parse::<u32>().map_err(|_| anyhow::anyhow!("{what}: {v:?} isn't a pid"));
    while let Some(a) = it.next() {
        match a.as_str() {
            "--json" => json = true,
            "--resume" => resume = true,
            "--pid" => pid = Some(pid_of(need(it.next(), "--pid PID")?, "--pid")?),
            "--for" => of = Some(pid_of(need(it.next(), "--for PID")?, "--for")?),
            "--name" => name = Some(need(it.next(), "--name NAME")?),
            "--expect-keys" => {
                expect.insert("keys".into(), json!({ "to": need(it.next(), "--expect-keys ID")? }));
            }
            "--expect" => {
                let (id, fields) = expect_of(&need(it.next(), "--expect 'ID F=V …'")?)?;
                expect.insert(id, fields);
            }
            "--text" => text = Some(need(it.next(), "--text TEXT")?),
            "--text-file" => {
                let f = need(it.next(), "--text-file FILE")?;
                text = Some(std::fs::read_to_string(&f).map_err(|e| anyhow::anyhow!("{f}: {e}"))?);
            }
            "-h" | "--help" => return say(format_args!("{KEYS_USAGE}\n")),
            k if k.starts_with("--") => anyhow::bail!("unknown: {k}\n{KEYS_USAGE}"),
            k => keys.push(k.to_string()),
        }
    }
    let req = match (keys.is_empty(), text, resume) {
        (true, None, true) => json!({ "resume": true }),
        (false, None, false) => json!({ "keys": keys.join(" "), "expect": expect }),
        (true, Some(t), false) => json!({ "text": t, "expect": expect }),
        _ => anyhow::bail!("keys, --text or --resume — one of them\n{KEYS_USAGE}"),
    };
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    // (no `--for`: the grant is this command's own, until it exits — never guessed wider: its parent is most often
    // your shell, and a grant to it would let everything you run there type)
    let of = of.or_else(|| env("TARAE_SHUTTLE_FOR").and_then(|v| v.parse().ok()));
    let mut hello = json!({});
    if let Some(of) = of {
        hello["for"] = json!(of);
    }
    if let Some(n) = name.or_else(|| env("TARAE_SHUTTLE_NAME")) {
        hello["name"] = json!(n);
    }
    let path = match pid {
        Some(p) => path_for(p).ok_or_else(|| anyhow::anyhow!("no state folder"))?,
        None => find()?,
    };
    let mut s = at(&path, |p: &Path| UnixStream::connect(p))
        .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    writeln!(s, "{}", json!({ "shuttle": hello }))?;
    let mut lines = BufReader::new(s.try_clone()?).lines();
    let mut next = || -> anyhow::Result<Value> {
        let line = lines.next().ok_or_else(|| anyhow::anyhow!("tarae closed the connection"))??;
        Ok(serde_json::from_str(&line)?)
    };
    let connected = next()?;
    if connected["shuttle"] != "connected" {
        return said_keys(&connected, json);
    }
    writeln!(s, "{req}")?;
    said_keys(&next()?, json)
}

/// `--expect 'ID F=V …'`: an element's id (`card#12@3`: at rev 3) and its fields, each value JSON or else a word.
fn expect_of(spec: &str) -> anyhow::Result<(String, Value)> {
    let mut words = spec.split_whitespace();
    let first = words.next().ok_or_else(|| anyhow::anyhow!("--expect: an element's id first"))?;
    let mut fields = serde_json::Map::new();
    let id = match first.rsplit_once('@').filter(|(_, rev)| rev.parse::<u64>().is_ok()) {
        Some((id, rev)) => {
            fields.insert("rev".into(), json!(rev.parse::<u64>().unwrap_or_default()));
            id
        }
        None => first,
    };
    for w in words {
        let (f, v) = w.split_once('=').ok_or_else(|| anyhow::anyhow!("--expect: {w:?} isn't FIELD=VALUE"))?;
        fields.insert(f.into(), serde_json::from_str(v).unwrap_or_else(|_| json!(v)));
    }
    Ok((id.to_string(), Value::Object(fields)))
}

/// A shuttle's answer printed — refused (or not taken): exit 1.
fn said_keys(v: &Value, json: bool) -> anyhow::Result<()> {
    let landed = v.get("landed").is_some() || v["shuttle"].is_string() && v.get("refused").is_none();
    match json {
        true => say(format_args!("{v}\n"))?,
        false => {
            let text = |k: &str| v[k].as_str().unwrap_or_default().to_string();
            let line = if let Some(e) = v["error"].as_str() {
                format!("error: {e}")
            } else if let Some(r) = v["refused"].as_str() {
                format!("refused ({r}): {} — keys → {}", text("why"), v["keys"]["to"].as_str().unwrap_or("?"))
            } else if v["taken"] == false {
                format!("not taken by {}: {}", text("to"), text("said"))
            } else if let Some(to) = v["landed"].as_str() {
                let said = v["said"].as_str().map(|s| format!(" · {s}")).unwrap_or_default();
                format!("landed on {to} ({}){said}", v["took"].as_str().unwrap_or("text"))
            } else {
                format!("shuttle {}", text("shuttle"))
            };
            say(format_args!("{line}\n"))?
        }
    }
    if !landed {
        std::process::exit(1);
    }
    Ok(())
}

/// One request, its answer.
fn ask(path: &Path, req: &Value) -> std::io::Result<Value> {
    let mut s = at(path, |p: &Path| UnixStream::connect(p))?;
    writeln!(s, "{req}")?;
    let mut line = String::new();
    BufReader::new(&s).read_line(&mut line)?;
    serde_json::from_str(&line).map_err(std::io::Error::other)
}

/// Which tarae: `$TARAE_UI`'s (a script run inside one), the only one running, or the one whose session
/// holds this folder — several and none does: they're listed.
fn find() -> anyhow::Result<PathBuf> {
    if let Some(p) = std::env::var_os("TARAE_UI") {
        return Ok(PathBuf::from(p));
    }
    let dir = crate::config::state_dir().ok_or_else(|| anyhow::anyhow!("no state folder"))?.join("ui");
    let socks: Vec<PathBuf> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "sock"))
        .collect();
    let here = std::env::current_dir().ok().map(|d| std::fs::canonicalize(&d).unwrap_or(d));
    let mut live = Vec::new();
    for p in socks {
        // (one that doesn't answer is a dead tarae's — left as it is: only its pid's next tarae removes it)
        let Ok(v) = ask(&p, &json!({ "get": "state", "timeout_ms": 2000 })) else { continue };
        if v.get("error").is_some() {
            continue;
        }
        let roots: Vec<PathBuf> = v["sessions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| s["root"].as_str())
            .map(PathBuf::from)
            .collect();
        let fit = here
            .as_ref()
            .and_then(|h| roots.iter().filter(|r| h.starts_with(r)).map(|r| r.as_os_str().len()).max());
        live.push((p, v, fit));
    }
    if live.len() == 1 {
        return Ok(live.remove(0).0);
    }
    if let Some((p, ..)) = live.iter().filter(|l| l.2.is_some()).max_by_key(|l| l.2) {
        return Ok(p.clone());
    }
    if live.is_empty() {
        anyhow::bail!("no tarae is running (none answers in {})", dir.display());
    }
    let mut list = String::from("several tarae run, none in this folder — --pid PID:\n");
    for (_, v, _) in &live {
        let front = v["front"].as_str().unwrap_or_default();
        let s = v["sessions"].as_array().into_iter().flatten().find(|s| s["id"] == front);
        let name = s.and_then(|s| s["name"].as_str()).unwrap_or_default();
        let root = s.and_then(|s| s["root"].as_str()).unwrap_or_default();
        list += &format!("  {:>7}  {name}  {root}\n", v["tarae"]["pid"]);
    }
    anyhow::bail!("{}", list.trim_end())
}
