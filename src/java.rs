//! Java debugging — java-debug runs inside jdtls, asked through a chain of jdtls commands (what data can't
//! say: the plugin batch takes it, `target/sb/core-audit.md` B6). What jdtls itself needs — its `-data`
//! folder, its options, `language/status`, `jdt://` classes, `java.apply.workspaceEdit` — is the java pack's
//! `[server.jdtls]`; java-debug's jar is its `[install.java-debug]` (loaded as jdtls starts: `init.bundles`).

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// java-debug's jar, where the java pack's `[install.java-debug]` finds it (downloaded, or another editor's).
/// Once found it's remembered (no folder scans per F5); while missing, every call looks again (it may get
/// installed).
pub fn debug_bundle() -> Option<PathBuf> {
    static FOUND: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    if let Some(p) = FOUND.get() {
        return Some(p.clone());
    }
    let found = crate::packs::installed_at("java-debug")?;
    Some(FOUND.get_or_init(|| found).clone())
}

// ── Debugging ────────────────────────────────────────────────────────────
//
// java-debug runs inside jdtls, so we ask jdtls in turn — main class (`vscode.java.resolveMainClass`) →
// classpath (`resolveClasspath`) → java binary (`resolveJavaExecutable`) → `startDebugSession` (= port).
// Attach to that port over TCP and the rest is the same DAP as other languages (dap.rs).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// Load java-debug into jdtls when it appeared after jdtls started — `java.reloadBundles`, no restart.
    Reload,
    MainClass,
    Classpath,
    JavaExec,
    Start,
}

#[derive(Clone, Debug)]
pub struct DebugPrep {
    generation: u64,
    pub(crate) cid: crate::lsp::ClientId,
    step: Step,
    /// The request for this step — other replies (an abandoned earlier run) are ignored.
    request: Option<u64>,
    file: PathBuf,
    root: PathBuf,
    main: String,
    project: String,
    module_paths: Value,
    class_paths: Value,
    java_exec: Value,
    /// Already loaded once (give up if "no command" again).
    reloaded: bool,
    /// Attach to a running JVM (JDWP host:port) — straight to the debug run, no main class/classpath lookup.
    attach: Option<(String, u16)>,
}

/// Pick from resolveMainClass: the current file's main, else the only one, else the first (+ other names).
fn pick_main(list: &[Value], file: &Path) -> Option<(String, String, Vec<String>)> {
    let same = |v: &Value| {
        v["filePath"].as_str().is_some_and(|f| {
            let f = Path::new(f);
            f == file || std::fs::canonicalize(f).is_ok_and(|c| c == file)
        })
    };
    let chosen = list.iter().find(|v| same(v)).or(list.first())?;
    let others = list
        .iter()
        .filter(|v| !std::ptr::eq(*v, chosen))
        .filter_map(|v| v["mainClass"].as_str().map(str::to_string))
        .collect();
    Some((
        chosen["mainClass"].as_str()?.to_string(),
        chosen["projectName"].as_str().unwrap_or_default().to_string(),
        others,
    ))
}

impl crate::editor::Editor {
    /// `F5` on a Java file — needs jdtls attached; if java-debug is missing, offers it (the java pack's
    /// `[install.java-debug]`), and debugging starts again once it's in.
    pub fn java_debug_start(&mut self, file: PathBuf, root: PathBuf, generation: u64) {
        self.java_debug_or_offer(file, root, generation, None);
    }

    /// Attach to a running JVM (`-agentlib:jdwp=transport=dt_socket,server=y,address=*:5005`) — attach.rs.
    pub fn java_attach(&mut self, file: PathBuf, root: PathBuf, host: String, port: u16, generation: u64) {
        self.java_debug_or_offer(file, root, generation, Some((host, port)));
    }

