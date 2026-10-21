//! Quoting (인용, `target/sb/quote-plan.md` §4): part of what a surface shows — the chat's conversation, the
//! terminal's scrollback, the code — taken to the chat as a chip over its input (`enter` on a view mode's
//! selection, `space >` on the code's) and sent with the input's next message as `<quoted>`: where it came from,
//! cut to a limit and said so, token-like lines taken out of the terminal's. The user's act — no card: what can't
//! go is said where they look.

use ropey::Rope;

use crate::editor::Editor;
use crate::movement as mv;

/// Most of one quote's text, and of all a message carries: past it the first quarter and the last three stay
/// (an error is at the end; the command that made it often at the top).
pub const QUOTE_MAX: usize = 16 * 1024;
pub const QUOTES_BYTES: usize = 48 * 1024;
/// Most quotes a message carries.
pub const QUOTES_MAX: usize = 8;
/// The least room a quote is made in: under it, it's refused (said) rather than cut to nothing.
const ROOM_MIN: usize = 256;

/// A selection taken, before it's a quote: where it came from, the chip's words, its text.
pub struct Taken {
    /// The tag's attributes, `from` first.
    pub attrs: Vec<(&'static str, String)>,
    /// The chip's words: `terminal 12 lines`, `src/a.rs L12–18`.
    pub label: String,
    pub text: String,
    /// Token-like lines taken out (the terminal's: what it shows may be `env`'s output, a header).
    pub redact: bool,
    /// The code's: its document, that document's version and the byte range — the message's `<selection>` doesn't
    /// carry it again (by the document, not its path: two scratch buffers have none).
    pub code: Option<(crate::document::DocId, u64, usize, usize)>,
}

/// A quote waiting over the chat's input (`Chat.quotes`) — as it was when taken, not as the surface is at Enter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Quote {
    /// Random, the closing tag's too: text that prints `</quoted>` can't end the block.
    pub id: String,
    pub attrs: Vec<(&'static str, String)>,
    pub label: String,
    pub text: String,
    /// What the cut kept (`kept the first 50 and the last 150 of 640 lines`).
    pub cut: Option<String>,
    /// Token-like lines taken out.
    pub hidden: usize,
    pub code: Option<(crate::document::DocId, u64, usize, usize)>,
}

impl Quote {
    /// Its chip over the input, and in the sent message's row.
    pub fn chip(&self) -> String {
        let mut s = self.label.clone();
        if self.cut.is_some() {
            s.push_str(" · cut");
        }
        if self.hidden > 0 {
            s.push_str(&format!(" · {} hidden", lines(self.hidden)));
        }
        s
    }

    /// What the message carries.
    pub fn block(&self) -> String {
        let mut head = format!("<quoted id=\"{}\"", self.id);
        let said = self.cut.iter().map(|c| ("cut", c.clone()));
        let n = self.hidden;
        let hidden =
            (n > 0).then(|| ("hidden", format!("{n} token-like line{}", if n == 1 { "" } else { "s" })));
        for (k, v) in self.attrs.iter().cloned().chain(said).chain(hidden) {
            head.push_str(&format!(" {k}=\"{}\"", v.replace('"', "'").replace('\n', " ")));
        }
        format!("{head}>\n{}\n</quoted id=\"{}\">", self.text, self.id)
    }

