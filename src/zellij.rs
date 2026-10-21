//! Inside zellij (uxaudit 7): its normal mode takes some `C-` keys before tarae sees them — by default zellij's own
//! (`DEFAULT_KEYS`: lock, quit, its modes, session on `C-o`); a config of your own names its own letters in
//! `editor.zellij-keys` (session on `C-y`: `y` for `o` — the user, 2026-10-21), and its unlock-first preset is
//! `editor.zellij-hints = off`. Where tarae hints one of those it says
//! the key's twin instead — the key or command that does the same without the modifier zellij eats — only while
//! tarae runs inside zellij (`ZELLIJ` set: zellij sets it in its panes, `ZELLIJ=0` in the first session). The
//! bindings never change; a key with no twin is said as it is — zellij's `C-g` locks it, and then every key is
//! tarae's (the tutor says so).

use crate::editor::{Editor, Mode};
use crate::key::{Code, Key};
use crate::keymap::Node;
use crate::uistate::Region;
use crate::view::Surface;

/// The letters zellij's default normal mode takes with `C-`: `g` lock, `q` quit, its modes `p` pane, `n` resize, `s`
/// scroll, `t` tab, `h` move, `o` session, `b` tmux — `editor.zellij-keys`' default (a config of your own names
/// its own: one with session on `C-y` says `y` for `o`). Everything that names or checks them reads the setting
/// (`Editor::zellij_letters`); the generated keymap reference says these.
pub const DEFAULT_KEYS: [char; 9] = ['g', 'q', 'p', 'n', 's', 't', 'h', 'o', 'b'];

/// Whether tarae runs inside zellij — read once at the start; never in tests (they set `Editor.zellij`).
pub fn inside() -> bool {
    !cfg!(test) && std::env::var_os("ZELLIJ").is_some()
}

/// Whether zellij takes `key` before tarae sees it, zellij taking `letters` with `C-`.
pub fn takes(key: &Key, letters: &[char]) -> bool {
    key.ctrl && !key.alt && matches!(key.code, Code::Char(c) if letters.contains(&c))
}

/// `letters` as the hints name them: `C-g C-q …`, each in `` ` ``s when `ticks`.
pub fn said(letters: &[char], ticks: bool) -> String {
    let tick = if ticks { "`" } else { "" };
    letters.iter().map(|c| format!("{tick}C-{c}{tick}")).collect::<Vec<_>>().join(" ")
}

/// What a `C-q` said reaches: where it goes now (`Editor::next_up`), or — a toast about a session behind
/// (`c_q_says`) — a card or an edit the inbox lists, or a session whose turn ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reach {
    Next,
    Inbox,
    Sessions,
}

impl Editor {
    /// The letters zellij takes with `C-` (`editor.zellij-keys`).
    pub fn zellij_letters(&self) -> &[char] {
        &self.config.zellij_keys
    }

    /// Whether the twins are said and zellij takes `key` (as written: `C-q`).
    pub fn twinned(&self, key: &str) -> bool {
        self.twins_said() && key.parse::<Key>().is_ok_and(|k| takes(&k, self.zellij_letters()))
    }

    /// The hints say the twins: `editor.zellij-hints` — `auto` while tarae runs inside zellij (`Editor.zellij`),
    /// `on` and `off` whatever runs it (zellij's unlock-first preset, keys of your own: `off`).
    pub fn twins_said(&self) -> bool {
        match self.config.zellij_hints.as_str() {
            "on" => true,
            "off" => false,
            _ => self.zellij,
        }
    }

    /// `key` as a hint says it now: where the twins are said (`twins_said`) one zellij takes is said by its twin
    /// (`twin`), else as it is.
    pub fn key_said(&self, key: &'static str) -> String {
        match self.twinned(key) {
            true => twin(self, key).unwrap_or_else(|| key.to_string()),
            false => key.to_string(),
        }
    }

