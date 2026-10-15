//! Diagrams of the code — state machines and flows whose nodes and edges are tied to it (`target/sb/diagram-plan.md`;
//! the drawing, the view and Claude's `diagram` tool come later). This is the model: what a diagram holds, where each
//! of its anchors is in the code now, and whether that code is still what it was when the diagram was drawn — told
//! as notes tell their lines changed (`notes::lines_digest`).
//!
//! An anchor is written as the read tool takes a part of a file (`readtool::item`), from the project's root:
//! `src/rebase.rs:Editor::rebase_run` — a name in tarae's outline: a function, a type, a module, an enum's variant
//! (`Stage::Stopped`) — or `src/rebase.rs:684-700`, lines from 1. When it's drawn it's pinned (`Anchor::pin`): its
//! lines and a digest of their text, and their first line's text to find lines the file moved.

#![cfg_attr(
    not(test),
    expect(dead_code, reason = "the diagram plan's P2 — the view (P4) and the tool (P5) use it")
)]

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use ropey::Rope;

use crate::nav::Sym;

pub mod layout;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    State,
    Flow,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Diagram {
    pub name: String,
    pub title: String,
    pub kind: Kind,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub id: String,
    pub label: String,
    /// The composite it's in (a node's id) — None: the diagram's top level.
    pub parent: Option<String>,
    /// Which of its parent's regions it's in, from 0 — regions run at the same time (Mermaid's `--`).
    pub region: usize,
    pub at: Option<Anchor>,
}

