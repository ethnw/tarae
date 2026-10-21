//! `:granted` — what you've let through here (card model P4, cardauto piece 7; plan §2.2; sandbox UX piece E): the
//! defaults first, then your answers to Claude's asks by how long they last — always (kept for every checkout of the
//! repository, or this checkout's own: piece J), this session, this chat —, then the trust answers kept for every checkout of the repository (tarae's project
//! settings, Claude's settings, git's own commands, a hook), the plugins' approvals and the packs' preflight told —
//! each with where it applies and when it was answered. `d` takes an answer back: Claude's through
//! `Editor::sandbox_disallow` (the next run and the next read ask again, and Claude hears it with the next message);
//! a trust answer is forgotten (`trust::forget`), what it let run held back at once and its card asking at the next
//! look (`trust_taken`); a plugin's through `plugin_revoke`. A default isn't an answer to take back, and its row
//! says so. Answers the repository's other checkouts keep as their own are counted on a faint last row, not used
//! here. The
//! chat's title says how many of Claude's answers there are (`› 3 granted`: `count`).

use std::path::{Path, PathBuf};

use crate::allow::{Allowed, Grant};
use crate::caps::{Caps, Source, Why};
use crate::editor::Editor;
use crate::picker::{Action, Item, Picker};
use crate::trust::{Kind, Place};

/// The list's kind (`features::PICKERS`) and title.
pub const KIND: &str = "granted";
/// What its input row says, faint (nothing is typed: its letters are keys).
pub const SAID: &str = "what you've let through";
/// After the defaults, when there's no answer.
const NONE_PAST: &str = "nothing let through past the defaults yet";

/// What `d` takes back on a row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Taken {
    /// Claude's answer — as `:sandbox-disallow` takes it.
    Grant(String),
    /// A trust answer, kept at its place.
    Trust(Kind, Place),
    /// A plugin's approvals (`:plugin-revoke`).
    Plugin(String),
    /// The packs' preflight told: the lines kept for it (`preflight::told_kept`).
    Preflight(Vec<String>),
    /// The session's no-sandbox card's `s` (`Session.no_sandbox`).
    NoSandbox,
}

/// What the list shows past Claude's answers — read on a worker (`More::look`).
#[derive(Clone, Debug, Default)]
pub struct More {
    /// The session runs Claude's commands without a sandbox, unasked.
    pub no_sandbox: bool,
    /// Whether the checkout is a repository's (the trust answers are kept for all its checkouts).
    pub repository: bool,
    pub trusted: Vec<crate::trust::Trusted>,
    /// (plugin, what it may do).
    pub plugins: Vec<(String, String)>,
    /// The lines kept for the packs' preflight in this clone.
    pub preflight: Vec<String>,
    /// Answers the repository's other checkouts keep as their own, not used here.
    pub elsewhere: usize,
}

/// Where `More` is read from, taken on the main thread.
struct Look {
    root: PathBuf,
    no_sandbox: bool,
    trust: Option<PathBuf>,
    allow: Option<PathBuf>,
    plugins: Option<PathBuf>,
    /// The preflight told file, and the steps yours add (None: the project lists its own — no packs' defaults).
    preflight: Option<(PathBuf, Vec<crate::preflight::Step>)>,
}

impl More {
    /// A worker's: it reads the stores.
    fn look(l: &Look) -> More {
        let common = crate::allow::common_dir(&l.root);
        let trusted = crate::trust::Store::read(l.trust.as_deref()).trusted_in(common.as_deref(), &l.root);
        let plugins = l.plugins.as_deref().map(crate::plugin::approved).unwrap_or_default();
        let preflight = l.preflight.as_ref().map_or_else(Vec::new, |(kept, yours)| {
            let steps = [crate::packs::preflight(&l.root).steps, yours.clone()].concat();
            crate::preflight::told_kept(kept, &l.root, &steps)
        });
        let here = crate::allow::Where::of(&l.root);
        let elsewhere = crate::allow::Store::read(l.allow.as_deref()).elsewhere(&here);
        More {
            no_sandbox: l.no_sandbox,
            repository: common.is_some(),
            trusted,
            plugins,
            preflight,
            elsewhere,
        }
    }
}
/// A default's row, once `d` was pressed on it.
const NOT_YOURS: &str = "a default — no answer of yours to take back";

fn heading(label: &str) -> Item {
    Item { label: label.into(), action: Action::Heading, hint: String::new(), glyph: None }
}

/// What `g` lets, as its row says it.
fn label(g: &Grant) -> String {
    let t = crate::allow::tilde;
    match g {
        Grant::Read(p) => format!("read {}", t(p)),
        Grant::Write(p) => format!("write in {}", t(p)),
        Grant::Port(n) => format!("connect to port {n}"),
        Grant::Socket(s) => format!("connect to the socket {}", t(s)),
        Grant::Host(h, p) => format!("reach {h}:{p}"),
        Grant::Terminals => "open terminals, yours too".into(),
        Grant::Key(k) => format!("read {}, named like a key", t(k)),
        Grant::Localhost => "connect to every port here, the risky ones too".into(),
        Grant::Env(n) => format!("have your variable {n}"),
    }
}

