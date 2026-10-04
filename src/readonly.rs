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
                        // A `$` nothing can follow as a name (a regex's end: `"^.*note$"`, `"a$|b"` — #92) is
                        // itself, as sh reads it; before anything else it may expand
                        '$' if chars.peek().is_some_and(|c| " \t\"|)/.,:]".contains(*c)) => w.text.push('$'),
                        '$' | '`' => return None,
                        // As sh reads it in double quotes: `\` takes `$ ` " \` as themselves, and stays
                        // before anything else (#80: `rg "sleep\(|a *[<>]"` went to a card) — a line
                        // continuation isn't read
                        '\\' => match chars.next()? {
                            '\n' => return None,
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
/// finds programs and libraries, what shells read at their start, what the sandbox sets or reads
/// (`sandbox::inside_env`, its home and temp folders), and what makes a program run another program or
/// load code (rustc and cargo's wrappers, runners, linkers and flags — `-C linker=` runs one —, git's ssh,
/// helpers and config, interpreters' startup options and paths, editors, pagers and password askers). A
/// name ending in `_` is a prefix, one starting with `_` a suffix (`CARGO_TARGET_<triple>_RUNNER`).
const ENV_NEVER: [&str; 40] = [
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

/// Whether a pattern may carry `name` among its assignments.
pub fn env_allowed(name: &str) -> bool {
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
    let (parts, _) = split(line, true)?;
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
            Part::Cd(d) => here = std::fs::canonicalize(here.join(d)).ok()?,
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
    let any = |f: &dyn Fn(&str) -> bool| args.iter().any(|w| f(w));
    match name {
        "git" => match git_after_c(args) {
            [sub, ..] if GIT.contains(sub) => true,
            ["branch"] => true,
            _ => false,
        },
        "sed" => sed_prints(args),
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
                    .or_else(|| w.text.find('/').map(|i| &w.text[i..]))?,
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
pub fn runs_outside(cmd: &str, cwd: &Path, root: &Path) -> bool {
    let Some(parts) = parse(cmd) else { return true };
    let inside = |p: &Path| std::fs::canonicalize(p).is_ok_and(|r| r.starts_with(root));
    let mut here = cwd.to_path_buf();
    for part in parts {
        match part {
            Part::Cd(dir) => match std::fs::canonicalize(here.join(&dir)) {
                Ok(d) if d.is_dir() => here = d,
                _ => return true,
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
            "tree -o out",
            "file -C -m magic",
            "tail -f log.txt",
            "tail -F log.txt",
            "tail --follow log.txt",
            "rg --pre sh fn",
            "uniq in.txt out.txt",
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
            "grep \"a\\",
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

    /// Review (#93's pins): only a text script in the checkout is pinned as it is — `#!` at its start or a
    /// script's ending, no NUL (a binary), its real path inside the checkout (a link out isn't); a program by
    /// its path (a runner anywhere, an absolute path, one through `..`) is no script at all. (Fixtures are
    /// files only — nothing here runs them.)
    #[test]
    fn only_a_text_script_in_the_checkout_is_pinned() {
        let dir = std::env::temp_dir().join(format!("tarae-pins-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("p/sub")).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
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
        // `git -C <dir>`: the folder is a read, the subcommand isn't a path (#57)
        assert_eq!(at("git -C ../out log"), std::slice::from_ref(&out));
        assert!(at("git -C src ls-files a.rs").is_empty());
        assert_eq!(at("git -C src ls-files ../../out"), std::slice::from_ref(&out), "from where -C goes");
        // #88: a -C behind git's harmless options is a read all the same, and where it runs (`runs_outside`)
        assert_eq!(at("git --no-pager -C ../out log"), std::slice::from_ref(&out));
        assert_eq!(at("git -C src --no-optional-locks ls-files ../../out"), std::slice::from_ref(&out));
        assert!(runs_outside("git --no-optional-locks -C ../out status", &p, &p));
        assert!(!runs_outside("git --no-optional-locks -C src status", &p, &p));
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
