//! The UI state's socket (`docs/design/ui-state.md`): `$state/ui/<pid>.sock`, one per tarae, on by default
//! — the folder 0700, the socket 0600 (bound under umask 077, then checked). A peer must be this user, and
//! is refused when it runs under one of Claude's processes or runs (Claude's runs can't reach it anyway:
//! Seatbelt lets them connect only to sockets in the project and their own `RunDir`). Read-only: snapshots,
//! one JSON line per request and per answer. Nobody asking, one thread sits in `accept` — no wakes.
//!
//! - `{"get":"state"}` → the state's JSON; `"format":"text"` → `{"text": …}`;
//! - `"after_input": N` holds the answer until `input` ≥ N and a frame after that (the keys sent were handled
//!   and drawn) — instead of sleeping after sending keys.
//!
//! `tarae state [--json] [--pid N] [--after-input N]` is the client (`cli`).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

/// How long a snapshot is waited for, unless the request says (`timeout_ms`, at most `WAIT_MAX`).
const WAIT: Duration = Duration::from_secs(10);
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
    for req in BufReader::new(&stream).lines() {
        let Ok(req) = req else { return };
        if req.trim().is_empty() {
            continue;
        }
        // (a watch takes the connection over: events a line until either end goes)
        if let Ok(v) = serde_json::from_str::<Value>(&req)
            && let Some(kinds) = v.get("watch")
        {
            drop(say);
            return watch(out, kinds, &line, &tx);
        }
        if !say(answer(&req, &line, &tx)) {
            return;
        }
    }
}

/// The kinds of events a watcher may ask for (`{"watch":[…]}`; none: all).
const KINDS: [&str; 6] = ["keys", "cards", "sessions", "turns", "runs", "toasts"];

/// `{"watch":["keys","cards",…]}`: this connection is told what changes, a JSON line an event, from the frame
/// after it's registered on (`uistate/watch.rs`). It ends when the reader goes — or reads too slowly: then its
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
        let claude = ed.claude_pids();
        if let Some(p) = line.iter().find(|p| claude.contains(p)) {
            let _ = ok.send(Err(format!("refused: under a process of Claude's (pid {p})")));
            ed.unchanged = true;
            return;
        }
        let at = (ed.frames, ed.input_seq);
        ed.ui_watch_add(crate::uistate::watch::Watcher { tx: events, kinds: asked, overflowed: flag });
        let _ = ok.send(Ok(at));
        // (no frame for it: what it's told comes with the frames drawn anyway)
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
    if req["get"] != "state" {
        return json!({ "error": "unknown request — {\"get\":\"state\"}" });
    }
    let text = req["format"] == "text";
    let after_input = req["after_input"].as_u64().unwrap_or(0);
    let wait = req["timeout_ms"].as_u64().map_or(WAIT, Duration::from_millis).min(WAIT_MAX);
    let (reply, got) = std::sync::mpsc::channel();
    let line = line.to_vec();
    let until = std::time::Instant::now() + wait;
    let refused = reply.clone();
    let job = move |ed: &mut crate::editor::Editor| {
        let claude = ed.claude_pids();
        if let Some(p) = line.iter().find(|p| claude.contains(p)) {
            let e = format!("refused: under a process of Claude's (pid {p}) — Claude has a view of its own");
            let _ = refused.send(json!({ "error": e }).to_string());
            // (nothing changed: no frame for it)
            ed.unchanged = true;
            return;
        }
        ed.ui_asks.push(crate::uistate::Ask { after_input, json: !text, reply, until });
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

/// `pid` and its parents, up to the first process (as far as the OS shows them).
fn ancestors(pid: u32) -> Vec<u32> {
    let mut v = vec![pid];
    while v.len() < 64 {
        match crate::procs::read(*v.last().expect("one")) {
            Some(os) if os.ppid > 1 && !v.contains(&os.ppid) => v.push(os.ppid),
            _ => break,
        }
    }
    v
}

// ── `tarae state` ────────────────────────────────────────────────────────

const USAGE: &str = "\
usage: tarae state [--json] [--pid PID] [--after-input N] [--watch [KINDS]]
  the UI state of a running tarae: where the keys go, the mode, the sessions, the cards, the panels,
  the runs, the toasts
  --json            as JSON (the schema: docs/design/ui-state.md)
  --pid PID         that tarae (else $TARAE_UI's, or the one whose session holds this folder)
  --after-input N   once N keys and clicks were handled and drawn (read `input` first, add what you send)
  --watch [KINDS]   what changes, a line an event, until interrupted — KINDS: keys,cards,sessions,turns,
                    runs,toasts (all when left out)";

pub fn cli(args: &[String]) -> anyhow::Result<()> {
    let (mut json, mut pid, mut after, mut watching) = (false, None, 0u64, None::<Vec<String>>);
    let mut it = args.iter().peekable();
    while let Some(a) = it.next() {
        let num = |v: Option<&String>| v.and_then(|v| v.parse::<u64>().ok());
        match a.as_str() {
            "--json" => json = true,
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
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
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
                true => println!("{line}"),
                false => println!("{:>6}  {}", v["frame"], v["said"].as_str().unwrap_or_default()),
            }
        }
        return Ok(());
    }
    let mut req = json!({ "get": "state", "after_input": after });
    if !json {
        req["format"] = json!("text");
    }
    let v = ask(&path, &req).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    if let Some(e) = v["error"].as_str() {
        anyhow::bail!("{e}");
    }
    match v["text"].as_str() {
        Some(t) if !json => print!("{t}"),
        _ => println!("{v}"),
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
