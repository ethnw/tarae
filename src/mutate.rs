//! Seeded mutation for the readers' never-panic loops (V2 step 5, `target/sb/v2-fuzz-plan.md` §4): a fixed-seed
//! generator, and mutators of a line of text and of a few bytes — the loops are the same every run. (The
//! structs' readers keep their own, which know their lengths: `wire/tests.rs`, `proxy/parse/tests.rs`.)

/// A small fixed-seed generator (xorshift).
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    /// One of `of`.
    pub fn pick<'a, T>(&mut self, of: &'a [T]) -> &'a T {
        &of[self.below(of.len())]
    }
}

/// Characters that mean something to a reader of lines: quotes (GNU's curly ones too), brackets, separators, a
/// NUL, a carriage return, a tab, a backslash, multibyte ones, digits.
const CHARS: &[char] = &[
    '\'', '"', '‘', '’', '`', '(', ')', '[', ']', '{', '/', '.', ':', '=', ',', ';', '~', ' ', '\t', '\r',
    '\n', '\0', '\\', 'é', '—', '�', '0', '9', '-', '+',
];

/// One to three of: a character changed to one of `CHARS`, put in, or a run of one (to 300); one of `words` put
/// in; a cut; a piece copied elsewhere; a piece taken out — on character boundaries, so the line stays text.
pub fn text(rng: &mut Rng, s: &mut String, words: &[&str]) {
    let mut c: Vec<char> = s.chars().collect();
    for _ in 0..1 + rng.below(3) {
        let at = rng.below(c.len() + 1);
        match rng.below(8) {
            0 if at < c.len() => c[at] = *rng.pick(CHARS),
            1 if !words.is_empty() => {
                let w = rng.pick(words);
                c.splice(at..at, w.chars());
            }
            2 => c.truncate(at),
            3 if at < c.len() => {
                let end = (at + 1 + rng.below(32)).min(c.len());
                let piece = c[at..end].to_vec();
                let to = rng.below(c.len() + 1);
                c.splice(to..to, piece);
            }
            4 if at < c.len() => {
                let end = (at + 1 + rng.below(16)).min(c.len());
                c.drain(at..end);
            }
            5 => {
                let ch = *rng.pick(CHARS);
                c.splice(at..at, std::iter::repeat_n(ch, 1 + rng.below(300)));
            }
            _ => c.insert(at, *rng.pick(CHARS)),
        }
    }
    *s = c.into_iter().collect();
}

/// Writes `seeds` (name, bytes) as the fuzz target `target`'s: `fuzz/parsers/seeds/<target>/<name>`, over what's
/// there by that name. For the ignored `writes_fuzz_seeds` tests, run by hand when a seed's builder changes
/// (`cargo test writes_fuzz_seeds -- --ignored`) — tarae's tests never read `fuzz/`.
pub fn fuzz_seeds(target: &str, seeds: impl IntoIterator<Item = (String, Vec<u8>)>) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz/parsers/seeds").join(target);
    std::fs::create_dir_all(&dir).unwrap();
    for (name, bytes) in seeds {
        assert!(!name.is_empty() && !name.contains(['/', '.']), "a seed's name: {name:?}");
        std::fs::write(dir.join(name), bytes).unwrap();
    }
}

/// One to three of: a byte changed (to any, or a bit flipped), one of `words` put in, a cut, a byte put in.
pub fn bytes(rng: &mut Rng, b: &mut Vec<u8>, words: &[&[u8]]) {
    for _ in 0..1 + rng.below(3) {
        let at = rng.below(b.len() + 1);
        match rng.below(5) {
            0 if at < b.len() => b[at] = rng.next() as u8,
            1 if at < b.len() => b[at] ^= 1 << rng.below(8),
            2 if !words.is_empty() => {
                let w = rng.pick(words);
                b.splice(at..at, w.iter().copied());
            }
            3 => b.truncate(at),
            _ => b.insert(at, rng.next() as u8),
        }
    }
}
