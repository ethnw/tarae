//! Tokens in the settings Claude's runs read (2026-10-05, after card model P2 — `sandbox::reads_for`). A run
//! reads in home what the toolchains need, and some of that can hold a token by design: git's settings
//! (`http.extraHeader`, a token in a `url.<…>.insteadOf`), Go's env file (`go env -w GOPROXY=https://user:…@`),
//! pip's (an `index-url` with `user:pass@`) — and npm's `~/.npmrc` (`_authToken`), kept from every run
//! (`allow::SECRETS`). Measured with fake tokens in a stand-in home under a run's profile
//! (`target/sb/tokens-checkpoint.md`): denied, git fails at every command, and Go, npm and pip lose their
//! settings without a word; each takes a copy by a variable (`Tool::var`). Git is tarae's own; the others are
//! the built-in packs' `[[sandbox.settings]]` (`sandboxpack.rs`). So where a token is there, tarae — on the
//! policy's worker, never Claude — writes a copy without those lines in the run's own folder
//! (`sandbox::RunDir`), points the tool at it, and the run doesn't read the original (`Copies::hidden`);
//! nothing changes where there's none. npm's file is kept whatever it holds, so its copy is always given.
//!
//! A setting that takes no copy (cargo reads its config as an ancestor's — denied, every cargo command fails)
//! is left as it is, and the user is told once, in its pack's words (`tell`: cargo keeps tokens in
//! `credentials.toml`, which runs never read — `told_holders`, `tell_holders`). A card that lets Claude read a
//! file or a folder says first when one holds a token-like line (`found_in`) — the file's name, never the value.

use std::path::{Path, PathBuf};

use crate::editor::Editor;
use crate::sandbox::Os;

// ── What looks like a token ─────────────────────────────────────────────────

/// Keys named for a secret (lowercased, anywhere in the key: `//host/:_authToken`, `GITHUB_TOKEN`,
/// `npmAuthIdent` — yarn's `user:password`). `auth` alone counts only as a word of the key (`AUTH_WORDS`).
const SECRET_KEYS: [&str; 14] = [
    "token",
    "password",
    "passwd",
    "secret",
    "authident",
    "auth_ident",
    "auth-ident",
    "apikey",
    "api_key",
    "api-key",
    "private_key",
    "private-key",
    "access_key",
    "access-key",
];

/// Words of a key that name a secret on their own (`_auth`, `NPM_CONFIG__AUTH`, `BASIC_AUTH`,
/// `YARN_NPM_AUTH_IDENT`) — a word, so `GIT_AUTHOR_NAME` isn't one.
const AUTH_WORDS: [&str; 1] = ["auth"];

/// …but not a key that names where one is or how it's got (`password_file`, `token-cmd`, `credential.helper`).
const NOT_THE_VALUE: [&str; 8] = ["file", "path", "helper", "command", "cmd", "url", "type", "env"];

/// Tokens known by how they start: GitHub's, GitLab's, npm's, PyPI's, Slack's.
const PREFIXES: [&str; 11] =
    ["ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_", "glpat-", "npm_", "pypi-Ag", "xoxb-", "xoxp-"];

/// Whether `line` holds what looks like a token: a URL with a password (`https://user:pass@`) or a long user
/// (`https://ghp_…@`), a key named for one with a value written there (`token = …`, `//host/:_authToken=…`,
/// `"password": "…"` — not `${VAR}`), an option giving one (`-Dsigning.password=…`, `--api-key=…`), an
/// `Authorization:` header, or a token by its known prefix. A comment counts: it's in the file all the same.
pub fn token_like(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    url_secret(line) || header_secret(&lower) || key_secret(&lower) || option_secret(&lower) || prefixed(line)
}

/// Whether variable `name` set to `value` looks like a token: `token_like` of `name=value`, a token's known
/// prefix looked for anywhere in the value but inside a path segment or a name — never in the variable's name
/// (`npm_config_store_dir` is `npm_` and 16 more: compat8, every `npm_config_*` that long was left out of runs, and
/// `/x/npm_config_store_dir` with it — review note 11; `-Dgpr.key=ghp_…`, `user:glpat-…` still are: verify5 N3).
pub fn var_token_like(name: &str, value: &str) -> bool {
    let line = format!("{name}={value}");
    let lower = line.to_ascii_lowercase();
    url_secret(&line)
        || header_secret(&lower)
        || key_secret(&lower)
        || option_secret(&lower)
        || prefixed_word(value)
}

/// A token by its known prefix in `s` where a word starts — not after a letter, a digit, `_`, `-`, `.` or `/`
/// (inside a name or a path segment) — with at least 16 characters after it.
fn prefixed_word(s: &str) -> bool {
    let inside = |c: char| c.is_ascii_alphanumeric() || "_-./".contains(c);
    PREFIXES.iter().any(|p| {
        s.match_indices(p).any(|(i, _)| {
            !s[..i].chars().next_back().is_some_and(inside) && token_tail(&s[i + p.len()..]) >= 16
        })
    })
}

/// How many characters a token's prefix is followed by.
fn token_tail(s: &str) -> usize {
    s.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').count()
}

/// An option among `lower`'s words that gives a secret: `-Dkey=value` (a JVM's system property), `-Pkey=value`
/// (Gradle's project property), `--key=value` — a key named for a secret, a value written there. Where options
/// are a variable's or a setting's value (`GRADLE_OPTS=-D…`, `org.gradle.jvmargs=-D…`) the word starts with it.
fn option_secret(lower: &str) -> bool {
    lower.split_whitespace().any(|word| {
        let word = word.trim_matches(['"', '\'']);
        // `NAME=-D…`, `NAME="-D…`: what's after the first `=`, when that's an option
        let word = match word.split_once('=') {
            Some((name, rest))
                if !name.starts_with('-') && rest.trim_start_matches(['"', '\'']).starts_with('-') =>
            {
                rest.trim_start_matches(['"', '\''])
            }
            _ => word,
        };
        let given =
            word.strip_prefix("--").or_else(|| word.strip_prefix("-d")).or_else(|| word.strip_prefix("-p"));
        given.is_some_and(|g| g.contains('=') && key_secret(g))
    })
}

/// A URL in `s` whose user part has a password, or is a token itself.
fn url_secret(s: &str) -> bool {
    s.match_indices("://").any(|(i, _)| {
        let rest = &s[i + 3..];
        let end = rest.find(|c: char| "/?#\"',;)<>".contains(c) || c.is_whitespace()).unwrap_or(rest.len());
        let Some((user, _)) = rest[..end].rsplit_once('@') else { return false };
        match user.split_once(':') {
            Some((_, pass)) => !pass.is_empty(),
            None => user.len() >= 20 || prefixed(user),
        }
    })
}

fn header_secret(lower: &str) -> bool {
    ["authorization:", "private-token:", "x-api-key:"].iter().any(|h| lower.contains(h))
        || lower
            .match_indices("bearer ")
            .any(|(i, _)| lower[i + 7..].trim_start().chars().take_while(|c| !c.is_whitespace()).count() >= 8)
}

/// `key = value`, `key: value`, `"key": "value"`, `export KEY=value` — a key named for a secret, a value
/// written there.
fn key_secret(lower: &str) -> bool {
    let t = lower.trim_start().trim_start_matches(['#', ';']).trim_start();
    let t = t.strip_prefix("export ").unwrap_or(t);
    let at = t.find('=').or_else(|| t.find(':').filter(|&i| !t[i..].starts_with("://")));
    let Some(at) = at else { return false };
    let key = t[..at].trim().trim_matches(['"', '\'']);
    let plain_key = !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || "_.-/:@".contains(c));
    let auth_word = || key.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| AUTH_WORDS.contains(&w));
    if !plain_key || !(SECRET_KEYS.iter().any(|k| key.contains(k)) || auth_word()) {
        return false;
    }
    if NOT_THE_VALUE.iter().any(|n| key.ends_with(n)) {
        return false;
    }
    let value = t[at + 1..].trim().trim_end_matches(',').trim().trim_matches(['"', '\'']);
    let placeholder = matches!(value, "true" | "false" | "null" | "none" | "yes" | "no" | "{" | "[");
    value.len() >= 4 && !value.starts_with('$') && !placeholder
}

/// Whether a variable's `name` alone names a secret (`GITHUB_TOKEN`, `DB_PASSWORD`, `NPM_CONFIG__AUTH`) —
/// `key_secret`'s rule on the name, whatever its value.
pub fn secret_name(name: &str) -> bool {
    key_secret(&format!("{}=value", name.to_ascii_lowercase()))
}

