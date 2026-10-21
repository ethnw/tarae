//! View mode on the terminal: keys in sequence on a stand-in terminal (`terminal::stand_in` — no pty, no
//! program: what it "prints" is given to `terminal::output` as the reader would), a frame after each.

use super::*;
use crate::config::Config;
use crate::terminal::output;

const W: u16 = 120;
const H: u16 = 34;

/// An editor whose terminal has printed `lines` lines (`line 1` …) and a prompt, the program with the keys.
fn term(config: Config, lines: usize) -> (Editor, std::sync::mpsc::Receiver<Vec<u8>>) {
    let mut ed = Editor::new(config);
    ed.docs[0].text = ropey::Rope::from_str("fn main() {}\n");
    let sent = crate::terminal::stand_in(&mut ed, 10, 80, 5000);
    // (its size is the panel's from the first frame on)
    frame(&mut ed);
    let mut text: String = (1..=lines).map(|i| format!("line {i}\r\n")).collect();
    text.push_str("$ ");
    print(&mut ed, &text);
    (ed, sent)
}

/// What the program prints.
fn print(ed: &mut Editor, s: &str) {
    let generation = ed.session.terminal.as_ref().unwrap().generation();
    output(ed, generation, s.as_bytes());
    frame(ed);
}

fn frame(ed: &mut Editor) -> String {
    let mut bytes = Vec::new();
    crate::term::render(ed, &mut bytes, W, H).unwrap();
    let mut p = vt100::Parser::new(H, W, 0);
    p.process(&bytes);
    p.screen().contents()
}

/// Keys as `run` takes them (`<C-\>`, `<esc>`), a frame after each; the code's document stays current.
fn feed(ed: &mut Editor, keys: &str) {
    let code = (ed.doc().id, ed.docs.len());
    let mut chars = keys.chars();
    while let Some(c) = chars.next() {
        let key: Key = match c {
            '<' => chars.by_ref().take_while(|&c| c != '>').collect::<String>().parse().unwrap(),
            c => Key::plain(Code::Char(c)),
        };
        ed.handle_key(key);
        assert_eq!((ed.doc().id, ed.docs.len()), code, "after {key}: the pane's document is current");
        frame(ed);
    }
}

fn region(ed: &Editor) -> String {
    ed.keys_to().region.name()
}

fn view(ed: &Editor) -> &View {
    terminal_view_of(ed).expect("in the terminal's view mode")
}

/// The view's cursor line's text, and what's selected.
fn at(ed: &Editor) -> (String, String) {
    let d = view(ed).doc();
    let r = d.selection().primary();
    let line = mv::line_of(&d.text, r.cursor(&d.text));
    (d.text.line(line).to_string().trim_end().to_string(), d.text.byte_slice(r.from()..r.to()).to_string())
}

fn status(shown: &str) -> String {
    shown.lines().last().unwrap_or_default().to_string()
}

fn program(ed: &Editor) -> bool {
    ed.session.terminal.as_ref().unwrap().focus
}

/// `C-\` goes round: the program → its view mode (said on the status line and the panel's head) → the code → the
/// program; `i` from the view types again; `esc` too goes to the code.
#[test]
fn ctrl_backslash_goes_round() {
    let (mut ed, _sent) = term(Config::default(), 3);
    assert_eq!(region(&ed), "terminal-program");
    assert!(frame(&mut ed).contains("C-\\ view"), "the program's way to its view");
    feed(&mut ed, "<C-\\>");
    assert_eq!(region(&ed), "view:terminal");
    let shown = frame(&mut ed);
    assert!(status(&shown).contains("view · terminal"), "{}", status(&shown));
    assert!(status(&shown).contains("i type") && status(&shown).contains("esc code"), "{}", status(&shown));
    assert!(shown.contains("● view"), "its head says it:\n{shown}");
    assert!(!program(&ed));
    feed(&mut ed, "i");
    assert_eq!(region(&ed), "terminal-program");
    assert!(ed.session.view.is_none());
    feed(&mut ed, "<C-\\><C-\\>");
    assert_eq!(region(&ed), "code", "the view's C-\\: the code");
    assert!(ed.session.view.is_none() && !ed.bottom_focus);
    feed(&mut ed, "<C-\\>");
    assert_eq!(region(&ed), "terminal-program", "the code's C-\\: the program");
    feed(&mut ed, "<C-\\><esc>");
    assert_eq!(region(&ed), "code", "esc: the code");
}

