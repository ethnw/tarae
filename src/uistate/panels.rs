//! The UI state's `panels` and `runs` (phase 2). A panel's rows are only the ones on screen, read from what
//! `term.rs` recorded in `editor.screen` as it drew them — never laid out again here — and each row's id is
//! its content (`git:src/a.rs`, `review:src/a.rs`, `files:src/`), never its index: indexes move as lists
//! change. Runs carry their trace's id (`run#7`), the same from the ask, through the line, to the end.

use serde_json::{Value, json};

use crate::editor::Editor;

/// A row on screen: its id (its content), what it says, whether it's the selected one.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub id: String,
    pub label: String,
    pub selected: bool,
    /// A review's file row: marked viewed (`space v m`) — a shuttle's step can expect it (`shuttle.rs`).
    pub viewed: Option<bool>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Pane {
    pub id: String,
    pub path: String,
    /// The first and last document lines on screen (1-based).
    pub lines: (usize, usize),
    pub focused: bool,
    pub cursor_line: usize,
    /// Its document's version: bumped by every change, undo and redo too — what a shuttle's step expects text was
    /// written for (`shuttle.rs`).
    pub version: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Panels {
    pub code: Vec<Pane>,
    /// The side panel: its tab, whether it has the keys, its rows on screen, and how many it has.
    pub side: Option<(String, bool, Vec<Row>, usize)>,
    /// The code review: its base, its files, how many viewed, the file in hand, its rail's rows on screen.
    pub review: Option<Review>,
    /// An edit's review in the buffer (Claude's — the only author since `space c` went): file, the change in
    /// hand (i of n), undecided, the reason's first line, what it warns of (ids).
    pub edit_review: Option<EditReview>,
    /// A picker: title, query, its rows on screen, how many match, loading or searching.
    pub picker: Option<Picker>,
    pub slot: Option<SlotPanel>,
    pub chat: Option<Chat>,
    /// A picker covers the side panel and the review's rail: no rows of theirs are in sight.
    pub covered: bool,
}

/// The bottom slot: its tabs (label, badge), the one in front, whether it has the keys.
#[derive(Clone, Debug, PartialEq)]
pub struct SlotPanel {
    pub tabs: Vec<(String, &'static str)>,
    pub front: Option<String>,
    pub keys: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Review {
    pub base: String,
    pub files: usize,
    pub viewed: usize,
    pub file: Option<String>,
    pub focused: bool,
    pub rows: Vec<Row>,
    /// Its high-risk changes no one has looked at (`path:line` — `risk.rs`).
    pub high: Vec<String>,
    /// Its reviewer (`reviewer.rs`): reading · done · stopped, and what it found — findings, on high-risk
    /// changes, answered.
    pub reviewer: Option<(&'static str, crate::reviewer::Tally)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EditReview {
    /// Which review it is (its `llm_generation`): the next edit to the same file is a new one — one `y` ended
    /// the last, though it reads the same (#216).
    pub id: u64,
    pub file: String,
    pub change: (usize, usize),
    pub undecided: usize,
    pub reason: String,
    /// What it warns of, by id (`edit-protected` — cardburden 4b).
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Picker {
    pub title: String,
    pub query: String,
    pub rows: Vec<Row>,
    pub total: usize,
    pub loading: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Chat {
    pub focused: bool,
    pub minimized: bool,
    pub busy: bool,
    pub queued: bool,
    /// What waits for the turn's end, said (`2 messages queued` — `Chat::queued_said`).
    pub queued_said: Option<String>,
    pub draft: String,
    pub follow: bool,
    pub paused: bool,
}

/// A run: Claude's, the user's or tarae's own (an install, a restart's build, git), in the task panel or the
/// test pane, or waiting in line for one.
#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    pub id: String,
    pub session: String,
    pub of: &'static str,
    pub kind: &'static str,
    pub label: String,
    /// `queued` · `running` · `waiting` · `quiet` (printing nothing, held from the stuck card — its own time
    /// limit to come, or its work maybe elsewhere) · `passed` · `failed` · `stopped` · `no tests`.
    pub state: String,
    /// What it waits on (`waiting for cargo's lock · 0:37`), or where it is in line (`1st`).
    pub waiting: Option<String>,
    /// Running and printing nothing for minutes: `no output for 27:04` (`waits::SILENT` — #157).
    pub silent: Option<String>,
    pub since_ms: u64,
    pub took_ms: Option<u64>,
    /// Claude's command: where it ran, from its run's record, and what it warned of (`chat::Ran` — cardburden
    /// 4); None for another's, or one in line.
    pub ran: Option<crate::chat::Ran>,
}

fn rel(ed: &Editor, p: &std::path::Path) -> String {
    p.strip_prefix(&ed.session.root).unwrap_or(p).display().to_string()
}

/// A side panel row's id: what it stands for, in its tab's words.
pub(crate) fn side_id(ed: &Editor, tab: &str, l: &crate::sidebar::Line) -> String {
    use crate::sidebar::Target as T;
    match &l.target {
        T::Dir(d) => format!("{tab}:{}/", d.trim_end_matches('/')),
        T::File(p) => format!("{tab}:{}", rel(ed, p)),
        T::At(at) => {
            let doc = ed.doc();
            let line = doc.text.byte_to_line((*at).min(doc.text.len_bytes())) + 1;
            format!("{tab}:{}@{line}", l.label)
        }
        T::Item(i) => match ed.session.qlist.as_ref().and_then(|q| q.items.get(*i)) {
            Some(it) => format!("{tab}:{}:{}", rel(ed, &it.path), it.line + 1),
            None => format!("{tab}:{}", l.label),
        },
        T::Change { rel, staged: true, .. } if rel.is_empty() => format!("{tab}:staged"),
        T::Change { rel, .. } if rel.is_empty() => format!("{tab}:{}", l.label.to_lowercase()),
        T::Change { rel, staged: true, .. } => format!("{tab}:staged/{rel}"),
        T::Change { rel, .. } => format!("{tab}:{rel}"),
        T::Hunk { rel, index, .. } => format!("{tab}:{rel}#{}", index + 1),
        T::Conflict { rel, index, .. } => format!("{tab}:{rel}!{}", index + 1),
        T::Plugin(i) => format!("{tab}:{i}:{}", l.label),
        T::None => format!("{tab}:{}", l.label),
    }
}

impl Editor {
    /// The panels of the session in front, as last drawn.
    pub fn ui_panels(&self) -> Panels {
        let s = &self.screen;
        let mut p = Panels::default();
        // (a picker floats over the side panel and the rail: their rows were drawn, then covered — found by
        // `every_row_the_state_lists_is_on_the_frame`)
        let covered = s.picker_list.is_some();
        p.covered = covered;
        // The code: each pane, its lines on screen
        for (view, _, _, rows) in &s.panes {
            let Some(v) = self.session.views.iter().find(|v| v.id == *view) else { continue };
            let Some(doc) = self.docs.iter().find(|d| d.id == v.doc) else { continue };
            let lines = rows.iter().map(|r| r.0);
            let (lo, hi) = (lines.clone().min().unwrap_or(0), lines.max().unwrap_or(0));
            let head = doc.selection().primary().cursor(&doc.text).min(doc.text.len_bytes());
            p.code.push(Pane {
                id: format!("pane#{view}"),
                path: doc.display_name(&self.session.root),
                lines: (lo + 1, hi + 1),
                focused: *view == self.session.focus,
                cursor_line: doc.text.byte_to_line(head) + 1,
                version: doc.version(),
            });
        }
        // The side panel: the rows `term.rs` drew (screen row, list row)
        if let Some(sb) = self.session.sidebar.as_ref().filter(|_| s.side_w > 0) {
            let tab = super::side_tab(sb.mode);
            let lines = sb.lines();
            let rows = s
                .sidebar
                .borrow()
                .iter()
                .filter(|_| !covered)
                .filter_map(|&(_, i)| {
                    let l = lines.get(i)?;
                    Some(Row {
                        id: side_id(self, tab, l),
                        label: l.label.clone(),
                        selected: i == sb.selected,
                        viewed: None,
                    })
                })
                .collect();
            p.side = Some((tab.to_string(), self.session.is_at(crate::session::At::Side), rows, lines.len()));
        }
        // The code review: its rail's file rows as drawn
        if let Some(r) = self.session.code_review.as_ref() {
            let marked = r.marked(self);
            let rows = s
                .review_rail
                .borrow()
                .iter()
                .filter(|_| !covered)
                .filter_map(|&(_, i)| {
                    let f = r.files.get(i)?;
                    let name = f.rel.rsplit('/').next().unwrap_or(&f.rel).to_string();
                    Some(Row {
                        id: format!("review:{}", f.rel),
                        label: name,
                        selected: Some(i) == marked,
                        viewed: Some(r.viewed(self, f)),
                    })
                })
                .collect();
            p.review = Some(Review {
                base: r.base.label(),
                files: r.files.len(),
                viewed: r.files.iter().filter(|f| r.viewed(self, f)).count(),
                file: marked.and_then(|i| r.files.get(i)).map(|f| f.rel.clone()),
                focused: self.session.is_at(crate::session::At::Rail),
                rows,
                high: r
                    .high_unseen(self)
                    .into_iter()
                    .map(|(i, at)| format!("{}:{}", r.files[i].rel, at + 1))
                    .collect(),
                // (the session's last run, whatever review is open: it reads a snapshot of its own — bgreview)
                reviewer: self.session.reviewer.as_ref().map(|v| {
                    let state = match v.state {
                        crate::reviewer::State::Waiting | crate::reviewer::State::Reading
                            if crate::reviewer::waits(self, &self.session) =>
                        {
                            "waiting"
                        }
                        crate::reviewer::State::Waiting | crate::reviewer::State::Reading => "reading",
                        crate::reviewer::State::Done => "done",
                        crate::reviewer::State::Failed(_) | crate::reviewer::State::Stopped => "stopped",
                    };
                    (state, crate::reviewer::review_tally(self).unwrap_or_default())
                }),
            });
        }
        if let Some(r) = self.session.review.as_ref().filter(|r| r.ready) {
            let file =
                self.docs.iter().find(|d| d.id == r.doc_id).map(|d| d.display_name(&self.session.root));
            p.edit_review = Some(EditReview {
                id: r.generation,
                file: file.unwrap_or_default(),
                change: (r.current + 1, r.hunks.len()),
                undecided: r.hunks.iter().filter(|h| h.decision == crate::llm::Decision::Undecided).count(),
                reason: r.reason.lines().next().unwrap_or_default().to_string(),
                warnings: r.warnings.iter().map(|w| w.id.clone()).collect(),
            });
        }
        if let Some(pk) = self.picker.as_ref().filter(|_| s.picker_list.is_some()) {
            let rows = pk
                .view
                .iter()
                .map(|r| Row {
                    id: format!("picker:{}", r.label),
                    label: r.label.clone(),
                    selected: r.selected,
                    viewed: None,
                })
                .collect();
            p.picker = Some(Picker {
                title: pk.title.clone(),
                query: pk.query.clone(),
                rows,
                total: pk.counts().0,
                loading: pk.loading || pk.searching || pk.filtering,
            });
        }
        if s.bottom_h > 0 {
            let tabs = self
                .slot_tabs()
                .into_iter()
                .map(|t| {
                    let badge = match t.badge {
                        crate::chrome::Badge::None => "",
                        crate::chrome::Badge::Running => "running",
                        crate::chrome::Badge::Passed => "passed",
                        crate::chrome::Badge::Failed => "failed",
                        crate::chrome::Badge::Warning => "warning",
                        crate::chrome::Badge::Stopped => "stopped",
                    };
                    (t.label, badge)
                })
                .collect();
            let front = self.slot().map(|f| super::slot_name(f).to_string());
            p.slot = Some(SlotPanel { tabs, front, keys: self.bottom_has_keys() });
        }
        if let Some(c) = self.session.chat.as_ref() {
            p.chat = Some(Chat {
                focused: self.session.is_at(crate::session::At::Chat),
                minimized: c.compact(),
                busy: c.busy(),
                queued: c.queued.is_some() || !c.sends.is_empty(),
                queued_said: c.queued_said().map(|q| q.0),
                draft: c.input.text(),
                follow: c.follow.on,
                paused: c.follow.paused,
            });
        }
        p
    }

    /// Every session's runs: in the task panel, in the test pane, waiting in line.
    pub fn ui_runs(&self) -> Vec<Run> {
        let now = std::time::Instant::now();
        let ms = |d: std::time::Duration| d.as_millis() as u64;
        let mut out = Vec::new();
        for l in std::iter::once(&self.session).chain(&self.sessions) {
            let session = super::session_id(l.id);
            if let Some(r) = &l.task_run {
                use crate::task::State as S;
                let preflight = l.preflight.as_ref().is_some_and(|p| p.generation == r.generation());
                let (of, kind) = match () {
                    _ if r.by_claude => ("claude", "task"),
                    _ if r.install => ("tarae", "install"),
                    _ if r.git => ("tarae", "git"),
                    _ if preflight => ("user", "preflight"),
                    _ => ("user", "task"),
                };
                let waiting = r.wait.shown(now);
                // (held from the stuck card: its own time limit to come, or its work maybe elsewhere — cardburden 5)
                let away = crate::stuck::away(r.guard.as_ref());
                let quiet =
                    r.by_claude && crate::stuck::held(&r.wait, r.last_output, now, away, &self.config.llm);
                let state = match &r.state {
                    S::Running if waiting.is_some() => "waiting".to_string(),
                    S::Running if quiet => "quiet".into(),
                    S::Running => "running".into(),
                    S::Passed => "passed".into(),
                    S::Failed(_) => "failed".into(),
                    S::Stopped(why) => format!("stopped · {why}"),
                };
                out.push(Run {
                    id: format!("run#{}", r.trace.id()),
                    session: session.clone(),
                    of,
                    kind,
                    label: if r.by_claude { crate::task::traced(&r.task.cmd) } else { r.task.name.clone() },
                    state,
                    waiting,
                    silent: (r.state == S::Running).then(|| r.wait.silent(r.last_output, now)).flatten(),
                    since_ms: ms(now.saturating_duration_since(r.started)),
                    took_ms: r.took.map(ms),
                    ran: r.ran.clone(),
                });
            }
            if let Some(r) = &l.test_run {
                use crate::testing::RunState as S;
                let waiting = r.wait.shown(now);
                let away = crate::stuck::away(r.guard.as_ref());
                let quiet =
                    r.by_claude && crate::stuck::held(&r.wait, r.last_output, now, away, &self.config.llm);
                let state = match &r.state {
                    S::Running if waiting.is_some() => "waiting",
                    S::Running if quiet => "quiet",
                    S::Running => "running",
                    S::Passed => "passed",
                    S::Failed(_) => "failed",
                    S::NoTests => "no tests",
                };
                out.push(Run {
                    id: format!("run#{}", r.trace.id()),
                    session: session.clone(),
                    of: if r.by_claude { "claude" } else { "user" },
                    kind: "tests",
                    label: r.target.label.clone(),
                    state: state.into(),
                    waiting,
                    silent: (r.state == S::Running).then(|| r.wait.silent(r.last_output, now)).flatten(),
                    since_ms: ms(now.saturating_duration_since(r.started)),
                    took_ms: r.took.map(ms),
                    ran: None,
                });
            }
            for slot in [crate::queued::Slot::Task, crate::queued::Slot::Tests] {
                let line = l.runs_next.iter().filter(|n| n.what.slot() == slot);
                for (pos, n) in line.enumerate() {
                    let claudes = matches!(
                        n.what,
                        crate::queued::What::Guarded { who: crate::tools::For::Claude(_), .. }
                    );
                    out.push(Run {
                        id: format!("run#{}", n.id),
                        session: session.clone(),
                        of: if claudes { "claude" } else { "user" },
                        kind: if slot == crate::queued::Slot::Task { "task" } else { "tests" },
                        label: n.what.label(),
                        state: "queued".into(),
                        waiting: Some(crate::queued::ordinal(pos + 1)),
                        silent: None,
                        since_ms: ms(now.saturating_duration_since(n.queued.since)),
                        took_ms: None,
                        ran: None,
                    });
                }
            }
        }
        out
    }
}

fn rows_json(rows: &[Row]) -> Vec<Value> {
    rows.iter()
        .map(|r| {
            let mut v = json!({ "id": r.id, "label": r.label, "selected": r.selected });
            // (a review's file row: viewed, and the file in hand — its selected row)
            if let Some(viewed) = r.viewed {
                (v["viewed"], v["in_hand"]) = (json!(viewed), json!(r.selected));
            }
            v
        })
        .collect()
}

impl Panels {
    pub fn json(&self) -> Value {
        let code: Vec<Value> = self
            .code
            .iter()
            .map(|p| {
                json!({ "id": p.id, "path": p.path, "lines": [p.lines.0, p.lines.1], "focused": p.focused,
                        "cursor_line": p.cursor_line, "version": p.version })
            })
            .collect();
        json!({
            "covered_by_picker": self.covered,
            "code": code,
            "side": self.side.as_ref().map(|(tab, focused, rows, total)| {
                json!({ "tab": tab, "focused": focused, "rows": rows_json(rows), "total": total })
            }),
            "review": self.review.as_ref().map(|r| json!({
                "base": r.base, "files": r.files, "viewed": r.viewed, "file": r.file, "focused": r.focused,
                "rows": rows_json(&r.rows), "high": r.high,
                "reviewer": r.reviewer.map(|(state, t)| json!({ "state": state, "findings": t.findings,
                    "high": t.high, "answered": t.answered })),
            })),
            "edit_review": self.edit_review.as_ref().map(|r| json!({
                "id": r.id, "file": r.file, "change": [r.change.0, r.change.1], "undecided": r.undecided,
                "reason": r.reason, "warnings": r.warnings,
            })),
            "picker": self.picker.as_ref().map(|p| json!({
                "title": p.title, "query": p.query, "rows": rows_json(&p.rows), "total": p.total,
                "loading": p.loading,
            })),
            "slot": self.slot.as_ref().map(|s| json!({
                "tabs": s.tabs.iter().map(|(l, b)| json!({ "label": l, "badge": b })).collect::<Vec<_>>(),
                "front": s.front, "keys": s.keys,
            })),
            "chat": self.chat.as_ref().map(|c| json!({
                "focused": c.focused, "minimized": c.minimized, "busy": c.busy, "queued": c.queued,
                "draft": c.draft, "follow": c.follow, "paused": c.paused,
            })),
        })
    }

    /// Its lines in the text form: one a panel that's there.
    pub fn text(&self, out: &mut Vec<String>) {
        let covered = self.covered;
        let rows = |rows: &[Row]| {
            if covered {
                return "covered by the picker".to_string();
            }
            let sel = rows.iter().find(|r| r.selected).map(|r| format!(" · in hand {}", r.id));
            format!("{} rows on screen{}", rows.len(), sel.unwrap_or_default())
        };
        if let Some((tab, focused, r, total)) = &self.side {
            let keys = if *focused { " (keys)" } else { "" };
            out.push(format!("side {tab}{keys} · {total} rows · {}", rows(r)));
        }
        if let Some(r) = &self.review {
            let file = r.file.as_deref().map(|f| format!(" · in hand {f}")).unwrap_or_default();
            let high = match r.high.len() {
                0 => String::new(),
                n => format!(" · {n} high-risk to look at"),
            };
            let by = r
                .reviewer
                .map(|(s, t)| format!(" · reviewer {s}, {} findings", t.findings))
                .unwrap_or_default();
            out.push(format!("review {} · {} files, {} viewed{high}{by}{file}", r.base, r.files, r.viewed));
        }
        if let Some(r) = &self.edit_review {
            out.push(format!(
                "edit review #{} · {} · change {}/{} · {} undecided",
                r.id, r.file, r.change.0, r.change.1, r.undecided
            ));
        }
        if let Some(p) = &self.picker {
            let loading = if p.loading { " · loading" } else { "" };
            out.push(format!("picker {:?} · {} match{loading} · {}", p.title, p.total, rows(&p.rows)));
        }
        if let Some(s) = &self.slot {
            let tabs: Vec<String> = s
                .tabs
                .iter()
                .map(|(l, b)| if b.is_empty() { l.clone() } else { format!("{l} {b}") })
                .collect();
            let keys = if s.keys { " (keys)" } else { "" };
            out.push(format!("slot {}{keys} · {}", s.front.as_deref().unwrap_or("-"), tabs.join(", ")));
        }
        if let Some(c) = &self.chat {
            let mut s = vec!["chat".to_string()];
            s.extend(c.minimized.then(|| "minimized".to_string()));
            s.extend(c.busy.then(|| "working".to_string()));
            s.extend(c.queued_said.clone());
            s.extend((!c.draft.is_empty()).then(|| format!("draft of {} chars", c.draft.chars().count())));
            out.push(s.join(" · "));
        }
    }
}

impl Run {
    pub fn json(&self) -> Value {
        json!({
            "id": self.id, "session": self.session, "of": self.of, "kind": self.kind, "label": self.label,
            "state": self.state, "waiting": self.waiting, "silent": self.silent, "since_ms": self.since_ms,
            "took_ms": self.took_ms,
            "ran": self.ran.as_ref().map(|r| json!({
                "where": r.mark, "past": r.past, "why": r.why,
                "warned": r.warned.iter().map(|w| json!({ "id": w.id, "said": w.said, "whose": w.whose }))
                    .collect::<Vec<_>>(),
            })),
        })
    }

    pub fn text(&self) -> String {
        let waiting = self.waiting.iter().chain(&self.silent).map(|w| format!(" · {w}")).collect::<String>();
        let ran = self.ran.as_ref().map(|r| format!(" · in {}", r.mark)).unwrap_or_default();
        format!("{} {} · {} · {}{waiting}{ran}", self.id, self.of, self.label, self.state)
    }
}