/// A token by its known prefix, with at least 16 characters after it.
fn prefixed(s: &str) -> bool {
    PREFIXES.iter().any(|p| s.match_indices(p).any(|(i, _)| token_tail(&s[i + p.len()..]) >= 16))
}

/// Headers whose value is a secret, lowercased (`header_secret`'s).
const HEADERS: [&str; 3] = ["authorization:", "private-token:", "x-api-key:"];

/// What a token's place says once it's taken out.
const GONE: &str = "…";

/// `text` with what looks like a token (`token_like`'s rules) taken out, a word at a time: the value given to a
/// key named for a secret (`FOO_TOKEN=…`, `--api-key=…`, `-Dsigning.password=…`), a URL's user part
/// (`https://…@host`), a token by its prefix, a header's value (`Authorization: Bearer …'`) to its quote's
/// end or the line's. A line `token_like` holds a token in that no word did (`password: hunter22`) is left out
/// whole. As good as `token_like` and no better: a heuristic — the card record's (`cards.rs`).
pub fn redacted(text: &str) -> String {
    let lines: Vec<String> = text.split('\n').map(redacted_line).collect();
    lines.join("\n")
}

fn redacted_line(line: &str) -> String {
    let mut changed = false;
    // (in a header's value: until a word that closes its quote)
    let mut header: Option<char> = None;
    let mut words = Vec::new();
    for word in line.split(' ') {
        let lower = word.to_ascii_lowercase();
        if let Some(q) = header {
            let scheme = matches!(lower.as_str(), "bearer" | "basic" | "token");
            let closes = q != ' ' && word.ends_with(q);
            if !scheme && !word.is_empty() {
                words.push(format!("{GONE}{}", if closes { q.to_string() } else { String::new() }));
                changed = true;
            } else {
                words.push(word.to_string());
            }
            if closes {
                header = None;
            }
            continue;
        }
        let bare = lower.trim_start_matches(['"', '\'']);
        if HEADERS.iter().any(|h| bare.starts_with(h)) {
            let q = word.chars().next().filter(|c| matches!(c, '"' | '\'')).unwrap_or(' ');
            // (`'Authorization:Bearer…'` in one word: its value goes with it)
            match bare.split_once(':').map(|(_, v)| v.trim_end_matches(['"', '\''])).filter(|v| !v.is_empty())
            {
                Some(_) => {
                    let name = &word[..word.find(':').map_or(word.len(), |i| i + 1)];
                    words.push(format!("{name}{GONE}"));
                    changed = true;
                }
                None => {
                    words.push(word.to_string());
                    header = Some(q);
                }
            }
            continue;
        }
        if !token_like(word) {
            words.push(word.to_string());
            continue;
        }
        changed = true;
        words.push(redacted_word(word));
    }
    let out = words.join(" ");
    // (what no word gave away — a key and its value apart, a header with nothing after it: the line goes)
    let header_name = |w: &str| {
        let lower = w.to_ascii_lowercase();
        HEADERS.iter().any(|h| lower.trim_start_matches(['"', '\'']).starts_with(h))
    };
    let left = out.split(' ').any(|w| token_like(w) && !header_name(w)) || (!changed && token_like(line));
    match left {
        true => format!("{GONE} (a line holding a token, left out)"),
        false => out,
    }
}

/// One word `token_like` holds a token in, without it: a URL's user part, a key's or an option's value, or
/// the word (a token by its prefix, or what can't be split) — its quotes kept.
fn redacted_word(word: &str) -> String {
    let quote = |s: &str| s.chars().rev().take_while(|c| matches!(c, '"' | '\'')).collect::<String>();
    if url_secret(word)
        && let Some(i) = word.find("://")
        && let Some(at) = word[i + 3..].find('@')
    {
        return format!("{}{GONE}{}", &word[..i + 3], &word[i + 3 + at..]);
    }
    match word.find('=') {
        Some(i) if !prefixed(&word[..i]) => format!("{}{GONE}{}", &word[..=i], quote(word)),
        _ => format!(
            "{}{GONE}{}",
            word.chars().take_while(|c| matches!(c, '"' | '\'')).collect::<String>(),
            quote(word)
        ),
    }
}

// ── Copies without them ─────────────────────────────────────────────────────

/// How a settings file is read, for leaving lines out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Form {
    /// git's: `[section]` headers (one holding a token — `[url "https://…@"]` — takes its section along), a
    /// line ending in `\` goes on in the next.
    Git,
    /// configparser's (pip): headers too, and an indented line goes on the one before.
    Ini,
    /// A line each (Go's env file, npm's).
    Lines,
}

/// `text` without its token-like lines as `form` reads it — what goes on a line dropped goes too, and a
/// section whose header holds one — and how many lines went. `fix` gives each kept line (with its section's
/// header) as it's written.
fn strip(text: &str, form: Form, mut fix: impl FnMut(&str, &str) -> String) -> (String, usize) {
    // Logical lines: git's `\` joins; an indented line goes on an ini's key
    let mut groups: Vec<Vec<&str>> = Vec::new();
    for line in text.lines() {
        let goes_on = match (form, groups.last()) {
            (Form::Git, Some(g)) => g.last().is_some_and(|l| l.trim_end().ends_with('\\')),
            (Form::Ini, Some(_)) => line.starts_with(char::is_whitespace) && !line.trim().is_empty(),
            _ => false,
        };
        match groups.last_mut() {
            Some(g) if goes_on => g.push(line),
            _ => groups.push(vec![line]),
        }
    }
    let (mut out, mut gone, mut section, mut dropping) = (String::new(), 0, String::new(), false);
    for g in groups {
        let header = form != Form::Lines && g[0].trim_start().starts_with('[');
        if header {
            section = g[0].trim().to_string();
            dropping = g.iter().any(|l| token_like(l));
        }
        let whole = dropping
            || match form {
                // (a key holding one takes what goes on it; a line going on holding one goes alone)
                Form::Ini => token_like(g[0]),
                _ => g.iter().any(|l| token_like(l)),
            };
        for l in &g {
            if whole || token_like(l) {
                gone += 1;
                continue;
            }
            out.push_str(&fix(&section, l));
            out.push('\n');
        }
    }
    (out, gone)
}

/// A tool whose user settings may hold a token, and the variable that points it at a copy instead — git
/// (tarae's own), and the packs' `[[sandbox.settings]]` that take a copy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tool {
    pub name: String,
    pub var: String,
    form: Form,
    /// The copy's name in the run's folder.
    file: String,
    /// Keys the copy leaves out besides token-like lines (a setting's `out-keys`).
    out_keys: Vec<String>,
}

/// git's: `GIT_CONFIG_GLOBAL`, else `$XDG_CONFIG_HOME/git/config`, then `~/.gitconfig`.
fn git() -> Tool {
    let (name, var, file) = ("git".into(), "GIT_CONFIG_GLOBAL".into(), "gitconfig".into());
    Tool { name, var, form: Form::Git, file, out_keys: Vec::new() }
}