    /// A hint's pair, `key` and what it does, as said now: inside zellij its twin — alone when that's a `:` command,
    /// which says what it does itself (`:chat-screen`, not `:chat-screen screen`).
    pub fn key_hint(&self, key: &'static str, what: &'static str) -> (String, &'static str) {
        let said = self.key_said(key);
        let command = said.rsplit(' ').next().is_some_and(|w| w.starts_with(':'));
        (said, if command { "" } else { what })
    }

    /// What `reach` is from the code — the inbox's key or the session list's, as bound — with no way back in it: for a
    /// line read later from wherever the keys are by then (a toast about a session behind).
    pub fn c_q_place(&self, reach: Reach) -> String {
        match reach {
            Reach::Sessions => bound_key(self, "session_list", ":session-switch"),
            Reach::Inbox | Reach::Next => inbox(self),
        }
    }

    /// `C-q` as said once the keys are back in the code's normal mode — for a line that tells the way back itself
    /// (the prompt's `esc close — then …`). None: inside zellij the card in front takes them there by itself.
    pub fn c_q_from_code(&self) -> Option<String> {
        match self.twinned("C-q") {
            true => c_q_tail(self, Reach::Next),
            false => Some("C-q".into()),
        }
    }
}

/// The twin of a key tarae's own surfaces hint (the keymap's find theirs — `Keymaps::twin`): `C-q`'s is
/// `c_q_twin`'s; the chat's `C-t` `C-p` `C-s` `C-o` are its commands, typed in the code — the way back first (in
/// the chat's input `:` is a character).
fn twin(ed: &Editor, key: &str) -> Option<String> {
    let command = match key {
        "C-q" => return Some(c_q_twin(ed, Reach::Next)),
        "C-t" => ":model",
        "C-p" => ":chat-resume",
        "C-s" => ":chat-screen",
        "C-o" => ":chat-point",
        _ => return None,
    };
    Some(then(way_back(ed), command.to_string()))
}

/// `keys`, then `tail`, as typed.
fn then(mut keys: Vec<String>, tail: String) -> String {
    keys.push(tail);
    keys.join(" ")
}

/// A command's key in `node` as a hint names it (`view::bound`'s rule): `like` when that's one of them (`esc` before
/// `q`), else the shortest (a remap moves it) — `like` when none is, what tarae takes then.
fn bound(node: &Node, command: &str, like: &str) -> String {
    let keys = node.keys_for(command);
    keys.iter().find(|k| k.as_str() == like).or(keys.first()).cloned().unwrap_or_else(|| like.to_string())
}

/// The keys back to the code's normal mode from where they go now (`keys_to`), each as bound: a card holding them
/// (`C-q` gave them, `card_keys` — it swallows what isn't its answer) lets go first with its `esc`, back to what it
/// took them from; then the way from there (`from`).
fn way_back(ed: &Editor) -> Vec<String> {
    let keys = ed.keys_to();
    match keys.region {
        Region::Card(_) if ed.card_keys => {
            let mut back = vec!["esc".to_string()];
            back.extend(from(ed, keys.under.unwrap_or(Region::Code)));
            back
        }
        region => from(ed, region),
    }
}

/// The way back from `region`: from the chat's input with a conversation its view mode (`[keys.chat]`'s
/// `chat_view`), then out of it (`view_leave`); with nothing said yet or minimized that key alone goes to the code.
/// From the terminal's program its view mode (`[keys.terminal]`'s `terminal_view`), then the code (its view's
/// `view_code`). A view mode's `view_leave`; insert and select mode their `normal_mode`; one `esc` from what isn't
/// bound — keys under way, a prompt, a picker, a popup, the labels, a panel (the side panel, the review's rail, the
/// bottom slot: `esc code`). None where `space` and `:` are the editor's already — the code's normal mode, the edit
/// review, a card not holding the keys (its answers only).
fn from(ed: &Editor, region: Region) -> Vec<String> {
    let km = &ed.keymaps;
    let leave = |s: Surface| bound(km.view(s, Mode::Normal), "view_leave", "esc");
    match region {
        Region::Chat => {
            let view = bound(&km.chat, "chat_view", "esc");
            match ed.session.chat.as_ref() {
                Some(c) if !c.msgs.is_empty() && !c.compact() => vec![view, leave(Surface::Chat)],
                _ => vec![view],
            }
        }
        Region::Program => vec![
            bound(&km.terminal, "terminal_view", "C-\\"),
            bound(km.view(Surface::Terminal, Mode::Normal), "view_code", "C-\\"),
        ],
        Region::View(s) => vec![leave(s)],
        Region::Output(_) => crate::rundoc::bound(ed, "view_leave", "esc").into_iter().collect(),
        Region::Code if !ed.pending.is_empty() => vec!["esc".into()],
        Region::Code if ed.mode == Mode::Insert => vec![bound(&km.insert, "normal_mode", "esc")],
        Region::Code if ed.mode == Mode::Select => vec![bound(&km.select, "normal_mode", "esc")],
        Region::Code | Region::EditReview | Region::Card(_) | Region::NextCard => Vec::new(),
        _ => vec!["esc".into()],
    }
}

