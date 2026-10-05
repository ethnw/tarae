//! Acting on what the sandbox blocked — the user's call, never Claude's: `space m s` on a command's panel,
//! `space x s` on a test run's, `:sandbox-blocked`. A list made of the run's denials: let Claude's commands
//! read a place in home (`allow::read_choices` — never keys or Claude's and tarae's own; Claude's Read reads
//! it too), write in a folder (each folder from the denied path's own up to just below home — how wide is the
//! user's call; `allow::write_choices` leaves out what must never open), connect to a port on this machine
//! (not the risky ones — `sandbox::RISKY_PORTS`, by hand only), or run that one command outside the
//! sandbox. Each is a weighty card. A write or a port is for once (the command runs again now with it),
//! for this conversation, or always (`allow.rs`) — and runs it again now too. That run is the user's: how it
//! ended goes to Claude with the next message (`tools::For::User`).

use std::path::{Path, PathBuf};

use crate::allow::{Grant, Scope};
use crate::editor::Editor;
use crate::sandbox::Denial;
use crate::tools::{Extra, For, Job};

/// A run of Claude's the sandbox blocked something in: what ran, and what was denied.
#[derive(Clone, Debug)]
pub struct BlockedRun {
    pub job: Job,
    pub denials: Vec<Denial>,
}

impl BlockedRun {
    /// What ran, as the cards and Claude say it.
    pub fn label(&self) -> String {
        match &self.job {
            Job::Command(c, _) => c.clone(),
            Job::Tests(t) => format!("tests: {}", t.label),
        }
    }
}

/// Which run's denials: the command's panel, the test run's, or whichever has them (newest first).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Which {
    Task,
    Test,
    Either,
}

/// One thing that could open: what, its list row's hint (what was denied, short — the part below the
/// folder offered), and what its card says was denied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    pub grant: Grant,
    pub hint: String,
    pub why: String,
}

/// What could open for `denials`: the places in home a read may open (`allow::read_choices` — never what's
/// kept from every run), the folders a write may open (`allow::write_choices`), a port on this machine (not a
/// risky one) — each once. Home, the project and the folders on PATH passed in.
pub fn choices(denials: &[Denial], home: &Path, root: &Path, path: &[PathBuf]) -> Vec<Choice> {
    let mut out: Vec<Choice> = Vec::new();
    let kept = crate::sandbox::kept_reads(home, None, &crate::sandbox::HomeVars::here());
    for d in denials {
        let target = crate::sandbox::profile_path(Path::new(&d.target));
        let below = |dir: &Path| {
            let below = target.strip_prefix(dir).map(|r| format!("…/{}", r.display()));
            below.ok().filter(|b| b != "…/").unwrap_or_else(|| crate::allow::tilde(&target))
        };
        let file = d.target.starts_with('/');
        let mut found: Vec<Choice> = Vec::new();
        if file && (d.op.starts_with("file-read-data") || d.op == crate::sandbox::EITHER) {
            found.extend(crate::allow::read_choices(&target, home, &kept).into_iter().map(|place| {
                let why = format!("{} was reading {} in it", d.process, below(&place));
                Choice { hint: below(&place), grant: Grant::Read(place), why }
            }));
        }
        if file && (d.op.starts_with("file-write") || d.op == crate::sandbox::EITHER) {
            found.extend(crate::allow::write_choices(&target, home, root, path).into_iter().map(|dir| {
                let why = format!("{} was writing {} in it", d.process, below(&dir));
                Choice { hint: below(&dir), grant: Grant::Write(dir), why }
            }));
        } else if d.op == "network-outbound"
            && let Some(n) = d.target.rsplit_once(':').and_then(|(_, p)| p.parse::<u16>().ok())
            && !crate::sandbox::RISKY_PORTS.iter().any(|(p, _)| *p == n)
        {
            let why = format!("{} was connecting to it", d.process);
            found.push(Choice { grant: Grant::Port(n), hint: format!("{} connecting", d.process), why });
        } else if d.op == "network-outbound" && file {
            // A unix socket (#119): that file, by its real path — one that's there and is a socket
            use std::os::unix::fs::FileTypeExt as _;
            let real = std::fs::canonicalize(&d.target).ok().filter(|p| {
                std::fs::metadata(p).is_ok_and(|m| m.file_type().is_socket()) && !p.starts_with(root)
            });
            let why = format!("{} was connecting to it", d.process);
            found.extend(real.map(|s| Choice {
                grant: Grant::Socket(s),
                hint: format!("{} connecting", d.process),
                why,
            }));
        }
        for c in found {
            if !out.iter().any(|o| o.grant == c.grant) {
                out.push(c);
            }
        }
    }
    out
}