/// What `:sandbox-disallow` takes to take `g` back.
pub(crate) fn taken_as(g: &Grant) -> String {
    match g {
        Grant::Read(p) | Grant::Write(p) | Grant::Key(p) => p.display().to_string(),
        Grant::Port(n) => format!("port {n}"),
        Grant::Socket(s) => format!("socket {}", s.display()),
        Grant::Host(h, p) => crate::hosts::listed(h, *p),
        Grant::Terminals => crate::allow::TERMINALS_LISTED.into(),
        Grant::Localhost => crate::allow::LOCALHOST_LISTED.into(),
        Grant::Env(n) => format!("env {n}"),
    }
}

/// Who answered and when, as an answer's hint ends.
fn when(at: Option<u64>, now: u64) -> String {
    match at.filter(|t| *t > 0) {
        Some(t) => format!("you · {}", crate::chats::ago(t, now)),
        None => "you".into(),
    }
}

/// A trust answer's group heading, before where it applies.
fn kind_heading(k: Kind) -> &'static str {
    match k {
        Kind::Tarae => "tarae's project settings",
        Kind::Claude => "Claude's settings",
        Kind::Git => "git's own commands",
        Kind::Hook => "git hooks",
    }
}

/// An answer's row: `d` takes it back.
fn answer(label: String, hint: String, taken: Taken) -> Item {
    Item { label, action: Action::Granted(Some(taken)), hint, glyph: Some(("•", "ui.virtual")) }
}

/// The list's rows: the defaults `caps` found (with why), then Claude's answers with their times — kept for the
/// checkout (`kept`), this session's, this chat's — each once, under the longest it lasts, newest first; the
/// session's hosts (the proxy's, until you quit) and its no-sandbox `s` with this session's; then `more`: the trust
/// answers by kind, the plugins, the preflight told, and what's kept for other checkouts.
pub fn items(
    caps: &Caps,
    [kept, session, chat]: [&Allowed; 3],
    hosts: &[(String, u16)],
    more: &More,
    now: u64,
) -> Vec<Item> {
    let mut out = vec![heading("by default")];
    let mut defaults: Vec<(Why, &Path)> = caps
        .read
        .iter()
        .filter_map(|e| match e.source {
            Source::Default(why) => Some((why, e.what.as_path())),
            Source::Grant(_) => None,
        })
        .collect();
    crate::sorting::by_key(&mut defaults, |(why, p)| (*why as u8, p.to_path_buf()));
    let default = |label: String, why: Why| Item {
        label,
        action: Action::Granted(None),
        hint: why.said().into(),
        glyph: Some(("◦", "ui.linenr")),
    };
    out.extend(defaults.iter().map(|(why, p)| default(format!("read {}", crate::allow::tilde(p)), *why)));
    if caps.outside {
        out.push(Item {
            hint: "not others' homes, volumes".into(),
            ..default("read outside home".into(), Why::Outside)
        });
    }
    // (kept: what every checkout of the repository uses, then the checkout's own — piece J)
    let (mut every, mut own) = (Allowed::default(), Allowed::default());
    for g in kept.grants() {
        let to = if caps.shared.contains(&g) { &mut every } else { &mut own };
        to.add(&g);
        if let Some(at) = kept.granted.get(&g) {
            to.granted.insert(g, *at);
        }
    }
    let mut seen: Vec<Grant> = Vec::new();
    let mut answered = 0;
    let groups = [(ALWAYS_SHARED, &every), (ALWAYS, &own), ("this session", session), ("this chat", chat)];
    for (title, a) in groups {
        let mut rows: Vec<(u64, Item)> = Vec::new();
        for g in a.grants() {
            if seen.contains(&g) {
                continue;
            }
            let at = a.granted.get(&g).copied();
            rows.push((at.unwrap_or(0), answer(label(&g), when(at, now), Taken::Grant(taken_as(&g)))));
            seen.push(g);
        }
        if title == "this session" {
            for (h, p) in hosts {
                let label = format!("reach {h}:{p}");
                let taken = Taken::Grant(crate::hosts::listed(h, *p));
                rows.push((0, answer(label, "you · until you quit".into(), taken)));
            }
            if more.no_sandbox {
                let label = NO_SANDBOX.into();
                rows.push((0, answer(label, "you · until you quit".into(), Taken::NoSandbox)));
            }
        }
        if rows.is_empty() {
            continue;
        }
        // (newest first; the same time in the order they were answered)
        crate::sorting::by(&mut rows, |a, b| b.0.cmp(&a.0));
        // (a row of air between groups)
        out.extend([heading(""), heading(title)]);
        answered += rows.len();
        out.extend(rows.into_iter().map(|(_, i)| i));
    }
    // The trust answers: for every checkout of the repository (outside one, the project's)
    let shared = if more.repository { "every checkout" } else { "this project" };
    let mut kind = None;
    for t in &more.trusted {
        if kind != Some(t.kind) {
            kind = Some(t.kind);
            out.extend([heading(""), heading(&format!("{} · {shared}", kind_heading(t.kind)))]);
        }
        let mut hint = when(Some(t.at), now);
        if t.versions > 1 {
            hint.push_str(&format!(" · {} versions", t.versions));
        }
        // (the file alone: its heading says what it runs)
        out.push(answer(t.place.shown(&caps.root), hint, Taken::Trust(t.kind, t.place.clone())));
        answered += 1;
    }
    if !more.plugins.is_empty() {
        out.extend([heading(""), heading("plugins · everywhere")]);
        for (name, what) in &more.plugins {
            out.push(answer(format!("{name}: {what}"), "you".into(), Taken::Plugin(name.clone())));
        }
        answered += more.plugins.len();
    }
    if !more.preflight.is_empty() {
        out.extend([heading(""), heading(&format!("preflight · {shared}"))]);
        out.push(answer(PREFLIGHT.into(), "you".into(), Taken::Preflight(more.preflight.clone())));
        answered += 1;
    }
    if answered == 0 {
        out.extend([heading(""), heading(NONE_PAST)]);
    }
    if more.elsewhere > 0 {
        let n = more.elsewhere;
        out.extend([heading(""), heading(&format!("{n} more kept for other checkouts — not used here"))]);
    }
    out
}

