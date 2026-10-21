//! Commands of Claude's that only read — `ls`, `cat`, `grep` and the like — run without a card when every
//! path they name stays in the project (or in a place the user let Claude read, `allow.rs`). One that names
//! a path outside gets the run card, saying what it reads: what's read goes to Claude, and the sandbox
//! doesn't stop reads. Writes and the network are the sandbox's either way, so a command misjudged as
//! read-only can still only write in the project and the temp folders.
//!
//! A line can be several of them (#27): `cd src && grep -n x a.rs | sort | head`, or the same as `sh -c
//! '…'`. `parse` reads it the way the shell would, but only the parts it can judge: words (quoted or not —
//! `grep -E 'a|b'` is one pattern), `cd <dir>`, and `;` `&&` `||` `|` between them. Anything that could
//! make a word say something else, or write a file, isn't read at all: `$`, backticks, `\`, `(`, `{`, `<`,
//! a redirect to a file (`2>&1` and `>/dev/null` are fine), a lone `&`, `~name`, an assignment, a newline,
//! and an `sh -c` inside the body — `unreadable` says which. `~` and `~/…` are home, as the shell fills
//! them in. A glob is fine where its fixed folder is read; a `cd` into one where it names one folder.

use std::path::{Path, PathBuf};

use crate::allow::Allowed;

/// One word of a line, quotes taken off; `glob`: it has an unquoted `*` `?` `[` (the shell fills it in).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Word {
    pub text: String,
    pub glob: bool,
}

/// One part of a line: a command's words, or a `cd` into a folder (a glob: the one folder it names —
/// `cd_into`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Part {
    Cd(Word),
    Cmd(Vec<Word>),
}

/// Shells whose `-c '<body>'` is read as the body (once — not one inside another).
const SHELLS: [&str; 4] = ["sh", "bash", "zsh", "dash"];

/// The parts of `line` (an `sh -c '…'` around it taken off), if it's only words, `cd` and `;` `&&` `||` `|`
/// — None for anything else (see the module's doc).
pub fn parse(line: &str) -> Option<Vec<Part>> {
    split(line, false).ok().map(|(parts, _)| parts)
}

/// What in `line` `parse` doesn't read, as a run card says it ("`$(…)` makes words as it runs") — None
/// when it reads it.
pub fn unreadable(line: &str) -> Option<&'static str> {
    split(line, false).err()
}

/// The commands of `line` when it's one command piped into others (`a 2>&1 | b | c`) and nothing else —
/// every part a command, every separator a single `|`.
pub fn piped(line: &str) -> Option<Vec<Vec<Word>>> {
    let (parts, piped) = split(line, false).ok()?;
    if !piped.iter().all(|p| *p) {
        return None;
    }
    parts
        .into_iter()
        .map(|p| match p {
            Part::Cmd(w) => Some(w),
            Part::Cd(_) => None,
        })
        .collect()
}

/// The parts of `line` before its first single `|` — what runs, not what only filters its output (`cargo
/// test x 2>&1 | tail`: `cargo test x`) — a command's leading `NAME=value` words kept. None as `parse`.
pub fn before_pipe(line: &str) -> Option<Vec<Part>> {
    let (parts, piped) = split(line, true).ok()?;
    let n = piped.iter().position(|p| *p).map_or(parts.len(), |i| i + 1);
    Some(parts.into_iter().take(n).collect())
}

/// The parts of `line` as `parse` reads them, a command's leading `NAME=value` words kept as its first words,
/// and between each two whether it's a single `|` — what a run card's `a` keys on (`tools::again`).
pub fn parse_env(line: &str) -> Option<(Vec<Part>, Vec<bool>)> {
    split(line, true).ok()
}

/// What `split` reads, or what in the line it doesn't (`unreadable`).
type Split = Result<(Vec<Part>, Vec<bool>), &'static str>;

/// The line's parts (an `sh -c '…'` around it taken off) and, between each two, whether it's a single `|`.
/// `env`: a command's leading `NAME=value` words are kept as its first words (else such a line is Err).
fn split(line: &str, env: bool) -> Split {
    split_with(line, env, false)
}

/// `split`, and with `vars` a variable's use (`$f`, `"${f}"`) kept in its word as a mark (`VAR`) for
/// `unrolled` to fill in — a line of loops.
fn split_with(line: &str, env: bool, vars: bool) -> Split {
    let outer = parts(line, env, vars)?;
    if let [Part::Cmd(w)] = &outer.0[..]
        && let [shell, c, body] = &w[..]
        && SHELLS.contains(&shell.text.as_str())
        && c.text == "-c"
        && !body.glob
    {
        let inner = parts(&body.text, env, vars)?;
        // (an `sh -c` inside that one isn't read)
        let nested =
            inner.0.iter().any(|p| matches!(p, Part::Cmd(w) if SHELLS.contains(&w[0].text.as_str())));
        return match nested {
            true => Err("an `sh -c` inside an `sh -c`"),
            false => Ok(inner),
        };
    }
    Ok(outer)
}

/// The `$(…)` words a line may hold — the built-in packs' `[claude] expand` (#112: `$(rustc --print
/// sysroot)`, the toolchain's sources' root as Claude spells it), a closed set: kept in its word as written,
/// and filled in with its folder where the word is judged (`outside` — `allow::expanded`) — unknown, the line
/// asks.
pub fn expansions() -> impl Iterator<Item = &'static str> {
    crate::sandboxpack::get().expand.iter().map(|e| e.word.as_str())
}

/// Where in `word` one of `expansions` starts, and which.
fn expansion_in(word: &str) -> Option<(usize, &'static str)> {
    expansions().filter_map(|e| Some((word.find(e)?, e))).min()
}

/// A variable's use in a word (`split_with`'s `vars`): `VAR.0`, `q` (in double quotes) or `u`, its name,
/// `VAR.1`. Characters no line of Claude's has (one that does isn't read that way).
const VAR: (char, char) = ('\u{E000}', '\u{E001}');

/// What a `$` starts, the rest of the line after it in `chars`: one of `expansions`, or with `vars` a
/// variable's name (`$f`, `${f}`) as its mark — taken from `chars`. None for anything else (`$(…)`, `$1`,
/// `$?` …).
fn dollar(chars: &mut std::iter::Peekable<std::str::Chars>, vars: bool, quoted: bool) -> Option<String> {
    for e in expansions() {
        let rest = &e[1..];
        if chars.clone().take(rest.chars().count()).eq(rest.chars()) {
            chars.nth(rest.chars().count() - 1);
            return Some(e.to_string());
        }
    }
    if !vars {
        return None;
    }
    let braced = chars.peek() == Some(&'{');
    if braced {
        chars.next();
    }
    let mut name = String::new();
    while let Some(&c) = chars.peek().filter(|c| c.is_ascii_alphanumeric() || **c == '_') {
        name.push(c);
        chars.next();
    }
    if braced && chars.next() != Some('}') || !env_name(&name) {
        return None;
    }
    Some(format!("{}{}{name}{}", VAR.0, if quoted { 'q' } else { 'u' }, VAR.1))
}

/// The line's parts and their separators (`split`), without looking for an `sh -c` around them. `env`:
/// leading `NAME=value` words stay in front of a command's words; `vars`: see `split_with`.
fn parts(line: &str, env: bool, vars: bool) -> Split {
    if line.contains([VAR.0, VAR.1]) {
        return Err("characters no command has");
    }
    let mut parts = Vec::new();
    let mut piped = Vec::new();
    let mut words: Vec<Word> = Vec::new();
    let mut word: Option<Word> = None;
    // The word in hand: it had a quote · it's an assignment (an unquoted name, then `=`); and how many of
    // the part's words so far are assignments in front
    let mut at = Assign::default();
    let end_word = |word: &mut Option<Word>, words: &mut Vec<Word>, at: &mut Assign| {
        if let Some(w) = word.take() {
            if at.assigns && at.lead == words.len() {
                at.lead += 1;
            }
            words.push(w);
        }
        (at.quoted, at.assigns) = (false, false);
    };
    let end_part = |words: &mut Vec<Word>, parts: &mut Vec<Part>, at: &mut Assign| -> Result<(), &str> {
        let w = std::mem::take(words);
        let lead = std::mem::take(&mut at.lead);
        let first = w.get(if env { lead } else { 0 }).ok_or("an empty command")?;
        // An assignment where the command goes (`PAGER=sh git log`) isn't a command's word
        if first.text.contains('=') {
            return Err("it sets a variable");
        }
        if first.glob {
            return Err("a pattern where the program's name goes");
        }
        if lead > 0 && first.text == "cd" {
            return Err("names set in front of `cd`");
        }
        parts.push(match &w[..] {
            [cd, dir] if cd.text == "cd" && !dir.text.starts_with('-') => Part::Cd(dir.clone()),
            [cd, ..] if cd.text == "cd" => return Err("`cd` to no folder, or with options"),
            _ => Part::Cmd(w),
        });
        Ok(())
    };
    const UNCLOSED: &str = "a quote that isn't closed";
    // (what a `$` starts that `dollar` doesn't read)
    let dollar_why = |chars: &std::iter::Peekable<std::str::Chars>| match chars.clone().next() {
        Some('(') => "`$(…)` makes its words as it runs",
        _ => "a `$` the shell fills in as it runs",
    };
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => end_word(&mut word, &mut words, &mut at),
            '\'' => {
                at.quoted = true;
                let w = word.get_or_insert(Word { text: String::new(), glob: false });
                loop {
                    match chars.next().ok_or(UNCLOSED)? {
                        '\'' => break,
                        c => w.text.push(c),
                    }
                }
            }
            '"' => {
                at.quoted = true;
                let w = word.get_or_insert(Word { text: String::new(), glob: false });
                loop {
                    match chars.next().ok_or(UNCLOSED)? {
                        '"' => break,
                        // A `$` nothing can follow as a name (a regex's end: `"^.*note$"`, `"a$|b"` — #92) is
                        // itself, as sh reads it; before anything else it may expand
                        '$' if chars.peek().is_some_and(|c| " \t\"|)/.,:]".contains(*c)) => w.text.push('$'),
                        '$' => {
                            let why = dollar_why(&chars);
                            w.text.push_str(&dollar(&mut chars, vars, true).ok_or(why)?)
                        }
                        '`' => return Err("backticks make its words as it runs"),
                        // As sh reads it in double quotes: `\` takes `$ ` " \` as themselves, and stays
                        // before anything else (#80: `rg "sleep\(|a *[<>]"` went to a card) — a line
                        // continuation isn't read
                        '\\' => match chars.next().ok_or(UNCLOSED)? {
                            '\n' => return Err("more than one line"),
                            c @ ('$' | '`' | '"' | '\\') => w.text.push(c),
                            c => {
                                w.text.push('\\');
                                w.text.push(c);
                            }
                        },
                        c => w.text.push(c),
                    }
                }
            }
            ';' | '|' | '&' => {
                end_word(&mut word, &mut words, &mut at);
                let pipe = match (c, chars.peek()) {
                    ('&', Some('&')) | ('|', Some('|')) => {
                        chars.next();
                        false
                    }
                    ('&', _) => return Err("a lone `&` runs it in the background"),
                    (c, _) => c == '|',
                };
                end_part(&mut words, &mut parts, &mut at)?;
                piped.push(pipe);
            }
            '>' => {
                const WRITES: &str = "a `>` writes to a file";
                // Only to nowhere: `2>&1`, `>/dev/null`, `2>/dev/null` (the fd as the word before it)
                if word.as_ref().is_some_and(|w| !matches!(w.text.as_str(), "1" | "2")) {
                    return Err(WRITES);
                }
                word = None;
                (at.quoted, at.assigns) = (false, false);
                if chars.peek() == Some(&'>') {
                    chars.next();
                }
                while chars.peek() == Some(&' ') {
                    chars.next();
                }
                let mut target = String::new();
                if chars.peek() == Some(&'&') {
                    target.push('&');
                    chars.next();
                }
                while let Some(&c) = chars.peek().filter(|c| !matches!(c, ' ' | ';' | '|' | '&')) {
                    target.push(c);
                    chars.next();
                }
                if !matches!(target.as_str(), "&1" | "&2" | "/dev/null") {
                    return Err(WRITES);
                }
            }
            '$' => {
                let why = dollar_why(&chars);
                let s = dollar(&mut chars, vars, false).ok_or(why)?;
                word.get_or_insert(Word { text: String::new(), glob: false }).text.push_str(&s);
            }
            '`' => return Err("backticks make its words as it runs"),
            '\\' => return Err("a `\\` outside quotes"),
            '(' | ')' => return Err("parentheses (a subshell)"),
            '{' | '}' => return Err("braces"),
            '<' => return Err("a `<` feeds it a file"),
            '!' => return Err("a `!`"),
            '\n' | '\r' => return Err("more than one line"),
            '#' if word.is_none() => return Err("a `#` comment"),
            // `~` and `~/…` are home, as the shell fills them in (#164: `ls ~/.cargo/registry/src/*/` asked);
            // `~name` is someone's, and a home with no name or not text can't be said
            '~' if word.is_none() => {
                let ends = chars.peek().is_none_or(|c| matches!(c, '/' | ' ' | '\t' | ';' | '|' | '&'));
                let home = crate::allow::home().and_then(|h| h.to_str().map(String::from));
                match (ends, home) {
                    (true, Some(h)) => word = Some(Word { text: h, glob: false }),
                    (false, _) => return Err("`~name` — someone else's home"),
                    (true, None) => return Err("a `~` with no home to fill in"),
                }
            }
            c => {
                let w = word.get_or_insert(Word { text: String::new(), glob: false });
                if c == '=' && !at.quoted && !at.assigns && env_name(&w.text) {
                    at.assigns = true;
                }
                w.glob |= matches!(c, '*' | '?' | '[' | ']');
                w.text.push(c);
            }
        }
    }
    end_word(&mut word, &mut words, &mut at);
    end_part(&mut words, &mut parts, &mut at)?;
    Ok((parts, piped))
}

