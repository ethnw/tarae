//! What a session's Claude may do (cardauto piece 3, plan §1.1) — one value, `Caps`: the places its reads and its
//! commands read unasked, the folders its commands write in past the sandbox's own, the ports, unix sockets, hosts,
//! terminals and files named like keys open to them — each with where it comes from: a default (`Why`) or the
//! user's answer (`Source::Grant`, with how long it lasts). Built on a worker from what the main thread took
//! (`Basis`): the answers kept for the project read from `$state/sandbox.json`, this conversation's, the defaults
//! found (`allow::defaults_in`). Every gate goes by it: a run's policy (`tools::with_allowed`), Claude's reads
//! (`reads::covered`), the capability model's rule (`cards::decide`), what the sandbox blocked may open
//! (`blocked::choices`). The session keeps its last (`Session.caps`, `Editor::caps_sync`) for what reads it on the
//! main thread.
//!
//! Never in it, whatever is answered: what runs never read (`kept` — keys, tokens, Claude's and tarae's state) and
//! where a write would run something later (`later`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::allow::{Allowed, Grant, Scope};
use crate::cards::Class;

/// Why a place is read by default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// The checkout itself.
    Checkout,
    /// Another checkout of the repository (`allow::checkouts`).
    OtherCheckout,
    /// The temp folders (`Editor.read_temp`).
    Temp,
    /// The toolchains' sources, the crates cargo downloaded, the project's build folders outside it.
    Toolchain,
}

impl Why {
    // (tests' for now: `:granted` will say it — piece 7)
    #[cfg(test)]
    pub fn word(self) -> &'static str {
        match self {
            Why::Checkout => "checkout",
            Why::OtherCheckout => "other-checkout",
            Why::Temp => "temp",
            Why::Toolchain => "toolchain",
        }
    }
}

/// Where an entry comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Default(Why),
    /// The user's answer: kept for the project (`Always`), or this conversation's (`ThisChat`).
    Grant(Scope),
}

impl Source {
    #[cfg(test)]
    fn said(self) -> String {
        match self {
            Source::Default(why) => format!("default: {}", why.word()),
            Source::Grant(Scope::Always) => "always".into(),
            Source::Grant(Scope::ThisChat) => "this chat".into(),
            Source::Grant(Scope::Session) => "this session".into(),
            Source::Grant(Scope::Once) => "once".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry<T> {
    pub what: T,
    pub source: Source,
}

fn entries<T: Clone>(v: &[T], source: Source) -> impl Iterator<Item = Entry<T>> + '_ {
    v.iter().map(move |what| Entry { what: what.clone(), source })
}

/// What a session's Claude may do. Real paths.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Caps {
    /// The checkout (empty: none — nothing is read by being in it).
    pub root: PathBuf,
    pub home: Option<PathBuf>,
    /// Files, and folders with everything in them, read unasked — Claude's Read, and its commands where the
    /// sandbox confines reads (home).
    pub read: Vec<Entry<PathBuf>>,
    /// Folders its commands write in besides the sandbox's own.
    pub write: Vec<Entry<PathBuf>>,
    /// Ports on this machine.
    pub ports: Vec<Entry<u16>>,
    /// Unix sockets, each one file.
    pub sockets: Vec<Entry<PathBuf>>,
    /// Hosts reached through tarae's proxy, each at its port.
    pub hosts: Vec<Entry<(String, u16)>>,
    /// macOS: the terminals (`Grant::Terminals`) — each answer that opened them.
    pub terminals: Vec<Source>,
    /// Files named like keys inside the project, each that file.
    pub keys: Vec<Entry<PathBuf>>,
    /// Outside home is read by default, as runs read it (D3) — but others' homes and volumes (`cards::class`).
    /// Off in tests (`Editor.read_outside`).
    pub outside: bool,
    /// What runs never read, wherever it is (`sandbox::kept_places`): never a default, never a read's choice.
    pub kept: Vec<PathBuf>,
    /// Where a write would run something later (`sandbox::RunsLater`): never a write's choice.
    pub later: crate::sandbox::RunsLater,
}

impl Caps {
    /// The checkout `root` and the answers: kept for the project, then this conversation's — no default found
    /// beyond the checkout (a run's policy has its own: `tools::with_allowed` takes the answers only).
    pub fn granted(root: &Path, kept: &Allowed, chat: &Allowed) -> Caps {
        let mut c = Caps { root: root.to_path_buf(), ..Caps::default() };
        if !root.as_os_str().is_empty() {
            c.read.push(Entry { what: root.to_path_buf(), source: Source::Default(Why::Checkout) });
        }
        c.add_answers(kept, Source::Grant(Scope::Always));
        c.add_answers(chat, Source::Grant(Scope::ThisChat));
        c
    }