/// A transition (a flow's arrow). An end that's None is Mermaid's `[*]` of `within`: as `from` its start, as
/// `to` its end.
#[derive(Clone, Debug, PartialEq)]
pub struct Edge {
    pub from: Option<String>,
    pub to: Option<String>,
    /// The composite whose start or end a None end is — None: the diagram's.
    pub within: Option<String>,
    pub label: String,
    /// Into the machine's next run, not this one (drawn `↑ name · next run`).
    pub next_run: bool,
    pub at: Option<Anchor>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Anchor {
    /// As written: `src/rebase.rs:Stage::Stopped`.
    pub given: String,
    /// From the project's root.
    pub path: PathBuf,
    pub want: Want,
    /// Where it was and what its text was when drawn — None until then.
    pub pin: Option<Pin>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Want {
    /// A name in tarae's outline: `name`, `Type::name` (`outlines::named`).
    Symbol(String),
    /// Lines, from 0, the end excluded.
    Lines(Range<usize>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Pin {
    pub lines: Range<usize>,
    pub digest: [u8; 32],
    /// Its first line's text, trimmed (`notes::nearest_line`).
    pub first: String,
}

/// What an anchor's code is now.
#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    /// There, as drawn — or found, not pinned yet.
    Same,
    /// The same text, elsewhere in its file.
    Moved,
    /// Its text changed since it was drawn.
    Changed,
    /// There when it was drawn, not now.
    Gone,
    /// Never found: why.
    Unresolved(String),
    /// That many symbols have its name — the first is taken.
    Ambiguous(usize),
}

impl Status {
    /// Not the code it was drawn on (or none): marked on the drawing, told to Claude.
    pub fn stale(&self) -> bool {
        !matches!(self, Status::Same | Status::Moved)
    }
}

/// An anchor checked: how it is, and its lines now when it's found.
#[derive(Clone, Debug, PartialEq)]
pub struct Check {
    pub status: Status,
    pub lines: Option<Range<usize>>,
}

impl Check {
    fn at(status: Status, lines: Range<usize>) -> Check {
        Check { status, lines: Some(lines) }
    }

    fn nowhere(status: Status) -> Check {
        Check { status, lines: None }
    }
}

impl Anchor {
    /// `path:Symbol` or `path:a-b`, the path inside the project, from its root.
    pub fn parse(spec: &str) -> Result<Anchor, String> {
        let it = crate::readtool::item(spec);
        let want = match it.want {
            crate::readtool::Want::Symbol(name) => Want::Symbol(name),
            crate::readtool::Want::Lines(r) if r.len() == 1 => Want::Lines(r[0].0 - 1..r[0].1),
            _ => return Err(format!("`{}`: an anchor is `path:Symbol` or `path:a-b`", it.given)),
        };
        let path = PathBuf::from(&it.path);
        if !path.components().all(|c| matches!(c, Component::Normal(_) | Component::CurDir)) {
            return Err(format!("`{}`: its path is from the project's root, inside it", it.given));
        }
        Ok(Anchor { given: it.given, path, want, pin: None })
    }

    /// Where it is in `text` — its file's text now; `syms`, that text's outline (`outlines::of_text`) — and how
    /// that compares with its pin.
    pub fn check(&self, text: &Rope, syms: Result<&[Sym], &str>) -> Check {
        self.check_with(text, syms, self.pin.as_ref())
    }

    /// Pinned as it is in `text` now: what later checks compare with. Its check before it was pinned.
    pub fn pin(&mut self, text: &Rope, syms: Result<&[Sym], &str>) -> Check {
        let check = self.check_with(text, syms, None);
        self.pin = check.lines.clone().and_then(|lines| {
            let digest = crate::notes::lines_digest(text, &lines)?;
            Some(Pin { first: text.line(lines.start).to_string().trim().into(), digest, lines })
        });
        check
    }

    fn check_with(&self, text: &Rope, syms: Result<&[Sym], &str>, pin: Option<&Pin>) -> Check {
        let n = text.len_lines();
        let lines = match &self.want {
            Want::Symbol(name) => {
                let syms = match syms {
                    Ok(s) => s,
                    Err(e) => return Check::nowhere(Status::Unresolved(e.into())),
                };
                let found = crate::outlines::named(syms, name);
                let Some(s) = found.first() else {
                    return Check::nowhere(match pin {
                        Some(_) => Status::Gone,
                        None => Status::Unresolved(format!("no symbol named {name}")),
                    });
                };
                let lines = s.lines.0..s.lines.1 + 1;
                if found.len() > 1 {
                    return Check::at(Status::Ambiguous(found.len()), lines);
                }
                lines
            }
            Want::Lines(r) => {
                let Some(pin) = pin else {
                    return match r.start < n {
                        true => Check::at(Status::Same, r.start..r.end.min(n)),
                        false => Check::nowhere(Status::Unresolved(format!("past its end ({n} lines)"))),
                    };
                };
                // (its first line, near where it was)
                let shift = crate::notes::nearest_line(text, pin.lines.start, &pin.first);
                let moved =
                    pin.lines.start.saturating_add_signed(shift)..pin.lines.end.saturating_add_signed(shift);
                return match crate::notes::lines_digest(text, &moved) == Some(pin.digest) {
                    true if shift == 0 => Check::at(Status::Same, moved),
                    true => Check::at(Status::Moved, moved),
                    false if moved.start < n => Check::at(Status::Changed, moved.start..moved.end.min(n)),
                    false => Check::nowhere(Status::Gone),
                };
            }
        };
        let Some(pin) = pin else { return Check::at(Status::Same, lines) };
        let status = match crate::notes::lines_digest(text, &lines) == Some(pin.digest) {
            true if lines == pin.lines => Status::Same,
            true => Status::Moved,
            false => Status::Changed,
        };
        Check::at(status, lines)
    }
}

/// What a label draws as: its gist, the text before " — " — the rest shows when its node is selected. Labels are
/// written gist first: "changes — in no commit, or unsaved buffers" draws as "changes".
pub fn gist(label: &str) -> &str {
    label.split_once(" — ").map_or(label, |(g, _)| g).trim()
}

/// An anchor's place in its diagram.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Of {
    Node(usize),
    Edge(usize),
}

/// A file the anchors are in: its text as read, and that text's outline — None: it couldn't be read.
type File = Option<(Rope, Result<Arc<Vec<Sym>>, String>)>;

impl Diagram {
    pub fn anchors(&self) -> impl Iterator<Item = (Of, &Anchor)> {
        let nodes = self.nodes.iter().enumerate().filter_map(|(i, n)| Some((Of::Node(i), n.at.as_ref()?)));
        let edges = self.edges.iter().enumerate().filter_map(|(i, e)| Some((Of::Edge(i), e.at.as_ref()?)));
        nodes.chain(edges)
    }

    fn anchors_mut(&mut self) -> impl Iterator<Item = (Of, &mut Anchor)> {
        let nodes =
            self.nodes.iter_mut().enumerate().filter_map(|(i, n)| Some((Of::Node(i), n.at.as_mut()?)));
        let edges =
            self.edges.iter_mut().enumerate().filter_map(|(i, e)| Some((Of::Edge(i), e.at.as_mut()?)));
        nodes.chain(edges)
    }

    /// What edge `e` says on the drawing: its label's gist — nothing when that only repeats the name of the
    /// node it goes to (running → done draws `├─▶ ● done · bring card`, not `├─done─▶ ● done · bring card`).
    pub fn says<'a>(&self, e: &'a Edge) -> &'a str {
        let g = gist(&e.label);
        let to = e.to.as_deref().and_then(|id| self.nodes.iter().find(|n| n.id == id));
        let repeats = to
            .and_then(|n| n.label.strip_prefix(g))
            .is_some_and(|rest| !rest.starts_with(char::is_alphanumeric));
        if repeats { "" } else { g }
    }