/// The folders on `PATH` (real paths) — never opened for writing: a program put there runs as the user.
pub fn path_dirs() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).filter_map(|d| std::fs::canonicalize(d).ok()).collect())
        .unwrap_or_default()
}

impl Editor {
    /// The run of Claude's whose denials `which` means, if the sandbox blocked anything in it — what the
    /// kernel reported, and what its output says (`Policy::said_in_output`: the kernel's report gets lost).
    fn blocked_run(&self, which: Which) -> Option<BlockedRun> {
        let denials = |g: Option<&crate::sandbox::Guard>, output: &[(bool, String)]| -> Option<Vec<Denial>> {
            let g = g?;
            let mut seen = g.watch().map(|w| w.blocked().seen).unwrap_or_default();
            if let crate::sandbox::Guard::In(p) = g {
                let said = p.said_in_output(output, &seen);
                seen.extend(said);
            }
            (!seen.is_empty()).then_some(seen)
        };
        let task = self.session.task_run.as_ref().and_then(|r| {
            Some((
                r.started,
                BlockedRun {
                    job: Job::Command(r.task.cmd.clone(), r.task.cwd.clone()),
                    denials: denials(r.guard.as_ref(), &r.output)?,
                },
            ))
        });
        let test = self.session.test_run.as_ref().and_then(|r| {
            let job = Job::Tests(Box::new(r.target.clone()));
            Some((r.started, BlockedRun { job, denials: denials(r.guard.as_ref(), &r.output)? }))
        });
        match which {
            Which::Task => task.map(|t| t.1),
            Which::Test => test.map(|t| t.1),
            Which::Either => match (task, test) {
                (Some(a), Some(b)) => Some(if a.0 >= b.0 { a.1 } else { b.1 }),
                (a, b) => a.or(b).map(|t| t.1),
            },
        }
    }

    /// `space m s` · `space x s` · `:sandbox-blocked` — what could open for what the sandbox blocked in that
    /// run: a list; Enter puts one on its card.
    pub fn sandbox_blocked(&mut self, which: Which) {
        let Some(run) = self.blocked_run(which) else {
            return self.set_status("the sandbox blocked nothing in that run — its panel says when it does");
        };
        let home = crate::allow::home().unwrap_or_else(|| PathBuf::from("/"));
        let list = choices(&run.denials, &home, &self.session.root, &path_dirs());
        use crate::picker::{Action, Item};
        let mut items: Vec<Item> = list
            .iter()
            .enumerate()
            .map(|(i, c)| Item {
                label: match &c.grant {
                    Grant::Read(p) => format!("allow reading {}", crate::allow::tilde(p)),
                    Grant::Write(p) => format!("allow writing in {}", crate::allow::tilde(p)),
                    Grant::Port(n) => format!("allow port {n} on this machine"),
                    Grant::Socket(s) => format!("allow the socket {} only", crate::allow::tilde(s)),
                },
                action: Action::Grant(i.to_string()),
                hint: c.hint.clone(),
                glyph: Some(("◦", "ui.virtual")),
            })
            .collect();
        items.push(Item {
            label: "run it outside the sandbox, once".into(),
            action: Action::Grant("outside".into()),
            hint: run.label(),
            glyph: Some(("▲", "warning")),
        });
        self.session.blocked = Some((run, list));
        let p = crate::picker::Picker::new("what the sandbox blocked", items, false).without_preview();
        self.open_picker(p, None);
    }