/// Output while its `/` is open moves nothing: the view isn't read again behind it (the output waits), the search
/// goes on over the same rows, its keys the view's; `G` afterwards catches up, the view's `v` with it.
#[test]
fn a_flood_while_searching_leaves_the_search_and_the_view_as_they_are() {
    let (mut ed, _sent) = term(Config::default(), 3);
    feed(&mut ed, "<C-\\>v/lin");
    let rows = view(&ed).rows();
    print(&mut ed, &"x".repeat(256 << 10));
    assert!(ed.prompt.is_some() && view(&ed).prompt, "its search still open, the view's");
    assert_eq!(view(&ed).rows(), rows, "not read again");
    feed(&mut ed, "e 2<ret>");
    assert_eq!(at(&ed).0, "line 2", "the search over the same rows");
    assert_eq!(ed.doc().text.to_string(), "fn main() {}\n", "the code untouched");
    feed(&mut ed, "G");
    assert_eq!(view(&ed).mode(), Mode::Select, "its `v` came along as it caught up");
    assert!(!ed.session.terminal.as_ref().unwrap().came(), "caught up");
}

/// The wheel across surfaces: over the chat while the terminal is viewed, the chat's view mode — the terminal's
/// left, what it held taken in; back down to the end, the terminal's view mode again (where the keys were). And the
/// other way round: from the chat's view mode, the terminal's wheel, back to the chat's.
#[test]
fn the_wheel_crosses_surfaces_and_back_to_the_view_it_left() {
    use crate::chat::{Chat, Msg, Role};
    use crate::event::{Mouse, MouseKind as K};
    let (mut ed, _sent) = term(Config::default(), 60);
    let mut c = Chat::new();
    c.msgs = (0..40)
        .map(|i| match i % 2 {
            0 => Msg::new(Role::User, format!("question {i}"), ""),
            _ => Msg::new(Role::Assistant, format!("answer {i}"), ""),
        })
        .collect();
    ed.session.chat = Some(c);
    frame(&mut ed);
    let chat_wheel = |ed: &mut Editor, kind| {
        let (y0, ..) = ed.screen.chat_view.get().unwrap();
        let x = ed.screen.chat_x.unwrap() + 4;
        ed.handle_mouse(Mouse { kind, x: x as u16, y: (y0 + 5) as u16, alt: false });
        frame(ed);
    };
    feed(&mut ed, "<C-\\>");
    print(&mut ed, "\r\nwhile-reading-the-chat\r\n$ ");
    chat_wheel(&mut ed, K::ScrollUp);
    assert_eq!(region(&ed), "view:chat");
    let t = ed.session.terminal.as_ref().unwrap();
    assert!(!t.holding(), "the terminal's view left: nothing held");
    assert!(t.parser.screen().contents().contains("while-reading-the-chat"), "what it held taken in");
    chat_wheel(&mut ed, K::ScrollDown);
    assert_eq!(region(&ed), "view:terminal", "back at the end: the view the keys were in");
    // (the other way: the chat's view mode, the terminal's wheel, back)
    feed(&mut ed, "<esc>");
    ed.session.chat.as_mut().unwrap().focused = true;
    feed(&mut ed, "<esc>");
    assert_eq!(region(&ed), "view:chat");
    ed.terminal_mouse(K::ScrollUp, Some((2, 0)), false);
    frame(&mut ed);
    assert_eq!(region(&ed), "view:terminal");
    ed.terminal_mouse(K::ScrollDown, Some((2, 0)), false);
    frame(&mut ed);
    assert_eq!(region(&ed), "view:chat", "back: the chat's view mode");
}

/// A new or an earlier conversation forgets the chat's view — never the terminal's: it stays, its output held, and
/// goes out as it's left.
#[test]
fn another_conversation_leaves_the_terminals_view_alone() {
    let (mut ed, _sent) = term(Config::default(), 3);
    feed(&mut ed, "<C-\\>");
    print(&mut ed, "\r\nlater-output\r\n$ ");
    forget(&mut ed);
    assert!(terminal_viewed(&ed), "still viewed");
    assert!(ed.session.terminal.as_ref().unwrap().came(), "its output still held");
    feed(&mut ed, "<C-\\>");
    assert!(frame(&mut ed).contains("later-output"), "taken in as it's left");
}

