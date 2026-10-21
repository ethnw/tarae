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

/// Shift isn't carried separately — char keys become uppercase, Tab becomes BackTab (same as helix) — but on
/// the page keys: `S-pageup`/`S-pagedown` scroll the terminal's past while its program has the keys, as every
/// terminal emulator's do (`shift`; elsewhere they're the page keys — `Keymaps::lookup`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Key {
    pub code: Code,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Key {
    pub const fn plain(code: Code) -> Self {
        Key { code, ctrl: false, alt: false, shift: false }
    }

    /// The key without its shift (the keymap's, for a shifted page key it has no binding for).
    pub const fn unshifted(self) -> Self {
        Key { shift: false, ..self }
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
        let shift =
            matches!(code, Code::PageUp | Code::PageDown) && ev.modifiers.contains(KeyModifiers::SHIFT);
        Some(Key { code, ctrl, alt: ev.modifiers.contains(KeyModifiers::ALT), shift })
    }

    /// The char, if this is a char key without modifiers.
    pub fn plain_char(&self) -> Option<char> {
        match self.code {
            Code::Char(c) if !self.ctrl && !self.alt => Some(c),
            _ => None,
        }
    }
}

/// A move through a panel's rows — a list's selection (the side panel, the review's rail, `:procs`, a plugin's
/// panel, `:granted`) or an output's top (the test, task and terminal panels): all take the same keys for it
/// (`list_move`), the code's for the same moves (uxaudit D7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListMove {
    By(isize),
    /// The view `by` rows (`C-e`/`C-y`), the selection staying unless it would leave the rows in sight — the code's
    /// act, Helix's (`in_sight`); an output's top moves as `By`.
    Scroll(isize),
    First,
    Last,
}

/// The rows a panel not drawn yet is taken to show (a page key's measure before the first frame).
pub const UNDRAWN_ROWS: usize = 20;

/// Half of `rows` in sight, a row at least — what `C-d`/`C-u` move everywhere (the code's, Helix's).
pub fn half(rows: usize) -> isize {
    (rows / 2).max(1) as isize
}

/// `j`/`k` (↓/↑, `C-n`/`C-p`) a row, `C-e`/`C-y` the view a row (`Scroll`), `C-d`/`C-u` half the `rows` in sight,
/// `C-f`/`C-b` (the page keys) all of them, `g`/`G` (Home/End) the ends — as the code's `C-d` `C-f` `C-e` (0 rows:
/// not drawn yet, `UNDRAWN_ROWS`).
pub fn list_move(key: Key, rows: usize) -> Option<ListMove> {
    use ListMove::*;
    let rows = if rows == 0 { UNDRAWN_ROWS } else { rows };
    let page = rows as isize;
    Some(match (key.code, key.ctrl, key.alt) {
        (Code::Char('j') | Code::Down, false, false) | (Code::Char('n'), true, false) => By(1),
        (Code::Char('k') | Code::Up, false, false) | (Code::Char('p'), true, false) => By(-1),
        (Code::Char('e'), true, false) => Scroll(1),
        (Code::Char('y'), true, false) => Scroll(-1),
        (Code::Char('d'), true, false) => By(half(rows)),
        (Code::Char('u'), true, false) => By(-half(rows)),
        (Code::Char('f'), true, false) | (Code::PageDown, ..) => By(page),
        (Code::Char('b'), true, false) | (Code::PageUp, ..) => By(-page),
        (Code::Char('g') | Code::Home, false, false) => First,
        (Code::Char('G') | Code::End, false, false) => Last,
        _ => return None,
    })
}

impl ListMove {
    /// The row it lands on from `at`, in a list of `n` (kept in it) — a `Scroll` leaves it (`in_sight` moves it).
    pub fn from(self, at: usize, n: usize) -> usize {
        let last = n.saturating_sub(1);
        match self {
            ListMove::By(by) => at.saturating_add_signed(by).min(last),
            ListMove::Scroll(_) => at.min(last),
            ListMove::First => 0,
            ListMove::Last => last,
        }
    }
}

