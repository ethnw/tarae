//! tarae's log — `$XDG_STATE_HOME/tarae/tarae.log` (`:log-open`). What language servers print on stderr, when
//! they start, get ready, and exit, what they report (`window/logMessage`, `showMessage`), and every
//! message the editor shows. Callable from any thread: a line goes over a channel to a writer thread,
//! so nothing waits on the disk. The file is started fresh each run (the previous one is kept as `.old`).
//! Tests write nothing.
//!
//! One log for every session: a line about one session (its runs, cards, denials, keeper, servers, Claude's
//! sizes) goes through `Session::line` and names it in a column of its own — `@linux7` after the time; tarae's
//! own lines (`line`) leave it blank. `grep @linux7 tarae.log` is one session's story. Past `KEPT` bytes the
//! oldest lines of whoever holds the most go first (`Kept`), so a busy session can't push a quiet one out.

use std::collections::{BTreeMap, VecDeque};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};

pub fn path() -> Option<PathBuf> {
    crate::config::state_dir().map(|d| d.join("tarae.log"))
}

/// Whether `p` is the log — open, it's followed quietly (a toast about it would be logged, change the file
/// again, and loop).
pub fn is_log(p: Option<&std::path::Path>) -> bool {
    p.is_some_and(|p| path().is_some_and(|l| l == p || std::fs::canonicalize(&l).is_ok_and(|l| l == p)))
}

/// Whether `real` (a real path) is the log itself — Claude reads it unasked (#45: servers' progress and
/// tarae's own messages are there); on a worker (the log's own path is resolved).
pub fn is_log_real(real: &std::path::Path) -> bool {
    let Some(log) = path() else { return false };
    let log = std::fs::canonicalize(&log).ok().or_else(|| crate::paths::real(&log));
    log.is_some_and(|l| l == real)
}

/// What the log keeps of this run, in bytes — past it, lines go (`Kept::push`).
const KEPT: usize = 2 << 20;

/// One line on its way to the file: its text as written, and whose it is (None: tarae's own).
struct Entry {
    who: Option<Arc<str>>,
    text: String,
}

fn sender() -> Option<&'static Sender<Entry>> {
    static TX: OnceLock<Option<Sender<Entry>>> = OnceLock::new();
    TX.get_or_init(|| {
        if cfg!(test) {
            return None;
        }
        let path = path()?;
        let (tx, rx) = channel::<Entry>();
        crate::event::thread("tarae-log", move || keep(&path, rx, KEPT));
        Some(tx)
    })
    .as_ref()
}

/// The writer: the last run's log moved to `.old`, a fresh file, then every line as it comes — appended, or,
/// once past `cap`, the file written again whole with what `Kept` keeps (through `.log.new` and a rename: a
/// reader never sees half of it). Returns when every sender is gone.
fn keep(path: &Path, rx: Receiver<Entry>, cap: usize) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::rename(path, path.with_extension("log.old"));
    let Ok(mut file) = std::fs::File::create(path) else { return };
    let (y, mo, d, h, m, sec) = now();
    let started =
        format!("tarae {} · started {y}-{mo:02}-{d:02} {h:02}:{m:02}:{sec:02}", env!("CARGO_PKG_VERSION"));
    let _ = write!(file, "{started}\n\n");
    let mut kept = Kept::new(cap);
    while let Ok(first) = rx.recv() {
        // Whatever else arrived meanwhile goes in the same write, then flush once — the batch as it came (a
        // trim may drop some of it from what's kept: the file gets them still, unless it's written again whole)
        let (mut came, mut trimmed) = (String::new(), false);
        for e in std::iter::once(first).chain(rx.try_iter()) {
            came.push_str(&e.text);
            trimmed |= kept.push(e);
        }
        let append = |file: &mut std::fs::File| {
            let _ = file.write_all(came.as_bytes());
            let _ = file.flush();
        };
        if !trimmed {
            append(&mut file);
            continue;
        }
        let head = format!("{started} · {} older lines of the busiest left out\n\n", kept.dropped);
        let new = path.with_extension("log.new");
        let whole = std::fs::File::create(&new).and_then(|mut f| {
            f.write_all(head.as_bytes())?;
            for e in &kept.lines {
                f.write_all(e.text.as_bytes())?;
            }
            f.flush()?;
            std::fs::rename(&new, path)?;
            Ok(f)
        });
        match whole {
            Ok(f) => file = f,
            // (can't be written again — its folder unwritable, the disk full: the batch is appended as it came,
            // each line once; the file outgrows the cap until a rewrite works, and nothing written is lost)
            Err(_) => {
                let _ = std::fs::remove_file(&new);
                append(&mut file);
            }
        }
    }
}

