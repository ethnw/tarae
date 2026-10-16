use super::*;
use crate::config::Config;
use crate::event::Event;
use crate::key::Key;

fn key(k: &str) -> Key {
    k.parse().unwrap()
}

/// Keys as events (`<esc>`, `<ret>` by name) — each one's after-event work (`recovery_sync`) runs too.
fn feed(ed: &mut Editor, keys: &str) {
    let mut chars = keys.chars();
    while let Some(c) = chars.next() {
        let k = match c {
            '<' => key(&chars.by_ref().take_while(|&c| c != '>').collect::<String>()),
            c => key(&c.to_string()),
        };
        ed.handle_event(Event::Key(k));
    }
}

/// A test's own folder: `state` (the copies), `home` (where keys are), `project` (the files).
fn place(name: &str) -> PathBuf {
    let dir = crate::scratch::Scratch::new(&format!("recovery-{name}")).into_path();
    std::fs::create_dir_all(dir.join("project")).unwrap();
    std::fs::create_dir_all(dir.join("home")).unwrap();
    dir.canonicalize().unwrap()
}

/// An editor keeping its copies in `dir/state`, after a pause of 20 ms, with `dir/home` as home.
fn editor(dir: &Path) -> Editor {
    let mut ed = Editor::new(Config::default());
    let mut r = Recovery::new(dir.join("state"), ed.session.root.clone());
    r.quiet = Duration::from_millis(20);
    r.rules.home = Some(dir.join("home"));
    ed.recovery = Some(r);
    ed
}

fn recovery(ed: &mut Editor) -> &mut Recovery {
    ed.recovery.as_mut().unwrap()
}

