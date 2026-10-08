//! Card warnings as data (cardburden 4): what a run card warns of, in plain words, in the warning colour
//! under what it asks — each from a data entry, a pack's `[[claude.warn]]` or the project's `[[cards.warn]]`.
//! The core decides facts only — the words of the line and of the scripts it runs, their options, what the
//! run's environment sets, what it reads outside the project, whether a script it runs changed since it was
//! seen, whether it runs outside the sandbox; the data says which facts warn, and in what words. No tool's
//! name is in this file.
//!
//! ```toml
//! [[claude.warn]]
//! id = "docker-own-config"                      # the card's log line: warn=docker-own-config
//! run = ["docker", "docker compose", "podman"]  # a command's leading words (after NAME=value and a wrapper)
//! missing = [                                   # each one missing named; none missing: no warning
//!   { env = "DOCKER_CONFIG", say = "no DOCKER_CONFIG: it reads your ~/.docker" },
//!   { wrapper = ["timeout", "gtimeout"], say = "no timeout: a hung container holds the run" },
//! ]
//! say = "Docker: {missing}."
//! ```
//!
//! Other keys: `options` (any of them: `-r` counts in a cluster `-rf`, `--recursive` as itself or `=…`, a
//! letter without a dash only as the first word, of letters only — `ps e`) with `option-values` (options whose
//! next word is their value, never an option: `ps -o user`); `unless-part` (a regex the command's own text
//! matches) with `unless-all` (regexes matching before it, in their order, in the line or the script it's in —
//! a checked form's guards before its `rm`) and `same` (named groups equal across all of them — what a
//! backreference would say); `unless-in = "outputs"` (every path it names
//! is in the project below a pack's `outputs` folder: a build's scratch); `fact` (`reads-outside`,
//! `script-changed`, `edit-protected`, `outside-sandbox`); `asks = true` (`a` on an earlier card doesn't let
//! it through unasked — D2). `{missing}` and `{paths}` are filled in.
//!
//! Each text an entry puts on a card (`say`, `missing`'s) is one line of words: no control character, no mark of
//! the card's, at most `SAY_MAX` characters — whose it is, tarae says after it. An id is one entry's: the first
//! to say it keeps it (the packs', then yours — `config.rs` refuses a project's that's taken).
//!
//! A warning never answers a card nor holds one back; it's said. Where a command can't be read as words (a
//! `$(…)`, a redirect), it's split the way a shell splits it, quotes taken off — read too widely, a warning
//! only says more.

use std::path::{Path, PathBuf};

use regex::Regex;
use toml::Value;

/// A fact of the core's a warning may be about (`fact`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fact {
    /// It reads outside the project (the card lists where).
    ReadsOutside,
    /// A script it runs changed since a card showed it, or since its last answer (`sinceyes`).
    ScriptChanged,
    /// An edit of Claude's to a protected file (`sandbox::protected_names`) — the edit review's.
    EditProtected,
    /// It asks to run outside the sandbox.
    OutsideSandbox,
}

impl Fact {
    fn of(s: &str) -> Option<Fact> {
        match s {
            "reads-outside" => Some(Fact::ReadsOutside),
            "script-changed" => Some(Fact::ScriptChanged),
            "edit-protected" => Some(Fact::EditProtected),
            "outside-sandbox" => Some(Fact::OutsideSandbox),
            _ => None,
        }
    }
}

/// What a warning wants a command to have (`missing`): a variable set, or a wrapper around it.
#[derive(Clone, Debug)]
pub enum Need {
    Env { name: String, say: String },
    Wrapper { names: Vec<String>, say: String },
}

/// One warning, as its data says it.
#[derive(Clone, Debug)]
pub struct Warn {
    pub id: String,
    /// Whose it is, as the card says it: `tools pack`, `.tarae.toml`.
    pub whose: String,
    /// The user's own (`config.toml`, `.tarae.local.toml`) — a project's `.tarae.toml` can't take its id.
    pub yours: bool,
    pub run: Vec<Vec<String>>,
    pub options: Vec<String>,
    /// Options whose next word is their value, never an option (`ps -o user`: `user`).
    pub option_values: Vec<String>,
    pub missing: Vec<Need>,
    pub unless_part: Option<Regex>,
    pub unless_all: Vec<Regex>,
    pub same: Vec<String>,
    pub unless_outputs: bool,
    pub fact: Option<Fact>,
    pub say: String,
    pub asks: bool,
}