/// Claude's kept answers' headings: those every checkout of the repository uses (`sandbox.json` by its git common
/// folder), and the checkout's own (by its real path — a place in it, a key file).
const ALWAYS_SHARED: &str = "always · every checkout";
const ALWAYS: &str = "always · this checkout";
/// The session's no-sandbox `s`, as its row says it.
const NO_SANDBOX: &str = "run without a sandbox";
/// The packs' preflight told, as its row says it (the card said they run the project's code).
const PREFLIGHT: &str = "run the packs' default steps";

/// How many answers the list has — the chat's `› N granted`: what the session's last `Caps` has past the
/// defaults (each once), and its hosts.
pub fn count(s: &crate::session::Session) -> usize {
    let mut seen: Vec<Grant> = Vec::new();
    for g in s.caps().map(Caps::grants).unwrap_or_default() {
        if !seen.contains(&g) {
            seen.push(g);
        }
    }
    seen.len() + s.hosts.len()
}

impl Editor {
    /// `:granted` (`:sandbox-allowed`), the chat's `› N granted` — the list, made on a worker (it reads the kept
    /// answers and looks for the defaults).
    pub fn granted(&mut self) {
        self.granted_list(false);
    }

    /// The list made again: opened (`again`: only if it's still open — after a `d`).
    fn granted_list(&mut self, again: bool) {
        let basis = self.caps_basis();
        let hosts = self.session.hosts.clone();
        let look = Look {
            root: self.session.root.clone(),
            no_sandbox: self.session.no_sandbox == Some(crate::tools::NoSandbox::Unasked),
            trust: self.trust_path.clone(),
            allow: self.allow_path.clone(),
            plugins: self.plugin_approvals_store(),
            preflight: (self.config.preflight.is_empty())
                .then(|| {
                    self.session.preflight_kept.clone().map(|k| (k, self.config.preflight_local.clone()))
                })
                .flatten(),
        };
        self.events.jobs().spawn(move || {
            let caps = basis.build();
            let more = More::look(&look);
            let answers = [&basis.kept().1.all, &basis.session, &basis.chat];
            let items = items(&caps, answers, &hosts, &more, crate::allow::now_s());
            move |ed: &mut Editor| ed.granted_open(items, again)
        });
    }

    pub(crate) fn granted_open(&mut self, items: Vec<Item>, again: bool) {
        match self.picker.as_mut().filter(|p| p.kind == KIND) {
            Some(p) => {
                let (at, had) = (p.selected, p.current().map(|i| i.action.clone()));
                p.set_found(items);
                // (the row in hand taken back: the one that came into its place)
                if p.current().map(|i| &i.action) != had.as_ref() {
                    p.select_near(at);
                }
            }
            None if again => self.unchanged = true,
            None => {
                // (its kind written out: `features::every_picker_kind_is_listed` reads it from here)
                let mut p = Picker::of("granted", KIND, Vec::new(), false).without_preview();
                (p.keys, p.hints_first) = (true, true);
                p.set_found(items);
                // (on the first answer: what a key acts on)
                p.select_where(|a| matches!(a, Action::Granted(Some(_))));
                self.open_picker(p, None);
            }
        }
    }

    /// A letter in the list: the list panels' moves (`key::list_move`), `d` takes the answer in hand back — on a
    /// default, its row says why nothing goes.
    pub(crate) fn granted_key(&mut self, c: char) {
        let Some(p) = self.picker.as_mut() else { return };
        let key = crate::key::Key::plain(crate::key::Code::Char(c));
        if let Some(m) = crate::key::list_move(key) {
            return match m {
                crate::key::ListMove::By(by) => p.move_by(by),
                m => p.select_near(m.from(p.selected, p.len())),
            };
        }
        match c {
            'd' => match p.current().map(|i| i.action.clone()) {
                Some(Action::Granted(Some(taken))) => {
                    if let Err(e) = self.granted_take(taken) {
                        self.set_error(e);
                    }
                }
                Some(Action::Granted(None)) => p.set_current_hint(NOT_YOURS),
                _ => {}
            },
            _ => self.key_unused(key, "the list of grants", "d takes an answer back, j and k move"),
        }
    }

