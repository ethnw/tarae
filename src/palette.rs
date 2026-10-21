//! The palette's order with nothing typed (`space ?`): what you ran from it, newest first, then what a first
//! hour asks for (`COMMON`), then the rest as before (bound commands first, by name). Kept in
//! `$XDG_STATE_HOME/tarae/palette`, one per line, newest on top — read at the start, written on a worker.

use crate::picker::Action;
use std::io::Write as _;
use std::path::{Path, PathBuf};

const KEEP: usize = 12;

/// A first hour's commands, in the tutor's order: learn, find, the panels, Claude, the review, sessions, the
/// inbox — by their names (a `:` line as typed).
pub const COMMON: &[&str] = &[
    ":tutor",
    "file_picker",
    "global_search",
    "file_explorer",
    "buffer_picker",
    "terminal",
    "chat_open",
    "llm_ask",
    "review_start",
    "session_list",
    "inbox",
    "theme_picker",
    ":w",
    ":q",
];

/// What a row runs, as it's kept: a command's name, a `:` line with its colon.
pub fn key(action: &Action) -> Option<String> {
    match action {
        Action::Command(name) => Some(name.to_string()),
        Action::Typed(line) => Some(format!(":{line}")),
        _ => None,
    }
}

/// Where a row goes with nothing typed: what you ran (newest first), then `COMMON`, then the rest.
pub fn rank(ran: &[String], key: &str) -> (u8, usize) {
    match (ran.iter().position(|r| r == key), COMMON.iter().position(|c| *c == key)) {
        (Some(i), _) => (0, i),
        (None, Some(i)) => (1, i),
        (None, None) => (2, 0),
    }
}

/// The list with `key` on top (saved by `save`, off the main thread).
pub fn bump(ran: &[String], key: &str) -> Vec<String> {
    let mut out = vec![key.to_string()];
    out.extend(ran.iter().filter(|r| *r != key).take(KEEP - 1).cloned());
    out
}

fn file(state: &Path) -> PathBuf {
    state.join("palette")
}

pub fn load() -> Vec<String> {
    crate::config::state_dir().map(|s| load_from(&s)).unwrap_or_default()
}

fn load_from(state: &Path) -> Vec<String> {
    let body = std::fs::read_to_string(file(state)).unwrap_or_default();
    body.lines().filter(|l| !l.is_empty()).take(KEEP).map(str::to_string).collect()
}

pub fn save(ran: &[String]) {
    if let Some(s) = crate::config::state_dir() {
        save_in(&s, ran);
    }
}

fn save_in(state: &Path, ran: &[String]) {
    let _ = std::fs::create_dir_all(state);
    let body: String = ran.iter().map(|r| format!("{r}\n")).collect();
    let _ = crate::disk::write_atomic(&file(state), |w| w.write_all(body.as_bytes()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_you_ran_comes_first_then_the_common_ones() {
        let ran = bump(&bump(&[], "terminal"), ":w");
        assert_eq!(ran, [":w", "terminal"]);
        assert_eq!(bump(&ran, "terminal"), ["terminal", ":w"], "again: on top, once");
        assert!(rank(&ran, ":w") < rank(&ran, "terminal"));
        assert!(rank(&ran, "terminal") < rank(&ran, ":tutor"), "ran before the common ones");
        assert!(rank(&ran, ":tutor") < rank(&ran, "file_picker"));
        assert!(rank(&ran, ":q") < rank(&ran, "hover"), "the common ones before the rest");
        let many = (0..20).fold(Vec::new(), |l, i| bump(&l, &format!("c{i}")));
        assert_eq!(many.len(), KEEP);
    }

    /// What's saved is what the next start reads: in order, `:` lines with their spaces, at most `KEEP`.
    #[test]
    fn what_you_ran_is_read_back_as_it_was_saved() {
        let dir = crate::scratch::Scratch::new("palette");
        let state = dir.join("state");
        assert!(load_from(&state).is_empty(), "none yet");
        let ran = bump(&bump(&[], "terminal"), ":toggle editor.cursorline");
        save_in(&state, &ran);
        assert_eq!(load_from(&state), ran);
        let many: Vec<String> = (0..20).map(|i| format!("c{i}")).collect();
        save_in(&state, &many);
        assert_eq!(load_from(&state), many[..KEEP]);
    }
}
