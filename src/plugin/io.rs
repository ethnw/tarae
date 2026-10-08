//! What plugins ask the world for — each only with its permission (`perm.rs`), and what comes back goes to
//! the plugin's `on-events`:
//!
//! - **Programs** (`spawn`): found on `PATH` (its absolute folders only), run with argv — no shell — in
//!   your home folder, in a process group of their own (`setsid` — stopping one stops what it started: `SIGTERM`, then
//!   `SIGKILL` 2 s later). They get a plain environment: `PATH`, `HOME`, `USER`, the locale, `TERM`,
//!   `TMPDIR`, and the variables the plugin may read — which it may also set for them (`AWS_PROFILE`). What one prints comes back a batch of lines at a
//!   time, then its end with the last lines of stderr (all of stderr goes to the log).
//! - **Connections** (`connect`, `send`): TCP, opened on a worker (each address in turn, 10 s in all),
//!   read on a thread of their own (`data`, up to 64 KB at a time), written on another (a big `send` never
//!   holds the editor up; 16 MB may wait to go out).
//! - **Files** (`read-file`): regular files only, read whole on a worker, up to 16 MB.
//! - **Timers** (`timer`): a `tick` later — every plugin's on one clock thread.
//!
//! A plugin has at most 16 programs, 16 connections and 256 timers at once. Each is the plugin's id's until
//! `stop` — or until the plugin is dropped or reloaded, or tarae quits: then programs are stopped and
//! connections closed. What an ended or replaced one still sends is dropped, and so is what was meant for a
//! plugin before it reloaded.
//!
//! What comes back while the plugin is busy waits, and goes to it in one call once it's done; while 4 MB of
//! output waits, its programs and connections aren't read (they wait too, rather than tarae's memory
//! growing).

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::abi::{Action, Connect, HostEvent, Permission, Spawn};
use super::host::Call;
use super::shown;
use crate::editor::Editor;
use crate::event::Event;

/// How many of a program's last stderr lines its end carries.
const ERR_TAIL: usize = 20;
/// How long connecting may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The most a connection's `data` carries at once.
const CHUNK: usize = 64 * 1024;
/// The biggest file a plugin reads.
const MAX_FILE: u64 = 16 << 20;
/// What may wait to be sent on a connection.
const MAX_SEND: usize = 16 << 20;
/// Output read for a plugin that it hasn't taken yet, past which its readers wait.
const MAX_BACKLOG: usize = 4 << 20;
/// At once, per plugin.
const MAX_PROCS: usize = 16;
const MAX_SOCKS: usize = 16;
const MAX_TIMERS: usize = 256;
/// How long a stopped program has before it's killed.
const KILL_AFTER: Duration = Duration::from_secs(2);

/// A thread, or why there isn't one (rather than a panic) — its name as short as `event::thread`'s.
fn thread(name: &str, f: impl FnOnce() + Send + 'static) -> Result<(), String> {
    crate::event::check_thread_name(name);
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(f)
        .map(drop)
        .map_err(|e| format!("couldn't start a thread: {e}"))
}

/// What's on its way to a plugin (one per load).
pub(super) struct Outbox {
    generation: u64,
    /// A call is in flight: what comes meanwhile waits in `queued`, and goes in one call after it.
    busy: bool,
    queued: Vec<HostEvent>,
    /// The output bytes in `queued`.
    weight: usize,
    flow: Arc<Flow>,
}

impl Outbox {
    pub(super) fn new(generation: u64) -> Outbox {
        Outbox { generation, busy: false, queued: Vec::new(), weight: 0, flow: Arc::default() }
    }
}

impl Drop for Outbox {
    fn drop(&mut self) {
        // Its readers stop waiting for it
        self.flow.gone.store(true, Ordering::SeqCst);
    }
}

/// Shared between a plugin's load and its readers: how much of what they read it hasn't taken yet.
#[derive(Default)]
struct Flow {
    backlog: AtomicUsize,
    gone: AtomicBool,
}