    /// `d` on an answer: taken back, the list made again (a store's write first, where there's one).
    fn granted_take(&mut self, taken: Taken) -> Result<(), String> {
        match taken {
            Taken::Grant(what) => self.sandbox_disallow(&what)?,
            Taken::Plugin(name) => self.plugin_revoke(&name)?,
            Taken::NoSandbox => {
                self.session.no_sandbox = None;
                self.set_status("Without a sandbox, each run asks again");
            }
            Taken::Preflight(keys) => {
                self.preflight_told.remove(&self.session.root);
                let Some(kept) = self.session.preflight_kept.clone() else { return Ok(()) };
                self.events.jobs().spawn(move || {
                    let r = crate::preflight::told_forget(&kept, &keys);
                    move |ed: &mut Editor| {
                        match r {
                            Ok(()) => ed.set_status("The packs' preflight asks again before it runs"),
                            Err(e) => ed.set_error(format!("couldn't take it back: {e}")),
                        }
                        ed.granted_list(true);
                    }
                });
                return Ok(());
            }
            Taken::Trust(kind, place) => {
                self.trust_take(kind, place);
                return Ok(());
            }
        }
        self.granted_list(true);
        Ok(())
    }

    /// A trust answer taken back: held back first, then forgotten in the store. From the `d` on the store reads as
    /// not trusting it (`trust::taking` — a reload, a resolve, a hook's next action), and what it let run follows
    /// before the write (on a worker: which places are it) — tarae's project settings held back, each chat whose
    /// claude runs with Claude's settings started again without them, tarae's git with the repository's own
    /// commands held (its answers and kept resolves dropped). A write that fails leaves it held back until you
    /// quit. Each card asks again at the next look — `:project-trust`, `:claude-settings`, `:git-trust` ask now.
    pub(crate) fn trust_take(&mut self, kind: Kind, place: Place) {
        let given = self.trust_taking_given(place.clone());
        // (a git resolve from now on reads it held, and one kept or under way isn't used: N8)
        if kind == Kind::Git {
            crate::gitrepo::trust_changed();
        }
        let store = self.trust_path.clone();
        let project = self.project_trust.as_ref().map(|p| p.file.clone());
        let chats: Vec<(crate::event::SessionId, PathBuf)> = std::iter::once(&self.session)
            .chain(&self.sessions)
            .filter_map(|s| Some((s.id, s.chat.as_ref()?.settings.seen.as_ref()?.file.clone())))
            .collect();
        // The store's write queued now, not from the result below: a quit right after the `d` waits for it (the
        // loop that would apply a result has ended), `:restart` gives it again. The mark holds the store's answer
        // back until its own write lands.
        match store {
            Some(s) => {
                let w = crate::trust::Write::Forget { kind, place: place.clone(), given };
                crate::trust::queue(self, s, vec![w], |ed, r| {
                    if let Err(e) = r {
                        ed.set_error(format!(
                            "couldn't take it out of the trust answers kept: {e} — held back until you quit"
                        ));
                    }
                });
            }
            None => crate::trust::taken(&place, given),
        }
        // What it let run held back, without waiting for the store (which places are it: on a worker)
        self.events.jobs().spawn(move || {
            if kind == Kind::Git {
                crate::gitrepo::forget_trust(&place);
            }
            let here = |f: &Path| crate::trust::place_of(f) == place;
            let project = kind == Kind::Tarae && project.is_some_and(|f| here(&f));
            let chats: Vec<_> = chats.into_iter().filter(|(_, f)| kind == Kind::Claude && here(f)).collect();
            move |ed: &mut Editor| {
                ed.trust_taken(kind, project, chats.into_iter().map(|(id, _)| id).collect());
                ed.granted_list(true);
            }
        });
    }

    /// The running state follows a trust answer taken back (`trust_take`): `project` — the open project's
    /// `.tarae.toml` was it; `chats` — the sessions whose chat read those Claude settings.
    fn trust_taken(&mut self, kind: Kind, project: bool, chats: Vec<crate::event::SessionId>) {
        if project {
            self.project_trust_forgotten();
        }
        for id in chats {
            self.with_session(id, crate::claudetrust::forgotten);
        }
        self.set_status(match kind {
            Kind::Tarae => {
                "Taken back — the project's settings are held back; its card asks at the next look"
            }
            Kind::Claude => "Taken back — Claude runs without the project's settings; their card asks again",
            Kind::Git => "Taken back — tarae's git holds this repository's commands; its card asks again",
            Kind::Hook => "Taken back — the hook is asked about before it runs again",
        });
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::allow::{Grant, Scope};
    use crate::key::{Code, Key};

    fn until(ed: &mut Editor, what: &str, done: impl Fn(&Editor) -> bool) {
        let end = Instant::now() + Duration::from_secs(10);
        while !done(ed) {
            assert!(Instant::now() < end, "timed out waiting for {what}");
            if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
                ed.handle_event(ev);
            }
        }
    }

    fn key(ed: &mut Editor, c: char) {
        ed.handle_event(crate::event::Event::Key(Key::plain(Code::Char(c))));
    }

