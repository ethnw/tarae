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
    /// Outside home, where it's read by default (`Caps.outside`, D3) — never an entry of `read`: what
    /// `Caps::default_for` says of a place there.
    Outside,
}

impl Why {
    /// As `:granted` and its toasts say it.
    pub fn said(self) -> &'static str {
        match self {
            Why::Checkout => "this checkout",
            Why::OtherCheckout => "another checkout",
            Why::Temp => "the temp folders",
            Why::Toolchain => "a toolchain's own",
            Why::Outside => "outside home",
        }
    }

    // (tests' for now: `:granted` will say it — piece 7)
    #[cfg(test)]
    pub fn word(self) -> &'static str {
        match self {
            Why::Checkout => "checkout",
            Why::OtherCheckout => "other-checkout",
            Why::Temp => "temp",
            Why::Toolchain => "toolchain",
            Why::Outside => "outside",
        }
    }
}

/// Where an entry comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Default(Why),
    /// The user's answer: kept for the project (`Always`), this session's (`Session`), or this conversation's
    /// (`ThisChat`).
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

/// What `A` keeps a checkout's own answer for, as the card and the toast say it (piece J).
const THIS_CHECKOUT: &str = "this checkout";

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
    /// Every port on this machine (`Grant::Localhost`) — each answer that opened them.
    pub localhost: Vec<Source>,
    /// The user's variables passed whole, by name (`Grant::Env`).
    pub env: Vec<Entry<String>>,
    /// Outside home is read by default, as runs read it (D3) — but others' homes and volumes (`cards::class`).
    /// Off in tests (`Editor.read_outside`).
    pub outside: bool,
    /// What runs never read, wherever it is (`sandbox::kept_places`): never a default, never a read's choice.
    pub kept: Vec<PathBuf>,
    /// Where a write would run something later (`sandbox::RunsLater`): never a write's choice.
    pub later: crate::sandbox::RunsLater,
    /// Where its kept answers are (`allow::Where::of` the root — the store's own): the repository's git common
    /// folder when the checkout is one's, its checkouts — what the cards and toasts say `A` is for, by the store's
    /// rule (review J 6, 10, 12).
    pub here: crate::allow::Where,
    /// The kept answers every checkout of the repository uses (the rest kept are this checkout's own).
    pub shared: Vec<Grant>,
}

impl Caps {
    /// The checkout `root` and the answers: kept for the project, this session's, then this conversation's — no
    /// default found beyond the checkout (a run's policy has its own: `tools::with_allowed` takes the answers
    /// only).
    pub fn granted(root: &Path, kept: &Allowed, session: &Allowed, chat: &Allowed) -> Caps {
        let mut c = Caps { root: root.to_path_buf(), ..Caps::default() };
        if !root.as_os_str().is_empty() {
            c.read.push(Entry { what: root.to_path_buf(), source: Source::Default(Why::Checkout) });
        }
        c.add_answers(kept, Source::Grant(Scope::Always));
        c.add_answers(session, Source::Grant(Scope::Session));
        c.add_answers(chat, Source::Grant(Scope::ThisChat));
        c
    }

    /// What `A` on a card about `grants` keeps them for, in a repository's checkout (piece J): every checkout of it
    /// when none names a place in or holding one (`allow::Where::shared`), else this checkout — None outside a
    /// repository (there it's the project's, as "Always here" says).
    pub fn always_said(&self, grants: &[Grant]) -> Option<String> {
        let at = self.always_for(grants)?;
        Some(match at.as_str() {
            THIS_CHECKOUT => format!("A is for {at} only."),
            _ => format!("A is for {at}."),
        })
    }