    fn java_debug_or_offer(
        &mut self,
        file: PathBuf,
        root: PathBuf,
        generation: u64,
        attach: Option<(String, u16)>,
    ) {
        if self.doc().lsp.client.is_none() {
            return self.set_error("debug: Java needs jdtls (the Java language server) on PATH");
        }
        if debug_bundle().is_none() {
            // (installed, what stopped starts again — `dap_resume` — and jdtls, started without it, is told
            // to load it: the first command finds no handler, `Step::Reload`)
            if !self.offer_install_debugger("java-debug") {
                self.set_error("debug: Java needs the java-debug plugin");
            }
            return;
        }
        self.java_debug_begin(file, root, generation, attach);
    }

    /// Start the prep steps (java-debug loaded into jdtls first when it came after jdtls started: when the
    /// first command finds no handler).
    fn java_debug_begin(
        &mut self,
        file: PathBuf,
        root: PathBuf,
        generation: u64,
        attach: Option<(String, u16)>,
    ) {
        use crate::dap::{Dap, State};
        // That file's jdtls (the user may have switched buffers during the download)
        let of_file =
            self.docs.iter().find(|d| d.path.as_deref() == Some(file.as_path())).and_then(|d| d.lsp.client);
        let Some(cid) = of_file.or(self.doc().lsp.client) else {
            return self.set_error("debug: Java needs jdtls (the Java language server) on PATH");
        };
        let file = std::fs::canonicalize(&file).unwrap_or(file);
        self.java_debug = Some(DebugPrep {
            generation,
            cid,
            step: if attach.is_some() { Step::Start } else { Step::MainClass },
            request: None,
            file,
            root,
            main: String::new(),
            project: String::new(),
            module_paths: Value::Null,
            class_paths: Value::Null,
            java_exec: Value::Null,
            reloaded: false,
            attach: attach.clone(),
        });
        let what = match &attach {
            Some((h, p)) => format!("{h}:{p}"),
            None => "resolving main class".into(),
        };
        self.dap = Some(Dap::placeholder(State::Starting, what));
        self.java_debug_send();
    }

    fn java_debug_send(&mut self) {
        let Some(p) = self.java_debug.clone() else { return };
        let (command, args) = match p.step {
            Step::Reload => ("java.reloadBundles", json!([debug_bundle().into_iter().collect::<Vec<_>>()])),
            Step::MainClass => ("vscode.java.resolveMainClass", json!([crate::lsp::uri(&p.root)])),
            Step::Classpath => ("vscode.java.resolveClasspath", json!([p.main, p.project])),
            Step::JavaExec => ("vscode.java.resolveJavaExecutable", json!([p.main, p.project])),
            Step::Start => ("vscode.java.startDebugSession", json!([])),
        };
        let params = json!({ "command": command, "arguments": args });
        match self.lsp_send(p.cid, crate::lsp_editor::Kind::JavaDebug, "workspace/executeCommand", params) {
            Some(id) => {
                if let Some(p) = &mut self.java_debug {
                    p.request = Some(id);
                }
            }
            None => self.java_debug_fail("jdtls is still starting — try again in a moment".into()),
        }
    }

    pub(crate) fn java_debug_fail(&mut self, why: String) {
        self.java_debug = None;
        self.dap = None;
        self.set_error(format!("debug: {why}"));
    }

