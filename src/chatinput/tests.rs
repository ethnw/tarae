//! The chat's input as a buffer: insert mode's keys on its document, `[chat]` over them, the chat's own first.

use super::*;
use crate::config::Config;

/// An editor with the chat up and its input with the keys, the code holding `code`.
fn editor(code: &str) -> Editor {
    let mut ed = Editor::new(Config::default());
    ed.docs[0].text = ropey::Rope::from_str(code);
    ed.session.chat = Some(crate::chat::Chat::new());
    ed.session.at = crate::session::At::Chat;
    ed
}

/// Keys as `run` takes them (`<C-w>`, `<A-ret>`).
fn feed(ed: &mut Editor, keys: &str) {
    let mut chars = keys.chars();
    while let Some(c) = chars.next() {
        let key: Key = match c {
            '<' => chars.by_ref().take_while(|&c| c != '>').collect::<String>().parse().unwrap(),
            c => Key::plain(Code::Char(c)),
        };
        ed.handle_key(key);
        assert_eq!(ed.doc().id, ed.docs[0].id, "after {key}: the code's document is current");
    }
}

/// What's typed, and the cursor.
fn input(ed: &Editor) -> (String, usize) {
    let i = &ed.session.chat.as_ref().unwrap().input;
    (i.text(), i.cursor())
}

/// Insert mode's keys edit it: digits are text (no counts), brackets and quotes as typed (no auto-pairs — it's
/// prose), `C-w` a word, `A-ret` a new line, `↑` a line up, `C-a`/`C-e` a line's ends, `C-k` to its end — the
/// code's document and mode untouched.
#[test]
fn the_input_takes_insert_modes_keys() {
    let mut ed = editor("fn main() {}\n");
    feed(&mut ed, "call f(\"x");
    assert_eq!(input(&ed).0, "call f(\"x", "nothing closed for you");
    feed(&mut ed, "\") 42");
    assert_eq!(input(&ed).0, "call f(\"x\") 42", "typed as it is");
    feed(&mut ed, "<C-w><backspace>");
    assert_eq!(input(&ed).0, "call f(\"x\")");
    feed(&mut ed, "<A-ret>second");
    assert_eq!(input(&ed), ("call f(\"x\")\nsecond".to_string(), 18));
    feed(&mut ed, "<up><C-a>!");
    assert_eq!(input(&ed).0, "!call f(\"x\")\nsecond");
    feed(&mut ed, "<C-e>;<down><C-a><C-k>");
    assert_eq!(input(&ed), ("!call f(\"x\");\n".to_string(), 14));
    assert_eq!(ed.doc().text.to_string(), "fn main() {}\n", "the code untouched");
    assert_eq!(ed.mode, Mode::Normal, "the code's mode untouched");
    assert_eq!(ed.keys_to().region.name(), "chat");
}