    /// Who an answer kept about `grants` is for, as the card and the toast say it: "every checkout of tarae" or
    /// "this checkout" — None outside a repository.
    pub fn always_for(&self, grants: &[Grant]) -> Option<String> {
        let (here, common) = (&self.here, self.here.repository.as_ref()?);
        // (the main checkout's folder: a common folder is `<main>/.git`, a bare one `<name>.git`)
        let name = match common.file_name().is_some_and(|n| n == ".git") {
            true => common.parent().and_then(Path::file_name),
            false => common.file_stem(),
        };
        let name = name.map_or_else(|| "this repository".into(), |n| n.to_string_lossy().into_owned());
        Some(match grants.iter().all(|g| here.shared(g)) {
            true => format!("every checkout of {name}"),
            false => THIS_CHECKOUT.into(),
        })
    }

    /// The longest an answer about `g` may last (plan §2.3) — the top scope its card offers, and what an answer
    /// is cut to. Once only: a place that holds keys, home itself or above it (that's everything), a file named
    /// like a key, Docker's socket (as much as the sandbox off — #193). This conversation at most: another
    /// socket (a server's socket is the server). The rest may be kept for the project.
    pub fn storable(g: &Grant) -> Scope {
        Caps::storable_in(g, crate::allow::home().as_deref())
    }

    /// `storable` for home `home`.
    pub fn storable_in(g: &Grant, home: Option<&Path>) -> Scope {
        match g {
            Grant::Read(p) if !crate::allow::keepable(p, home) => Scope::Once,
            Grant::Key(_) => Scope::Once,
            Grant::Socket(_) if g.as_outside() => Scope::Once,
            Grant::Socket(_) => Scope::ThisChat,
            // (piece C, cards.md decision 4: the network is grantable up to this session at most — always is
            // `llm.sandbox-network`, with a host list's `A` on the proxy's own card)
            Grant::Host(..) => Scope::Session,
            // (a name for a secret: never written to the project's kept answers — its value is a token's)
            Grant::Env(n) if crate::tokens::secret_name(n) => Scope::Session,
            Grant::Read(_)
            | Grant::Write(_)
            | Grant::Port(_)
            | Grant::Terminals
            | Grant::Localhost
            | Grant::Env(_) => Scope::Always,
        }
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
        if a.localhost {
            self.localhost.push(source);
        }
        self.env.extend(entries(&a.env, source));
    }

    /// The answers, as a run's policy is widened by them (`tools::with_allowed`): the kept ones first, then this
    /// session's, then this conversation's, each kind in `Allowed`'s order.
    pub fn grants(&self) -> Vec<Grant> {
        let mut out = Vec::new();
        for s in [Scope::Always, Scope::Session, Scope::ThisChat].map(Source::Grant) {
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
            out.extend(self.localhost.contains(&s).then_some(Grant::Localhost));
            out.extend(self.env.iter().filter(|e| of(e.source)).map(|e| Grant::Env(e.what.clone())));
        }
        out
    }

    /// `grants` as a run's policy takes them: the network closed (`network_open` false), no host kept only for the
    /// project — those are a host list's, with `llm.sandbox-network` on (piece C, cards.md decision 4).
    pub fn run_grants(&self, network_open: bool) -> Vec<Grant> {
        let kept_only = |h: &str, p: u16| {
            let of = self.hosts.iter().filter(|e| e.what.0 == h && e.what.1 == p);
            !network_open && of.into_iter().all(|e| e.source == Source::Grant(Scope::Always))
        };
        let out = self.grants().into_iter();
        out.filter(|g| !matches!(g, Grant::Host(h, p) if kept_only(h, *p))).collect()
    }

    /// Whether `real` is read unasked: in the checkout, a place read by default, or one answered.
    pub fn reads(&self, real: &Path) -> bool {
        self.read.iter().any(|e| real.starts_with(&e.what))
    }

    /// Whether the file named like a key at `real` was opened.
    pub fn opens_key(&self, real: &Path) -> bool {
        self.keys.iter().any(|e| e.what == real)
    }