/// `C-q`'s twin. `C-q` goes to the inbox's first, any session's (`next_up`); with the keys back in the code's normal
/// mode the card in front takes them itself — so when that's where `C-q` goes (this session's, not stepped aside, no
/// edit review open, its text not the buffer in sight: `card_keys_free`'s), the way back is the twin. Anything
/// else — a card in a session behind, an edit, the edit review — is the inbox's, from the code (`space I`, `enter`
/// goes to it); a turn that ended, the session list's (`space W`); nothing waiting, the inbox too — it says nothing
/// waits, as `C-q` does. Each key as bound.
fn c_q_twin(ed: &Editor, reach: Reach) -> String {
    let back = way_back(ed);
    match c_q_tail(ed, reach) {
        Some(tail) => then(back, tail),
        // (the card in front: the way back alone — already there, the inbox)
        None if back.is_empty() => inbox(ed),
        None => back.join(" "),
    }
}

/// A command's shortest key as bound none of whose keys zellij takes (`Keymaps::untaken`) — its `:` command when
/// none is, or only keys zellij takes are.
fn bound_key(ed: &Editor, command: &str, typed: &str) -> String {
    ed.keymaps.untaken(Mode::Normal, command, ed.zellij_letters()).unwrap_or_else(|| typed.into())
}

/// The inbox's key as bound, else its command.
fn inbox(ed: &Editor) -> String {
    bound_key(ed, "inbox", ":inbox")
}

/// What follows the way back to reach what `C-q` reaches, from the code (`c_q_twin`) — None: the card in front, which
/// takes the keys there by itself.
fn c_q_tail(ed: &Editor, reach: Reach) -> Option<String> {
    use crate::inbox::Next;
    let reach = match reach {
        Reach::Next => match ed.next_up() {
            Next::Card(id)
                if ed.offers.first().is_some_and(|o| o.id == id && !o.aside)
                    && ed.session.review.is_none()
                    && !ed.offer_reading() =>
            {
                return None;
            }
            Next::Done(_) => Reach::Sessions,
            Next::Card(_) | Next::Review | Next::Go(_) | Next::None => Reach::Inbox,
        },
        r => r,
    };
    Some(match reach {
        Reach::Sessions => bound_key(ed, "session_list", ":session-switch"),
        _ => inbox(ed),
    })
}

#[cfg(test)]
mod tests;

/// `:config-show`'s last lines inside zellij: the keys it takes (`editor.zellij-keys`), every binding of the code's
/// modes among them, each with its twin.
pub fn config_lines(ed: &Editor) -> String {
    let letters = ed.zellij_letters();
    let mut out = format!(
        "\n# Inside zellij — its normal mode takes {} first (editor.zellij-keys); the same without them\n",
        said(letters, false)
    );
    for (mode, keys, cmds, twin) in ed.keymaps.taken(letters) {
        let twin = twin.unwrap_or_else(|| "none — C-g locks zellij, then it's tarae's".into());
        out.push_str(&format!("# {mode} {keys} ({cmds}): {twin}\n"));
    }
    out
}