/// The entries of a `warn` list (`[[claude.warn]]`, `[[cards.warn]]`), each checked: a wrong one is left out,
/// and why said (`problems`) — the rest kept.
pub fn read(v: &Value, whose: &str, problems: &mut Vec<String>) -> Vec<Warn> {
    let Some(list) = v.as_array() else {
        problems.push("`warn` isn't a list of tables".into());
        return Vec::new();
    };
    list.iter()
        .filter_map(|e| match entry(e, whose) {
            Ok(w) => Some(w),
            Err(why) => {
                let id = e.get("id").and_then(Value::as_str).unwrap_or("?");
                problems.push(format!("warning `{id}`: {why}"));
                None
            }
        })
        .collect()
}

/// A warning's words at most (characters) — a card's line, not a page; and each of `missing`'s.
const SAY_MAX: usize = 300;
const MISSING_SAY_MAX: usize = 160;

/// `s`, a warning's words (`what` names the key), as a card may show them: one line — no control character
/// (a newline would make lines of its own that read as tarae's, `Its reason: …`), no mark that's the card's
/// (`▲`, which starts a warning line; bidi and zero-width marks), at most `max` characters. The whose-it-is tag
/// is tarae's, put after it.
fn card_words(s: &str, what: &str, max: usize) -> Result<String, String> {
    let hidden = |c: char| {
        c.is_control()
            || matches!(c, '▲' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}')
    };
    if s.chars().any(hidden) {
        return Err(format!(
            "`{what}` has a control character or a mark of the card's (one line, words only)"
        ));
    }
    if s.chars().count() > max {
        return Err(format!("`{what}` is longer than {max} characters"));
    }
    Ok(s.to_string())
}

const KEYS: [&str; 12] = [
    "id",
    "run",
    "options",
    "option-values",
    "missing",
    "unless-part",
    "unless-all",
    "same",
    "unless-in",
    "fact",
    "say",
    "asks",
];

fn entry(e: &Value, whose: &str) -> Result<Warn, String> {
    let t = e.as_table().ok_or("isn't a table")?;
    if let Some(k) = t.keys().find(|k| !KEYS.contains(&k.as_str())) {
        return Err(format!("no key `{k}` — {}", KEYS.join(", ")));
    }
    let s = |k: &str| t.get(k).and_then(Value::as_str).map(str::to_string);
    let list = |k: &str| -> Result<Vec<String>, String> {
        match t.get(k) {
            None => Ok(Vec::new()),
            Some(Value::Array(a)) => a
                .iter()
                .map(|x| x.as_str().map(str::to_string).ok_or(format!("`{k}` isn't strings")))
                .collect(),
            Some(_) => Err(format!("`{k}` isn't a list")),
        }
    };
    let id = s("id").filter(|i| !i.is_empty()).ok_or("no `id`")?;
    if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err("`id` is letters, digits and `-` (it goes in the log line)".into());
    }
    let say = s("say").filter(|s| !s.trim().is_empty()).ok_or("no `say`")?;
    let say = card_words(&say, "say", SAY_MAX)?;
    let run: Vec<Vec<String>> =
        list("run")?.iter().map(|r| r.split_whitespace().map(str::to_string).collect()).collect();
    if run.iter().any(Vec::is_empty) {
        return Err("an empty `run`".into());
    }
    let fact = match s("fact") {
        Some(f) => Some(Fact::of(&f).ok_or(format!(
            "no fact `{f}` — reads-outside, script-changed, edit-protected, outside-sandbox"
        ))?),
        None => None,
    };
    if run.is_empty() && fact.is_none() {
        return Err("neither `run` nor `fact`: it would warn of every command".into());
    }
    let regex = |r: &str| Regex::new(r).map_err(|e| format!("`{r}`: {e}"));
    let unless_part = s("unless-part").map(|r| regex(&r)).transpose()?;
    let unless_all = list("unless-all")?.iter().map(|r| regex(r)).collect::<Result<Vec<_>, _>>()?;
    let same = list("same")?;
    let all: Vec<&Regex> = unless_part.iter().chain(&unless_all).collect();
    for g in &same {
        if !all.iter().any(|r| r.capture_names().flatten().any(|n| n == g)) {
            return Err(format!("`same` names `{g}`, which no `unless` regex captures"));
        }
    }
    if (unless_part.is_some() || !unless_all.is_empty()) && run.is_empty() {
        return Err("`unless-part`/`unless-all` without `run`".into());
    }
    let unless_outputs = match s("unless-in").as_deref() {
        None => false,
        Some("outputs") => true,
        Some(o) => return Err(format!("`unless-in = \"{o}\"` — only \"outputs\"")),
    };
    let mut missing = Vec::new();
    if let Some(m) = t.get("missing") {
        for n in m.as_array().ok_or("`missing` isn't a list")? {
            let n = n.as_table().ok_or("a `missing` entry isn't a table")?;
            let say = n.get("say").and_then(Value::as_str).ok_or("a `missing` entry has no `say`")?;
            let say = card_words(say, "missing.say", MISSING_SAY_MAX)?;
            match (n.get("env").and_then(Value::as_str), n.get("wrapper")) {
                (Some(env), None) => missing.push(Need::Env { name: env.to_string(), say }),
                (None, Some(Value::Array(w))) => {
                    let names = w.iter().filter_map(|x| x.as_str().map(str::to_string)).collect();
                    missing.push(Need::Wrapper { names, say })
                }
                _ => return Err("a `missing` entry is `env` or `wrapper` (a list), with `say`".into()),
            }
        }
    }
    if say.contains("{missing}") && missing.is_empty() {
        return Err("`say` has `{missing}` but there's no `missing`".into());
    }
    if say.contains("{paths}") && fact != Some(Fact::ReadsOutside) {
        return Err("`{paths}` is said only with `fact = \"reads-outside\"`".into());
    }
    let asks =
        t.get("asks").map(|a| a.as_bool().ok_or("`asks` isn't true or false")).transpose()?.unwrap_or(false);
    let options = list("options")?;
    let option_values = list("option-values")?;
    Ok(Warn {
        id,
        whose: whose.to_string(),
        yours: false,
        run,
        options,
        option_values,
        missing,
        unless_part,
        unless_all,
        same,
        unless_outputs,
        fact,
        say,
        asks,
    })
}

