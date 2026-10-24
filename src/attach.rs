//! Attach a debugger to a running program (remote debugging) — `space G a` · `:attach [name | host:port]`.
//!
//! How each language attaches is its pack's `[debug]` (`debugpack.rs`): `attach-tcp` — the program's own
//! debug server speaks DAP, so connect straight to host:port (debugpy's, a headless dlv's); `attach-remote` —
//! the debugger started here attaches to a remote stub at host:port (lldb-dap's `gdb-remote`); `attach-pid` —
//! to a local process; or a way of its own (Java: java-debug inside jdtls attaches over JDWP, java.rs).
//!
//! Config (`~/.config/tarae/config.toml`·`.tarae.toml`) — name targets and pick one in `space G a`:
//! ```toml
//! [[attach]]
//! name = "api (k8s)"
//! lang = "java"                  # defaults to the current file's language
//! port = 5005                    # host defaults to 127.0.0.1
//! before = "kubectl -n app port-forward deploy/api 5005:5005"   # run first, stopped with the debug run
//! remote-root = "/app"           # source root on the remote (container) ↔ this project's root
//! ```

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::dap::{kill_group, setsid};
use crate::debugpack::Attach;
use crate::editor::Editor;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AttachTarget {
    pub name: String,
    /// Defaults to the current file's language.
    pub lang: Option<String>,
    pub host: String,
    pub port: Option<u16>,
    /// Local process id (instead of host·port).
    pub pid: Option<u32>,
    /// Command to launch first (`kubectl port-forward …`·`ssh -L …`) — attach once it prints a line
    /// or after a moment.
    pub before: Option<String>,
    /// Source root on the remote side (`/app` in a container etc.) — maps breakpoint·call stack paths
    /// to this project's root.
    pub remote_root: Option<String>,
    /// The executable to read symbols from, from the project's root (`{program}` in the pack's attach).
    pub program: Option<String>,
}

impl AttachTarget {
    /// Debug pane header — shown once if the name is the address.
    pub fn label(&self) -> String {
        let addr = self.address();
        if self.name == addr { addr } else { format!("{} · {addr}", self.name) }
    }

    pub fn address(&self) -> String {
        match (self.pid, self.port) {
            (Some(pid), _) => format!("pid {pid}"),
            (None, Some(p)) => format!("{}:{p}", self.host),
            (None, None) => "?".into(),
        }
    }
}

/// `[[attach]]` tables (from every config layer — for the same name, the later layer wins).
pub fn parse_config(v: &toml::Value, file: &str, warnings: &mut Vec<String>) -> Vec<AttachTarget> {
    let Some(list) = v.as_array() else {
        warnings.push(format!("{file}: attach must be [[attach]] tables"));
        return Vec::new();
    };
    let s = |t: &toml::Value, k: &str| t.get(k).and_then(|x| x.as_str()).map(str::to_string);
    list.iter()
        .enumerate()
        .filter_map(|(i, t)| {
            let port = t.get("port").and_then(|p| p.as_integer()).and_then(|p| u16::try_from(p).ok());
            let pid = t.get("pid").and_then(|p| p.as_integer()).and_then(|p| u32::try_from(p).ok());
            if port.is_none() && pid.is_none() {
                warnings.push(format!("{file}: attach #{}: needs port = … (or pid = …)", i + 1));
                return None;
            }
            Some(AttachTarget {
                name: s(t, "name").unwrap_or_else(|| format!("attach #{}", i + 1)),
                lang: s(t, "lang"),
                host: s(t, "host").unwrap_or_else(|| "127.0.0.1".into()),
                port,
                pid,
                before: s(t, "before"),
                remote_root: s(t, "remote-root"),
                program: s(t, "program"),
            })
        })
        .collect()
}

/// `host:port` · `:port` · `port` → (host, port).
pub fn parse_addr(s: &str) -> Option<(String, u16)> {
    let s = s.trim();
    let (host, port) = match s.rsplit_once(':') {
        Some((h, p)) => (if h.is_empty() { "127.0.0.1" } else { h }, p),
        None => ("127.0.0.1", s),
    };
    Some((host.trim_matches(['[', ']']).to_string(), port.parse().ok()?))
}