/// A list's view scrolled `by` rows from `top`, `rows` in sight of `n` (kept in the list): the new top, and the
/// selection `at` where it was — or, where it would leave the rows in sight, the nearest one in them. `hidden`: rows
/// at the top a panel covers (the side panel's pinned folders).
pub fn in_sight(by: isize, top: usize, at: usize, n: usize, rows: usize, hidden: usize) -> (usize, usize) {
    let rows = if rows == 0 { UNDRAWN_ROWS } else { rows };
    let top = top.saturating_add_signed(by).min(n.saturating_sub(rows));
    let first = (top + hidden).min(top + rows - 1);
    (top, at.clamp(first, top + rows - 1).min(n.saturating_sub(1)))
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
        let shift = shift && matches!(code, Code::PageUp | Code::PageDown);
        Ok(Key { code, ctrl, alt, shift })
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
        if self.shift {
            f.write_str("S-")?;
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

    /// Every list panel moves by the same keys — the code's measures: half the rows in sight, a page all of them —
    /// and stays in its list.
    #[test]
    fn list_panels_move_by_the_same_keys() {
        // (8 rows in sight)
        let to = |key: &str, at: usize| list_move(k(key), 8).map(|m| m.from(at, 30));
        assert_eq!((to("j", 3), to("down", 3), to("C-n", 3)), (Some(4), Some(4), Some(4)));
        assert_eq!((to("k", 3), to("up", 3), to("C-p", 3)), (Some(2), Some(2), Some(2)));
        // (`C-e`/`C-y` scroll the view: the selection stays — `in_sight` keeps it there)
        let ce = |key: &str| list_move(k(key), 8);
        assert_eq!((ce("C-e"), ce("C-y")), (Some(ListMove::Scroll(1)), Some(ListMove::Scroll(-1))));
        assert_eq!(to("C-e", 3), Some(3));
        assert_eq!((to("C-d", 3), to("C-u", 10)), (Some(7), Some(6)), "half");
        assert_eq!(
            (to("C-f", 3), to("pagedown", 3), to("C-b", 10), to("pageup", 10)),
            (Some(11), Some(11), Some(2), Some(2)),
            "a page"
        );
        assert_eq!(
            (to("g", 12), to("home", 12), to("G", 2), to("end", 2)),
            (Some(0), Some(0), Some(29), Some(29))
        );
        assert_eq!((to("j", 29), to("C-d", 27)), (Some(29), Some(29)), "kept in the list");
        assert_eq!(list_move(k("j"), 8).map(|m| m.from(0, 0)), Some(0), "an empty list");
        assert_eq!((to("A-j", 3), to("x", 3), to("space", 3)), (None, None, None), "not a move");
        let one = |key: &str, rows| list_move(k(key), rows);
        assert_eq!(
            (one("C-d", 1), one("C-d", 0)),
            (Some(ListMove::By(1)), Some(ListMove::By(10))),
            "one row; undrawn"
        );
    }

    /// A scrolled view keeps the selection where it was, unless it would leave the rows in sight — then the nearest
    /// one (under what covers the top, too); the view stays in the list.
    #[test]
    fn a_scroll_keeps_the_selection_in_sight() {
        // (30 rows, 8 in sight)
        assert_eq!(in_sight(1, 0, 4, 30, 8, 0), (1, 4), "stays");
        assert_eq!(in_sight(1, 0, 0, 30, 8, 0), (1, 1), "would leave at the top: the nearest");
        assert_eq!(in_sight(-1, 5, 12, 30, 8, 0), (4, 11), "would leave at the bottom: the nearest");
        assert_eq!(in_sight(1, 22, 25, 30, 8, 0), (22, 25), "at the end the view stays");
        assert_eq!(in_sight(-1, 0, 3, 30, 8, 0), (0, 3), "at the top too");
        assert_eq!(in_sight(1, 0, 1, 30, 8, 2), (1, 3), "under two pinned rows: below them");
        assert_eq!(in_sight(1, 0, 2, 3, 8, 0), (0, 2), "a list shorter than its rows");
    }

    /// A shifted page key carries its shift (the terminal's past, `S-pageup`) — parsed, shown, from a terminal;
    /// every other key's shift is folded in as before.
    #[test]
    fn shift_is_carried_on_the_page_keys_only() {
        assert!(k("S-pageup").shift && k("S-pagedown").shift);
        assert_eq!(k("S-pageup").to_string(), "S-pageup");
        assert_eq!(k("S-pageup").unshifted(), k("pageup"));
        assert!(!k("S-up").shift && !k("S-x").shift);
        let ev = |code, m| Key::from_event(KeyEvent::new(code, m)).unwrap();
        assert_eq!(ev(KeyCode::PageDown, KeyModifiers::SHIFT), k("S-pagedown"));
        assert_eq!(ev(KeyCode::Home, KeyModifiers::SHIFT), k("home"));
    }

    #[test]
    fn parses_helix_notation() {
        assert_eq!(k("x"), Key::plain(Code::Char('x')));
        assert_eq!(k("A-x"), Key { code: Code::Char('x'), ctrl: false, alt: true, shift: false });
        assert_eq!(k("C-y"), Key { code: Code::Char('y'), ctrl: true, alt: false, shift: false });
        assert_eq!(k("A-,"), Key { code: Code::Char(','), ctrl: false, alt: true, shift: false });
        assert_eq!(k("C--"), Key { code: Code::Char('-'), ctrl: true, alt: false, shift: false });
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
        for s in ["x", "A-x", "C-y", "space", "minus", "ret", "C-A-backspace", "F3", "S-pagedown"] {
            assert_eq!(k(s).to_string(), s);
        }
    }

    /// C-\\ comes from a terminal as 0x1c, which reads back as C-4 — it's C-\\ (the terminal's way out).
    #[test]
    fn control_backslash_from_a_terminal() {
        let ev = KeyEvent::new(KeyCode::Char('4'), KeyModifiers::CONTROL);
        assert_eq!(
            Key::from_event(ev),
            Some(Key { code: Code::Char('\\'), ctrl: true, alt: false, shift: false })
        );
        assert_eq!("C-\\".parse::<Key>().ok(), Key::from_event(ev));
    }
}