/// The parts of `line` as `parse` reads them — or, for a line of loops (#113: `for f in a b; do wc -l "$f";
/// done`, `find src -name '*.rs' | while read -r f; do head -3 "$f"; done`), unrolled: each `for` body
/// once per value with the variable filled in (a glob value stays a glob, judged by its fixed folder), a
/// `while read` body once with the variable read as "a path from the list" (`./*` where the body runs —
/// see `unrolled`). What only judges a line (`read_only`, `outside`, `runs_outside`) reads it so; what
/// matches a line word for word (checks, patterns, scripts) doesn't — a loop is never a check.
pub fn flat(line: &str) -> Option<Vec<Part>> {
    parse(line).or_else(|| {
        let (parts, piped) = split_with(line, false, true).ok()?;
        unrolled(&parts, &piped)
    })
}

/// Words that start a loop or a shell's own construct — never a command of a line `flat` judges.
const KEYWORDS: [&str; 12] =
    ["for", "while", "until", "do", "done", "if", "then", "else", "elif", "fi", "case", "esac"];

/// Commands a `while read` loop may read its paths from (`lists`), and what they may go through before it
/// (`keeps_lines`).
fn lists(w: &[Word]) -> bool {
    let words: Vec<&str> = w.iter().map(|w| w.text.as_str()).collect();
    let short = |o: &str, allowed: &str| {
        o.len() > 1
            && o.starts_with('-')
            && !o.starts_with("--")
            && o[1..].chars().all(|c| allowed.contains(c))
    };
    match words.split_first() {
        // Bare names (no long form: `-l` shows where a link leads)
        Some((&"ls", args)) => args.iter().all(|a| !a.starts_with('-') || short(a, "1aAd")),
        // Not following links, one path a line (`-print0`, `-ls`, `-printf` print otherwise)
        Some((&"find", args)) => !args.iter().any(|a| {
            matches!(*a, "-L" | "-H" | "-follow" | "-ls" | "-print0" | "-printf" | "-fprint" | "-fprint0")
        }),
        Some((&"git", args)) => match git_after_c(args) {
            ["ls-files", opts @ ..] => opts.iter().all(|o| {
                !o.starts_with('-')
                    || matches!(
                        *o,
                        "-c" | "-o"
                            | "-m"
                            | "-d"
                            | "--cached"
                            | "--others"
                            | "--modified"
                            | "--deleted"
                            | "--exclude-standard"
                    )
            }),
            _ => false,
        },
        // Only the names of files (that match), links not followed
        Some((&("rg" | "grep"), args)) => {
            let names = |a: &&str| {
                matches!(*a, "--files" | "--files-with-matches") || (short(a, "lriEFws") && a.contains('l'))
            };
            args.iter().any(names)
                && args.iter().all(|a| {
                    !a.starts_with('-')
                        || matches!(*a, "--files" | "--files-with-matches" | "--hidden")
                        || short(a, "lriEFws")
                })
        }
        _ => false,
    }
}

/// A filter that passes lines on whole (or drops them) — between a list and a `while read` loop.
fn keeps_lines(w: &[Word]) -> bool {
    let words: Vec<&str> = w.iter().map(|w| w.text.as_str()).collect();
    match words.split_first() {
        Some((&("sort" | "head" | "tail"), args)) => {
            only_reads(w) && !args.iter().any(|a| a.starts_with("-k"))
        }
        Some((&"grep", args)) => {
            only_reads(w)
                && args.iter().all(|a| !a.starts_with('-') || matches!(*a, "-v" | "-E" | "-F" | "-i" | "-e"))
        }
        _ => false,
    }
}