/// Launches the `before` command (`setsid` — stopped with `kill_group`) and waits until it's ready
/// (prints its first line, or 2 s) — if it dies first, its reason (on a worker thread).
fn start_helper(cmd: &str, cwd: &Path) -> Result<Child, String> {
    let mut c = Command::new("sh");
    c.args(["-c", cmd]).current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = setsid(&mut c).spawn().map_err(|e| format!("before: {e}"))?;
    let (tx, rx) = std::sync::mpsc::channel();
    if let Some(out) = child.stdout.take() {
        let tx = tx.clone();
        crate::event::thread("tarae-before", move || {
            let mut lines = BufReader::new(out).lines();
            let _ = tx.send(lines.next().and_then(Result::ok));
            for _ in lines {} // keep draining (so a full pipe doesn't stall it)
        });
    }
    let err = child.stderr.take();
    let err_text = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    if let Some(e) = err {
        let t = err_text.clone();
        crate::event::thread("tarae-before-er", move || {
            for l in BufReader::new(e).lines().map_while(Result::ok) {
                let mut t = t.lock().unwrap();
                if t.len() < 400 {
                    t.push_str(l.trim());
                    t.push(' ');
                }
            }
        });
    }
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(2) {
        // Printed a line → ready (unless it simply closed — check whether it exited right away)
        if let Ok(Some(_)) = rx.try_recv() {
            break;
        }
        if let Ok(Some(status)) = child.try_wait() {
            std::thread::sleep(Duration::from_millis(50));
            let why = err_text.lock().unwrap().trim().to_string();
            return Err(format!(
                "`{cmd}` exited ({status}){}",
                if why.is_empty() { String::new() } else { format!(": {why}") }
            ));
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    Ok(child)
}

/// Attaches over TCP — the server may still be starting, so keep trying for a few seconds
/// (on a worker thread).
fn connect(host: &str, port: u16) -> Result<std::net::TcpStream, String> {
    use std::net::ToSocketAddrs;
    let addrs: Vec<_> = (host, port).to_socket_addrs().map_err(|e| format!("{host}: {e}"))?.collect();
    let started = Instant::now();
    loop {
        let mut last = None;
        for a in &addrs {
            match std::net::TcpStream::connect_timeout(a, Duration::from_secs(2)) {
                Ok(s) => return Ok(s),
                Err(e) => last = Some(e),
            }
        }
        if started.elapsed() > Duration::from_secs(5) {
            return Err(format!(
                "can't reach {host}:{port} — {}",
                last.map(|e| e.to_string()).unwrap_or_default()
            ));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

impl Editor {
    /// `space G a` — pick a target to attach to (asks for an address if none are configured;
    /// targets are layered up to this project's `.tarae.toml`).
    pub fn attach_picker(&mut self) {
        let targets = &self.config.attach;
        if targets.is_empty() {
            self.open_prompt(crate::editor::PromptKind::Command, "attach ");
            return self.set_status("attach to host:port (or add [[attach]] to .tarae.toml)");
        }
        let items = targets
            .iter()
            .map(|t| crate::picker::Item {
                label: t.name.clone(),
                action: crate::picker::Action::Typed(format!("attach {}", t.name)),
                hint: [t.lang.clone().unwrap_or_default(), t.address()]
                    .iter()
                    .filter(|s| !s.is_empty())
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" · "),
                glyph: None,
            })
            .collect();
        let mut p = crate::picker::Picker::of("attach", "attach", items, false).without_preview();
        p.compact = true;
        self.open_picker(p, None);
    }

    /// `:attach [name | host:port]`.
    pub fn attach_command(&mut self, arg: &str) -> Result<(), String> {
        let arg = arg.trim();
        if arg.is_empty() {
            self.attach_picker();
            return Ok(());
        }
        let target = match self.config.attach.iter().find(|t| t.name == arg).cloned() {
            Some(t) => t,
            None => {
                let (host, port) =
                    parse_addr(arg).ok_or_else(|| format!("no attach target '{arg}' (name or host:port)"))?;
                AttachTarget { name: format!("{host}:{port}"), host, port: Some(port), ..Default::default() }
            }
        };
        self.attach(target);
        Ok(())
    }

    /// Attaches — helper·TCP on a worker thread (editing isn't blocked even if the remote is slow).
    pub fn attach(&mut self, t: AttachTarget) {
        use crate::dap::{Dap, State};
        if matches!(
            self.dap.as_ref().map(|d| &d.state),
            Some(State::Running | State::Stopped { .. } | State::Starting)
        ) {
            return self.set_status("already debugging — space G t first");
        }
        let doc = self.doc();
        let file = doc.path.clone();
        // Language = config's lang, else the current file's — if its grammar is missing, offer a download
        // and attach once it's downloaded
        let lang = match (t.lang.clone(), &doc.syntax, file.as_deref().and_then(crate::syntax::detect)) {
            (Some(l), ..) => l,
            (None, Some(s), _) => s.lang.name.clone(),
            (None, None, Some(spec)) if crate::syntax::Loader::global().load(spec).is_err() => {
                return self.offer_grammar_then(spec, crate::offer::Resume::Attach(t));
            }
            (None, None, Some(spec)) => spec.name.clone(),
            (None, None, None) => String::new(),
        };
        let root = file
            .as_deref()
            .map(|f| crate::lsp::find_root_for(&lang, f))
            .unwrap_or_else(|| self.session.root.clone());
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        self.dap_test = None;
        self.dap_resume = Some(crate::offer::Resume::Attach(t.clone()));
        self.dap_generation += 1;
        let generation = self.dap_generation;
        self.dap = Some(Dap::placeholder(State::Starting, t.label()));
        let (before, cwd) = (t.before.clone(), root.clone());
        let t2 = t.clone();
        self.events.jobs().spawn(move || {
            // How this language attaches (its pack), then the helper, then straight to host:port if that's it
            let how = crate::debugpack::of(&lang).and_then(|d| {
                d.attach(&cwd, t2.pid, &t2.host, t2.port, t2.program.as_deref(), t2.remote_root.as_deref())
            });
            let helper = match &how {
                Ok(_) => before.as_deref().map(|b| start_helper(b, &cwd)).transpose(),
                Err(e) => Err(e.clone()),
            };
            let stream = match (&helper, &how, t2.port) {
                (Ok(_), Ok(Attach::Tcp(..)), Some(p)) => Some(connect(&t2.host, p)),
                _ => None,
            };
            move |ed: &mut Editor| {
                if ed.dap_generation != generation {
                    if let Ok(Some(mut h)) = helper {
                        kill_group(&mut h);
                    }
                    return;
                }
                let (helper, how) = match (helper, how) {
                    (Ok(h), Ok(how)) => (h, how),
                    (Err(e), _) | (_, Err(e)) => {
                        ed.dap = None;
                        return ed.set_error(format!("attach: {e}"));
                    }
                };
                ed.attach_continue(t, lang, root, file, how, stream, helper, generation);
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn attach_continue(
        &mut self,
        t: AttachTarget,
        lang: String,
        root: PathBuf,
        file: Option<PathBuf>,
        how: Attach,
        stream: Option<Result<std::net::TcpStream, String>>,
        helper: Option<Child>,
        generation: u64,
    ) {
        let program = t.label();
        let fail = |ed: &mut Editor, helper: Option<Child>, e: String| {
            if let Some(mut h) = helper {
                kill_group(&mut h);
            }
            ed.dap = None;
            ed.set_error(format!("attach: {e}"));
        };
        match (how, stream) {
            // Directly over TCP — the program's debug server speaks DAP
            (Attach::Tcp(args, debugger), Some(stream)) => {
                let s = match stream {
                    Ok(s) => s,
                    Err(e) => return fail(self, helper, e),
                };
                let reader = Box::new(s.try_clone().expect("tcp clone"));
                self.dap_connect(&debugger.id, reader, Box::new(s), None, args, program, generation);
                if let Some(d) = &mut self.dap {
                    d.quiet = debugger.quiet;
                }
            }
            (Attach::Tcp(..), None) => return fail(self, helper, "needs host:port".into()),
            // The one way in code: Java's, through jdtls (java.rs — a plugin later)
            (Attach::Way, _) => {
                let Some(port) = t.port else {
                    return fail(self, helper, "Java attaches to host:port (JDWP)".into());
                };
                let file = file.unwrap_or_else(|| root.join("x.java"));
                // (a leftover prep from an earlier debug run must not look like this one's)
                self.java_debug = None;
                self.java_attach(file, root, t.host.clone(), port, generation);
                if self.java_debug.is_none() {
                    // Couldn't attach (no jdtls etc. — reason already reported) or a download offer (which
                    // starts over once downloaded) — don't stay "starting" forever
                    self.dap = None;
                    if let Some(mut h) = helper {
                        kill_group(&mut h);
                    }
                    return;
                }
                // On java.rs's placeholder — dap_connect takes it over once java-debug gives a port
                if let Some(d) = &mut self.dap {
                    d.helper = helper;
                }
                return;
            }
            // The debugger started here to attach — by pid, or to a remote stub
            (Attach::Started(args), _) => self.dap_start(&lang, root, args, program, generation),
        }
        // On the debug run (or dap_start's placeholder, which the debug run takes over)
        match &mut self.dap {
            Some(d) => {
                d.request = "attach";
                d.helper = helper;
            }
            None => {
                if let Some(mut h) = helper {
                    kill_group(&mut h);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_attach_targets_and_addresses() {
        let v: toml::Table = toml::from_str(
            r#"
            [[attach]]
            name = "api (k8s)"
            lang = "java"
            port = 5005
            before = "kubectl port-forward deploy/api 5005:5005"
            remote-root = "/app"

            [[attach]]
            pid = 4242

            [[attach]]
            name = "broken"
            "#,
        )
        .unwrap();
        let mut w = Vec::new();
        let list = parse_config(&v["attach"], "x.toml", &mut w);
        assert_eq!(list.len(), 2);
        assert_eq!(
            (list[0].name.as_str(), list[0].lang.as_deref(), list[0].address().as_str()),
            ("api (k8s)", Some("java"), "127.0.0.1:5005")
        );
        assert_eq!(list[0].remote_root.as_deref(), Some("/app"));
        assert_eq!((list[1].name.as_str(), list[1].address().as_str()), ("attach #2", "pid 4242"));
        assert!(w[0].contains("attach #3: needs port"), "{w:?}");
        assert_eq!(parse_addr("10.0.0.5:2345"), Some(("10.0.0.5".into(), 2345)));
        assert_eq!(parse_addr(":5678"), Some(("127.0.0.1".into(), 5678)));
        assert_eq!(parse_addr("5005"), Some(("127.0.0.1".into(), 5005)));
        assert_eq!(parse_addr("[::1]:9"), Some(("::1".into(), 9)));
        assert_eq!(parse_addr("host"), None);
    }

    #[test]
    fn helper_that_dies_reports_why() {
        let e = start_helper("echo nope >&2; exit 3", Path::new("/")).unwrap_err();
        assert!(e.contains("nope"), "{e}");
        let mut ok = start_helper("echo ready; sleep 30", Path::new("/")).unwrap();
        kill_group(&mut ok);
    }

    #[test]
    fn java_attach_that_cannot_start_does_not_stay_starting() {
        let mut ed = Editor::new(crate::config::Config::default());
        let t = AttachTarget {
            name: "jvm".into(),
            lang: Some("java".into()),
            host: "127.0.0.1".into(),
            port: Some(5005),
            ..Default::default()
        };
        ed.attach(t);
        assert!(ed.dap.is_some(), "starting");
        let ev = ed.events.recv_timeout(crate::test_results::PATIENCE).expect("attach job");
        ed.handle_event(ev);
        // No jdtls here — the reason is shown and F5·attach work again
        assert!(ed.dap.is_none());
        assert!(ed.status.as_ref().is_some_and(|s| s.0.contains("jdtls")), "{:?}", ed.status);
    }

    #[test]
    fn connects_to_a_listening_server() {
        // Fake remote server — hangs up right after connecting
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || drop(l.accept()));
        assert!(connect("127.0.0.1", port).is_ok());
    }
}