impl Flow {
    /// Waits while too much is waiting for the plugin; false if the plugin went meanwhile.
    fn wait(&self) -> bool {
        while self.backlog.load(Ordering::SeqCst) > MAX_BACKLOG {
            if self.gone.load(Ordering::SeqCst) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        !self.gone.load(Ordering::SeqCst)
    }

    fn add(&self, n: usize) {
        self.backlog.fetch_add(n, Ordering::SeqCst);
    }

    fn ease(&self, n: usize) {
        let _ = self.backlog.try_update(Ordering::SeqCst, Ordering::SeqCst, |b| Some(b.saturating_sub(n)));
    }
}

/// The output bytes an event carries (what the backlog counts).
fn weight(ev: &HostEvent) -> usize {
    match ev {
        HostEvent::Lines(_, lines) => lines.iter().map(|l| l.len() + 1).sum(),
        HostEvent::Data(_, bytes) => bytes.len(),
        _ => 0,
    }
}

type Due = Box<dyn FnOnce() + Send>;

/// One thread for every plugin's timers, and for killing programs that don't stop.
#[derive(Clone, Default)]
pub(super) struct Clock(Arc<(Mutex<ClockState>, Condvar)>);

#[derive(Default)]
struct ClockState {
    due: BTreeMap<(Instant, u64), Due>,
    serial: u64,
    running: bool,
}

impl Clock {
    /// Runs `f` at `when` (on the clock's thread — it must be quick); the key cancels it.
    fn at(
        &self,
        when: Instant,
        f: impl FnOnce((Instant, u64)) + Send + 'static,
    ) -> Result<(Instant, u64), String> {
        let (lock, cv) = &*self.0;
        let mut st = lock.lock().expect("never poisoned");
        if !st.running {
            let clock = self.0.clone();
            thread("tarae-plug-tick", move || tick(&clock))?;
            st.running = true;
        }
        st.serial += 1;
        let key = (when, st.serial);
        st.due.insert(key, Box::new(move || f(key)));
        cv.notify_one();
        Ok(key)
    }

