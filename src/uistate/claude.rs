//! The UI state, phase 3 — Claude's side (`docs/design/ui-state.md`):
//!
//! - **Claude's level**: a strict subset of the state — its own session in detail, other sessions as name,
//!   what they do and how many cards; no unsent text (the chat's draft, a prompt's or picker's query); a
//!   plugin's card and a repository's git settings (`git-trust`) by their titles only, in the inbox too.
//!   Less than the `screen` tool sends, so its `ui` tool asks no card.
//! - **The context line**: every message's `<editor-context>` gains `ui: …` when something's up beyond the
//!   code and the chat — the cards, the toasts, the runs under way, by id.
//! - **Ids in Claude's answers** (`card#12`, `run#7`, `git:src/a.rs`) are links: a click or `C-]` brings the
//!   element into sight without changing what the keys answer, and it flashes a moment; one that went says
//!   how (the gone ring); one that never was says so, faintly.
//! - **Given and cited**: this turn's ids that went to Claude carry a faint `◦`, the ones its answer cites a
//!   `•` — until the next question; none on toasts.

use std::ops::Range;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::UiState;
use crate::editor::Editor;
use crate::sizes::Slot;

/// How long an element brought into sight from a link has the selected row's colour behind it.
pub const FLASH: Duration = Duration::from_millis(1200);
/// What one item of the context's `ui:` line says of a card or a toast, at most.
const ITEM_MAX: usize = 60;
/// A panel's rows Claude's state names, at most (the rest are counted).
const ROWS_NAMED: usize = 12;

/// The kinds of element ids with a number (`card#12`).
const NUMBERED: [&str; 5] = ["card", "toast", "run", "pane", "note"];
/// The panels whose rows are named by their content (`git:src/a.rs`) — the side panel's tabs and the review.
pub const ROWS: [&str; 6] = ["files", "outline", "list", "git", "plugin", "review"];

// ── Claude's level ───────────────────────────────────────────────────────

impl Editor {
    /// The state at Claude's level, for the session whose chat asks (`ed.session` — a session behind is
    /// swapped in for its own Claude's calls).
    pub fn ui_state_claude(&self) -> UiState {
        let own = super::session_id(self.session.id);
        let mut s = self.ui_state();
        if self.behind {
            // (behind: nothing of it is on screen — no keys, panels or toasts are its; its cards wait parked)
            s.panels = Default::default();
            s.toasts.clear();
            s.prompt = None;
            s.cards.retain(|c| c.up == "parked");
            for c in &mut s.cards {
                c.session = own.clone();
            }
        }
        s.prompt = s.prompt.map(|(label, _)| (label, String::new()));
        if let Some(c) = s.panels.chat.as_mut() {
            c.draft.clear();
        }
        if let Some(p) = s.panels.picker.as_mut() {
            p.query.clear();
            // (a row carrying a line of a file named like a key Claude may not read: its path and line, the
            // text not — `reads::picker_row_shut`; the rows are the visible lines, top to bottom)
            for (line, r) in p.rows.iter_mut().enumerate() {
                if let Some(shut) = crate::reads::picker_row_shut(self, line) {
                    r.id = format!("picker:{shut}");
                    r.label = shut;
                }
            }
        }
        // (the side panel's the same — its ids carry no text: `list:path:12`, `git:a.rs#2`)
        if let Some((_, _, rows, _)) = s.panels.side.as_mut() {
            let shut = crate::reads::side_rows_shut(self);
            for r in rows.iter_mut() {
                if let Some((_, label)) = shut.iter().find(|(id, _)| *id == r.id) {
                    r.label = label.clone();
                }
            }
        }
        for l in s.sessions.iter_mut().filter(|l| l.id != own) {
            l.branch = None;
            l.root.clear();
            l.rebasing = None;
            l.compacted = None;
            l.key_source = None;
            l.sandbox = None;
        }
        // A reviewer — its own session's too — is how it goes and its counts, never its words: nothing of it
        // goes to the author until you send its findings (`space v s`), and its verdict never does
        for r in s.sessions.iter_mut().filter_map(|l| l.reviewer_run.as_mut()) {
            r.verdict.clear();
            r.why = None;
            r.of.clear();
        }
        s.cards.retain(|c| c.session == own);
        // (a toast that's news of another session — its turn ended, it waits: theirs, a count here)
        let others: Vec<String> = self
            .toasts
            .iter()
            .filter(|t| t.of.is_some_and(|of| of != self.session.id))
            .map(|t| format!("toast#{}", t.id))
            .collect();
        s.toasts.retain(|t| !others.contains(&t.id));
        // (the inbox is every session's: only its own session's entries — the others are counts)
        s.inbox.retain(|e| e.session == own);
        // (a plugin's card, a repository's own git settings and its Claude settings — text from outside, a
        // credential helper's value maybe a secret: by title only, on the card and in the inbox; any other card's
        // ask cut as the one-line forms cut it: the whole is for scripts — Claude's level doesn't grow with a
        // long card)
        let by_title =
            |kind: &str| kind.starts_with("plugin") || kind == "git-trust" || kind == "claude-settings";
        for c in &mut s.cards {
            c.asks = match by_title(c.kind) {
                true => c.title.clone(),
                false => super::asks_line(&c.asks),
            };
            // (the text whole is for scripts — the warnings' ids stay, but not a plugin's or git's)
            c.text.clear();
            if by_title(c.kind) {
                c.warnings.clear();
            }
        }
        for e in &mut s.inbox {
            let card = s.cards.iter().find(|c| c.id == e.id);
            e.asks = match by_title(e.kind) {
                true => card.map_or_else(|| e.kind.into(), |c| c.title.clone()),
                false => super::asks_line(&e.asks),
            };
        }
        s.runs.retain(|r| r.session == own);
        // (its `procs` tool says these, each process's command line and folder with them)
        s.procs = Default::default();
        s.gone = self.ui_gone.iter().rev().filter(|g| g.session == own).take(3).cloned().collect();
        s.front = own;
        s
    }
}