    /// A row of the list (`Action::Grant`: a grant's number, or `outside`) — its card.
    pub fn sandbox_grant(&mut self, arg: &str) -> Result<(), String> {
        let (run, list) =
            self.session.blocked.clone().ok_or("nothing blocked to act on — space m s on a run's panel")?;
        use crate::cards::{Asked, Past, Request, Why};
        let cmd = run.label();
        let run = Box::new(run);
        let card = match arg.trim() {
            "outside" => crate::offer::Offer::new(
                OUTSIDE_TITLE.into(),
                &format!(
                    "{cmd}\n\nOutside the sandbox it runs with all your permissions — your files, your network — \
                     this once. Claude hears how it ended with your next message."
                ),
                "",
                String::new(),
                crate::offer::What::Outside { run },
            )
            .asked(Asked::new(
                Why::Blocked,
                Request::Run { past: Past::Outside, sandboxed: true, reads: Vec::new() },
            )),
            n => {
                let Choice { grant, why, .. } =
                    n.parse::<usize>().ok().and_then(|i| list.get(i).cloned()).ok_or("no such row")?;
                // A read: its card says first what looks like a token there — looked through on a worker
                let Grant::Read(place) = &grant else {
                    self.push_offer(grant_card(grant, &why, &cmd, run, None));
                    return Ok(());
                };
                let place = place.clone();
                self.events.jobs().spawn(move || {
                    let said = crate::tokens::found_in(&place).said(&place);
                    move |ed: &mut Editor| ed.push_offer(grant_card(grant, &why, &cmd, run, said))
                });
                return Ok(());
            }
        };
        self.push_offer(card);
        Ok(())
    }
}

/// The card that opens `grant` for what was blocked in `run` (`why`, in `cmd`) — warning what it opens
/// (Docker's socket; a place holding a token-like line: `tokens`).
fn grant_card(
    grant: Grant,
    why: &str,
    cmd: &str,
    run: Box<BlockedRun>,
    tokens: Option<String>,
) -> crate::offer::Offer {
    use crate::cards::{Asked, Request, Why};
    // (a socket: this run, or this conversation — everything else stays in the sandbox)
    let lasts = match grant.keepable() {
        true => "Always lasts until :sandbox-allowed takes it back.",
        false => "Everything else stays in the sandbox; it's never kept past this conversation.",
    };
    let mut question = format!(
        "Let Claude's commands {}\n\n{why}, in `{cmd}` — which runs again now with it open, whichever you \
         choose. {lasts}",
        grant.shown()
    );
    let warn: Vec<String> = grant.warning().map(String::from).into_iter().chain(tokens).collect();
    let warning = (!warn.is_empty()).then(|| {
        question.push_str("\n\n");
        let at = question.split('\n').count() - 1;
        question.push_str(&warn.join("\n"));
        at..at + warn.len()
    });
    let asked = Asked::new(Why::Blocked, Request::Grant(grant.clone()));
    let what = crate::offer::What::Grant { grant, run };
    let o = crate::offer::Offer::new(GRANT_TITLE.into(), &question, "", String::new(), what);
    match warning {
        Some(lines) => o.warning(lines).asked(asked),
        None => o.asked(asked),
    }
}

pub const GRANT_TITLE: &str = "Open the sandbox a little?";
pub const OUTSIDE_TITLE: &str = "Run it outside the sandbox?";

/// The answer on a grant's card: None = no; else `grant` is allowed for `scope` and the run goes again now
/// with it — the user's run, its end told to Claude.
pub fn grant_answer(ed: &mut Editor, id: u64, grant: Grant, run: BlockedRun, scope: Option<Scope>) {
    ed.offer_finished(id, Ok(()));
    let Some(scope) = scope else {
        return ed.set_status("nothing opened — the sandbox stays as it was");
    };
    crate::allow::grant(ed, &grant, scope);
    let head = format!(
        "The user ran `{}` again, letting it {}{}. How it ended:",
        run.label(),
        grant.shown(),
        if scope == Scope::Once { " this once" } else { "" }
    );
    let extra = Extra { grant: (scope == Scope::Once).then_some(grant), outside: false };
    crate::tools::guard_and_go(ed, For::User(head), run.job, true, extra);
}