/// `flat` for the parts of a line read with variables: its loops unrolled. None for anything it can't
/// say what it reads: a loop inside a loop, a variable no loop sets, a `for` value with a variable in it,
/// a `for` value with a space used unquoted (split into more words), a `while read` not fed by a list
/// (`lists` — what a file says could name anything) or using its variable unquoted, or with `..` after it.
fn unrolled(parts: &[Part], piped: &[bool]) -> Option<Vec<Part>> {
    let first = |p: &Part| match p {
        Part::Cmd(w) => Some(w[0].text.clone()),
        Part::Cd(_) => None,
    };
    let has_var = |p: &Part| match p {
        Part::Cmd(w) => w.iter().any(|w| w.text.contains(VAR.0)),
        Part::Cd(d) => d.text.contains(VAR.0),
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i < parts.len() {
        let Part::Cmd(w) = &parts[i] else {
            if has_var(&parts[i]) {
                return None;
            }
            out.push(parts[i].clone());
            i += 1;
            continue;
        };
        let head: Vec<&str> = w.iter().map(|w| w.text.as_str()).collect();
        // The loop's head, its separator `;` (not `|`), then `do …` up to `done`
        let (var, source) = match head.as_slice() {
            ["for", name, "in", ..] if env_name(name) => (name.to_string(), Source::Values(w[3..].to_vec())),
            ["while", rest @ ..] => {
                let rest = match rest {
                    ["IFS=", rest @ ..] => rest,
                    r => r,
                };
                let name = match rest {
                    ["read", "-r", name] | ["read", name] if env_name(name) => name.to_string(),
                    _ => return None,
                };
                // Fed by a list, through filters that keep its lines whole
                let mut at = i;
                loop {
                    at = at.checked_sub(1)?;
                    if !*piped.get(at)? {
                        return None;
                    }
                    let Part::Cmd(fw) = &parts[at] else { return None };
                    if lists(fw) {
                        break;
                    }
                    if !keeps_lines(fw) {
                        return None;
                    }
                }
                (name, Source::Listed)
            }
            [kw, ..] if KEYWORDS.contains(kw) => return None,
            _ => {
                if has_var(&parts[i]) {
                    return None;
                }
                out.push(parts[i].clone());
                i += 1;
                continue;
            }
        };
        if *piped.get(i)? {
            return None;
        }
        // The body: `do`'s own words (if any) are its first command, then each part up to `[done]`
        let mut body = Vec::new();
        match parts.get(i + 1)? {
            Part::Cmd(d) if d[0].text == "do" => {
                if d.len() > 1 {
                    body.push(Part::Cmd(d[1..].to_vec()));
                }
            }
            _ => return None,
        }
        let mut j = i + 2;
        loop {
            let p = parts.get(j)?;
            if matches!(p, Part::Cmd(d) if d.len() == 1 && d[0].text == "done") {
                break;
            }
            if first(p).is_some_and(|f| KEYWORDS.contains(&f.as_str())) {
                return None;
            }
            body.push(p.clone());
            j += 1;
        }
        if body.is_empty()
            || first(&body[0]).is_some_and(|f| KEYWORDS.contains(&f.as_str()))
            || *piped.get(j - 1)?
        {
            return None;
        }
        match source {
            // (no values: the body would go unjudged)
            Source::Values(values) if values.is_empty() => return None,
            Source::Values(values) => {
                for v in &values {
                    if v.text.contains(VAR.0) {
                        return None;
                    }
                    for p in &body {
                        out.push(fill(p, &var, Some(v))?);
                    }
                }
            }
            Source::Listed => {
                for p in &body {
                    out.push(fill(p, &var, None)?);
                }
            }
        }
        i = j + 1;
    }
    Some(out)
}

/// Where a loop's variable takes its values: the `for`'s words, or the lines of a list (`while read`).
enum Source {
    Values(Vec<Word>),
    Listed,
}

/// `p` with `var`'s uses filled in: `value` (a `for`'s) — or, a list's line (None), as a glob of where the
/// body runs (`./*`, its fixed folder `.`), only when used in quotes at a word's start without `..` after.
/// None for any other variable, or a use that can't be told (see `unrolled`).
fn fill(p: &Part, var: &str, value: Option<&Word>) -> Option<Part> {
    let one = |text: &str, glob: bool| -> Option<Word> {
        let mut out = String::new();
        let mut glob = glob;
        let mut rest = text;
        while let Some(at) = rest.find(VAR.0) {
            out.push_str(&rest[..at]);
            let after = &rest[at + VAR.0.len_utf8()..];
            let end = after.find(VAR.1)?;
            let (quoted, name) = (after.starts_with('q'), &after[1..end]);
            if name != var {
                return None;
            }
            rest = &after[end + VAR.1.len_utf8()..];
            match value {
                Some(v) => {
                    let wild = v.text.contains(['*', '?', '[', ']']);
                    if !quoted && v.text.contains(char::is_whitespace) {
                        return None;
                    }
                    glob |= v.glob || (!quoted && wild);
                    out.push_str(&v.text);
                }
                None => {
                    let dots = Path::new(rest).components().any(|c| c == std::path::Component::ParentDir);
                    if !quoted || !out.is_empty() || dots || rest.contains(VAR.0) {
                        return None;
                    }
                    glob = true;
                    out.push_str("./*");
                }
            }
        }
        out.push_str(rest);
        Some(Word { text: out, glob })
    };
    Some(match p {
        // (a loop's value as a folder to go into: never a glob — a list's line isn't what `./*` names)
        Part::Cd(d) => {
            let w = one(&d.text, d.glob)?;
            if w.glob {
                return None;
            }
            Part::Cd(w)
        }
        Part::Cmd(w) => Part::Cmd(w.iter().map(|w| one(&w.text, w.glob)).collect::<Option<_>>()?),
    })
}

/// What `parts` knows of the word in hand and the part's leading assignments.
#[derive(Default)]
struct Assign {
    quoted: bool,
    assigns: bool,
    lead: usize,
}

/// Whether `s` is a shell variable's name (`[A-Za-z_][A-Za-z0-9_]*`).
fn env_name(s: &str) -> bool {
    s.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Names a command's leading assignments may never set and still match a chat's pattern (#44): what
/// finds programs and libraries, what shells read at their start, what the sandbox sets or reads
/// (`sandbox::inside_env`, its home and temp folders), and what makes a program run another program or
/// load code (rustc and cargo's wrappers, runners, linkers and flags — `-C linker=` runs one —, git's ssh,
/// helpers and config, interpreters' and JVMs' startup options and paths, editors, pagers and password askers). A
/// name ending in `_` is a prefix, one starting with `_` a suffix (`CARGO_TARGET_<triple>_RUNNER`). Also
/// what picks the toolchain, cargo's config and aliases, or git's repository. (A pattern takes only the
/// names its card showed — #132 — so this list needn't be whole; it keeps such a name off any card.)
const ENV_NEVER: [&str; 48] = [
    "CARGO_HOME",
    "CARGO_ALIAS_",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
    "GIT_DIR",
    "GIT_WORK_TREE",
    "PATH",
    "HOME",
    "TMPDIR",
    crate::sandbox::MARK,
    "GIT_OPTIONAL_LOCKS",
    "BASH_ENV",
    "ENV",
    "IFS",
    "SHELLOPTS",
    "LD_",
    "DYLD_",
    "RUSTC",
    "RUSTDOC",
    "RUSTFLAGS",
    "RUSTDOCFLAGS",
    "CARGO_BUILD_RUSTC",
    "CARGO_BUILD_RUSTDOC",
    "_WRAPPER",
    "_RUNNER",
    "_LINKER",
    "_RUSTFLAGS",
    "_RUSTDOCFLAGS",
    "GIT_SSH",
    "GIT_SSH_COMMAND",
    "GIT_EXEC_PATH",
    "GIT_EXTERNAL_DIFF",
    "GIT_CONFIG",
    "GIT_CONFIG_",
    "NODE_OPTIONS",
    // (the `java` launcher's options and HotSpot's — `JAVA_TOOL_OPTIONS` is the Java pack's own `env`, kept off
    // patterns by `env_allowed`; `_JAVA_OPTIONS` is a suffix here: it covers `JDK_JAVA_OPTIONS` too, named for
    // finding it)
    "JDK_JAVA_OPTIONS",
    "_JAVA_OPTIONS",
    "PYTHONPATH",
    "PYTHONSTARTUP",
    "PERL5OPT",
    "PERL5LIB",
    "RUBYOPT",
    "EDITOR",
    "VISUAL",
    "PAGER",
    "_PAGER",
    "_EDITOR",
    "_ASKPASS",
];

/// Whether a pattern may carry `name` among its assignments: not one of `ENV_NEVER`, nor a variable the
/// built-in packs set or read for a run (a tool's home, a run's own home, their `env`, a setting's file —
/// `sandboxpack::Packs::env_names`: `GRADLE_USER_HOME=~/.gradle` would point a run back at the user's).
pub fn env_allowed(name: &str) -> bool {
    let packs = crate::sandboxpack::get();
    env_core(name)
        && !packs.env_names().any(|n| n == name)
        && !packs.env_on.iter().any(|e| e.name == name)
        && !packs.any_case.iter().any(|a| a.eq_ignore_ascii_case(name))
}

/// `env_allowed` with `ENV_NEVER` alone — what the packs' own variables are checked against as they're read.
pub fn env_core(name: &str) -> bool {
    env_name(name)
        && !ENV_NEVER.iter().any(|n| {
            if n.ends_with('_') {
                name.starts_with(n)
            } else if n.starts_with('_') {
                name.ends_with(n)
            } else {
                name == *n
            }
        })
}

/// `one_command` with `NAME=value` words in front (`A=1 B=x cargo xtask shot …`, in an `sh -c` or not):
/// the names set, in order, and the command's words. None as for `one_command`.
pub fn one_command_env(line: &str) -> Option<(Vec<String>, Vec<String>)> {
    let (parts, _) = split(line, true).ok()?;
    let [Part::Cmd(w)] = &parts[..] else { return None };
    if w.iter().any(|w| w.glob) {
        return None;
    }
    env_words(&w.iter().map(|w| w.text.clone()).collect::<Vec<_>>())
}

/// A command's words with the names it sets in front taken off — `NAME=value` words, and `env NAME=value …`
/// read the same (#93: `env A=1 cargo xtask shot` is `A=1 cargo xtask shot`): the names, in order, and the
/// command's words. None for `env` with an option (`-i`, `-S` splits a string into more words) or with no
/// command after it.
pub fn env_words(w: &[String]) -> Option<(Vec<String>, Vec<String>)> {
    let assign = |s: &String| s.split_once('=').filter(|(n, _)| env_name(n)).map(|(n, _)| n.to_string());
    let mut names: Vec<String> = w.iter().map_while(assign).collect();
    let mut rest = &w[names.len()..];
    if rest.first().is_some_and(|x| x == "env") {
        let more: Vec<String> = rest[1..].iter().map_while(assign).collect();
        rest = &rest[1 + more.len()..];
        names.extend(more);
        if rest.first().is_none_or(|x| x.starts_with('-')) {
            return None;
        }
    }
    Some((names, rest.to_vec()))
}

/// The script a command runs by its path — `sh target/x/setup.sh …`, `bash x.sh`, `./run.sh …` — as
/// written: not a glob, not an option (`sh -c`, `sh -x x.sh`). What's in it decides what runs: a chat
/// allows it as it is (`script_pins`). Never a program by its path (review: a pinned `/bin/sh` let `/bin/sh
/// -c '<anything>'` run unasked — a binary's hash never changes): not a runner whatever its folder
/// (`tools::RUNNERS` — `/bin/sh`, `./node_modules/.bin/sh`, `/usr/bin/env`, python…), not an absolute path,
/// not one through `..` (the checkout's own scripts only); run by its path (`./run`, no shell), it ends in
/// `.sh`, `.bash` or `.zsh` or is a relative path whose `#!` the worker reads (`script_pins`).
pub fn script_part(w: &[Word]) -> Option<&str> {
    let (first, rest) = w.split_first()?;
    let path = match first.text.as_str() {
        s if SHELLS.contains(&s) => rest.first()?,
        s if s.contains('/') => first,
        _ => return None,
    };
    let p = Path::new(&path.text);
    let runner = p.file_name().and_then(|n| n.to_str()).is_some_and(crate::tools::runner);
    let escapes = p.is_absolute() || p.components().any(|c| c == std::path::Component::ParentDir);
    let usable = !path.glob && !path.text.is_empty() && !path.text.starts_with('-');
    (usable && !runner && !escapes).then_some(path.text.as_str())
}

/// Endings a script may have; one without them must start with `#!`.
const SCRIPT_ENDINGS: [&str; 3] = ["sh", "bash", "zsh"];

/// The scripts `cmd` runs by path (`script_part`), each where it runs (after a `cd`) by its real path, with
/// a hash of what's in it — on a worker. None: a line `parse` doesn't read, or a script that isn't there,
/// isn't inside `root` (the session's checkout — a link out of it isn't), or isn't a text script: a NUL in
/// it (a binary) or neither `#!` at its start nor a script's ending.
pub fn script_pins(cmd: &str, cwd: &Path, root: &Path) -> Option<Vec<(PathBuf, u64)>> {
    use std::hash::{Hash, Hasher};
    let mut here = cwd.to_path_buf();
    let mut out = Vec::new();
    for part in parse(cmd)? {
        match part {
            Part::Cd(d) => here = cd_into(&here, &d, &[]).ok()?,
            Part::Cmd(w) => {
                let Some(path) = script_part(&w) else { continue };
                let real = std::fs::canonicalize(here.join(path)).ok()?;
                if !real.starts_with(root) {
                    return None;
                }
                let text = std::fs::read(&real).ok()?;
                let ending =
                    real.extension().and_then(|e| e.to_str()).is_some_and(|e| SCRIPT_ENDINGS.contains(&e));
                if text.contains(&0) || !(text.starts_with(b"#!") || ending) {
                    return None;
                }
                let mut h = std::collections::hash_map::DefaultHasher::new();
                text.hash(&mut h);
                out.push((real, h.finish()));
            }
        }
    }
    Some(out)
}

/// What a command `a` allowed is pinned to (#93, #210): the scripts it runs by their path (`script_pins`) and
/// the files the packs say it runs that its words don't name (`entry_pins`). None: one can't be pinned — the
/// command runs this once only.
pub fn run_pins(cmd: &str, cwd: &Path, root: &Path) -> Option<Vec<(PathBuf, u64)>> {
    let mut pins = script_pins(cmd, cwd, root)?;
    pins.extend(entry_pins_in(cmd, root, &crate::sandboxpack::get().entry, ENTRY_PIN_MAX)?);
    Some(pins)
}

/// The most of an entry's files hashed for a pin (a bigger one isn't pinned: its command runs once only).
const ENTRY_PIN_MAX: u64 = 64 << 20;

/// The packs' `entry` files `cmd` runs (`sinceyes::entry_files`: `cargo xtask` runs `xtask/`), each by its real
/// path with a hash of what's in it — a folder: every file (its bytes and mode) and link (where it points) in it,
/// by name — on a worker. #210: `a` on `cargo xtask linux test` with Docker's socket let an `xtask/` edited since
/// run with it unasked. One not there is pinned as not there. None: one is outside `root` (the session's
/// checkout), or more than `max` bytes.
pub fn entry_pins_in(
    cmd: &str,
    root: &Path,
    entries: &[crate::sandboxpack::Entry],
    max: u64,
) -> Option<Vec<(PathBuf, u64)>> {
    use std::hash::{Hash, Hasher};
    let mut left = max;
    let mut out = Vec::new();
    for f in crate::sinceyes::entry_files_in(cmd, entries) {
        let at = root.join(&f);
        let mut h = std::collections::hash_map::DefaultHasher::new();
        let real = match std::fs::canonicalize(&at) {
            Ok(real) if real.starts_with(root) => {
                tree_hash(&real, &mut h, &mut left)?;
                real
            }
            Ok(_) => return None,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                "not there".hash(&mut h);
                at
            }
            Err(_) => return None,
        };
        out.push((real, h.finish()));
    }
    Some(out)
}

/// `p`'s content into `h` (`entry_pins_in`), `left` bytes still allowed — None past them, or unreadable.
fn tree_hash(p: &Path, h: &mut impl std::hash::Hasher, left: &mut u64) -> Option<()> {
    use std::hash::Hash;
    use std::os::unix::fs::PermissionsExt;
    let m = std::fs::symlink_metadata(p).ok()?;
    if m.file_type().is_symlink() {
        std::fs::read_link(p).ok()?.hash(h);
    } else if m.is_dir() {
        let mut names: Vec<std::ffi::OsString> =
            std::fs::read_dir(p).ok()?.map(|e| e.map(|e| e.file_name())).collect::<Result<_, _>>().ok()?;
        crate::sorting::sort(&mut names);
        for n in names {
            n.hash(h);
            tree_hash(&p.join(&n), h, left)?;
        }
    } else {
        *left = left.checked_sub(m.len())?;
        m.permissions().mode().hash(h);
        std::fs::read(p).ok()?.hash(h);
    }
    Some(())
}

/// The words of `line` when it's one command and nothing else (quotes taken off, no glob) — what runs,
/// word for word: the checks and a chat's patterns are matched against these.
pub fn one_command(line: &str) -> Option<Vec<String>> {
    match &parse(line)?[..] {
        [Part::Cmd(w)] if !w.iter().any(|w| w.glob) => Some(w.iter().map(|w| w.text.clone()).collect()),
        _ => None,
    }
}

/// The commands that only read (with their options below), by name — `nm` `otool` `strings` `size` read
/// a binary's tables and text (#116).
const COMMANDS: [&str; 21] = [
    "ls", "cat", "head", "tail", "wc", "grep", "rg", "find", "tree", "stat", "file", "du", "diff", "sort",
    "uniq", "cut", "pwd", "nm", "otool", "strings", "size",
];

/// Commands that read nothing — what they say is their words (`cargo clippy -q && echo clean`): none of
/// them a path.
const SAYS: [&str; 4] = ["echo", "printf", "true", "false"];

/// Commands that read only what's piped in — no word of theirs names a file (`tr`'s are sets of characters).
const STDIN_ONLY: [&str; 1] = ["tr"];

/// Whether a command only says its words (`SAYS`).
pub fn says(w: &[Word]) -> bool {
    w.first().is_some_and(|w| SAYS.contains(&w.text.as_str()))
}

/// git's that only read — `branch` only bare (with words it creates, renames, deletes); `show` reads the
/// repository (`git show :2:src/a.rs`, `HEAD~1:a`, #110); `diff --no-index` the files it names (judged as
/// reads, `outside` — never a check's unasked run).
const GIT: [&str; 4] = ["blame", "rev-parse", "ls-files", "show"];

/// Whether `cmd` only reads: a line `flat` reads (loops unrolled), each command in it one above with none
/// of the options that make it write a file, run a program, or never end. Where it reads is `outside`'s.
pub fn read_only(cmd: &str) -> bool {
    let Some(parts) = flat(cmd) else { return false };
    parts.iter().all(|p| match p {
        Part::Cd(_) => true,
        Part::Cmd(w) => only_reads(w),
    })
}

/// What awk can do past reading its files and printing, by name — anywhere in any of its words, even in a
/// string (no tokenizing to get wrong): run a command (`system`), read or write through a pipe or a file
/// (`getline`, and `close`/`fflush` on one), read the environment (`ENVIRON`, `PROCINFO`), read a file its
/// words don't name (`ARGV`/`ARGC` set in the program; `SYMTAB` reaches them by name), load code (`@load`,
/// `@include`, `@f()`).
const AWK_NEVER: [&str; 10] =
    ["system", "getline", "close", "fflush", "ENVIRON", "PROCINFO", "ARGV", "ARGC", "SYMTAB", "@"];

/// `awk` that only reads and prints to its output (#207): options `-F` and `-v` only (no `-f` file of code,
/// `-i`/`-l` loading more, `-i inplace`), one program, then the files it reads (`files`; never a
/// `name=value` operand) — none of `AWK_NEVER`, and no `>` or `|` after the first `print`/`printf`. A
/// redirect (`> f`, `>> f`, `| "cmd"`, `|& "cmd"`) only ever follows one of those in the program's text (a
/// pipe into awk needs `getline`), so a `>` before it is a comparison (`NR>=702`) — and after it, even a
/// comparison asks.
pub fn awk_reads(args: &[&str], files: bool) -> bool {
    let mut program = None;
    let mut i = 0;
    while i < args.len() {
        match args[i] {
            "-F" | "-v" if i + 1 < args.len() => i += 1,
            a if (a.starts_with("-F") || a.starts_with("-v")) && a.len() > 2 => {}
            a if a.starts_with('-') => return false,
            a if program.is_none() => program = Some(a),
            a if files && !a.contains('=') => {}
            _ => return false,
        }
        i += 1;
    }
    let Some(p) = program else { return false };
    if args.iter().any(|a| AWK_NEVER.iter().any(|n| a.contains(n))) {
        return false;
    }
    !p[p.find("print").unwrap_or(p.len())..].contains(['>', '|'])
}

/// Programs that move, copy or remove files and run none of them; git's below (#76).
const MOVES: [&str; 7] = ["mv", "cp", "rm", "ln", "mkdir", "rmdir", "touch"];
const GIT_MOVES: [&str; 3] = ["mv", "rm", "add"];

/// Whether every part of `line` only moves, copies, removes or reads files (a `cd` aside): nothing in it
/// runs what a file says — `mv src/a.rs src/b.rs`, `git mv a b && ls` (#76). What it writes is the
/// sandbox's and its card's.
pub fn only_moves(line: &str) -> bool {
    let Some(parts) = parse(line) else { return false };
    parts.iter().all(|p| match p {
        Part::Cd(_) => true,
        Part::Cmd(w) => {
            let words: Vec<&str> = w.iter().map(|w| w.text.as_str()).collect();
            only_reads(w)
                || match words.split_first() {
                    Some((&"git", args)) => matches!(git_after_c(args), [sub, ..] if GIT_MOVES.contains(sub)),
                    Some((name, _)) => MOVES.contains(name),
                    None => false,
                }
        }
    })
}

/// Whether one command only reads (`read_only`'s rule for each part of a line).
pub fn only_reads(w: &[Word]) -> bool {
    let words: Vec<&str> = w.iter().map(|w| w.text.as_str()).collect();
    !words.iter().any(|w| crate::tools::WRITES.iter().any(|o| w.starts_with(o))) && reads_only(&words)
}

impl AsRef<str> for Word {
    fn as_ref(&self) -> &str {
        &self.text
    }
}

/// git's options before its subcommand that change neither what runs nor where (#88: `git
/// --no-optional-locks status` went to a card) — read as if they weren't there. Not `-c`, `--git-dir`,
/// `--work-tree`, `--exec-path`, `--namespace`, `--config-env`, nor `-p`/`--paginate` (its pager runs):
/// git's words stop at them, so they still ask.
const GIT_QUIET: [&str; 9] = [
    "--no-pager",
    "-P",
    "--no-optional-locks",
    "--literal-pathspecs",
    "--glob-pathspecs",
    "--noglob-pathspecs",
    "--icase-pathspecs",
    "--no-replace-objects",
    "--no-lazy-fetch",
];

/// git's options in front of its subcommand, read: the folders of its `-C <dir>`s, in order, and its words
/// from the subcommand on — the harmless ones (`GIT_QUIET`) passed over, anything else where they stop.
pub fn git_globals<T: AsRef<str>>(mut args: &[T]) -> (Vec<&str>, &[T]) {
    let mut dirs = Vec::new();
    loop {
        match args {
            [c, dir, rest @ ..] if c.as_ref() == "-C" => {
                dirs.push(dir.as_ref());
                args = rest;
            }
            [q, rest @ ..] if GIT_QUIET.contains(&q.as_ref()) => args = rest,
            _ => return (dirs, args),
        }
    }
}

/// git's words after the `-C <dir>`s and harmless options in front of them (`git -C sub log` reads as `git
/// log`, run in `sub` — #57; the folder is a read, `candidates`').
pub fn git_after_c<T: AsRef<str>>(args: &[T]) -> &[T] {
    git_globals(args).1
}

/// One command's words with git's harmless options (`GIT_QUIET`) taken out from before its subcommand — its
/// `-C <dir>`s kept: where it runs is still `read_only`'s to judge (`tools::allowed_words`).
pub fn git_plain(words: &[String]) -> Vec<String> {
    let Some((git, mut args)) = words.split_first().filter(|(g, _)| *g == "git") else {
        return words.to_vec();
    };
    let mut out = vec![git.clone()];
    loop {
        match args {
            [c, dir, rest @ ..] if c == "-C" => {
                out.extend([c.clone(), dir.clone()]);
                args = rest;
            }
            [q, rest @ ..] if GIT_QUIET.contains(&q.as_str()) => args = rest,
            _ => break,
        }
    }
    out.extend(args.iter().cloned());
    out
}

/// `sed` that only prints lines by number or pattern (`sed -n '3,40p'`, `-n '$p'`, #66; `-n '/begin/,/end/p'`,
/// #92): no script that writes (`w`), runs (`e`) or edits in place (`-i`) gets this far — a pattern holding
/// `/`, `,` or `;` isn't read at all.
fn sed_prints(args: &[&str]) -> bool {
    let (opts, rest): (Vec<&str>, Vec<&str>) = args.iter().partition(|w| w.starts_with('-'));
    let Some(script) = rest.first() else { return false };
    let pattern = |a: &str| {
        a.len() >= 2 && a.starts_with('/') && a.ends_with('/') && !a[1..a.len() - 1].contains(['/', ';', ','])
    };
    let line = |a: &str| a == "$" || (!a.is_empty() && a.chars().all(|c| c.is_ascii_digit())) || pattern(a);
    let prints = |item: &str| {
        item.trim().strip_suffix('p').is_some_and(|at| match at.split_once(',') {
            Some((a, b)) => line(a) && line(b),
            None => line(at),
        })
    };
    opts.contains(&"-n")
        && opts.iter().all(|o| matches!(*o, "-n" | "-E" | "-r"))
        && script.split(';').all(prints)
}

/// One command's words: a command above, without the options that make it write, run, or never end.
fn reads_only(words: &[&str]) -> bool {
    let Some((&name, args)) = words.split_first() else { return false };
    // A short option cluster with `c` in it (`-ro` has `-o`)
    let short = |w: &str, c: char| w.starts_with('-') && !w.starts_with("--") && w[1..].contains(c);
    // A long option as getopt_long takes it: any start of its name (`--out=f` is `--output=f`, `--fol` is
    // `--follow`) — an ambiguous one is refused here too, as the tool itself would
    let long = |w: &str, name: &str| {
        w.strip_prefix("--")
            .map(|o| o.split_once('=').map_or(o, |(o, _)| o))
            .is_some_and(|o| !o.is_empty() && name.starts_with(o))
    };
    let any = |f: &dyn Fn(&str) -> bool| args.iter().any(|w| f(w));
    match name {
        "git" => match git_after_c(args) {
            [sub, ..] if GIT.contains(sub) => true,
            ["branch"] => true,
            ["diff", rest @ ..] => rest.contains(&"--no-index"),
            _ => false,
        },
        _ if SAYS.contains(&name) => true,
        "sed" => sed_prints(args),
        "find" => !any(&|w| ["-exec", "-ok", "-delete", "-fprint", "-fls"].iter().any(|o| w.starts_with(o))),
        "sort" => !any(&|w| short(w, 'o') || long(w, "output") || long(w, "compress-program")),
        // (`-R` with `-H` writes a `00Tree.html` in each folder, through a shell)
        "tree" => !any(&|w| short(w, 'o') || short(w, 'R')),
        "file" => !any(&|w| short(w, 'C') || long(w, "compile")),
        "tail" => !any(&|w| short(w, 'f') || short(w, 'F') || long(w, "follow")),
        "rg" => !any(&|w| w.starts_with("--pre")),
        // (only what's piped in; its operands are sets of characters — #202)
        "tr" => crate::filters::reads(words),
        "awk" => awk_reads(args, true),
        // `uniq in out` writes `out` — `uniq - out` too (`-` is the input: an operand, not an option)
        "uniq" => args.iter().filter(|w| **w == "-" || !w.starts_with('-')).count() <= 1,
        // (a script's syntax checked: nothing in it runs — f22 j)
        _ if SHELLS.contains(&name) => syntax_only(args),
        // (it reads the scripts it names and its rc file, runs none of them — but never with what lets it follow a
        // source the line doesn't name: `-x`, a source path, another rc file; an rc file or a directive that turns
        // external sources on is `outside`'s — f22 review)
        "shellcheck" => shellcheck_names_only(args),
        _ => COMMANDS.contains(&name),
    }
}

/// shellcheck's words that let it read nothing but what it's named (f22 review): no `-x`/`--external-sources`,
/// no source path (`-P`, `--source-path`), no other rc file (`--rcfile`) — its line's, and `SHELLCHECK_OPTS`'s
/// (`shellcheck_follows`).
fn shellcheck_names_only(args: &[&str]) -> bool {
    let short = |w: &str, c: char| w.starts_with('-') && !w.starts_with("--") && w[1..].contains(c);
    // (any start of a long option's name, as getopt_long takes it — `reads_only`'s rule)
    let long = |w: &str, name: &str| {
        w.strip_prefix("--")
            .map(|o| o.split_once('=').map_or(o, |(o, _)| o))
            .is_some_and(|o| !o.is_empty() && name.starts_with(o))
    };
    !args.iter().any(|w| {
        short(w, 'x')
            || short(w, 'P')
            || long(w, "external-sources")
            || long(w, "source-path")
            || long(w, "rcfile")
    })
}

/// A shell's words that only check one script's syntax: `-n` (noexec: the script read, nothing of it run), then
/// the script alone — no other option (`-c`'s body, `-i`'s or `-l`'s startup files, `-s`'s input, `-o`), nothing
/// after it.
fn syntax_only(args: &[&str]) -> bool {
    match args {
        [opts @ .., script] => {
            !opts.is_empty()
                && opts.iter().all(|o| *o == "-n")
                && !script.is_empty()
                && !script.starts_with(['-', '+'])
        }
        [] => false,
    }
}

/// The words of one command that could name a path: every one that isn't an option, and an option's own
/// value (`--file=x`, `-f/x`); a glob as its fixed folder (`../*.rs` reads `..`). A word that names nothing
/// (a pattern) is left out by `outside`.
fn candidates(words: &[Word]) -> Vec<String> {
    if words.first().is_some_and(|w| SAYS.contains(&w.text.as_str()) || STDIN_ONLY.contains(&w.text.as_str()))
    {
        return Vec::new();
    }
    let mut rest = words.get(1..).unwrap_or_default();
    // (git: its subcommand names no path; each `-C <dir>`'s folder is read, and git goes there — the rest
    // is where it ends up)
    let mut base = PathBuf::new();
    let mut dirs = Vec::new();
    if words.first().is_some_and(|w| w.text == "git") {
        let (cs, after) = git_globals(rest);
        for dir in cs {
            base = base.join(dir);
            dirs.push(base.to_string_lossy().into_owned());
        }
        rest = after.get(1..).unwrap_or_default();
    }
    let at = |p: String| match base.as_os_str().is_empty() {
        true => p,
        false => base.join(p).to_string_lossy().into_owned(),
    };
    let named = rest
        .iter()
        .filter_map(|w| {
            let path = match w.text.strip_prefix('-') {
                None => w.text.as_str(),
                Some(_) => w
                    .text
                    .split_once('=')
                    .map(|(_, v)| v)
                    // (`-f$(rustc --print sysroot)/…` from the sysroot on — `outside` fills it in)
                    .or_else(|| {
                        let at = expansion_in(&w.text).map(|(i, _)| i).or_else(|| w.text.find('/'));
                        at.map(|i| &w.text[i..])
                    })?,
            };
            if !w.glob {
                return Some(path.to_string());
            }
            let fixed = &path[..path.find(['*', '?', '[', ']']).unwrap_or(path.len())];
            Some(fixed.rfind('/').map_or(".".to_string(), |i| fixed[..i.max(1)].to_string()))
        })
        .filter(|w| !w.is_empty())
        .map(at);
    dirs.into_iter().chain(named).collect()
}

/// Whether a part of `cmd` that runs something would run outside `root` — after a `cd` there, or git with a
/// `-C` there. Anything but a read runs something (a build runs the folder's build scripts), and so does
/// git (a repository's config runs its hooks, fsmonitor, pager): a read allowed outside (`allow.rs`) never
/// lets those run there — code nobody saw (review note). A `cd` that can't be followed counts as outside. On
/// a worker.
pub fn runs_outside(cmd: &str, cwd: &Path, root: &Path, expanded: &[(String, PathBuf)]) -> bool {
    let Some(parts) = flat(cmd) else { return true };
    let inside = |p: &Path| std::fs::canonicalize(p).is_ok_and(|r| r.starts_with(root));
    let mut here = cwd.to_path_buf();
    for part in parts {
        match part {
            Part::Cd(dir) => match cd_into(&here, &dir, expanded) {
                Ok(d) => here = d,
                Err(_) => return true,
            },
            Part::Cmd(w) => {
                let git = w.first().is_some_and(|w| w.text == "git");
                if only_reads(&w) && !git {
                    continue;
                }
                let mut at = here.clone();
                if git {
                    for dir in git_globals(w.get(1..).unwrap_or_default()).0 {
                        at = at.join(dir);
                    }
                }
                if !inside(&at) {
                    return true;
                }
            }
        }
    }
    false
}

/// The places `cmd` reads outside `root` (real paths, links followed) that aren't allowed — on a worker.
/// A word that names nothing there isn't a path; `..` and links are resolved. Each command's words are
/// where it runs: after a `cd`, in that folder (its real path) — which is read too. Err: a `cd` into
/// what isn't a folder that's there (or a glob naming none or several — `cd_into`) — the shell would stay
/// where it was and run the rest (after `;` or `||`) from there, so where those read can't be told: the
/// line is asked about. So is a word naming one of `expansions` when `expanded` (each one's real folder,
/// `allow::expanded`) doesn't know it, and a glob with `..` after its first wildcard (`*/../..` climbs out of
/// what's fixed) — and a shellcheck that would read what it isn't named (`sc`: home's rc files and its options
/// from the environment, `Shellcheck::of_process` on the run path).
pub fn outside_with(
    cmd: &str,
    cwd: &Path,
    root: &Path,
    chat: &Allowed,
    kept: &Allowed,
    expanded: &[(String, PathBuf)],
    sc: &Shellcheck,
) -> Result<Vec<PathBuf>, String> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut here = cwd.to_path_buf();
    for part in flat(cmd).ok_or("a line that can't be read")? {
        let words = match part {
            Part::Cd(dir) => {
                here = cd_into(&here, &dir, expanded)?;
                vec![".".to_string()]
            }
            Part::Cmd(w) => {
                if let Some(w) = w.iter().find(|w| w.glob && climbs(&w.text)) {
                    return Err(format!("{}: `..` after a wildcard — where it leads can't be told", w.text));
                }
                if let Some(why) = shellcheck_follows(&w, &here, &sc.homes, &sc.opts, expanded) {
                    return Err(why);
                }
                candidates(&w)
            }
        };
        for w in words {
            let w = expand(&w, expanded)?;
            let Ok(real) = std::fs::canonicalize(here.join(w)) else { continue };
            if real.starts_with(root)
                || chat.reads(&real)
                || kept.reads(&real)
                || crate::log::is_log_real(&real)
                || out.contains(&real)
            {
                continue;
            }
            out.push(real);
        }
    }
    Ok(out)
}

/// Why shellcheck would read a file its line doesn't name (f22 review) — its options are `reads_only`'s; this is
/// what turns external sources on without them: an rc file it looks for (`shellcheck_rcs`; it reads the first
/// it finds, any of them counts here) that says `external-sources=true` or can't be read, or a script it checks
/// whose own `# shellcheck` directive says it (shellcheck's docs give that key to the rc file; a script's is
/// taken as one all the same) — and the options the run's environment gives it (`opts`: each value of
/// `SHELLCHECK_OPTS` it may see, `shellcheck_env`), judged as its line's words; one that isn't plain words can't
/// be judged. None: it reads only what it's named. `--norc`: no rc file. On a worker.
fn shellcheck_follows(
    w: &[Word],
    here: &Path,
    homes: &[PathBuf],
    opts: &[String],
    expanded: &[(String, PathBuf)],
) -> Option<String> {
    let (first, args) = w.split_first()?;
    if first.text != "shellcheck" {
        return None;
    }
    // (never its value on the card: a name only)
    for o in opts {
        let plain = o.chars().all(|c| c.is_ascii_graphic() || c == ' ' || c == '\t')
            && !o.contains(['\'', '"', '\\', '$', '`']);
        if !plain {
            return Some("shellcheck: SHELLCHECK_OPTS in the run's environment isn't plain words".into());
        }
        if !shellcheck_names_only(&o.split_ascii_whitespace().collect::<Vec<_>>()) {
            return Some(
                "shellcheck: SHELLCHECK_OPTS in the run's environment lets it read what its line doesn't name"
                    .into(),
            );
        }
    }
    // (every word that isn't an option may be a script: an option's value only adds a folder to look in)
    let scripts: Vec<PathBuf> = (args.iter())
        .filter(|a| !a.text.starts_with('-'))
        .filter_map(|a| expand(&a.text, expanded).ok())
        .map(|a| here.join(a))
        .collect();
    for s in &scripts {
        // (a file only: a fifo or a device would never end the read)
        let file = std::fs::metadata(s).is_ok_and(|m| m.is_file());
        if file && std::fs::read_to_string(s).is_ok_and(|t| t.lines().any(directive_turns_on)) {
            let s = s.display();
            return Some(format!(
                "shellcheck: {s} turns external sources on itself — what it sources is read too"
            ));
        }
    }
    if args.iter().any(|a| a.text == "--norc") {
        return None;
    }
    for rc in shellcheck_rcs(here, &scripts, homes) {
        match std::fs::read_to_string(&rc) {
            Ok(t) if t.lines().any(|l| turns_on(l.split('#').next().unwrap_or(""))) => {
                let rc = rc.display();
                return Some(format!(
                    "shellcheck: {rc} turns external sources on — what a script sources is read too"
                ));
            }
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                let rc = rc.display();
                return Some(format!(
                    "shellcheck: {rc} can't be read — whether it turns external sources on can't be told"
                ));
            }
            _ => {}
        }
    }
    None
}