    /// What's wrong with its shape: a node twice, a parent or an edge's end that's no node, a composite inside
    /// itself, an edge from a start to an end.
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut ids: HashMap<&str, usize> = HashMap::new();
        for (i, n) in self.nodes.iter().enumerate() {
            if ids.insert(n.id.as_str(), i).is_some() {
                out.push(format!("{}: two nodes have this id", n.id));
            }
        }
        let known = |id: &Option<String>| id.as_deref().is_none_or(|id| ids.contains_key(id));
        for n in &self.nodes {
            if !known(&n.parent) {
                out.push(format!(
                    "{}: its parent {} is no node",
                    n.id,
                    n.parent.as_deref().unwrap_or_default()
                ));
                continue;
            }
            // (up its parents: back to itself is a loop — at most as many steps as there are nodes)
            let mut up = n.parent.as_deref();
            for _ in 0..self.nodes.len() {
                let Some(p) = up else { break };
                if p == n.id {
                    out.push(format!("{}: it's inside itself", n.id));
                    break;
                }
                up = ids.get(p).and_then(|&i| self.nodes[i].parent.as_deref());
            }
        }
        for e in &self.edges {
            let name = |end: &Option<String>| end.as_deref().unwrap_or("[*]").to_string();
            let said = format!("{} → {}", name(&e.from), name(&e.to));
            for end in [&e.from, &e.to, &e.within] {
                if !known(end) {
                    out.push(format!("{said}: {} is no node", end.as_deref().unwrap_or_default()));
                }
            }
            if e.from.is_none() && e.to.is_none() {
                out.push(format!("{said}: from a start straight to an end"));
            }
        }
        out
    }

    /// Every anchor checked against its file as `read` gives it — an open buffer's text, else the file's (the
    /// caller's, on a worker: it parses each file); `root`, the project's.
    pub fn check(&self, root: &Path, read: impl Fn(&Path) -> Option<String>) -> Vec<(Of, Check)> {
        let files = files(self.anchors().map(|(_, a)| a.path.as_path()), root, read);
        self.anchors().map(|(of, a)| (of, in_file(a, &files, |a, text, syms| a.check(text, syms)))).collect()
    }

    /// Every anchor pinned as its code is now (`check`'s `read` and `root`) — the diagram is drawn. Their checks
    /// before.
    pub fn pin(&mut self, root: &Path, read: impl Fn(&Path) -> Option<String>) -> Vec<(Of, Check)> {
        let paths: Vec<PathBuf> = self.anchors().map(|(_, a)| a.path.clone()).collect();
        let files = files(paths.iter().map(PathBuf::as_path), root, read);
        self.anchors_mut()
            .map(|(of, a)| (of, in_file(a, &files, |a, text, syms| a.pin(text, syms))))
            .collect()
    }
}

/// The anchors' files, each read and parsed once.
fn files<'a>(
    paths: impl Iterator<Item = &'a Path>,
    root: &Path,
    read: impl Fn(&Path) -> Option<String>,
) -> HashMap<PathBuf, File> {
    let mut out = HashMap::new();
    for p in paths {
        if out.contains_key(p) {
            continue;
        }
        let full = root.join(p);
        let file = read(&full).map(|text| {
            let syms = crate::outlines::of_text(&full, None, &text);
            (Rope::from_str(&text), syms)
        });
        out.insert(p.to_path_buf(), file);
    }
    out
}

/// `then` on an anchor, its file's text and outline — or, its file not read, gone (it was there when pinned) or
/// unresolved.
fn in_file<A: std::ops::Deref<Target = Anchor>>(
    a: A,
    files: &HashMap<PathBuf, File>,
    then: impl FnOnce(A, &Rope, Result<&[Sym], &str>) -> Check,
) -> Check {
    match files.get(&a.path).and_then(Option::as_ref) {
        Some((text, syms)) => then(a, text, syms.as_ref().map(|s| s.as_slice()).map_err(String::as_str)),
        None if a.pin.is_some() => Check::nowhere(Status::Gone),
        None => Check::nowhere(Status::Unresolved(format!("no file {} to read", a.path.display()))),
    }
}

#[cfg(test)]
mod tests;
