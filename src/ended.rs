//! `session.ended` — a command of the user's, run once a session's checkout is removed (`session_remove`):
//! for what lived outside the checkout on its behalf (tarae's own repository sets `cargo xtask linux prune`,
//! which drops the Docker volume its Linux checks built in). A trusted setting (`trust::RISKY`): it runs
//! outside the sandbox, in the checkout tarae started in, `VAR` naming the one removed — on a worker, so the end
//! waits for nothing; past `LIMIT` its process group is stopped. How it ended goes to the log; a failure is a
//! warning. Not after a removal that failed.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::editor::Editor;

/// How long it may take (a cold `cargo xtask` builds first).
pub const LIMIT: Duration = Duration::from_secs(120);

/// The variable naming the checkout that was removed.
pub const VAR: &str = "TARAE_ENDED_CHECKOUT";

/// Those running: (pid, the command) — for `:procs`.
static RUNNING: Mutex<Vec<(u32, String)>> = Mutex::new(Vec::new());

pub fn running() -> Vec<(u32, String)> {
    RUNNING.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

/// Lines kept of what it prints, per stream (the last ones).
const KEPT: usize = 20;

/// How a run went: Err says why not — how it ended and its last line (stderr's, else stdout's), stopped past
/// its limit, or didn't start — and what it printed, each stream's last `KEPT` lines that aren't blank (the
/// two read apart: their order between them isn't known).
#[derive(Debug, PartialEq)]
pub struct Ran {
    pub result: Result<(), String>,
    pub out: Vec<String>,
    pub err: Vec<String>,
}

/// `cmd` in `cwd`, `VAR` = `ended`, at most `limit` (on a worker: it waits).
pub fn run(cmd: &[String], cwd: &Path, ended: &Path, limit: Duration) -> Ran {
    let failed = |why: String| Ran { result: Err(why), out: Vec::new(), err: Vec::new() };
    let Some((program, args)) = cmd.split_first() else { return failed("no command".into()) };
    let mut c = std::process::Command::new(program);
    c.args(args).current_dir(cwd).env(VAR, ended);
    let s = match crate::spawn::setsid(&c) {
        Ok(s) => s,
        Err(e) => return failed(format!("didn't start: {e}")),
    };
    let pid = s.pid;
    RUNNING.lock().unwrap_or_else(|p| p.into_inner()).push((pid, cmd.join(" ")));
    let said = [Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(Vec::new()))];
    let (eof_tx, eof) = std::sync::mpsc::channel();
    for (pipe, said) in [s.stdout, s.stderr].into_iter().zip(&said) {
        let Some(pipe) = pipe else { continue };
        let (said, eof_tx) = (said.clone(), eof_tx.clone());
        crate::event::thread("tarae-endedout", move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                if !line.trim().is_empty() {
                    let mut s = said.lock().unwrap_or_else(|p| p.into_inner());
                    if s.len() == KEPT {
                        s.remove(0);
                    }
                    s.push(line.trim().to_string());
                }
            }
            let _ = eof_tx.send(());
        });
    }
    drop(eof_tx);
    let (done_tx, done) = std::sync::mpsc::channel();
    crate::event::thread("tarae-ended", move || {
        let _ = done_tx.send(crate::spawn::wait_and_drain(pid, &eof, 2));
    });
    let status = done.recv_timeout(limit).ok();
    if status.is_none() {
        // (its whole group: what it started goes with it; one that ignores the stop is killed)
        crate::dap::term_group(pid);
        if done.recv_timeout(Duration::from_secs(5)).is_err() {
            // SAFETY: only signals the process group it was started in (`spawn::setsid`).
            unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        }
    }
    RUNNING.lock().unwrap_or_else(|p| p.into_inner()).retain(|(p, _)| *p != pid);
    let [out, err] = said.map(|s| std::mem::take(&mut *s.lock().unwrap_or_else(|p| p.into_inner())));
    let last = err.last().or(out.last()).cloned();
    let with = |how: String| match &last {
        Some(l) => format!("{how}: {l}"),
        None => how,
    };
    let result = match status {
        Some(Ok(s)) if s.success() => Ok(()),
        Some(Ok(s)) => Err(with(match s.code() {
            Some(code) => format!("exit {code}"),
            None => "stopped by a signal".into(),
        })),
        Some(Err(e)) => Err(format!("not waited for: {e}")),
        None => Err(with(format!("stopped after {} s", limit.as_secs_f64()))),
    };
    Ran { result, out, err }
}

