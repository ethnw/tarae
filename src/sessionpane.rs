//! The session list, a floating pane (panes P3 step 7, `target/sb/panes-P3-plan.md` §3.6; CLAUDE.md "The manager's
//! view"): `space W` opens it in a named frame (`╭─ sessions ─…─╮`) over the code and the chat — not a modal
//! picker: it stays while you work (`esc code` gives the keys back, it stays in sight; `space W`, a click or `C-w w`
//! gives it them again; `q` closes it), and across switching sessions — it's the viewer's, not a workspace's. Done
//! once used (`floatlist.rs`, user 2026-10-23): what takes you to a session from it — `enter`, a digit, a double
//! click, `o`, `f`, `v` — closes it.
//!
//! It's the manager's overview: one row a session, its name in its colour, then what a person watching several
//! sessions work needs — its state (`working` · `▲ waits` · `◦ stuck?` · `done` · `queued 2nd`), what waits for you
//! (cards, an edit to review — counted), `≈` whom it overlaps, `↓N` behind its base, its context, its last report line
//! (its Claude's last sentence), then the rest the list always said (its branch, what's changed, where its Claude is,
//! its reviewer, who pays, its cache and cost, its build folder, its processes). Each a column as wide as its widest,
//! row under row; narrow, whole columns fold (`lay`): the rest first, then the report, the context, behind — what
//! waits for you and the state last, the name never. Built from the sessions every frame it's drawn (`rows`), nothing
//! slow: git's part is read on a worker as it opens (`Editor::session_list`).
//!
//! Its keys are a floating list's (`floatlist.rs`: the moves, its own search, `esc code`, `q`), `enter` switches to
//! the row's session, and its letters as the picker's were (`Editor::session_list_key`: `o` new, `d` end, `b` bring,
//! `v` woven, `f` follow, digits, …).

use crate::editor::Editor;
use crate::event::SessionId;
use crate::pane::GroupKind;
use unicode_width::UnicodeWidthStr as _;

/// How a part of a row is inked (`term::draw_sessions_pane` maps it to the theme): the row's text, faint, what needs
/// you (warning), done (the accent), a session's own colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ink {
    Text,
    Faint,
    Warn,
    Accent,
    Participant(u8),
}

/// A row's columns, in their order on the row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Col {
    State,
    Waits,
    Overlap,
    Behind,
    Context,
    Report,
    Branch,
    Ahead,
    Changed,
    Reloaded,
    Compacted,
    At,
    Reviewer,
    Billing,
    Cache,
    Cost,
    Build,
    Usage,
}

/// Every column, in the row's order.
pub const COLS: [Col; 18] = [
    Col::State,
    Col::Waits,
    Col::Overlap,
    Col::Behind,
    Col::Context,
    Col::Report,
    Col::Branch,
    Col::Ahead,
    Col::Changed,
    Col::Reloaded,
    Col::Compacted,
    Col::At,
    Col::Reviewer,
    Col::Billing,
    Col::Cache,
    Col::Cost,
    Col::Build,
    Col::Usage,
];

/// The order whole columns fold in when the pane is narrow, the first to go first: the rest from the last, then the
/// report line, the context, behind, the overlaps — what waits for you and the state last (user, 2026-10-23).
pub const FOLDS: [Col; 18] = [
    Col::Usage,
    Col::Build,
    Col::Cost,
    Col::Cache,
    Col::Billing,
    Col::Reviewer,
    Col::At,
    Col::Compacted,
    Col::Reloaded,
    Col::Changed,
    Col::Ahead,
    Col::Branch,
    Col::Report,
    Col::Context,
    Col::Behind,
    Col::Overlap,
    Col::Waits,
    Col::State,
];

/// The report line is cut to this at most, and folds before it would be cut shorter than `REPORT_MIN`.
pub const REPORT_MAX: usize = 56;
pub const REPORT_MIN: usize = 16;

/// Two cells between columns, as between key hints.
pub const GAP: usize = 2;

/// One part of a cell: its text and ink.
pub type Part = (String, Ink);