/// `tab` / `S-tab` in its view mode step to the slot's other tabs, on and back (three tabs: the next and the one
/// before differ): another tab in front ends the view, what was held taken in; back on the terminal, its view mode
/// again (tarae's keys on its panel).
#[test]
fn tab_steps_to_the_slots_other_tab_and_back() {
    let (mut ed, _sent) = term(Config::default(), 3);
    let task = crate::task::Task { name: "check".into(), cmd: "true".into(), cwd: ed.session.root.clone() };
    ed.session.task_run = Some(crate::task::Run::for_test(task));
    ed.session.slot_front = Some(crate::sizes::Slot::Terminal);
    frame(&mut ed);
    feed(&mut ed, "<C-\\>");
    print(&mut ed, "\r\nwhile-on-the-task\r\n$ ");
    feed(&mut ed, "<tab>");
    assert_eq!(ed.slot(), Some(crate::sizes::Slot::Task));
    assert_eq!(region(&ed), "slot:task");
    let t = ed.session.terminal.as_ref().unwrap();
    assert!(ed.session.view.is_none() && !t.holding(), "the view gone, nothing held");
    assert!(t.parser.screen().contents().contains("while-on-the-task"), "what came taken in");
    feed(&mut ed, "<S-tab>");
    assert_eq!(ed.slot(), Some(crate::sizes::Slot::Terminal));
    assert_eq!(region(&ed), "view:terminal", "back: its view mode");
    // (a third tab — a test run's: `S-tab` from the terminal is the one before it, not the next)
    ed.session.test_run = Some(crate::testing::TestRun::finished("t", Vec::new(), vec!["ok".into()]));
    frame(&mut ed);
    ed.session.slot_front = Some(crate::sizes::Slot::Terminal);
    ed.bottom_take_keys();
    frame(&mut ed);
    assert_eq!(region(&ed), "view:terminal");
    let tabs: Vec<crate::sizes::Slot> = ed.slot_tabs().iter().map(|t| t.slot).collect();
    assert_eq!(tabs.len(), 3, "{tabs:?}");
    let at = tabs.iter().position(|&s| s == crate::sizes::Slot::Terminal).unwrap();
    feed(&mut ed, "<S-tab>");
    assert_eq!(ed.slot(), Some(tabs[(at + 2) % 3]), "the one before: {tabs:?}");
}

/// `space` in its view mode does what it does in the chat's — one code path (`view::key` → `key_in` → `said`):
/// said, with the way to type and the way out; the next key is the view's own (`T` waits for its character, as `T`
/// in the code), never the space menu's — `q` hides the panel.
#[test]
fn space_in_view_mode_is_said_as_in_the_chats() {
    let (mut ed, _sent) = term(Config::default(), 3);
    feed(&mut ed, "<C-\\><space>");
    let said = ed.status.as_ref().map(|s| s.0.clone()).unwrap_or_default();
    assert_eq!(said, "space isn't in view mode — i types in the program, esc to the code");
    assert_eq!(region(&ed), "view:terminal");
    assert!(ed.session.terminal.as_ref().unwrap().visible, "nothing hidden");
    let chat = crate::view::tests::viewing("<esc><space>");
    let said = chat.status.as_ref().map(|s| s.0.clone()).unwrap_or_default();
    assert_eq!(said, "space isn't in view mode — i types, esc to the code", "the chat's, the same sentence");
}

/// A blank row at the screen's bottom is a line of its own: the view's last rows are the screen's, `G` shows it
/// whole (not scrolled back by one).
#[test]
fn the_screens_blank_last_rows_are_lines_too() {
    let (mut ed, _sent) = term(Config::default(), 60);
    print(&mut ed, "\r\n");
    feed(&mut ed, "<C-\\>");
    feed(&mut ed, "ggG");
    let (v, t) = (view(&ed), ed.session.terminal.as_ref().unwrap());
    // (60 lines, the prompt's, the blank one under it)
    assert_eq!(v.rows(), 62, "every row of the scrollback and the screen");
    assert_eq!(t.parser.screen().scrollback(), 0, "the bottom: the screen as it is");
}