/// What warnings are judged on: the command line and what it runs, and the facts the core knows of it.
pub struct Facts<'a> {
    pub cmd: &'a str,
    /// The scripts it runs (shown name, text) — what's in them runs too.
    pub scripts: &'a [(String, String)],
    /// Whether a variable is set in the run's environment (not in front of the command — that's read here).
    pub env: &'a dyn Fn(&str) -> bool,
    /// Programs that run what follows their words (`timeout 600 docker …`) — the packs' `wrappers`.
    pub wrappers: &'a [String],
    /// Where it reads outside the project.
    pub reads: &'a [PathBuf],
    pub script_changed: bool,
    pub edit_protected: bool,
    pub outside: bool,
    /// Whether a path it names is in the project below a pack's `outputs` folder (`unless-in`).
    pub in_outputs: &'a dyn Fn(&str) -> bool,
}

/// A warning that holds for a command: its id, its words filled in, whose, and whether `a` lets it through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Warned {
    pub id: String,
    pub said: String,
    pub whose: String,
    pub asks: bool,
}

impl Warned {
    /// As a card's line says it: `Docker: no timeout … (tools pack)`.
    pub fn line(&self) -> String {
        format!("{} ({})", self.said, self.whose)
    }
}

/// The warnings among `warns` that hold for `f`, each once, in their order.
pub fn judge<'w>(warns: impl IntoIterator<Item = &'w Warn>, f: &Facts) -> Vec<Warned> {
    let line = commands(f.cmd);
    let scripts: Vec<Vec<Command>> = f.scripts.iter().map(|(_, t)| commands(t)).collect();
    // (what the line sets and wraps around a script it runs: `DOCKER_HOST=x timeout 600 sh run.sh`)
    let inherited = Around::of(&line, f.wrappers);
    let mut out: Vec<Warned> = Vec::new();
    for w in warns {
        let warned = match w.fact {
            Some(fact) if !holds(fact, f) => None,
            Some(_) if w.run.is_empty() => Some(fill(w, &[], f)),
            _ => {
                let mut said = None;
                // (the line's commands, then each script's — each with its own text and what's around it)
                let groups = std::iter::once((&line, f.cmd, Around::default())).chain(
                    scripts.iter().zip(f.scripts).map(|(s, (_, text))| (s, text.as_str(), inherited.clone())),
                );
                'all: for (cmds, text, around) in groups {
                    for (i, c) in cmds.iter().enumerate() {
                        let Some(at) = matched(w, c, f.wrappers) else { continue };
                        if !w.options.is_empty() && !has_option(&c.words[at..], &w.options, &w.option_values)
                        {
                            continue;
                        }
                        if excused(w, c, at, text, f) {
                            continue;
                        }
                        let set_before = Around::of(&cmds[..i], f.wrappers);
                        let missing: Vec<&str> = w
                            .missing
                            .iter()
                            .filter(|n| !needed_there(n, c, at, &set_before, &around, f))
                            .map(|n| match n {
                                Need::Env { say, .. } | Need::Wrapper { say, .. } => say.as_str(),
                            })
                            .collect();
                        if !w.missing.is_empty() && missing.is_empty() {
                            continue;
                        }
                        said = Some(fill(w, &missing, f));
                        break 'all;
                    }
                }
                said
            }
        };
        if let Some(said) = warned
            && !out.iter().any(|o| o.id == w.id)
        {
            out.push(Warned { id: w.id.clone(), said, whose: w.whose.clone(), asks: w.asks });
        }
    }
    out
}