/// A session's row: who it is and its cells, by column (empty ones left out).
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub id: SessionId,
    pub name: String,
    pub colour: u8,
    /// The session in front — its name bold, as the strip's.
    pub front: bool,
    pub cells: Vec<(Col, Vec<Part>)>,
}

impl Row {
    /// Column `c`'s parts (none: empty).
    pub fn cell(&self, c: Col) -> &[Part] {
        self.cells.iter().find(|x| x.0 == c).map_or(&[], |x| &x.1)
    }

    /// Column `c`'s text.
    pub fn text(&self, c: Col) -> String {
        self.cell(c).iter().map(|p| p.0.as_str()).collect()
    }

    /// Every cell's text, two cells apart (the tests' and `session_items`' one line).
    #[cfg(test)]
    pub fn line(&self) -> String {
        COLS.iter().map(|&c| self.text(c)).filter(|t| !t.is_empty()).collect::<Vec<_>>().join("  ")
    }
}

/// Session `l`'s row as `shown` says it.
pub fn row(l: &crate::worktree::Shown) -> Row {
    use crate::session::Doing;
    let mut cells: Vec<(Col, Vec<Part>)> = Vec::new();
    let mut put = |c: Col, parts: Vec<Part>| {
        if parts.iter().any(|p| !p.0.is_empty()) {
            cells.push((c, parts));
        }
    };
    let one = |s: String, ink: Ink| vec![(s, ink)];
    // The state as the strip says it (collab C1): what needs you in warning, done in the accent, the calm states
    // faint — a rebase under way and its place in the bring queue before it
    let (said, ink) = match l.doing {
        Doing::Waiting => ("▲ waits".to_string(), Ink::Warn),
        Doing::Done => ("done".into(), Ink::Accent),
        d @ Doing::Stuck { .. } => (format!("◦ stuck? {}", d.said()), Ink::Warn),
        d @ Doing::Queued { .. } => (d.said(), Ink::Faint),
        Doing::Working => ("working".into(), Ink::Faint),
        Doing::Idle => (String::new(), Ink::Faint),
    };
    let mut state: Vec<Part> = Vec::new();
    for s in [&l.rebasing, &l.queue].into_iter().flatten() {
        state.push((
            format!("{s}{}", if said.is_empty() && state.is_empty() { "" } else { " · " }),
            Ink::Text,
        ));
    }
    state.push((said, ink));
    put(Col::State, state);
    // What waits for you there: its cards, its edits (a review, a change set, a read), counted
    let (cards, edits) = l.waits;
    let waits: Vec<String> = [(cards, "card"), (edits, "edit")]
        .into_iter()
        .filter(|w| w.0 > 0)
        .map(|(n, what)| crate::nav::count(n, what))
        .collect();
    put(Col::Waits, one(waits.join(" · "), Ink::Warn));
    // Whom its changes would conflict with: `≈` in warning, each name in its colour
    if !l.overlap.is_empty() {
        let mut parts = vec![("≈ ".to_string(), Ink::Warn)];
        for (i, (name, c)) in l.overlap.iter().enumerate() {
            if i > 0 {
                parts.push((", ".into(), Ink::Text));
            }
            parts.push((name.clone(), c.map_or(Ink::Text, Ink::Participant)));
        }
        put(Col::Overlap, parts);
    }
    let f = l.facts.clone().unwrap_or_default();
    put(Col::Behind, one(if f.behind > 0 { format!("↓{}", f.behind) } else { String::new() }, Ink::Text));
    if let Some(pct) = l.context {
        let ink = if pct >= 80 { Ink::Warn } else { Ink::Text };
        put(Col::Context, vec![("ctx ".into(), Ink::Faint), (format!("{pct}%"), ink)]);
    }
    put(Col::Report, one(l.report.clone(), Ink::Faint));
    // The rest the list always said
    put(Col::Branch, one(l.branch.clone().filter(|b| *b != l.name).unwrap_or_default(), Ink::Faint));
    put(Col::Ahead, one(if f.ahead > 0 { format!("↑{}", f.ahead) } else { String::new() }, Ink::Text));
    put(
        Col::Changed,
        one(if f.changed > 0 { format!("{} changed", f.changed) } else { String::new() }, Ink::Text),
    );
    put(
        Col::Reloaded,
        one(if l.reloaded > 0 { format!("• {} reloaded", l.reloaded) } else { String::new() }, Ink::Text),
    );
    put(Col::Compacted, one(if l.compacted { "≈ compacted".into() } else { String::new() }, Ink::Text));
    put(Col::At, one(l.at.clone(), Ink::Faint));
    put(Col::Reviewer, one(l.reviewer.clone(), Ink::Text));
    // (who pays: `api` — the plan, the calm default, says nothing — D5)
    let api = if l.billing == crate::billing::Billing::Api { "api" } else { "" };
    put(Col::Billing, one(api.into(), Ink::Text));
    let (cache, cost) = l.claude.clone().unwrap_or_default();
    put(Col::Cache, one(cache, Ink::Faint));
    put(Col::Cost, one(cost, Ink::Text));
    put(
        Col::Build,
        one(
            l.build.map(|s| format!("build {}", crate::worktree::size_text(s))).unwrap_or_default(),
            Ink::Faint,
        ),
    );
    let usage = l.usage.map(|(cpu, bytes)| format!("{cpu:>3.0}%  {:>6}", crate::procs::mem(bytes)));
    put(Col::Usage, one(usage.unwrap_or_default(), Ink::Faint));
    Row { id: l.id, name: l.name.clone(), colour: l.colour, front: l.front, cells }
}

