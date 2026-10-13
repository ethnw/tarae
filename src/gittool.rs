//! The `git` tool (frictions19, #215): git's reads for Claude with no card — a day's runs had 46 of 525 git, every
//! one a read (diff, log, show, archive, status, ls-tree, grep at a revision), every one a card. Only these
//! subcommands: `log` `show` `diff` `status` `blame` `ls-tree` `grep` (at a revision too); writes stay the user's
//! git tab.
//!
//! - Run on the session's repository through `gitrepo`'s guarded path (`Repo::command`: the repository's own
//!   commands held until trusted, no prompt, no optional locks), hooks off (`NO_HOOKS`), no pager, no external
//!   diff or textconv (`--no-ext-diff`, `--no-textconv` where the subcommand takes them, `GIT_EXTERNAL_DIFF`
//!   unset), no signatures checked (gpg) — tarae's own git, as the git tab's: outside the sandbox, on a worker.
//! - Options: an allowlist per subcommand (`takes`) — anything else is refused, git's own options before the
//!   subcommand too (`-c`, `--git-dir` …), and an alias is never a subcommand here. Long ones by their whole
//!   name (git takes any unambiguous start of one — not here); formats by name only (`FORMATS`: no `%` string,
//!   so no signature placeholder). Color, prefixes, quoting and the signing programs are set on every run over
//!   the user's settings (`PLAIN`, `--no-color`).
//! - Key files stay out on git's side, never filtered from its output (review of #215: a filter on what git
//!   prints can't be made sound — color, prefixes, `-h`, a merge's `diff --cc`, a blob by its id each got
//!   past one): what can print a file's contents (diff, show, grep, log with a patch) gets an exclusion pathspec
//!   for every key-name pattern (`exclusions`), so git never reads one; a path named that is a key file is
//!   refused before git runs (`named_key`); every revision is resolved first (`revisions`, one `cat-file
//!   --batch-check`) — a blob by its id (or a tag of one) refused, `rev:path` only with the path judged.
//!   Paths go after `--`: a word before it that isn't a revision is refused (said so).
//! - Output: at most `CAP` bytes read (git stopped there, said), shown `PAGE` lines at a time from `offset`, each
//!   cut at `WIDTH` characters.

use std::io::Read as _;
use std::path::Path;

use serde_json::{Value, json};

use crate::editor::Editor;
use crate::tools::Call;

/// The subcommands it runs.
const SUBCOMMANDS: [&str; 7] = ["log", "show", "diff", "status", "blame", "ls-tree", "grep"];

/// What a subcommand takes — nothing else: long options by their whole name (`--stat`), those taking a value
/// after `=` (`--since=2.weeks`), short flags (clustered too: `-nw`), short ones taking a value — the rest of
/// the word, else the next one (`-n5`, `-e x`) — short ones whose value can only be attached (`-U3`, `-M50%`),
/// and whether `-<n>` is a count (log's `-20`).
struct Takes {
    long: &'static [&'static str],
    valued: &'static [&'static str],
    flags: &'static str,
    short_valued: &'static str,
    attached: &'static str,
    counts: bool,
}

const DIFF_VALUED: [&str; 5] = ["--unified", "--stat", "--diff-filter", "--find-renames", "--abbrev"];
const LOG_LONG: [&str; 46] = [
    "--patch",
    "--no-patch",
    "--stat",
    "--numstat",
    "--shortstat",
    "--name-only",
    "--name-status",
    "--summary",
    "--raw",
    "--find-renames",
    "--no-renames",
    "--ignore-all-space",
    "--ignore-space-change",
    "--ignore-blank-lines",
    "--minimal",
    "--patience",
    "--histogram",
    "--function-context",
    "--full-index",
    "--oneline",
    "--all",
    "--branches",
    "--tags",
    "--remotes",
    "--first-parent",
    "--no-merges",
    "--merges",
    "--reverse",
    "--graph",
    "--decorate",
    "--no-decorate",
    "--abbrev-commit",
    "--follow",
    "--left-right",
    "--cherry-pick",
    "--topo-order",
    "--date-order",
    "--regexp-ignore-case",
    "--extended-regexp",
    "--fixed-strings",
    "--all-match",
    "--invert-grep",
    "--source",
    "--full-history",
    "--simplify-by-decoration",
    "--pickaxe-all",
];
const LOG_VALUED: [&str; 18] = [
    "--unified",
    "--stat",
    "--diff-filter",
    "--find-renames",
    "--abbrev",
    "--max-count",
    "--skip",
    "--since",
    "--until",
    "--after",
    "--before",
    "--author",
    "--committer",
    "--grep",
    "--format",
    "--pretty",
    "--date",
    "--decorate",
];
const LOG: Takes = Takes {
    long: &LOG_LONG,
    valued: &LOG_VALUED,
    flags: "pswiEFR",
    short_valued: "nSG",
    attached: "MU",
    counts: true,
};

