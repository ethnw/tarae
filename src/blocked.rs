//! Acting on what the sandbox blocked — the user's call, never Claude's: `space m s` on a command's panel,
//! `space x s` on a test run's, `:sandbox-blocked`. A list made of the run's denials: let Claude's commands
//! read a place in home (`allow::read_choices` — never keys or Claude's and tarae's own; Claude's Read reads
//! it too), write in a folder (each folder from the denied path's own up to just below home — how wide is the
//! user's call; `allow::write_choices` leaves out what must never open), connect to a port on this machine
//! (not the risky ones — `sandbox::RISKY_PORTS`, by hand only), or run that one command outside the
//! sandbox. Each is a weighty card. A write or a port is for once (the command runs again now with it),
//! for this conversation, this session, or always (`allow.rs`; no longer than `Caps::storable` lets it) — and
//! runs it again now too. That run is the user's: how it ended goes to Claude with the next message
//! (`tools::For::User`).

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
    /// Linux: where the denials were told from (`Policy::denials_from`) — the cards say it.
    pub from: Option<String>,
    /// Claude's call waiting on the card, when Claude asked for it (`also` — `tools::also_ask`): the run is its,
    /// not the user's run again, and a no is said to it.
    pub claude: Option<crate::tools::Call>,
    /// The variables left out of its environment that its output names (`RunEnv::named`) — names only.
    pub env: Vec<String>,
    /// Claude's run's report, held while the card about it waits (Gradle's loopback — `loopback_card`): a no
    /// gives Claude that.
    pub report: Option<String>,
    /// Which run it was (the panel's) — the user's run again of it goes on by itself when it was Claude's last
    /// turn's failed one (piece D3, `Chat.failed`).
    pub key: Option<RunKey>,
}

/// A run of the session's: its command's or its tests', and which of them (`generation`: each spawn a new one).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunKey {
    pub tests: bool,
    pub generation: u64,
}