/// `C-\` in its view mode goes to the code whatever is selected — the round's next stop; `esc` collapses first.
#[test]
fn ctrl_backslash_leaves_with_a_selection_too() {
    let (mut ed, _sent) = term(Config::default(), 3);
    feed(&mut ed, "<C-\\>kx<C-\\>");
    assert_eq!(region(&ed), "code");
    feed(&mut ed, "<C-\\><C-\\>kx<esc>");
    assert_eq!(region(&ed), "view:terminal", "esc: the selection collapsed, still viewing");
}

/// A press while viewed is the view's, a program that asked for the mouse or not: its cursor goes to the cell
/// pressed, as a click in the code (a wide character's second cell: the character), nothing reaches the program
/// (its screen is held, maybe scrolled back), the keys stay the view's.
#[test]
fn a_click_in_view_mode_moves_its_cursor_and_sends_the_program_nothing() {
    use crate::event::MouseKind as K;
    let (mut ed, sent) = term(Config::default(), 30);
    // (the program asks for the mouse: SGR, every press)
    print(&mut ed, "\x1b[?1000h\x1b[?1006h\r\n文字 wide\r\n");
    feed(&mut ed, "<C-\\>gg");
    while sent.try_recv().is_ok() {}
    for kind in [K::Down, K::Drag, K::Up] {
        ed.terminal_mouse(kind, Some((2, 3)), false);
    }
    let said = ed.status.as_ref().map(|s| s.0.clone()).unwrap_or_default();
    assert_eq!(
        said, "a drag doesn't select in view mode yet — x or v selects, y copies, i types",
        "never a silent drag"
    );
    frame(&mut ed);
    let col = |ed: &Editor| {
        let d = view(ed).doc();
        let c = d.selection().primary().cursor(&d.text);
        c - mv::line_start(&d.text, mv::line_of(&d.text, c))
    };
    assert_eq!(col(&ed), 3, "on the cell pressed: `line 3`'s `e`");
    // (the second cell of `字`, past the 30 lines and the prompt's row: on the character)
    feed(&mut ed, "G");
    let row = ed
        .session
        .terminal
        .as_ref()
        .unwrap()
        .parser
        .screen()
        .contents()
        .lines()
        .position(|l| l.contains("wide"));
    ed.terminal_mouse(K::Down, Some((row.unwrap() as u16, 3)), false);
    assert_eq!((at(&ed).0.as_str(), col(&ed)), ("文字 wide", "文".len()), "a wide character's second cell");
    assert!(sent.try_recv().is_err(), "the program got nothing");
    assert_eq!(region(&ed), "view:terminal");
    assert_eq!(at(&ed).0, "文字 wide");
}

/// A sequence in your `[keys.terminal]` can't be taken from the program (its first key goes there): said, left out.
#[test]
fn a_sequence_in_your_terminal_keys_is_said_and_left_out() {
    let (config, warnings) = crate::config::parse("[keys.terminal]\n\"C-a\" = { d = \"terminal_view\" }\n");
    assert!(warnings.iter().any(|w| w.starts_with("keys.terminal.C-a: ")), "{warnings:?}");
    let ed = Editor::new(config);
    assert!(ed.keymaps.terminal_takes("C-a".parse().unwrap()).is_none());
    assert!(ed.keymaps.terminal_takes("C-\\".parse().unwrap()).is_some(), "the default kept");
}