fn holds(fact: Fact, f: &Facts) -> bool {
    match fact {
        Fact::ReadsOutside => !f.reads.is_empty(),
        Fact::ScriptChanged => f.script_changed,
        Fact::EditProtected => f.edit_protected,
        Fact::OutsideSandbox => f.outside,
    }
}

/// `w`'s words, `{missing}` and `{paths}` filled in — a path's control characters made spaces (a file's name
/// may have a newline: the warning stays one line).
fn fill(w: &Warn, missing: &[&str], f: &Facts) -> String {
    let paths: Vec<String> = f.reads.iter().map(|p| crate::allow::tilde(p)).collect();
    let paths: String = paths.join(", ").chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    w.say.replace("{missing}", &missing.join("; ")).replace("{paths}", &paths)
}

/// One command of a line or a script: its text as written, where it starts there (bytes), its words (quotes
/// off; leading `NAME=value` words kept in front).
#[derive(Clone, Debug, PartialEq)]
struct Command {
    raw: String,
    at: usize,
    words: Vec<String>,
}

/// Where `w`'s `run` starts in `c`'s words: after its `NAME=value` words and keywords; or, its first word a
/// wrapper, anywhere after it (`timeout -k 5 600 docker run`). None: not this command.
fn matched(w: &Warn, c: &Command, wrappers: &[String]) -> Option<usize> {
    let start = lead(&c.words);
    let words = &c.words[start..];
    let at = |run: &Vec<String>, k: usize| words.len() >= k + run.len() && words[k..k + run.len()] == run[..];
    for run in &w.run {
        if at(run, 0) {
            return Some(start + run.len());
        }
        if words.first().is_some_and(|f| wrappers.contains(f))
            && let Some(k) = (1..words.len()).find(|&k| at(run, k))
        {
            return Some(start + k + run.len());
        }
    }
    None
}

/// How many of `words` lead a command without being it: `NAME=value`, and a shell's keywords.
pub(crate) fn lead(words: &[String]) -> usize {
    const KEYWORDS: [&str; 11] =
        ["if", "then", "else", "elif", "do", "while", "until", "!", "{", "exec", "command"];
    words.iter().take_while(|w| assign(w).is_some() || KEYWORDS.contains(&w.as_str())).count()
}

/// The name a `NAME=value` word sets.
fn assign(w: &str) -> Option<&str> {
    let (name, _) = w.split_once('=')?;
    let ok = !name.is_empty()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !name.starts_with(|c: char| c.is_ascii_digit());
    ok.then_some(name)
}

/// Whether one of `options` is among `words` (the command's words after `run`), read as options are: `-r` in a
/// cluster (`-rf`), `--recursive` itself or with `=`, a dashless letter only in the first word and a word of
/// letters only (`ps e`, `ps auxe` — BSD's form); the word after one of `values` is its value, never an option
/// (`ps -o user`).
fn has_option(words: &[String], options: &[String], values: &[String]) -> bool {
    let mut i = 0;
    while i < words.len() {
        let w = &words[i];
        if options.iter().any(|o| is_option(w, o, i == 0)) {
            return true;
        }
        i += if values.contains(w) { 2 } else { 1 };
    }
    false
}