    /// jdtls reply to request `id` — on to the next step; at the last one (port), attach.
    pub fn java_debug_reply(
        &mut self,
        cid: crate::lsp::ClientId,
        id: u64,
        error: Option<&Value>,
        result: &Value,
    ) {
        if !self.java_debug.as_ref().is_some_and(|p| p.cid == cid && p.request == Some(id)) {
            return;
        }
        let Some(mut p) = self.java_debug.take() else { return };
        if p.generation != self.dap_generation {
            return;
        }
        if let Some(e) = error {
            let m = e["message"].as_str().unwrap_or("request failed");
            // jdtls started before the jar existed (no such command) — load it once and retry
            if m.contains("No delegateCommandHandler") && !p.reloaded && debug_bundle().is_some() {
                (p.step, p.reloaded) = (Step::Reload, true);
                self.java_debug = Some(p);
                return self.java_debug_send();
            }
            let hint = if m.contains("No delegateCommandHandler") || m.contains("not supported") {
                " (java-debug plugin not loaded — restart tarae after installing it)"
            } else {
                ""
            };
            return self.java_debug_fail(format!("{m}{hint}"));
        }
        match p.step {
            Step::Reload => {
                if result != &Value::Bool(true) {
                    return self.java_debug_fail("jdtls couldn't load java-debug — restart tarae".into());
                }
                p.step = if p.attach.is_some() { Step::Start } else { Step::MainClass };
                p.reloaded = true;
            }
            Step::MainClass => {
                let list = result.as_array().cloned().unwrap_or_default();
                let Some((main, project, others)) = pick_main(&list, &p.file) else {
                    return self.java_debug_fail("no main class found (public static void main)".into());
                };
                if !others.is_empty() {
                    self.set_status(format!("Debugging {main} — also: {}", others.join(", ")));
                }
                if let Some(d) = &mut self.dap {
                    d.program = main.rsplit('.').next().unwrap_or(&main).to_string();
                }
                (p.main, p.project, p.step) = (main, project, Step::Classpath);
            }
            Step::Classpath => {
                p.module_paths = result[0].clone();
                p.class_paths = result[1].clone();
                p.step = Step::JavaExec;
            }
            Step::JavaExec => {
                p.java_exec = result.clone();
                p.step = Step::Start;
            }
            Step::Start => {
                let Some(port) = result.as_u64().and_then(|n| u16::try_from(n).ok()) else {
                    return self.java_debug_fail(format!("no debug port from jdtls ({result})"));
                };
                let (request, args) = match &p.attach {
                    Some((host, port)) => {
                        ("attach", json!({ "hostName": host, "port": port, "timeout": 10000 }))
                    }
                    None => (
                        "launch",
                        json!({
                            "mainClass": p.main,
                            "projectName": p.project,
                            "modulePaths": p.module_paths,
                            "classPaths": p.class_paths,
                            "javaExec": p.java_exec,
                            "cwd": p.root,
                            "args": "",
                            "vmArgs": "",
                            "console": "internalConsole",
                            "shortenCommandLine": "none",
                            "stopOnEntry": false,
                        }),
                    ),
                };
                // Connecting is a socket wait — worker thread; a newer/ended debug run drops the result
                let generation = p.generation;
                self.events.jobs().spawn(move || {
                    let conn = std::net::TcpStream::connect(("127.0.0.1", port))
                        .and_then(|s| Ok((s.try_clone()?, s)))
                        .map_err(|e| format!("connect to java-debug: {e}"));
                    move |ed: &mut crate::editor::Editor| {
                        if ed.dap_generation != generation {
                            return;
                        }
                        let (reader, writer) = match conn {
                            Ok(c) => c,
                            Err(e) => return ed.java_debug_fail(e),
                        };
                        let program = ed.dap.as_ref().map(|d| d.program.clone()).unwrap_or_default();
                        ed.dap_connect(
                            "java",
                            Box::new(reader),
                            Box::new(writer),
                            None,
                            args,
                            program,
                            generation,
                        );
                        if let Some(d) = &mut ed.dap {
                            d.request = request;
                        }
                    }
                });
                return;
            }
        }
        self.java_debug = Some(p);
        self.java_debug_send();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_main_of_the_current_file() {
        let list = [
            json!({ "mainClass": "app.Tool", "projectName": "p", "filePath": "/p/src/app/Tool.java" }),
            json!({ "mainClass": "app.Main", "projectName": "p", "filePath": "/p/src/app/Main.java" }),
        ];
        let (m, proj, others) = pick_main(&list, Path::new("/p/src/app/Main.java")).unwrap();
        assert_eq!((m.as_str(), proj.as_str(), others), ("app.Main", "p", vec!["app.Tool".to_string()]));
        let (m, ..) = pick_main(&list, Path::new("/p/src/app/Util.java")).unwrap();
        assert_eq!(m, "app.Tool", "first one if the current file has no main");
        assert!(pick_main(&[], Path::new("/x")).is_none());
    }
}