/// Where each tool reads its user settings, in the order it reads them (the later wins), as it finds them
/// from `home` and the environment (`var`) on `os` — a variable naming its file wins (git's, a pack's `var`
/// or `var-also`; its `off` value: none; with `var-if-there`, only when that file is there), else its
/// `files` (`{ first, file }`: in the first folder that's there, else the last).
pub fn sources_on(home: &Path, var: &dyn Fn(&str) -> Option<String>, os: Os) -> Vec<(Tool, Vec<PathBuf>)> {
    let set = |name: &str| var(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    let t = crate::sandboxpack::get();
    let vars = crate::sandboxpack::Vars::new(t, home, &set, os);
    let git_files = match set("GIT_CONFIG_GLOBAL") {
        Some(f) => vec![f],
        None => vec![vars.xdg_config.join("git/config"), home.join(".gitconfig")],
    };
    let mut out = vec![(git(), git_files)];
    for s in &t.settings {
        let (Some(name), Some(file)) = (&s.var, &s.copy) else { continue };
        let form = if s.form == "ini" { Form::Ini } else { Form::Lines };
        let (out_keys, file) = (s.out_keys.clone(), file.clone());
        let tool = Tool { name: s.tool.clone(), var: name.clone(), form, file, out_keys };
        let named = std::iter::once(name).chain(&s.var_also).find_map(|v| set(v));
        let named = named.filter(|f| !s.var_if_there || f.exists());
        let files = match named {
            Some(f) if s.off.as_deref().is_some_and(|off| f == Path::new(off)) => vec![],
            Some(f) => vec![f],
            None => s.files.iter().filter_map(|f| setting_file(f, &vars)).collect(),
        };
        out.push((tool, files));
    }
    out
}

/// `sources_on` this system.
#[cfg(test)]
pub fn sources(home: &Path, var: &dyn Fn(&str) -> Option<String>) -> Vec<(Tool, Vec<PathBuf>)> {
    sources_on(home, var, Os::HOST)
}

/// Where one of a setting's files is (`{ first, file }`: in the first folder that's there, else the last).
fn setting_file(f: &crate::sandboxpack::SettingFile, vars: &crate::sandboxpack::Vars) -> Option<PathBuf> {
    use crate::sandboxpack::SettingFile;
    match f {
        SettingFile::Place(p) => vars.place(p),
        SettingFile::First { dirs, file } => {
            let dirs: Vec<PathBuf> = dirs.iter().filter_map(|d| vars.place(d)).collect();
            Some(dirs.iter().find(|d| d.is_dir()).or(dirs.last())?.join(file))
        }
    }
}

/// What a run gets in place of the settings that hold a token.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Copies {
    /// Each tool's variable and its copy, for the run's environment.
    pub env: Vec<(String, PathBuf)>,
    /// The files that hold a token-like line (real paths) and the variable whose copy stands in: runs don't
    /// read them — but one the user let Claude read (`copies`' `granted`).
    pub hidden: Vec<(PathBuf, String)>,
    /// The files whose token-like lines a copy left out (hidden or not: npm's is kept anyway, one the user
    /// let Claude read isn't), and the copy's variable.
    pub left_out: Vec<(PathBuf, String)>,
    /// The settings of tools that take no copy that hold one, left as they are (`told_holders` — cargo's), and
    /// what the user is told of each (its pack's `tell`).
    pub told: Vec<(PathBuf, String)>,
    /// Each tool's variable a copy is for — made or not (no folder to make it in: `:sandbox-env`'s page).
    pub for_vars: Vec<String>,
}

impl Copies {
    /// The files whose token lines a run's copies leave out, as the result says them (`~/.gitconfig`), and
    /// the variables.
    pub fn said(&self) -> Option<(String, String)> {
        if self.left_out.is_empty() {
            return None;
        }
        let files: Vec<String> = self.left_out.iter().map(|(f, _)| crate::allow::tilde(f)).collect();
        let mut vars: Vec<&str> = self.left_out.iter().map(|(_, v)| v.as_str()).collect();
        vars.dedup();
        Some((files.join(", "), vars.join(", ")))
    }

    /// The variable whose copy stands in for `real`, when it's a file kept for its token.
    pub fn instead_of(&self, real: &Path) -> Option<&str> {
        self.hidden.iter().find(|(f, _)| f == real).map(|(_, v)| v.as_str())
    }
}

/// Making one tool's copy: where the copies go, which files held a token, how many copies were made, the keys
/// left out besides (`Tool.out_keys`) and how many lines they took.
struct Making<'a> {
    dir: Option<&'a Path>,
    home: &'a Path,
    held: Vec<PathBuf>,
    made: usize,
    out_keys: &'a [String],
    keys_gone: usize,
}

/// The deepest include followed (git's own limit is 10).
const INCLUDES_MAX: usize = 8;

impl Making<'_> {
    /// A settings file's text without its token-like lines — and git's includes made whole (`git_line`).
    /// None: not there, or not readable. (Read as the tools read it: a byte that isn't UTF-8 is no reason to pass
    /// a file holding a token as it is — verify4 F5.)
    fn file(&mut self, src: &Path, form: Form, depth: usize) -> Option<String> {
        let mut text = String::from_utf8_lossy(&std::fs::read(src).ok()?).into_owned();
        // (a key left out besides: `key = value` lines, any case, `_` as `-` — npm's own proxy settings)
        if !self.out_keys.is_empty() {
            let fold = |k: &str| k.trim().trim_matches(['"', '\'']).to_ascii_lowercase().replace('_', "-");
            let out: Vec<String> = self.out_keys.iter().map(|k| fold(k)).collect();
            let lines = text.lines().filter(|l| {
                let key = l.split_once('=').map(|(k, _)| fold(k));
                let gone = key.is_some_and(|k| out.contains(&k));
                self.keys_gone += usize::from(gone);
                !gone
            });
            text = lines.map(|l| format!("{l}\n")).collect();
        }
        let base = src.parent().unwrap_or(Path::new("/")).to_path_buf();
        let (out, gone) = match form {
            Form::Git => strip(&text, form, |section, line| self.git_line(section, line, &base, depth)),
            _ => strip(&text, form, |_, line| line.to_string()),
        };
        if gone > 0 && !self.held.iter().any(|h| h == src) {
            self.held.push(src.to_path_buf());
        }
        Some(out)
    }

    /// A kept line of git's settings: an include's path made whole from the folder of the file it's in (git's
    /// rule — the copy is elsewhere), an `includeIf "gitdir:./…"` too; an included file holding a token copied
    /// the same way and included from the copy.
    fn git_line(&mut self, section: &str, line: &str, base: &Path, depth: usize) -> String {
        let lower = section.to_ascii_lowercase();
        if line.trim() == section {
            if lower.starts_with("[includeif") {
                let at = format!("{}/", base.display());
                return line
                    .replace("gitdir:./", &format!("gitdir:{at}"))
                    .replace("gitdir/i:./", &format!("gitdir/i:{at}"));
            }
            return line.to_string();
        }
        let included = lower.starts_with("[include]") || lower.starts_with("[includeif");
        let Some((key, value)) =
            line.split_once('=').filter(|(k, _)| included && k.trim().eq_ignore_ascii_case("path"))
        else {
            return line.to_string();
        };
        let value = value.trim().trim_matches('"');
        let path = match value.strip_prefix("~/") {
            Some(rest) => self.home.join(rest),
            None if value.starts_with('/') => PathBuf::from(value),
            None => base.join(value),
        };
        let real = crate::sandbox::profile_path(&path);
        let mut to = path;
        if depth < INCLUDES_MAX && real.is_file() {
            let before = self.held.len();
            if let Some(text) = self.file(&real, Form::Git, depth + 1)
                && self.held.len() > before
                && let Some(dir) = self.dir
            {
                self.made += 1;
                let copy = dir.join(format!("gitconfig-include-{}", self.made));
                if write_private(&copy, &copy_head(&real, &text)).is_ok() {
                    to = copy;
                }
            }
        }
        let indent: String = key.chars().take_while(|c| c.is_whitespace()).collect();
        format!("{indent}path = \"{}\"", to.display().to_string().replace('\\', "\\\\").replace('"', "\\\""))
    }
}

/// A copy's first line: what it's a copy of (a comment in every form here — Go skips lines not starting with
/// a capital).
fn copy_head(of: &Path, text: &str) -> String {
    format!("# tarae: {} without its token-like lines — what Claude's commands read\n{text}", of.display())
}

