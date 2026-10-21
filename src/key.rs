//! Key representation — 1:1 with helix config notation ("C-y", "A-x", "space", "ret").

use std::fmt;
use std::str::FromStr;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Code {
    Char(char),
    Enter,
    Esc,
    Tab,
    BackTab,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    F(u8),
}

/// Shift isn't carried separately — char keys become uppercase, Tab becomes BackTab (same as helix).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Key {
    pub code: Code,
    pub ctrl: bool,
    pub alt: bool,
}

impl Key {
    pub const fn plain(code: Code) -> Self {
        Key { code, ctrl: false, alt: false }
    }

    pub fn from_event(ev: KeyEvent) -> Option<Key> {
        let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
        let code = match ev.code {
            // A terminal sends C-\ C-] C-^ C-_ as bytes 0x1c–0x1f, which read back as C-4 … C-7
            KeyCode::Char('4') if ctrl => Code::Char('\\'),
            KeyCode::Char('5') if ctrl => Code::Char(']'),
            KeyCode::Char('6') if ctrl => Code::Char('^'),
            KeyCode::Char('7') if ctrl => Code::Char('_'),
            KeyCode::Char(c) => Code::Char(c),
            KeyCode::Enter => Code::Enter,
            KeyCode::Esc => Code::Esc,
            KeyCode::Tab => Code::Tab,
            KeyCode::BackTab => Code::BackTab,
            KeyCode::Backspace => Code::Backspace,
            KeyCode::Delete => Code::Delete,
            KeyCode::Left => Code::Left,
            KeyCode::Right => Code::Right,
            KeyCode::Up => Code::Up,
            KeyCode::Down => Code::Down,
            KeyCode::Home => Code::Home,
            KeyCode::End => Code::End,
            KeyCode::PageUp => Code::PageUp,
            KeyCode::PageDown => Code::PageDown,
            KeyCode::F(n) => Code::F(n),
            _ => return None,
        };
        Some(Key { code, ctrl, alt: ev.modifiers.contains(KeyModifiers::ALT) })
    }

    /// The char, if this is a char key without modifiers.
    pub fn plain_char(&self) -> Option<char> {
        match self.code {
            Code::Char(c) if !self.ctrl && !self.alt => Some(c),
            _ => None,
        }
    }
}

/// A move through a list panel's rows — the side panel, the review's rail, `:procs`, a plugin's panel all take
/// the same keys for it (`list_move`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListMove {
    By(isize),
    First,
    Last,
}

/// Rows a page key moves in a list panel.
const LIST_PAGE: isize = 10;

/// `j`/`k` (↓/↑, `C-n`/`C-p`) a row, `C-d`/`C-u` (the page keys) `LIST_PAGE`, `g`/`G` (Home/End) the ends.
pub fn list_move(key: Key) -> Option<ListMove> {
    use ListMove::*;
    Some(match (key.code, key.ctrl, key.alt) {
        (Code::Char('j') | Code::Down, false, false) | (Code::Char('n'), true, false) => By(1),
        (Code::Char('k') | Code::Up, false, false) | (Code::Char('p'), true, false) => By(-1),
        (Code::Char('d'), true, false) | (Code::PageDown, ..) => By(LIST_PAGE),
        (Code::Char('u'), true, false) | (Code::PageUp, ..) => By(-LIST_PAGE),
        (Code::Char('g') | Code::Home, false, false) => First,
        (Code::Char('G') | Code::End, false, false) => Last,
        _ => return None,
    })
}

impl ListMove {
    /// The row it lands on from `at`, in a list of `n` (kept in it).
    pub fn from(self, at: usize, n: usize) -> usize {
        let last = n.saturating_sub(1);
        match self {
            ListMove::By(by) => at.saturating_add_signed(by).min(last),
            ListMove::First => 0,
            ListMove::Last => last,
        }
    }
}

impl FromStr for Key {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let (mut ctrl, mut alt, mut shift) = (false, false, false);
        let mut rest = s;
        // "C-", "A-", "S-" prefixes. Also works when the key itself is '-', as in "C--".
        while rest.len() > 2 && rest.as_bytes()[1] == b'-' {
            match rest.as_bytes()[0] {
                b'C' => ctrl = true,
                b'A' => alt = true,
                b'S' => shift = true,
                _ => break,
            }
            rest = &rest[2..];
        }
        let code = match rest {
            "space" => Code::Char(' '),
            "ret" | "enter" => Code::Enter,
            "esc" => Code::Esc,
            "tab" => Code::Tab,
            "backtab" => Code::BackTab,
            "backspace" => Code::Backspace,
            "del" => Code::Delete,
            "left" => Code::Left,
            "right" => Code::Right,
            "up" => Code::Up,
            "down" => Code::Down,
            "home" => Code::Home,
            "end" => Code::End,
            "pageup" => Code::PageUp,
            "pagedown" => Code::PageDown,
            "minus" => Code::Char('-'),
            "lt" => Code::Char('<'),
            "gt" => Code::Char('>'),
            f if f.len() > 1 && f.starts_with('F') && f[1..].parse::<u8>().is_ok() => {
                Code::F(f[1..].parse().unwrap())
            }
            _ => {
                let mut it = rest.chars();
                match (it.next(), it.next()) {
                    (Some(c), None) => Code::Char(c),
                    _ => return Err(format!("unknown key: {s:?}")),
                }
            }
        };
        let code = match (code, shift) {
            (Code::Char(c), true) => Code::Char(c.to_ascii_uppercase()),
            (Code::Tab, true) => Code::BackTab,
            (c, _) => c,
        };
        Ok(Key { code, ctrl, alt })
    }
}