/// It lands on the program's cursor row (the prompt); `k`, `gg` and a search go into the scrollback, the panel
/// shows what the cursor is on; `x` and `w` select there.
#[test]
fn it_lands_on_the_prompt_and_reads_the_scrollback() {
    let (mut ed, _sent) = term(Config::default(), 60);
    feed(&mut ed, "<C-\\>");
    assert_eq!(at(&ed).0, "$", "on the prompt");
    feed(&mut ed, "k");
    assert_eq!(at(&ed).0, "line 60");
    feed(&mut ed, "gg");
    assert_eq!(at(&ed).0, "line 1");
    let shown = frame(&mut ed);
    assert!(
        shown.lines().any(|l| l.trim_start().starts_with("line 1") && l.trim_end().ends_with("line 1")),
        "{shown}"
    );
    assert!(!shown.contains("line 60"), "scrolled back to the first rows:\n{shown}");
    feed(&mut ed, "/line 42<ret>");
    assert_eq!(at(&ed), ("line 42".into(), "line 42".into()));
    assert!(frame(&mut ed).contains("line 42"));
    feed(&mut ed, "x");
    assert_eq!(at(&ed).1, "line 42\n");
    // (the code's cursor and document never moved)
    assert_eq!(ed.doc().text.to_string(), "fn main() {}\n");
}

/// It holds still: what the program prints meanwhile is kept back — the panel says something came; `G` catches
/// up, on the latest line. Leaving takes it in too.
#[test]
fn it_holds_still_until_g_catches_up() {
    let (mut ed, _sent) = term(Config::default(), 3);
    feed(&mut ed, "<C-\\>gg");
    let rows = view(&ed).rows();
    assert!(!frame(&mut ed).contains("output waits"));
    print(&mut ed, "\r\nfresh output\r\n$ ");
    let shown = frame(&mut ed);
    assert!(shown.contains("output waits"), "said:\n{shown}");
    assert!(!shown.contains("fresh output"), "held still:\n{shown}");
    assert_eq!(view(&ed).rows(), rows);
    feed(&mut ed, "G");
    let shown = frame(&mut ed);
    assert!(shown.contains("fresh output") && !shown.contains("output waits"), "caught up:\n{shown}");
    assert_eq!(at(&ed).0, "$", "on the latest line");
    // (leaving takes in what came)
    print(&mut ed, "\r\nlater still\r\n$ ");
    assert!(!frame(&mut ed).contains("later still"));
    feed(&mut ed, "<esc>");
    assert!(frame(&mut ed).contains("later still"));
}

/// What the program asks of the terminal is answered as the reader reads it — also in the batch it reads while it
/// waits for the last one's go-ahead (kept while viewed): that batch isn't handed on yet, its question is answered
/// all the same. Caught up, the batch comes, nothing answered twice.
#[test]
fn its_questions_are_answered_while_held() {
    let (mut ed, sent) = term(Config::default(), 3);
    let generation = ed.session.terminal.as_ref().unwrap().generation();
    feed(&mut ed, "<C-\\>");
    while sent.try_recv().is_ok() {}
    // (the first batch while viewed: held, its go-ahead kept)
    let first = "while you read\r\n";
    crate::terminal::reader_reads(&ed, first.as_bytes());
    let (done, go) = std::sync::mpsc::sync_channel(1);
    crate::terminal::output_acked(&mut ed, generation, first.as_bytes(), done);
    assert!(go.try_recv().is_err(), "its go-ahead kept");
    // (the reader reads the next, a question in it, and waits on that go-ahead — the batch not handed on)
    let second = "\x1b[c";
    crate::terminal::reader_reads(&ed, second.as_bytes());
    assert_eq!(sent.try_recv().ok().as_deref(), Some(&b"\x1b[?62;22c"[..]), "answered as it's read");
    assert!(ed.session.terminal.as_ref().unwrap().came(), "the output still held");
    feed(&mut ed, "G");
    assert!(go.try_recv().is_ok());
    let (done, _go) = std::sync::mpsc::sync_channel(1);
    crate::terminal::output_acked(&mut ed, generation, second.as_bytes(), done);
    assert!(sent.try_recv().is_err(), "handed on, not answered twice");
}

/// Leaving from the scrollback keeps the rows you read in sight, what came below them; `i` goes to the bottom.
#[test]
fn leaving_keeps_the_rows_you_read() {
    let (mut ed, _sent) = term(Config::default(), 60);
    feed(&mut ed, "<C-\\>gg");
    print(&mut ed, "\r\nnew\r\n$ ");
    feed(&mut ed, "<esc>");
    let shown = frame(&mut ed);
    assert!(shown.contains("line 1") && shown.contains("lines back"), "the rows you read:\n{shown}");
    feed(&mut ed, "<C-\\>");
    assert_eq!(region(&ed), "terminal-program");
    feed(&mut ed, "<C-\\>i");
    assert_eq!(ed.session.terminal.as_ref().unwrap().parser.screen().scrollback(), 0, "typing: the bottom");
}

