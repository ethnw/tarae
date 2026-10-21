//! One card for a repository's trust (sandbox UX piece I): when more than one of a checkout's three waits —
//! tarae's own settings (`trust.rs`), its Claude settings (`claudetrust.rs`), its git's own commands
//! (`gittrust.rs`) — they're one card, each part under a heading with the lines its own card shows. `y` trusts
//! each, `n` holds each back, `v` shows each whole.
//!
//! Nothing waits for it: each part is still found on its own worker when it's found, and its card goes up at
//! once (the chat starts held back beside it, git runs with its commands held); one found while another of the
//! checkout's waits unanswered joins that card, which settles again before a `y` counts (`rev`), saying
//! faintly which part came. Each answer is kept in its part's own store as its own card keeps it, so a later
//! change asks about that part alone, on its own card, word for word as ever. A part asked alone (`:project-trust`,
//! `:claude-settings`, `:git-trust`) or no longer asking leaves the card; one left is its own card again. A git
//! value too long to show is never trusted: `y` holds git's part back, as its own card's only answer does.

use std::path::Path;

use crate::editor::Editor;
use crate::offer::{Offer, OfferState, What};

pub const TITLE: &str = "Trust this repository's settings?";

/// A part's name on the card, and the command that asks about it alone.
fn named(w: &What) -> Option<(&'static str, &'static str)> {
    match w {
        What::ProjectTrust { .. } => Some(("tarae", ":project-trust")),
        What::ClaudeSettings { .. } => Some(("Claude", ":claude-settings")),
        What::GitTrust(_) => Some(("git", ":git-trust")),
        _ => None,
    }
}

/// Where a part stands on the card: tarae's, Claude's, git's.
fn order(o: &Offer) -> u8 {
    match o.what {
        What::ProjectTrust { .. } => 0,
        What::ClaudeSettings { .. } => 1,
        _ => 2,
    }
}

/// The same part — a later read of it takes its place: its kind; git's, its repository.
fn same(a: &What, b: &What) -> bool {
    match (a, b) {
        (What::GitTrust(x), What::GitTrust(y)) => x.config == y.config,
        _ => std::mem::discriminant(a) == std::mem::discriminant(b),
    }
}

/// Whether a part is about the checkout at `root` (a session's, its real path): tarae's file in it or above,
/// Claude's its session's own, git's the repository it's in — a submodule's stays a card of its own.
fn of_checkout(ed: &Editor, root: &Path, w: &What) -> bool {
    match w {
        What::ProjectTrust { .. } => {
            ed.project_trust.as_ref().and_then(|p| p.file.parent()).is_some_and(|d| root.starts_with(d))
        }
        What::ClaudeSettings { .. } => true,
        What::GitTrust(a) => root.starts_with(&a.place),
        _ => false,
    }
}

/// The heading over a part's lines.
fn heading(root: &Path, part: &Offer, project: Option<&Path>) -> String {
    let shown = |p: &Path| crate::trust::visible_line(&crate::chat::shown(root, p));
    match &part.what {
        What::ProjectTrust { .. } => format!("tarae · {}", project.map_or(".tarae.toml".into(), shown)),
        What::ClaudeSettings { .. } => "Claude · it started with these held back".into(),
        What::GitTrust(a) => format!("git · {} — they run {}", shown(&a.config), a.when()),
        _ => String::new(),
    }
}

/// The card for `parts` (each its own card, in order) — one part: that part's own card, as it is.
fn card(root: &Path, mut parts: Vec<Offer>, joined: Option<&str>, project: Option<&Path>) -> Offer {
    if parts.len() == 1 {
        return parts.remove(0);
    }
    let name = root.file_name().map_or_else(|| root.display().to_string(), |n| n.to_string_lossy().into());
    let joined = joined.map(|j| format!(" · {j} joined")).unwrap_or_default();
    let first = format!(
        "{}'s settings run commands as you, even unasked.{joined}",
        crate::trust::visible_line(&name)
    );
    let mut lines = vec![first, String::new()];
    let mut warnings = Vec::new();
    let mut filters = false;
    let start = lines.len();
    for p in &parts {
        lines.push(heading(root, p, project));
        // (the lines its own card decides on, as they are there)
        let q: Vec<&str> = p.question.split('\n').collect();
        lines.extend(p.decided_lines().filter_map(|i| q.get(i)).map(|l| l.to_string()));
        if let What::GitTrust(a) = &p.what {
            if let Some(names) = a.files_named() {
                lines.push(format!("  as these files are now: {names} — a change to one asks again"));
            }
            filters |= a.filters;
            if !a.showable() {
                warnings.push(lines.len()..lines.len() + 1);
                lines.push("  a value is too long to show — y holds git's part back".into());
            }
        }
    }
    let decided = start..lines.len();
    lines.push(String::new());
    // (git's filters held: what that shows, said as its own card says it)
    let filtered = if filters { " Filtered files show as git stores them (LFS pointers, say)." } else { "" };
    lines.push(format!(
        "Held back until y.{filtered} Each answer is kept apart — a later change asks only its part."
    ));
    let asked = parts.iter().find_map(|p| p.asked.clone());
    let what = What::RepoTrust { root: root.to_path_buf(), parts };
    let mut o =
        Offer::new(TITLE.into(), &lines.join("\n"), "", "Repository settings".into(), what).deciding(decided);
    o.warnings = warnings;
    o.asked = asked;
    o
}

/// `card` in the place of `x`: the same card, changed — settled again before `y` counts, a key pressed while it
/// settled not carried over (both go by `rev`).
fn replace(x: &mut Offer, mut card: Offer) {
    (card.id, card.since, card.came, card.yours) = (x.id, x.since, x.came, x.yours);
    card.drawn = x.drawn.clone();
    card.rev = x.rev + 1;
    *x = card;
}

