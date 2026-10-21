//! `:granted` — what Claude may do here without asking (card model P4, cardauto piece 7; plan §2.2): the defaults
//! first, then the user's answers by how long they last — always (kept for the project), this session, this chat —
//! each with where it comes from and when it was answered. `d` takes an answer back (`Editor::sandbox_disallow`:
//! the next run and the next read ask again, and Claude hears it with the next message); a default isn't an answer
//! to take back, and its row says so. The chat's title says how many answers there are (`› 3 granted`: `count`).

use std::path::Path;

use crate::allow::{Allowed, Grant};
use crate::caps::{Caps, Source, Why};
use crate::editor::Editor;
use crate::picker::{Action, Item, Picker};
use crate::sandbox::proxy::Kind;

/// The list's kind (`features::PICKERS`) and title.
pub const KIND: &str = "granted";
/// What its input row says, faint (nothing is typed: its letters are keys).
pub const SAID: &str = "what Claude may do without asking";
/// After the defaults, when there's no answer.
const NONE_PAST: &str = "nothing granted past the defaults yet";
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
fn taken_as(g: &Grant) -> String {
    match g {
        Grant::Read(p) | Grant::Write(p) | Grant::Key(p) => p.display().to_string(),
        Grant::Port(n) => format!("port {n}"),
        Grant::Socket(s) => format!("socket {}", s.display()),
        Grant::Host(h, p) => crate::hosts::listed(Kind::Host, h, *p),
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

/// The list's rows: the defaults `caps` found (with why), then the answers with their times — kept for the project
/// (`kept`), this session's, this chat's — each once, under the longest it lasts, newest first; the session's hosts
/// (the proxy's, until you quit) with this session's.
pub fn items(
    caps: &Caps,
    [kept, session, chat]: [&Allowed; 3],
    hosts: &[(Kind, String, u16)],
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
    let mut seen: Vec<Grant> = Vec::new();
    let mut answered = 0;
    for (title, a) in [("always", kept), ("this session", session), ("this chat", chat)] {
        let mut rows: Vec<(u64, Item)> = Vec::new();
        for g in a.grants() {
            if seen.contains(&g) {
                continue;
            }
            let at = a.granted.get(&g).copied();
            let (label, hint, action) = (label(&g), when(at, now), Action::Granted(Some(taken_as(&g))));
            rows.push((at.unwrap_or(0), Item { label, action, hint, glyph: Some(("•", "ui.virtual")) }));
            seen.push(g);
        }
        if title == "this session" {
            for (kind, h, p) in hosts {
                let label = match kind {
                    Kind::Host => format!("reach {h}:{p}"),
                    Kind::Raw => format!("raw TCP to {h}:{p}"),
                    Kind::Lookup => format!("look up {h}'s records"),
                };
                let action = Action::Granted(Some(crate::hosts::listed(*kind, h, *p)));
                let hint = "you · until you quit".into();
                rows.push((0, Item { label, action, hint, glyph: Some(("•", "ui.virtual")) }));
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
    if answered == 0 {
        out.extend([heading(""), heading(NONE_PAST)]);
    }
    out
}

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
        self.events.jobs().spawn(move || {
            let caps = basis.build();
            let items =
                items(&caps, [&basis.kept(), &basis.session, &basis.chat], &hosts, crate::allow::now_s());
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

    /// A letter in the list: `j`/`k` move, `d` takes the answer in hand back — on a default, its row says why
    /// nothing goes.
    pub(crate) fn granted_key(&mut self, c: char) {
        let Some(p) = self.picker.as_mut() else { return };
        match c {
            'j' => p.move_by(1),
            'k' => p.move_by(-1),
            'd' => match p.current().map(|i| i.action.clone()) {
                Some(Action::Granted(Some(what))) => {
                    if let Err(e) = self.sandbox_disallow(&what) {
                        return self.set_error(e);
                    }
                    self.granted_list(true);
                }
                Some(Action::Granted(None)) => p.set_current_hint(NOT_YOURS),
                _ => {}
            },
            _ => self.key_unused(
                crate::key::Key::plain(crate::key::Code::Char(c)),
                "the list of grants",
                "d takes an answer back, j and k move",
            ),
        }
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
        assert!(at("always") < at("this session") && at("this session") < at("this chat"), "{labels:?}");
        // The kept one, its write held back: the next run already hasn't it
        let pick = |ed: &mut Editor, what: String| {
            let p = ed.picker.as_mut().unwrap();
            p.select_where(|a| *a == Action::Granted(Some(what.clone())));
            assert_eq!(p.current().map(|i| &i.action), Some(&Action::Granted(Some(what))));
        };
        pick(&mut ed, shown(&kept));
        {
            let _held = crate::allow::KEEP.lock().unwrap();
            key(&mut ed, 'd');
            assert!(
                crate::allow::Store::read(Some(&store)).get(&project).write.contains(&kept),
                "not written yet"
            );
            assert_eq!(next_run(&ed).llm.sandbox_write, [shown(&chat)], "taken back before the write lands");
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
            !labels.iter().any(|l| ["always", "this session", "this chat"].contains(&l.as_str())),
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
        until(&mut ed, "taken back", |ed| {
            let kept = std::fs::read_to_string(&store).unwrap_or_default();
            !kept.contains("DATABASE_URL")
                && !kept.contains("localhost")
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
        assert!(at("by default") < at(&root_row) && at(&root_row) < at("always"));
        let terminals = "open terminals, yours too";
        assert_eq!(rows[at(terminals)].1, "you · just now");
        assert_eq!(rows.iter().filter(|r| r.0 == terminals).count(), 1, "once, under always: {rows:?}");
        assert!(at("always") < at(terminals) && at(terminals) < at("this session"));
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

    /// The chip keeps its width whatever the count (`GRANTED_W` is its widest).
    #[test]
    fn the_chips_widest_is_its_width() {
        use unicode_width::UnicodeWidthStr;
        assert_eq!(" › 99+ granted".width(), crate::term::GRANTED_W);
    }
}