impl UiState {
    /// The text form at Claude's level (`ui_state_claude`): no pid or frame; the panes; the rows on screen
    /// by id.
    pub fn text_claude(&self, behind: bool) -> String {
        let mut out = Vec::new();
        if behind {
            out.push(format!(
                "your session ({}) is behind another — nothing of yours is on screen",
                self.front
            ));
        } else {
            out.extend(self.keys_line());
            out.push(self.mode_line());
        }
        if self.sessions.len() > 1 {
            let s: Vec<String> = self
                .sessions
                .iter()
                .map(|s| {
                    let whose = match (s.id == self.front, s.front && !behind) {
                        (true, _) => " (yours)",
                        (false, true) => " (in front)",
                        _ => "",
                    };
                    let cards = if s.cards == 0 { String::new() } else { format!(" · {} cards", s.cards) };
                    let reviewer = s.reviewer.map(|r| format!(" · {r}")).unwrap_or_default();
                    format!("{} {}{whose} · {}{reviewer}{cards}", s.id, s.name, s.doing)
                })
                .collect();
            out.push(format!("sessions: {}", s.join("   ")));
        }
        // (its own session's reviewer, how it went and its counts — the others' are their `· reviewing` above)
        if let Some(r) =
            self.sessions.iter().find(|s| s.id == self.front).and_then(|s| s.reviewer_run.as_ref())
        {
            out.push(format!("reviewer: {}", r.text()));
        }
        // (its own session's sandbox, decision 13 — here, not in the context's `ui:` line every request reads: the
        // run reports say it once a conversation)
        if let Some(g) = self.sessions.iter().find(|s| s.id == self.front).and_then(|s| s.sandbox.as_ref()) {
            let why = g.why.as_ref().map(|w| format!(" — not the top tier: {w}")).unwrap_or_default();
            out.push(format!("sandbox: {}{why}", g.line_after_label()));
        }
        out.extend(self.card_lines());
        for p in &self.panels.code {
            let focused = if p.focused { " (focused)" } else { "" };
            out.push(format!(
                "{} {} lines {}-{} · cursor {}{focused}",
                p.id, p.path, p.lines.0, p.lines.1, p.cursor_line
            ));
        }
        self.panels.text(&mut out);
        let named = |rows: &[super::panels::Row]| -> String {
            let mut ids: Vec<&str> = rows.iter().take(ROWS_NAMED).map(|r| r.id.as_str()).collect();
            let more = rows.len().saturating_sub(ROWS_NAMED);
            let more = if more > 0 { format!(" +{more}") } else { String::new() };
            ids.dedup();
            format!("{}{more}", ids.join(" "))
        };
        if !self.panels.covered {
            if let Some((tab, _, rows, _)) = self.panels.side.as_ref().filter(|s| !s.2.is_empty()) {
                out.push(format!("{ROWS_LINE}{tab}: {}", named(rows)));
            }
            if let Some(r) = self.panels.review.as_ref().filter(|r| !r.rows.is_empty()) {
                out.push(format!("{ROWS_LINE}review: {}", named(&r.rows)));
            }
        }
        out.extend(self.runs.iter().map(super::panels::Run::text));
        for g in &self.gone {
            out.push(format!("gone: {} {:?} — {}", g.id, g.title, g.how));
        }
        out.join("\n") + "\n"
    }