/// The parts a trust card holds: its own, or the repository card's.
fn parts_of(o: &Offer) -> Vec<Offer> {
    match &o.what {
        What::RepoTrust { parts, .. } => parts.clone(),
        w if named(w).is_some() => vec![o.clone()],
        _ => Vec::new(),
    }
}

impl Editor {
    /// A part's own card, just found: it joins the card of another part of this checkout waiting unanswered —
    /// up (in front or stacked) or parked with its session — else it goes up on its own.
    pub(crate) fn trust_offer(&mut self, o: Offer) {
        let root = self.session.root.clone();
        if !of_checkout(self, &root, &o.what) {
            return self.push_offer(o);
        }
        let front = !self.background();
        let list = if front { &self.offers } else { &self.session.parked };
        let with = |x: &Offer| {
            matches!(x.state, OfferState::Asking)
                && match &x.what {
                    What::RepoTrust { root: r, .. } => *r == root,
                    // (the same part up already: it takes its place — `push_offer`, by its title)
                    w => named(w).is_some() && !same(w, &o.what) && of_checkout(self, &root, w),
                }
        };
        let Some(at) = list.iter().position(with) else { return self.push_offer(o) };
        let mut parts = parts_of(&list[at]);
        if parts.len() == 1 {
            self.count(crate::features::Area::Card, "repo-trust");
        }
        self.count(crate::features::Area::Card, o.kind());
        let joined = named(&o.what).map(|(n, _)| n);
        match parts.iter().position(|p| same(&p.what, &o.what)) {
            Some(i) => parts[i] = o,
            None => parts.push(o),
        }
        crate::sorting::by_key(&mut parts, order);
        let project = self.project_trust.as_ref().map(|p| p.file.clone());
        let card = card(&root, parts, joined, project.as_deref());
        let list = if front { &mut self.offers } else { &mut self.session.parked };
        replace(&mut list[at], card);
    }
}

/// A part no longer asked about here (its file needs no answer now, or a command asks about it alone): out of the
/// repository's card — one part left, that part's own card again; none, the card goes. `everywhere`: every
/// session's cards (tarae's and git's parts), else this session's (Claude's).
pub(crate) fn drop_part(ed: &mut Editor, gone: impl Fn(&What) -> bool, everywhere: bool) {
    let project = ed.project_trust.as_ref().map(|p| p.file.clone());
    let front = !ed.background();
    let mut lists: Vec<&mut Vec<Offer>> = vec![&mut ed.session.parked];
    if front || everywhere {
        lists.push(&mut ed.offers);
    }
    if everywhere {
        lists.extend(ed.sessions.iter_mut().map(|s| &mut s.parked));
    }
    let mut emptied = false;
    for list in lists {
        for o in list.iter_mut() {
            let What::RepoTrust { root, parts } = &o.what else { continue };
            if !parts.iter().any(|p| gone(&p.what)) {
                continue;
            }
            let root = root.clone();
            let left: Vec<Offer> = parts.iter().filter(|p| !gone(&p.what)).cloned().collect();
            match left.is_empty() {
                true => {
                    o.what = What::RepoTrust { root, parts: left };
                    emptied = true;
                }
                false => replace(o, card(&root, left, None, project.as_deref())),
            }
        }
    }
    if emptied {
        let empty = |w: &What| matches!(w, What::RepoTrust { parts, .. } if parts.is_empty());
        ed.drop_cards(empty);
        ed.offers.retain(|o| !empty(&o.what));
        ed.sessions.iter_mut().for_each(|s| s.parked.retain(|o| !empty(&o.what)));
    }
}

/// The card answered (`trusted`): each part as its own card's answer would, one toast for them all.
pub(crate) fn answer(ed: &mut Editor, id: u64, parts: Vec<Offer>, trusted: bool) {
    ed.offer_finished(id, Ok(()));
    let (mut took, mut asks, mut too_long) = (Vec::new(), Vec::new(), false);
    for p in parts {
        let Some((name, command)) = named(&p.what) else { continue };
        asks.push(command);
        let n = match p.what {
            What::ProjectTrust { hash } => ed.project_trust_answered(hash, trusted),
            What::ClaudeSettings { hash } => ed.claude_settings_answered(hash, trusted),
            What::GitTrust(a) => {
                // (a value too long to show is never trusted — held back, as its own card's `n`)
                let yes = trusted && a.showable();
                too_long |= trusted && !yes;
                let n = a.items.len();
                ed.git_trust_answered(*a, yes);
                yes.then_some(n)
            }
            _ => None,
        };
        if let Some(n) = n {
            took.push(format!("{name}'s {n}"));
        }
    }
    match trusted {
        true => {
            let long = if too_long { " — git's held back: a value too long to show" } else { "" };
            ed.set_success(format!("Trusted — this repository's settings apply: {}{long}", took.join(", ")))
        }
        false => ed.set_status(format!("Held back — {} to look again", asks.join(", "))),
    }
}

/// What `v` adds after the card's text: each part's own card whole, and Claude's detail.
pub(crate) fn view_text(ed: &Editor, parts: &[Offer]) -> String {
    let seen = ed.session.chat.as_ref().and_then(|c| c.settings.seen.as_ref());
    let mut more = String::new();
    for p in parts {
        more += &format!("\n── {}\n\n{}\n", p.title, p.question);
        if let What::ClaudeSettings { hash } = p.what
            && let Some(s) = seen.filter(|s| s.digest() == Some(hash))
        {
            more += &format!("\n{}\n", crate::claudetrust::detail(s));
        }
    }
    more
}

#[cfg(test)]
mod tests;
