//! A tool call's input read as it streams (`input_json_delta`): each piece of partial JSON is read once, from where
//! the last one stopped — the strings an edit is made of come out as they grow (O(delta)), never parsed whole again.

/// The tool whose input is read: tarae's `edit` (a list of items), Claude Code's Edit (one item) or Write (one, the
/// whole file — its `content` read as the new text).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Edit,
    NativeEdit,
    Write,
}

impl Tool {
    pub fn of(name: &str) -> Option<Tool> {
        match name {
            "mcp__tarae__edit" => Some(Tool::Edit),
            "Edit" => Some(Tool::NativeEdit),
            "Write" => Some(Tool::Write),
            _ => None,
        }
    }
}

/// A field of an item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    File,
    Old,
    New,
    All,
    /// `move_to` or `delete`: an item that isn't a change of text.
    Other,
}

/// What came of an item since the last `take`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Piece {
    /// Text added to a string field; `closed`: its closing quote came.
    Text { item: usize, field: Field, add: String, closed: bool },
    /// A `true` or `false` (`replace_all`, `delete`).
    Flag { item: usize, field: Field, on: bool },
}

enum Frame {
    /// An object: its key in hand (read, its value not over yet), whether a key comes next.
    Obj {
        key: Option<String>,
        want_key: bool,
    },
    Arr {
        index: usize,
    },
}

#[derive(Clone, Copy)]
enum Esc {
    No,
    Slash,
    /// `\u` and the hex digits read so far.
    Hex {
        n: u8,
        v: u32,
    },
}

pub struct Scan {
    tool: Tool,
    stack: Vec<Frame>,
    /// Inside a string: `Some(true)` a key's, `Some(false)` a value's.
    string: Option<bool>,
    esc: Esc,
    /// A high surrogate (`\ud83d`) waiting for its low half.
    high: Option<u32>,
    key: String,
    /// A bare value being read (`true`, a number).
    word: String,
    /// The item field the string being read is (None: one nobody draws).
    field: Option<(usize, Field)>,
    out: Vec<Piece>,
}

impl Scan {
    pub fn new(tool: Tool) -> Scan {
        Scan {
            tool,
            stack: Vec::new(),
            string: None,
            esc: Esc::No,
            high: None,
            key: String::new(),
            word: String::new(),
            field: None,
            out: Vec::new(),
        }
    }

    /// What came since the last call.
    pub fn take(&mut self) -> Vec<Piece> {
        std::mem::take(&mut self.out)
    }

    /// The next piece of the input's JSON, as it came.
    pub fn feed(&mut self, json: &str) {
        for c in json.chars() {
            match self.string {
                Some(key) => self.in_string(c, key),
                None => self.outside(c),
            }
        }
    }

    fn in_string(&mut self, c: char, key: bool) {
        match self.esc {
            Esc::Slash => {
                self.esc = Esc::No;
                let e = match c {
                    'n' => '\n',
                    't' => '\t',
                    'r' => '\r',
                    'b' => '\u{8}',
                    'f' => '\u{c}',
                    'u' => {
                        self.esc = Esc::Hex { n: 0, v: 0 };
                        return;
                    }
                    c => c,
                };
                self.push(e, key);
            }
            Esc::Hex { n, v } => {
                let v = v * 16 + c.to_digit(16).unwrap_or(0);
                if n < 3 {
                    self.esc = Esc::Hex { n: n + 1, v };
                    return;
                }
                self.esc = Esc::No;
                match (v, self.high.take()) {
                    (0xD800..=0xDBFF, _) => self.high = Some(v),
                    (0xDC00..=0xDFFF, Some(h)) => {
                        let cp = 0x10000 + ((h - 0xD800) << 10) + (v - 0xDC00);
                        self.push(char::from_u32(cp).unwrap_or('\u{fffd}'), key);
                    }
                    (v, _) => self.push(char::from_u32(v).unwrap_or('\u{fffd}'), key),
                }
            }
            Esc::No => match c {
                '\\' => self.esc = Esc::Slash,
                '"' => {
                    self.string = None;
                    if key {
                        if let Some(Frame::Obj { key: k, want_key }) = self.stack.last_mut() {
                            *k = Some(std::mem::take(&mut self.key));
                            *want_key = false;
                        }
                    } else if let Some((item, field)) = self.field.take() {
                        self.text(item, field, None, true);
                    }
                }
                c => self.push(c, key),
            },
        }
    }

    fn push(&mut self, c: char, key: bool) {
        if key {
            self.key.push(c);
        } else if let Some((item, field)) = self.field {
            self.text(item, field, Some(c), false);
        }
    }

    /// `c` added to the item's field (merged into the piece this read made already), or the field closed.
    fn text(&mut self, item: usize, field: Field, c: Option<char>, closed: bool) {
        match self.out.last_mut() {
            Some(Piece::Text { item: i, field: f, add, closed: done })
                if *i == item && *f == field && !*done =>
            {
                add.extend(c);
                *done = closed;
            }
            _ => self.out.push(Piece::Text { item, field, add: c.into_iter().collect(), closed }),
        }
    }

    fn outside(&mut self, c: char) {
        match c {
            '"' => {
                self.word_done();
                let key = matches!(self.stack.last(), Some(Frame::Obj { want_key: true, .. }));
                self.string = Some(key);
                if key {
                    self.key.clear();
                } else {
                    self.field = self.field_here();
                    // (an empty string is an item's field all the same: its closing quote says so)
                    if let Some((item, field)) = self.field {
                        self.text(item, field, None, false);
                    }
                }
            }
            '{' => self.stack.push(Frame::Obj { key: None, want_key: true }),
            '[' => self.stack.push(Frame::Arr { index: 0 }),
            '}' | ']' => {
                self.word_done();
                self.stack.pop();
            }
            ',' => {
                self.word_done();
                match self.stack.last_mut() {
                    Some(Frame::Obj { key, want_key }) => (*key, *want_key) = (None, true),
                    Some(Frame::Arr { index }) => *index += 1,
                    None => {}
                }
            }
            ':' => {}
            c if c.is_whitespace() => self.word_done(),
            c => self.word.push(c),
        }
    }