    /// The defaults found (`allow::defaults_in`), and what's never one.
    fn found(&mut self, d: crate::allow::Defaults) {
        self.read.extend(entries(&d.checkouts, Source::Default(Why::OtherCheckout)));
        self.read.extend(entries(&d.temp, Source::Default(Why::Temp)));
        self.read.extend(entries(&d.code, Source::Default(Why::Toolchain)));
        self.kept = d.kept;
    }

    /// (Tests: the defaults `d` for home `home`, outside home read too.)
    #[cfg(test)]
    pub fn of_defaults(d: crate::allow::Defaults, home: Option<&Path>) -> Caps {
        let mut c = Caps { home: home.map(Path::to_path_buf), outside: true, ..Caps::default() };
        c.found(d);
        c
    }

    fn add_answers(&mut self, a: &Allowed, source: Source) {
        self.read.extend(entries(&a.read, source));
        self.write.extend(entries(&a.write, source));
        self.ports.extend(entries(&a.ports, source));
        self.sockets.extend(entries(&a.sockets, source));
        self.hosts.extend(entries(&a.hosts, source));
        if a.terminals {
            self.terminals.push(source);
        }
        self.keys.extend(entries(&a.keys, source));
    }

    /// The answers, as a run's policy is widened by them (`tools::with_allowed`): the kept ones first, then this
    /// conversation's, each kind in `Allowed`'s order.
    pub fn grants(&self) -> Vec<Grant> {
        let mut out = Vec::new();
        for s in [Source::Grant(Scope::Always), Source::Grant(Scope::ThisChat)] {
            let of = |source: Source| source == s;
            out.extend(self.read.iter().filter(|e| of(e.source)).map(|e| Grant::Read(e.what.clone())));
            out.extend(self.write.iter().filter(|e| of(e.source)).map(|e| Grant::Write(e.what.clone())));
            out.extend(self.ports.iter().filter(|e| of(e.source)).map(|e| Grant::Port(e.what)));
            out.extend(self.sockets.iter().filter(|e| of(e.source)).map(|e| Grant::Socket(e.what.clone())));
            out.extend(
                self.hosts.iter().filter(|e| of(e.source)).map(|e| Grant::Host(e.what.0.clone(), e.what.1)),
            );
            out.extend(self.terminals.contains(&s).then_some(Grant::Terminals));
            out.extend(self.keys.iter().filter(|e| of(e.source)).map(|e| Grant::Key(e.what.clone())));
        }
        out
    }

    /// Whether `real` is read unasked: in the checkout, a place read by default, or one answered.
    pub fn reads(&self, real: &Path) -> bool {
        self.read.iter().any(|e| real.starts_with(&e.what))
    }

    /// Whether the file named like a key at `real` was opened.
    pub fn opens_key(&self, real: &Path) -> bool {
        self.keys.iter().any(|e| e.what == real)
    }

    /// The places read by default for `why`.
    pub fn defaults(&self, why: Why) -> impl Iterator<Item = &Path> {
        self.read.iter().filter(move |e| e.source == Source::Default(why)).map(|e| e.what.as_path())
    }