    /// The context's `ui:` line, and the ids it names — None when nothing's up but the code and the chat (the
    /// context says those already): the cards, the toasts, the runs under way.
    pub fn context_line(&self) -> Option<(String, Vec<String>)> {
        let mut items = Vec::new();
        let mut ids = Vec::new();
        for c in &self.cards {
            items.push(format!("{} {} {:?} ({}, {})", c.id, c.kind, cut(&c.asks), c.whose, c.up));
            ids.push(c.id.clone());
        }
        for t in &self.toasts {
            items.push(format!("{} {} {:?}", t.id, t.kind, cut(t.text.lines().next().unwrap_or_default())));
            ids.push(t.id.clone());
        }
        for r in self.runs.iter().filter(|r| matches!(r.state.as_str(), "queued" | "running" | "waiting")) {
            items.push(format!("{} {} {}", r.id, cut(&r.label), r.state));
            ids.push(r.id.clone());
        }
        (!items.is_empty()).then(|| (format!("ui: {}", items.join(" · ")), ids))
    }
}

/// The start of the text form's lines naming a panel's rows (`rows git: …`) — what a `ui` answer names
/// whole, not one by one: they aren't given marks.
const ROWS_LINE: &str = "rows ";

fn cut(s: &str) -> String {
    match s.chars().count() > ITEM_MAX {
        true => s.chars().take(ITEM_MAX - 1).collect::<String>() + "…",
        false => s.to_string(),
    }
}

/// The `ui` tool as tarae's MCP server lists it.
pub fn tool_schema() -> Value {
    json!({
        "name": "ui",
        "description": "What's on the user's screen in tarae, by id: where the keys go, the cards up and what \
    each asks, toasts, panes, panels and their rows, runs, cards gone and how; other sessions as counts. \
    Read-only, no card.",
        "inputSchema": { "type": "object", "properties": {} }
    })
}

/// A `ui` call: the state at Claude's level as text — the elements it names one by one are given this turn.
pub fn tool_call(ed: &mut Editor) -> Result<String, String> {
    let text = ed.ui_state_claude().text_claude(ed.behind);
    let named: String = text.lines().filter(|l| !l.starts_with(ROWS_LINE)).collect::<Vec<_>>().join("\n");
    give(ed, ids_in(&named).into_iter().map(|(_, id)| id));
    Ok(text)
}

/// Elements that went to Claude this turn (`Chat.given`) — they carry the faint `◦`.
pub fn give(ed: &mut Editor, ids: impl IntoIterator<Item = String>) {
    let Some(c) = ed.session.chat.as_mut() else { return };
    for id in ids {
        if !c.given.contains(&id) {
            c.given.push(id);
        }
    }
}

/// The ids Claude's answer cites — kept as it streams (`Chat.cited`).
pub fn cite(c: &mut crate::chat::Chat) {
    let turn = c.msgs.iter().rposition(|m| m.role == crate::chat::Role::User).map_or(0, |i| i + 1);
    let mut cited: Vec<String> = Vec::new();
    for m in c.msgs[turn..].iter().filter(|m| m.role == crate::chat::Role::Assistant) {
        for (_, id) in ids_in(&m.text) {
            if !cited.contains(&id) {
                cited.push(id);
            }
        }
    }
    c.cited = cited;
}