    /// Why `real` is read by default, if it is: a default place holding it, else outside home where that's read
    /// (but others' homes and volumes).
    pub fn default_for(&self, real: &Path) -> Option<Why> {
        let held = self.read.iter().find_map(|e| match e.source {
            Source::Default(why) if real.starts_with(&e.what) => Some(why),
            _ => None,
        });
        held.or_else(|| (self.outside && self.class(Some(real)) == Class::Elsewhere).then_some(Why::Outside))
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
            ..Caps::granted(root, &Allowed::default(), &Allowed::default(), &Allowed::default())
        }
    }

    /// (Tests: the answers in `scopes` — the first kept for the project, the second this conversation's, the
    /// third this session's.)
    #[cfg(test)]
    pub fn answered(scopes: &[&Allowed]) -> Caps {
        let none = Allowed::default();
        let at = |i: usize| scopes.get(i).copied().unwrap_or(&none);
        Caps::granted(Path::new(""), at(0), at(2), at(1))
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
    /// This session's answers.
    pub session: Allowed,
    /// This conversation's answers.
    pub chat: Allowed,
    /// The temp folders read by default (`Editor.read_temp`).
    pub temp: bool,
    /// Outside home read by default (`Editor.read_outside`).
    pub outside: bool,
    /// Kept answers taken back, their write not landed yet (`Editor.allow_taking`) — left out of what's read.
    pub taking: Vec<String>,
    /// The hosts this session's `s` let through (`Session.hosts`, the proxy's): what a run's rules carry with the
    /// answers (`tools::with_allowed`) — so a registry's caches open under them (piece C).
    pub hosts: Vec<(String, u16)>,
    /// (Tests: home in place of the process's — `Editor.caps_home`.)
    #[cfg(test)]
    pub home: Option<PathBuf>,
}

impl Basis {
    /// The answers only — the kept ones read from the store, the checkout's and its repository's. A worker's.
    pub fn granted(&self) -> Caps {
        let (here, kept) = self.kept();
        let mut c = Caps::granted(&self.root, &kept.all, &self.session, &self.chat);
        (c.here, c.shared) = (here, kept.shared);
        c
    }

    /// The answers kept for the project (`allow::Where`), but those being taken back. A worker's: it reads the
    /// store and git's files.
    pub fn kept(&self) -> (crate::allow::Where, crate::allow::Kept) {
        let here = crate::allow::Where::of(&self.root);
        let mut kept = crate::allow::Store::read(self.store.as_deref()).kept(&here);
        self.taking.iter().for_each(|w| kept.all.remove(w));
        kept.shared.retain(|g| kept.all.grants().contains(g));
        (here, kept)
    }

    /// The schema step (plan §2.2): what `caps` (made from this) keeps for the project that a default reads now
    /// — an old answer the defaults caught up with — taken out of the store, each with why. A worker's: it
    /// writes the store (only when there's one to take out).
    pub fn drop_covered(&self, caps: &Caps) -> Vec<(PathBuf, Why)> {
        let Some(path) = self.store.as_deref() else { return Vec::new() };
        let covered: Vec<(PathBuf, Why)> = caps
            .read
            .iter()
            .filter(|e| e.source == Source::Grant(Scope::Always))
            // (never one every checkout uses: a default here — a build folder, another checkout — may be none
            // there, review J 4)
            .filter(|e| !caps.shared.contains(&Grant::Read(e.what.clone())))
            .filter_map(|e| Some((e.what.clone(), caps.default_for(&e.what)?)))
            .collect();
        if covered.is_empty() {
            return covered;
        }
        let gone: Vec<String> = covered.iter().map(|(p, _)| p.display().to_string()).collect();
        match crate::allow::keep(path, &self.root, |a| gone.iter().for_each(|w| a.remove(w))) {
            Ok(()) => covered,
            Err(e) => {
                crate::log::line("sandbox", "warning", &format!("couldn't drop what a default covers: {e}"));
                Vec::new()
            }
        }
    }