/// Whether an rc file's line (its comment cut off) or a directive's words say `external-sources=true` —
/// spaces and case aside, the safe way.
fn turns_on(s: &str) -> bool {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_lowercase();
    s.contains("external-sources=true")
}

/// A script's line that's a `# shellcheck …` directive turning external sources on.
fn directive_turns_on(line: &str) -> bool {
    let directive = line.trim_start().strip_prefix('#').map(str::trim_start);
    directive.and_then(|d| d.strip_prefix("shellcheck")).is_some_and(turns_on)
}

/// The rc files shellcheck looks for checking `scripts` from `here`: `.shellcheckrc` and `shellcheckrc` in each
/// script's folder (as named and as it really is) and every folder above it — `here`'s too, for what's read from
/// stdin — then `homes`' (`shellcheck_homes`).
fn shellcheck_rcs(here: &Path, scripts: &[PathBuf], homes: &[PathBuf]) -> Vec<PathBuf> {
    let mut starts = vec![here.to_path_buf()];
    for s in scripts {
        starts.extend(s.parent().map(Path::to_path_buf));
        starts.extend(std::fs::canonicalize(s).ok().and_then(|r| r.parent().map(Path::to_path_buf)));
    }
    let mut out: Vec<PathBuf> = Vec::new();
    for dir in starts.iter().flat_map(|s| s.ancestors()) {
        for name in [".shellcheckrc", "shellcheckrc"] {
            let p = dir.join(name);
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out.extend(homes.iter().cloned());
    out
}

/// What shellcheck may read besides its line's words (`shellcheck_follows`): home's rc files, and the values of
/// `SHELLCHECK_OPTS` a run may see. `of_process` is the run path's; a test gives its own (`Default`: none).
#[derive(Clone, Debug, Default)]
pub struct Shellcheck {
    pub homes: Vec<PathBuf>,
    pub opts: Vec<String>,
}

impl Shellcheck {
    /// This process's: `~/.shellcheckrc`, `$XDG_CONFIG_HOME/shellcheckrc` (`~/.config/` without it); and
    /// `SHELLCHECK_OPTS` — tarae's own (a sandboxed run gets it or nothing: `runenv`; one not UTF-8 as a value
    /// that can't be judged) and one a pack sets. On a worker.
    pub fn of_process() -> Shellcheck {
        const NAME: &str = "SHELLCHECK_OPTS";
        let homes = crate::allow::home().map_or_else(Vec::new, |home| {
            let config = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).filter(|p| p.is_absolute());
            vec![
                home.join(".shellcheckrc"),
                config.unwrap_or_else(|| home.join(".config")).join("shellcheckrc"),
            ]
        });
        let own = std::env::var_os(NAME).map(|v| v.into_string().unwrap_or_else(|_| "\u{FFFD}".into()));
        let packs = crate::sandboxpack::get().env.iter().filter(|(k, _)| k == NAME).map(|(_, v)| v.clone());
        Shellcheck { homes, opts: own.into_iter().chain(packs).collect() }
    }
}

/// `outside_with` where shellcheck has no rc file at home and no options from the environment — a test's (they
/// never read yours).
#[cfg(test)]
pub fn outside(
    cmd: &str,
    cwd: &Path,
    root: &Path,
    chat: &Allowed,
    kept: &Allowed,
    expanded: &[(String, PathBuf)],
) -> Result<Vec<PathBuf>, String> {
    outside_with(cmd, cwd, root, chat, kept, expanded, &Shellcheck::default())
}

/// `word` with the one of `expansions` in it filled in with its folder (`expanded`). Err: it names one whose
/// folder isn't known, or one the shell would split or fill in (a space, a glob), or names one twice, or
/// climbs out of it (`..` after it).
fn expand(word: &str, expanded: &[(String, PathBuf)]) -> Result<String, String> {
    let Some((at, e)) = expansion_in(word) else { return Ok(word.to_string()) };
    let unknown = || format!("where {e} leads couldn't be told");
    let folder = expanded.iter().find(|(w, _)| w == e).map(|(_, p)| p.to_string_lossy().into_owned());
    let root = folder.ok_or_else(unknown)?;
    let after = &word[at + e.len()..];
    let climbs = Path::new(after).components().any(|c| c == std::path::Component::ParentDir);
    let again = expansion_in(after).is_some();
    if root.contains(|c: char| c.is_whitespace() || "*?[]".contains(c)) || again || climbs {
        return Err(unknown());
    }
    Ok(word.replacen(e, &root, 1))
}

/// Whether a glob's `..` comes after its first wildcard (`src/*/../..`): through a link, `..` is the
/// parent of where the link leads — where it ends can't be told from the fixed folder.
fn climbs(glob: &str) -> bool {
    let after = &glob[glob.find(['*', '?', '[']).unwrap_or(glob.len())..];
    after.split('/').any(|s| s == "..")
}

/// The folder a `cd dir` from `here` goes into (real path), `expansions` filled in: a folder that's there —
/// a glob only when it names exactly one, as the shell fills it in (#164: `cd
/// ~/.cargo/registry/src/*/libc-0.2.189/src` asked). Err says why not: no such folder; a glob naming none
/// or several, or with `[` or `..` in it. On a worker.
pub fn cd_into(here: &Path, dir: &Word, expanded: &[(String, PathBuf)]) -> Result<PathBuf, String> {
    let text = expand(&dir.text, expanded)?;
    let path = match dir.glob {
        false => here.join(&text),
        true => {
            if text.contains(['[', ']']) || climbs(&text) {
                return Err(format!("cd {text}: a pattern whose folder can't be told"));
            }
            match &glob_paths(&here.join(&text))[..] {
                [one] => one.clone(),
                [] => return Err(format!("cd {text}: no folder matches")),
                many => return Err(format!("cd {text}: {} folders match", many.len())),
            }
        }
    };
    std::fs::canonicalize(&path)
        .ok()
        .filter(|d| d.is_dir())
        .ok_or_else(|| format!("cd {text}: no such folder"))
}

/// The paths a glob of `*` and `?` names, as sh fills it in: each segment with a wildcard matched against
/// what's in the folder so far (a name starting with `.` only by a segment that does), the rest joined as
/// written and there.
fn glob_paths(glob: &Path) -> Vec<PathBuf> {
    let mut at: Vec<PathBuf> = vec![PathBuf::new()];
    for c in glob.components() {
        let seg = c.as_os_str().to_string_lossy();
        if !seg.contains(['*', '?']) {
            at = at.into_iter().map(|p| p.join(c)).filter(|p| p.exists()).collect();
            continue;
        }
        let mut next = Vec::new();
        for p in &at {
            for e in std::fs::read_dir(p).into_iter().flatten().flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if (seg.starts_with('.') || !name.starts_with('.')) && wild_match(&seg, &name) {
                    next.push(p.join(name));
                }
            }
        }
        at = next;
    }
    at
}