/// What the user pointed at, as the message carries it: `<pointed id="card#12">its line</pointed>` — the
/// line Claude's state has for it, a row's panel and label, a slot panel's tab.
pub fn pointed(ed: &Editor, id: &str) -> String {
    let s = ed.ui_state_claude();
    let text = s.text_claude(ed.behind);
    let own =
        text.lines().find(|l| l.strip_prefix(id).is_some_and(|r| r.starts_with(' '))).map(str::to_string);
    let p = &s.panels;
    let side = p.side.iter().flat_map(|(tab, _, rows, _)| rows.iter().map(move |r| (tab.as_str(), r)));
    let review = p.review.iter().flat_map(|r| r.rows.iter().map(|x| ("review", x)));
    let row = side.chain(review).find(|(_, r)| r.id == id).map(|(tab, r)| {
        let hand = if r.selected { ", in hand" } else { "" };
        format!("{id} · {:?} — a row of the {tab} panel{hand}", r.label)
    });
    let slot = p.slot.as_ref().filter(|_| !id.contains('#')).map(|s| {
        let tab = s.tabs.iter().find(|t| t.0 == id).map(|t| t.1).unwrap_or_default();
        let front = if s.front.as_deref() == Some(id) { ", in front" } else { "" };
        format!("{id} — the bottom slot's panel{front} {tab}").trim_end().to_string()
    });
    let body = own.or(row).or(slot).unwrap_or_else(|| ed.ui_gone_said(id));
    format!("<pointed id=\"{id}\">{body}</pointed>")
}

// ── Ids in text ──────────────────────────────────────────────────────────

/// The element ids in `text`, each with its byte range: `card#12`, `toast#3`, `run#7`, `pane#2`, `note#4`,
/// and rows by content — `git:src/a.rs`, `review:src/a.rs`, `files:src/`, `list:src/a.rs:12`, one of
/// another session as `s2/git:src/a.rs`. Not inside a word, a path or a URL.
pub fn ids_in(text: &str) -> Vec<(Range<usize>, String)> {
    let b = text.as_bytes();
    let joined = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'/' | b'-' | b'#' | b':');
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if (i > 0 && joined(b[i - 1])) || !b[i].is_ascii_lowercase() {
            i += 1;
            continue;
        }
        match id_at(&text[i..]) {
            Some(n) => {
                out.push((i..i + n, text[i..i + n].to_string()));
                i += n;
            }
            None => i += 1,
        }
    }
    out
}

/// The length of the id `s` starts with.
fn id_at(s: &str) -> Option<usize> {
    let digits = |s: &str| s.bytes().take_while(u8::is_ascii_digit).count();
    // (a session's: `s2/` before a row)
    let q = match s.strip_prefix('s').map(|r| (digits(r), r)) {
        Some((n, r)) if n > 0 && r[n..].starts_with('/') => 1 + n + 1,
        _ => 0,
    };
    let rest = &s[q..];
    let end_ok = |n: usize| rest[n..].bytes().next().is_none_or(|c| !c.is_ascii_alphanumeric() && c != b'_');
    if q == 0 {
        for k in NUMBERED {
            if let Some(r) = rest.strip_prefix(k).and_then(|r| r.strip_prefix('#')) {
                let n = digits(r);
                let len = k.len() + 1 + n;
                return (n > 0 && end_ok(len)).then_some(len);
            }
        }
    }
    let tab = ROWS.iter().find(|t| rest.strip_prefix(**t).is_some_and(|r| r.starts_with(':')))?;
    let body = &rest[tab.len() + 1..];
    let stop = |c: char| c.is_whitespace() || "`'\"<>()[]{},;|*".contains(c);
    let mut n = body.find(stop).unwrap_or(body.len());
    // (a sentence's end isn't the row's)
    while n > 0 && body[..n].ends_with(['.', ':', '!', '?']) {
        n -= 1;
    }
    let row = &body[..n];
    if row.is_empty() || row.starts_with("//") {
        return None;
    }
    Some(q + tab.len() + 1 + n)
}

// ── Where an id is, and bringing it into sight ───────────────────────────

impl Editor {
    /// Whether `id` is (or was) an element here — a made-up one is drawn faint, not as a link. Rows are
    /// taken as they're written (whether one's in its panel is found when it's followed).
    pub fn ui_known(&self, id: &str) -> bool {
        let Some((kind, n)) = numbered(id) else { return true };
        let sessions = || std::iter::once(&self.session).chain(&self.sessions);
        match kind {
            "card" => {
                self.offers.iter().any(|o| o.id == n)
                    || sessions().any(|l| l.parked.iter().any(|o| o.id == n))
                    || self.ui_gone.iter().any(|g| g.id == id)
            }
            "toast" => n >= 1 && n <= self.said,
            "run" => n >= 1 && n < crate::trace::next_id(),
            "pane" => sessions().any(|l| l.views.iter().any(|v| v.id == n)),
            "note" => sessions().any(|l| l.notes.list.iter().any(|x| x.id == n)),
            _ => false,
        }
    }