/// The lines of this run's log, each with whose it is, within `cap` bytes. Past the cap it goes down to three
/// quarters of it, each time the oldest line of whoever holds the most bytes then — a session, or tarae's own
/// lines as one more. Not a share per session: how many sessions there'll be isn't known, and one alone would
/// waste the rest. This way one holding less than an even share never loses a line, however busy the others;
/// a busy one loses its own oldest first; the whole cap is used whoever's there.
struct Kept {
    cap: usize,
    lines: VecDeque<Entry>,
    bytes: usize,
    /// Lines left out so far (the file's first line says it).
    dropped: usize,
}

impl Kept {
    fn new(cap: usize) -> Self {
        Self { cap, lines: VecDeque::new(), bytes: 0, dropped: 0 }
    }

    /// One more line — true: that took it past the cap, and lines went.
    fn push(&mut self, e: Entry) -> bool {
        self.bytes += e.text.len();
        self.lines.push_back(e);
        if self.bytes <= self.cap {
            return false;
        }
        self.trim(self.cap / 4 * 3);
        true
    }

    /// Down to `to` bytes, the oldest line of the one holding the most first.
    fn trim(&mut self, to: usize) {
        // (whose → bytes held, its lines' places oldest first)
        let mut held: BTreeMap<Option<Arc<str>>, (usize, VecDeque<usize>)> = BTreeMap::new();
        for (i, e) in self.lines.iter().enumerate() {
            let h = held.entry(e.who.clone()).or_default();
            h.0 += e.text.len();
            h.1.push_back(i);
        }
        let mut gone = vec![false; self.lines.len()];
        while self.bytes > to {
            let Some((bytes, places)) = held.values_mut().max_by_key(|(b, _)| *b) else { break };
            let Some(i) = places.pop_front() else { break };
            let len = self.lines[i].text.len();
            *bytes -= len;
            self.bytes -= len;
            self.dropped += 1;
            gone[i] = true;
        }
        let mut i = 0;
        self.lines.retain(|_| {
            i += 1;
            !gone[i - 1]
        });
    }
}

/// Whose a line is: a session's — its name in the line's own column (`Session::line`) — or tarae's own (`line`).
/// Every line about one session goes through here, never its name written into the text by hand.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session(Option<Arc<str>>);

impl Session {
    /// The session called `name` (`session::Session::log`).
    pub fn named(name: &str) -> Self {
        Self(Some(name.into()))
    }

    /// No session's: tarae's own line (what's several sessions' at once — a server they share).
    pub fn none() -> Self {
        Self(None)
    }

    /// The session whose checkout holds `path` (a real path: a run's root, a keeper's, a server's) — the
    /// innermost when checkouts nest; none of them: tarae's own (the line goes in blank).
    pub fn of(path: &Path) -> Self {
        let c = checkouts();
        let best = c.iter().filter(|c| path.starts_with(&c.1)).max_by_key(|c| c.1.as_os_str().len());
        Self(best.map(|c| c.2.clone()))
    }

    /// Its name — None: tarae's own.
    pub fn name(&self) -> Option<&str> {
        self.0.as_deref()
    }