/// Whether `name` matches `pat` — `*` any run of characters, `?` any one, the rest as itself.
fn wild_match(pat: &str, name: &str) -> bool {
    let (p, n): (Vec<char>, Vec<char>) = (pat.chars().collect(), name.chars().collect());
    // (the last `*` and where in `name` it took up to, to try one more character on a mismatch)
    let (mut i, mut j, mut star) = (0, 0, None);
    while j < n.len() {
        match p.get(i) {
            Some('*') => {
                star = Some((i, j));
                i += 1;
            }
            Some(&c) if c == '?' || c == n[j] => (i, j) = (i + 1, j + 1),
            _ => match star {
                Some((si, sj)) => (i, j, star) = (si + 1, sj + 1, Some((si, sj + 1))),
                None => return false,
            },
        }
    }
    p[i..].iter().all(|c| *c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No pattern may set a name that makes the command run another program or load code — by name, by
    /// prefix, or by suffix (cargo's per-target runners and linkers) — while plain ones stay settable.
    #[test]
    fn names_that_run_or_load_code_are_never_settable() {
        for n in [
            "RUSTC_WRAPPER",
            "RUSTC",
            "RUSTC_WORKSPACE_WRAPPER",
            "CARGO_BUILD_RUSTC_WRAPPER",
            "CARGO_BUILD_RUSTC",
            "CARGO_TARGET_AARCH64_APPLE_DARWIN_RUNNER",
            "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER",
            "CARGO_HOST_LINKER",
            "RUSTFLAGS",
            "CARGO_ENCODED_RUSTFLAGS",
            "GIT_SSH_COMMAND",
            "GIT_EXEC_PATH",
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_KEY_0",
            "GIT_PAGER",
            "GIT_EDITOR",
            "SSH_ASKPASS",
            "NODE_OPTIONS",
            "JDK_JAVA_OPTIONS",
            "_JAVA_OPTIONS",
            "PYTHONPATH",
            "PYTHONSTARTUP",
            "PERL5OPT",
            "RUBYOPT",
            "EDITOR",
            "VISUAL",
            "PAGER",
            "PATH",
            "LD_PRELOAD",
            "DYLD_INSERT_LIBRARIES",
        ] {
            assert!(!env_allowed(n), "{n}");
        }
        for n in
            ["TARAE_SHOT_XDG", "CARGO_TARGET_DIR", "RUST_LOG", "RUST_BACKTRACE", "CARGO_BUILD_JOBS", "FOO"]
        {
            assert!(env_allowed(n), "{n}");
        }
        // Not a name at all
        assert!(!env_allowed("1A") && !env_allowed("A-B") && !env_allowed(""));
        // What the built-in packs set or read for a run: a tool's home, a run's own home, its `env`, a
        // setting's file, a cache's variable, what a toolchain is asked — though `ENV_NEVER` doesn't name them
        // (a pack may set them itself)
        for n in ["GRADLE_USER_HOME", "GRADLE_OPTS", "GRADLE_RO_DEP_CACHE", "npm_config_logs_dir", "GOENV"] {
            assert!(!env_allowed(n) && env_core(n), "{n}");
        }
        // (and what a tool reads in any case, in every spelling: npm's settings files, its store — compat8)
        for n in ["npm_config_userconfig", "Npm_Config_GlobalConfig", "PNPM_CONFIG_STORE_DIR"] {
            assert!(!env_allowed(n) && env_core(n), "{n}");
        }
        // (every name the packs' caches and `ask` read, whatever pack: none of them a pattern's)
        let t = crate::sandboxpack::get();
        let asked = t.caches.iter().filter_map(|c| c.var.as_deref());
        let asked: Vec<&str> = asked.chain(t.ask.iter().flat_map(|a| a.keys().map(String::as_str))).collect();
        assert!(asked.contains(&"GOCACHE") && asked.contains(&"GOMODCACHE"), "{asked:?}");
        for n in &asked {
            assert!(!env_allowed(n) && env_core(n), "{n}");
        }
        let gomodcache = ["GOMODCACHE=… go build".to_string()];
        assert!(!crate::tools::allowed("GOMODCACHE=/x go build", &gomodcache), "the module cache moved");
        let rule = ["GRADLE_USER_HOME=… cargo build".to_string()];
        assert!(!crate::tools::allowed("GRADLE_USER_HOME=/x cargo build", &rule), "no pattern sets one");
    }

    /// f22 j: a script's syntax checked (`sh -n`, `bash -n`, `zsh -n`, `dash -n`; `shellcheck`) only reads it —
    /// never with another option (a `-c` body, `-i`'s startup files, `-s`'s input), nor anything after it.
    #[test]
    fn a_scripts_syntax_check_only_reads() {
        for c in
            ["sh -n x.sh", "bash -n target/sb/a.sh", "zsh -n x.zsh", "dash -n -n x.sh", "shellcheck x.sh"]
        {
            assert!(read_only(c), "{c}");
        }
        assert!(read_only("shellcheck -f gcc -s bash -e SC1091 --norc x.sh && sh -n x.sh"));
        for c in [
            "sh x.sh",
            "sh -n",
            "sh -n -c 'rm -rf x'",
            "bash -n -i x.sh",
            "bash -nx x.sh",
            "bash -n --rcfile r x.sh",
            "sh -n x.sh y",
            "sh -n -s",
            "sh -n x.sh; sh x.sh",
            // (what lets shellcheck follow a source its line doesn't name)
            "shellcheck -x x.sh",
            "shellcheck -ax x.sh",
            "shellcheck --external-sources x.sh",
            "shellcheck --ext x.sh",
            "shellcheck -P /etc x.sh",
            "shellcheck -PSCRIPTDIR x.sh",
            "shellcheck --source-path=/etc x.sh",
            "shellcheck --rcfile r x.sh",
            "SHELLCHECK_OPTS=-x shellcheck x.sh",
        ] {
            assert!(!read_only(c), "{c}");
        }
    }

    /// f22 review: shellcheck reads only what its line names — an rc file where it looks (the script's folder or
    /// one above it, home's) or the script's own directive that turns external sources on, or an rc file it
    /// can't read, and where it reads can't be told: the card. `--norc` leaves the rc files out; a comment or
    /// another key says nothing.
    #[test]
    fn shellcheck_follows_a_source_only_where_external_sources_are_on() {
        let dir = crate::scratch::Scratch::new("shellcheck-rc");
        let p = dir.join("p");
        std::fs::create_dir_all(p.join("sub")).unwrap();
        std::fs::write(p.join("sub/x.sh"), "# shellcheck source=/etc/hosts\n. ./lib.sh\n").unwrap();
        let none = Allowed::default();
        let at = |cmd: &str| outside(cmd, &p, &p, &none, &none, &[]);
        assert_eq!(at("shellcheck sub/x.sh"), Ok(Vec::new()), "its directive names a file: not followed");
        std::fs::write(p.join(".shellcheckrc"), "# external-sources=true\nsource-path=SCRIPTDIR\n").unwrap();
        assert_eq!(at("shellcheck sub/x.sh"), Ok(Vec::new()), "a comment, another key");
        // (in the folder above the script's)
        std::fs::write(p.join(".shellcheckrc"), "disable=SC1091\nexternal-sources = true # mine\n").unwrap();
        let e = at("cd sub && shellcheck -f gcc x.sh").unwrap_err();
        assert!(e.contains(".shellcheckrc turns external sources on"), "{e}");
        assert_eq!(at("shellcheck --norc sub/x.sh"), Ok(Vec::new()), "no rc file");
        std::fs::remove_file(p.join(".shellcheckrc")).unwrap();
        // (one it can't read: a folder by that name)
        std::fs::create_dir(p.join("sub/shellcheckrc")).unwrap();
        assert!(at("shellcheck sub/x.sh").unwrap_err().contains("can't be read"));
        std::fs::remove_dir(p.join("sub/shellcheckrc")).unwrap();
        // (the script's own directive, --norc or not)
        std::fs::write(p.join("sub/x.sh"), "#!/bin/sh\n# shellcheck disable=SC1 external-sources=true\n")
            .unwrap();
        let e = at("shellcheck --norc sub/x.sh").unwrap_err();
        assert!(e.contains("x.sh turns external sources on itself"), "{e}");
        std::fs::write(p.join("sub/x.sh"), "echo external-sources=true\n").unwrap();
        assert_eq!(at("shellcheck sub/x.sh"), Ok(Vec::new()), "not a directive");
        // Home's: `~/.shellcheckrc`, the config folder's
        let home = dir.join("home");
        std::fs::create_dir_all(home.join(".config")).unwrap();
        let homes = [home.join(".shellcheckrc"), home.join(".config/shellcheckrc")];
        let words = parse("shellcheck sub/x.sh").unwrap();
        let [Part::Cmd(w)] = &words[..] else { panic!() };
        assert_eq!(shellcheck_follows(w, &p, &homes, &[], &[]), None);
        // The run's environment: `SHELLCHECK_OPTS`'s words, as its line's
        let env = |v: &str| shellcheck_follows(w, &p, &homes, &[v.to_string()], &[]);
        for fine in ["", "-f gcc --severity=warning", "-s bash\t-e SC1091 --norc"] {
            assert_eq!(env(fine), None, "{fine:?}");
        }
        for follows in
            ["-x", "-a -x", "--external-sources", "-P /etc", "--source-path=SCRIPTDIR", "--rcfile r"]
        {
            assert!(env(follows).is_some_and(|e| e.contains("lets it read")), "{follows}");
        }
        for odd in ["'-x'", "-f $F", "-e\nSC1", "\u{FFFD}"] {
            assert!(env(odd).is_some_and(|e| e.contains("isn't plain words")), "{odd:?}");
        }
        std::fs::write(&homes[1], "external-sources=true\n").unwrap();
        assert!(
            shellcheck_follows(w, &p, &homes, &[], &[]).is_some_and(|e| e.contains(".config/shellcheckrc"))
        );
        // (through `outside_with`, as the run path gives them — `Shellcheck::of_process`)
        let with = |sc: Shellcheck| outside_with("shellcheck sub/x.sh", &p, &p, &none, &none, &[], &sc);
        let e = with(Shellcheck { homes: homes.to_vec(), opts: Vec::new() }).unwrap_err();
        assert!(e.contains(".config/shellcheckrc"), "{e}");
        let e = with(Shellcheck { homes: Vec::new(), opts: vec!["--ext".into()] }).unwrap_err();
        assert!(e.contains("SHELLCHECK_OPTS"), "{e}");
        assert_eq!(with(Shellcheck::default()), Ok(Vec::new()));
    }

    /// The list, and each option that makes one of them write, run something, or never end.
    #[test]
    fn which_commands_only_read() {
        for c in [
            "ls",
            "ls -la src",
            "cat Cargo.toml",
            "head -n 20 src/main.rs",
            "tail -n 5 log.txt",
            "wc -l src/a.rs src/b.rs",
            "grep -rn fn src",
            "rg --line-number fn src",
            "rg --glob=src/x fn",
            "find src -name x.rs -type f",
            "tree -L 2",
            "stat Cargo.toml",
            "file target/x",
            "du -sh target",
            "diff a.txt b.txt",
            "sort -r names.txt",
            "sort --check --numeric-sort names.txt",
            "tail --lines=5 log.txt",
            "uniq names.txt",
            "pwd",
            "git blame src/main.rs",
            "git rev-parse HEAD",
            "git ls-files src",
            "git branch",
            // #57: `-C <dir>` as without it (its folder is a read — `where_a_command_reads`)
            "git -C /x blame a",
            "git -C sub -C deeper ls-files",
            // #88: git's harmless options in front, -C among them
            "git --no-pager blame a",
            "git -P -C sub ls-files",
            "git --no-optional-locks rev-parse HEAD",
            // #66: sed printing lines by number
            "sed -n '3,40p' src/a.rs",
            "sed -n 10p",
            "sed -n -E '$p'",
            "sed -n '1,5p;9p'",
            // #92: by pattern too
            "sed -n '/x/p' a",
            "sed -n '/begin/,/end/p' target/x/y.events",
            "sed -n '0,/end/p;$p' a",
        ] {
            assert!(read_only(c), "{c}");
        }
        for c in [
            "find . -exec rm {} ;",
            "find . -execdir touch x",
            "find . -ok rm",
            "find . -okdir rm",
            "find . -delete",
            "find . -fprint out",
            "find . -fprintf out x",
            "find . -fls out",
            "sort -o out names.txt",
            "sort -ro out names.txt",
            "sort --output=out x",
            "sort --compress-program=sh x",
            // (getopt_long takes any start of a long option's name)
            "sort --compress-prog=./x x",
            "sort --co ./x x",
            "sort --out=f x",
            "sort --o f x",
            "tree -o out",
            "tree -R -H . -L 1",
            "tree -dR -H . -L 1",
            "file -C -m magic",
            "file --comp -m magic",
            "tail -f log.txt",
            "tail -F log.txt",
            "tail --follow log.txt",
            "tail --fol log.txt",
            "tail --f=name log.txt",
            "rg --pre sh fn",
            "uniq in.txt out.txt",
            // (#193: `-` is stdin, `out` is still written)
            "uniq - out.txt",
            "uniq -c - out.txt",
            "git branch new",
            "git branch -D main",
            "git -C /x push",
            "git -C /x -c core.pager=sh log",
            "git --no-pager -c core.pager=sh blame a",
            "git --git-dir=/x blame a",
            "git -p blame a",
            "git -C",
            "git push",
            "sed 1,5p x",
            "sed -n '1,5w out' x",
            "sed -n 1e x",
            "sed -n s/a/b/p x",
            "sed -n -i 1p x",
            "sed -n -e 1p x",
            "sed -n '/x/w out' a",
            "sed -n '/x;w out/p' a",
            "sed -n '/a,b/p' a",
            "sed -n '/x/Ip' a",
            "sed -n '/x/,+3p' a",
            "sed -n",
            "cat a | sh",
            "cat ~root/.ssh/id_ed25519",
            "cat $HOME/x",
            "ls; rm -rf x",
            "rm -rf x",
            "cp a b",
            "sed -i s/a/b/ x",
            "",
        ] {
            assert!(!read_only(c), "{c}");
        }
    }

    /// #76: a line that only moves, copies, removes or reads files runs none of them; anything else in it
    /// might run what was moved.
    #[test]
    fn lines_that_only_move_files() {
        for c in [
            "mv src/a.rs src/b.rs",
            "git mv src/a.rs src/b.rs",
            "git -C sub rm -r old",
            "cp x.sh y.sh && rm x.sh",
            "cd src && mkdir m && mv a.rs m/ && ls m",
            "git add src/b.rs; git ls-files src",
            "git --no-pager mv a b",
        ] {
            assert!(only_moves(c), "{c}");
        }
        for c in [
            "sh x.sh",
            "mv a b && sh b",
            "cp x.sh y.sh; ./y.sh",
            "git mv a b && cargo test",
            "source x.sh",
            "python3 tools/gen.py",
            "git -c alias.mv=x mv a b",
            "mv $(cat x) y",
            "",
        ] {
            assert!(!only_moves(c), "{c}");
        }
    }

    /// Lines of several reads (#27): `cd`, `;` `&&` `||` `|` between read-only commands, or the same in
    /// `sh -c '…'` — and every line that must still ask: a redirect to a file, an `sh -c` inside the body,
    /// anything the shell fills in or runs. A `|` inside quotes is part of a word (#22: `grep -E 'a|b'`).
    #[test]
    fn lines_of_several_reads() {
        for c in [
            "cd src && grep -n fn a.rs | sort | head -5",
            "sh -c 'wc -l src/*.rs | sort -n | tail -8'",
            "bash -c \"ls -la; pwd\"",
            "grep -E 'a|b' src/x.rs",
            "grep -E \"a|b\" src/x.rs",
            "grep -rn 'fn main' src 2>/dev/null | head",
            "rg -n x src 2>&1 | wc -l",
            "ls || pwd",
            "find . -name '*.rs' | wc -l",
            "cat 'a file with spaces.txt'",
            "git ls-files src | grep -c rs",
            // #80: quotes keep `|` `<` `>` `*` `{}` as they are, and a `\` in them as sh reads it
            r#"rg -n "thread::sleep\(|elapsed\(\) *[<>]|assert!\(.*elapsed|< *SOON|recv_timeout\(Duration::from_millis\([0-9]{1,3}\)\)" src tests --glob "*.rs""#,
            "grep \"a\\b\" x",
            // #92: the line as it was carded, and its likely kin — a regex's `$` in double quotes, sed by
            // pattern, a file under the git-ignored target/
            r#"sh -c 'grep -v "^.*note" target/ra-out/grow/grow.events | sed -n "1,200p" | grep -E "begin|end|status|stopped"'"#,
            r#"sh -c 'grep -v "^.*note$" target/x/y.events | sed -n "/begin/,/end/p" | grep -E "begin$|end$"'"#,
            r#"grep -nE "fn [a-z_]+\(\)$" src/a.rs"#,
            r#"rg "(a|b)$" src"#,
        ] {
            assert!(read_only(c), "{c}");
        }
        let w = |t: &str| Word { text: t.into(), glob: false };
        assert_eq!(
            parse(r#"rg "sleep\(|a *[<>]" --glob "*.rs""#),
            Some(vec![Part::Cmd(vec![w("rg"), w(r"sleep\(|a *[<>]"), w("--glob"), w("*.rs")])]),
            "one pattern, one glob — nothing the shell would split or fill in"
        );
        assert_eq!(
            parse(r#"grep "a\"b\\c\$d" x"#),
            Some(vec![Part::Cmd(vec![w("grep"), w(r#"a"b\c$d"#), w("x")])]),
            "\\ before \" \\ $ ` is the character itself"
        );
        for c in [
            // Redirects to a file
            "ls > out.txt",
            "cat a >> b",
            "grep x a 2> err.txt",
            "sh -c 'ls > x'",
            "cat a 1>b",
            // An sh -c inside the body, or a shell anywhere in the line
            "sh -c 'sh -c \"ls\"'",
            "sh -c 'bash -c ls'",
            "ls | sh",
            "sh -c 'ls' extra",
            // What the shell fills in or runs
            "cat $(echo x)",
            "cat \"$HOME\"",
            "grep \"a$b\" x",
            "grep \"$1\" x",
            "grep \"$(id)\" x",
            "grep \"${x}\" x",
            "grep \"$$\" x",
            "grep note$ x",
            "ls `pwd`",
            "ls \\; rm x",
            "(ls)",
            "{ ls; }",
            "cat <<EOF",
            "cat < x",
            "ls & rm x",
            "ls &",
            "ls\npwd",
            "FOO=1 ls",
            "cat ~root/x",
            "ls ! x",
            // Not read-only, or not a line at all
            "cd /tmp && rm x",
            "cd",
            "cd -",
            "cd a b",
            "xargs cat",
            "ls; ",
            "; ls",
            "ls && && ls",
            "grep 'unterminated",
            "grep \"a\\",
        ] {
            assert!(!read_only(c), "{c}");
        }
        let w = |t: &str| Word { text: t.into(), glob: false };
        assert_eq!(
            parse("cd src && grep -n 'a b' x.rs | head"),
            Some(vec![
                Part::Cd(w("src")),
                Part::Cmd(vec![w("grep"), w("-n"), w("a b"), w("x.rs")]),
                Part::Cmd(vec![w("head")]),
            ])
        );
        assert_eq!(
            parse("ls src/*.rs"),
            Some(vec![Part::Cmd(vec![w("ls"), Word { text: "src/*.rs".into(), glob: true }])])
        );
        assert_eq!(
            parse("ls '*.rs'"),
            Some(vec![Part::Cmd(vec![w("ls"), w("*.rs")])]),
            "a quoted glob is a word"
        );
        assert_eq!(
            one_command("sh -c 'cargo test -- \"a b\"'"),
            Some(vec!["cargo".into(), "test".into(), "--".into(), "a b".into()])
        );
        assert_eq!(one_command("ls | head"), None);
    }

    /// Review (#93's pins): only a text script in the checkout is pinned as it is — `#!` at its start or a
    /// script's ending, no NUL (a binary), its real path inside the checkout (a link out isn't); a program by
    /// its path (a runner anywhere, an absolute path, one through `..`) is no script at all. (Fixtures are
    /// files only — nothing here runs them.)
    #[test]
    fn only_a_text_script_in_the_checkout_is_pinned() {
        let scratch = crate::scratch::Scratch::new("pins");
        let dir = scratch.to_path_buf();
        std::fs::create_dir_all(dir.join("p/sub")).unwrap();
        let p = dir.join("p");
        std::fs::write(p.join("run"), "#!/bin/sh\necho marker\n").unwrap();
        std::fs::write(p.join("x.sh"), "echo marker\n").unwrap();
        std::fs::write(p.join("sub/y.zsh"), "echo marker\n").unwrap();
        std::fs::write(p.join("plain"), "echo marker\n").unwrap();
        std::fs::write(p.join("tool"), b"\xcf\xfa\xed\xfe\x00\x00binary").unwrap();
        std::fs::write(p.join("tool.sh"), b"#!/bin/sh\n\x00").unwrap();
        std::fs::write(dir.join("out.sh"), "echo marker\n").unwrap();
        std::os::unix::fs::symlink(dir.join("out.sh"), p.join("link.sh")).unwrap();
        let pins =
            |cmd: &str| script_pins(cmd, &p, &p).map(|v| v.into_iter().map(|(f, _)| f).collect::<Vec<_>>());
        assert_eq!(pins("./run fast"), Some(vec![p.join("run")]), "#! at its start");
        assert_eq!(pins("sh x.sh"), Some(vec![p.join("x.sh")]), "a script's ending");
        assert_eq!(pins("cd sub && ./y.zsh"), Some(vec![p.join("sub/y.zsh")]), "where it runs");
        assert_eq!(pins("ls && cargo test"), Some(vec![]), "no script, nothing to pin");
        for c in ["./plain", "./tool", "./tool.sh", "./link.sh", "sh link.sh"] {
            assert_eq!(pins(c), None, "{c}");
        }
        // A program by its path: not a script — not even looked at
        for c in [
            "/bin/sh -c 'echo marker'",
            "./node_modules/.bin/env x",
            "../out.sh",
            "sh ../out.sh",
            "/usr/bin/true",
        ] {
            let w = match &parse(c).unwrap()[..] {
                [Part::Cmd(w)] => w.clone(),
                other => panic!("{other:?}"),
            };
            assert_eq!(script_part(&w), None, "{c}");
        }
        // Same text, another hash once it changes
        let before = script_pins("sh x.sh", &p, &p).unwrap();
        std::fs::write(p.join("x.sh"), "echo another\n").unwrap();
        assert_ne!(script_pins("sh x.sh", &p, &p).unwrap(), before);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #210: what a command runs that its words don't name (a pack's `entry`: `cargo xtask` runs `xtask/`) is
    /// pinned as it is — any file in the folder changed, added or made executable, a link pointed elsewhere,
    /// is another pin; the same content the same pin. One not there is pinned as not there (made later: another
    /// pin); one reaching out of the checkout, or past the bytes allowed, can't be pinned. (Files only — nothing
    /// here runs them.)
    #[test]
    fn what_a_packs_entry_runs_is_pinned_as_it_is() {
        let scratch = crate::scratch::Scratch::new("entry-pins");
        let dir = scratch.to_path_buf();
        std::fs::create_dir_all(dir.join("p/xtask/src")).unwrap();
        let p = dir.join("p");
        std::fs::write(p.join("xtask/Cargo.toml"), "[package]\n").unwrap();
        std::fs::write(p.join("xtask/src/main.rs"), "fn main() {}\n").unwrap();
        let entries = [crate::sandboxpack::Entry {
            run: vec!["cargo".into(), "xtask".into()],
            files: vec!["xtask/".into(), "gen.toml".into()],
        }];
        let pins = |cmd: &str| entry_pins_in(cmd, &p, &entries, 1 << 20);
        let first = pins("cargo xtask linux test -- a").unwrap();
        let paths: Vec<&Path> = first.iter().map(|(f, _)| f.as_path()).collect();
        assert_eq!(paths, [p.join("xtask").as_path(), p.join("gen.toml").as_path()]);
        assert_eq!(pins("cargo test"), Some(vec![]), "no entry, nothing to pin");
        assert_eq!(pins("cargo xtask linux test -- b"), Some(first.clone()), "the same files: the same pin");
        let changed = |what: &str, first: &[(PathBuf, u64)]| {
            let now = pins("cargo xtask linux test").unwrap();
            assert_ne!(now[0].1, first[0].1, "{what}");
            now
        };
        std::fs::write(p.join("xtask/src/main.rs"), "fn main() { other() }\n").unwrap();
        let now = changed("a file changed", &first);
        std::fs::write(p.join("xtask/src/new.rs"), "").unwrap();
        let now = changed("a file added", &now);
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p.join("xtask/src/new.rs"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let now = changed("made executable", &now);
        std::os::unix::fs::symlink("main.rs", p.join("xtask/src/l.rs")).unwrap();
        let now = changed("a link", &now);
        std::fs::remove_file(p.join("xtask/src/l.rs")).unwrap();
        std::os::unix::fs::symlink("new.rs", p.join("xtask/src/l.rs")).unwrap();
        let now = changed("the link pointed elsewhere", &now);
        std::fs::write(p.join("gen.toml"), "").unwrap();
        assert_ne!(pins("cargo xtask").unwrap()[1].1, now[1].1, "made since: another pin");
        assert_eq!(entry_pins_in("cargo xtask", &p, &entries, 8), None, "past the bytes allowed");
        std::fs::remove_dir_all(p.join("xtask")).unwrap();
        std::fs::create_dir_all(dir.join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(dir.join("elsewhere"), p.join("xtask")).unwrap();
        assert_eq!(pins("cargo xtask"), None, "out of the checkout");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #113: a loop of reads is a read — each `for` value judged as a word of its own (a glob by its fixed
    /// folder), a `while read` fed by a list judged where its body runs; a loop whose body writes or runs
    /// something, a variable no loop sets, or a use that can't be told still asks. #116, #110, and an
    /// `echo` in a line (A9) likewise.
    #[test]
    fn loops_of_reads_and_the_reads_seen_today() {
        for c in [
            "for f in a.rs b.rs; do wc -l $f; done",
            "for f in src/*.rs; do echo \"== $f\"; head -20 \"$f\"; done",
            "for f in a b; do grep -n x \"src/${f}.rs\" | head -3; done | sort",
            "sh -c 'for d in src tests; do ls \"$d\"; done'",
            "find src -name '*.rs' | while read -r f; do head -3 \"$f\"; done",
            "git ls-files src | sort | while IFS= read -r f; do wc -l \"$f\"; done",
            "rg -l fn src | while read f; do grep -c fn \"$f\"; done",
            // #116, #110, A9
            "nm -gU target/debug/tarae | head",
            "otool -L target/debug/tarae",
            "strings target/debug/tarae | grep -c x",
            "size -m target/debug/tarae",
            "du -sh target",
            "git show :2:src/a.rs",
            "git show HEAD~1:src/a.rs | sed -n '1,40p'",
            "git diff --no-index --stat a.txt b.txt",
            "wc -l src/a.rs | tail -3 && echo clean",
            "echo hi",
            "ls src || true",
        ] {
            assert!(read_only(c), "{c}");
        }
        for c in [
            "for f in a b; do rm $f; done",
            "for f in a b; do cat $f > out; done",
            "for f in a b; do cat $g; done",
            "for f in $(ls); do cat $f; done",
            "for f in; do rm -rf x; done",
            "for f in a; do for g in b; do cat $g; done; done",
            "for f in 'a b'; do cat $f; done",
            "for f in a b; do sh -c \"cat $f\"; done",
            "for f in a b | cat; do cat $f; done",
            "cat list.txt | while read f; do cat \"$f\"; done",
            "ls -la | while read f; do cat \"$f\"; done",
            "find . -L | while read f; do cat \"$f\"; done",
            "find . | while read f; do cat $f; done",
            "find . | while read f; do cat \"../$f\"; done",
            "find . | while read f; do cat \"$f/../../x\"; done",
            "find . | cut -c3- | while read f; do cat \"$f\"; done",
            "find . | grep -o x | while read f; do cat \"$f\"; done",
            "while read f; do cat \"$f\"; done",
            "find . | while read f; do cat \"$f\"; done; cat $f",
            "cat $HOME/x",
            "git diff HEAD",
            "otool -l x; rm y",
            "echo $(id)",
        ] {
            assert!(!read_only(c), "{c}");
        }
        // The `for` values each a word of its own, a glob staying a glob
        let w = |t: &str, glob: bool| Word { text: t.into(), glob };
        assert_eq!(
            flat("for f in a.rs src/*.rs; do wc -l \"$f\"; done | sort"),
            Some(vec![
                Part::Cmd(vec![w("wc", false), w("-l", false), w("a.rs", false)]),
                Part::Cmd(vec![w("wc", false), w("-l", false), w("src/*.rs", true)]),
                Part::Cmd(vec![w("sort", false)]),
            ])
        );
        assert_eq!(
            flat("cd src && ls | while read -r f; do head -1 \"$f\"; done"),
            Some(vec![
                Part::Cd(w("src", false)),
                Part::Cmd(vec![w("ls", false)]),
                Part::Cmd(vec![w("head", false), w("-1", false), w("./*", true)]),
            ]),
            "a listed line: anything where the body runs"
        );
    }

    /// #112: `$(rustc --print sysroot)` is the sysroot rustup names for where it runs — judged there, as any
    /// path: inside the toolchain's sources a read like any other; unknown, or climbing out of it, asked.
    #[test]
    fn the_sysroot_is_filled_in_where_it_is_judged() {
        let scratch = crate::scratch::Scratch::new("sysroot");
        let base = scratch.to_path_buf();
        let tc = base.join("rustup/toolchains/1.99.0-aarch64-apple-darwin");
        std::fs::create_dir_all(tc.join("lib/rustlib/src/rust/library/core/src")).unwrap();
        std::fs::create_dir_all(base.join("p")).unwrap();
        std::fs::write(tc.join("lib/rustlib/src/rust/library/core/src/lib.rs"), "x\n").unwrap();
        let base = std::fs::canonicalize(&base).unwrap();
        let (tc, p) = (base.join("rustup/toolchains/1.99.0-aarch64-apple-darwin"), base.join("p"));
        let trees = Allowed { read: vec![tc.join("lib/rustlib/src")], ..Default::default() };
        let none = Allowed::default();
        // (the rust pack's one `$(…)`, its folder known)
        assert_eq!(expansions().collect::<Vec<_>>(), ["$(rustc --print sysroot)"]);
        let known = [("$(rustc --print sysroot)".to_string(), tc.clone())];
        let core = "$(rustc --print sysroot)/lib/rustlib/src/rust/library/core/src/lib.rs";
        for c in [
            format!("sed -n '1,40p' {core}"),
            format!("grep -n fn \"{core}\""),
            "cd \"$(rustc --print sysroot)/lib/rustlib/src/rust/library\" && grep -rn fn core/src | head"
                .into(),
        ] {
            assert!(read_only(&c), "{c}");
            assert_eq!(outside(&c, &p, &p, &none, &trees, &known), Ok(vec![]), "{c}");
            assert!(!runs_outside(&c, &p, &p, &known), "{c}");
        }
        let c = format!("cat {core}");
        assert!(outside(&c, &p, &p, &none, &trees, &[]).is_err(), "not known: asked");
        assert_eq!(outside(&c, &p, &p, &none, &none, &known), Ok(vec![tc.join(&core[25..])]), "not allowed");
        let climbs = "cat $(rustc --print sysroot)/../../x";
        assert!(outside(climbs, &p, &p, &none, &trees, &known).is_err(), "out of it");
        let opt = "grep -f$(rustc --print sysroot)/../../x src";
        assert!(outside(opt, &p, &p, &none, &trees, &known).is_err(), "an option's value");
        assert_eq!(parse("cat $(rustc --print sysroot )"), None, "only as written");
        assert_eq!(parse("cat $(rustc --print target-libdir)"), None);
        std::fs::remove_dir_all(&base).ok();
    }

    /// Paths it names are resolved where it runs: inside the project they're fine, outside they're what the
    /// card says — unless allowed; a word that names nothing isn't a path, and a link that leads out is out.
    #[test]
    fn where_a_command_reads() {
        let dir = std::env::temp_dir().join(format!("tarae-readonly-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("p/src")).unwrap();
        std::fs::create_dir_all(dir.join("out")).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let (p, out) = (dir.join("p"), dir.join("out"));
        std::fs::write(p.join("src/a.rs"), "x\n").unwrap();
        std::fs::write(out.join("m.txt"), "x\n").unwrap();
        std::os::unix::fs::symlink(&out, p.join("link")).unwrap();
        let none = Allowed::default();
        let at = |cmd: &str| outside(cmd, &p, &p, &none, &none, &[]).unwrap();
        assert!(at("cat src/a.rs").is_empty());
        assert!(at("grep -rn nothing-like-a-path src").is_empty(), "a pattern isn't a path");
        assert!(at("ls").is_empty());
        assert_eq!(at("cat ../out/m.txt"), [out.join("m.txt")]);
        assert_eq!(at(&format!("ls {}", out.display())), std::slice::from_ref(&out));
        assert_eq!(at("cat link/m.txt"), [out.join("m.txt")], "a link that leads out");
        assert_eq!(at(&format!("grep --file={}/m.txt src", out.display())), [out.join("m.txt")]);
        assert_eq!(at(&format!("grep -f{}/m.txt src", out.display())), [out.join("m.txt")]);
        assert_eq!(at("git ls-files ../out"), std::slice::from_ref(&out));
        // `git -C <dir>`: the folder is a read, the subcommand isn't a path (#57)
        assert_eq!(at("git -C ../out log"), std::slice::from_ref(&out));
        assert!(at("git -C src ls-files a.rs").is_empty());
        assert_eq!(at("git -C src ls-files ../../out"), std::slice::from_ref(&out), "from where -C goes");
        // #88: a -C behind git's harmless options is a read all the same, and where it runs (`runs_outside`)
        assert_eq!(at("git --no-pager -C ../out log"), std::slice::from_ref(&out));
        assert_eq!(at("git -C src --no-optional-locks ls-files ../../out"), std::slice::from_ref(&out));
        assert!(runs_outside("git --no-optional-locks -C ../out status", &p, &p, &[]));
        assert!(!runs_outside("git --no-optional-locks -C src status", &p, &p, &[]));
        // A line of several: each part where it runs — a cd's folder read too; a glob by its fixed folder
        assert!(at("cd src && cat a.rs | wc -l").is_empty());
        assert_eq!(at("cd ../out && cat m.txt"), [out.clone(), out.join("m.txt")]);
        assert_eq!(at("ls ../out/*.txt"), std::slice::from_ref(&out), "the glob's folder");
        assert!(at("ls src/*.rs").is_empty());
        assert_eq!(at("sh -c 'grep -n x src/a.rs; cat ../out/m.txt'"), [out.join("m.txt")]);
        // A cd into what isn't a folder that's there: the shell stays put and runs the rest from where it
        // was — where that reads can't be told, so the line is asked about (not skipped as naming nothing)
        for line in [
            "cd nope; cat ../../out/m.txt",
            "cd nope || cat ../out/m.txt",
            "cd src/a.rs; cat ../out/m.txt",
            "cd nope && cat x",
            "sh -c 'cd nope; cat ../out/m.txt'",
        ] {
            let r = outside(line, &p, &p, &none, &none, &[]);
            assert!(r.as_ref().is_err_and(|e| e.contains("no such folder")), "{line}: {r:?}");
        }
        // One there is followed to its real path: a link into somewhere else is read there
        assert_eq!(at("cd link; cat m.txt"), [out.clone(), out.join("m.txt")]);
        let allowed = Allowed { read: vec![out.clone()], ..Default::default() };
        assert!(
            outside("cat ../out/m.txt", &p, &p, &allowed, &none, &[]).unwrap().is_empty(),
            "allowed this chat"
        );
        assert!(outside("cat ../out/m.txt", &p, &p, &none, &allowed, &[]).unwrap().is_empty(), "kept");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #202, #207: lines read with every day that went to a card. A glob that's an option's value is filled in
    /// where the run starts, so it reads that folder; `tr` reads only what's piped in (its words are sets of
    /// characters, never files); an awk that only prints reads only the files it names. A glob or a file
    /// outside is still outside, and an awk that could run, write, or read past its words isn't a read.
    #[test]
    fn a_glob_value_tr_and_a_printing_awk_read_only_what_they_name() {
        for line in [
            "grep -rn 'x' src --include=*.rs | head",
            "grep -rln 'Command::new' src --include=*.rs | grep -v test | head -60 | tr '\\n' ' '",
            "awk 'NR>=702 && NR<=800' src/offer.rs | grep -n x",
            "awk -F: '$2 > 3 { print $1 }' a.txt b.txt",
            "awk '/a|b/ { n++ } END { print n }' a.txt",
            "cat a.txt | awk -F: -v n=2 '{ print $n }'",
        ] {
            assert!(read_only(line), "{line}");
        }
        for line in [
            "awk '{ print > \"f\" }' a.txt",
            "awk '{ print $1 >> \"f\" }' a.txt",
            "awk 'NR > 1 { print | \"sh\" }' a.txt",
            "awk '{ printf \"%s\", $0 |& \"sh\" }' a.txt",
            "awk '{ xprint = 1; print > \"f\" }' a.txt",
            "awk 'BEGIN { system(\"true\") }'",
            "awk '{ \"date\" | getline d }' a.txt",
            "awk '{ while ((getline l < \"/etc/hosts\") > 0) print l }'",
            "awk 'BEGIN { ARGV[1] = \"/etc/hosts\"; ARGC = 2 } { print }'",
            "awk 'BEGIN { SYMTAB[\"AR\" \"GC\"] = 2 }'",
            "awk 'BEGIN { print ENVIRON[\"HOME\"] }'",
            "awk '@load \"x\"' a.txt",
            "awk -f x.awk a.txt",
            "awk -i inplace '{ print }' a.txt",
            "awk '{ print }' FS=: a.txt",
            "awk",
        ] {
            assert!(!read_only(line), "{line}");
        }
        let dir = std::env::temp_dir().join(format!("tarae-readonly-glob-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("p/src")).unwrap();
        std::fs::create_dir_all(dir.join("out")).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let (p, out) = (dir.join("p"), dir.join("out"));
        std::fs::write(out.join("m.txt"), "x\n").unwrap();
        let none = Allowed::default();
        let at = |cmd: &str| outside(cmd, &p, &p, &none, &none, &[]).unwrap();
        assert!(at("grep -rn x src --include=*.rs | head").is_empty(), "the folder it starts in");
        assert!(at("grep -rn x . --include=*.txt").is_empty());
        assert_eq!(at("grep -rn x ../out --include=*.txt"), std::slice::from_ref(&out));
        assert_eq!(at("grep x ../out/*.txt"), std::slice::from_ref(&out), "a glob climbing out");
        assert_eq!(at("awk '{ print }' ../out/m.txt"), [out.join("m.txt")], "awk's files are reads");
        assert!(at("ls | tr ../out/m.txt x").is_empty(), "tr's words name no file");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #164: three lines that went to a card saying nothing of why. `~/` is home as the shell fills it in,
    /// and a `cd` into a glob is the one folder it names — so a read in cargo's downloaded crates (read
    /// unasked) runs unasked; a glob naming several folders, or climbing out after its wildcard, still
    /// asks, saying why. `$(…)` isn't read, and `unreadable` says so.
    #[test]
    fn home_and_a_cd_into_a_glob_are_read_as_the_shell_reads_them() {
        let Some(home) = crate::allow::home().and_then(|h| h.to_str().map(String::from)) else { return };
        let one = "cd ~/.cargo/registry/src/*/libc-0.2.189/src && grep -rn \"fn sysctl\" .";
        let list = "ls ~/.cargo/registry/src/*/ | grep '^libc-'";
        let var = "sh -c 'd=$(ls -d ~/.cargo/registry/src/*/libc-0.2.189 | head -1); grep -rn sysctl $d/src'";
        let w = |t: &str, glob: bool| Word { text: t.into(), glob };
        assert_eq!(
            parse(one),
            Some(vec![
                Part::Cd(w(&format!("{home}/.cargo/registry/src/*/libc-0.2.189/src"), true)),
                Part::Cmd(vec![w("grep", false), w("-rn", false), w("fn sysctl", false), w(".", false)]),
            ])
        );
        assert!(read_only(one) && read_only(list), "both only read");
        assert_eq!(
            parse("cat ~ '~/x' a~b"),
            Some(vec![Part::Cmd(vec![w("cat", false), w(&home, false), w("~/x", false), w("a~b", false)])]),
            "only an unquoted `~` at a word's start"
        );
        assert_eq!(unreadable(one), None);
        assert_eq!(unreadable(var), Some("`$(…)` makes its words as it runs"));
        assert_eq!(unreadable("cat ~root/x"), Some("`~name` — someone else's home"));
        assert_eq!(unreadable("ls > x"), Some("a `>` writes to a file"));
        assert_eq!(unreadable("grep $x a"), Some("a `$` the shell fills in as it runs"));
        assert_eq!(unreadable("sh -c 'bash -c ls'"), Some("an `sh -c` inside an `sh -c`"));
        // Where they read, with a folder in the temp dir standing in for home: cargo's crates are allowed
        let scratch = crate::scratch::Scratch::new("cd-glob");
        let base = scratch.to_path_buf();
        std::fs::create_dir_all(base.join("reg/src/index-a/libc-0.2.189/src")).unwrap();
        std::fs::create_dir_all(base.join("reg/src/.hidden/libc-0.2.189/src")).unwrap();
        std::fs::create_dir_all(base.join("p")).unwrap();
        let base = std::fs::canonicalize(&base).unwrap();
        let (reg, p) = (base.join("reg/src"), base.join("p"));
        let crates = Allowed { read: vec![reg.clone()], ..Default::default() };
        let none = Allowed::default();
        // (home read as a read: outside the project, what the card names)
        let real_home = std::fs::canonicalize(&home).unwrap();
        assert_eq!(outside("ls ~", &p, &p, &none, &none, &[]), Ok(vec![real_home]), "home is out");
        let at =
            |line: &str| line.replace('~', &base.display().to_string()).replace("/.cargo/registry", "/reg");
        for line in [one, list] {
            let line = at(line);
            assert_eq!(outside(&line, &p, &p, &none, &crates, &[]), Ok(vec![]), "{line}");
            assert!(!runs_outside(&line, &p, &p, &[]), "{line}");
        }
        let into = reg.join("index-a/libc-0.2.189/src");
        assert_eq!(outside(&at(one), &p, &p, &none, &none, &[]), Ok(vec![into.clone()]), "not allowed");
        // A name starting with `.` isn't one `*` names; `[ ]` and `..` after a wildcard can't be told
        let glob = Word { text: format!("{}/*/libc-0.2.189/src", reg.display()), glob: true };
        assert_eq!(cd_into(&p, &glob, &[]), Ok(into));
        for bad in ["*/../../..", "[i]*/libc-0.2.189"] {
            let w = Word { text: format!("{}/{bad}", reg.display()), glob: true };
            assert!(cd_into(&p, &w, &[]).is_err(), "{bad}");
        }
        let climbs = format!("ls {}/*/../../..", reg.display());
        assert!(outside(&climbs, &p, &p, &none, &crates, &[]).is_err(), "out past the wildcard");
        // Two folders it could be: the shell's pick isn't told — asked, saying so
        std::fs::create_dir_all(base.join("reg/src/index-b/libc-0.2.189/src")).unwrap();
        let r = outside(&at(one), &p, &p, &none, &crates, &[]);
        assert!(r.as_ref().is_err_and(|e| e.ends_with("2 folders match")), "{r:?}");
        assert!(runs_outside(&at(one), &p, &p, &[]));
        std::fs::remove_dir_all(&base).ok();
        for (pat, name, is) in [
            ("libc-*", "libc-0.2", true),
            ("*-0.2.*", "libc-0.2.189", true),
            ("l?bc", "libc", true),
            ("*", "", true),
            ("a*b*c", "abxbc", true),
            ("libc-*", "lib-0.2", false),
            ("l?bc", "lbc", false),
            ("*.rs", "a.rsx", false),
        ] {
            assert_eq!(wild_match(pat, name), is, "{pat} {name}");
        }
    }
}