    /// The state's shape of it — where from and how much, never its text.
    pub fn state(&self) -> serde_json::Value {
        let from = self.attrs.iter().find(|(k, _)| *k == "from").map(|(_, v)| v.as_str());
        serde_json::json!({
            "kind": "quote",
            "from": from,
            "lines": self.text.lines().count(),
            "cut": self.cut.is_some(),
            "hidden": self.hidden,
        })
    }
}

/// `n line(s)`.
pub fn lines(n: usize) -> String {
    format!("{n} line{}", if n == 1 { "" } else { "s" })
}

/// A quote of `t`, cut to `room` bytes.
fn made(t: Taken, room: usize) -> Quote {
    let (text, hidden) = match t.redact {
        true => redacted(&t.text),
        false => (t.text, 0),
    };
    let (text, cut) = cut(&text, room);
    Quote { id: id_for(&text), attrs: t.attrs, label: t.label, text, cut, hidden, code: t.code }
}

/// `text` with token-like lines taken out (`tokens::redacted`), and how many lines it changed.
fn redacted(text: &str) -> (String, usize) {
    let out = crate::tokens::redacted(text);
    let n = text.split('\n').zip(out.split('\n')).filter(|(a, b)| a != b).count();
    (out, n)
}

/// `text` within `max` bytes: as it is, else its first quarter's whole lines and its last three quarters', `… N
/// lines cut …` between — one long line (nothing whole fits) cut by characters the same way. And what was kept.
pub fn cut(text: &str, max: usize) -> (String, Option<String>) {
    if text.len() <= max {
        return (text.to_string(), None);
    }
    // (the marker between is in `max` too: its longest form — what's cut is never more than all of it)
    let max = max.saturating_sub(format!("\n… {} bytes cut …\n", text.len()).len());
    let all: Vec<&str> = text.split('\n').collect();
    let head_max = max / 4;
    let (mut head, mut used) = (0, 0);
    // (each line with its line break)
    while head < all.len() && used + all[head].len() < head_max {
        used += all[head].len() + 1;
        head += 1;
    }
    let (mut tail, mut used) = (0, 0);
    while head + tail < all.len() && used + all[all.len() - 1 - tail].len() < max - head_max {
        used += all[all.len() - 1 - tail].len() + 1;
        tail += 1;
    }
    if head + tail == 0 {
        let floor = |mut i: usize| {
            while !text.is_char_boundary(i) {
                i -= 1;
            }
            i
        };
        let a = floor(head_max);
        let b = text.len() - (max - head_max);
        let b = (b..=text.len()).find(|&i| text.is_char_boundary(i)).unwrap_or(text.len());
        let gone = b - a;
        let out = format!("{}\n… {gone} bytes cut …\n{}", &text[..a], &text[b..]);
        return (
            out,
            Some(format!("kept the first {a} and the last {} of {} bytes", text.len() - b, text.len())),
        );
    }
    let gone = all.len() - head - tail;
    let mut out: Vec<String> = all[..head].iter().map(|s| s.to_string()).collect();
    out.push(format!("… {} cut …", lines(gone)));
    out.extend(all[all.len() - tail..].iter().map(|s| s.to_string()));
    (out.join("\n"), Some(format!("kept the first {head} and the last {tail} of {} lines", all.len())))
}

/// A fresh id, `q` and five hex digits, that `text` doesn't close.
fn id_for(text: &str) -> String {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    loop {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(N.fetch_add(1, Ordering::Relaxed));
        let id = format!("q{:05x}", h.finish() & 0xf_ffff);
        if !text.contains(&format!("id=\"{id}\"")) {
            return id;
        }
    }
}

/// How document line `l` is quoted: its first characters that aren't the text's (a bar, a code well's edge, a
/// wrapped row's hanging indent) to leave out, and — when it goes on from the line before (the layout wrapped the
/// line) — how many spaces join them (the chat's: the one its break took, or none; the terminal's: the blanks at the
/// end of the row before, which its document trims).
pub type Lines<'a> = &'a dyn Fn(usize) -> (usize, Option<usize>);

/// The bytes `from..to` of a surface's document as a quote's text: each line's part, what the drawer put before
/// it left out and lines the layout wrapped joined again (`how`), a line cut with `…` given its whole end when the
/// selection reaches it (`rest`). And its first and last line.
pub fn text_of(
    text: &Rope,
    (from, to): (usize, usize),
    how: Lines,
    rest: &dyn Fn(usize) -> Option<std::sync::Arc<str>>,
) -> (String, usize, usize) {
    let to = to.max(from);
    let first = mv::line_of(text, from);
    let last = mv::line_of(text, if to > from { to - 1 } else { from });
    let mut out = String::new();
    for l in first..=last {
        let (start, end) = (mv::line_start(text, l), mv::line_end(text, l));
        let (skip, join) = how(l);
        let skip: usize = text.byte_slice(start..end).chars().take(skip).map(char::len_utf8).sum();
        let a = from.max(start + skip);
        if l > first {
            match join {
                Some(spaces) => out.extend(std::iter::repeat_n(' ', spaces)),
                None => out.push('\n'),
            }
        }
        let b = to.min(end).max(a);
        let dots = end.saturating_sub('…'.len_utf8()).max(start);
        match rest(l) {
            Some(r) if b > dots && text.byte_slice(dots..end) == "…" => {
                out.extend(text.byte_slice(a.min(dots)..dots).chunks());
                out.push_str(&r);
            }
            _ => out.extend(text.byte_slice(a..b).chunks()),
        }
    }
    (out, first, last)
}

/// The code's selections wider than a character, one quote each (`space >`, `:quote`) — None: nothing is
/// selected. A file named like a key Claude may not read is refused, said.
pub fn from_code(ed: &Editor) -> Option<Result<Vec<Taken>, String>> {
    let d = ed.doc();
    let wide: Vec<_> = d
        .selection()
        .ranges()
        .iter()
        .filter(|r| crate::graphemes::next_boundary(&d.text, r.from()) < r.to())
        .copied()
        .collect();
    if wide.is_empty() {
        return None;
    }
    let name = d.display_name(&ed.session.root);
    if d.path.as_ref().is_some_and(|p| crate::reads::key_shut(ed, p, &[])) {
        return Some(Err(format!("{name} is named like a key — point at its pane (space @) to choose")));
    }
    let taken = wide
        .iter()
        .map(|r| {
            let (text, l0, l1) = text_of(&d.text, (r.from(), r.to()), &|_| (0, None), &|_| None);
            let at = match l0 == l1 {
                true => format!("L{}", l0 + 1),
                false => format!("L{}–{}", l0 + 1, l1 + 1),
            };
            Taken {
                attrs: vec![
                    ("from", "code".into()),
                    ("path", name.clone()),
                    ("lines", format!("{}-{}", l0 + 1, l1 + 1)),
                ],
                label: format!("{name} {at}"),
                text,
                redact: false,
                code: Some((d.id, d.version(), r.from(), r.to())),
            }
        })
        .collect();
    Some(Ok(taken))
}

/// `space >` · `:quote`: the code's selection to the chat as quotes, the input with the keys; nothing selected,
/// the chat's view mode.
pub fn code(ed: &mut Editor) {
    match from_code(ed) {
        Some(Ok(taken)) => add(ed, taken),
        Some(Err(e)) => ed.set_status(e),
        None => crate::view::chat_view(ed),
    }
}

/// Quotes over the chat's input — the chat opened (brought back when minimized), its input with the keys. Past
/// `QUOTES_MAX` or `QUOTES_BYTES` the rest isn't taken, said; each is cut to the room left.
pub fn add(ed: &mut Editor, taken: Vec<Taken>) {
    if taken.is_empty() {
        return;
    }
    if ed.session.chat.is_none() {
        crate::chat::open(ed);
    }
    let Some(c) = ed.session.chat.as_mut() else { return };
    c.focused = true;
    c.unfolded = true;
    let mut left = 0;
    for t in taken {
        let used: usize = c.quotes.iter().map(|q| q.text.len()).sum();
        let room = QUOTES_BYTES.saturating_sub(used).min(QUOTE_MAX);
        if c.quotes.len() >= QUOTES_MAX || room < ROOM_MIN {
            left += 1;
            continue;
        }
        c.quotes.push(made(t, room));
        c.with.push(crate::chat::With::Quote);
    }
    if left > 0 {
        ed.set_status(format!(
            "{left} not quoted — a message carries {QUOTES_MAX} quotes, {} KB at most: send it, or backspace takes \
             one off",
            QUOTES_BYTES / 1024
        ));
    }
}

/// Whether `quotes` are within one message's limits.
pub fn fit<'a>(quotes: impl Iterator<Item = &'a Quote>) -> bool {
    let (n, bytes) = quotes.fold((0, 0), |(n, b), q| (n + 1, b + q.text.len()));
    n <= QUOTES_MAX && bytes <= QUOTES_BYTES
}