    /// One line about this session: `12:03:04  @linux7      sandbox          info     message`.
    pub fn line(&self, source: &str, level: &str, text: &str) {
        said(self.0.as_ref(), source, level, text);
    }
}

/// Every live session's checkout as `Session::of` finds it: (key, real root, name).
static CHECKOUTS: Mutex<Vec<(u64, PathBuf, Arc<str>)>> = Mutex::new(Vec::new());

fn checkouts() -> std::sync::MutexGuard<'static, Vec<(u64, PathBuf, Arc<str>)>> {
    CHECKOUTS.lock().unwrap_or_else(|e| e.into_inner())
}

/// A session's checkout, known to the log while it lives (`session::Session::new`): lines about a path in it
/// name it. Dropped with its session, it's forgotten.
#[derive(Debug)]
pub struct Checkout(u64);

impl Checkout {
    pub fn new(root: &Path, name: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let key = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        checkouts().push((key, root.to_path_buf(), name.into()));
        Self(key)
    }

    /// Its session is called `name` from now on (the main checkout's label: its branch, once known).
    pub fn named(&self, name: &str) {
        if let Some(c) = checkouts().iter_mut().find(|c| c.0 == self.0)
            && &*c.2 != name
        {
            c.2 = name.into();
        }
    }
}

impl Drop for Checkout {
    fn drop(&mut self) {
        checkouts().retain(|c| c.0 != self.0);
    }
}

/// Tests: the lines `line` was given (every thread's) while any test captures them — tests write no log, so what a
/// line would carry is read here (`capture`) — and the captures under way, each its id and where its lines start.
#[cfg(test)]
struct Captured {
    lines: Vec<String>,
    live: Vec<(u64, usize)>,
    next: u64,
}

#[cfg(test)]
static CAPTURED: std::sync::Mutex<Captured> =
    std::sync::Mutex::new(Captured { lines: Vec::new(), live: Vec::new(), next: 0 });

#[cfg(test)]
fn captured() -> std::sync::MutexGuard<'static, Captured> {
    CAPTURED.lock().unwrap_or_else(|e| e.into_inner())
}

/// Tests: every line from here on, until the capture is dropped — a test that panics ends its capture as it
/// unwinds, so none is left under way to keep every later line. Tests running at once share the lines: each reads
/// those since its own start (`Capture::lines`) and picks out its own; with none under way, none are kept.
#[cfg(test)]
pub fn capture() -> Capture {
    let mut c = captured();
    // (with none under way, none are kept — the last to end cleared them: this one starts at the first line)
    let (id, from) = (c.next, c.lines.len());
    c.next += 1;
    c.live.push((id, from));
    Capture { id, from }
}

/// A capture under way (`capture`).
#[cfg(test)]
pub struct Capture {
    id: u64,
    from: usize,
}

#[cfg(test)]
impl Capture {
    /// The lines since it started.
    pub fn lines(&self) -> Vec<String> {
        captured().lines.get(self.from..).unwrap_or_default().to_vec()
    }

    /// Its id — `is_capturing` tells whether it's still under way.
    pub fn id(&self) -> u64 {
        self.id
    }
}

#[cfg(test)]
impl Drop for Capture {
    fn drop(&mut self) {
        let mut c = captured();
        c.live.retain(|(id, _)| *id != self.id);
        if c.live.is_empty() {
            c.lines.clear();
        }
    }
}

/// Tests: whether capture `id` is still under way.
#[cfg(test)]
pub fn is_capturing(id: u64) -> bool {
    captured().live.iter().any(|(i, _)| *i == id)
}

/// Tests: how many lines are kept with no capture under way (none, when the last to end cleared them) — None:
/// one is under way.
#[cfg(test)]
fn kept_while_idle() -> Option<usize> {
    let c = captured();
    c.live.is_empty().then_some(c.lines.len())
}

