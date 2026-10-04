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
//! a redirect to a file (`2>&1` and `>/dev/null` are fine), a lone `&`, `~` at a word's start, an
//! assignment, a newline, and an `sh -c` inside the body. A glob is fine where its fixed folder is read.

use std::path::{Path, PathBuf};

use crate::allow::Allowed;

/// One word of a line, quotes taken off; `glob`: it has an unquoted `*` `?` `[` (the shell fills it in).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Word {
    pub text: String,
    pub glob: bool,
}

/// One part of a line: a command's words, or a `cd` into a folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Part {
    Cd(String),
    Cmd(Vec<Word>),
}

/// Shells whose `-c '<body>'` is read as the body (once — not one inside another).
const SHELLS: [&str; 4] = ["sh", "bash", "zsh", "dash"];

/// The parts of `line` (an `sh -c '…'` around it taken off), if it's only words, `cd` and `;` `&&` `||` `|`
/// — None for anything else (see the module's doc).
pub fn parse(line: &str) -> Option<Vec<Part>> {
    split(line, false).map(|(parts, _)| parts)
}

/// The commands of `line` when it's one command piped into others (`a 2>&1 | b | c`) and nothing else —
/// every part a command, every separator a single `|`.
pub fn piped(line: &str) -> Option<Vec<Vec<Word>>> {
    let (parts, piped) = split(line, false)?;
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

/// The line's parts (an `sh -c '…'` around it taken off) and, between each two, whether it's a single `|`.
/// `env`: a command's leading `NAME=value` words are kept as its first words (else such a line is None).
fn split(line: &str, env: bool) -> Option<(Vec<Part>, Vec<bool>)> {
    let outer = parts(line, env)?;
    if let [Part::Cmd(w)] = &outer.0[..]
        && let [shell, c, body] = &w[..]
        && SHELLS.contains(&shell.text.as_str())
        && c.text == "-c"
        && !body.glob
    {
        let inner = parts(&body.text, env)?;
        // (an `sh -c` inside that one isn't read)
        let nested =
            inner.0.iter().any(|p| matches!(p, Part::Cmd(w) if SHELLS.contains(&w[0].text.as_str())));
        return (!nested).then_some(inner);
    }
    Some(outer)
}

/// The line's parts and their separators (`split`), without looking for an `sh -c` around them. `env`:
/// leading `NAME=value` words stay in front of a command's words.
fn parts(line: &str, env: bool) -> Option<(Vec<Part>, Vec<bool>)> {
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
    let end_part = |words: &mut Vec<Word>, parts: &mut Vec<Part>, at: &mut Assign| -> Option<()> {
        let w = std::mem::take(words);
        let lead = std::mem::take(&mut at.lead);
        let first = w.get(if env { lead } else { 0 })?;
        // An assignment where the command goes (`PAGER=sh git log`) isn't a command's word
        if first.text.contains('=') || first.glob || (lead > 0 && first.text == "cd") {
            return None;
        }
        parts.push(match &w[..] {
            [cd, dir] if cd.text == "cd" && !dir.glob && !dir.text.starts_with('-') => {
                Part::Cd(dir.text.clone())
            }
            [cd, ..] if cd.text == "cd" => return None,
            _ => Part::Cmd(w),
        });
        Some(())
    };
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => end_word(&mut word, &mut words, &mut at),
            '\'' => {
                at.quoted = true;
                let w = word.get_or_insert(Word { text: String::new(), glob: false });
                loop {
                    match chars.next()? {
                        '\'' => break,
                        c => w.text.push(c),
                    }
                }
            }
            '"' => {
                at.quoted = true;
                let w = word.get_or_insert(Word { text: String::new(), glob: false });
                loop {
                    match chars.next()? {
                        '"' => break,
                        '$' | '`' | '\\' => return None,
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
                    ('&', _) => return None,
                    (c, _) => c == '|',
                };
                end_part(&mut words, &mut parts, &mut at)?;
                piped.push(pipe);
            }
            '>' => {
                // Only to nowhere: `2>&1`, `>/dev/null`, `2>/dev/null` (the fd as the word before it)
                if word.as_ref().is_some_and(|w| !matches!(w.text.as_str(), "1" | "2")) {
                    return None;
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
                    return None;
                }
            }
            '$' | '`' | '\\' | '(' | ')' | '{' | '}' | '<' | '!' | '\n' | '\r' => return None,
            '#' | '~' if word.is_none() => return None,
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
    Some((parts, piped))
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
/// finds programs and libraries, what shells read at their start, and what the sandbox sets or reads
/// (`sandbox::inside_env`, its home and temp folders). `LD_`/`DYLD_` are prefixes.
const ENV_NEVER: [&str; 11] = [
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
];

/// Whether a pattern may carry `name` among its assignments.
pub fn env_allowed(name: &str) -> bool {
    env_name(name)
        && !ENV_NEVER.iter().any(|n| if n.ends_with('_') { name.starts_with(n) } else { name == *n })
}

/// `one_command` with `NAME=value` words in front (`A=1 B=x cargo xtask shot …`, in an `sh -c` or not):
/// the names set, in order, and the command's words. None as for `one_command`.
pub fn one_command_env(line: &str) -> Option<(Vec<String>, Vec<String>)> {
    let (parts, _) = split(line, true)?;
    let [Part::Cmd(w)] = &parts[..] else { return None };
    if w.iter().any(|w| w.glob) {
        return None;
    }
    let lead = w.iter().take_while(|w| w.text.split_once('=').is_some_and(|(n, _)| env_name(n))).count();
    let names = w[..lead].iter().map(|w| w.text.split_once('=').map_or("", |(n, _)| n).to_string());
    Some((names.collect(), w[lead..].iter().map(|w| w.text.clone()).collect()))
}

/// The words of `line` when it's one command and nothing else (quotes taken off, no glob) — what runs,
/// word for word: the checks and a chat's patterns are matched against these.
pub fn one_command(line: &str) -> Option<Vec<String>> {
    match &parse(line)?[..] {
        [Part::Cmd(w)] if !w.iter().any(|w| w.glob) => Some(w.iter().map(|w| w.text.clone()).collect()),
        _ => None,
    }
}

/// The commands that only read (with their options below), by name.
const COMMANDS: [&str; 17] = [
    "ls", "cat", "head", "tail", "wc", "grep", "rg", "find", "tree", "stat", "file", "du", "diff", "sort",
    "uniq", "cut", "pwd",
];

/// git's that only read — `branch` only bare (with words it creates, renames, deletes).
const GIT: [&str; 3] = ["blame", "rev-parse", "ls-files"];

/// Whether `cmd` only reads: a line `parse` reads, each command in it one above with none of the options
/// that make it write a file, run a program, or never end. Where it reads is `outside`'s.
pub fn read_only(cmd: &str) -> bool {
    let Some(parts) = parse(cmd) else { return false };
    parts.iter().all(|p| match p {
        Part::Cd(_) => true,
        Part::Cmd(w) => only_reads(w),
    })
}

/// Whether one command only reads (`read_only`'s rule for each part of a line).
pub fn only_reads(w: &[Word]) -> bool {
    let words: Vec<&str> = w.iter().map(|w| w.text.as_str()).collect();
    !words.iter().any(|w| crate::tools::WRITES.iter().any(|o| w.starts_with(o))) && reads_only(&words)
}

/// One command's words: a command above, without the options that make it write, run, or never end.
fn reads_only(words: &[&str]) -> bool {
    let Some((&name, args)) = words.split_first() else { return false };
    // A short option cluster with `c` in it (`-ro` has `-o`)
    let short = |w: &str, c: char| w.starts_with('-') && !w.starts_with("--") && w[1..].contains(c);
    let any = |f: &dyn Fn(&str) -> bool| args.iter().any(|w| f(w));
    match name {
        "git" => match args {
            [sub, ..] if GIT.contains(sub) => true,
            ["branch"] => true,
            _ => false,
        },
        "find" => !any(&|w| ["-exec", "-ok", "-delete", "-fprint", "-fls"].iter().any(|o| w.starts_with(o))),
        "sort" => !any(&|w| short(w, 'o') || w.starts_with("--compress-program")),
        "tree" => !any(&|w| short(w, 'o')),
        "file" => !any(&|w| short(w, 'C') || w.starts_with("--compile")),
        "tail" => !any(&|w| short(w, 'f') || short(w, 'F') || w.starts_with("--follow")),
        "rg" => !any(&|w| w.starts_with("--pre")),
        // `uniq in out` writes `out`
        "uniq" => args.iter().filter(|w| !w.starts_with('-')).count() <= 1,
        _ => COMMANDS.contains(&name),
    }
}

/// The words of one command that could name a path: every one that isn't an option, and an option's own
/// value (`--file=x`, `-f/x`); a glob as its fixed folder (`../*.rs` reads `..`). A word that names nothing
/// (a pattern) is left out by `outside`.
fn candidates(words: &[Word]) -> Vec<String> {
    let skip = if words.first().is_some_and(|w| w.text == "git") { 2 } else { 1 };
    words
        .iter()
        .skip(skip)
        .filter_map(|w| {
            let path = match w.text.strip_prefix('-') {
                None => w.text.as_str(),
                Some(_) => w
                    .text
                    .split_once('=')
                    .map(|(_, v)| v)
                    .or_else(|| w.text.find('/').map(|i| &w.text[i..]))?,
            };
            if !w.glob {
                return Some(path.to_string());
            }
            let fixed = &path[..path.find(['*', '?', '[', ']']).unwrap_or(path.len())];
            Some(fixed.rfind('/').map_or(".".to_string(), |i| fixed[..i.max(1)].to_string()))
        })
        .filter(|w| !w.is_empty())
        .collect()
}

/// The places `cmd` reads outside `root` (real paths, links followed) that aren't allowed — on a worker.
/// A word that names nothing there isn't a path; `..` and links are resolved. Each command's words are
/// where it runs: after a `cd`, in that folder (its real path) — which is read too. Err: a `cd` into
/// what isn't a folder that's there — the shell would stay where it was and run the rest (after `;` or
/// `||`) from there, so where those read can't be told: the line is asked about.
pub fn outside(
    cmd: &str,
    cwd: &Path,
    root: &Path,
    chat: &Allowed,
    kept: &Allowed,
) -> Result<Vec<PathBuf>, String> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut here = cwd.to_path_buf();
    for part in parse(cmd).unwrap_or_default() {
        let words = match part {
            Part::Cd(dir) => {
                here = std::fs::canonicalize(here.join(&dir))
                    .ok()
                    .filter(|d| d.is_dir())
                    .ok_or_else(|| format!("cd {dir}: no such folder"))?;
                vec![".".to_string()]
            }
            Part::Cmd(w) => candidates(&w),
        };
        for w in words {
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

#[cfg(test)]
mod tests {
    use super::*;

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
            "uniq names.txt",
            "pwd",
            "git blame src/main.rs",
            "git rev-parse HEAD",
            "git ls-files src",
            "git branch",
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
            "tree -o out",
            "file -C -m magic",
            "tail -f log.txt",
            "tail -F log.txt",
            "tail --follow log.txt",
            "rg --pre sh fn",
            "uniq in.txt out.txt",
            "git branch new",
            "git branch -D main",
            "git -C /x blame a",
            "git push",
            "cat a | sh",
            "cat ~/.ssh/id_ed25519",
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
        ] {
            assert!(read_only(c), "{c}");
        }
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
            "cat ~/x",
            "ls ! x",
            // Not read-only, or not a line at all
            "cd /tmp && rm x",
            "cd",
            "cd -",
            "cd a b",
            "xargs cat",
            "echo hi",
            "ls; ",
            "; ls",
            "ls && && ls",
            "grep 'unterminated",
            "grep \"a\\b\" x",
        ] {
            assert!(!read_only(c), "{c}");
        }
        let w = |t: &str| Word { text: t.into(), glob: false };
        assert_eq!(
            parse("cd src && grep -n 'a b' x.rs | head"),
            Some(vec![
                Part::Cd("src".into()),
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
        let at = |cmd: &str| outside(cmd, &p, &p, &none, &none).unwrap();
        assert!(at("cat src/a.rs").is_empty());
        assert!(at("grep -rn nothing-like-a-path src").is_empty(), "a pattern isn't a path");
        assert!(at("ls").is_empty());
        assert_eq!(at("cat ../out/m.txt"), [out.join("m.txt")]);
        assert_eq!(at(&format!("ls {}", out.display())), std::slice::from_ref(&out));
        assert_eq!(at("cat link/m.txt"), [out.join("m.txt")], "a link that leads out");
        assert_eq!(at(&format!("grep --file={}/m.txt src", out.display())), [out.join("m.txt")]);
        assert_eq!(at(&format!("grep -f{}/m.txt src", out.display())), [out.join("m.txt")]);
        assert_eq!(at("git ls-files ../out"), std::slice::from_ref(&out));
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
            let r = outside(line, &p, &p, &none, &none);
            assert!(r.as_ref().is_err_and(|e| e.contains("no such folder")), "{line}: {r:?}");
        }
        // One there is followed to its real path: a link into somewhere else is read there
        assert_eq!(at("cd link; cat m.txt"), [out.clone(), out.join("m.txt")]);
        let allowed = Allowed { read: vec![out.clone()], ..Default::default() };
        assert!(
            outside("cat ../out/m.txt", &p, &p, &allowed, &none).unwrap().is_empty(),
            "allowed this chat"
        );
        assert!(outside("cat ../out/m.txt", &p, &p, &none, &allowed).unwrap().is_empty(), "kept");
        std::fs::remove_dir_all(&dir).ok();
    }
}