/// configparser's files merged as pip reads them one after another: a section's keys gathered, a key read
/// later (`index_url` = `index-url`) over the one before.
fn ini_merge(texts: &[String]) -> String {
    type Entries = Vec<(String, Vec<String>)>;
    let mut sections: Vec<(String, Entries)> = Vec::new();
    for t in texts {
        let (mut cur, mut key): (Option<usize>, Option<usize>) = (None, None);
        for line in t.lines() {
            let s = line.trim();
            if s.is_empty() || s.starts_with(['#', ';']) {
                continue;
            }
            if line.starts_with(char::is_whitespace) {
                if let (Some(c), Some(k)) = (cur, key) {
                    sections[c].1[k].1.push(line.to_string());
                }
                continue;
            }
            if let Some(name) = s.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                let name = name.trim().to_string();
                cur = Some(sections.iter().position(|(n, _)| *n == name).unwrap_or_else(|| {
                    sections.push((name, Vec::new()));
                    sections.len() - 1
                }));
                key = None;
                continue;
            }
            let Some(c) = cur else { continue };
            let name =
                s.split(['=', ':']).next().unwrap_or_default().trim().to_ascii_lowercase().replace('_', "-");
            let entries = &mut sections[c].1;
            let i = entries.iter().position(|(k, _)| *k == name).unwrap_or_else(|| {
                entries.push((name, Vec::new()));
                entries.len() - 1
            });
            entries[i].1 = vec![line.to_string()];
            key = Some(i);
        }
    }
    let mut out = String::new();
    for (name, entries) in sections {
        out.push_str(&format!("[{name}]\n"));
        for line in entries.into_iter().flat_map(|(_, l)| l) {
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

/// Written for this user only (its folder made when it's the first).
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
    f.write_all(text.as_bytes())
}

/// The copies for a run whose own folder is `dir` (None: none could be made — the files that hold a token
/// stay closed all the same), from each tool's `sources`: a tool whose files hold a token-like line — or one
/// of which is kept from every run (`kept`: npm's) — gets one copy of them all, in the order it reads them,
/// without those lines. A file that holds one is hidden from the run — unless the user let Claude read it
/// (`granted`; their card said what it holds), or it's kept already. Real paths in.
pub fn copies(
    sources: &[(Tool, Vec<PathBuf>)],
    dir: Option<&Path>,
    home: &Path,
    granted: &[PathBuf],
    kept: &[PathBuf],
) -> Copies {
    let mut out = Copies::default();
    let folder = dir.map(|d| d.join("settings"));
    for (tool, files) in sources {
        let real: Vec<PathBuf> =
            files.iter().filter(|f| f.is_file()).map(|f| crate::sandbox::profile_path(f)).collect();
        let mut m = Making {
            dir: folder.as_deref(),
            home,
            held: Vec::new(),
            made: 0,
            out_keys: &tool.out_keys,
            keys_gone: 0,
        };
        let texts: Vec<(PathBuf, String)> =
            real.iter().filter_map(|f| Some((f.clone(), m.file(f, tool.form, 0)?))).collect();
        let is_kept = |f: &Path| kept.iter().any(|k| f.starts_with(k));
        if m.held.is_empty() && m.keys_gone == 0 && !real.iter().any(|f| is_kept(f)) {
            continue;
        }
        let text = match tool.form {
            Form::Ini if texts.len() > 1 => {
                ini_merge(&texts.iter().map(|(_, t)| t.clone()).collect::<Vec<_>>())
            }
            _ => texts.iter().map(|(f, t)| copy_head(f, t)).collect::<Vec<_>>().join("\n"),
        };
        out.for_vars.push(tool.var.clone());
        let head = match (tool.form, texts.len()) {
            (Form::Ini, 2..) => format!("# tarae: {} merged, without their token-like lines\n", texts.len()),
            _ => String::new(),
        };
        if let Some(f) = &folder {
            let copy = f.join(&tool.file);
            if write_private(&copy, &format!("{head}{text}")).is_ok() {
                out.env.push((tool.var.clone(), copy));
                out.left_out.extend(m.held.iter().map(|h| (h.clone(), tool.var.clone())));
            }
        }
        for h in m.held {
            if !is_kept(&h) && !granted.iter().any(|g| h.starts_with(g)) {
                out.hidden.push((h, tool.var.clone()));
            }
        }
    }
    out
}

/// The settings a run reads of the tools that take no copy (the packs' settings with `tell` — cargo reads
/// its config as an ancestor's) that hold a token-like line: their `files` (`vars`: their places), and those
/// above the project (`sandbox::reads_above`) for one that reads them there (`above`). Real paths.
pub fn told_holders(vars: &crate::sandboxpack::Vars, above: &[PathBuf]) -> Vec<(PathBuf, String)> {
    let t = crate::sandboxpack::get();
    let mut files: Vec<(PathBuf, String)> = Vec::new();
    for s in &t.settings {
        let Some(tell) = &s.tell else { continue };
        let theirs = s.files.iter().filter_map(|f| setting_file(f, vars));
        files.extend(theirs.chain(above.iter().filter(|_| s.above).cloned()).map(|f| (f, tell.clone())));
    }
    files
        .into_iter()
        .filter(|(f, _)| std::fs::read_to_string(f).is_ok_and(|t| t.lines().any(token_like)))
        .map(|(f, tell)| (crate::sandbox::profile_path(&f), tell))
        .collect()
}

// ── Saying so ───────────────────────────────────────────────────────────────

/// The settings already told about (`told_holders`; `$state/tokens-said`, a path a line): each is said once,
/// and said again only once it held none for a while (it's taken off when a run finds it without one).
pub fn said_path() -> Option<PathBuf> {
    crate::config::state_dir().map(|d| d.join("tokens-said"))
}

fn said_read(path: &Path) -> Vec<PathBuf> {
    std::fs::read_to_string(path)
        .map(|s| s.lines().filter(|l| !l.is_empty()).map(PathBuf::from).collect())
        .unwrap_or_default()
}

/// What to tell about the settings holding a token (`told_holders`, on the policy's worker): those not told
/// before — and the list of told ones as it should now be (None: unchanged).
pub fn to_tell(
    holders: &[(PathBuf, String)],
    said: Option<&Path>,
) -> (Vec<(PathBuf, String)>, Option<Vec<PathBuf>>) {
    let told = said.map(said_read).unwrap_or_default();
    let new: Vec<(PathBuf, String)> = holders.iter().filter(|(h, _)| !told.contains(h)).cloned().collect();
    // (a file told before that holds none now is taken off: a token put back is said again)
    let holds = |t: &PathBuf| holders.iter().any(|(h, _)| h == t);
    let now: Vec<PathBuf> =
        told.iter().filter(|t| holds(t)).cloned().chain(new.iter().map(|(h, _)| h.clone())).collect();
    (new, (now != told).then_some(now))
}

/// Tells the user, once, that these settings (cargo's) hold a token runs read — Claude's commands can't do
/// without them — and what to do, in each one's pack's words (`{them}`). A toast, not the run's report: it's
/// the user who can move a token (Claude can't, and its report would point it at the file); and not a card:
/// there's nothing to answer. `told`: the list to keep (`to_tell`), written on a worker.
pub fn tell_holders(ed: &mut Editor, holders: &[(PathBuf, String)], told: Option<Vec<PathBuf>>) {
    if let (Some(list), Some(path)) = (told, ed.tokens_said.clone()) {
        ed.events.jobs().background(move || {
            let body: String = list.iter().map(|p| format!("{}\n", p.display())).collect();
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = crate::disk::write_atomic(&path, |w| std::io::Write::write_all(w, body.as_bytes()));
        });
    }
    let files: Vec<String> = holders.iter().map(|(f, _)| crate::allow::tilde(f)).collect();
    if files.is_empty() || ed.tokens_told.iter().any(|t| files.contains(t)) {
        return;
    }
    ed.tokens_told.extend(files.iter().cloned());
    let (them, it) = if files.len() == 1 { ("it", "holds a token") } else { ("them", "hold tokens") };
    let mut tells: Vec<String> = Vec::new();
    for (_, tell) in holders {
        let tell = tell.replace("{them}", them);
        if !tells.contains(&tell) {
            tells.push(tell);
        }
    }
    ed.set_warning(format!(
        "{} {it}, and Claude's commands read {them} — {}",
        files.join(", "),
        tells.join(" ")
    ));
}

// ── On a card ───────────────────────────────────────────────────────────────

/// What a place holds that looks like a token (`found_in`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Found {
    /// The files that hold a token-like line — `FOUND_MAX` at most.
    pub files: Vec<PathBuf>,
    /// How many more do.
    pub more: usize,
    /// Not every file was looked at (more than `LOOK_MAX`, `BYTES_MAX` read, `VISIT_MAX` entries met) — or
    /// none: a place that's everything isn't walked.
    pub cut: bool,
    /// The files named like keys met on the way (`secretnames`, below tools' output not walked) — `FOUND_MAX` at
    /// most: a repository holding one is never offered whole (`reads::folder_for`, review J 5).
    pub named: Vec<PathBuf>,
}

const FOUND_MAX: usize = 3;
const LOOK_MAX: usize = 2000;
/// Entries a walk visits at most, read or not (folders, code): `/` in a Linux container, 44k entries, took
/// ~1 s in a debug build — a quarter of that holds no card up even on a busy machine.
const VISIT_MAX: usize = 10_000;
const BYTES_MAX: u64 = 32 << 20;
/// Bigger files aren't settings.
const FILE_MAX: u64 = 256 << 10;
/// Pictures, documents and lock files — no language's: never settings.
const NOT_SETTINGS_FLOOR: [&str; 6] = ["svg", "png", "jpg", "pdf", "lock", "sum"];