/// Local (year, month, day, hour, minute, second).
fn now() -> (i32, i32, i32, i32, i32, i32) {
    local(std::time::SystemTime::now())
}

/// `14:02` — when `at` was, local time.
pub fn clock(at: std::time::SystemTime) -> String {
    let (.., h, m, _) = local(at);
    format!("{h:02}:{m:02}")
}

/// `2026-10-15 14:02:09` — when `at` was, local time, to the second.
pub fn stamp(at: std::time::SystemTime) -> String {
    let (y, mo, d, h, m, s) = local(at);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}:{s:02}")
}

/// `Tue 10:00` — the local weekday and time of `at` (a plan window's reset: within a week).
pub fn day_clock(at: std::time::SystemTime) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let tm = local_tm(at);
    format!("{} {:02}:{:02}", DAYS[tm.tm_wday.rem_euclid(7) as usize], tm.tm_hour, tm.tm_min)
}

/// The local day `at` falls on (year, month, day).
pub fn day(at: std::time::SystemTime) -> (i32, i32, i32) {
    let (y, mo, d, ..) = local(at);
    (y, mo, d)
}

fn local(at: std::time::SystemTime) -> (i32, i32, i32, i32, i32, i32) {
    let tm = local_tm(at);
    (tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec)
}

/// Seconds local time is east of UTC at `at`.
pub fn utc_offset(at: std::time::SystemTime) -> i64 {
    local_tm(at).tm_gmtoff
}

fn local_tm(at: std::time::SystemTime) -> libc::tm {
    let secs = at.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let t = secs as libc::time_t;
    // SAFETY: localtime_r only writes the `tm` we own; a zeroed `tm` is a valid value to start from.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        tm
    }
}

/// One line of tarae's own: `12:03:04               rust-analyzer    error    message` (the session's column
/// blank; a line about one session goes through `Session::line`).
pub fn line(source: &str, level: &str, text: &str) {
    said(None, source, level, text);
}

fn said(who: Option<&Arc<str>>, source: &str, level: &str, text: &str) {
    #[cfg(test)]
    {
        let mut c = captured();
        if !c.live.is_empty() {
            c.lines.push(match who {
                Some(w) => format!("@{w}  {source}  {level}  {text}"),
                None => format!("{source}  {level}  {text}"),
            });
        }
    }
    let Some(tx) = sender() else { return };
    let (.., h, m, sec) = now();
    let _ = tx.send(Entry {
        who: who.cloned(),
        text: formatted(who.map(|w| &**w), (h, m, sec), source, level, text),
    });
}

/// The session's column: `@` and its name (spaces as `_`, so a grep for it is one word), this wide — a longer
/// name is cut to it, its end a `…` (`@fix-login-ever…`: the columns after it stay put, and a grep for its
/// start still finds it).
const SESSION_W: usize = 16;