    pub(super) fn cancel(&self, key: (Instant, u64)) {
        self.0.0.lock().expect("never poisoned").due.remove(&key);
    }
}

fn tick(clock: &(Mutex<ClockState>, Condvar)) {
    let (lock, cv) = clock;
    let mut st = lock.lock().expect("never poisoned");
    loop {
        let now = Instant::now();
        match st.due.first_key_value().map(|(k, _)| k.0) {
            Some(when) if when <= now => {
                let (_, f) = st.due.pop_first().expect("just seen");
                drop(st);
                f();
                st = lock.lock().expect("never poisoned");
            }
            Some(when) => st = cv.wait_timeout(st, when - now).expect("never poisoned").0,
            None => st = cv.wait(st).expect("never poisoned"),
        }
    }
}

/// An open connection: closed (both ways, so its reader ends too) when dropped.
pub(super) struct Sock {
    serial: u64,
    stream: TcpStream,
    /// To its writer thread, and what's waiting there.
    tx: mpsc::Sender<Vec<u8>>,
    queued: Arc<AtomicUsize>,
}

impl Drop for Sock {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

/// A running program: stopped (its whole group) when dropped.
pub(super) struct Proc {
    /// 0 once it ended.
    pid: u32,
    /// Tells this run's output from an earlier one with the same id.
    serial: u64,
    started: Instant,
    program: String,
    /// Set once it's been waited for.
    ended: Arc<AtomicBool>,
    clock: Clock,
}

impl Drop for Proc {
    fn drop(&mut self) {
        if self.pid == 0 {
            return;
        }
        crate::dap::term_group(self.pid);
        let (pid, ended) = (self.pid, self.ended.clone());
        let _ = self.clock.at(Instant::now() + KILL_AFTER, move |_| {
            if !ended.load(Ordering::SeqCst) {
                // SAFETY: only signals the process group of a program started in a process group of its own,
                // whose leader hasn't been waited for (so its id isn't anyone else's yet).
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
        });
    }
}

/// How many of `plugin`'s entries `map` holds, besides `id`'s.
fn others<V>(map: &HashMap<(String, u64), V>, plugin: &str, id: u64) -> usize {
    map.keys().filter(|(p, i)| p == plugin && *i != id).count()
}

/// Finds `program` in `PATH`'s absolute folders (a relative one, like `.`, would run what's in whatever
/// folder it runs in).
fn find_program(program: &str) -> Result<(PathBuf, std::ffi::OsString), String> {
    use std::os::unix::fs::PermissionsExt;
    let dirs: Vec<PathBuf> = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .filter(|d| d.is_absolute())
        .collect();
    let path = std::env::join_paths(&dirs).map_err(|e| e.to_string())?;
    let exe = dirs
        .iter()
        .map(|d| d.join(program))
        .find(|f| std::fs::metadata(f).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
        .ok_or_else(|| format!("{program} isn't on PATH"))?;
    Ok((exe, path))
}

/// The program a plugin's `spawn` names, started (`plugin_spawn_now`): found on `PATH`, in home, with the
/// environment its permissions pass on (`declared`) and what it sets (`env`: variables it may read — the caller
/// checked), stdin closed, stdout and stderr piped, in a session of its own — and kept from user namespaces
/// where Claude's runs are (`spawn::Acts::Plugin`: tarae's AppArmor profile isn't a plugin's).
pub(crate) fn plugin_process(
    program: &str,
    args: &[String],
    env: &[(String, String)],
    declared: &[Permission],
) -> Result<std::process::Child, String> {
    let (exe, path) = find_program(program)?;
    let home =
        std::env::var_os("HOME").map(PathBuf::from).filter(|h| h.is_dir()).unwrap_or_else(|| "/".into());
    let mut cmd = Command::new(exe);
    cmd.args(args)
        .env_clear()
        .envs(std::env::vars_os().filter(|(k, _)| k.to_str().is_some_and(|k| passed_on(k, declared))))
        .env("PATH", path)
        .envs(env.iter().map(|(k, v)| (k, v)))
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::spawn::keep_from_userns(&mut cmd, crate::spawn::Acts::Plugin).map_err(|e| e.to_string())?;
    crate::dap::setsid(&mut cmd).spawn().map_err(|e| format!("{}: {e}", shown(program)))
}

/// Whether a program a plugin runs gets this environment variable: the basics, and what it may read.
fn passed_on(name: &str, declared: &[Permission]) -> bool {
    matches!(name, "HOME" | "USER" | "LOGNAME" | "LANG" | "TERM" | "TMPDIR" | "TZ")
        || name.starts_with("LC_")
        || declared.iter().any(|p| matches!(p, Permission::Env(e) if e == name))
}

/// A regular file's bytes, up to `MAX_FILE` — not a device, a pipe or a socket (which could block or never
/// end), and not more than that even if it grows while being read.
fn read_file(path: &Path) -> Result<Vec<u8>, String> {
    use std::os::unix::fs::OpenOptionsExt;
    let at = |e: std::io::Error| format!("{}: {e}", path.display());
    let too_big = || format!("{} is over {} MB", path.display(), MAX_FILE >> 20);
    // (not blocking on a pipe with no writer as it opens)
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)
        .map_err(at)?;
    let m = f.metadata().map_err(at)?;
    if !m.is_file() {
        return Err(format!("{} isn't a regular file", path.display()));
    }
    if m.len() > MAX_FILE {
        return Err(too_big());
    }
    let mut bytes = Vec::with_capacity(m.len() as usize);
    f.take(MAX_FILE + 1).read_to_end(&mut bytes).map_err(at)?;
    if bytes.len() as u64 > MAX_FILE {
        return Err(too_big());
    }
    Ok(bytes)
}

/// Connects to each address `host` names in turn, until one answers — 10 s in all.
fn connect(host: &str, port: u16) -> Result<TcpStream, String> {
    let addr = if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") };
    let addrs: Vec<_> = (host, port).to_socket_addrs().map_err(|e| format!("{addr}: {e}"))?.collect();
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    let mut why = format!("{addr}: no address");
    for (i, a) in addrs.iter().enumerate() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        // (an address that doesn't answer leaves time for the others)
        let turn = (left / (addrs.len() - i) as u32).max(Duration::from_secs(1)).min(left);
        match TcpStream::connect_timeout(a, turn) {
            Ok(s) => return Ok(s),
            Err(e) => why = format!("{addr}: {e}"),
        }
    }
    Err(why)
}

impl Editor {
    /// A plugin's `spawn`, if it may (else its failure goes back to it).
    pub(super) fn plugin_spawn(&mut self, plugin: &str, sp: Spawn) -> Result<(), String> {
        let id = sp.id;
        let r = if sp.program.contains('/') || sp.program.chars().any(char::is_control) {
            Err(format!("“{}” isn't a program name", shown(&sp.program)))
        } else {
            self.plugin_may(plugin, &Permission::Run(sp.program.clone()))
                .and_then(|()| self.plugin_spawn_now(plugin, sp))
        };
        if let Err(why) = r {
            self.plugin_events(plugin, vec![HostEvent::Failed(id, why)]);
        }
        Ok(())
    }

    /// Reads a file for a plugin (it may — checked), on a worker: its bytes, or why not, come back.
    pub(super) fn plugin_read_now(&mut self, plugin: &str, id: u64, path: PathBuf) {
        let Some(generation) = self.plugin_generation(plugin) else { return };
        let plugin = plugin.to_string();
        self.events.jobs().spawn(move || {
            let ev = match read_file(&path) {
                Ok(bytes) => HostEvent::File(id, bytes),
                Err(why) => HostEvent::Failed(id, why),
            };
            move |ed: &mut Editor| {
                // (for this load of it, not one since)
                if ed.plugin_generation(&plugin) == Some(generation) {
                    ed.plugin_events(&plugin, vec![ev]);
                }
            }
        });
    }

    /// Which load of `plugin` is current.
    fn plugin_generation(&self, plugin: &str) -> Option<u64> {
        self.plugins.as_ref()?.outbox.get(plugin).map(|o| o.generation)
    }