    fn word_done(&mut self) {
        if self.word.is_empty() {
            return;
        }
        let on = match std::mem::take(&mut self.word).as_str() {
            "true" => true,
            "false" => false,
            _ => return,
        };
        if let Some((item, field @ (Field::All | Field::Other))) = self.field_here() {
            self.out.push(Piece::Flag { item, field, on });
        }
    }

    /// The item and field the value starting here is: tarae's `{"edits": [{…}, …]}` at depth 3, the native
    /// tools' `{…}` at depth 1.
    fn field_here(&self) -> Option<(usize, Field)> {
        fn key(f: &Frame) -> Option<&str> {
            match f {
                Frame::Obj { key: Some(k), .. } => Some(k.as_str()),
                _ => None,
            }
        }
        let (item, k) = match (self.tool, self.stack.as_slice()) {
            (Tool::Edit, [root, Frame::Arr { index }, obj]) if key(root) == Some("edits") => {
                (*index, key(obj)?)
            }
            (Tool::NativeEdit | Tool::Write, [obj]) => (0, key(obj)?),
            _ => return None,
        };
        let field = match (self.tool, k) {
            (_, "file_path") => Field::File,
            (Tool::Write, "content") => Field::New,
            (Tool::Edit | Tool::NativeEdit, "old_string") => Field::Old,
            (Tool::Edit | Tool::NativeEdit, "new_string") => Field::New,
            (Tool::Edit | Tool::NativeEdit, "replace_all") => Field::All,
            (Tool::Edit, "move_to" | "delete") => Field::Other,
            _ => return None,
        };
        Some((item, field))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every field's text, by item, as the pieces built it up — and which closed.
    fn read(tool: Tool, chunks: &[&str]) -> Vec<(usize, Field, String, bool)> {
        let mut s = Scan::new(tool);
        let mut got: Vec<(usize, Field, String, bool)> = Vec::new();
        for c in chunks {
            s.feed(c);
            for p in s.take() {
                match p {
                    Piece::Text { item, field, add, closed } => {
                        match got.iter_mut().find(|g| g.0 == item && g.1 == field) {
                            Some(g) => {
                                g.2.push_str(&add);
                                g.3 = closed;
                            }
                            None => got.push((item, field, add, closed)),
                        }
                    }
                    Piece::Flag { item, field, on } => got.push((item, field, on.to_string(), true)),
                }
            }
        }
        got
    }

    /// Any split of the input reads the same: escapes, `\u` escapes and surrogate pairs cut anywhere.
    #[test]
    fn the_input_reads_the_same_however_it_is_cut() {
        // (a `\u` escape for `é` and a surrogate pair for the emoji, as some writers send them; the keys in the
        // order the model wrote them)
        let input = r#"{"edits": [
            {"file_path": "src/a.rs", "old_string": "let x = 1;\n", "new_string": "let x = \"é😀\\t\";\n\tdone", "replace_all": true},
            {"file_path": "b.rs", "move_to": "c.rs"},
            {"file_path": "d.rs", "old_string": "", "new_string": "{[,]}:"}
        ]}"#;
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(input).unwrap()["edits"][0]["new_string"],
            "let x = \"é😀\\t\";\n\tdone"
        );
        let whole = read(Tool::Edit, &[input]);
        assert_eq!(
            whole,
            [
                (0, Field::File, "src/a.rs".into(), true),
                (0, Field::Old, "let x = 1;\n".into(), true),
                (0, Field::New, "let x = \"é😀\\t\";\n\tdone".into(), true),
                (0, Field::All, "true".into(), true),
                (1, Field::File, "b.rs".into(), true),
                (1, Field::Other, "c.rs".into(), true),
                (2, Field::File, "d.rs".into(), true),
                (2, Field::Old, "".into(), true),
                (2, Field::New, "{[,]}:".into(), true),
            ]
        );
        let chars: Vec<char> = input.chars().collect();
        for cut in 1..chars.len() {
            let (a, b): (String, String) = (chars[..cut].iter().collect(), chars[cut..].iter().collect());
            assert_eq!(read(Tool::Edit, &[&a, &b]), whole, "cut at {cut}: {a:?} | {b:?}");
        }
        let singles: Vec<String> = chars.iter().map(char::to_string).collect();
        let singles: Vec<&str> = singles.iter().map(String::as_str).collect();
        assert_eq!(read(Tool::Edit, &singles), whole, "a character at a time");
    }

    /// A string not closed yet is said as far as it goes; the native tools' fields are read at the top.
    #[test]
    fn a_string_comes_as_it_grows() {
        assert_eq!(
            read(Tool::NativeEdit, &[r#"{"file_path":"a.rs","old_string":"x","new_string":"fn ma"#]),
            [
                (0, Field::File, "a.rs".into(), true),
                (0, Field::Old, "x".into(), true),
                (0, Field::New, "fn ma".into(), false)
            ]
        );
        assert_eq!(
            read(Tool::Write, &[r#"{"file_path":"a.rs","content":"one\ntw"#]),
            [(0, Field::File, "a.rs".into(), true), (0, Field::New, "one\ntw".into(), false)]
        );
        // (a key named so deeper down isn't the item's)
        assert_eq!(read(Tool::NativeEdit, &[r#"{"x":{"new_string":"no"}}"#]), []);
    }
}