/// A key view mode has no use for is said, with the two ways on; nothing reaches the program.
#[test]
fn an_edit_is_said_not_sent() {
    let (mut ed, sent) = term(Config::default(), 3);
    feed(&mut ed, "<C-\\>");
    while sent.try_recv().is_ok() {}
    feed(&mut ed, "d");
    let said = ed.status.as_ref().map(|s| s.0.clone()).unwrap_or_default();
    assert_eq!(said, "d isn't in view mode — i types in the program, esc to the code");
    assert!(sent.try_recv().is_err(), "the program got nothing");
}

/// For `term.rs`'s painter test: an editor whose terminal printed three lines and a prompt, in its view mode
/// after `keys`.
pub(crate) fn viewing(keys: &str) -> Editor {
    let (mut ed, sent) = term(Config::default(), 3);
    feed(&mut ed, keys);
    drop(sent);
    ed
}

/// `y` copies the rows as drawn.
#[test]
fn y_copies_the_rows() {
    let (mut ed, _sent) = term(Config::default(), 3);
    feed(&mut ed, "<C-\\>kxy");
    let ev = ed.events.recv_timeout(std::time::Duration::from_secs(5)).expect("the copy's result");
    ed.handle_event(ev);
    assert!(crate::clipboard::written("line 3\n"));
    assert_eq!(region(&ed), "view:terminal");
}

/// The wheel up over the scrollback: its view mode at the row under the pointer; back down to the bottom, the
/// keys where they were (the program). Entered by a key, it stays.
#[test]
fn the_wheel_enters_view_mode_and_leaves_it_at_the_bottom() {
    use crate::event::MouseKind as K;
    let (mut ed, _sent) = term(Config::default(), 60);
    ed.terminal_mouse(K::ScrollUp, Some((2, 0)), false);
    frame(&mut ed);
    assert_eq!(region(&ed), "view:terminal");
    assert!(view(&ed).by_wheel == Some(Was::Program));
    ed.terminal_mouse(K::ScrollDown, Some((2, 0)), false);
    frame(&mut ed);
    assert_eq!(region(&ed), "terminal-program", "back at the bottom: the keys where they were");
    feed(&mut ed, "<C-\\>");
    ed.terminal_mouse(K::ScrollUp, Some((2, 0)), false);
    ed.terminal_mouse(K::ScrollDown, Some((2, 0)), false);
    frame(&mut ed);
    assert_eq!(region(&ed), "view:terminal", "entered by a key, it stays");
}

/// `C-w j` from the code puts tarae's keys on it: its view mode; `q` hides it (the shell runs on), as every
/// output panel's `q` closes it; `C-w q` too.
#[test]
fn ctrl_w_j_views_it_and_q_hides_it() {
    let (mut ed, _sent) = term(Config::default(), 3);
    feed(&mut ed, "<C-\\><esc><C-w>j");
    assert_eq!(region(&ed), "view:terminal");
    feed(&mut ed, "q");
    let t = ed.session.terminal.as_ref().unwrap();
    assert!(!t.visible && t.exited.is_none(), "hidden, running");
    assert_eq!(region(&ed), "code");
    assert!(ed.session.view.is_none());
    feed(&mut ed, "<C-\\><C-\\><C-w>q");
    assert!(!ed.session.terminal.as_ref().unwrap().visible, "C-w q: hidden too");
}

/// An ended program's output is read in view mode; `i` has nowhere to type, said.
#[test]
fn an_ended_program_is_read_not_typed_in() {
    let (mut ed, _sent) = term(Config::default(), 3);
    feed(&mut ed, "<C-\\>");
    ed.session.terminal.as_mut().unwrap().exited = Some(0);
    feed(&mut ed, "i");
    assert_eq!(region(&ed), "view:terminal");
    let said = ed.status.as_ref().map(|s| s.0.clone()).unwrap_or_default();
    assert_eq!(said, "its program has ended — q close, esc code");
    assert!(!status(&frame(&mut ed)).contains("i type"), "no `i type` said");
}