    /// Whether a place of class `c` is read without asking (`cards::decide`'s rule): another checkout, the temp
    /// folders, the toolchains' — and outside home where it's read by default (`outside`, D3).
    pub fn by_default(&self, c: Class) -> bool {
        match c {
            Class::OtherCheckout | Class::Temp | Class::Toolchain => true,
            Class::Elsewhere => self.outside,
            Class::Home | Class::OtherHome | Class::Volume | Class::Secret | Class::Unknown => false,
        }
    }

    /// Where `at` (real; None: can't be told) is, as the capability model sorts places (`cards::class`).
    pub fn class(&self, at: Option<&Path>) -> Class {
        crate::cards::class(at, self)
    }

    /// (Tests: home and the checkout, and what's never in it there as the process's variables say.)
    #[cfg(test)]
    pub fn at(home: &Path, root: &Path) -> Caps {
        let here = crate::sandbox::Here::now();
        let vars = crate::sandbox::vars_in(home, &here);
        Caps {
            kept: crate::sandbox::kept_places(home, &here),
            later: crate::sandbox::RunsLater::new(home, &vars, &[]),
            home: Some(home.to_path_buf()),
            ..Caps::granted(root, &Allowed::default(), &Allowed::default())
        }
    }

    /// (Tests: the answers in `scopes` — the first kept for the project, the second this conversation's.)
    #[cfg(test)]
    pub fn answered(scopes: &[&Allowed]) -> Caps {
        let none = Allowed::default();
        let at = |i: usize| scopes.get(i).copied().unwrap_or(&none);
        Caps::granted(Path::new(""), at(0), at(1))
    }

    /// All of it, a line each, each path as `shown` says it, sorted within each part — `src/snapshots/caps.txt`
    /// (`sandbox::golden`).
    #[cfg(test)]
    pub fn said(&self, shown: impl Fn(&Path) -> String) -> String {
        fn part(out: &mut String, title: &str, mut rows: Vec<String>) {
            rows.sort();
            out.push_str(&format!("{title}\n"));
            rows.iter().for_each(|r| out.push_str(&format!("    {r}\n")));
        }
        let row = |what: String, s: Source| format!("{what} · {}", s.said());
        let mut out = format!("outside home read: {}\n", if self.outside { "yes" } else { "no" });
        part(&mut out, "read", self.read.iter().map(|e| row(shown(&e.what), e.source)).collect());
        part(&mut out, "write", self.write.iter().map(|e| row(shown(&e.what), e.source)).collect());
        part(&mut out, "ports", self.ports.iter().map(|e| row(e.what.to_string(), e.source)).collect());
        part(&mut out, "sockets", self.sockets.iter().map(|e| row(shown(&e.what), e.source)).collect());
        let hosts = self.hosts.iter().map(|e| row(format!("{}:{}", e.what.0, e.what.1), e.source));
        part(&mut out, "hosts", hosts.collect());
        part(&mut out, "terminals", self.terminals.iter().map(|s| s.said()).collect());
        part(&mut out, "keys", self.keys.iter().map(|e| row(shown(&e.what), e.source)).collect());
        part(&mut out, "never read", self.kept.iter().map(|p| shown(p)).collect());
        let later = &self.later;
        part(&mut out, "never written: programs", later.bins.iter().map(|p| shown(p)).collect());
        part(&mut out, "never written: shared caches", later.caches.iter().map(|c| shown(&c.at)).collect());
        part(&mut out, "never written: toolchains' homes", later.homes.iter().map(|p| shown(p)).collect());
        out
    }
}

/// What a `Caps` is made from, taken on the main thread (`Editor::caps_basis`).
#[derive(Clone, Debug, Default)]
pub struct Basis {
    /// The checkout (a commit's tree for a run on one).
    pub root: PathBuf,
    /// `$state/sandbox.json` (None: nothing kept — tests).
    pub store: Option<PathBuf>,
    /// This conversation's answers.
    pub chat: Allowed,
    /// The temp folders read by default (`Editor.read_temp`).
    pub temp: bool,
    /// Outside home read by default (`Editor.read_outside`).
    pub outside: bool,
}

impl Basis {
    /// The answers only — the kept ones read from the store, by the checkout's real path. A worker's.
    pub fn granted(&self) -> Caps {
        let kept =
            crate::allow::Store::read(self.store.as_deref()).get(&crate::sandbox::profile_path(&self.root));
        Caps::granted(&self.root, &kept, &self.chat)
    }