/// The answer on the run-outside card: `y` runs it now, outside the sandbox — the user's run.
pub fn outside_answer(ed: &mut Editor, id: u64, run: BlockedRun, yes: bool) {
    ed.offer_finished(id, Ok(()));
    if !yes {
        return ed.set_status("not run — the sandbox stays as it was");
    }
    let head = format!("The user ran `{}` again outside the sandbox, this once. How it ended:", run.label());
    crate::tools::guard_and_go(ed, For::User(head), run.job, true, Extra { grant: None, outside: true });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a run's denials offer: the folders a write may open, a port that isn't a risky one — once each.
    #[test]
    fn what_a_runs_denials_offer() {
        let base = std::env::temp_dir().join(format!("tarae-blocked-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        std::fs::create_dir_all(base.join("home/.npm/_cacache")).unwrap();
        std::fs::create_dir_all(base.join("home/proj")).unwrap();
        let home = std::fs::canonicalize(base.join("home")).unwrap();
        let root = home.join("proj");
        let d = |op: &str, target: String| Denial { process: "npm".into(), op: op.into(), target };
        let denials = [
            d("file-write-create", home.join(".npm/_cacache/x").display().to_string()),
            d("file-write-data", home.join(".npm/_cacache/y").display().to_string()),
            d("network-outbound", "remote:*:5432".into()),
            d("network-outbound", "remote:*:2375".into()),
            d("file-write-create", root.join(".git/index.lock").display().to_string()),
            d("mach-lookup", "com.apple.lsd.mapdb".into()),
        ];
        let all = choices(&denials, &home, &root, &[]);
        assert_eq!((all[0].hint.as_str(), all[1].hint.as_str()), ("…/x", "…/_cacache/x"), "below the folder");
        assert_eq!(all[2].why, "npm was connecting to it");
        let got: Vec<Grant> = all.into_iter().map(|c| c.grant).collect();
        assert_eq!(
            got,
            [Grant::Write(home.join(".npm/_cacache")), Grant::Write(home.join(".npm")), Grant::Port(5432)],
            "Docker's port and the project's .git aren't offered; each folder once"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    /// Card model P2: a read the sandbox denied in home offers the places it may open, nearest first — never
    /// a key's; one only the output said (reading or writing — it doesn't say which) offers both.
    #[test]
    fn what_a_denied_read_offers() {
        let base = std::env::temp_dir().join(format!("tarae-blocked-read-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        for d in ["home/src/lib", "home/.ssh", "home/notes", "home/proj"] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        let home = std::fs::canonicalize(base.join("home")).unwrap();
        let d = |op: &str, target: PathBuf| Denial {
            process: "cat".into(),
            op: op.into(),
            target: target.display().to_string(),
        };
        let denials = [
            d("file-read-data", home.join("src/lib/a.rs")),
            d("file-read-data", home.join(".ssh/id_ed25519")),
            d(crate::sandbox::EITHER, home.join("notes/x")),
        ];
        let all = choices(&denials, &home, &home.join("proj"), &[]);
        assert_eq!((all[0].hint.as_str(), all[1].hint.as_str()), ("…/a.rs", "…/lib/a.rs"));
        assert_eq!(all[0].why, "cat was reading …/a.rs in it");
        let got: Vec<Grant> = all.into_iter().map(|c| c.grant).collect();
        assert_eq!(
            got,
            [
                Grant::Read(home.join("src/lib")),
                Grant::Read(home.join("src")),
                Grant::Read(home.join("notes")),
                Grant::Write(home.join("notes")),
            ],
            "nothing for the key"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    /// A read's grant card says first when the place holds a token-like line — by the file's name, never the
    /// value, in the warning colour; it comes once the place's been looked through (a worker's). A place
    /// without one says nothing of tokens. (Fake values in the test's own temp folder.)
    #[test]
    fn a_grants_card_says_when_a_token_is_there() {
        let base = crate::sandbox::profile_path(&std::env::temp_dir())
            .join(format!("tarae-blocked-tok-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        for (f, text) in [("conf/hosts.yml", "oauth_token: tarae-fake-token\n"), ("plain/x.txt", "marker\n")]
        {
            std::fs::create_dir_all(base.join(f).parent().unwrap()).unwrap();
            std::fs::write(base.join(f), text).unwrap();
        }
        let mut ed = Editor::new(crate::config::Config::default());
        let run = BlockedRun { job: Job::Command("cat x".into(), base.clone()), denials: Vec::new() };
        let choice = |dir: &str| Choice {
            grant: Grant::Read(base.join(dir)),
            hint: String::new(),
            why: "cat was reading …/x in it".into(),
        };
        ed.session.blocked = Some((run, vec![choice("conf"), choice("plain")]));
        let card = |ed: &mut Editor, row: &str| {
            ed.offers.clear();
            ed.sandbox_grant(row).unwrap();
            let end = std::time::Instant::now() + std::time::Duration::from_secs(60);
            while ed.offers.is_empty() {
                let left = end.saturating_duration_since(std::time::Instant::now());
                let ev = ed.events.recv_timeout(left).expect("the card before the timeout");
                ed.handle_event(ev);
            }
            let o = &ed.offers[0];
            let warned: Vec<&str> = match &o.warning {
                Some(r) => o.question.split('\n').skip(r.start).take(r.len()).collect(),
                None => Vec::new(),
            };
            (o.question.clone(), warned.join("\n"))
        };
        let (q, warned) = card(&mut ed, "0");
        let said = format!("In {}, hosts.yml holds a token-like line.", base.join("conf").display());
        assert_eq!(warned, said, "{q}");
        assert!(!q.contains("tarae-fake-token"), "{q}");
        let (q, warned) = card(&mut ed, "1");
        assert!(warned.is_empty() && !q.contains("token"), "{q}");
        std::fs::remove_dir_all(&base).ok();
    }

    /// #119: a denied connect to a unix socket offers that socket — its real path, the file only — and not
    /// a path that isn't a socket, nor one in the project (that's open already); Docker's says what it is,
    /// and none is kept for good.
    #[test]
    fn a_denied_socket_offers_that_socket_only() {
        // (a socket's path has 104 bytes at most: short names — the temp folder is the home)
        let base =
            crate::sandbox::profile_path(&std::env::temp_dir()).join(format!("k{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        std::fs::create_dir_all(base.join(".colima/t")).unwrap();
        std::fs::create_dir_all(base.join("p")).unwrap();
        let home = base.clone();
        let (root, sock) = (home.join("p"), home.join(".colima/t/docker.sock"));
        let _l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let _in = std::os::unix::net::UnixListener::bind(root.join("own.sock")).unwrap();
        std::os::unix::fs::symlink(&sock, home.join("docker.sock")).unwrap();
        std::fs::write(home.join("plain.txt"), "marker").unwrap();
        let d = |target: PathBuf| Denial {
            process: "docker".into(),
            op: "network-outbound".into(),
            target: target.display().to_string(),
        };
        let denials = [
            d(home.join("docker.sock")),
            d(sock.clone()),
            d(home.join("plain.txt")),
            d(root.join("own.sock")),
            d(home.join("gone.sock")),
        ];
        let got: Vec<Grant> = choices(&denials, &home, &root, &[]).into_iter().map(|c| c.grant).collect();
        assert_eq!(got, [Grant::Socket(sock.clone())], "the link's socket, once — nothing else");
        let g = Grant::Socket(sock);
        assert!(!g.keepable() && g.warning().is_some_and(|w| w.contains("mounts any folder")));
        assert!(Grant::Socket("/tmp/x/app.sock".into()).warning().is_none());
        std::fs::remove_dir_all(&base).ok();
    }
}