impl BlockedRun {
    /// `job`, nothing blocked in it known, nobody waiting.
    pub fn of(job: Job) -> BlockedRun {
        BlockedRun {
            job,
            denials: Vec::new(),
            from: None,
            claude: None,
            env: Vec::new(),
            report: None,
            key: None,
        }
    }

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

/// What could open for `denials` in a session that may do `caps`: the places in home a read may open
/// (`allow::read_choices` — never what's kept from every run, `Caps.kept`), the folders a write may open
/// (`allow::write_choices` — never where a write runs something later, `Caps.later`), a port on this machine
/// (not a risky one) — each once.
pub fn choices(denials: &[Denial], caps: &crate::caps::Caps) -> Vec<Choice> {
    let mut out: Vec<Choice> = Vec::new();
    let home = caps.home.as_deref().unwrap_or(Path::new("/"));
    let (root, kept, later) = (caps.root.as_path(), &caps.kept, &caps.later);
    for d in denials {
        let target = crate::sandbox::profile_path(Path::new(&d.target));
        let below = |dir: &Path| {
            let below = target.strip_prefix(dir).map(|r| format!("…/{}", r.display()));
            below.ok().filter(|b| b != "…/").unwrap_or_else(|| crate::allow::tilde(&target))
        };
        let file = d.target.starts_with('/');
        let mut found: Vec<Choice> = Vec::new();
        // A terminal (macOS, 3c): the terminals — no folder of `/dev` nor the file itself
        if d.terminal() {
            let why = format!("{} was opening {}", d.process, d.target);
            found.push(Choice {
                grant: Grant::Terminals,
                hint: format!("{} opening a terminal", d.process),
                why,
            });
        }
        // A host the proxy held and the user's answer would lift (`host:port: why`) — never what its other checks
        // refused (an address, the server name)
        if d.op == crate::sandbox::proxy::HOST_OP
            && let Some((host, port)) =
                d.target.split_once(": ").and_then(|(at, _)| crate::allow::host_port(at))
        {
            let why = format!("{} was reaching it", d.process);
            found.push(Choice {
                grant: Grant::Host(host, port),
                hint: format!("{} reaching", d.process),
                why,
            });
        }
        let read = d.op.starts_with("file-read-data") || d.op == crate::sandbox::EITHER;
        // A file named like a key in the project (7a, 7b): that one file — no folder of it (the project is
        // read anyway; what's closed is the name)
        let key = file && read && crate::sandbox::key_named_in(&crate::sandbox::profile_path(root), &target);
        if key {
            found.push(Choice {
                grant: Grant::Key(target.clone()),
                hint: format!("{} reading {}", d.process, below(&crate::sandbox::profile_path(root))),
                why: format!("{} was reading {}, named like a key", d.process, crate::allow::tilde(&target)),
            });
        } else if file && !d.terminal() && read {
            found.extend(crate::allow::read_choices(&target, home, kept).into_iter().map(|place| {
                let why = format!("{} was reading {} in it", d.process, below(&place));
                Choice { hint: below(&place), grant: Grant::Read(place), why }
            }));
        }
        if file && !d.terminal() && (d.op.starts_with("file-write") || d.op == crate::sandbox::EITHER) {
            found.extend(crate::allow::write_choices(&target, home, root, later).into_iter().map(|dir| {
                let why = format!("{} was writing {} in it", d.process, below(&dir));
                Choice { hint: below(&dir), grant: Grant::Write(dir), why }
            }));
        } else if let Some(n) = d.port_here()
            && !crate::sandbox::RISKY_PORTS.iter().any(|(p, _)| *p == n)
        {
            let why = format!("{} was connecting to it", d.process);
            let hint = format!("{} connecting", d.process);
            // (every port here: what picks its ports as it starts — Gradle's client and daemon — needs them all;
            // the one it was refused, picked anew at its next start, would only fail again: not offered)
            let picked = crate::sandboxpack::get().refused_port.iter().any(|r| r.picks(d));
            if !picked {
                found.push(Choice { grant: Grant::Port(n), hint: hint.clone(), why: why.clone() });
            }
            found.push(Choice { grant: Grant::Localhost, hint, why });
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

/// What could open for the variables `names` (left out of a run's environment, named in its output): each one
/// passed whole — never a name `readonly::env_allowed` keeps off (what finds programs, runs or loads code, the
/// packs' own).
pub fn env_choices(names: &[String]) -> Vec<Choice> {
    names
        .iter()
        .filter(|n| crate::readonly::env_allowed(n))
        .map(|n| Choice {
            grant: Grant::Env(n.clone()),
            hint: "named in its output".into(),
            why: format!("Its output names {n}, left out of your commands' environment"),
        })
        .collect()
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
        // (what it was refused, and the variables left out its output names — None: neither)
        let denials = |g: Option<&crate::sandbox::Guard>,
                       output: &[(bool, String)]|
         -> Option<(Vec<Denial>, Vec<String>)> {
            let g = g?;
            let mut seen = g.watch().map(|w| w.blocked().seen).unwrap_or_default();
            if let crate::sandbox::Guard::In(p) = g {
                let said = p.said_in_output(output, &seen);
                let rest = p.merge_made(&mut seen, said);
                seen.extend(rest);
            }
            let env = g.run_env().map(|e| e.named(output)).unwrap_or_default();
            (!seen.is_empty() || !env.is_empty()).then_some((seen, env))
        };
        let from = |g: Option<&crate::sandbox::Guard>| match g {
            Some(crate::sandbox::Guard::In(p)) => p.denials_from(),
            _ => None,
        };
        // (a run on a commit's tree: not offered — its tree is gone, so neither a grant nor a run outside could
        // run it again where it ran — review of #212)
        let task = self.session.task_run.as_ref().filter(|r| r.base.is_none()).and_then(|r| {
            let (denials, env) = denials(r.guard.as_ref(), &r.output)?;
            Some((
                r.started,
                BlockedRun {
                    denials,
                    from: from(r.guard.as_ref()),
                    env,
                    key: Some(RunKey { tests: false, generation: r.generation() }),
                    ..BlockedRun::of(Job::Command(r.task.cmd.clone(), r.task.cwd.clone()))
                },
            ))
        });
        let test = self.session.test_run.as_ref().and_then(|r| {
            let job = Job::Tests(Box::new(r.target.clone()));
            let from = from(r.guard.as_ref());
            let (denials, env) = denials(r.guard.as_ref(), &r.output)?;
            let key = Some(RunKey { tests: true, generation: r.generation() });
            Some((r.started, BlockedRun { denials, from, env, key, ..BlockedRun::of(job) }))
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
        // (the run meant was on a commit's tree: said, before what it was refused — review of #212)
        let base = self.session.task_run.as_ref().filter(|r| r.base.is_some());
        let newer =
            |t: &crate::task::Run| self.session.test_run.as_ref().is_none_or(|x| t.started >= x.started);
        let on_base = match which {
            Which::Task => base.is_some(),
            Which::Test => false,
            Which::Either => base.is_some_and(newer),
        };
        if on_base {
            return self.set_status(
                "that run was on a commit's tree, gone since — nothing of it to open or run again",
            );
        }
        // (nothing blocked: the panel still opens — its state line, and why its list is empty)
        let Some(run) = self.blocked_run(which) else { return self.blocked_open(None, Vec::new()) };
        // (what could open, by what the session may do — made on a worker: it reads the kept answers, and the
        // places never opened are looked for)
        let basis = self.caps_basis();
        self.events.jobs().spawn(move || {
            let mut list = choices(&run.denials, &basis.build());
            list.extend(env_choices(&run.env));
            move |ed: &mut Editor| ed.blocked_open(Some(run), list)
        });
    }

    /// The list of what the sandbox blocked in `run`, `list` what could open.
    fn blocked_open(&mut self, run: Option<BlockedRun>, list: Vec<Choice>) {
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
                    Grant::Host(h, p) => format!("allow reaching {h}:{p}"),
                    Grant::Terminals => "allow opening terminals — yours too".into(),
                    Grant::Key(k) => format!("allow reading {} — that file only", crate::allow::tilde(k)),
                    Grant::Localhost => "allow every port on this machine — the risky ones too".into(),
                    Grant::Env(n) => format!("allow passing your variable {n}"),
                },
                action: Action::Grant(i.to_string()),
                hint: c.hint.clone(),
                glyph: Some(("◦", "ui.virtual")),
            })
            .collect();
        if let Some(run) = &run {
            items.push(Item {
                label: "run it outside the sandbox, once".into(),
                action: Action::Grant("outside".into()),
                hint: run.label(),
                glyph: Some(("▲", "warning")),
            });
        }
        // (9f: where tarae's AppArmor profile would raise the tier, its step to copy — after what was blocked)
        let step = self.session.sandbox.as_ref().and_then(|g| g.step_row());
        let nothing = run.is_none();
        items.extend(step.map(|(label, hint)| Item {
            label,
            action: Action::Grant(PROFILE_STEP.into()),
            hint: hint.into(),
            glyph: Some(("◦", "ui.virtual")),
        }));
        self.session.blocked = run.map(|run| (run, list));
        let mut p =
            crate::picker::Picker::of("blocked", "what the sandbox blocked", items, false).without_preview();
        // (before any sandboxed run, one thing said: no header, and the list's row one says there's been none)
        p.head = self.session.sandbox.as_ref().map(head);
        let empty = if self.session.sandbox.is_some() { NOTHING_BLOCKED } else { NO_RUN_YET };
        // (nothing blocked but a step to copy: said where the list's first row would be, the step's row under it)
        let listed = !p.items().is_empty();
        match (&mut p.head, nothing && listed) {
            (Some(h), true) => h.note = Some(empty.into()),
            _ => p.empty = Some(empty.into()),
        }
        self.open_picker(p, None);
    }

    /// A row of the list (`Action::Grant`: a grant's number, or `outside`) — its card; the profile's step
    /// (`PROFILE_STEP`) copied, and said so (`clipboard::copy`: the system clipboard, as `space y`).
    pub fn sandbox_grant(&mut self, arg: &str) -> Result<(), String> {
        if arg == PROFILE_STEP {
            let hint = self.session.sandbox.as_ref().and_then(|g| g.hint.as_ref());
            let (step, to) =
                hint.and_then(|h| Some((h.step.clone()?, h.to.said()))).ok_or("no step to copy")?;
            let what =
                format!("the step that makes this tier {to} — run it in a terminal, then restart tarae");
            crate::clipboard::copy(&self.events.jobs(), step, what);
            return Ok(());
        }
        let (run, list) =
            self.session.blocked.clone().ok_or("nothing blocked to act on — space m s on a run's panel")?;
        use crate::cards::{Asked, Past, Request, Why};
        let cmd = run.label();
        let then = then_said(self, &run);
        let run = Box::new(run);
        let card = match arg.trim() {
            "outside" => crate::offer::Offer::new(
                OUTSIDE_TITLE.into(),
                &format!(
                    "{cmd}\n\nOutside the sandbox it runs with all your permissions — your files, your network — \
                     this once. {then}"
                ),
                "",
                String::new(),
                crate::offer::What::Outside { run },
            )
            .asked(
                Asked::new(
                    Why::Blocked,
                    Request::Run {
                        past: Past::Outside,
                        sandboxed: crate::cards::sandboxed(self.config.llm.sandbox, &self.sandbox),
                        reads: Vec::new(),
                        outward: false,
                    },
                )
                .about(&cmd),
            ),
            n => {
                let Choice { grant, why, .. } =
                    n.parse::<usize>().ok().and_then(|i| list.get(i).cloned()).ok_or("no such row")?;
                // A read: its card says first what looks like a token there — looked through on a worker
                let Grant::Read(place) = &grant else {
                    self.push_offer(grant_card(grant, &why, &cmd, run, None, then));
                    return Ok(());
                };
                let place = place.clone();
                self.events.jobs().spawn(move || {
                    let said = crate::tokens::found_in(&place).said(&place);
                    move |ed: &mut Editor| {
                        let then = then_said(ed, &run);
                        ed.push_offer(grant_card(grant, &why, &cmd, run, said, then))
                    }
                });
                return Ok(());
            }
        };
        self.push_offer(card);
        Ok(())
    }
}

/// The card that opens `grant` for what was blocked in `run` (`why`, in `cmd`) — warning what it opens
/// (Docker's socket; a place holding a token-like line: `tokens`); `then`: how Claude hears its end (`then_said`).
fn grant_card(
    grant: Grant,
    why: &str,
    cmd: &str,
    run: Box<BlockedRun>,
    tokens: Option<String>,
    then: &str,
) -> crate::offer::Offer {
    use crate::cards::{Asked, Request, Why};
    // (every port for what picks its ports: the pack's words, as on the card Claude's turn held — `loopback_card`)
    let said = match (&grant, crate::tools::loopback_of(&run.denials)) {
        (Grant::Localhost, Some(card)) => loopback_said(&card, cmd, Some(then)),
        _ => format!("{why}, in `{cmd}` — which runs again now with it open, whichever you choose. {then}"),
    };
    let asked = Asked::new(Why::Blocked, Request::Grant(grant.clone())).about(cmd);
    grant_offer(grant, said, asked, run, tokens, Vec::new())
}

/// The card for what Claude's `also` asks (cardauto P3): `grant` for `cmd`, Claude's `reason` — its call waits on
/// it (`run.claude`); `y` runs the command with it open. `asks`: the command's warnings that ask where the run
/// could reach out (D4) — on this card, since any answer but `n` runs it, as the run card's `y` would.
pub fn also_card(
    grant: Grant,
    reason: &str,
    cmd: &str,
    run: Box<BlockedRun>,
    tokens: Option<String>,
    asks: Vec<crate::cardwarn::Warned>,
) -> crate::offer::Offer {
    use crate::cards::{Asked, Request, Why};
    let said = format!("Claude asks, for `{cmd}`: {reason}\n\nIt runs with it open, whichever you choose.");
    let ids = asks.iter().map(|w| w.id.clone()).collect();
    let asked = Asked::new(Why::Also, Request::Grant(grant.clone())).about(cmd).warned(ids);
    let lines = asks.iter().map(|w| format!("▲ {}", w.line())).collect();
    grant_offer(grant, said, asked, run, tokens, lines)
}

/// Gradle's loopback (piece D2): Claude's `cmd` failed because a program the packs say picks its ports as it
/// starts was refused one (`refused-port`) — every port's card at once, in the pack's words (`card`), Claude's
/// call waiting on it (`run.claude`) with the run's report (`run.report`): any answer but `n` runs it again with
/// them open, as Claude's run; `n` gives Claude the report.
pub fn loopback_card(card: &str, cmd: &str, run: Box<BlockedRun>) -> crate::offer::Offer {
    use crate::cards::{Asked, Request, Why};
    let said = loopback_said(card, cmd, None);
    let asked = Asked::new(Why::Blocked, Request::Grant(Grant::Localhost)).about(cmd);
    grant_offer(Grant::Localhost, said, asked, run, None, Vec::new())
}

/// Gradle's loopback card's words, Claude's turn holding it or not: the pack's `card`, then what failed and runs
/// again — Claude waiting on it (`then` None), or how it hears the end (`then_said`; the panel's card).
fn loopback_said(card: &str, cmd: &str, then: Option<&str>) -> String {
    let (waits, then) = match then {
        None => (" and waits", String::new()),
        Some(t) => ("", format!(" {t}")),
    };
    format!(
        "{card}\n\nClaude's `{cmd}` failed for it{waits}: it runs again now with them open, whichever you choose.{then}"
    )
}

/// A grant's card: what it lets, `said` (why, and what runs), how long it lasts, what it warns of (the grant's
/// risk, `tokens`, then the command's own: `warned`).
fn grant_offer(
    grant: Grant,
    said: String,
    asked: crate::cards::Asked,
    run: Box<BlockedRun>,
    tokens: Option<String>,
    warned: Vec<String>,
) -> crate::offer::Offer {
    // (as far as it may be kept: a socket this conversation's at most, a key and Docker's socket this run's —
    // everything else stays in the sandbox)
    let lasts = match (&grant, crate::caps::Caps::storable(&grant)) {
        (Grant::Host(..), _) => "This chat lasts until the conversation ends, this session until you quit.",
        (_, Scope::Always) => "s lasts until this session ends, A until :granted takes it back.",
        (_, Scope::Session) => "s lasts until this session ends — a name for a secret is never kept.",
        (_, Scope::ThisChat) => {
            "Everything else stays in the sandbox; it's never kept past this conversation."
        }
        (_, Scope::Once) => "Everything else stays in the sandbox; it's never kept past this run.",
    };
    let mut question = format!("Let Claude's commands {}\n\n{said} {lasts}", grant.shown());
    if let Some(from) = &run.from {
        question.push_str(&format!("\n\nWhat it refused was read {from}."));
    }
    let warn: Vec<String> = grant.risk().into_iter().chain(tokens).chain(warned).collect();
    let warning = (!warn.is_empty()).then(|| {
        question.push_str("\n\n");
        let at = question.split('\n').count() - 1;
        question.push_str(&warn.join("\n"));
        at..at + warn.len()
    });
    let what = crate::offer::What::Grant { grant, run };
    let o = crate::offer::Offer::new(GRANT_TITLE.into(), &question, "", String::new(), what);
    match warning {
        Some(lines) => o.warning(lines).asked(asked),
        None => o.asked(asked),
    }
}

/// The panel's row for the AppArmor profile's step (`Action::Grant`'s argument).
const PROFILE_STEP: &str = "profile";

/// The panel's list when the run meant had nothing blocked (or there's none).
const NOTHING_BLOCKED: &str = "the sandbox blocked nothing in that run";

/// The panel's list before the session's first sandboxed run — the one thing it says then (no header).
const NO_RUN_YET: &str = "no sandboxed run yet in this session";

/// The panel's state line (9e; the user's decision 14: here and `:procs`, never the status line): what the session's
/// last sandboxed run was kept apart from and shared (`Guarantees::panel_pairs`, folded by `Guarantees::FOLD` when
/// narrow), why not the top tier in a few words faint under it (`Guarantees::brief`; the whole `why` is the report's
/// and `tarae state`'s), and under that how the tier would go up (9f, `Guarantees::hint_brief`: the profile's).
fn head(g: &crate::sandbox::guarantees::Guarantees) -> crate::picker::Head {
    use crate::sandbox::guarantees::Guarantees;
    crate::picker::Head {
        pairs: g.panel_pairs(),
        fold: Guarantees::FOLD,
        faint: g.brief.clone(),
        then: g.hint_brief(),
        note: None,
    }
}

pub const GRANT_TITLE: &str = "Open the sandbox a little?";
pub const OUTSIDE_TITLE: &str = "Run it outside the sandbox?";

/// The answer on a grant's card: None = no; else `grant` is allowed for `scope` and the run goes again now
/// with it — the user's run, its end told to Claude.
pub fn grant_answer(ed: &mut Editor, id: u64, grant: Grant, mut run: BlockedRun, scope: Option<Scope>) {
    ed.offer_finished(id, Ok(()));
    // Claude's `also`: its call runs with it open — or hears no, which sticks for the chat
    if let Some(call) = run.claude.take() {
        // (its turn over: nobody waits — nothing kept, no refusal recorded, nothing runs)
        let waits = |c: &crate::chat::Chat| c.generation() == call.generation() && c.busy();
        if !ed.session.chat.as_ref().is_some_and(waits) {
            return;
        }
        let Some(scope) = scope else {
            if let Some(c) = ed.session.chat.as_mut() {
                c.also_refused.push(grant.clone());
            }
            let no = format!(
                "The user said no to letting your commands {} — for the rest of this chat: don't ask for it \
                 again. If the command can't work without it, tell the user what it needs.",
                grant.shown()
            );
            // (a card tarae raised on the run's refusal: Claude hears how the run ended, then the no)
            return match run.report.take() {
                Some(report) => crate::tools::answer(ed, call, Ok(format!("{report}\n{no}\n"))),
                None => crate::tools::answer(ed, call, Err(no)),
            };
        };
        crate::allow::grant(ed, &grant, scope);
        let extra = Extra { grant: (scope == Scope::Once).then_some(grant), outside: false, base: None };
        return crate::tools::guard_and_go(ed, For::Claude(call), run.job, false, extra);
    }
    let Some(scope) = scope else {
        return ed.set_status("nothing opened — the sandbox stays as it was");
    };
    match &grant {
        // (a host is the proxy's to let through — and the session's, not the conversation's)
        Grant::Host(h, p) => crate::hosts::granted(ed, h, *p, scope),
        _ => crate::allow::grant(ed, &grant, scope),
    }
    let head = format!(
        "The user ran `{}` again, letting it {}{}. How it ended:",
        run.label(),
        grant.shown(),
        if scope == Scope::Once { " this once" } else { "" }
    );
    rerun_of(ed, &run, &head);
    let extra = Extra { grant: (scope == Scope::Once).then_some(grant), outside: false, base: None };
    crate::tools::guard_and_go(ed, For::User(head), run.job, true, extra);
}

/// The user's run again of `run` is told with `head` as it ends (`tools::told`): which run it was, for whether it
/// goes on to Claude by itself.
fn rerun_of(ed: &mut Editor, run: &BlockedRun, head: &str) {
    if let (Some(key), Some(c)) = (run.key, ed.session.chat.as_mut()) {
        c.reruns.retain(|(h, _)| h != head);
        c.reruns.push((head.to_string(), key));
    }
}

/// What a card that runs Claude's command again says of how Claude hears its end (piece D3): by itself now, or
/// with the next message — by the state as the card is made; the end looks again (`tools::told`).
pub(crate) fn then_said(ed: &Editor, run: &BlockedRun) -> &'static str {
    match run.key.is_some_and(|k| crate::tools::goes_on(ed, k)) {
        true => GOES_ON,
        false => NEXT_MESSAGE,
    }
}

pub const GOES_ON: &str = "Claude goes on when it ends.";
pub const NEXT_MESSAGE: &str = "Claude hears how it ended with your next message.";

/// The answer on the run-outside card: `y` runs it now, outside the sandbox — the user's run.
pub fn outside_answer(ed: &mut Editor, id: u64, run: BlockedRun, yes: bool) {
    ed.offer_finished(id, Ok(()));
    if !yes {
        return ed.set_status("not run — the sandbox stays as it was");
    }
    let head = format!("The user ran `{}` again outside the sandbox, this once. How it ended:", run.label());
    rerun_of(ed, &run, &head);
    crate::tools::guard_and_go(
        ed,
        For::User(head),
        run.job,
        true,
        Extra { grant: None, outside: true, base: None },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Keys as the main loop takes them (`<ret>` by name).
    fn feed(ed: &mut Editor, keys: &str) {
        let mut chars = keys.chars();
        while let Some(c) = chars.next() {
            let key: crate::key::Key = match c {
                '<' => chars.by_ref().take_while(|&c| c != '>').collect::<String>().parse().unwrap(),
                c => crate::key::Key::plain(crate::key::Code::Char(c)),
            };
            ed.handle_key(key);
        }
    }

    /// 9f: where tarae's AppArmor profile would raise the tier, its step is a row of the panel — the list's one row
    /// when the run blocked nothing, which is still said where the first row would be — and Enter on it copies the
    /// whole step to the system clipboard, said with what to do next. Without a hint (Seatbelt) no such row: the
    /// list is empty and says why, as before.
    #[test]
    fn the_profiles_step_is_a_row_enter_copies() {
        use crate::sandbox::guarantees::{Found, Guarantees, Hint, Ns, Tier};
        let place = crate::sandbox::apparmor::Place {
            path: "/usr/local/bin/tarae".into(),
            root_owned: Ok(()),
            packaged: false,
        };
        let hint = Hint::of(Tier::C, &place);
        let step = hint.step.clone().expect("a root-owned plain path: a step");
        let mut ed = Editor::new(crate::config::Config::default());
        ed.session.sandbox = Some(Guarantees::landlock(Found {
            ns: Ns::NONE,
            served: Ok(()),
            copies: false,
            abi: 4,
            ptys: Some(4),
            tproxy: Err("no network namespace of its own".into()),
            lo_missing: Vec::new(),
            short: None,
            brief: Some("AppArmor blocks user namespaces".into()),
            hint: Some(hint),
            network: false,
        }));
        ed.sandbox_blocked(Which::Either);
        let p = ed.picker.as_ref().expect("the panel");
        let rows: Vec<(&str, &str)> = p.items().iter().map(|i| (i.label.as_str(), i.hint.as_str())).collect();
        assert_eq!(rows, [("copy the step that makes this tier A+", "needs sudo, then restart tarae")]);
        let head = p.head.as_ref().unwrap();
        assert_eq!(head.then.as_deref(), Some("tarae's AppArmor profile would make this A+"));
        assert_eq!((head.note.as_deref(), &p.empty), (Some(NOTHING_BLOCKED), &None), "said above the row");
        feed(&mut ed, "<ret>");
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let said = "copied the step that makes this tier A+ — run it in a terminal, then restart tarae";
        while !ed.toasts.iter().any(|t| t.text == said) {
            let left = end.saturating_duration_since(std::time::Instant::now());
            let ev = ed.events.recv_timeout(left).unwrap_or_else(|| panic!("{said:?} before the timeout"));
            ed.handle_event(ev);
        }
        assert!(crate::clipboard::written(&step), "the whole step copied: {step}");
        // (Seatbelt's: no tiers, no step — the list says the run blocked nothing in its first row)
        ed.picker = None;
        ed.session.sandbox = Some(Guarantees::seatbelt(false, false));
        ed.sandbox_blocked(Which::Either);
        let p = ed.picker.as_ref().expect("the panel");
        assert!(p.items().is_empty() && p.head.as_ref().is_some_and(|h| h.note.is_none()));
        assert_eq!(p.empty.as_deref(), Some(NOTHING_BLOCKED));
    }

    /// Review of #212: a run on a commit's tree isn't offered to grant or run again outside — its tree is gone
    /// (or another run's now), so either would run it where it no longer is; said instead.
    #[test]
    fn a_run_on_a_commits_tree_is_not_offered_again() {
        let mut ed = Editor::new(crate::config::Config::default());
        let tree = std::env::temp_dir().join(format!("tarae-blocked-base-{}", std::process::id()));
        let task =
            crate::task::Task { name: "cargo test".into(), cmd: "cargo test".into(), cwd: tree.clone() };
        let mut r = crate::task::Run::for_test(task);
        r.base = Some(crate::basetree::Base { rev: "HEAD".into(), tree, ..Default::default() });
        ed.session.task_run = Some(r);
        ed.sandbox_blocked(Which::Task);
        assert!(ed.picker.is_none());
        let said = ed.status.as_ref().map(|s| s.0.clone()).unwrap_or_default();
        assert!(said.starts_with("that run was on a commit's tree"), "{said}");
    }

    /// What a run's denials offer: the folders a write may open, a port that isn't a risky one — once each.
    #[test]
    fn what_a_runs_denials_offer() {
        let base = crate::scratch::Scratch::new("blocked");
        std::fs::create_dir_all(base.join("home/data/cache")).unwrap();
        std::fs::create_dir_all(base.join("home/proj")).unwrap();
        let home = std::fs::canonicalize(base.join("home")).unwrap();
        let root = home.join("proj");
        let d = |op: &str, target: String| Denial { process: "npm".into(), op: op.into(), target };
        let denials = [
            d("file-write-create", home.join("data/cache/x").display().to_string()),
            d("file-write-data", home.join("data/cache/y").display().to_string()),
            d("network-outbound", "remote:*:5432".into()),
            d("network-outbound", "remote:*:2375".into()),
            d("file-write-create", root.join(".git/index.lock").display().to_string()),
            d("mach-lookup", "com.apple.lsd.mapdb".into()),
            // (Linux's supervisor names the address: a port on this machine is offered, one beyond isn't)
            d("network-outbound", "remote:[::1]:8080".into()),
            d("network-outbound", "remote:192.0.2.1:9".into()),
        ];
        let all = choices(&denials, &crate::caps::Caps::at(&home, &root));
        assert_eq!((all[0].hint.as_str(), all[1].hint.as_str()), ("…/x", "…/cache/x"), "below the folder");
        assert_eq!(all[2].why, "npm was connecting to it");
        let got: Vec<Grant> = all.into_iter().map(|c| c.grant).collect();
        assert_eq!(
            got,
            [
                Grant::Write(home.join("data/cache")),
                Grant::Write(home.join("data")),
                Grant::Port(5432),
                Grant::Localhost,
                Grant::Port(8080)
            ],
            "Docker's port, an address beyond this machine and the project's .git aren't offered; each folder once \
             and every port here once, after the first port"
        );
    }

    /// A terminal refused (macOS, 3c — reading or writing `/dev/ttysN`) offers the terminals, once, whichever
    /// and however many: never `/dev` nor the file itself for writing. Its card says what yes risks — though
    /// not as much as outside, so a chat's `a` and the project's `A` take it — and the project keeps it
    /// (`sandbox.json`'s `terminals`) until `:sandbox-disallow terminals` takes it back.
    #[test]
    fn a_terminal_refused_offers_the_terminals_once() {
        let d = |op: &str, target: &str| Denial {
            process: "script".into(),
            op: op.into(),
            target: target.into(),
        };
        let denials = [
            d("file-write-data", "/dev/ttys012"),
            d("file-read-data", "/dev/ttys012"),
            d("file-read-data", "/dev/ttys003"),
            d("file-write-create", "/dev/ttysq"),
            // (not an open: no grant offered, nor `/dev` to write in)
            d("file-write-unlink", "/dev/ttys005"),
            d("file-write-mode", "/dev/ttys005"),
        ];
        let all = choices(&denials, &crate::caps::Caps::at(Path::new("/Users/u"), Path::new("/Users/u/p")));
        let got: Vec<(&Grant, &str)> = all.iter().map(|c| (&c.grant, c.why.as_str())).collect();
        assert_eq!(got, [(&Grant::Terminals, "script was opening /dev/ttys012")]);
        let g = Grant::Terminals;
        assert!(g.keepable() && !g.as_outside());
        assert!(g.risk().is_some_and(|r| r.contains("read what you type")), "{:?}", g.risk());
        let base = crate::scratch::Scratch::new("blocked-tty");
        let store = base.join("sandbox.json");
        crate::allow::keep(&store, Path::new("/p"), |a| a.add(&g)).unwrap();
        let kept = crate::allow::Store::read(Some(&store)).get(Path::new("/p"));
        assert!(kept.terminals && !kept.is_empty());
        let w = crate::tools::with_allowed(
            &crate::llm::LlmConfig::default(),
            &crate::caps::Caps::answered(&[&kept]),
            None,
        );
        assert!(w.terminals && w.granted == [g.shown()], "{:?}", w.granted);
        crate::allow::keep(&store, Path::new("/p"), |a| a.remove(crate::allow::TERMINALS_LISTED)).unwrap();
        assert_eq!(crate::allow::Store::read(Some(&store)).get(Path::new("/p")), Default::default());
    }

    /// A port refused here offers that port, then every port; one a pack's `refused-port` program picks as it
    /// starts (Gradle's `java`) only every port — the one it was refused is picked anew next time — and that card
    /// says the pack's words, as the card Claude's turn held does, ending with how Claude hears the end. Every
    /// port's card names the risky ones it opens too. A variable the output names offers it by name — never one no
    /// card passes, and its value is on no card.
    #[test]
    fn a_port_offers_every_port_too_and_a_variable_its_name_only() {
        let caps = crate::caps::Caps::at(Path::new("/Users/u"), Path::new("/Users/u/p"));
        let port = |process: &str, n: u16| Denial {
            process: process.into(),
            op: "network-outbound".into(),
            target: format!("remote:*:{n}"),
        };
        let grants = |d: Denial| choices(&[d], &caps).into_iter().map(|c| c.grant).collect::<Vec<_>>();
        assert_eq!(grants(port("psql", 5432)), [Grant::Port(5432), Grant::Localhost]);
        assert_eq!(grants(port("java", 52113)), [Grant::Localhost], "Gradle's: one port won't do");
        let gradle = BlockedRun {
            denials: vec![port("java", 52113)],
            ..BlockedRun::of(Job::Command("bin/gradle build".into(), PathBuf::from("/Users/u/p")))
        };
        let c = choices(&gradle.denials, &caps).remove(0);
        let card = grant_card(c.grant, &c.why, "bin/gradle build", Box::new(gradle.clone()), None, GOES_ON);
        let held = loopback_card(&crate::tools::loopback_of(&gradle.denials).unwrap(), "bin/gradle build", {
            Box::new(gradle)
        });
        let (q, h) = (&card.question, &held.question);
        assert!(q.contains("one port won't do") && !q.contains("connecting to it"), "{q}");
        let tail = "Claude's `bin/gradle build` failed for it: it runs again now with them open, whichever you choose. \
                    Claude goes on when it ends.";
        assert!(q.contains(tail), "{q}");
        assert_eq!(q.split("failed for it").next(), h.split("failed for it").next(), "the held card's words");
        let risky = Grant::Localhost.risk().unwrap();
        for (n, _) in crate::sandbox::RISKY_PORTS {
            assert!(risky.contains(&n.to_string()), "{n}: {risky}");
        }
        assert!(Grant::Localhost.keepable() && !Grant::Localhost.as_outside());
        let docker =
            Denial { process: "curl".into(), op: "network-outbound".into(), target: "remote:*:2375".into() };
        assert!(choices(&[docker], &caps).is_empty(), "a risky port: by hand only");
        // (a value tarae has, as a run's environment holds it — a fake token)
        let value = "tarae-fake-token-4f9a1c";
        let env = crate::sandbox::runenv::RunEnv {
            vars: Vec::new(),
            tokens: vec!["MY_API_TOKEN".into()],
            others: vec!["RUSTFLAGS".into(), "DATABASE_URL".into()],
        };
        let said = format!("MY_API_TOKEN={value} refused; RUSTFLAGS and DATABASE_URL unset");
        let named = env.named(&[(true, said)]);
        let list = env_choices(&named);
        let got: Vec<&Grant> = list.iter().map(|c| &c.grant).collect();
        assert_eq!(got, [&Grant::Env("MY_API_TOKEN".into()), &Grant::Env("DATABASE_URL".into())]);
        let run = BlockedRun {
            env: named,
            ..BlockedRun::of(Job::Command("psql".into(), PathBuf::from("/Users/u/p")))
        };
        let c = list[0].clone();
        let card = grant_card(c.grant, &c.why, "psql", Box::new(run), None, NEXT_MESSAGE);
        let text = card.question.clone();
        assert!(text.contains("MY_API_TOKEN") && !text.contains(value), "{text}");
        assert!(text.contains("s lasts until this session ends") && !text.contains("A until"), "{text}");
    }

    /// A file named like a key refused in the project (macOS, 7a) offers that one file, once — never a folder of
    /// it (the project is read anyway), never kept past this run (`Caps::storable`, cardauto 6); kept by an
    /// earlier build's `A`, it reaches the run (`with_allowed`) and `:sandbox-disallow` takes it back. One outside
    /// the project isn't this (home's rules: its folders).
    #[test]
    fn a_key_named_file_refused_offers_that_file() {
        let d = |target: &str| Denial {
            process: "node".into(),
            op: "file-read-data".into(),
            target: target.into(),
        };
        let (home, root) = (Path::new("/Users/u"), Path::new("/Users/u/p"));
        let env = root.join("app/.env");
        let all = choices(
            &[d("/Users/u/p/app/.env"), d("/Users/u/p/app/.env")],
            &crate::caps::Caps::at(home, root),
        );
        let got: Vec<(&Grant, &str, &str)> =
            all.iter().map(|c| (&c.grant, c.hint.as_str(), c.why.as_str())).collect();
        let g = Grant::Key(env.clone());
        let why = format!("node was reading {}, named like a key", crate::allow::tilde(&env));
        assert_eq!(got, [(&g, "node reading …/app/.env", why.as_str())]);
        let outside = choices(&[d("/Users/u/q/.env")], &crate::caps::Caps::at(home, root));
        assert!(outside.iter().all(|c| matches!(c.grant, Grant::Read(_))), "{outside:?}");
        assert!(!g.keepable() && !g.as_outside() && g.risk().is_some_and(|r| r.contains("reaches Claude")));
        let base = crate::scratch::Scratch::new("blocked-key");
        let store = base.join("sandbox.json");
        crate::allow::keep(&store, root, |a| a.add(&g)).unwrap();
        let kept = crate::allow::Store::read(Some(&store)).get(root);
        assert_eq!(kept.keys, std::slice::from_ref(&env));
        let w = crate::tools::with_allowed(
            &crate::llm::LlmConfig::default(),
            &crate::caps::Caps::answered(&[&kept]),
            None,
        );
        assert!(w.keys == [env.clone()] && w.granted == [g.shown()] && w.reads.is_empty(), "{:?}", w.granted);
        crate::allow::keep(&store, root, |a| a.remove(&env.display().to_string())).unwrap();
        assert_eq!(crate::allow::Store::read(Some(&store)).get(root), Default::default());
    }

    /// A host the proxy held and the user's answer would lift (`network-host`: not among the hosts, no answer,
    /// refused) is offered — that host at that port, once; what the proxy's other checks refused (an address
    /// never reached, a server name not the host's, `network-proxy`) never is: no grant makes those safe.
    #[test]
    fn a_host_refusal_offers_that_host_only() {
        let d =
            |op: &str, target: &str| Denial { process: "curl".into(), op: op.into(), target: target.into() };
        let host = crate::sandbox::proxy::HOST_OP;
        let denials = [
            d(host, "static.example.invalid:443: the user refused it for this session"),
            d(host, "static.example.invalid:443: 8 hosts already wait for the user's answer"),
            d(
                host,
                "api.example.invalid:8443: not among the hosts it may reach (llm.sandbox-hosts); the user was asked and didn't answer within 20 s",
            ),
            d(
                crate::sandbox::proxy::OP,
                "meta.example.invalid:443: it resolves only to addresses never reached: 169.254.169.254",
            ),
            d(
                crate::sandbox::proxy::OP,
                "sni.example.invalid:443: its ClientHello names other.example.invalid — the tunnel closed",
            ),
        ];
        let all = choices(&denials, &crate::caps::Caps::at(Path::new("/Users/u"), Path::new("/Users/u/p")));
        assert_eq!((all[0].hint.as_str(), all[0].why.as_str()), ("curl reaching", "curl was reaching it"));
        let got: Vec<Grant> = all.into_iter().map(|c| c.grant).collect();
        assert_eq!(
            got,
            [
                Grant::Host("static.example.invalid".into(), 443),
                Grant::Host("api.example.invalid".into(), 8443)
            ]
        );
    }

    /// A toolchain's shared cache is never offered for writing, in a project that isn't the tool's either (no
    /// `go.mod` here, so nothing asked Go where its module cache is): not Go's module cache at its default, nor
    /// cargo's registry, nor what holds them — and no folder the packs' `ask` could name there opens. Written
    /// only through the caches rule, with the network open. Nor npm's home (`npx` runs what's there). (A
    /// stand-in home in the test's temp folder.)
    #[test]
    fn a_shared_cache_is_never_offered_in_a_project_not_the_tools() {
        let base = crate::scratch::Scratch::new("blocked-cache");
        let modcache = "go/pkg/mod/golang.org/x/text@v0.1.0";
        for d in [
            modcache,
            "go/pkg/mod/cache/download",
            ".cargo/registry/src/i/serde-1.0.0",
            ".npm/_cacache",
            ".npm/_npx/0a1b",
            "data/cache",
            "proj",
        ] {
            std::fs::create_dir_all(base.join("home").join(d)).unwrap();
        }
        let home = std::fs::canonicalize(base.join("home")).unwrap();
        let root = home.join("proj");
        assert!(!root.join("go.mod").exists());
        let d = |target: PathBuf| Denial {
            process: "x".into(),
            op: "file-write-create".into(),
            target: target.display().to_string(),
        };
        let denials = [
            d(home.join(modcache).join("new.go")),
            d(home.join("go/pkg/mod/cache/download/x.zip")),
            d(home.join(".cargo/registry/src/i/serde-1.0.0/build.rs")),
            d(home.join(".npm/_cacache/x")),
            d(home.join(".npm/_npx/0a1b/x.js")),
            d(home.join("data/cache/x")),
        ];
        let got: Vec<Grant> =
            choices(&denials, &crate::caps::Caps::at(&home, &root)).into_iter().map(|c| c.grant).collect();
        assert_eq!(
            got,
            [Grant::Write(home.join("data/cache")), Grant::Write(home.join("data"))],
            "only the folder no toolchain keeps"
        );
        // …nor as a folder the toolchain names, this project's policy holding no write there
        let here = crate::sandbox::Here {
            env: [("HOME".to_string(), home.clone().into())].into(),
            ..Default::default()
        };
        let llm = crate::llm::LlmConfig::default();
        crate::sandbox::toolchain_found_in(&root, Default::default(), &here);
        let env = crate::sandbox::env_in(&root, &llm, &[], &here).0;
        for d in [home.join(modcache), home.join("go/pkg/mod"), home.join(".cargo/registry")] {
            let why = crate::sandbox::ask_dir_verdict(&d, &root, Some(&home), &env.runs_later).unwrap_err();
            assert!(why.contains("a toolchain's shared cache"), "{}: {why}", d.display());
        }
        let why =
            crate::sandbox::ask_dir_verdict(&home.join(".npm/_npx"), &root, Some(&home), &env.runs_later);
        assert!(why.as_ref().is_err_and(|w| w.starts_with("it's in a toolchain's home")), "{why:?}");
        let p = crate::sandbox::Policy::for_project(&env);
        assert!(!p.writes_allow(&home.join(modcache).join("new.go")), "{:?}", p.write);
    }

    /// Card model P2: a read the sandbox denied in home offers the places it may open, nearest first — never
    /// a key's; one only the output said (reading or writing — it doesn't say which) offers both.
    #[test]
    fn what_a_denied_read_offers() {
        let base = crate::scratch::Scratch::new("blocked-read");
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
        let all = choices(&denials, &crate::caps::Caps::at(&home, &home.join("proj")));
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
    }

    /// A read's grant card says first when the place holds a token-like line — by the file's name, never the
    /// value, in the warning colour; it comes once the place's been looked through (a worker's). A place
    /// without one says nothing of tokens. (Fake values in the test's own temp folder.)
    #[test]
    fn a_grants_card_says_when_a_token_is_there() {
        let scratch = crate::scratch::Scratch::new("blocked-tok");
        let base = scratch.to_path_buf();
        for (f, text) in [("conf/hosts.yml", "oauth_token: tarae-fake-token\n"), ("plain/x.txt", "marker\n")]
        {
            std::fs::create_dir_all(base.join(f).parent().unwrap()).unwrap();
            std::fs::write(base.join(f), text).unwrap();
        }
        let mut ed = Editor::new(crate::config::Config::default());
        let run = BlockedRun::of(Job::Command("cat x".into(), base.clone()));
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
        // (as the card shows it, `~` for home: the test's temp folder is in home in a sandboxed run)
        let said =
            format!("In {}, hosts.yml holds a token-like line.", crate::allow::tilde(&base.join("conf")));
        assert_eq!(warned, said, "{q}");
        assert!(!q.contains("tarae-fake-token"), "{q}");
        let (q, warned) = card(&mut ed, "1");
        assert!(warned.is_empty() && !q.contains("token"), "{q}");
    }

    /// cardauto 6: a grant's card offers each scope up to what its grant may be kept for (`Caps::storable`) —
    /// `s` (this session) on every one kept past the conversation; a socket the conversation's at most; Docker's
    /// socket, a file named like a key, home and a place holding keys this run's only; a host as its own card.
    #[test]
    fn a_grants_card_offers_each_scope_up_to_what_may_be_kept() {
        let scratch = crate::scratch::Scratch::new("blocked-scopes");
        let base = scratch.to_path_buf();
        let home = crate::allow::home().expect("a home");
        let card = |g: Grant| {
            let run = BlockedRun::of(Job::Command("true".into(), base.clone()));
            grant_card(g, "it was refused", "true", Box::new(run), None, NEXT_MESSAGE)
        };
        let all = "yasAn";
        for (g, keys) in [
            (Grant::Read(base.join("r")), all),
            (Grant::Write(base.join("w")), all),
            (Grant::Port(5432), all),
            (Grant::Terminals, all),
            (Grant::Host("example.invalid".into(), 443), "yasn"),
            (Grant::Socket(base.join("s.sock")), "yan"),
            (Grant::Socket("/run/docker.sock".into()), "yn"),
            (Grant::Key(base.join(".env")), "yn"),
            (Grant::Read(home.clone()), "yn"),
            (Grant::Read(home.join(".ssh")), "yn"),
        ] {
            let o = card(g.clone());
            let answers: String = o.answers(false).iter().map(|a| a.0).collect();
            assert_eq!(answers, keys, "{g:?}");
            // (what the keys take: each letter shown is taken, `s` among them)
            assert_eq!(o.lasts(), keys.contains('a'), "{g:?}");
            assert_eq!(o.for_session(), keys.contains('s'), "{g:?}");
            assert_eq!(o.keeps(), keys.contains('A'), "{g:?}");
        }
    }

    /// #119: a denied connect to a unix socket offers that socket — its real path, the file only — and not
    /// a path that isn't a socket, nor one in the project (that's open already); Docker's says what it is,
    /// and none is kept for good.
    #[test]
    fn a_denied_socket_offers_that_socket_only() {
        // (a socket's path has 104 bytes at most: short names — the temp folder is the home)
        let scratch = crate::scratch::Scratch::new("k");
        let base = scratch.to_path_buf();
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
        let got: Vec<Grant> =
            choices(&denials, &crate::caps::Caps::at(&home, &root)).into_iter().map(|c| c.grant).collect();
        assert_eq!(got, [Grant::Socket(sock.clone())], "the link's socket, once — nothing else");
        let g = Grant::Socket(sock);
        assert!(!g.keepable() && g.warning().is_some_and(|w| w.contains("mounts any folder")));
        assert!(Grant::Socket("/tmp/x/app.sock".into()).warning().is_none());
    }
}