    /// The whole value, for home and the variables as the process has them. A worker's: it reads the store and
    /// looks for the defaults.
    pub fn build(&self) -> Caps {
        self.build_in(crate::allow::home().as_deref(), &crate::sandbox::Here::now())
    }

    /// `build` for `home` and what `here` says of the variables (a stand-in's: `sandbox::golden`).
    pub fn build_in(&self, home: Option<&Path>, here: &crate::sandbox::Here) -> Caps {
        let mut c = self.granted();
        c.found(crate::allow::defaults_in(&self.root, self.temp, home, here));
        c.later = home
            .map(|h| {
                let vars = crate::sandbox::vars_in(h, here);
                crate::sandbox::RunsLater::new(h, &vars, &crate::sandbox::path_dirs_in(here))
            })
            .unwrap_or_default();
        c.home = home.map(Path::to_path_buf);
        c.outside = self.outside;
        c
    }
}

impl crate::editor::Editor {
    /// What this session's `Caps` is made from now.
    pub fn caps_basis(&self) -> Basis {
        Basis {
            root: self.session.root.clone(),
            store: self.allow_path.clone(),
            chat: self.session.chat.as_ref().map(|c| c.allowed.clone()).unwrap_or_default(),
            temp: self.read_temp,
            outside: self.read_outside,
        }
    }

    /// The session in front's `Caps` made again — a chat opened, an answer given or taken back.
    pub fn caps_sync(&mut self) {
        self.caps_sync_of(self.session.id);
    }