/// A line as the file has it: the time, the session's column (blank for tarae's own), source, level, text —
/// multi-line text indented under the text's column.
fn formatted(
    who: Option<&str>,
    (h, m, sec): (i32, i32, i32),
    source: &str,
    level: &str,
    text: &str,
) -> String {
    let at = who.map(|n| {
        let n = n.replace(char::is_whitespace, "_");
        match n.chars().count() < SESSION_W {
            true => format!("@{n}"),
            false => format!("@{}…", n.chars().take(SESSION_W - 2).collect::<String>()),
        }
    });
    let at = at.unwrap_or_default();
    let under = " ".repeat(10 + SESSION_W + 1 + 17 + 9);
    let text = text.trim_end().replace('\n', &format!("\n{under}"));
    format!("{h:02}:{m:02}:{sec:02}  {at:<SESSION_W$} {source:<16} {level:<8} {text}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reset's day is said by its local weekday: seven days in a row are the seven names, each the one after
    /// the day before's — whatever time zone the tests run in.
    #[test]
    fn a_day_clock_says_the_local_weekday() {
        const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
        let start = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_791_853_200);
        let names: Vec<String> = (0..8u64)
            .map(|d| day_clock(start + std::time::Duration::from_secs(d * 86_400))[..3].to_string())
            .collect();
        let first = DAYS.iter().position(|d| *d == names[0]).expect("a weekday's name");
        for (i, n) in names.iter().enumerate() {
            assert_eq!(n, DAYS[(first + i) % 7], "{names:?}");
        }
        assert_eq!(day_clock(start).len(), "Tue 10:00".len());
    }

    /// A line about one session names it in its own column, whichever way it comes: from the session in hand
    /// (a card answered, a message's size — `session::Session::log`), or on a worker from a path in its
    /// checkout (a run's root, a keeper's, a server's — `Session::of`), the innermost checkout when they nest;
    /// a server several sessions share is none of theirs alone, and an ended session's checkout names it no
    /// more. The checkout tarae started in goes by its branch, as the session list has it. tarae's own lines
    /// leave the column blank, and a long name is cut to the column — the source starts in the same place.
    #[test]
    fn every_session_scoped_line_names_its_session() {
        let tag = format!("log-session-test-{}", std::process::id());
        let base = std::env::temp_dir().join(&tag);
        let (main, nested) =
            (base.join(format!("{tag}-main")), base.join(format!("{tag}-main/wt/{tag}-fix")));
        let other = base.join(format!("{tag}-other"));
        let front = crate::session::Session::new(1, main.clone(), 1);
        let inner = crate::session::Session::new(2, nested.clone(), 2);
        let behind = crate::session::Session::new(3, other.clone(), 3);
        let (m, f, o) = (format!("{tag}-main"), format!("{tag}-fix"), format!("{tag}-other"));
        let captured = capture();
        let mine = || -> Vec<String> { captured.lines().into_iter().filter(|l| l.contains(&tag)).collect() };

        // From the session in hand
        let call = crate::tools::Call::test(1);
        let mut card =
            crate::offer::Offer::new("t".into(), "q", "", String::new(), crate::offer::What::Screen { call });
        card.id = 7;
        crate::cards::said(&front.log(), &card, ("y", Some("you")), None);
        crate::usage::message_sent(&behind.log(), 40, 10);
        let said = mine();
        assert!(said.iter().any(|l| l.starts_with(&format!("@{m}  tarae  card  card#7 · "))), "{said:#?}");
        assert!(
            said.iter().any(|l| l.starts_with(&format!("@{o}  tokens  info  message bytes=40"))),
            "{said:#?}"
        );

        // On a worker, from a path
        assert_eq!(
            Session::of(&main.join("src/a.rs")).name(),
            Some(m.as_str()),
            "a run in the main checkout"
        );
        assert_eq!(
            Session::of(&nested.join("Cargo.toml")).name(),
            Some(f.as_str()),
            "the innermost checkout"
        );
        assert_eq!(Session::of(&other).name(), Some(o.as_str()), "its root itself");
        // (tests running beside it have checkouts too — the temp folder itself, some: one of this test's own
        // around its folder is the innermost for what's above its sessions, whatever else is registered — one
        // such made here, so that's always so, not only when they overlap)
        let _beside = Checkout::new(&std::env::temp_dir(), "a-test-beside");
        let around = format!("{tag}-around");
        let _around = Checkout::new(&base, &around);
        assert_eq!(Session::of(&base.join("x")).name(), Some(around.as_str()), "above them: the one around");
        assert_eq!(Session::of(Path::new("/")).name(), None, "in no checkout: tarae's own");
        assert_eq!(
            crate::lsp::log_of(&[nested.join("crates/x")]).name(),
            Some(f.as_str()),
            "one session's server"
        );
        assert_eq!(
            crate::lsp::log_of(&[main.clone(), other.clone()]),
            Session::none(),
            "a server they share"
        );
        Session::of(&other.join("target")).line("sandbox", "info", &format!("{tag} a keeper for it"));
        assert!(
            mine().iter().any(|l| l.starts_with(&format!("@{o}  sandbox  info  {tag} a keeper"))),
            "{:#?}",
            mine()
        );

        // Ended, forgotten
        drop(behind);
        assert_eq!(Session::of(&other).name(), Some(around.as_str()), "an ended session's checkout: not its");
        drop((front, inner));

        // The checkout tarae started in, as the list calls it: by its branch once known — in hand and from a path
        let started = base.join(format!("{tag}-started"));
        let mut first = crate::session::Session::new(0, started.clone(), 4);
        let branch = format!("{tag}-feature");
        first.git_branch = Some(branch.clone());
        first.in_log.named(first.label());
        assert_eq!(first.log().name(), Some(branch.as_str()), "its branch, not its folder");
        assert_eq!(Session::of(&started.join("src")).name(), Some(branch.as_str()), "from a path too");

        // As the file has it: `@name` after the time, blank for tarae's own, the rest where it always is — a
        // name too long for the column cut to it, its end a `…`
        let at = (12, 3, 4);
        let theirs = formatted(Some("fix login"), at, "rust-analyzer", "ready", "after 3 ms\nand more");
        let long =
            formatted(Some("fix-login-everywhere"), at, "rust-analyzer", "ready", "after 3 ms\nand more");
        let own = formatted(None, at, "rust-analyzer", "ready", "after 3 ms");
        assert_eq!(
            theirs.lines().next().unwrap(),
            "12:03:04  @fix_login       rust-analyzer    ready    after 3 ms"
        );
        assert_eq!(
            long.lines().next().unwrap(),
            "12:03:04  @fix-login-ever… rust-analyzer    ready    after 3 ms"
        );
        assert_eq!(own, "12:03:04                   rust-analyzer    ready    after 3 ms\n");
        let text_at = own.find("after").unwrap();
        for l in [&theirs, &long] {
            let column = l[..l.find("after").unwrap()].chars().count();
            assert_eq!(column, text_at, "the text where it always is: {l}");
            let under = format!("{}and more", " ".repeat(text_at));
            assert_eq!(l.lines().nth(1).unwrap(), under, "indented under it");
        }
    }

    /// Past the cap the busiest's oldest lines go: a quiet session keeps every line however busy another is,
    /// and so do tarae's own; the busy one keeps its newest; the file stays within the cap and says how many
    /// went. The last run's log is still `tarae.log.old`, untouched by the rewrites.
    #[test]
    fn a_busy_session_doesnt_push_out_anothers() {
        let scratch = crate::scratch::Scratch::new("log-busy");
        let dir = scratch.to_path_buf();
        let path = dir.join("tarae.log");
        std::fs::write(&path, "the last run's log\n").unwrap();
        let cap = 8 << 10;
        let (tx, rx) = channel::<Entry>();
        let writer = std::thread::spawn({
            let path = path.clone();
            move || keep(&path, rx, cap)
        });
        let send = |who: Option<&str>, text: String| {
            let line = formatted(who, (1, 2, 3), "sandbox", "info", &text);
            tx.send(Entry { who: who.map(Arc::from), text: line }).unwrap();
        };
        for i in 0..5 {
            send(Some("quiet"), format!("quiet {i}"));
            send(None, format!("own {i}"));
        }
        for i in 0..2000 {
            send(Some("busy"), format!("busy {i}"));
        }
        send(Some("quiet"), "quiet last".into());
        drop(tx);
        writer.join().unwrap();

        let log = std::fs::read_to_string(&path).unwrap();
        let has = |t: &str| log.lines().any(|l| l.ends_with(&format!("info     {t}")));
        for i in 0..5 {
            assert!(
                has(&format!("quiet {i}")) && has(&format!("own {i}")),
                "kept: quiet {i}, own {i}\n{log}"
            );
        }
        assert!(has("quiet last") && has("busy 1999"), "the newest kept\n{log}");
        assert!(!has("busy 0"), "the busy one's oldest went first");
        let busy: Vec<usize> = log
            .lines()
            .filter_map(|l| l.split_once("info     busy ").and_then(|(_, n)| n.parse().ok()))
            .collect();
        assert!(busy.windows(2).all(|w| w[1] == w[0] + 1), "its newest, none skipped: {busy:?}");
        assert!(log.len() <= cap + 200, "within the cap: {} bytes", log.len());
        let head = log.lines().next().unwrap();
        assert!(
            head.ends_with(&format!("· {} older lines of the busiest left out", 2000 - busy.len())),
            "{head}"
        );
        assert_eq!(std::fs::read_to_string(dir.join("tarae.log.old")).unwrap(), "the last run's log\n");
        assert!(!dir.join("tarae.log.new").exists(), "no rewrite left half done");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A trim whose rewrite can't be made (the log's folder unwritable: no `tarae.log.new`) appends the batch
    /// as it came — every line once, in order, those the trim dropped from what's kept too — and the file is
    /// the one it was, not said to be trimmed. (One batch: every line is queued before the writer reads, so
    /// the trim drops lines of the very batch it falls back on.)
    #[test]
    fn a_rewrite_that_cant_be_made_loses_no_line_and_repeats_none() {
        use std::os::unix::fs::PermissionsExt as _;
        let test = "log::tests::a_rewrite_that_cant_be_made_loses_no_line_and_repeats_none";
        let scratch = crate::scratch::Scratch::new("log-unwritable");
        let dir = scratch.to_path_buf();
        let path = dir.join("tarae.log");
        std::fs::write(&path, "").unwrap();
        let mode = |m: u32| std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(m)).unwrap();
        mode(0o555);
        // (root writes in it all the same: nothing to see then)
        if std::fs::write(dir.join("probe"), "").is_ok() {
            mode(0o755);
            let _ = std::fs::remove_dir_all(&dir);
            let why = "its folder can't be made unwritable here (root)";
            crate::test_results::said_skip(&format!("{test} — {why}"));
            return;
        }
        let (tx, rx) = channel::<Entry>();
        let sent: Vec<String> = (0..300).map(|i| format!("busy {i:03}")).collect();
        for t in &sent {
            let text = formatted(Some("busy"), (1, 2, 3), "sandbox", "info", t);
            tx.send(Entry { who: Some("busy".into()), text }).unwrap();
        }
        drop(tx);
        keep(&path, rx, 8 << 10);
        mode(0o755);
        let log = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let written: Vec<&str> =
            log.lines().filter_map(|l| l.split_once("info     ").map(|(_, t)| t)).collect();
        assert_eq!(written, sent, "every line once, in order");
        assert!(!log.contains("left out"), "not said to be trimmed: {}", log.lines().next().unwrap());
    }

    /// The log open in a buffer (`:log-open`) follows a trim — `tarae.log.new` renamed over the file it showed
    /// — and the lines appended after it, to the new file: never the old file's text, which a reader holding
    /// the old inode would go on showing. Each well within the watch's backstop: the round's watch hears the
    /// rename, and the next round watches the new file.
    #[test]
    fn the_open_log_follows_a_trim_and_what_comes_after() {
        use std::time::{Duration, Instant};
        let scratch = crate::scratch::Scratch::new("log-follow-trim");
        let dir = scratch.to_path_buf();
        let path = std::fs::canonicalize(&dir).unwrap().join("tarae.log");
        let (tx, rx) = channel::<Entry>();
        let writer = std::thread::spawn({
            let path = path.clone();
            move || keep(&path, rx, 8 << 10)
        });
        let send = |text: String| {
            let text = formatted(Some("busy"), (1, 2, 3), "sandbox", "info", &text);
            tx.send(Entry { who: Some("busy".into()), text }).unwrap();
        };
        let on_disk = || std::fs::read_to_string(&path).unwrap_or_default();
        // (close to the cap, so one line takes it past: the trim is the only change — the old file hears
        // nothing after the rename but the rename itself)
        send("before the trim".into());
        for i in 0..64 {
            send(format!("fill {i:03} {}", "x".repeat(50)));
        }
        let end = Instant::now() + Duration::from_secs(5);
        while !on_disk().contains("fill 063") && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(on_disk().len() < 8 << 10, "not trimmed yet");
        let mut ed = crate::editor::Editor::new(crate::config::Config::default());
        ed.open(&path).unwrap();
        ed.disk_watch();
        // (the backstop is 5 s: well under it, only the watch can have brought the change)
        let follows = |ed: &mut crate::editor::Editor, what: &str| {
            let end = Instant::now() + Duration::from_secs(3);
            loop {
                let shown = ed.doc().text.to_string();
                if shown.contains(what) && shown == on_disk() {
                    return true;
                }
                let Some(left) = end.checked_duration_since(Instant::now()) else { return false };
                if let Some(ev) = ed.events.recv_timeout(left.min(Duration::from_millis(50))) {
                    ed.handle_event(ev);
                    ed.disk_sync();
                }
            }
        };
        assert!(follows(&mut ed, "before the trim"), "{}", ed.doc().text);
        std::thread::sleep(Duration::from_millis(100)); // (the round arms its watch)
        send(format!("past the cap {}", "x".repeat(2000)));
        assert!(follows(&mut ed, "past the cap"), "the trimmed file shown: {}", ed.doc().text);
        assert!(ed.doc().text.to_string().contains("older lines of the busiest left out"));
        assert!(!ed.doc().text.to_string().contains("before the trim"), "the old file's text gone");
        std::thread::sleep(Duration::from_millis(100));
        send("after the trim".into());
        assert!(follows(&mut ed, "after the trim"), "appended to the new file, shown: {}", ed.doc().text);
        drop(tx);
        writer.join().unwrap();
        drop(ed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Captures under way at once each read their own lines — those since each began — and a test that panics
    /// mid-capture ends its capture as it unwinds: none is left under way keeping every later line, and with none
    /// under way none are kept. (Other tests capture alongside: this one reads only its own tagged lines.)
    #[test]
    fn captures_end_with_their_test_even_one_that_panics() {
        let tag = format!("log-capture-test-{}", std::process::id());
        let mine = |c: &Capture| -> Vec<String> {
            c.lines()
                .into_iter()
                .filter_map(|l| l.split_once(&tag).map(|(_, w)| w.trim().to_string()))
                .collect()
        };
        let a = capture();
        line("test", "info", &format!("{tag} one"));
        let b = capture();
        line("test", "info", &format!("{tag} two"));
        assert_eq!(mine(&a), ["one", "two"], "a: every line since it began");
        assert_eq!(mine(&b), ["two"], "b: only those since it began");
        drop(b);
        line("test", "info", &format!("{tag} three"));
        assert_eq!(mine(&a), ["one", "two", "three"], "b's end isn't a's");
        drop(a);
        let mut id = u64::MAX;
        let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let c = capture();
            id = c.id();
            line("test", "info", &format!("{tag} panicked"));
            panic!("a test failing mid-capture (expected here)");
        }));
        assert!(failed.is_err());
        assert!(!is_capturing(id), "ended as its test unwound");
        // (with none under way, none kept — what this test's captures saw went with the last to end; unless another
        // test captures right now: then its lines are rightly kept, and this can't be seen)
        match kept_while_idle() {
            Some(kept) => assert_eq!(kept, 0, "lines kept with no capture under way: stale for the next"),
            None => eprintln!(
                "captures_end_with_their_test_even_one_that_panics: another test captures — not seen"
            ),
        }
        let next = capture();
        line("test", "info", &format!("{tag} four"));
        assert_eq!(mine(&next), ["four"], "the next starts with its own");
    }
}