/// The columns shown in a pane whose rows are `room` cells wide (the name's column first, then each shown column
/// `GAP` after the one before), and each one's width: every column as wide as its widest; too wide, whole columns fold
/// in `FOLDS`' order — the report cut to `REPORT_MIN` first once only it and what folds after it are left.
pub fn lay(rows: &[Row], room: usize) -> (usize, Vec<(Col, usize)>) {
    let width = |c: Col| rows.iter().map(|r| r.text(c).width()).max().unwrap_or(0);
    let name_w = rows.iter().map(|r| r.name.width()).max().unwrap_or(0).min(room);
    let all: Vec<(Col, usize)> = (COLS.iter().map(|&c| (c, width(c))))
        .map(|(c, w)| (c, if c == Col::Report { w.min(REPORT_MAX) } else { w }))
        .filter(|c| c.1 > 0)
        .collect();
    let report_at = FOLDS.iter().position(|&c| c == Col::Report).unwrap_or(0);
    for k in 0..=FOLDS.len() {
        let shown: Vec<(Col, usize)> = all.iter().filter(|c| !FOLDS[..k].contains(&c.0)).copied().collect();
        let total = name_w + shown.iter().map(|c| GAP + c.1).sum::<usize>();
        if total <= room {
            return (name_w, shown);
        }
        // (only the report and what outranks it left: the report cut to what's there, while that's enough to read)
        let rest_gone = FOLDS[..report_at].iter().all(|f| !shown.iter().any(|c| c.0 == *f));
        if let Some(r) = shown.iter().find(|c| c.0 == Col::Report).filter(|_| rest_gone)
            && total - r.1 + REPORT_MIN <= room
        {
            let cut = room - (total - r.1);
            let shown =
                shown.iter().map(|&(c, w)| if c == Col::Report { (c, cut) } else { (c, w) }).collect();
            return (name_w, shown);
        }
    }
    (name_w, Vec::new())
}

/// Its Claude's last report line: the last sentence of its last answer, the Markdown's marks taken out — empty before
/// one.
pub fn report(chat: Option<&crate::chat::Chat>) -> String {
    let Some(text) = chat.and_then(|c| c.msgs.iter().rev().find(|m| m.role == crate::chat::Role::Assistant))
    else {
        return String::new();
    };
    let line = text.text.lines().rev().map(str::trim).find(|l| !l.is_empty() && !l.starts_with("```"));
    let line = line.unwrap_or_default().trim_start_matches(['#', '-', '*', '>', ' ']);
    let plain: String = line.replace("**", "").replace('`', "");
    // (its last sentence: past the last `. ` that ends one)
    let last = plain.trim_end_matches(['.', ' ']).rfind(". ").map_or(plain.as_str(), |i| &plain[i + 2..]);
    last.trim().to_string()
}