/// Your `[keys.terminal]` merges over the defaults, as every `[keys.*]` table does: a key added takes the program's
/// key too, the default's `C-\` still views it.
#[test]
fn a_key_added_to_your_terminal_keys_keeps_the_defaults() {
    let (config, warnings) = crate::config::parse("[keys.terminal]\n\"C-]\" = \"terminal_view\"\n");
    assert!(warnings.is_empty(), "{warnings:?}");
    let (mut ed, sent) = term(config, 3);
    feed(&mut ed, "<C-]>");
    assert_eq!(region(&ed), "view:terminal", "the key added");
    feed(&mut ed, "i");
    while sent.try_recv().is_ok() {}
    feed(&mut ed, "<C-\\>");
    assert_eq!(region(&ed), "view:terminal", "the default kept");
    assert!(sent.try_recv().is_err(), "the program got nothing");
}

/// `to_program` gives a default back: `C-\` bound to it goes to the program as an unlisted key does, `C-]` views it,
/// and the head's hint names `C-]`.
#[test]
fn to_program_gives_a_key_back_to_the_program() {
    let toml = "[keys.terminal]\n\"C-\\\\\" = \"to_program\"\n\"C-]\" = \"terminal_view\"\n";
    let (config, warnings) = crate::config::parse(toml);
    assert!(warnings.is_empty(), "{warnings:?}");
    let (mut ed, sent) = term(config, 3);
    assert!(frame(&mut ed).contains("C-] view"), "the hint names the key as bound");
    while sent.try_recv().is_ok() {}
    feed(&mut ed, "<C-\\>");
    assert_eq!(region(&ed), "terminal-program");
    assert_eq!(sent.try_recv().ok().as_deref(), Some(&b"\x1c"[..]), "C-\\ went to the program");
    feed(&mut ed, "<C-]>");
    assert_eq!(region(&ed), "view:terminal");
}

/// Not viewed, each batch's go-ahead goes back to the reader at once. Viewed, it's kept: the reader reads no more,
/// the program waits on its write (a real terminal's flow control) — the head says `output waits`, the view where
/// you read. `G` sends it (still viewed, the next is kept again); leaving sends what's kept since.
#[test]
fn while_viewed_the_reader_waits_until_you_catch_up() {
    let (mut ed, _sent) = term(Config::default(), 3);
    let generation = ed.session.terminal.as_ref().unwrap().generation();
    let batch = |ed: &mut Editor, s: &str| {
        let (done, go) = std::sync::mpsc::sync_channel(1);
        crate::terminal::output_acked(ed, generation, s.as_bytes(), done);
        go
    };
    let free = batch(&mut ed, "before\r\n");
    assert!(free.try_recv().is_ok(), "not viewed: at once");
    feed(&mut ed, "<C-\\>gg");
    let first = batch(&mut ed, "a line while you read\r\n");
    assert!(first.try_recv().is_err(), "viewed: the reader waits, and the program with it");
    assert!(frame(&mut ed).contains("output waits"));
    assert_eq!(at(&ed).0, "line 1", "the view where you read");
    feed(&mut ed, "G");
    assert!(first.try_recv().is_ok(), "caught up: it goes on");
    let second = batch(&mut ed, "more\r\n");
    assert!(second.try_recv().is_err(), "still viewed: kept again");
    feed(&mut ed, "<esc>");
    assert_eq!(region(&ed), "code");
    assert!(second.try_recv().is_ok(), "left: it goes on");
    assert!(!ed.session.terminal.as_ref().unwrap().came());
}