    /// Whether `id` flashes now (brought into sight from a link, `FLASH` ago at most).
    pub fn ui_flashing(&self, id: &str) -> bool {
        self.ui_flash.as_ref().is_some_and(|(f, at)| f == id && at.elapsed() < FLASH)
    }

    /// The mark an element carries: `•` cited by this turn's answer, `◦` given to Claude this turn — the
    /// front session's chat's, faint (never on a toast).
    pub fn ui_mark(&self, id: &str) -> Option<&'static str> {
        let c = self.session.chat.as_ref()?;
        if c.cited.iter().any(|x| x == id) {
            Some("•")
        } else if c.given.iter().any(|x| x == id) {
            Some("◦")
        } else {
            None
        }
    }

    /// Whether any element is marked or flashing — drawing looks ids up only then.
    pub fn ui_lit(&self) -> bool {
        self.ui_flash.is_some()
            || self.session.chat.as_ref().is_some_and(|c| !c.given.is_empty() || !c.cited.is_empty())
    }

    /// What a link to `id` says when it can't show it: how it went (the gone ring), what it said (a toast),
    /// that it's over (a run) — or that there's no such element.
    pub fn ui_gone_said(&self, id: &str) -> String {
        if let Some(g) = self.ui_gone.iter().rev().find(|g| g.id == id) {
            let ago = crate::term::duration(g.at.elapsed().as_secs_f32());
            return format!("{id} went {ago} ago: {} — {}", g.how, g.title);
        }
        match numbered(id) {
            Some(("toast", n)) if n >= 1 && n <= self.said => {
                let back = (self.said - n) as usize;
                match self.messages.len().checked_sub(back + 1).and_then(|i| self.messages.get(i)) {
                    Some((_, _, text)) => format!("{id} went: {}", text.lines().next().unwrap_or_default()),
                    None => format!("{id} went — :messages has the latest"),
                }
            }
            Some(("run", _)) if self.ui_known(id) => format!("{id} is over — its panel has another run now"),
            _ if !self.ui_known(id) => format!("{id} — no such element"),
            _ => format!("{id} isn't on screen"),
        }
    }

    /// A link to `id` followed (a click, `C-]`): the element into sight — another session to the front, a
    /// side tab shown and its row scrolled to, a run's panel in front of the slot — and a flash behind it, the
    /// keys answering what they did. A card stacked behind flashes where it is (to the front, `y` would
    /// answer it). Gone, or never there: said, no flash.
    pub fn ui_jump(&mut self, id: &str) {
        let (session, rest) = match id.split_once('/') {
            Some((s, r)) if s.starts_with('s') && s[1..].parse::<u64>().is_ok() => {
                (s[1..].parse::<crate::event::SessionId>().ok(), r.to_string())
            }
            _ => (None, id.to_string()),
        };
        if let Some(s) = session
            && s != self.session.id
            && !self.session_switch(s)
        {
            return self.set_status(format!("{id} — no such session"));
        }
        let id = rest.as_str();
        let shown = match numbered(id) {
            Some(("card", n)) => self.jump_card(n),
            Some(("toast", n)) => self.toasts.iter().any(|t| t.id == n && t.alive()),
            Some(("run", n)) => self.jump_run(n),
            Some(("pane", n)) => {
                let here = self.session.views.iter().any(|v| v.id == n);
                // (only the focused pane has a path bar to flash — another one is said)
                if here && self.session.focus != n {
                    let name = self.session.views.iter().find(|v| v.id == n).and_then(|v| {
                        self.docs.iter().find(|d| d.id == v.doc).map(|d| d.display_name(&self.session.root))
                    });
                    return self.set_status(format!("{id}: {} — the other pane", name.unwrap_or_default()));
                }
                here
            }
            Some(("note", n)) => {
                let Some(note) = self.session.notes.list.iter().find(|x| x.id == n) else {
                    return self.set_status(self.ui_gone_said(id));
                };
                let link = crate::chat::Link { path: note.path.clone(), line: note.lines.start };
                return crate::chat::jump(self, link);
            }
            _ => match self.jump_row(id) {
                Ok(()) => true,
                Err(why) => return self.set_status(why),
            },
        };
        if shown {
            self.ui_flash = Some((id.to_string(), Instant::now()));
        } else {
            self.set_status(self.ui_gone_said(id));
        }
    }

    /// Card `n` into sight: up here (front or stacked) as it is; parked in another session — that session to
    /// the front, its cards up with it.
    fn jump_card(&mut self, n: u64) -> bool {
        if self.offers.iter().any(|o| o.id == n) {
            return true;
        }
        let parked = self.sessions.iter().find(|l| l.parked.iter().any(|o| o.id == n)).map(|l| l.id);
        match parked {
            Some(s) => self.session_switch(s) && self.offers.iter().any(|o| o.id == n),
            None => false,
        }
    }

    /// Run `n`'s panel in front of the slot (the keys stay where they are), in its session.
    fn jump_run(&mut self, n: u64) -> bool {
        let of = |l: &crate::session::Session| {
            if l.task_run.as_ref().is_some_and(|r| r.trace.id() == n) {
                return Some(Slot::Task);
            }
            if l.test_run.as_ref().is_some_and(|r| r.trace.id() == n) {
                return Some(Slot::Test);
            }
            // (waiting in line: the panel it waits for)
            l.runs_next.iter().find(|q| q.id == n).map(|q| match q.what.slot() {
                crate::queued::Slot::Task => Slot::Task,
                crate::queued::Slot::Tests => Slot::Test,
            })
        };
        let here = of(&self.session);
        let there = self.sessions.iter().find_map(|l| of(l).map(|s| (l.id, s)));
        let slot = match (here, there) {
            (Some(s), _) => s,
            (None, Some((l, s))) if self.session_switch(l) => s,
            _ => return false,
        };
        if self.slot_there(slot, false) {
            self.slot_show(slot);
        }
        true
    }

    /// A panel's row into sight: the review's rail scrolled to its file; the side panel showing the row's tab
    /// (opened without the keys if it was closed), scrolled so the row shows. Err: why it can't.
    fn jump_row(&mut self, id: &str) -> Result<(), String> {
        let (tab, _) = id.split_once(':').ok_or_else(|| self.ui_gone_said(id))?;
        if tab == "review" {
            let rel = &id["review:".len()..];
            let Some(r) = self.session.code_review.as_mut() else {
                return Err(format!("{id} — no review is open"));
            };
            let order = r.rail_rows();
            let at = order.iter().position(|t| t.item.is_some_and(|i| r.files[i].rel == rel));
            let Some(at) = at else { return Err(format!("{id} isn't among the review's files")) };
            // (into the rail's view — `fit` keeps a scroll that shows the marked file)
            if at < r.scroll || at >= r.scroll + 8 {
                r.scroll = at.saturating_sub(2);
            }
            return Ok(());
        }
        use crate::sidebar::Mode as M;
        let mode = match tab {
            "files" => M::Files,
            "outline" => M::Outline,
            "list" => M::List,
            "git" => M::Git,
            "plugin" => M::Plugin,
            _ => return Err(self.ui_gone_said(id)),
        };
        if self.session.code_review.is_some() && self.review_rail_up() {
            return Err(format!("{id} — the review's rail has the side panel's place (space g shows it)"));
        }
        match self.session.sidebar.as_mut() {
            Some(s) if s.mode == mode => {}
            Some(s) => {
                s.mode = mode;
                s.shown = None;
            }
            None => {
                let mut s = crate::sidebar::Sidebar::new(mode, self.session.root.clone());
                s.focus = false;
                self.session.sidebar = Some(s);
                if mode == M::Files {
                    self.sidebar_list_files();
                }
            }
        }
        self.sidebar_sync();
        let Some(sb) = self.session.sidebar.as_ref() else { return Err(self.ui_gone_said(id)) };
        let lines = sb.lines();
        let at = lines.iter().position(|l| super::panels::side_id(self, tab, l) == id);
        let Some(at) = at else { return Err(format!("{id} isn't in the {tab} tab now")) };
        let sb = self.session.sidebar.as_mut().expect("there");
        // (scrolled so the row shows — `fit` keeps a scroll while the selection doesn't move)
        if at < sb.scroll || at >= sb.scroll + 10 {
            sb.scroll = at.saturating_sub(3);
        }
        sb.shown = Some(sb.selected);
        Ok(())
    }
}

/// `card#12` → ("card", 12).
fn numbered(id: &str) -> Option<(&'static str, u64)> {
    let (kind, n) = id.split_once('#')?;
    Some((NUMBERED.into_iter().find(|k| *k == kind)?, n.parse().ok()?))
}

#[cfg(test)]
mod tests;