    /// An editor on a checkout in `s`, with what's kept for it in a stand-in `sandbox.json` there (never the
    /// user's), and a chat.
    fn editor(s: &crate::scratch::Scratch) -> (Editor, PathBuf, PathBuf) {
        let root = s.path().join("p");
        std::fs::create_dir_all(&root).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let mut ed = Editor::new(crate::config::Config::default());
        ed.session.root = root.clone();
        ed.allow_path = Some(s.path().join("sandbox.json"));
        ed.session.chat = Some(crate::chat::Chat::new());
        (ed, root, s.path().join("sandbox.json"))
    }

    fn open(ed: &mut Editor) {
        ed.granted();
        until(ed, "the list", |ed| ed.picker.as_ref().is_some_and(|p| p.kind == KIND));
    }

    fn rows(ed: &Editor) -> Vec<(String, String)> {
        let p = ed.picker.as_ref().unwrap();
        p.items().iter().map(|i| (i.label.clone(), i.hint.clone())).collect()
    }

    /// The next run's policy, as `tools::guard_then_go` makes it: the session's answers as they are now.
    fn next_run(ed: &Editor) -> crate::tools::Widened {
        crate::tools::with_allowed(&ed.config.llm, &ed.caps_basis().granted(), None)
    }

    /// cardauto 7: `d` in `:granted` takes an answer back — kept, this session's, this chat's — and the next run's
    /// policy hasn't it: at once, the kept one too while its write to `sandbox.json` hasn't landed (held here), and
    /// after. Claude hears each with the next message.
    #[test]
    fn a_revoke_reaches_the_next_runs_policy() {
        let s = crate::scratch::Scratch::new("granted-revoke");
        let (mut ed, root, store) = editor(&s);
        let (kept, chat) = (s.path().join("kept"), s.path().join("chat"));
        let project = crate::sandbox::profile_path(&root);
        crate::allow::keep(&store, &project, |a| a.add_write(kept.clone())).unwrap();
        crate::allow::grant(&mut ed, &Grant::Port(5432), Scope::Session);
        crate::allow::grant(&mut ed, &Grant::Write(chat.clone()), Scope::ThisChat);
        let (shown, said) = (|p: &PathBuf| p.display().to_string(), |p: &PathBuf| crate::allow::tilde(p));
        let w = next_run(&ed);
        assert_eq!(w.llm.sandbox_write, [shown(&kept), shown(&chat)]);
        assert_eq!(w.llm.sandbox_ports, [5432]);
        // (what Claude is told names it as the session's last `Caps` has it)
        until(&mut ed, "the session's Caps", |ed| ed.session.caps().is_some_and(|c| c.write.len() == 2));
        open(&mut ed);
        let labels: Vec<String> = rows(&ed).into_iter().map(|r| r.0).collect();
        let at = |l: &str| labels.iter().position(|x| x == l).unwrap_or_else(|| panic!("{l} in {labels:?}"));
        assert!(at(ALWAYS) < at("this session") && at("this session") < at("this chat"), "{labels:?}");
        // The kept one, its write held back: the next run already hasn't it
        let pick = |ed: &mut Editor, what: String| {
            let p = ed.picker.as_mut().unwrap();
            let taken = Action::Granted(Some(Taken::Grant(what)));
            p.select_where(|a| *a == taken);
            assert_eq!(p.current().map(|i| &i.action), Some(&taken));
        };
        pick(&mut ed, shown(&kept));
        {
            // (the store's writes held a moment)
            crate::storewrite::hold(&store, Some(0));
            key(&mut ed, 'd');
            let written = !crate::allow::Store::read(Some(&store)).get(&project).write.contains(&kept);
            let next = next_run(&ed).llm.sandbox_write;
            crate::storewrite::hold(&store, None);
            assert!(!written, "not written yet");
            assert_eq!(next, [shown(&chat)], "taken back before the write lands");
        }
        until(&mut ed, "the write", |ed| ed.allow_taking.is_empty());
        assert!(crate::allow::Store::read(Some(&store)).get(&project).is_empty(), "and kept so");
        assert_eq!(next_run(&ed).llm.sandbox_write, [shown(&chat)]);
        // This session's, this chat's
        until(&mut ed, "the list again", |ed| !rows(ed).iter().any(|r| r.0.contains("kept")));
        pick(&mut ed, "port 5432".into());
        key(&mut ed, 'd');
        pick(&mut ed, shown(&chat));
        key(&mut ed, 'd');
        let w = next_run(&ed);
        assert!(w.llm.sandbox_write.is_empty() && w.llm.sandbox_ports.is_empty() && w.granted.is_empty());
        assert!(ed.session.allowed.is_empty(), "the session's kept so too");
        let told = ed.session.checkpoints.tell.join("\n");
        for gone in [
            format!("write in {}", said(&kept)),
            "connect to port 5432".into(),
            format!("write in {}", said(&chat)),
        ] {
            assert!(
                told.contains(&format!("took back what they had let you and your commands do: {gone}")),
                "{told}"
            );
        }
        // The list, once it's made again: the defaults, and nothing past them
        until(&mut ed, "the list again", |ed| rows(ed).iter().any(|r| r.0 == NONE_PAST));
        let labels: Vec<String> = rows(&ed).into_iter().map(|r| r.0).collect();
        assert_eq!(labels[..2], ["by default".to_string(), format!("read {}", said(&root))]);
        assert_eq!(labels.last().map(String::as_str), Some(NONE_PAST));
        assert!(
            !labels.iter().any(|l| [ALWAYS, "this session", "this chat"].contains(&l.as_str())),
            "{labels:?}"
        );
    }