/// Its view mode on a full scrollback — 5,000 rows of 100 characters and a 40-row screen — read a page at a time
/// (`C-\`, the best of 5), on the main loop as the key is handled: a budget CI runs, 5 ms (times
/// `TARAE_PERF_SLACK`, as the frame budgets: shared runners are slower).
#[test]
fn perf_budget_view_terminal() {
    if crate::test_results::unoptimized("view::terminal_tests::perf_budget_view_terminal") {
        return;
    }
    let mut ed = Editor::new(Config::default());
    let _sent = crate::terminal::stand_in(&mut ed, 40, 120, 5000);
    let generation = ed.session.terminal.as_ref().unwrap().generation();
    let row = "x".repeat(96);
    let text: String = (0..5100).map(|i| format!("{i:04}{row}\r\n")).collect();
    output(&mut ed, generation, text.as_bytes());
    let ctrl_backslash: Key = "C-\\".parse().unwrap();
    let mut best = f64::MAX;
    for _ in 0..5 {
        let t0 = std::time::Instant::now();
        ed.handle_key(ctrl_backslash);
        best = best.min(t0.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(view(&ed).rows(), 5040, "the scrollback and the screen, whole");
        ed.handle_key(Key::plain(Code::Char('i')));
    }
    // (measured 2026-10-21: 3.4 ms of 3.7 is vt100 handing out its rows, `Screen::rows` a page at a time — no
    // faster way in through its API; the document itself 0.17 ms. The document is read on entering, on `G`, and on a
    // new panel size once it has settled (`terminal_resized`) — never by a move)
    let line = format!("PERF view_terminal_5k_rows_ms {best:.3} budget=5 noise=0.5\n");
    let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
    let slack =
        std::env::var("TARAE_PERF_SLACK").ok().and_then(|s| s.parse::<u32>().ok()).unwrap_or(1).max(1);
    let budget = 5.0 * f64::from(slack);
    assert!(best < budget, "{best:.2} ms to view 5,000 rows (budget {budget} ms)");
}

/// The panel taking another size reads the view again once the size has settled — the same at two frames in a row
/// (a frame is asked for, `SETTLE` on) — the cursor on its line; the cursor drawn where its character is — a wide
/// one two cells.
#[test]
fn a_new_size_reads_it_again_and_the_cursor_is_drawn_on_its_cell() {
    let (mut ed, _sent) = term(Config::default(), 30);
    print(&mut ed, "\r\n가나 end\r\n$ ");
    feed(&mut ed, "<C-\\>kfe");
    assert_eq!(at(&ed), ("가나 end".into(), "가나 e".into()));
    let cursors = ed.screen.cursors.borrow().clone();
    let x0 = ed.screen.slot_x + crate::chrome::PAD;
    assert_eq!(
        cursors.iter().map(|c| c.0 - x0).collect::<Vec<_>>(),
        vec![5],
        "after two wide characters and a space"
    );
    // (`v` under way: the view's mode and selection live through the reading again)
    ed.handle_key(Key::plain(Code::Char('v')));
    let before = view(&ed).rows();
    let mut bytes = Vec::new();
    crate::term::render(&mut ed, &mut bytes, W, H + 6).unwrap();
    assert_eq!(view(&ed).rows(), before, "a size seen once: not read yet (a border drag's frame)");
    assert!(terminal_settling(&ed));
    assert_eq!(ed.tick_wanted(), Some(SETTLE), "a frame asked for, once it has held");
    crate::term::render(&mut ed, &mut bytes, W, H + 6).unwrap();
    assert_ne!(view(&ed).rows(), before, "settled: read again at its new size");
    assert!(!terminal_settling(&ed) && ed.tick_wanted().is_none(), "nothing more wanted: idle stays idle");
    assert_eq!(at(&ed), ("가나 end".into(), "가나 e".into()), "the same line, the same selection");
    assert_eq!(view(&ed).mode(), Mode::Select, "its `v` kept");
    // (smaller, then back to the size it was read at: vt100 dropped the bottom rows and brought them back blank —
    // read again all the same, the view what the screen shows)
    crate::term::render(&mut ed, &mut bytes, W, H).unwrap();
    crate::term::render(&mut ed, &mut bytes, W, H + 6).unwrap();
    assert!(terminal_settling(&ed), "back at its size, but changed since the last frame");
    crate::term::render(&mut ed, &mut bytes, W, H + 6).unwrap();
    assert!(!terminal_settling(&ed));
    let (d, screen) = (view(&ed).doc(), ed.session.terminal.as_ref().unwrap().parser.screen());
    for (r, row) in screen.rows(0, screen.size().1).enumerate() {
        let line = d.text.get_line(d.top + r).map(|l| l.to_string().trim_end().to_string());
        assert_eq!(line.as_deref(), Some(row.trim_end()), "row {r}: the view as the screen");
    }
}