/// Whether word `w` is option `o` (`first`: the command's first word after its name).
fn is_option(w: &str, o: &str, first: bool) -> bool {
    match (o.strip_prefix("--"), o.strip_prefix('-')) {
        (Some(long), _) => {
            w == o
                || w.strip_prefix("--").and_then(|r| r.strip_prefix(long)).is_some_and(|r| r.starts_with('='))
        }
        (None, Some(short)) => w.strip_prefix('-').is_some_and(|c| {
            !c.starts_with('-') && c.chars().all(|x| x.is_ascii_alphanumeric()) && c.contains(short)
        }),
        (None, None) => first && w.chars().all(|x| x.is_ascii_alphabetic()) && w.contains(o),
    }
}

/// What the commands before (or the line around a script) set and wrap: names set (`NAME=value` alone,
/// `export NAME=…`, or in front of a command), and the wrappers' names among their words.
#[derive(Clone, Debug, Default)]
struct Around {
    set: Vec<String>,
    wrappers: Vec<String>,
}

impl Around {
    fn of(cmds: &[Command], wrappers: &[String]) -> Around {
        let mut a = Around::default();
        for c in cmds {
            let words = match c.words.first().map(String::as_str) {
                Some("export") => &c.words[1..],
                _ => &c.words[..],
            };
            a.set.extend(words.iter().map_while(|w| assign(w)).map(str::to_string));
            a.wrappers.extend(c.words.iter().filter(|w| wrappers.contains(w)).cloned());
        }
        a
    }
}

/// Whether `n` is had by command `c` (its `run` ends at `at`): a variable set in front of it, before it, around
/// its script, or in the run's environment; a wrapper's name among its own words before its `run`, or around
/// the script it's in — never one of an earlier command's (`timeout 5 true; docker run …` isn't wrapped).
fn needed_there(n: &Need, c: &Command, at: usize, before: &Around, around: &Around, f: &Facts) -> bool {
    match n {
        Need::Env { name, .. } => {
            c.words[..lead(&c.words)].iter().any(|w| assign(w) == Some(name.as_str()))
                || before.set.contains(name)
                || around.set.contains(name)
                || (f.env)(name)
        }
        Need::Wrapper { names, .. } => {
            c.words[..at].iter().any(|w| names.contains(w))
                || around.wrappers.iter().any(|w| names.contains(w))
        }
    }
}

/// Whether `w`'s `unless` holds for command `c` (its `run` ending at `at`), in `text` (the line or the script
/// it's in): every path it names is a build's scratch (`unless-in`); or its own text matches `unless-part`, and
/// each of `unless-all` matches before it in the same text, in their order, one after the other — the form's
/// guards come before its `rm` — the `same` groups equal in all of them (the first match that agrees taken).
fn excused(w: &Warn, c: &Command, at: usize, text: &str, f: &Facts) -> bool {
    if w.unless_outputs {
        let paths: Vec<&String> = c.words[at..].iter().filter(|x| !x.starts_with('-')).collect();
        if !paths.is_empty() && paths.iter().all(|p| (f.in_outputs)(p)) {
            return true;
        }
    }
    let Some(part) = &w.unless_part else { return false };
    let Some(own) = part.captures(&c.raw) else { return false };
    // (each `same` group's value once a regex has fixed it — the command's own first)
    let mut fixed: Vec<(&str, String)> =
        w.same.iter().filter_map(|g| own.name(g).map(|m| (g.as_str(), m.as_str().to_string()))).collect();
    let until = c.at.min(text.len());
    let mut from = 0;
    for r in &w.unless_all {
        if from > until {
            return false;
        }
        let agrees =
            |cap: &regex::Captures| fixed.iter().all(|(g, v)| cap.name(g).is_none_or(|m| m.as_str() == v));
        let Some(cap) = r.captures_iter(&text[from..until]).find(agrees) else { return false };
        for g in &w.same {
            if let Some(m) = cap.name(g)
                && !fixed.iter().any(|(n, _)| n == g)
            {
                fixed.push((g.as_str(), m.as_str().to_string()));
            }
        }
        from += cap.get(0).map_or(0, |m| m.end());
    }
    true
}