/// Until `done` — looked at every few ms, 5 s at most (what a worker does on disk, seen from outside).
fn until(what: &str, done: impl Fn() -> bool) {
    let end = Instant::now() + Duration::from_secs(5);
    while !done() {
        assert!(Instant::now() < end, "never: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Every copy in `dir/state`, any tarae's.
fn copies_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir.join("state")) else { return Vec::new() };
    rd.flatten().flat_map(|e| copies(&e.path())).collect()
}

/// `project/a.txt` ("one"), edited in a tarae that then dies with the copy on disk — the file.
fn left_behind(dir: &Path) -> PathBuf {
    let file = dir.join("project/a.txt");
    std::fs::write(&file, "one\n").unwrap();
    let mut ed = editor(dir);
    ed.open(&file).unwrap();
    feed(&mut ed, "cTWO<esc>");
    until("the copy is written", || copies_in(dir).len() == 1);
    recovery(&mut ed).killed();
    file
}

/// Events until `done` (5 s at most).
fn settle_until(ed: &mut Editor, done: impl Fn(&Editor) -> bool) {
    let end = Instant::now() + Duration::from_secs(5);
    while !done(ed) {
        let left = end.saturating_duration_since(Instant::now());
        let ev = ed.events.recv_timeout(left).expect("event before timeout");
        ed.handle_event(ev);
    }
}

/// A new tarae over `dir`'s state, its look at what gone ones left done: its recovery cards.
fn started(dir: &Path) -> Editor {
    let mut ed = editor(dir);
    ed.recovery_scan();
    settle_until(&mut ed, |ed| ed.recovery.as_ref().is_some_and(|r| r.scans > 0));
    ed
}

fn cards(ed: &Editor) -> Vec<&Offer> {
    ed.offers.iter().filter(|o| o.recovers()).collect()
}

/// A buffer edited: its copy is written after the pause — its whole text after a header naming the file
/// and its stamp. The tarae killed, the next start offers it on a card that says what changed against the
/// file and that the file is as it was.
#[test]
fn a_killed_taraes_unsaved_edits_are_offered_at_the_next_start() {
    let dir = place("offered");
    let file = left_behind(&dir);
    let c = read_copy(&copies_in(&dir)[0]).unwrap();
    assert_eq!((c.path.as_path(), c.text.as_str()), (file.as_path(), "TWOne\n"));
    assert!(c.disk.is_some(), "the file's stamp as the buffer knew it");
    let ed = started(&dir);
    let cards = cards(&ed);
    assert_eq!(cards.len(), 1);
    assert!(cards[0].title.ends_with("a.txt"), "{}", cards[0].title);
    assert!(cards[0].question.contains("+1 −1 lines"), "{}", cards[0].question);
    assert!(cards[0].question.contains("as it was then"), "{}", cards[0].question);
    assert_eq!(cards[0].answers(false).iter().map(|a| a.0).collect::<String>(), "ycDn");
    std::fs::remove_dir_all(dir).ok();
}

/// A tarae still running keeps its copies to itself — its lock is held. Once it's gone, they're offered.
#[test]
fn a_running_taraes_copies_are_not_offered() {
    let dir = place("running");
    let file = dir.join("project/a.txt");
    std::fs::write(&file, "one\n").unwrap();
    let mut running = editor(&dir);
    running.open(&file).unwrap();
    feed(&mut running, "cTWO<esc>");
    until("the copy is written", || copies_in(&dir).len() == 1);
    assert!(cards(&started(&dir)).is_empty(), "offered while its tarae runs");
    assert_eq!(copies_in(&dir).len(), 1, "and left alone");
    recovery(&mut running).killed();
    assert_eq!(cards(&started(&dir)).len(), 1);
    std::fs::remove_dir_all(dir).ok();
}

/// `y` puts the copy's text in the buffer — unsaved, `u` goes back to the file's — and the copy and its
/// folder go. The file isn't written.
#[test]
fn restore_puts_the_text_back() {
    let dir = place("restore");
    let file = left_behind(&dir);
    let copy = copies_in(&dir)[0].clone();
    let mut ed = started(&dir);
    ed.offer_accept();
    assert!(cards(&ed).is_empty());
    assert_eq!(ed.doc().path.as_deref(), Some(file.as_path()));
    assert_eq!(ed.doc().text.to_string(), "TWOne\n");
    assert!(ed.doc().is_modified());
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\n", "nothing written");
    until("the copy and its folder go", || !copy.parent().unwrap().exists());
    feed(&mut ed, "u");
    assert_eq!(ed.doc().text.to_string(), "one\n");
    assert!(!ed.doc().is_modified());
    std::fs::remove_dir_all(dir).ok();
}

/// `y` while the file's buffer has edits of the user's own: refused on the card, which stays.
#[test]
fn restore_never_goes_over_edits_of_yours() {
    let dir = place("over");
    let file = left_behind(&dir);
    let mut ed = editor(&dir);
    ed.open(&file).unwrap();
    feed(&mut ed, "iMINE <esc>");
    ed.recovery_scan();
    settle_until(&mut ed, |ed| !cards(ed).is_empty());
    ed.offer_accept();
    assert_eq!(cards(&ed).len(), 1, "the card stays");
    assert_eq!(ed.doc().text.to_string(), "MINE one\n");
    assert!(ed.offer_said_now().is_some_and(|s| s.contains("unsaved edits of yours")));
    std::fs::remove_dir_all(dir).ok();
}

/// `D` removes the copy, opens nothing; `n` leaves it — and the next start asks again.
#[test]
fn discard_removes_the_copy_and_not_now_keeps_it() {
    let dir = place("discard");
    left_behind(&dir);
    let copy = copies_in(&dir)[0].clone();
    let mut ed = started(&dir);
    ed.offer_dismiss();
    assert!(cards(&ed).is_empty() && copy.exists(), "n keeps it");
    quit(ed, &dir, || {});
    let mut ed = started(&dir);
    assert_eq!(cards(&ed).len(), 1, "asked again");
    let docs = ed.docs.len();
    ed.offer_discard();
    assert!(cards(&ed).is_empty());
    assert_eq!(ed.docs.len(), docs, "nothing opened");
    until("the copy goes", || !copy.exists());
    assert!(cards(&started(&dir)).is_empty());
    std::fs::remove_dir_all(dir).ok();
}

/// `c` shows the copy against the file in a buffer of its own; the card stays, and goes with its answer.
#[test]
fn compare_shows_the_copy_against_the_file() {
    let dir = place("compare");
    left_behind(&dir);
    let mut ed = started(&dir);
    crate::recovery::compare(&mut ed);
    assert_eq!(cards(&ed).len(), 1, "the card stays");
    assert!(ed.doc().title.as_deref().is_some_and(|t| t.starts_with(COMPARE)));
    assert_eq!(ed.doc().text.to_string(), "TWOne\n");
    let id = ed.doc().id;
    settle_until(&mut ed, |ed| ed.docs.iter().any(|d| d.id == id && !d.git_hunks.is_empty()));
    ed.offer_dismiss();
    assert!(!ed.docs.iter().any(|d| d.id == id), "closed with the card");
    std::fs::remove_dir_all(dir).ok();
}

/// The file changed on disk after the copy was kept: the card says so, in the warning colour. A copy that's
/// what the file is now is no card — it goes.
#[test]
fn a_file_changed_on_disk_since_says_so() {
    let dir = place("changed");
    let file = left_behind(&dir);
    std::fs::write(&file, "three\n").unwrap();
    let ed = started(&dir);
    let card = cards(&ed)[0];
    assert!(card.question.contains("The file changed since"), "{}", card.question);
    assert!(card.warning.is_some());
    // (a child forked while this tarae holds the gone one's folder for its card — as another test's would — let
    // go only once this tarae has quit)
    let child = HeldBack::fork();
    quit(ed, &dir, move || drop(child));
    std::fs::write(&file, "TWOne\n").unwrap();
    let copy = copies_in(&dir)[0].clone();
    assert!(cards(&started(&dir)).is_empty(), "nothing to recover");
    assert!(!copy.exists());
    std::fs::remove_dir_all(dir).ok();
}

/// This tarae quits, and the next start finds the folders in `dir`'s state as a real next start would: once no
/// process has their locks open. A child forked meanwhile holds them too until its exec (another test's, here;
/// a fork copies every descriptor), so a lock it held for a card, its own keeper's, one let go at `n`, stay held
/// a moment past the quit — the next scan took the gone tarae for a running one and left its copy (a full run's
/// flake, `a_file_changed_on_disk_since_says_so`). `meanwhile` runs while it waits (a held-back child let go).
/// As `Recovery::killed` waits for a death.
fn quit(ed: Editor, dir: &Path, meanwhile: impl FnOnce() + Send + 'static) {
    drop(ed);
    let going = std::thread::spawn(meanwhile);
    let locks: Vec<PathBuf> = std::fs::read_dir(dir.join("state"))
        .map(|rd| rd.flatten().map(|e| e.path().join(LOCK)).filter(|l| l.exists()).collect())
        .unwrap_or_default();
    for lock in &locks {
        until(&format!("{} let go", lock.display()), || File::open(lock).is_ok_and(|f| f.try_lock().is_ok()));
    }
    going.join().unwrap();
}

/// A child forked now — holding a copy of every descriptor open, a lock's too — that waits before its exec until
/// it's let go (`go`, or dropped: a failing test's too): another test's child not yet exec'd, made to wait as long
/// as the test needs. Harmless: `true`. Never longer than `HELD_AT_MOST`: it holds the test binary's output open
/// (the first version, without a bound, held a `cargo test … | grep` open for good once its test failed before
/// letting it go) — then it gives up before its exec, and `go` says so.
struct HeldBack {
    go: Option<std::io::PipeWriter>,
    child: Option<std::thread::JoinHandle<std::io::Result<std::process::ExitStatus>>>,
}

/// How long a held-back child waits to be let go, at most.
const HELD_AT_MOST: Duration = Duration::from_secs(10);

impl HeldBack {
    fn fork() -> HeldBack {
        use std::io::Read as _;
        use std::os::fd::AsRawFd as _;
        use std::os::unix::process::CommandExt as _;
        let (mut forked, said) = std::io::pipe().unwrap();
        let (wait, go) = std::io::pipe().unwrap();
        let (said_fd, wait_fd) = (said.as_raw_fd(), wait.as_raw_fd());
        let at_most = HELD_AT_MOST.as_millis() as libc::c_int;
        let child = std::thread::spawn(move || {
            let mut cmd = std::process::Command::new("true");
            // SAFETY: only async-signal-safe calls (write, poll, read) between the fork and the exec; the error
            // made is an OS code's, no allocation
            unsafe {
                cmd.pre_exec(move || {
                    libc::write(said_fd, b"f".as_ptr().cast(), 1);
                    // (it holds the write end too: a byte lets it go, no end of file ever would)
                    let mut p = libc::pollfd { fd: wait_fd, events: libc::POLLIN, revents: 0 };
                    if libc::poll(&mut p, 1, at_most) == 0 {
                        return Err(std::io::Error::from_raw_os_error(libc::ETIMEDOUT));
                    }
                    let mut b = 0u8;
                    libc::read(wait_fd, (&raw mut b).cast(), 1);
                    Ok(())
                });
            }
            let status = cmd.status();
            drop((said, wait));
            status
        });
        forked.read_exact(&mut [0]).unwrap();
        HeldBack { go: Some(go), child: Some(child) }
    }

    /// Its exec goes ahead; it's waited for (within `HELD_AT_MOST` of its fork, by its own bound). Err: what
    /// went wrong, in words. None: let go already.
    fn go(&mut self) -> Option<Result<std::process::ExitStatus, String>> {
        let mut go = self.go.take()?;
        let child = self.child.take()?;
        let wrote = std::io::Write::write_all(&mut go, b"g").map_err(|e| format!("couldn't let it go: {e}"));
        let ended = match child.join() {
            Err(_) => Err("its thread panicked".to_string()),
            Ok(Err(e)) if e.raw_os_error() == Some(libc::ETIMEDOUT) => {
                Err(format!("never let go: it gave up after {HELD_AT_MOST:?} before its exec"))
            }
            Ok(Err(e)) => Err(format!("it didn't start: {e}")),
            Ok(Ok(s)) => Ok(s),
        };
        Some(wrote.and(ended))
    }
}

impl Drop for HeldBack {
    fn drop(&mut self) {
        let ended = self.go();
        if !std::thread::panicking()
            && let Some(ended) = ended
        {
            assert!(ended.as_ref().is_ok_and(|s| s.success()), "the held-back child: {ended:?}");
        }
    }
}

/// Saving removes the copy; so does undoing back to the file, and closing the buffer without saving.
#[test]
fn saving_or_dropping_the_edits_removes_the_copy() {
    let dir = place("saved");
    let file = dir.join("project/a.txt");
    std::fs::write(&file, "one\n").unwrap();
    let mut ed = editor(&dir);
    ed.open(&file).unwrap();
    let written = || copies_in(&dir).len() == 1;
    let gone = || copies_in(&dir).is_empty();
    for (edit, undo) in [("cTWO<esc>", ":w<ret>"), ("iX<esc>", "u"), ("iY<esc>", ":bc!<ret>")] {
        feed(&mut ed, edit);
        until(&format!("{edit}'s copy is written"), written);
        feed(&mut ed, undo);
        assert!(ed.flush_saves());
        // (the save's end is an event — it lands on the next one)
        feed(&mut ed, "<esc>");
        until(&format!("{undo} removes the copy"), gone);
    }
    std::fs::remove_dir_all(dir).ok();
}

/// A quit removes this tarae's folder; a signal's quit (the terminal went) keeps it, with what was typed
/// since the last copy.
#[test]
fn a_quit_removes_the_copies_and_a_signal_keeps_them() {
    let dir = place("quit");
    let file = dir.join("project/a.txt");
    std::fs::write(&file, "one\n").unwrap();
    let mut ed = editor(&dir);
    recovery(&mut ed).quiet = Duration::from_secs(60);
    ed.open(&file).unwrap();
    feed(&mut ed, "cTWO<esc>");
    recovery(&mut ed).keep = true;
    ed.recovery_end(false);
    let copies = copies_in(&dir);
    assert_eq!(copies.len(), 1, "written at the end, not after its pause");
    assert_eq!(read_copy(&copies[0]).unwrap().text, "TWOne\n");
    let folder = recovery(&mut ed).folder().to_path_buf();
    drop(ed);
    let mut ed = editor(&dir);
    ed.open(&file).unwrap();
    feed(&mut ed, "cTHREE<esc>");
    recovery(&mut ed).flush();
    assert_eq!(copies_in(&dir).len(), 2);
    ed.recovery_end(false);
    assert_eq!(copies_in(&dir).len(), 1, "the quit's own went");
    assert!(folder.exists(), "the gone tarae's stays");
    std::fs::remove_dir_all(dir).ok();
}

/// Nothing is written for a key's file: in a place that holds keys (by the real path — a link from the
/// project into `~/.ssh` too), named like one, in `editor.recovery-skip`; nor a file over the limit. The
/// plain file beside them is kept.
#[test]
fn nothing_is_written_for_a_keys_file() {
    let dir = place("keys");
    let (home, project) = (dir.join("home"), dir.join("project"));
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    std::fs::create_dir_all(home.join("notes")).unwrap();
    let files = [
        home.join(".ssh/config"),
        project.join(".env"),
        project.join("id_ed25519"),
        project.join("server.PEM"),
        project.join("x.secret"),
        home.join("notes/n.txt"),
        project.join("big.txt"),
        project.join("plain.txt"),
    ];
    for f in &files {
        std::fs::write(f, "one\n").unwrap();
    }
    std::fs::write(home.join(".ssh/id_work"), "one\n").unwrap();
    std::os::unix::fs::symlink(home.join(".ssh/id_work"), project.join("work")).unwrap();
    let mut ed = editor(&dir);
    ed.config.recovery_skip = vec!["*.secret".into(), "~/notes/".into()];
    for f in files.iter().chain([&project.join("work")]) {
        ed.open(f).unwrap();
        if f.ends_with("big.txt") {
            ed.doc_mut().text = Rope::from("x".repeat(MAX_BYTES));
        }
        feed(&mut ed, "iX<esc>");
    }
    assert!(recovery(&mut ed).flush());
    let kept: Vec<PathBuf> = copies_in(&dir).iter().filter_map(|c| Some(read_copy(c)?.path)).collect();
    assert_eq!(kept, [project.join("plain.txt")]);
    std::fs::remove_dir_all(dir).ok();
}

/// The rules on their own: names in any case with one `*`, paths from a root, from `/` and from `~/`.
#[test]
fn skip_entries_match_names_and_paths() {
    let rules = Rules {
        home: Some(PathBuf::from("/h")),
        skip: vec!["*.Secret".into(), "private/".into(), "/abs/x".into(), "~/n/".into(), " ".into()],
        roots: vec![PathBuf::from("/p")],
    };
    let s = |p: &str| skipped(Path::new(p), &rules);
    assert!(s("/p/a/B.SECRET") && s("/q/x.secret"));
    assert!(s("/p/private/a.rs") && !s("/q/private/a.rs"), "a path is from a root");
    assert!(s("/abs/x/y") && s("/h/n/a") && !s("/h/m/a"));
    assert!(!s("/p/a.rs"), "a blank entry matches nothing");
    let wild = crate::secretnames::wild;
    assert!(wild(".env.*", ".env.local") && !wild(".env.*", ".envrc") && wild("*.pem", "a.pem"));
}

/// A slow disk (each copy takes a second) never holds the keys: typing goes on at full speed while a copy is
/// being written, and nothing the main loop does waits for it.
#[test]
fn the_main_thread_never_waits_on_a_slow_disk() {
    let dir = place("slow");
    let file = dir.join("project/a.txt");
    std::fs::write(&file, "one\n").unwrap();
    let mut ed = editor(&dir);
    let slow = Duration::from_secs(1);
    (recovery(&mut ed).slow, recovery(&mut ed).quiet) = (slow, Duration::ZERO);
    ed.open(&file).unwrap();
    let start = Instant::now();
    let mut worst = Duration::ZERO;
    for k in "iabcdefghijklmnopqrstuvwxyz".chars().map(String::from).chain(["esc".into()]) {
        let t = Instant::now();
        ed.handle_event(Event::Key(key(&k)));
        worst = worst.max(t.elapsed());
    }
    let typed = start.elapsed();
    eprintln!("27 keys while copies take {slow:?}: {typed:?}, worst key {worst:?}");
    assert!(typed < slow / 2, "the keys waited for the copy: {typed:?}");
    assert!(copies_in(&dir).is_empty(), "the first copy is still being written");
    until("the copy lands, later", || copies_in(&dir).len() == 1);
    std::fs::remove_dir_all(dir).ok();
}

/// A child forked while a tarae's lock was open holds it too until it execs (a fork copies every
/// descriptor; close-on-exec closes them only at the exec). The tarae killed meanwhile, its copies are offered
/// once that child lets go — `killed` waits for it, as a real death's next start would find it. (The flake
/// this pins: on a loaded Linux, another test's child not yet exec'd hid a gone tarae's folder from the scan
/// — no card, or the older copy left behind.) A harmless child: `true`, held back 300 ms before its exec.
#[test]
fn a_child_forked_while_the_lock_was_open_doesnt_hide_the_copies() {
    use std::io::Read as _;
    use std::os::fd::AsRawFd as _;
    use std::os::unix::process::CommandExt as _;
    let dir = place("forked");
    let file = dir.join("project/a.txt");
    std::fs::write(&file, "one\n").unwrap();
    let mut ed = editor(&dir);
    ed.open(&file).unwrap();
    feed(&mut ed, "cTWO<esc>");
    until("the copy is written", || copies_in(&dir).len() == 1);
    // The child says it's forked — holding every descriptor, the lock's too — then waits before its exec
    let (mut forked, said) = std::io::pipe().unwrap();
    let fd = said.as_raw_fd();
    let child = std::thread::spawn(move || {
        let mut cmd = std::process::Command::new("true");
        // SAFETY: only async-signal-safe calls (write, nanosleep) between the fork and the exec
        unsafe {
            cmd.pre_exec(move || {
                libc::write(fd, b"f".as_ptr().cast(), 1);
                let t = libc::timespec { tv_sec: 0, tv_nsec: 300_000_000 };
                libc::nanosleep(&t, std::ptr::null_mut());
                Ok(())
            });
        }
        let status = cmd.status();
        drop(said);
        status
    });
    forked.read_exact(&mut [0]).unwrap();
    recovery(&mut ed).killed();
    let ed = started(&dir);
    assert_eq!(cards(&ed).len(), 1, "the copy hidden by the child's hold on the lock");
    assert!(child.join().unwrap().unwrap().success());
    std::fs::remove_dir_all(dir).ok();
}

/// Two gone taraes' copies of one file: the newer is offered, the older goes.
#[test]
fn the_newest_copy_of_a_file_is_the_one_offered() {
    let dir = place("newest");
    let file = left_behind(&dir);
    let mut ed = editor(&dir);
    ed.open(&file).unwrap();
    feed(&mut ed, "cNEWER<esc>");
    until("the second copy is written", || copies_in(&dir).len() == 2);
    recovery(&mut ed).killed();
    let mut ed = started(&dir);
    assert_eq!(cards(&ed).len(), 1);
    assert_eq!(copies_in(&dir).len(), 1, "the older went");
    ed.offer_accept();
    assert_eq!(ed.doc().text.to_string(), "NEWERne\n");
    std::fs::remove_dir_all(dir).ok();
}