impl Editor {
    /// `session.ended`, if set, for `root` — session `name`'s checkout just removed (the session itself gone:
    /// its name comes along for its last log line).
    pub fn session_ended(&mut self, root: &Path, name: &str) {
        let cmd = self.config.session_ended.clone();
        if cmd.is_empty() {
            return;
        }
        let (cwd, ended): (PathBuf, PathBuf) = (self.start_root(), root.to_path_buf());
        let log = crate::log::Session::named(name);
        self.events.shared_jobs().spawn(move || {
            let ran = run(&cmd, &cwd, &ended, LIMIT);
            let shown = cmd.join(" ");
            let (level, how) = match &ran.result {
                Ok(()) => ("info", "done".to_string()),
                Err(why) => ("warn", why.clone()),
            };
            let printed: Vec<&str> = ran.out.iter().chain(&ran.err).map(String::as_str).collect();
            let text = format!("{shown} for {}: {how}\n{}", ended.display(), printed.join("\n"));
            log.line("session.ended", level, &text);
            move |ed: &mut Editor| match ran.result {
                // (nothing on screen changed — the log has it)
                Ok(_) => ed.unchanged = true,
                Err(why) => ed.set_warning(format!("session.ended `{shown}` — {why}")),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tarae-ended-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    fn sh(script: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), script.into()]
    }

    fn alive(pid: i32) -> bool {
        // SAFETY: signal 0 only asks whether the process is there.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// What it printed, each stream's own (blank lines left out, the last `KEPT` kept); how it ended on a
    /// failure, with stderr's last line, else stdout's; a program that isn't there didn't start.
    #[test]
    fn a_run_says_what_it_printed_and_how_it_ended() {
        let d = dir("said");
        let lines = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let ok = run(&sh("echo one; echo two >&2; echo; echo three"), &d, &d, LIMIT);
        assert_eq!(ok, Ran { result: Ok(()), out: lines(&["one", "three"]), err: lines(&["two"]) });
        let many = run(&sh("i=0; while [ $i -lt 30 ]; do i=$((i+1)); echo $i; done"), &d, &d, LIMIT);
        assert_eq!((many.out.len(), many.out[0].as_str(), many.out[19].as_str()), (KEPT, "11", "30"));
        let failed = run(&sh("echo out; echo why >&2; exit 2"), &d, &d, LIMIT);
        assert_eq!(failed.result, Err("exit 2: why".into()), "stderr's last line");
        assert_eq!(run(&sh("echo out; exit 2"), &d, &d, LIMIT).result, Err("exit 2: out".into()));
        assert_eq!(run(&sh("exit 4"), &d, &d, LIMIT).result, Err("exit 4".into()));
        let missing = run(&["tarae-no-such-program".into()], &d, &d, LIMIT).result.unwrap_err();
        assert!(missing.starts_with("didn't start: "), "{missing}");
        assert_eq!(run(&[], &d, &d, LIMIT).result, Err("no command".into()));
        std::fs::remove_dir_all(d).ok();
    }

    /// Past its limit it's stopped with what it started (its group), and said so; while it runs `:procs`
    /// knows it, and not after.
    #[test]
    fn a_run_past_its_limit_is_stopped_with_what_it_started() {
        let d = dir("limit");
        let child = d.join("child.pid");
        let script = format!("sleep 30 & echo $! > '{}'; echo waiting; wait", child.display());
        let cmd = sh(&script);
        let ran = {
            let (cmd, d) = (cmd.clone(), d.clone());
            std::thread::spawn(move || run(&cmd, &d, &d, Duration::from_millis(1500)))
        };
        let end = Instant::now() + Duration::from_secs(5);
        while !child.exists() {
            assert!(Instant::now() < end, "it started");
            std::thread::sleep(Duration::from_millis(10));
        }
        let shown = cmd.join(" ");
        assert!(running().iter().any(|(_, c)| *c == shown), "{:?}", running());
        let ed = Editor::new(crate::config::Config::default());
        let known = ed.procs_known();
        assert!(known.iter().any(|k| k.title == "session.ended" && k.what == shown), "in :procs");
        let t = Instant::now();
        assert_eq!(ran.join().unwrap().result, Err("stopped after 1.5 s: waiting".into()));
        assert!(t.elapsed() < Duration::from_secs(5), "not waited for past its stop");
        assert!(!running().iter().any(|(_, c)| *c == shown), "gone from :procs");
        let pid: i32 = std::fs::read_to_string(&child).unwrap().trim().parse().unwrap();
        let end = Instant::now() + Duration::from_secs(5);
        while alive(pid) {
            assert!(Instant::now() < end, "what it started ({pid}) stopped with it");
            std::thread::sleep(Duration::from_millis(20));
        }
        std::fs::remove_dir_all(d).ok();
    }
}