impl Editor {
    /// `space W` (§3.6): the session list floats in sight with the keys, the session in front selected — up already,
    /// the keys to it. Git's part of each row is read again on a worker (the last read shown meanwhile); the
    /// processes looked at once.
    pub fn session_list(&mut self) {
        self.float_list_open(GroupKind::Sessions, self.session.id, None);
        self.session_facts();
    }

    /// The session list's rows as things are now.
    pub fn sessions_rows(&self) -> Vec<Row> {
        self.session_shown().iter().map(row).collect()
    }

    /// The session list has the keys.
    #[cfg(test)]
    pub fn sessions_list_keys(&self) -> bool {
        self.float_list_keys(GroupKind::Sessions)
    }

    /// The session the list's selection is on (by its id, else the row it was on).
    pub fn sessions_here(&self) -> Option<SessionId> {
        self.float_list_here(GroupKind::Sessions)
    }

    /// `enter` on the list (a digit, a double click): to the selected session — the list done once used, so it
    /// closes (user, 2026-10-23: floating in the middle, it would cover the code); on the session in front too.
    pub(crate) fn sessions_enter(&mut self) {
        let Some(id) = self.sessions_here() else { return };
        self.float_list_used(GroupKind::Sessions);
        if id != self.session.id {
            self.session_switch(id);
        } else if self.docked_in_sight(GroupKind::Sessions) {
            // (docked it stays: nothing to switch to, said — no silent key)
            self.set_status(format!("{} is in front — q closes the list", self.session.label()));
        }
    }
}

/// Row `row`'s text `re` matches: its name, else a column's (its parts as one).
pub(crate) fn matches(row: &Row, re: &regex::Regex) -> bool {
    re.is_match(&row.name) || COLS.iter().any(|&c| re.is_match(&row.text(c)))
}

/// The list's keys for its top line, as they apply to the row in hand: `enter` switches, `/` searches, then what the row's session
/// takes — `v` its changes woven where it overlaps, `f` follow while its Claude works, `B` the marked ones in turn —,
/// `o` new, `d` end and `b` bring (not the checkout tarae started in), `m` mark, `x` its build folder once measured,
/// `w` keep warm, `$` who pays; `esc code`, `q close`. The line folds them from the last.
pub fn hints(ed: &Editor) -> Vec<(&'static str, String)> {
    use crate::session::Doing;
    let here = ed.sessions_here();
    let order = ed.sessions_in_order();
    let l = here.and_then(|id| order.iter().find(|l| l.id == id));
    let working = l.is_some_and(|l| matches!(ed.doing(l), Doing::Working));
    let woven = here.is_some_and(|id| !ed.sessions.is_empty() && crate::overlap::file_of(ed, id).is_some());
    let marked = ed.bring_queue.marks.len();
    let started = here == Some(0);
    let build = here.is_some_and(|id| id != ed.session.id && ed.session_store.build_dirs.contains_key(&id));
    // (`/` second: how the list is searched stays in sight as the line folds)
    let mut v: Vec<(&'static str, String)> = vec![("enter", "switch".into()), ("/", "search".into())];
    v.extend(woven.then(|| ("v", "woven".into())));
    v.extend(working.then(|| ("f", "follow".into())));
    v.extend((marked > 0).then(|| ("B", format!("bring the {marked} in turn"))));
    v.push(("o", "new".into()));
    if !started {
        v.extend([("d", "end".into()), ("b", "bring into its base".into()), ("m", "mark to bring".into())]);
    }
    v.extend(build.then(|| ("x", "drop its build".into())));
    v.extend([
        ("w", "keep warm".into()),
        ("$", "billing".into()),
        ("esc", "code".into()),
        ("q", "close".into()),
    ]);
    v
}

#[cfg(test)]
mod tests;