    /// A variable and every port here (piece D): an `A` on a token-like name lasts this session — never in the
    /// project's kept answers —, any other name is kept; each reaches the next run's settings (the whole name,
    /// as one `llm.sandbox-env` entry) and `:sandbox-disallow` takes each back. Names only, never a value.
    #[test]
    fn a_token_like_variable_is_never_kept_past_the_session() {
        let s = crate::scratch::Scratch::new("granted-env");
        let (mut ed, _, store) = editor(&s);
        crate::allow::grant(&mut ed, &Grant::Env("GITHUB_TOKEN".into()), Scope::Always);
        crate::allow::grant(&mut ed, &Grant::Env("DATABASE_URL".into()), Scope::Always);
        crate::allow::grant(&mut ed, &Grant::Localhost, Scope::Always);
        // (an `A` is the chat's meanwhile, until it's kept)
        let always = Source::Grant(Scope::Always);
        until(&mut ed, "kept", |ed| {
            ed.session.caps().is_some_and(|c| {
                c.localhost.contains(&always)
                    && c.env.iter().any(|e| e.what == "DATABASE_URL" && e.source == always)
            })
        });
        let caps = ed.session.caps().unwrap().clone();
        let of = |n: &str| caps.env.iter().filter(|e| e.what == n).map(|e| e.source).collect::<Vec<_>>();
        assert!(!of("GITHUB_TOKEN").contains(&always), "{:?}", caps.env);
        assert!(of("GITHUB_TOKEN").contains(&Source::Grant(Scope::Session)), "{:?}", caps.env);
        let kept = std::fs::read_to_string(&store).unwrap();
        assert!(kept.contains("DATABASE_URL") && !kept.contains("GITHUB_TOKEN"), "{kept}");
        let w = next_run(&ed);
        assert!(w.llm.sandbox_localhost, "every port here");
        assert!(["GITHUB_TOKEN", "DATABASE_URL"].iter().all(|n| w.llm.sandbox_env.iter().any(|e| e == n)));
        open(&mut ed);
        let rows = rows(&ed);
        for l in ["have your variable GITHUB_TOKEN", "connect to every port here, the risky ones too"] {
            assert!(rows.iter().any(|r| r.0 == l), "{l} in {rows:?}");
        }
        ed.picker = None;
        ed.sandbox_disallow("env DATABASE_URL").unwrap();
        ed.sandbox_disallow("localhost").unwrap();
        let project = crate::sandbox::profile_path(&ed.session.root);
        until(&mut ed, "taken back", |ed| {
            let kept = crate::allow::Store::read(Some(&store)).get(&project);
            !kept.env.iter().any(|e| e == "DATABASE_URL")
                && !kept.localhost
                && ed.session.caps().is_some_and(|c| c.localhost.is_empty())
        });
        let w = next_run(&ed);
        assert!(!w.llm.sandbox_localhost && !w.llm.sandbox_env.iter().any(|e| e == "DATABASE_URL"));
    }

    /// The defaults come first, each saying why; the answers under the longest they last, each once (an `A` is
    /// the chat's too meanwhile), with who and when; no heading is ever the row in hand; `d` on a default takes
    /// nothing back and its row says why; another letter is said.
    #[test]
    fn the_list_says_where_each_comes_from_and_a_default_stays() {
        let s = crate::scratch::Scratch::new("granted-list");
        let (mut ed, root, _) = editor(&s);
        crate::allow::grant(&mut ed, &Grant::Write("/t/w".into()), Scope::Session);
        crate::allow::grant(&mut ed, &Grant::Port(8080), Scope::ThisChat);
        crate::allow::grant(&mut ed, &Grant::Terminals, Scope::Always);
        until(&mut ed, "kept", |ed| {
            ed.session.caps().is_some_and(|c| c.terminals.contains(&Source::Grant(Scope::Always)))
        });
        open(&mut ed);
        let rows = rows(&ed);
        let at = |l: &str| rows.iter().position(|r| r.0 == l).unwrap_or_else(|| panic!("{l} in {rows:?}"));
        let root_row = format!("read {}", crate::allow::tilde(&root));
        assert_eq!(rows[at(&root_row)].1, "this checkout");
        assert!(at("by default") < at(&root_row) && at(&root_row) < at(ALWAYS));
        let terminals = "open terminals, yours too";
        assert_eq!(rows[at(terminals)].1, "you · just now");
        assert_eq!(rows.iter().filter(|r| r.0 == terminals).count(), 1, "once, under always: {rows:?}");
        assert!(at(ALWAYS) < at(terminals) && at(terminals) < at("this session"));
        assert!(at("this session") < at("write in /t/w") && at("write in /t/w") < at("this chat"));
        assert!(at("this chat") < at("connect to port 8080"));
        assert_eq!(count(&ed.session), 3, "the chip's count: the answers, each once");
        // Never on a heading, every row in reach
        let p = ed.picker.as_mut().unwrap();
        // (the toolchains' found here are this machine's: counted as they come; a heading's hint is empty)
        let n = rows.iter().filter(|r| !r.1.is_empty()).count();
        assert_eq!(p.counts(), (n, n), "headings aren't counted");
        let mut visited = Vec::new();
        for _ in 0..rows.len() {
            p.move_by(1);
            let a = p.current().map(|i| i.action.clone()).unwrap();
            assert_ne!(a, Action::Heading);
            visited.push(p.current().unwrap().label.clone());
        }
        assert!(visited.contains(&root_row) && visited.contains(&"connect to port 8080".to_string()));
        // A default: nothing goes, its row says why
        p.select_where(|a| *a == Action::Granted(None));
        key(&mut ed, 'd');
        let p = ed.picker.as_ref().unwrap();
        assert_eq!(p.current().map(|i| i.hint.as_str()), Some(NOT_YOURS));
        assert_eq!(p.counts(), (n, n));
        assert_eq!(count(&ed.session), 3);
        key(&mut ed, 'x');
        let said = ed.toasts.last().map(|t| t.text.clone()).unwrap_or_default();
        assert!(said.contains("does nothing in the list of grants"), "{said}");
    }