impl Key {
    /// The key as people type it, for what's shown (which-key, the palette, hints): as `Display` writes it for the
    /// config, but `-` as itself (`minus` is only the config's name for it, `C-w -` not `C-w minus`).
    pub fn shown(&self) -> String {
        match self.code {
            Code::Char('-') => {
                format!("{}{}-", if self.ctrl { "C-" } else { "" }, if self.alt { "A-" } else { "" })
            }
            _ => self.to_string(),
        }
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.ctrl {
            f.write_str("C-")?;
        }
        if self.alt {
            f.write_str("A-")?;
        }
        match self.code {
            Code::Char(' ') => f.write_str("space"),
            Code::Char('-') => f.write_str("minus"),
            Code::Char(c) => write!(f, "{c}"),
            Code::Enter => f.write_str("ret"),
            Code::Esc => f.write_str("esc"),
            Code::Tab => f.write_str("tab"),
            Code::BackTab => f.write_str("backtab"),
            Code::Backspace => f.write_str("backspace"),
            Code::Delete => f.write_str("del"),
            Code::Left => f.write_str("left"),
            Code::Right => f.write_str("right"),
            Code::Up => f.write_str("up"),
            Code::Down => f.write_str("down"),
            Code::Home => f.write_str("home"),
            Code::End => f.write_str("end"),
            Code::PageUp => f.write_str("pageup"),
            Code::PageDown => f.write_str("pagedown"),
            Code::F(n) => write!(f, "F{n}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(s: &str) -> Key {
        s.parse().unwrap()
    }

    /// Every list panel moves by the same keys, and stays in its list.
    #[test]
    fn list_panels_move_by_the_same_keys() {
        let to = |key: &str, at: usize| list_move(k(key)).map(|m| m.from(at, 30));
        assert_eq!((to("j", 3), to("down", 3), to("C-n", 3)), (Some(4), Some(4), Some(4)));
        assert_eq!((to("k", 3), to("up", 3), to("C-p", 3)), (Some(2), Some(2), Some(2)));
        assert_eq!(
            (to("C-d", 3), to("pagedown", 3), to("C-u", 3), to("pageup", 3)),
            (Some(13), Some(13), Some(0), Some(0))
        );
        assert_eq!(
            (to("g", 12), to("home", 12), to("G", 2), to("end", 2)),
            (Some(0), Some(0), Some(29), Some(29))
        );
        assert_eq!((to("j", 29), to("C-d", 25)), (Some(29), Some(29)), "kept in the list");
        assert_eq!(list_move(k("j")).map(|m| m.from(0, 0)), Some(0), "an empty list");
        assert_eq!((to("A-j", 3), to("x", 3), to("space", 3)), (None, None, None), "not a move");
    }

    #[test]
    fn parses_helix_notation() {
        assert_eq!(k("x"), Key::plain(Code::Char('x')));
        assert_eq!(k("A-x"), Key { code: Code::Char('x'), ctrl: false, alt: true });
        assert_eq!(k("C-y"), Key { code: Code::Char('y'), ctrl: true, alt: false });
        assert_eq!(k("A-,"), Key { code: Code::Char(','), ctrl: false, alt: true });
        assert_eq!(k("C--"), Key { code: Code::Char('-'), ctrl: true, alt: false });
        assert_eq!(k("-"), Key::plain(Code::Char('-')));
        assert_eq!(k("space"), Key::plain(Code::Char(' ')));
        assert_eq!(k("S-x"), Key::plain(Code::Char('X')));
        assert_eq!(k("S-tab"), Key::plain(Code::BackTab));
        assert_eq!(k("F12"), Key::plain(Code::F(12)));
        assert_eq!(k("ret"), Key::plain(Code::Enter));
        assert!("nope".parse::<Key>().is_err());
    }

    #[test]
    fn display_roundtrips() {
        for s in ["x", "A-x", "C-y", "space", "minus", "ret", "C-A-backspace", "F3"] {
            assert_eq!(k(s).to_string(), s);
        }
    }

    /// C-\\ comes from a terminal as 0x1c, which reads back as C-4 — it's C-\\ (the terminal's way out).
    #[test]
    fn control_backslash_from_a_terminal() {
        let ev = KeyEvent::new(KeyCode::Char('4'), KeyModifiers::CONTROL);
        assert_eq!(Key::from_event(ev), Some(Key { code: Code::Char('\\'), ctrl: true, alt: false }));
        assert_eq!("C-\\".parse::<Key>().ok(), Key::from_event(ev));
    }
}