/// Whether a file of extension `ext` (lowercase) is code, docs or a picture: a `token = …` there is code, not a
/// token — the built-in packs' `not-settings` (their code's extensions) and `NOT_SETTINGS_FLOOR`.
fn not_settings(ext: &str) -> bool {
    NOT_SETTINGS_FLOOR.contains(&ext) || crate::sandboxpack::get().not_settings.iter().any(|x| x == ext)
}
/// A folder of tools' output, never looked in (below the place asked about): git's `objects`, and the
/// packs' `outputs` — the built-in packs' only (`packs::Reach::BuiltIn`): one of one's own would hide a token.
pub(crate) fn output_dir(name: &str) -> bool {
    name == "objects" || crate::languages::get().outputs.iter().any(|o| o == name)
}

/// Whether `dir` is on one of the kernel's own file systems (statfs's `f_type`): what's there is the kernel's,
/// never a user's token file, and walking it is slow and racy (`/proc` changes as processes come and go).
/// devtmpfs isn't told apart — it says tmpfs's type, as `/tmp` does — but `/dev`'s nodes aren't files.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn the_kernels(dir: &Path) -> bool {
    const KERNELS: [u32; 15] = [
        0x9FA0,      // proc
        0x6265_6572, // sysfs
        0x1CD1,      // devpts
        0x1980_0202, // mqueue
        0x0027_E0EB, // cgroup
        0x6367_7270, // cgroup2
        0x6462_6720, // debugfs
        0x7472_6163, // tracefs
        0x7363_6673, // securityfs
        0xCAFE_4A11, // bpf
        0x6165_676C, // pstore
        0x6265_6570, // configfs
        0xDE5E_81E4, // efivarfs
        0x4249_4E4D, // binfmt_misc
        0x6573_5543, // fusectl
    ];
    crate::fswatch::statfs(dir).is_some_and(|st| KERNELS.contains(&(st.f_type as u32)))
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn the_kernels(_dir: &Path) -> bool {
    false
}

/// The files in `place` (a file, or a folder and everything in it — links not followed) that hold a
/// token-like line: settings-like files only, within bounds (`Found::cut` says when they were met), each read
/// no further than its size says, the kernel's file systems not looked in (`the_kernels`); `/`, home and the
/// folders above it not walked at all (cut). Blocks: a worker's.
pub fn found_in(place: &Path) -> Found {
    found_under(place, crate::allow::home().as_deref())
}

/// `found_in`, home being `home`.
fn found_under(place: &Path, home: Option<&Path>) -> Found {
    use std::io::Read as _;
    use std::os::unix::fs::MetadataExt as _;
    let mut f = Found::default();
    // (a place allowed only as a whole — `/`, home or above, `allow::keepable`'s — isn't walked: too big to
    // say anything of use, and its card would wait on it)
    if place == Path::new("/") || home.is_some_and(|h| h.starts_with(place)) {
        f.cut = true;
        return f;
    }
    let (mut looked, mut bytes, mut visited) = (0, 0, 0);
    // (whether each device is the kernel's: asked once a device, not once a folder)
    let mut kernels = std::collections::HashMap::new();
    let mut stack = vec![place.to_path_buf()];
    while let Some(p) = stack.pop() {
        if visited >= VISIT_MAX {
            f.cut = true;
            break;
        }
        visited += 1;
        let Ok(meta) = std::fs::symlink_metadata(&p) else { continue };
        if meta.is_dir() {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            if p != place && output_dir(name) {
                continue;
            }
            if *kernels.entry(meta.dev()).or_insert_with(|| the_kernels(&p)) {
                continue;
            }
            let Ok(rd) = std::fs::read_dir(&p) else { continue };
            stack.extend(rd.flatten().map(|e| e.path()));
            continue;
        }
        // (by its name, whatever its size or kind of content)
        if meta.is_file() && crate::secretnames::named_secret(&p) && f.named.len() < FOUND_MAX {
            f.named.push(p.clone());
        }
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
        if !meta.is_file() || meta.len() > FILE_MAX || (p != place && not_settings(&ext)) {
            continue;
        }
        if looked >= LOOK_MAX || bytes + meta.len() > BYTES_MAX {
            f.cut = true;
            break;
        }
        looked += 1;
        bytes += meta.len();
        // (no further than its size says: a file of the kernel's can say 0 and be endless —
        // `/proc/<pid>/pagemap`, 8 bytes a page of the address space)
        let mut data = Vec::new();
        let Ok(file) = std::fs::File::open(&p) else { continue };
        if file.take(meta.len()).read_to_end(&mut data).is_err() {
            continue;
        }
        if data.contains(&0) {
            continue;
        }
        if String::from_utf8_lossy(&data).lines().any(token_like) {
            match f.files.len() < FOUND_MAX {
                true => f.files.push(p),
                false => f.more += 1,
            }
        }
    }
    crate::sorting::sort(&mut f.files);
    f
}