/// The options `sub` takes (`Takes`).
fn takes(sub: &str) -> Takes {
    match sub {
        "log" | "show" => LOG,
        "diff" => Takes {
            long: &[
                "--patch",
                "--no-patch",
                "--stat",
                "--numstat",
                "--shortstat",
                "--name-only",
                "--name-status",
                "--summary",
                "--raw",
                "--find-renames",
                "--no-renames",
                "--ignore-all-space",
                "--ignore-space-change",
                "--ignore-blank-lines",
                "--minimal",
                "--patience",
                "--histogram",
                "--function-context",
                "--full-index",
                "--cached",
                "--staged",
                "--merge-base",
            ],
            valued: &DIFF_VALUED,
            flags: "pswbR",
            short_valued: "",
            attached: "MU",
            counts: false,
        },
        "status" => Takes {
            long: &["--short", "--branch", "--porcelain", "--long", "--ahead-behind", "--no-ahead-behind"],
            valued: &["--porcelain", "--untracked-files", "--ignored"],
            flags: "sb",
            short_valued: "",
            attached: "u",
            counts: false,
        },
        "blame" => Takes {
            long: &[
                "--root",
                "--show-stats",
                "--porcelain",
                "--line-porcelain",
                "--show-name",
                "--show-number",
            ],
            valued: &["--date", "--abbrev"],
            // (not `-C`/`-M`: they search other files for where a line came from, and name one — a key file
            // among them: an oracle on its lines)
            flags: "wsefltnc",
            short_valued: "L",
            attached: "",
            counts: false,
        },
        "ls-tree" => Takes {
            long: &["--name-only", "--name-status", "--full-name", "--full-tree", "--object-only", "--long"],
            valued: &["--abbrev"],
            flags: "rtdl",
            short_valued: "",
            attached: "",
            counts: false,
        },
        // grep
        _ => Takes {
            long: &[
                "--line-number",
                "--ignore-case",
                "--word-regexp",
                "--invert-match",
                "--extended-regexp",
                "--fixed-strings",
                "--perl-regexp",
                "--basic-regexp",
                "--files-with-matches",
                "--name-only",
                "--files-without-match",
                "--count",
                "--untracked",
                "--cached",
                "--and",
                "--or",
                "--not",
                "--only-matching",
                "--column",
                "--all-match",
                "--show-function",
                "--function-context",
                "--full-name",
                "--break",
                "--heading",
                "--no-filename",
            ],
            valued: &["--context", "--after-context", "--before-context", "--max-depth", "--max-count"],
            flags: "niwvEFPGlLcohHWpI",
            short_valued: "eABCm",
            attached: "",
            counts: false,
        },
    }
}

/// Settings on every run, over the user's: no color, the plain `a/` `b/` prefixes, names unquoted, no signature
/// checked — and if one were asked for anyway, `false` is the program that would check it, never gpg — tarae's
/// own config parameters on the guarded run, never the user's files.
const PLAIN: [&str; 24] = [
    "-c",
    "log.showSignature=false",
    "-c",
    "gpg.program=false",
    "-c",
    "gpg.ssh.program=false",
    "-c",
    "gpg.x509.program=false",
    "-c",
    "color.ui=never",
    "-c",
    "color.diff=never",
    "-c",
    "color.grep=never",
    "-c",
    "diff.noprefix=false",
    "-c",
    "diff.mnemonicPrefix=false",
    "-c",
    "diff.srcPrefix=a/",
    "-c",
    "diff.dstPrefix=b/",
    "-c",
    "core.quotePath=false",
];

/// An exclusion pathspec for every key-name pattern (`secretnames`, any case, any folder): what's appended to a
/// run that can print a file's contents, so git never reads a key file. (Templates the names' exceptions let
/// through — `.env.example` — are left out with them: not worth a second rule.)
fn exclusions() -> Vec<String> {
    crate::secretnames::patterns().map(|p| format!(":(exclude,glob,icase)**/{p}")).collect()
}