    /// Piece E: a hook's kept yes, the packs' preflight told and the session's no-sandbox `s` are each listed —
    /// outside a repository, for this project — and `d` on each makes the next look ask: the hook's answer gone from
    /// the store, the told line gone (and since tarae started), the no-sandbox card's answer gone. Other letters move
    /// as in every list panel.
    #[test]
    fn a_hook_the_preflight_and_no_sandbox_are_listed_and_taken_back() {
        let s = crate::scratch::Scratch::new("granted-more");
        let (mut ed, root, _) = editor(&s);
        let trust = s.path().join("trust.json");
        ed.trust_path = Some(trust.clone());
        let hook = root.join(".githooks/pre-commit");
        crate::trust::record(trust.clone(), Kind::Hook, hook.clone(), [7; 32], true).unwrap();
        ed.session.no_sandbox = Some(crate::tools::NoSandbox::Unasked);
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"p\"\n").unwrap();
        let told = s.path().join("preflight-told");
        let steps = crate::packs::preflight(&root).steps;
        assert!(!steps.is_empty(), "the packs' steps for a Cargo project");
        let line = crate::preflight::told_key(&root, &steps).expect("a clone told apart");
        std::fs::write(&told, format!("other\n{line}\n")).unwrap();
        ed.session.preflight_kept = Some(told.clone());
        ed.preflight_told.insert(root.clone());
        open(&mut ed);
        let listed = rows(&ed);
        let at =
            |l: &str| listed.iter().position(|r| r.0 == l).unwrap_or_else(|| panic!("{l} in {listed:?}"));
        let no_sandbox = NO_SANDBOX;
        assert!(at("this session") < at(no_sandbox));
        assert!(at("git hooks · this project") < at(".githooks/pre-commit"));
        assert!(at("preflight · this project") < at(PREFLIGHT));
        assert!(listed[at(".githooks/pre-commit")].1.starts_with("you · "), "{listed:?}");
        assert!(!listed.iter().any(|r| r.0 == NONE_PAST));
        // `G` goes to the last row, `g` to the first answer-or-default
        key(&mut ed, 'G');
        let p = ed.picker.as_ref().unwrap();
        assert!(matches!(p.current().map(|i| &i.action), Some(Action::Granted(Some(Taken::Preflight(_))))));
        let take = |ed: &mut Editor, f: &dyn Fn(&Taken) -> bool| {
            let p = ed.picker.as_mut().unwrap();
            p.select_where(|a| matches!(a, Action::Granted(Some(t)) if f(t)));
            key(ed, 'd');
        };
        take(&mut ed, &|t| matches!(t, Taken::NoSandbox));
        assert_eq!(ed.session.no_sandbox, None, "the card asks before the next run");
        until(&mut ed, "the list again", |ed| !rows(ed).iter().any(|r| r.0 == no_sandbox));
        take(&mut ed, &|t| matches!(t, Taken::Trust(Kind::Hook, _)));
        // (the store written, not only the mark `Store::state` reads meanwhile)
        until(&mut ed, "the hook taken back", |_| {
            !crate::trust::is_taking(&crate::trust::place_of(&hook))
                && crate::trust::Store::read(Some(&trust)).state(&hook, [7; 32]) == crate::trust::State::Ask
        });
        take(&mut ed, &|t| matches!(t, Taken::Preflight(_)));
        until(&mut ed, "the told line gone", |_| std::fs::read_to_string(&told).unwrap() == "other\n");
        assert!(!ed.preflight_told.contains(&root), "nor kept since tarae started");
        until(&mut ed, "the list again", |ed| rows(ed).iter().any(|r| r.0 == NONE_PAST));
    }

    /// The schema step (plan §2.2): a kept read a default covers now — here, in the checkout — is dropped from
    /// `sandbox.json` and a toast names it; one no default covers stays.
    #[test]
    fn a_kept_read_a_default_covers_is_dropped_and_said() {
        let s = crate::scratch::Scratch::new("granted-schema");
        let (mut ed, root, store) = editor(&s);
        let (inside, outside) = (root.join("vendor"), PathBuf::from("/opt/tarae-stand-in"));
        let project = crate::sandbox::profile_path(&root);
        crate::allow::keep(&store, &project, |a| {
            a.add_read(inside.clone());
            a.add_read(outside.clone());
        })
        .unwrap();
        ed.caps_sync();
        until(&mut ed, "the step", |ed| ed.toasts.iter().any(|t| t.text.contains("No longer kept")));
        let said = ed.toasts.last().map(|t| t.text.clone()).unwrap_or_default();
        assert_eq!(
            said,
            format!("No longer kept — read by default now: {} (this checkout)", crate::allow::tilde(&inside))
        );
        assert_eq!(crate::allow::Store::read(Some(&store)).get(&project).read, [outside]);
    }

    /// Piece J: in a repository's checkout, what every checkout uses is listed under its own heading and the
    /// checkout's own under another; what another checkout keeps as its own is counted on the faint last row,
    /// not used here; `d` on a shared one takes it back for every checkout.
    #[test]
    fn a_shared_answer_is_listed_apart_and_taken_back_for_every_checkout() {
        let s = crate::scratch::Scratch::new("granted-repo");
        let (main, wt) = crate::allow::tests::repo_with_worktree(s.path());
        let store = s.path().join("sandbox.json");
        let outside = PathBuf::from("/opt/tarae-stand-in");
        crate::allow::keep(&store, &main, |a| {
            a.add_read(outside.clone());
            a.add(&Grant::Key(main.join(".env")));
        })
        .unwrap();
        crate::allow::keep(&store, &wt, |a| a.add(&Grant::Key(wt.join(".env")))).unwrap();
        let mut ed = Editor::new(crate::config::Config::default());
        (ed.session.root, ed.allow_path) = (main.clone(), Some(store.clone()));
        ed.session.chat = Some(crate::chat::Chat::new());
        open(&mut ed);
        let listed = rows(&ed);
        let at =
            |label: &str| listed.iter().position(|r| r.0 == label).unwrap_or_else(|| panic!("{listed:?}"));
        let (shared, own) = (at(ALWAYS_SHARED), at(ALWAYS));
        assert_eq!(at("read /opt/tarae-stand-in"), shared + 1, "{listed:?}");
        assert_eq!(
            at(&format!("read {}, named like a key", crate::allow::tilde(&main.join(".env")))),
            own + 1
        );
        assert_eq!(listed.last().unwrap().0, "1 more kept for other checkouts — not used here");
        let p = ed.picker.as_mut().unwrap();
        p.select_where(
            |a| matches!(a, Action::Granted(Some(Taken::Grant(w))) if w.contains("tarae-stand-in")),
        );
        key(&mut ed, 'd');
        until(&mut ed, "taken back for every checkout", |_| {
            crate::allow::Store::read(Some(&store)).get(&wt).read.is_empty()
        });
        assert!(crate::allow::Store::read(Some(&store)).get(&main).read.is_empty());
    }

    /// Review J 8: `d` in one session on an answer every checkout uses takes it back in the other session too — its
    /// conversation's copy (made as its `A` was answered) goes, its `Caps` no longer read it, and its Claude is told.
    #[test]
    fn d_takes_a_shared_answer_back_in_every_session() {
        let s = crate::scratch::Scratch::new("granted-every-session");
        let (main, wt) = crate::allow::tests::repo_with_worktree(s.path());
        let store = s.path().join("sandbox.json");
        let outside = PathBuf::from("/opt/tarae-stand-in");
        let mut ed = Editor::new(crate::config::Config::default());
        (ed.session.root, ed.allow_path) = (main.clone(), Some(store.clone()));
        ed.session.chat = Some(crate::chat::Chat::new());
        crate::allow::grant_reads(&mut ed, std::slice::from_ref(&outside), Scope::Always);
        until(&mut ed, "kept", |_| crate::allow::Store::read(Some(&store)).get(&wt).reads(&outside));
        let main_id = ed.session.id;
        let other = ed.session_add(wt.clone());
        assert!(ed.session_switch(other));
        ed.sandbox_disallow(&outside.display().to_string()).unwrap();
        until(&mut ed, "taken back", |_| !crate::allow::Store::read(Some(&store)).get(&main).reads(&outside));
        let m = ed.session_by_id(main_id).unwrap();
        assert!(!m.chat.as_ref().unwrap().allowed.reads(&outside), "its conversation's copy");
        assert!(m.checkpoints.tell.iter().any(|t| t.contains("took back") && t.contains("tarae-stand-in")));
        until(&mut ed, "its caps", |ed| {
            let m = ed.sessions.iter().find(|s| s.id == main_id).unwrap();
            m.caps().is_some_and(|c| !c.reads(&outside))
        });
    }

    /// The chip keeps its width whatever the count (`GRANTED_W` is its widest).
    #[test]
    fn the_chips_widest_is_its_width() {
        use unicode_width::UnicodeWidthStr;
        assert_eq!(" › 99+ granted".width(), crate::term::GRANTED_W);
    }
}