    /// Connects for a plugin (it may — checked), on a worker; `connected` or `failed` comes back.
    pub(super) fn plugin_connect_now(&mut self, plugin: &str, c: Connect) {
        let Some(p) = &mut self.plugins else { return };
        if others(&p.socks, plugin, c.id) + others(&p.pending, plugin, c.id) >= MAX_SOCKS {
            let why = format!("it has {MAX_SOCKS} connections open already");
            return self.plugin_events(plugin, vec![HostEvent::Failed(c.id, why)]);
        }
        p.serial += 1;
        let serial = p.serial;
        // (claimed now: a `stop` before it's open drops it; one with the same id is replaced)
        let key = (plugin.to_string(), c.id);
        p.socks.remove(&key);
        p.pending.insert(key, serial);
        let plugin = plugin.to_string();
        self.events.jobs().spawn(move || {
            let opened = connect(&c.host, c.port as u16);
            move |ed: &mut Editor| ed.plugin_connected(&plugin, c.id, serial, opened)
        });
    }

    /// A connection opened (or didn't): kept, its reader and writer started, and the plugin told.
    fn plugin_connected(&mut self, plugin: &str, id: u64, serial: u64, opened: Result<TcpStream, String>) {
        let Some(p) = &mut self.plugins else { return };
        let key = (plugin.to_string(), id);
        if p.pending.get(&key) != Some(&serial) {
            return; // stopped meanwhile (or the plugin went): dropping it closes it
        }
        p.pending.remove(&key);
        let Some(flow) = p.outbox.get(plugin).map(|o| o.flow.clone()) else { return };
        let started = opened.and_then(|stream| {
            let _ = stream.set_nodelay(true);
            let reader = stream.try_clone().map_err(|e| e.to_string())?;
            let writer = stream.try_clone().map_err(|e| e.to_string())?;
            let (tx, rx) = mpsc::channel::<Vec<u8>>();
            let queued = Arc::new(AtomicUsize::new(0));
            let sock = Sock { serial, stream, tx, queued: queued.clone() };
            thread("tarae-plug-send", move || {
                let mut w = writer;
                while let Ok(bytes) = rx.recv() {
                    let ok = w.write_all(&bytes).is_ok();
                    queued.fetch_sub(bytes.len(), Ordering::SeqCst);
                    if !ok {
                        break;
                    }
                }
            })?;
            let events = self.events.shared_sender();
            let who = plugin.to_string();
            thread("tarae-plug-read", move || {
                let mut r = reader;
                let mut buf = vec![0u8; CHUNK];
                let send = |ev: HostEvent| {
                    let (who, flow) = (who.clone(), flow.clone());
                    let w = weight(&ev);
                    flow.add(w);
                    let job = move |ed: &mut Editor| {
                        ed.plugin_socket_event(&who, serial, ev, &flow, w);
                        ed.unchanged = true; // (for the plugin: nothing on screen yet)
                    };
                    events.send(Event::Job(Box::new(job))).is_ok()
                };
                let why = loop {
                    if !flow.wait() {
                        return;
                    }
                    match r.read(&mut buf) {
                        Ok(0) => break String::new(),
                        Ok(n) => {
                            if !send(HostEvent::Data(id, buf[..n].to_vec())) {
                                return;
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(e) => break e.to_string(),
                    }
                };
                send(HostEvent::Closed(id, why));
            })?;
            Ok(sock)
        });
        match started {
            Ok(sock) => {
                if let Some(p) = &mut self.plugins {
                    p.socks.insert(key, sock);
                }
                self.plugin_events(plugin, vec![HostEvent::Connected(id)]);
            }
            Err(why) => self.plugin_events(plugin, vec![HostEvent::Failed(id, why)]),
        }
    }

    /// From connection run `serial`: to the plugin, if it's still its.
    fn plugin_socket_event(&mut self, plugin: &str, serial: u64, ev: HostEvent, flow: &Flow, w: usize) {
        let id = match &ev {
            HostEvent::Data(id, _) | HostEvent::Closed(id, _) => *id,
            _ => return flow.ease(w),
        };
        let key = (plugin.to_string(), id);
        let Some(p) = self.plugins.as_mut().filter(|p| p.socks.get(&key).is_some_and(|s| s.serial == serial))
        else {
            return flow.ease(w);
        };
        if matches!(ev, HostEvent::Closed(..)) {
            p.socks.remove(&key);
        }
        self.plugin_queue(plugin, vec![ev], w);
    }

    /// A plugin's `send`: to its writer (a closed or unknown one fails, and so does one with too much
    /// waiting to go out).
    pub(super) fn plugin_send(&mut self, plugin: &str, id: u64, bytes: Vec<u8>) {
        let sock = self.plugins.as_ref().and_then(|p| p.socks.get(&(plugin.to_string(), id)));
        let why = match sock {
            None => Some("no open connection with that id".to_string()),
            Some(s) if s.queued.load(Ordering::SeqCst) + bytes.len() > MAX_SEND => {
                Some(format!("over {} MB waiting to be sent", MAX_SEND >> 20))
            }
            Some(s) => {
                let n = bytes.len();
                s.queued.fetch_add(n, Ordering::SeqCst);
                match s.tx.send(bytes) {
                    Ok(()) => None,
                    Err(_) => {
                        s.queued.fetch_sub(n, Ordering::SeqCst);
                        Some("the connection closed".to_string())
                    }
                }
            }
        };
        if let Some(why) = why {
            self.plugin_events(plugin, vec![HostEvent::Failed(id, why)]);
        }
    }

    /// A plugin's `timer`: a `tick` in `ms` (unless stopped, or set again, meanwhile).
    pub(super) fn plugin_timer(&mut self, plugin: &str, id: u64, ms: u64) {
        let Some(p) = &mut self.plugins else { return };
        let key = (plugin.to_string(), id);
        if let Some(old) = p.timers.remove(&key) {
            p.clock.cancel(old);
        }
        if others(&p.timers, plugin, id) >= MAX_TIMERS {
            let why = format!("it has {MAX_TIMERS} timers set already");
            return self.plugin_events(plugin, vec![HostEvent::Failed(id, why)]);
        }
        let events = self.events.shared_sender();
        let who = plugin.to_string();
        // (a day at most)
        let when = Instant::now() + Duration::from_millis(ms.min(86_400_000));
        let set = p.clock.at(when, move |at| {
            let job = move |ed: &mut Editor| {
                let Some(p) = &mut ed.plugins else { return };
                let key = (who.clone(), id);
                if p.timers.get(&key) == Some(&at) {
                    p.timers.remove(&key);
                    ed.plugin_events(&who, vec![HostEvent::Tick(id)]);
                }
                ed.unchanged = true; // (for the plugin: nothing on screen yet)
            };
            let _ = events.send(Event::Job(Box::new(job)));
        });
        match set {
            Ok(at) => {
                p.timers.insert(key, at);
            }
            Err(why) => self.plugin_events(plugin, vec![HostEvent::Failed(id, why)]),
        }
    }

    fn plugin_spawn_now(&mut self, plugin: &str, sp: Spawn) -> Result<(), String> {
        let p = self.plugins.as_ref().ok_or("plugins aren't running")?;
        if others(&p.procs, plugin, sp.id) >= MAX_PROCS {
            return Err(format!("it has {MAX_PROCS} programs running already"));
        }
        let flow = p.outbox.get(plugin).map(|o| o.flow.clone()).ok_or("it isn't loaded")?;
        let declared = p.permissions.get(plugin).cloned().unwrap_or_default();
        // It sets only variables it may read — not `PATH`, `DYLD_INSERT_LIBRARIES` and the like
        if let Some((k, _)) = sp.env.iter().find(|(k, _)| !declared.contains(&Permission::Env(k.clone()))) {
            return Err(format!("it may set only variables it may read, not {} (permissions())", shown(k)));
        }
        let mut child = plugin_process(&sp.program, &sp.args, &sp.env, &declared)?;
        let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
        let tail = Arc::new(Mutex::new(VecDeque::new()));
        let ended = Arc::new(AtomicBool::new(false));
        let who = plugin.to_string();
        let id = sp.id;
        if let Some(err) = stderr {
            let (tail, name) = (tail.clone(), who.clone());
            let started = thread("tarae-plug-err", move || {
                crate::task::read_batches(err, |lines| {
                    let mut t = tail.lock().expect("never poisoned");
                    for l in lines {
                        crate::log::line(&name, "stderr", &l);
                        t.push_back(l);
                        if t.len() > ERR_TAIL {
                            t.pop_front();
                        }
                    }
                    true
                });
            });
            if let Err(why) = started {
                crate::dap::kill_group(&mut child);
                return Err(why);
            }
        }
        let Some(p) = &mut self.plugins else { return Ok(()) };
        p.serial += 1;
        let serial = p.serial;
        let pid = child.id();
        let events = self.events.shared_sender();
        let done = ended.clone();
        thread("tarae-plug-run", move || {
            let reader_flow = flow.clone();
            let sender = move || {
                let (who, flow, events) = (who.clone(), flow.clone(), events.clone());
                move |ev: HostEvent| {
                    let (who, flow) = (who.clone(), flow.clone());
                    let w = weight(&ev);
                    flow.add(w);
                    let job = move |ed: &mut Editor| {
                        ed.plugin_process_from(&who, serial, ev, &flow, w);
                        ed.unchanged = true; // (for the plugin: nothing on screen yet)
                    };
                    events.send(Event::Job(Box::new(job))).is_ok()
                }
            };
            // Its output read beside the wait: the program is reaped as it ends, whatever still holds its
            // output (#131), and what's left to read gets `DRAIN_AFTER_EXIT`
            let (eof, read_all) = std::sync::mpsc::channel();
            let readers = usize::from(stdout.is_some());
            if let Some(out) = stdout {
                let (send, flow) = (sender(), reader_flow);
                // (not started: `eof` gone with it, the wait below doesn't wait)
                let _ = thread("tarae-plug-out", move || {
                    crate::task::read_batches(out, |lines| flow.wait() && send(HostEvent::Lines(id, lines)));
                    let _ = eof.send(());
                });
            }
            let send = sender();
            let code = child.wait().ok().and_then(|s| s.code()).map(|c| c as u64);
            for _ in 0..readers {
                let _ = read_all.recv_timeout(crate::spawn::DRAIN_AFTER_EXIT);
            }
            done.store(true, Ordering::SeqCst);
            // (stderr's reader is done by now, or nearly: what it has is what there is)
            let err: Vec<String> = tail.lock().expect("never poisoned").iter().cloned().collect();
            send(HostEvent::Exit(id, code, err));
        })?;
        // (one with the same id before: stopped)
        let proc =
            Proc { pid, serial, started: Instant::now(), program: sp.program, ended, clock: p.clock.clone() };
        p.procs.insert((plugin.to_string(), id), proc);
        Ok(())
    }

    /// The plugins' programs running: (plugin, its id for it, process, program) — `:procs`.
    pub(crate) fn plugin_pids(&self) -> Vec<(String, u64, u32, String)> {
        let Some(p) = &self.plugins else { return Vec::new() };
        p.procs
            .iter()
            .filter(|(_, r)| r.pid != 0)
            .map(|((plugin, id), r)| (plugin.clone(), *id, r.pid, r.program.clone()))
            .collect()
    }

    /// Output from run `serial` of a plugin's program: to the plugin, if that run is still its.
    fn plugin_process_from(&mut self, plugin: &str, serial: u64, ev: HostEvent, flow: &Flow, w: usize) {
        let id = match &ev {
            HostEvent::Lines(id, _) | HostEvent::Exit(id, ..) => *id,
            _ => return flow.ease(w),
        };
        let key = (plugin.to_string(), id);
        let Some(p) = self.plugins.as_mut().filter(|p| p.procs.get(&key).is_some_and(|r| r.serial == serial))
        else {
            return flow.ease(w);
        };
        if let HostEvent::Exit(_, code, _) = &ev
            && let Some(mut r) = p.procs.remove(&key)
        {
            r.pid = 0; // it ended: nothing to stop
            let took = r.started.elapsed().as_millis();
            let code = code.map_or("a signal".to_string(), |c| c.to_string());
            crate::log::line(plugin, "exit", &format!("{} ended ({code}) after {took} ms", r.program));
        }
        self.plugin_queue(plugin, vec![ev], w);
    }

    /// `plugin_events`, for the editor's other parts (an install a plugin asked for).
    pub(crate) fn plugin_events_from(&mut self, plugin: &str, events: Vec<HostEvent>) {
        self.plugin_events(plugin, events);
    }

    /// Tells a plugin what came back from what it asked for, and applies its answer to the buffer in front.
    pub(super) fn plugin_events(&mut self, plugin: &str, events: Vec<HostEvent>) {
        self.plugin_queue(plugin, events, 0);
    }

    /// `events` (carrying `w` bytes of output) to the plugin: now, or after the call in flight.
    fn plugin_queue(&mut self, plugin: &str, events: Vec<HostEvent>, w: usize) {
        let Some(o) = self.plugins.as_mut().and_then(|p| p.outbox.get_mut(plugin)) else { return };
        o.queued.extend(events);
        o.weight += w;
        self.plugin_flush(plugin);
    }

    /// What waits for a plugin, in one call — unless one is in flight.
    fn plugin_flush(&mut self, plugin: &str) {
        let doc = self.doc().id;
        let Some(o) = self.plugins.as_ref().and_then(|p| p.outbox.get(plugin)) else { return };
        if o.busy || o.queued.is_empty() {
            return;
        }
        let Some((input, seen)) = self.plugin_input(doc, "") else { return };
        let Some(p) = &mut self.plugins else { return };
        let Some(o) = p.outbox.get_mut(plugin) else { return };
        let events = std::mem::take(&mut o.queued);
        // A key in its panel, a line you typed: what it answers is an answer to you
        let yours = events.iter().any(|e| matches!(e, HostEvent::Key(..) | HostEvent::Answer(_, Some(_))));
        let w = std::mem::take(&mut o.weight);
        let (generation, flow) = (o.generation, o.flow.clone());
        o.busy = true;
        let tx = self.events.shared_sender();
        let who = plugin.to_string();
        let taken = flow.clone();
        let reply = Box::new(move |result: Result<Vec<Action>, String>| {
            let apply = move |ed: &mut Editor| {
                taken.ease(w);
                // An answer for an earlier load of it: dropped
                let Some(o) = ed.plugins.as_mut().and_then(|p| p.outbox.get_mut(&who)) else { return };
                if o.generation != generation {
                    return;
                }
                o.busy = false;
                match result {
                    Ok(actions) => {
                        if let Some(p) = ed.plugins.as_mut().filter(|_| yours) {
                            p.answering = Some(who.clone());
                        }
                        // Only I/O asked for, and nothing said about it: not drawn (a watch's every change)
                        let quiet = actions.iter().all(Action::unseen);
                        let said = (ed.status.clone(), ed.toasts.len());
                        ed.plugin_apply(&who, &seen, actions);
                        if let Some(p) = &mut ed.plugins {
                            p.answering = None;
                        }
                        if quiet && said == (ed.status.clone(), ed.toasts.len()) {
                            ed.unchanged = true;
                        }
                    }
                    Err(e) => ed.set_error(format!("{who}: {}", shown(&e))),
                }
                ed.plugin_flush(&who);
            };
            let _ = tx.send(Event::Job(Box::new(apply)));
        });
        if let Err(e) = p.host.call(plugin, Call::Events(generation, events), input, reply) {
            flow.ease(w);
            if let Some(o) = p.outbox.get_mut(plugin) {
                o.busy = false;
            }
            self.set_error(format!("{plugin}: {e}"));
        }
    }

    /// A plugin's `find`: the project's files `pattern` matches (from its folder; `**` any folders, `*` any
    /// name) — listed on a worker (as the file picker lists them: what git ignores, left out), up to 2000.
    /// Not ones named like keys and tokens (`secretnames`): `project` doesn't read them.
    pub(super) fn plugin_find(&mut self, plugin: &str, id: u64, pattern: &str) {
        let why = match self.plugin_may(plugin, &Permission::Project) {
            Err(why) => Some(why),
            Ok(())
                if pattern.starts_with('/')
                    || pattern.split('/').any(|c| c == "..")
                    || pattern.len() > 200 =>
            {
                Some(format!("“{}”: a pattern from the project's folder", shown(pattern)))
            }
            Ok(()) => None,
        };
        // (the project's folder — the editor's root — is what the `project` permission reads)
        if let Some(why) = why {
            return self.plugin_events(plugin, vec![HostEvent::Failed(id, why)]);
        }
        let root = self.session.root.clone();
        let Some(generation) = self.plugin_generation(plugin) else { return };
        let (plugin, pattern) = (plugin.to_string(), pattern.to_string());
        self.events.jobs().spawn(move || {
            let whole = format!("{}/{pattern}", root.display());
            let found: Vec<String> = crate::picker::list_files(&root)
                .into_iter()
                .map(|rel| root.join(rel))
                .filter(|p| super::perm::path_matches(&whole, p) && !crate::secretnames::named_secret(p))
                .take(2000)
                .map(|p| p.display().to_string())
                .collect();
            move |ed: &mut Editor| {
                if ed.plugin_generation(&plugin) == Some(generation) {
                    ed.plugin_events(&plugin, vec![HostEvent::Found(id, found)]);
                }
            }
        });
    }

    /// A plugin's `write-own`: a file in its own folder (`plugins/<name>/` in the state folder) — a plain
    /// relative name, up to 1 MB.
    pub(super) fn plugin_write_own(&mut self, plugin: &str, id: u64, name: &str, bytes: &[u8]) {
        let plain = !name.is_empty()
            && name.len() <= 200
            && std::path::Path::new(name).components().all(|c| matches!(c, std::path::Component::Normal(_)))
            && !name.chars().any(|c| c.is_control() || super::bidi(c));
        let own = self.plugins.as_ref().and_then(|p| p.own_root.clone());
        let ev = match (own, plain, bytes.len() <= 1 << 20) {
            (None, ..) => HostEvent::Failed(id, "no state folder to keep it in".into()),
            (_, false, _) => {
                HostEvent::Failed(id, format!("“{}” isn't a plain name in its folder", shown(name)))
            }
            (_, _, false) => HostEvent::Failed(id, "over 1 MB".into()),
            (Some(own), true, true) => {
                let path = own.join(plugin).join(name);
                // (the plugin's alone: what it keeps there may be its secrets — a release's values, say)
                use std::os::unix::fs::PermissionsExt;
                let wrote = path
                    .parent()
                    .map_or(Ok(()), std::fs::create_dir_all)
                    .and_then(|()| std::fs::write(&path, bytes))
                    .and_then(|()| std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)));
                match wrote {
                    Ok(()) => HostEvent::Written(id, path.display().to_string()),
                    Err(e) => HostEvent::Failed(id, format!("{}: {e}", path.display())),
                }
            }
        };
        self.plugin_events(plugin, vec![ev]);
    }