/// Bytes of git's output read at most — git is stopped there.
const CAP: usize = 4 << 20;
/// Lines shown per answer.
const PAGE: usize = 300;
/// Characters of a line shown at most.
const WIDTH: usize = 400;

pub fn schema() -> Value {
    json!({
        "name": "git",
        "description": "Read the repository with git — log, show, diff, status, blame, ls-tree, grep (at a revision \
    too), no card. Revisions before --, paths after it. Long output comes a page at a time; the answer says the \
    next offset. Nothing that writes.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "args": { "type": "array", "items": { "type": "string" }, "description": "git's words after `git`, subcommand first: [\"show\", \"HEAD~1:a.rs\"]" },
                "offset": { "type": "integer", "description": "Lines to skip — the next page" }
            },
            "required": ["args"]
        }
    })
}

/// A call: its words checked now (a refused one is answered at once), git run on a worker.
pub fn call(ed: &mut Editor, call: Call, args: &Value) -> Result<(), String> {
    let words: Vec<String> = args["args"]
        .as_array()
        .ok_or("args: git's words after `git`, as a list — [\"log\", \"-5\"]")?
        .iter()
        .map(|w| w.as_str().map(str::to_string).ok_or("args: each a string"))
        .collect::<Result<_, _>>()?;
    let offset = args["offset"].as_u64().unwrap_or(0) as usize;
    let parsed = parse(&words)?;
    let (root, keys) = (ed.session.root.clone(), crate::reads::Keys::of(ed));
    ed.events.jobs().spawn(move || {
        let r = run(&root, &keys, &parsed, offset);
        move |ed: &mut Editor| crate::tools::answer(ed, call, r)
    });
    Ok(())
}

/// A call's words as git will get them: the subcommand, its options (each with its value), grep's pattern,
/// the revisions, the paths (after `--`).
#[derive(Clone, Debug, Default)]
struct Parsed {
    sub: String,
    opts: Vec<String>,
    pattern: Option<String>,
    revs: Vec<String>,
    paths: Vec<String>,
}

/// `words` against what their subcommand takes (`takes`) — Err: why not (a subcommand that isn't a read, an
/// option not taken, a value it needs missing).
fn parse(words: &[String]) -> Result<Parsed, String> {
    let Some((sub, rest)) = words.split_first() else { return Err("args: the subcommand first".into()) };
    if !SUBCOMMANDS.contains(&sub.as_str()) {
        return Err(format!(
            "git {sub}: not here — this tool reads only ({}); git's own options (-c, --git-dir …) aren't taken",
            SUBCOMMANDS.join(" ")
        ));
    }
    let t = takes(sub);
    let refused = |w: &str| {
        format!(
            "git {sub} {w}: not an option this tool takes — it takes {}a few short ones (-{}{}{}); a value after `=` \
             (--since=2.weeks)",
            t.long.iter().chain(t.valued).take(12).map(|l| format!("{l} ")).collect::<String>(),
            t.flags,
            t.short_valued,
            t.attached
        )
    };
    let mut p = Parsed { sub: sub.clone(), ..Default::default() };
    let mut positional = Vec::new();
    let mut e_given = false;
    let mut i = 0;
    while i < rest.len() {
        let w = &rest[i];
        i += 1;
        if w == "--" {
            p.paths = rest[i..].to_vec();
            break;
        }
        if let Some(long) = w.strip_prefix("--") {
            let ok = match long.split_once('=') {
                Some((name, _)) => t.valued.contains(&format!("--{name}").as_str()),
                None => t.long.contains(&w.as_str()),
            };
            // (a format only by its name: no `%` string, so no placeholder — `%G?` and its modified spellings run
            // gpg on each commit — can be asked for at all)
            let named = match long.split_once('=') {
                Some(("format" | "pretty", v)) => FORMATS.contains(&v),
                _ => true,
            };
            if !ok || !named {
                return Err(refused(w));
            }
            p.opts.push(w.clone());
            continue;
        }
        let Some(cluster) = w.strip_prefix('-').filter(|c| !c.is_empty()) else {
            positional.push(w.clone());
            continue;
        };
        if t.counts && cluster.chars().all(|c| c.is_ascii_digit()) {
            p.opts.push(w.clone());
            continue;
        }
        p.opts.push(w.clone());
        for (at, c) in cluster.char_indices() {
            if t.flags.contains(c) {
                continue;
            }
            if t.attached.contains(c) {
                break;
            }
            if !t.short_valued.contains(c) {
                return Err(refused(w));
            }
            e_given |= c == 'e';
            // (its value: the rest of the word, else the next one)
            if at + c.len_utf8() == cluster.len() {
                let v = rest.get(i).ok_or_else(|| format!("git {sub} {w}: its value is missing"))?;
                p.opts.push(v.clone());
                i += 1;
            }
            break;
        }
    }
    // (grep's first word that isn't an option is its pattern — unless `-e` gave one)
    if p.sub == "grep" && !e_given && !positional.is_empty() {
        p.pattern = Some(positional.remove(0));
    }
    p.revs = positional;
    Ok(p)
}