    /// The whole value, for home and the variables as the process has them. A worker's: it reads the store and
    /// looks for the defaults.
    pub fn build(&self) -> Caps {
        #[cfg(test)]
        if let Some(home) = &self.home {
            return self.build_in(Some(home), &crate::sandbox::Here::now());
        }
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
            session: self.session.allowed.clone(),
            chat: self.session.chat.as_ref().map(|c| c.allowed.clone()).unwrap_or_default(),
            temp: self.read_temp,
            outside: self.read_outside,
            taking: self.allow_taking.clone(),
            hosts: self.session.granted_hosts(),
            #[cfg(test)]
            home: self.caps_home.clone(),
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
        let taking = self.allow_taking.clone();
        #[cfg(test)]
        let home = self.caps_home.clone();
        let Some(s) = self.session_by_id(id) else { return };
        s.caps_asked += 1;
        let generation = s.caps_asked;
        let chat = s.chat.as_ref().map(|c| c.allowed.clone()).unwrap_or_default();
        let (root, session) = (s.root.clone(), s.allowed.clone());
        let hosts = s.granted_hosts();
        let basis = Basis {
            root,
            store,
            session,
            chat,
            temp,
            outside,
            taking,
            hosts,
            #[cfg(test)]
            home,
        };
        self.events.shared_jobs().spawn(move || {
            let caps = Arc::new(basis.build());
            let dropped = basis.drop_covered(&caps);
            move |ed: &mut crate::editor::Editor| {
                caps_landed(ed, id, generation, caps);
                if !dropped.is_empty() {
                    let said: Vec<String> = dropped
                        .iter()
                        .map(|(p, why)| format!("{} ({})", crate::allow::tilde(p), why.said()))
                        .collect();
                    ed.set_status(format!("No longer kept — read by default now: {}", said.join(", ")));
                    ed.unchanged = false;
                }
            }
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
    // (on screen only as the chat's `› N granted`)
    let shown = crate::granted::count(&ed.session);
    if let Some(s) = ed.session_by_id(id)
        && s.caps.as_ref().is_none_or(|(had, _)| *had < generation)
    {
        s.caps = Some((generation, caps));
    }
    ed.unchanged = crate::granted::count(&ed.session) == shown;
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
        let mut caps = Caps::granted(Path::new("/p"), &kept, &Allowed::default(), &chat);
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

    /// Review J 4: what a default covers here is dropped from this checkout's own answers only — one every checkout
    /// uses stays (the default — a build folder, another checkout — may be none in the others).
    #[test]
    fn a_shared_answer_a_default_covers_here_stays() {
        let s = crate::scratch::Scratch::new("caps-drop-shared");
        let (main, wt) = crate::allow::tests::repo_with_worktree(s.path());
        let store = s.path().join("sandbox.json");
        let place = std::fs::canonicalize(s.path()).unwrap().join("deps/x");
        crate::allow::keep(&store, &main, |a| a.add_read(place.clone())).unwrap();
        let basis = Basis { root: wt.clone(), store: Some(store.clone()), ..Default::default() };
        let mut caps = basis.granted();
        assert!(caps.shared.contains(&Grant::Read(place.clone())));
        let covering = place.parent().unwrap().to_path_buf();
        caps.read.push(Entry { what: covering, source: Source::Default(Why::Toolchain) });
        assert_eq!(basis.drop_covered(&caps), []);
        assert_eq!(crate::allow::Store::read(Some(&store)).get(&main).read, [place]);
    }

    /// Review J 6, 10, 12, 13: what the card and the toast say `A` is for goes by the store's own rule — here for a
    /// session started in a subfolder of its checkout: a place in the checkout, in another checkout, a key file,
    /// a folder holding the checkouts or a list with one of them is this checkout's; a place outside them all is
    /// every checkout's — and `keep` files it there.
    #[test]
    fn what_a_is_for_is_the_stores_rule() {
        let s = crate::scratch::Scratch::new("caps-always-for");
        let (main, wt) = crate::allow::tests::repo_with_worktree(s.path());
        let base = std::fs::canonicalize(s.path()).unwrap();
        let sub = main.join("crates/core");
        std::fs::create_dir_all(&sub).unwrap();
        let store = base.join("sandbox.json");
        let caps = Basis { root: sub.clone(), store: Some(store.clone()), ..Default::default() }.granted();
        let (only, every) = ("A is for this checkout only.", "A is for every checkout of main.");
        let outside = Grant::Read(base.join("outside"));
        for (grants, said) in [
            (vec![Grant::Read(main.join("README.md"))], only),
            (vec![Grant::Read(wt.join("x"))], only),
            (vec![Grant::Write(wt.join("out"))], only),
            (vec![Grant::Key(sub.join(".env"))], only),
            (vec![Grant::Read(base.clone())], only),
            (vec![outside.clone(), Grant::Read(main.join("README.md"))], only),
            (vec![outside.clone()], every),
            (vec![Grant::Port(5432)], every),
        ] {
            assert_eq!(caps.always_said(&grants).as_deref(), Some(said), "{grants:?}");
        }
        crate::allow::keep(&store, &sub, |a| a.add_read(main.join("README.md"))).unwrap();
        let kept = |p: &Path| crate::allow::Store::read(Some(&store)).get(p).read;
        assert!(kept(&wt).is_empty() && kept(&sub) == [main.join("README.md")]);
        // Outside a repository: nothing said ("Always here" is the project's)
        let plain = base.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let caps = Basis { root: plain, ..Default::default() }.granted();
        assert_eq!(caps.always_said(std::slice::from_ref(&outside)), None);
    }

    /// No checkout, nothing read by being in it: an empty path holds every path.
    #[test]
    fn no_checkout_reads_nothing() {
        let none = Allowed::default();
        let caps = Caps::granted(Path::new(""), &none, &none, &none);
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

    /// cardauto 6 (plan §2.3): the longest each grant may be kept — once only for a place holding keys, home
    /// or above it, a file named like a key, Docker's socket; this conversation for another socket; the rest
    /// for the project.
    #[test]
    fn what_may_be_kept_and_for_how_long() {
        let home = Path::new("/h");
        let s = |g: Grant| Caps::storable_in(&g, Some(home));
        for (g, top) in [
            (Grant::Read("/h/code/x".into()), Scope::Always),
            (Grant::Read("/opt/x".into()), Scope::Always),
            (Grant::Read("/h".into()), Scope::Once),
            (Grant::Read("/".into()), Scope::Once),
            (Grant::Read("/h/.ssh/config".into()), Scope::Once),
            (Grant::Read("/h/.config/gh".into()), Scope::Once),
            (Grant::Key("/p/.env".into()), Scope::Once),
            (Grant::Socket("/run/docker.sock".into()), Scope::Once),
            (Grant::Socket("/p/.tarae/x.sock".into()), Scope::ThisChat),
            (Grant::Write("/h/w".into()), Scope::Always),
            (Grant::Port(5432), Scope::Always),
            // (piece C, cards.md decision 4: always is `llm.sandbox-network`)
            (Grant::Host("example.invalid".into(), 443), Scope::Session),
            (Grant::Terminals, Scope::Always),
            (Grant::Localhost, Scope::Always),
            (Grant::Env("DATABASE_URL".into()), Scope::Always),
            // (a name for a secret: this session at most, whatever was answered)
            (Grant::Env("GITHUB_TOKEN".into()), Scope::Session),
            (Grant::Env("DB_PASSWORD".into()), Scope::Session),
            (Grant::Env("NPM_CONFIG__AUTH".into()), Scope::Session),
            (Grant::Env("GIT_AUTHOR_NAME".into()), Scope::Always),
        ] {
            assert_eq!(s(g.clone()), top, "{g:?}");
        }
        assert!(
            Scope::Once < Scope::ThisChat
                && Scope::ThisChat < Scope::Session
                && Scope::Session < Scope::Always
        );
    }
}