/// The commands of a line or a script, as a shell splits them: at `;` `&` `|` `(` `)` a newline, a backtick
/// and `$(`, outside quotes; words at whitespace, quotes taken off; a `#` starting a word ends the line. An
/// `sh -c '<body>'` is its body's commands too.
fn commands(text: &str) -> Vec<Command> {
    let mut out = Vec::new();
    let (mut raw, mut words, mut word) = (String::new(), Vec::<String>::new(), String::new());
    let (mut quote, mut in_word, mut comment) = (None::<char>, false, false);
    let mut chars = text.char_indices().peekable();
    // (where the command under way starts: its text from there, leading blanks aside)
    let mut begin = 0;
    let end = |raw: &mut String, words: &mut Vec<String>, out: &mut Vec<Command>, begin: usize| {
        let at = begin + (raw.len() - raw.trim_start().len());
        let r = raw.trim().to_string();
        if !words.is_empty() {
            out.push(Command { raw: r, at, words: std::mem::take(words) });
        }
        raw.clear();
    };
    while let Some((i, ch)) = chars.next() {
        if comment {
            if ch == '\n' {
                comment = false;
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
                end(&mut raw, &mut words, &mut out, begin);
                begin = i + 1;
            }
            continue;
        }
        match (quote, ch) {
            (Some(q), c) if c == q => {
                quote = None;
                raw.push(c);
            }
            (Some('"'), '\\') => {
                raw.push(ch);
                if let Some((_, n)) = chars.next() {
                    raw.push(n);
                    word.push(n);
                }
            }
            (Some(_), c) => {
                raw.push(c);
                word.push(c);
            }
            (None, '\'' | '"') => {
                quote = Some(ch);
                in_word = true;
                raw.push(ch);
            }
            (None, '\\') => {
                raw.push(ch);
                if let Some((_, n)) = chars.next() {
                    raw.push(n);
                    if n != '\n' {
                        word.push(n);
                        in_word = true;
                    }
                }
            }
            (None, '#') if !in_word => comment = true,
            (None, c) if c.is_whitespace() && c != '\n' => {
                raw.push(c);
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            (None, c)
                if matches!(c, ';' | '&' | '|' | '(' | ')' | '\n' | '`')
                    || (c == '$' && chars.peek().is_some_and(|(_, n)| *n == '(')) =>
            {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
                end(&mut raw, &mut words, &mut out, begin);
                // (the next starts after it — after the `(` of a `$(`)
                begin = i + 1;
                if c == '$' {
                    chars.next();
                    begin += 1;
                }
            }
            (None, c) => {
                raw.push(c);
                word.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(word);
    }
    end(&mut raw, &mut words, &mut out, begin);
    // (`sh -c '<body>'`: the body's commands are what runs — each placed where the `sh` is: nothing in the body
    // is before it, so a checked form's guards inside one don't count)
    let shells = ["sh", "bash", "zsh", "dash"];
    let mut more = Vec::new();
    for c in &out {
        let s = lead(&c.words);
        if let [shell, dash_c, body, ..] = &c.words[s..]
            && shells.contains(&shell.as_str())
            && dash_c == "-c"
        {
            more.extend(commands(body).into_iter().map(|b| Command { at: c.at, ..b }));
        }
    }
    out.extend(more);
    out
}

/// Whether `word` (a path a command names, from `cwd`) is in the project at `root` below one of `outputs`
/// (folder names — the packs' `outputs`, `target`): a relative path, no `..`, no glob, no variable.
pub fn in_outputs(root: &Path, cwd: &Path, word: &str, outputs: &[String]) -> bool {
    if word.starts_with(['/', '~', '$']) || word.contains(['*', '?', '[', '$']) {
        return false;
    }
    let rel = Path::new(word);
    if rel.components().any(|c| !matches!(c, std::path::Component::Normal(_) | std::path::Component::CurDir))
    {
        return false;
    }
    let Ok(from_root) = cwd.join(rel).strip_prefix(root).map(Path::to_path_buf) else { return false };
    let mut parts = from_root.components().filter(|c| !matches!(c, std::path::Component::CurDir));
    parts.next().is_some_and(|first| outputs.iter().any(|o| first.as_os_str() == o.as_str()))
}

#[cfg(test)]
mod tests;