/// The formats `--format=` / `--pretty=` take: git's named ones only — never a `%` string.
const FORMATS: [&str; 6] = ["oneline", "short", "medium", "full", "fuller", "reference"];

/// What `p` names that's a file named like a key, as the repository has it (`HEAD:.env`, `:2:a.pem`, a path
/// after `--`) — the first.
fn named_key(p: &Parsed, is_key: &dyn Fn(&str) -> bool) -> Option<String> {
    for w in p.revs.iter().filter(|w| w.contains(':')).chain(&p.paths) {
        let mut candidates = vec![w.as_str()];
        if let Some((_, after)) = w.split_once(':') {
            candidates.push(after);
        }
        if let Some((_, last)) = w.rsplit_once(':') {
            candidates.push(last);
        }
        if candidates.iter().any(|c| is_key(c)) {
            return Some(w.clone());
        }
    }
    None
}

/// The answer: git's output (`offset` on, a page), or why not. A worker's.
fn run(root: &Path, keys: &crate::reads::Keys, p: &Parsed, offset: usize) -> Result<String, String> {
    // (named like a key: a cheap test of the name first, then where it really is — `Keys::shut`)
    let is_key = |rel: &str| {
        let rel = rel.trim_matches('"');
        !rel.is_empty() && crate::secretnames::named_secret(Path::new(rel)) && keys.shut(&root.join(rel))
    };
    if let Some(w) = named_key(p, &is_key) {
        return Err(crate::reads::key_left_out(&w));
    }
    let repo = crate::gitrepo::repo(root)?;
    revisions(&repo, &p.revs)?;
    let sub = p.sub.as_str();
    let mut all: Vec<String> = PLAIN.iter().chain(&crate::gitrepo::NO_HOOKS).map(|s| s.to_string()).collect();
    all.push(sub.into());
    let forced: &[&str] = match sub {
        "log" | "show" | "diff" => &["--no-color", "--no-ext-diff", "--no-textconv"],
        "grep" => &["--no-color", "--no-textconv"],
        "blame" => &["--no-textconv"],
        _ => &[],
    };
    all.extend(forced.iter().map(|s| s.to_string()));
    all.extend(p.opts.iter().cloned());
    all.extend(p.pattern.iter().cloned());
    all.extend(p.revs.iter().cloned());
    // What can print a file's contents — or answer a question about them (log's `-S`/`-G` search every
    // diff: run again and again, a match oracle on a key file) — never reads a key file: excluded on git's side,
    // every log too (one that only touched a key file isn't listed)
    let contents = matches!(sub, "diff" | "show" | "grep" | "log");
    if contents || !p.paths.is_empty() {
        all.push("--".into());
        all.extend(p.paths.iter().cloned());
    }
    if contents {
        if p.paths.is_empty() {
            all.push(".".into());
        }
        all.extend(exclusions());
    }
    let words: Vec<&str> = all.iter().map(String::as_str).collect();
    let mut cmd = repo.command(&words);
    cmd.env("GIT_PAGER", "cat").env("PAGER", "cat").env_remove("GIT_EXTERNAL_DIFF");
    let (out, err, ok, cut) = read_capped(&cmd)?;
    // (grep found nothing: exit 1, nothing said)
    if !ok && sub == "grep" && out.is_empty() && err.trim().is_empty() {
        return Ok("(no matches)".into());
    }
    if !ok {
        let said: Vec<&str> = err.lines().filter(|l| !l.trim().is_empty()).take(6).collect();
        return Err(if said.is_empty() { "git failed".to_string() } else { said.join("\n") });
    }
    Ok(page(&String::from_utf8_lossy(&out), offset, cut))
}

/// The objects a revision word names (`main`, `A..B`, `^A`, `HEAD^@`, `HEAD:src/a.rs`) — each end of a range.
fn ends(w: &str) -> Vec<&str> {
    let w = w.strip_prefix('^').unwrap_or(w);
    let w = w.strip_suffix("^@").or_else(|| w.strip_suffix("^!")).unwrap_or(w);
    let w = w.split_once("^-").map_or(w, |(a, _)| a);
    w.split("...").flat_map(|p| p.split("..")).filter(|p| !p.is_empty()).collect()
}