    /// Session `id`'s `Caps` made again on a worker, as its next generation: it lands in that session wherever it
    /// is then (in front or behind; gone: dropped), and only over an older one — two out of order keep the newer.
    pub fn caps_sync_of(&mut self, id: crate::event::SessionId) {
        let (store, temp, outside) = (self.allow_path.clone(), self.read_temp, self.read_outside);
        let Some(s) = self.session_by_id(id) else { return };
        s.caps_asked += 1;
        let generation = s.caps_asked;
        let chat = s.chat.as_ref().map(|c| c.allowed.clone()).unwrap_or_default();
        let basis = Basis { root: s.root.clone(), store, chat, temp, outside };
        self.events.shared_jobs().spawn(move || {
            let caps = Arc::new(basis.build());
            move |ed: &mut crate::editor::Editor| caps_landed(ed, id, generation, caps)
        });
    }
}

/// A `Caps` made for session `id` as `generation`: kept unless one asked later is there already.
fn caps_landed(
    ed: &mut crate::editor::Editor,
    id: crate::event::SessionId,
    generation: u64,
    caps: Arc<Caps>,
) {
    // (nothing on screen reads it yet: `:granted` will)
    ed.unchanged = true;
    if let Some(s) = ed.session_by_id(id)
        && s.caps.as_ref().is_none_or(|(had, _)| *had < generation)
    {
        s.caps = Some((generation, caps));
    }
}

impl crate::session::Session {
    /// Its `Caps` as last made — None before the first lands.
    pub fn caps(&self) -> Option<&Caps> {
        self.caps.as_ref().map(|(_, c)| c.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The answers widen a run's policy as they did before there was a `Caps`: the kept ones first, each kind in
    /// `Allowed`'s order — the defaults never (the policy has its own).
    #[test]
    fn the_answers_come_kept_first_and_no_default_among_them() {
        let kept = Allowed { write: vec!["/w/k".into()], ports: vec![9], ..Allowed::default() };
        let chat = Allowed {
            read: vec!["/r/c".into()],
            terminals: true,
            keys: vec!["/p/.env".into()],
            ..Allowed::default()
        };
        let mut caps = Caps::granted(Path::new("/p"), &kept, &chat);
        caps.read.push(Entry { what: "/t".into(), source: Source::Default(Why::Temp) });
        assert_eq!(
            caps.grants(),
            [
                Grant::Write("/w/k".into()),
                Grant::Port(9),
                Grant::Read("/r/c".into()),
                Grant::Terminals,
                Grant::Key("/p/.env".into())
            ]
        );
        assert!(
            caps.reads(Path::new("/p/src/a.rs"))
                && caps.reads(Path::new("/t/x"))
                && caps.reads(Path::new("/r/c"))
        );
        assert!(
            !caps.reads(Path::new("/pq/a.rs")) && !caps.reads(Path::new("/w/k/x")),
            "a write isn't a read"
        );
        assert!(caps.opens_key(Path::new("/p/.env")) && !caps.opens_key(Path::new("/p/.env.local")));
        assert_eq!(caps.defaults(Why::Temp).collect::<Vec<_>>(), [Path::new("/t")]);
    }

    /// Review of piece 3: a `Caps` lands in the session it was made for, wherever that is when it lands — one
    /// asked in front, then switched away from, is the session behind's — and only over an older one: two
    /// landing out of order keep the newer.
    #[test]
    fn a_caps_lands_in_its_own_session_and_the_newer_stays() {
        let mut ed = crate::editor::Editor::new(crate::config::Config::default());
        let (first, first_root) = (ed.session.id, ed.session.root.clone());
        let scratch = crate::scratch::Scratch::new("caps-sessions");
        let other = std::fs::canonicalize(&scratch).unwrap();
        // (made behind: its own asked as it's made)
        let second = ed.session_add(other.clone());
        ed.caps_sync();
        assert!(ed.session_switch(second));
        let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let made = |ed: &mut crate::editor::Editor, id| ed.session_by_id(id).and_then(|s| s.caps.clone());
        while made(&mut ed, first).is_none() || made(&mut ed, second).is_none() {
            let left = end.saturating_duration_since(std::time::Instant::now());
            let ev = ed.events.recv_timeout(left).expect("both made");
            ed.handle_event(ev);
        }
        let root = |ed: &mut crate::editor::Editor, id| made(ed, id).map(|(_, c)| c.root.clone()).unwrap();
        assert_eq!((root(&mut ed, first), root(&mut ed, second)), (first_root, other.clone()));
        // Two out of order
        let asked = ed.session_by_id(first).unwrap().caps_asked;
        let at = |r: &str| Arc::new(Caps { root: r.into(), ..Caps::default() });
        caps_landed(&mut ed, first, asked + 2, at("/newer"));
        caps_landed(&mut ed, first, asked + 1, at("/older"));
        assert_eq!(root(&mut ed, first), Path::new("/newer"));
    }

    /// No checkout, nothing read by being in it: an empty path holds every path.
    #[test]
    fn no_checkout_reads_nothing() {
        let caps = Caps::granted(Path::new(""), &Allowed::default(), &Allowed::default());
        assert!(!caps.reads(Path::new("/etc/hosts")) && !Caps::default().reads(Path::new("/etc/hosts")));
    }

    /// Outside home is read by default only where the session reads it so (D3; off in tests) — the rest of the
    /// classes as `cards::decide` has them.
    #[test]
    fn outside_home_is_a_default_only_when_on() {
        let on = Caps { outside: true, ..Caps::default() };
        assert!(on.by_default(Class::Elsewhere) && !Caps::default().by_default(Class::Elsewhere));
        for c in [Class::Home, Class::OtherHome, Class::Volume, Class::Secret, Class::Unknown] {
            assert!(!on.by_default(c), "{c:?}");
        }
        for c in [Class::OtherCheckout, Class::Temp, Class::Toolchain] {
            assert!(Caps::default().by_default(c), "{c:?}");
        }
    }
}