/// `quotes` put back over the input ahead of its own (older than them) — as many as fit beside them, in order; how
/// many didn't.
pub fn over_input(c: &mut crate::chat::Chat, quotes: Vec<Quote>) -> usize {
    let mut lost = 0;
    let mut at = 0;
    for q in quotes {
        if !fit(c.quotes.iter().chain(std::iter::once(&q))) {
            lost += 1;
            continue;
        }
        c.quotes.insert(at, q);
        c.with.insert(0, crate::chat::With::Quote);
        at += 1;
    }
    lost
}

/// `quotes` ahead of `onto`'s; past the limits the newest are taken off — those, in their order.
pub fn ahead(mut quotes: Vec<Quote>, onto: &mut Vec<Quote>) -> Vec<Quote> {
    quotes.append(onto);
    let mut off = Vec::new();
    while quotes.len() > QUOTES_MAX || quotes.iter().map(|q| q.text.len()).sum::<usize>() > QUOTES_BYTES {
        off.extend(quotes.pop());
    }
    off.reverse();
    *onto = quotes;
    off
}

/// A message going again (its conversation was gone — `chat`'s resend, queued first): its quotes and screen wait
/// with it, ahead of those of messages queued since (a screen queued since is newer: it goes in its place). The
/// quotes queued since past one message's limits wait over the input for the next message, ahead of its own — how
/// many, and how many didn't fit there either (taken off).
pub fn put_back(
    c: &mut crate::chat::Chat,
    quotes: Vec<Quote>,
    screen: Option<crate::chat::Shot>,
) -> (usize, usize) {
    if c.queued_screen.is_none() {
        c.queued_screen = screen;
    }
    let over = ahead(quotes, &mut c.queued_quotes);
    let n = over.len();
    let lost = over_input(c, over);
    (n - lost, lost)
}

/// `n quote(s)`.
pub fn count(n: usize) -> String {
    format!("{n} quote{}", if n == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests;