/// Every revision resolved before git runs (one `cat-file --batch-check`, each peeled: `^{}`) — Err for one
/// that isn't (a path before `--`), and for a blob named by itself, an id or a tag of one: `ls-tree` shows
/// `.env`'s id, and `show <id>` would print it, named nowhere. `rev:path` names its path, judged by name.
fn revisions(repo: &crate::gitrepo::Repo, revs: &[String]) -> Result<(), String> {
    let asked: Vec<(&String, &str)> =
        revs.iter().flat_map(|w| ends(w).into_iter().map(move |e| (w, e))).collect();
    if asked.is_empty() {
        return Ok(());
    }
    // (peeled — a tag of a blob is the blob — but `rev:path`, which `^{}` would make part of the path)
    let input: String = asked
        .iter()
        .map(|(_, e)| if e.contains(':') { format!("{e}\n") } else { format!("{e}^{{}}\n") })
        .collect();
    let out = crate::gitrepo::finish(&repo.command(&["cat-file", "--batch-check"]), Some(input.as_bytes()))?;
    let types = String::from_utf8_lossy(&out);
    // (a line each, in order: `<id> <type> <size>`, or `<name> missing` / `ambiguous`)
    for ((w, e), line) in asked.iter().zip(types.lines()) {
        match line.split(' ').nth(1) {
            Some("blob") if !e.contains(':') => {
                return Err(format!(
                    "{w} is a file's contents by its id — name it as rev:path (HEAD:src/a.rs), so what it is can be \
                     told"
                ));
            }
            Some("blob" | "tree" | "commit" | "tag") => {}
            _ => return Err(format!("{w} isn't a revision here — paths go after --")),
        }
    }
    Ok(())
}

/// git run, at most `CAP` bytes of its output read — past that it's stopped (its process group: `setsid`'s).
/// (stdout, stderr, whether it went well — stopped counts —, whether it was stopped).
fn read_capped(cmd: &std::process::Command) -> Result<(Vec<u8>, String, bool, bool), String> {
    let said = |e: std::io::Error| format!("git: {e}");
    let s = crate::spawn::setsid_git(cmd).map_err(said)?;
    // (stderr read beside stdout: a full pipe of warnings can't hold git up)
    let errs = s.stderr.map(|mut e| {
        crate::event::thread("tarae-git-err", move || {
            let mut v = Vec::new();
            let _ = e.read_to_end(&mut v);
            v
        })
    });
    let mut out = Vec::new();
    let mut cut = false;
    if let Some(o) = s.stdout {
        let read = o.take(CAP as u64 + 1).read_to_end(&mut out);
        if out.len() > CAP {
            cut = true;
            out.truncate(CAP);
            // SAFETY: the process group `setsid` made for it — ours, and still unwaited (its pid can't be reused).
            unsafe { libc::kill(-(s.pid as libc::pid_t), libc::SIGKILL) };
        }
        read.map_err(said)?;
    }
    let err = errs.and_then(|t| t.join().ok()).unwrap_or_default();
    let status = crate::spawn::wait(s.pid).map_err(said)?;
    Ok((out, String::from_utf8_lossy(&err).into_owned(), status.success() || cut, cut))
}

/// `text` from line `offset`: a page, each line cut at `WIDTH`, what's around it said.
fn page(text: &str, offset: usize, cut: bool) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let mut said = String::new();
    if total == 0 {
        return "(no output)".into();
    }
    if offset >= total {
        return said + &format!("{total} lines; none past offset {offset}");
    }
    let end = (offset + PAGE).min(total);
    if offset > 0 || end < total {
        said.push_str(&format!("lines {}–{end} of {total}{}\n", offset + 1, if cut { "+" } else { "" }));
    }
    for l in &lines[offset..end] {
        match l.char_indices().nth(WIDTH) {
            Some((i, _)) => said.push_str(&format!("{}…\n", &l[..i])),
            None => said.push_str(&format!("{l}\n")),
        }
    }
    if end < total {
        said.push_str(&format!("… {} more — offset: {end} for the next ones\n", total - end));
    }
    if cut && end == total {
        said.push_str("(git was stopped after 4 MB of output — narrow it: -n, a path, --stat)\n");
    }
    said
}

#[cfg(test)]
mod tests;