/// A sequence of `[insert]`'s under way (Helix's `j k` escape): its `j` waits, said on the status line; `enter`
/// sends the message with it, never leaves it for the next one; `j` then `k` runs what it's bound to.
#[test]
fn an_insert_sequence_under_way_goes_with_the_message() {
    let (config, w) = crate::config::parse("[keys.insert]\nj = { k = \"normal_mode\" }\n");
    assert!(w.is_empty(), "{w:?}");
    let mut ed = Editor::new(config);
    (ed.config.llm.command, ed.config.llm.args) = ("sh".into(), vec!["-c".into(), "cat > /dev/null".into()]);
    ed.session.chat = Some(crate::chat::Chat::new());
    ed.session.at = crate::session::At::Chat;
    feed(&mut ed, "hej");
    assert_eq!(input(&ed).0, "he", "the j waits for the next key");
    assert_eq!(ed.pending_display(), "j", "and the status line says it");
    feed(&mut ed, "<ret>");
    let c = ed.session.chat.as_ref().unwrap();
    assert!(
        c.msgs.iter().any(|m| m.text == "hej"),
        "sent with its j: {:?}",
        c.msgs.iter().map(|m| &m.text).collect::<Vec<_>>()
    );
    feed(&mut ed, "x");
    assert_eq!(input(&ed).0, "x", "no j left for the next message");
    assert_eq!(ed.pending_display(), "");
    // (a sequence broken by a key of the chat's own is typed too, before it: `j` then `C-a`)
    feed(&mut ed, "j<C-a>");
    assert_eq!(input(&ed), ("xj".to_string(), 0));
    // (and by a paste: the `j` goes in first, the pasted text after it — nothing left for the next key)
    feed(&mut ed, "<C-e>j");
    ed.handle_paste("X".into());
    assert_eq!(input(&ed).0, "xjjX");
    assert_eq!(ed.pending_display(), "");
    feed(&mut ed, "y");
    assert_eq!(input(&ed).0, "xjjXy");
    // (the input's text replaced — a message recalled, sent, cleared: the held `j` goes with it)
    feed(&mut ed, "j");
    ed.session.chat.as_mut().unwrap().input.set("recalled");
    feed(&mut ed, "!");
    assert_eq!(input(&ed).0, "recalled!");
}

/// A paste as the terminal hands it in (`Event::Paste`) while an `[insert]` sequence waits: its `j` goes in first,
/// the pasted text after it (`hej` + `X` → `hejX`, never `heXj`), and the sequence is over — a `k` next is typed.
#[test]
fn a_paste_breaks_off_an_insert_sequence_under_way() {
    let (config, w) = crate::config::parse("[keys.insert]\nj = { k = \"normal_mode\" }\n");
    assert!(w.is_empty(), "{w:?}");
    let mut ed = Editor::new(config);
    ed.session.chat = Some(crate::chat::Chat::new());
    ed.session.at = crate::session::At::Chat;
    feed(&mut ed, "hej");
    assert_eq!(ed.pending_display(), "j");
    ed.handle_event(crate::event::Event::Paste("X".into()));
    assert_eq!(input(&ed), ("hejX".to_string(), 4));
    assert_eq!(ed.pending_display(), "", "nothing left waiting");
    feed(&mut ed, "k");
    assert_eq!(input(&ed).0, "hejXk", "a k after it is text");
    assert_eq!(ed.keys_to().region.name(), "chat", "still the input's keys");
}

/// A `C-`/`A-` key nothing binds is said with where `esc` goes from here: the code while there's nothing to
/// read, normal mode on the conversation once there is (as the input's hint says it).
#[test]
fn an_unused_key_says_where_esc_goes() {
    let mut ed = editor("fn main() {}\n");
    let said = |ed: &Editor| ed.toasts.last().map(|t| t.text.clone()).unwrap_or_default();
    feed(&mut ed, "<A-z>");
    assert!(said(&ed).ends_with("esc code"), "{}", said(&ed));
    let c = ed.session.chat.as_mut().unwrap();
    c.msgs.push(crate::chat::Msg::new(crate::chat::Role::User, "a question", ""));
    feed(&mut ed, "<A-z>");
    assert!(said(&ed).ends_with("esc normal"), "{}", said(&ed));
}

/// What the code's `.` repeats stays the code's last insert — never what was typed in the chat.
#[test]
fn the_codes_repeat_stays_the_codes() {
    let mut ed = editor("abc\n");
    ed.session.at = crate::session::At::Code;
    feed(&mut ed, "ihey<esc>");
    ed.session.at = crate::session::At::Chat;
    feed(&mut ed, "zzz");
    ed.session.at = crate::session::At::Code;
    feed(&mut ed, ".");
    let code = ed.doc().text.to_string();
    assert_eq!(code.matches("hey").count(), 2, "{code:?}");
    assert!(!code.contains('z'), "{code:?}");
    assert_eq!(input(&ed).0, "zzz");
}