impl Found {
    /// What a card says of `place` before it's opened (None: nothing to say) — the files' names, never what's
    /// in them.
    pub fn said(&self, place: &Path) -> Option<String> {
        let at = crate::allow::tilde(place);
        let name = |f: &PathBuf| match f.strip_prefix(place) {
            Ok(r) if !r.as_os_str().is_empty() => r.display().to_string(),
            _ => crate::allow::tilde(f),
        };
        let mut s = match self.files.as_slice() {
            [] => String::new(),
            [one] if one == place => format!("{at} holds a token-like line."),
            [one] => format!("In {at}, {} holds a token-like line.", name(one)),
            many => {
                let mut names: Vec<String> = many.iter().map(name).collect();
                let last = match self.more {
                    0 => names.pop().unwrap_or_default(),
                    n => format!("{n} more"),
                };
                format!("In {at}, {} and {last} hold token-like lines.", names.join(", "))
            }
        };
        if self.cut {
            s.push_str(if s.is_empty() { "" } else { " " });
            s.push_str(&format!("{at} has more files than tarae looks through for tokens."));
        }
        (!s.is_empty()).then_some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// f22 f: the files never looked in for a token are the code packs' (`[sandbox] not-settings`) — each one of
    /// its own pack's languages' extensions — and the core's floor, the 30 the core listed before; settings stay
    /// looked in (`kts`: a Gradle script's credentials; toml, json, yaml, env, sh).
    #[test]
    fn files_not_settings_are_the_code_packs_and_a_floor() {
        let mut all: Vec<&str> = NOT_SETTINGS_FLOOR.to_vec();
        all.extend(crate::sandboxpack::get().not_settings.iter().map(String::as_str));
        crate::sorting::sort(&mut all);
        let mut before = [
            "rs", "py", "js", "mjs", "ts", "tsx", "jsx", "go", "java", "kt", "c", "h", "cc", "cpp", "hpp",
            "swift", "rb", "php", "cs", "scala", "lua", "md", "html", "css", "svg", "png", "jpg", "pdf",
            "lock", "sum",
        ];
        crate::sorting::sort(&mut before);
        assert_eq!(all, before);
        for x in ["kts", "toml", "json", "yaml", "env", "sh", ""] {
            assert!(!not_settings(x), "{x}");
        }
        // (each pack's its own languages')
        for (pack, t) in crate::packs::builtin_manifests() {
            let Ok(t) = t else { continue };
            let Some(said) = t.get("sandbox").and_then(|s| s.get("not-settings")).and_then(|v| v.as_array())
            else {
                continue;
            };
            let langs = t.get("language").and_then(|l| l.as_table());
            let exts: Vec<&str> = langs
                .into_iter()
                .flat_map(|l| l.values())
                .filter_map(|l| l.get("extensions").and_then(|e| e.as_array()))
                .flatten()
                .filter_map(|e| e.as_str())
                .collect();
            for x in said.iter().filter_map(|x| x.as_str()) {
                assert!(exts.contains(&x), "the {pack} pack's `{x}` isn't one of its languages': {exts:?}");
            }
        }
    }

    fn scratch(name: &str) -> PathBuf {
        crate::scratch::Scratch::new(&format!("tokens-{name}")).into_path()
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    const FAKE: &str = "tarae-fake-token";

    #[test]
    fn what_looks_like_a_token() {
        let yes = [
            "\textraHeader = Authorization: Bearer tarae-fake-token",
            "[url \"https://x-access-token:tarae-fake-token@github.com/\"]",
            "GOPROXY=https://user:tarae-fake-token@proxy.example.com,direct",
            "//registry.example.com/:_authToken=tarae-fake-token",
            "_auth=dGFyYWUtZmFrZQ==",
            "index-url = https://u:tarae-fake-token@pypi.example.com/simple",
            "token = \"tarae-fake-token\"",
            "  \"password\": \"tarae-fake-token\",",
            "export GITHUB_TOKEN=tarae-fake-token",
            "# token = tarae-fake-token",
            "url = https://ghp_0123456789abcdefABCDEF@github.com/o/r",
            "remote = ghp_0123456789abcdefABCDEF",
            "Private-Token: tarae-fake-token",
            // yarn's npmAuthIdent (`user:password`), as a variable and in .yarnrc.yml; npm's `_auth` as one
            "YARN_NPM_AUTH_IDENT=stand-in:tarae-fake-token",
            "npmAuthIdent: \"stand-in:tarae-fake-token\"",
            "NPM_CONFIG__AUTH=dGFyYWUtZmFrZQ==",
            "BASIC_AUTH=stand-in:tarae-fake-token",
            // Options giving one, in a variable's value or a setting's
            "JAVA_OPTS=-Xmx2g -Dorg.gradle.project.repoPassword=tarae-fake-token",
            "MAVEN_OPTS=-Dpassword=tarae-fake-token -B",
            "export GRADLE_OPTS=\"-Dorg.gradle.project.signingPassword=tarae-fake-token\"",
            "GRADLE_OPTS=-Porg.gradle.project.signing.secretKey=tarae-fake-token",
            "org.gradle.jvmargs=-Xmx2g -Dhttps.proxyPassword=tarae-fake-token",
            "NODE_OPTIONS=--api-key=tarae-fake-token",
        ];
        for l in yes {
            assert!(token_like(l), "not seen: {l}");
        }
        let no = [
            "[url \"git@github.com:\"]",
            "\tinsteadOf = https://github.com/",
            "url = ssh://git@github.com:22/o/r",
            "always-auth=true",
            "registry=https://registry.example.com/",
            "//registry.example.com/:_authToken=${NPM_TOKEN}",
            "\thelper = osxkeychain",
            "password_file = ~/.secret",
            "token-cmd = pass show npm",
            "let token = parse(input);",
            "GOFLAGS=-mod=mod",
            "[registry]",
            "token =",
            "Authorization header goes here", // (no colon: prose)
            // `auth` as a word, not inside one; options that give no secret, or name where it is
            "GIT_AUTHOR_NAME=Stand In",
            "GIT_AUTHOR_EMAIL=stand.in@example.com",
            "auth-type=legacy",
            "JAVA_OPTS=-Xmx2g -Dfile.encoding=UTF-8 -Dorg.gradle.daemon=false",
            "GRADLE_OPTS=-Dsigning.password.file=/x/key.txt",
            "MAVEN_OPTS=-Dpassword=${REPO_PASSWORD}",
            "RUSTFLAGS=-Dwarnings -Ctarget-cpu=native",
            "GOFLAGS=-tags=password",
            "git log --author=Stand",
        ];
        for l in no {
            assert!(!token_like(l), "seen: {l}");
        }
    }

    /// A variable's known token prefix is looked for in its value, never its name: `npm_config_store_dir` is
    /// npm's prefix and 16 more, and was left out of runs as a token (compat8); one holding a token still is.
    #[test]
    fn a_variables_name_is_no_token() {
        for name in ["npm_config_store_dir", "pnpm_config_store_dir", "npm_config_userconfig"] {
            assert!(!var_token_like(name, "/workspace/.pnpm-store"), "{name}");
            // (a path naming one: no token — review note 11)
            assert!(!var_token_like(name, &format!("/x/{name}")), "{name} in a path");
        }
        let fake = "npm_0123456789abcdefABCDEFghij";
        assert!(var_token_like("NPM_REGISTRY_KEY_VALUE", fake), "a token in the value");
        assert!(var_token_like("NPM_REGISTRY_KEY_VALUE", &format!("'{fake}'")), "quoted");
        // (one past the value's start, a word of its own: still a token — review note 3, verify5 N3, N6)
        // (in two parts: a 20-character `glpat-` literal reads as a real token to GitHub's push protection)
        let lab = concat!("glpat-", "0123456789abcdefABCD");
        for (name, value) in [
            ("SOME_USER", format!("user:{lab}")),
            ("SOME_LIST", format!("a,{fake}")),
            ("SOME_WORDS", format!("x {fake}")),
            ("JAVA_OPTS", format!("-Dgpr.key={fake}")),
            ("GOFLAGS", format!("-ldflags=-X=main.ghToken={fake}")),
        ] {
            assert!(var_token_like(name, &value), "{name}={value}");
        }
        // (inside a name or a path segment: none)
        for value in ["/x/npm_config_store_dir", "a.npm_config_store_dir_more", "x-npm_config_store_dir"] {
            assert!(!var_token_like("SOME_PATH", value), "{value}");
        }
        assert!(var_token_like("GITHUB_TOKEN", "x1234"), "a name for one");
        assert!(var_token_like("npm_config__authToken", "x1234"), "npm's own");
    }

    /// git's form: a header holding one drops its section; a key holding one drops itself and what goes on
    /// it (`\`); the rest stays, comments and all.
    #[test]
    fn a_git_copy_leaves_out_the_token_lines() {
        let text = "[user]\n\tname = Stand In\n[http]\n\textraHeader = Authorization: \\\n\t\tBearer tarae-fake-token\n\
                    \tsslVerify = true\n[url \"https://x:tarae-fake-token@github.com/\"]\n\tinsteadOf = https://github.com/\n\
                    [alias]\n\tco = checkout\n";
        let (out, gone) = strip(text, Form::Git, |_, l| l.to_string());
        assert_eq!(out, "[user]\n\tname = Stand In\n[http]\n\tsslVerify = true\n[alias]\n\tco = checkout\n");
        assert_eq!(gone, 4);
        assert!(!out.contains(FAKE));
    }

    /// configparser's form: an indented line goes on its key — a key holding one goes with them, a line going
    /// on holding one goes alone.
    #[test]
    fn an_ini_copy_keeps_what_goes_on_a_key() {
        let text = "[global]\nindex-url = https://u:tarae-fake-token@p.example.com/simple\n  https://more.example.com\n\
                    extra-index-url =\n  https://a.example.com/simple\n  https://u:tarae-fake-token@b.example.com/simple\n\
                    timeout = 7\n";
        let (out, gone) = strip(text, Form::Ini, |_, l| l.to_string());
        assert_eq!(out, "[global]\nextra-index-url =\n  https://a.example.com/simple\ntimeout = 7\n");
        assert_eq!(gone, 3);
        // Lines: a line each, headers are lines
        let (out, _) =
            strip("[x]\n_auth=dGFyYWUtZmFrZQ==\nsave-exact=true\n", Form::Lines, |_, l| l.to_string());
        assert_eq!(out, "[x]\nsave-exact=true\n");
    }

    /// pip reads its files one after another: one copy, a key read later over the one before (`_` = `-`).
    #[test]
    fn pips_files_merge_in_its_order() {
        let a =
            "[global]\ntimeout = 7\nindex_url = https://a.example.com\n[install]\nuser = true\n".to_string();
        let b = "[global]\nindex-url = https://b.example.com\n  https://c.example.com\n".to_string();
        assert_eq!(
            ini_merge(&[a, b]),
            "[global]\ntimeout = 7\nindex-url = https://b.example.com\n  https://c.example.com\n[install]\nuser = true\n"
        );
    }

    /// Every tool's file as it's found: the variable naming it wins, `GOENV=off` is none, pip's folder on
    /// macOS only when it's there.
    #[test]
    fn where_each_tool_reads_its_settings() {
        let home = scratch("sources");
        let none = |_: &str| None;
        let all = sources(&home, &none);
        let get =
            |all: &[(Tool, Vec<PathBuf>)], t: &str| all.iter().find(|(x, _)| x.name == t).unwrap().1.clone();
        assert_eq!(get(&all, "git"), [home.join(".config/git/config"), home.join(".gitconfig")]);
        assert_eq!(get(&all, "npm"), [home.join(".npmrc")]);
        assert_eq!(get(&all, "pip"), [home.join(".pip/pip.conf"), home.join(".config/pip/pip.conf")]);
        if cfg!(target_os = "macos") {
            assert_eq!(get(&all, "go"), [home.join("Library/Application Support/go/env")]);
            std::fs::create_dir_all(home.join("Library/Application Support/pip")).unwrap();
            let pip = get(&sources(&home, &none), "pip");
            assert_eq!(pip[1], home.join("Library/Application Support/pip/pip.conf"));
        }
        let given = home.join("given.conf");
        write(&given, "[global]\n");
        let set = |n: &str| match n {
            "GIT_CONFIG_GLOBAL" => Some("/x/gitconfig".to_string()),
            "GOENV" => Some("off".to_string()),
            "PIP_CONFIG_FILE" => Some(given.display().to_string()),
            "XDG_CONFIG_HOME" => Some("/xdg".to_string()),
            _ => None,
        };
        let all = sources(&home, &set);
        assert_eq!(get(&all, "git"), [PathBuf::from("/x/gitconfig")]);
        assert!(get(&all, "go").is_empty());
        assert_eq!(get(&all, "pip"), std::slice::from_ref(&given));
        // (npm's `userconfig`, read in any case — `Here::var` folds it: compat8)
        let by_var =
            |all: &[(Tool, Vec<PathBuf>)], v: &str| all.iter().find(|(x, _)| x.var == v).unwrap().1.clone();
        for user in ["npm_config_userconfig", "NPM_CONFIG_USERCONFIG", "Npm_Config_UserConfig"] {
            let here = crate::sandbox::Here {
                env: [(user.to_string(), "/x/npmrc".into())].into(),
                ..Default::default()
            };
            let all = sources(&home, &|n| here.var(n));
            assert_eq!(by_var(&all, "NPM_CONFIG_USERCONFIG"), [PathBuf::from("/x/npmrc")], "{user}");
        }
        // (`PIP_CONFIG_FILE` naming no file: pip reads its own, as without it)
        let gone = |n: &str| (n == "PIP_CONFIG_FILE").then(|| "/x/none.conf".to_string());
        assert_eq!(get(&sources(&home, &gone), "pip")[0], home.join(".pip/pip.conf"));
        std::fs::remove_dir_all(&home).ok();
    }

    /// A stand-in home with fake tokens: a copy for git (both its files, an include made whole, an included
    /// file holding one copied too), Go and pip, each file holding one hidden; npm's copy though it holds
    /// none (its file is kept); a file the user let Claude read isn't hidden; written for this user only.
    #[test]
    fn copies_leave_out_the_tokens_and_hide_their_files() {
        let t = scratch("copies");
        let (home, dir) = (t.join("home"), t.join("run"));
        std::fs::create_dir_all(&dir).unwrap();
        write(
            &home.join(".gitconfig"),
            "[alias]\n\tco = checkout\n[include]\n\tpath = extra.inc\n\tpath = secret.inc\n\
             [includeIf \"gitdir:./work/\"]\n\tpath = ~/work.inc\n[http]\n\textraHeader = Authorization: Bearer tarae-fake-token\n",
        );
        write(&home.join("extra.inc"), "[alias]\n\tst = status\n");
        write(
            &home.join("secret.inc"),
            "[http]\n\textraHeader = Authorization: Bearer tarae-fake-token\n[core]\n\tabbrev = 9\n",
        );
        write(&home.join(".config/git/config"), "[user]\n\tname = Stand In\n");
        write(
            &home.join("go.env"),
            "GOFLAGS=-mod=mod\nGOPROXY=https://u:tarae-fake-token@proxy.example.com\n",
        );
        write(&home.join(".npmrc"), "registry=https://r.example.com/\n");
        write(&home.join(".pip/pip.conf"), "[global]\ntimeout = 7\n");
        write(
            &home.join(".config/pip/pip.conf"),
            "[global]\nindex-url = https://u:tarae-fake-token@p.example.com/\n",
        );
        let home = std::fs::canonicalize(&home).unwrap();
        let mut src = sources(&home, &|_| None);
        src[1].1 = vec![home.join("go.env")];
        let kept = [home.join(".npmrc")];
        let c = copies(&src, Some(&dir), &home, &[], &kept);
        let vars: Vec<&str> = c.env.iter().map(|(v, _)| v.as_str()).collect();
        assert_eq!(vars, ["GIT_CONFIG_GLOBAL", "GOENV", "NPM_CONFIG_USERCONFIG", "PIP_CONFIG_FILE"]);
        let read = |v: &str| std::fs::read_to_string(&c.env.iter().find(|(x, _)| x == v).unwrap().1).unwrap();
        let git = read("GIT_CONFIG_GLOBAL");
        assert!(
            git.find("name = Stand In").unwrap() < git.find("co = checkout").unwrap(),
            "git's order: {git}"
        );
        assert!(git.contains(&format!("path = \"{}\"", home.join("extra.inc").display())), "{git}");
        assert!(git.contains(&format!("[includeIf \"gitdir:{}/work/\"]", home.display())), "{git}");
        assert!(git.contains(&format!("path = \"{}\"", home.join("work.inc").display())), "{git}");
        let inc = dir.join("settings/gitconfig-include-1");
        assert!(git.contains(&format!("path = \"{}\"", inc.display())), "the include's copy: {git}");
        assert!(std::fs::read_to_string(&inc).unwrap().contains("abbrev = 9"));
        assert!(read("GOENV").contains("GOFLAGS=-mod=mod"));
        assert_eq!(read("NPM_CONFIG_USERCONFIG").lines().last(), Some("registry=https://r.example.com/"));
        let pip = read("PIP_CONFIG_FILE");
        assert!(pip.contains("timeout = 7") && !pip.contains("index-url"), "{pip}");
        for (_, f) in &c.env {
            assert!(!std::fs::read_to_string(f).unwrap().contains(FAKE), "{}", f.display());
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(std::fs::metadata(f).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert!(!std::fs::read_to_string(&inc).unwrap().contains(FAKE));
        let hidden: Vec<(PathBuf, &str)> = c.hidden.iter().map(|(h, v)| (h.clone(), v.as_str())).collect();
        assert_eq!(
            hidden,
            [
                (home.join("secret.inc"), "GIT_CONFIG_GLOBAL"),
                (home.join(".gitconfig"), "GIT_CONFIG_GLOBAL"),
                (home.join("go.env"), "GOENV"),
                (home.join(".config/pip/pip.conf"), "PIP_CONFIG_FILE"),
            ]
        );
        assert_eq!(c.instead_of(&home.join(".gitconfig")), Some("GIT_CONFIG_GLOBAL"));
        assert_eq!(c.left_out, c.hidden, "what's left out is what's hidden here");
        let (files, vars) = c.said().unwrap();
        assert!(files.contains("go.env") && vars == "GIT_CONFIG_GLOBAL, GOENV, PIP_CONFIG_FILE", "{vars}");
        // A file the user let Claude read: copied all the same, not hidden
        let dir2 = t.join("run2");
        std::fs::create_dir_all(&dir2).unwrap();
        let granted = copies(&src, Some(&dir2), &home, &[home.join(".config/pip")], &kept);
        assert!(!granted.hidden.iter().any(|(h, _)| h.starts_with(home.join(".config/pip"))));
        assert!(granted.env.iter().any(|(v, _)| v == "PIP_CONFIG_FILE"));
        assert!(granted.left_out.iter().any(|(h, _)| h.starts_with(home.join(".config/pip"))));
        // No run folder: nothing copied, the files still hidden
        let none = copies(&src, None, &home, &[], &kept);
        assert!(none.env.is_empty() && none.left_out.is_empty() && none.hidden == c.hidden);
        std::fs::remove_dir_all(&t).ok();
    }

    /// No token anywhere and no kept file: nothing changes — no copy, nothing hidden.
    #[test]
    fn without_a_token_nothing_changes() {
        let t = scratch("none");
        let (home, dir) = (t.join("home"), t.join("run"));
        std::fs::create_dir_all(&dir).unwrap();
        write(&home.join(".gitconfig"), "[alias]\n\tco = checkout\n[credential]\n\thelper = osxkeychain\n");
        write(&home.join(".config/pip/pip.conf"), "[global]\nindex-url = https://p.example.com/simple\n");
        write(&home.join(".npmrc"), "//r.example.com/:_authToken=${NPM_TOKEN}\n");
        let home = std::fs::canonicalize(&home).unwrap();
        let c = copies(&sources(&home, &|_| None), Some(&dir), &home, &[], &[]);
        assert_eq!(c, Copies::default());
        assert!(!dir.join("settings").exists(), "nothing written");
        std::fs::remove_dir_all(&t).ok();
    }

    /// Cargo's configs holding one — in its home wherever `CARGO_HOME` puts it, and above the project — are
    /// found, said once in the rust pack's words, and again only after a time without one.
    #[test]
    fn cargos_tokens_are_told_once() {
        let t = scratch("cargo");
        let cargo = t.join("cargo");
        write(&cargo.join("config.toml"), "[net]\nretry = 3\n[registry]\ntoken = \"tarae-fake-token\"\n");
        write(&t.join("p/.cargo/config.toml"), "[build]\njobs = 2\n");
        write(&t.join("q/.cargo/config"), "[registries.x]\ntoken = \"tarae-fake-token\"\n");
        let var = |n: &str| (n == "CARGO_HOME").then(|| cargo.clone());
        let vars = crate::sandboxpack::Vars::new(crate::sandboxpack::get(), &t.join("home"), &var, Os::HOST);
        let above = [t.join("p/.cargo/config.toml"), t.join("q/.cargo/config"), t.join("gone/.cargo/config")];
        let holders = told_holders(&vars, &above);
        let files: Vec<&PathBuf> = holders.iter().map(|(f, _)| f).collect();
        assert_eq!(files, [&cargo.join("config.toml"), &t.join("q/.cargo/config")]);
        let said = t.join("state/tokens-said");
        let (new, now) = to_tell(&holders, Some(&said));
        let paths: Vec<PathBuf> = holders.iter().map(|(f, _)| f.clone()).collect();
        assert_eq!((new.clone(), now.clone()), (holders.clone(), Some(paths.clone())));
        write(&said, &paths.iter().map(|p| format!("{}\n", p.display())).collect::<String>());
        assert_eq!(to_tell(&holders, Some(&said)), (vec![], None), "told already");
        assert_eq!(to_tell(&[], Some(&said)), (vec![], Some(vec![])), "none now: taken off");
        // On screen: once since tarae started, too — the pack's words, `{them}` filled in
        let mut ed = Editor::new(crate::config::Config::default());
        let said = |ed: &Editor| ed.messages.iter().filter(|m| m.2.contains("credentials.toml")).count();
        tell_holders(&mut ed, &new, None);
        assert_eq!(said(&ed), 1, "{:?}", ed.toasts);
        let m = ed.messages.iter().find(|m| m.2.contains("credentials.toml")).unwrap();
        assert!(
            m.2.contains("hold tokens, and Claude's commands read them — cargo can't work without them."),
            "{}",
            m.2
        );
        assert!(!ed.toasts.iter().any(|t| t.text.contains(FAKE)));
        tell_holders(&mut ed, &new, None);
        assert_eq!(said(&ed), 1, "said twice");
        std::fs::remove_dir_all(&t).ok();
    }

    /// A card's words: the file itself, files in a folder (by name, never the value; a few, then how many
    /// more), code skipped, tools' output not looked in — and a folder too big to look through all.
    #[test]
    fn what_a_card_says_of_a_place() {
        let t = scratch("found");
        write(&t.join("a/.env"), "GITHUB_TOKEN=tarae-fake-token\n");
        write(&t.join("a/hosts.yml"), "oauth_token: tarae-fake-token\n");
        write(&t.join("a/src/main.rs"), "const PAT: &str = \"ghp_0123456789abcdefABCDEF\";\n");
        write(&t.join("a/node_modules/x/.npmrc"), "_auth=tarae-fake-token\n");
        write(&t.join("a/plain.toml"), "[x]\ny = 1\n");
        let a = t.join("a");
        let f = found_in(&a);
        let named = vec![a.join(".env")];
        assert_eq!(f, Found { files: vec![a.join(".env"), a.join("hosts.yml")], more: 0, cut: false, named });
        // (places as a card shows them, `~` for home: the test's temp folder is in home in a sandboxed run —
        // its own folder, in tarae's cache)
        let shown = crate::allow::tilde;
        let said = f.said(&a).unwrap();
        assert_eq!(said, format!("In {}, .env and hosts.yml hold token-like lines.", shown(&a)));
        assert!(!said.contains(FAKE));
        let one = found_in(&a.join("hosts.yml"));
        let file = a.join("hosts.yml");
        assert_eq!(one.said(&file), Some(format!("{} holds a token-like line.", shown(&file))));
        assert_eq!(found_in(&a.join("plain.toml")).said(&a.join("plain.toml")), None);
        for i in 0..5 {
            write(&t.join(format!("b/{i}.conf")), "password = tarae-fake-token\n");
        }
        let b = found_in(&t.join("b"));
        assert_eq!((b.files.len(), b.more), (3, 2));
        let said = b.said(&t.join("b")).unwrap();
        assert!(said.ends_with(".conf and 2 more hold token-like lines."), "{said}");
        let cut = Found { cut: true, ..Found::default() };
        let said = cut.said(&a).unwrap();
        assert_eq!(said, format!("{} has more files than tarae looks through for tokens.", shown(&a)));
        std::fs::remove_dir_all(&t).ok();
    }

    /// #177: a file of the kernel's says it's empty and isn't — `/proc/<pid>/pagemap` holds 8 bytes for every
    /// page of the address space (~256 GiB on x86_64). Read to its end, a card for `/` never came, and the
    /// worker went on reading after its test had failed until the test binary was 15 GB. Linux: procfs.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_file_saying_its_empty_is_read_no_further() {
        let pagemap = Path::new("/proc/self/pagemap");
        assert_eq!(std::fs::symlink_metadata(pagemap).unwrap().len(), 0, "procfs says 0");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || tx.send(found_in(pagemap)).ok());
        let found = rx.recv_timeout(std::time::Duration::from_secs(2)).expect("read past the size it says");
        assert_eq!(found, Found::default());
    }

    /// The kernel's own file systems aren't looked in — told by their type, not their name: `/proc` and
    /// `/sys` hold thousands of files (walked, more than `LOOK_MAX`: cut), `/` and the temp folder are no
    /// kernel's. Linux.
    #[test]
    #[cfg(target_os = "linux")]
    fn the_kernels_file_systems_arent_looked_in() {
        assert!(the_kernels(Path::new("/proc")) && the_kernels(Path::new("/sys")));
        assert!(!the_kernels(Path::new("/")) && !the_kernels(&std::env::temp_dir()));
        for place in ["/proc", "/sys"] {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || tx.send(found_in(Path::new(place))).ok());
            let found = rx.recv_timeout(std::time::Duration::from_secs(10)).expect(place);
            assert_eq!(found, Found::default(), "{place} looked in");
        }
    }

    /// A place allowed only as a whole — `/`, home, a folder above home — is too big for a walk to say
    /// anything of use, and a card waited seconds on one: it isn't walked, and says so as a cut walk does.
    /// What's below home is walked as before.
    #[test]
    fn a_place_thats_everything_isnt_walked() {
        let t = scratch("everything");
        let home = t.join("home");
        write(&home.join(".env"), "GITHUB_TOKEN=tarae-fake-token\n");
        write(&home.join("work/hosts.yml"), "oauth_token: tarae-fake-token\n");
        let cut = Found { cut: true, ..Found::default() };
        assert_eq!(found_under(&home, Some(&home)), cut, "home itself");
        assert_eq!(found_under(&t, Some(&home)), cut, "above home");
        assert_eq!(found_under(Path::new("/"), None), cut, "/ with no home");
        let work = found_under(&home.join("work"), Some(&home));
        assert_eq!(work.files, [home.join("work/hosts.yml")], "below home: walked");
        // (home as tarae finds it: `/` is above every home)
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || tx.send(found_in(Path::new("/"))).ok());
        assert_eq!(rx.recv_timeout(std::time::Duration::from_millis(200)), Ok(cut), "/ walked");
        std::fs::remove_dir_all(&t).ok();
    }

    /// A walk stops after `VISIT_MAX` entries, whether it read them or not (code and folders aren't read):
    /// 44k entries took ~1 s in a debug build — no big folder holds a card up. Cut, and said so.
    #[test]
    fn a_walk_stops_after_so_many_entries() {
        let t = scratch("entries");
        for i in 0..VISIT_MAX {
            write(&t.join(format!("d{}/{i}.rs", i % 100)), "");
        }
        assert_eq!(found_in(&t), Found { cut: true, ..Found::default() });
        // (fewer: walked to the end)
        let few = scratch("entries-few");
        write(&few.join("a/b.rs"), "");
        assert_eq!(found_in(&few), Found::default());
        std::fs::remove_dir_all(&t).ok();
        std::fs::remove_dir_all(&few).ok();
    }
}