    /// A plugin's `stop`.
    pub(super) fn plugin_stop(&mut self, plugin: &str, id: u64) {
        if let Some(p) = &mut self.plugins {
            // Dropped: stopped or closed — and what it still sends no longer reaches the plugin
            let key = (plugin.to_string(), id);
            p.procs.remove(&key);
            p.socks.remove(&key);
            p.pending.remove(&key);
            if let Some(at) = p.timers.remove(&key) {
                p.clock.cancel(at);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_regular_files_are_read_and_no_more_than_the_limit() {
        let dir = std::env::temp_dir().join(format!("tarae-io-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, "hello").unwrap();
        assert_eq!(read_file(&file).unwrap(), b"hello");
        // A pipe with no writer (opening it would block), a device that never ends
        let fifo = dir.join("fifo");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: a plain mkfifo on a path of ours.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        for special in [fifo.as_path(), Path::new("/dev/zero")] {
            let why = read_file(special).unwrap_err();
            assert!(why.contains("isn't a regular file"), "{why}");
        }
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn programs_get_the_basics_and_what_the_plugin_may_read() {
        let declared = [Permission::Env("KUBECONFIG".into())];
        for name in ["HOME", "LANG", "LC_ALL", "TERM", "KUBECONFIG"] {
            assert!(passed_on(name, &declared), "{name}");
        }
        for name in
            ["AWS_SECRET_ACCESS_KEY", "GITHUB_TOKEN", "SSH_AUTH_SOCK", "LD_PRELOAD", "DYLD_INSERT_LIBRARIES"]
        {
            assert!(!passed_on(name, &declared), "{name}");
        }
        let (exe, path) = find_program("sh").unwrap();
        assert!(exe.is_absolute() && exe.ends_with("sh"));
        assert!(std::env::split_paths(&path).all(|d| d.is_absolute()));
        assert!(find_program("tarae-no-such-program").is_err());
    }

    #[test]
    fn the_clock_runs_what_is_due_in_order_and_not_what_was_cancelled() {
        let clock = Clock::default();
        let (tx, rx) = mpsc::channel();
        let now = Instant::now();
        for (ms, name) in [(60, "late"), (20, "early"), (40, "cancelled")] {
            let tx = tx.clone();
            let key = clock.at(now + Duration::from_millis(ms), move |_| tx.send(name).unwrap()).unwrap();
            if name == "cancelled" {
                clock.cancel(key);
            }
        }
        let got: Vec<_> = (0..2).map(|_| rx.recv_timeout(Duration::from_secs(5)).unwrap()).collect();
        assert_eq!(got, ["early", "late"]);
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
    }

    /// Easing takes off what was taken, from many threads at once, and never below nothing (a late count
    /// mustn't wrap around and stall every reader).
    #[test]
    fn easing_the_backlog_adds_up_and_stops_at_nothing() {
        let flow = Arc::new(Flow::default());
        flow.add(8000);
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let flow = flow.clone();
                std::thread::spawn(move || (0..500).for_each(|_| flow.ease(1)))
            })
            .collect();
        threads.into_iter().for_each(|t| t.join().unwrap());
        assert_eq!(flow.backlog.load(Ordering::SeqCst), 4000);
        flow.ease(5000);
        assert_eq!(flow.backlog.load(Ordering::SeqCst), 0, "not below nothing");
    }

    #[test]
    fn readers_wait_while_the_plugin_is_behind_and_stop_when_it_goes() {
        let flow = Arc::new(Flow::default());
        assert!(flow.wait());
        flow.add(MAX_BACKLOG + 1);
        let waiting = {
            let flow = flow.clone();
            std::thread::spawn(move || flow.wait())
        };
        std::thread::sleep(Duration::from_millis(30));
        assert!(!waiting.is_finished(), "waits while too much is waiting");
        flow.ease(MAX_BACKLOG + 1);
        assert!(waiting.join().unwrap(), "goes on once the plugin took it");
        flow.add(MAX_BACKLOG + 1);
        let waiting = {
            let flow = flow.clone();
            std::thread::spawn(move || flow.wait())
        };
        let mut outbox = Outbox::new(1);
        outbox.flow = flow.clone();
        drop(outbox);
        assert!(!waiting.join().unwrap(), "the plugin went: it stops");
    }
}
