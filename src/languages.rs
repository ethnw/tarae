//! The language table — every language tarae knows, from the packs (`runtime/packs/<pack>/pack.toml`):
//!
//! - `[language.<name>]`: how its files are recognised (`extensions`, `filenames`, `path-suffixes`, `globs` —
//!   one `*` per segment), `grammar` (its name unless set), commented (`comment-token`,
//!   `block-comment-tokens = { start, end }`), paired (`auto-pairs`, each `"()"`), called (`title`, else the
//!   name with its first letter up; `aliases` — code fence and shebang names besides its extensions), drawn
//!   (`doc-comments = true`: `///`, `//!`, `/** */` rendered), indented (`indent-tabs`, `indent-after-colon`,
//!   `indent-heuristic = "simple"`: the line before's indentation as it is),
//!   wrapped (`prose = true`, for `editor.soft-wrap = "prose"`); its servers (`lsp` — the first on `PATH`
//!   starts — and `also`, beside it; `language-id`, the name unless set); what makes its project (`roots`,
//!   the topmost below the repository wins; none: any language's) and whether its server needs one
//!   (`needs-project`); its test files by name (`test-files`, `test-dirs`); grammars installed along with
//!   its own (`grammars-with`).
//! - `[grammar.<name>]`: `git`, `rev`, `subpath` — the upstream C, at the commit its queries were written
//!   for; `core`, `dev`, `bundle`: built into which binaries (`build.rs`, the built-in packs only);
//!   `everywhere = true`: installed with every grammar.
//! - `[server.<name>]`: `command` (the name unless set), `args`; what it speaks beyond the protocol
//!   (`lsp::ServerTraits`): `share = { toolchain, checks, reload-progress }` (one for a repository's checkouts —
//!   `share.rs`), `quiescent = { method, capability, field, health, message }`, `symbol-params`, `hide-lines`;
//!   how it starts beyond `args` (`args-add`, `args-unless`, `init`); `progress = { method, kind, text, done,
//!   done-text, error }`, `virtual = { scheme, fetch, language, title-join }` (`virtualdoc.rs`),
//!   `client-commands`.
//! - `config-files = { toml = [...], json = [...] }`: files checked for parse errors before a save (`disk.rs`;
//!   a name with a `/` is the path's last parts); `outputs`: tools' output folders that walks skip.
//!
//! Built once (`get`), before the first frame — `main` asks for it before reading the config — from the
//! built-in packs and a runtime folder's (`~/.local/share/tarae/packs/`), each key by its `packs::Reach`:
//! languages and servers key by key, grammars entry by entry, a language merged by its name whichever folder
//! says it. After that it's memory — files opened, fences drawn, servers started read no disk for it. So a
//! runtime folder's `[language]`, `[grammar]` and `[server]` count from the next start.

use std::sync::OnceLock;

use toml::{Table, Value};

use crate::lsp::{ClientAction, Quiescent, ServerSpec, ServerTraits, Share, Status, Virtual};
use crate::packs::{Reach, known, reach, string, strings};

#[derive(Debug, Clone)]
pub struct LangSpec {
    pub name: String,
    pub grammar: String,
    pub extensions: Vec<String>,
    pub filenames: Vec<String>,
    pub path_suffixes: Vec<String>,
    /// Path-suffix patterns — up to one `*` per segment (`templates/*.yaml` = Helm chart templates).
    pub globs: Vec<String>,
    /// Line comment token (`//`) · block comment (`/*`, `*/`) — `space c`/`C-c` and `space C` toggle them.
    pub comment_token: Option<String>,
    pub block_comment: Option<(String, String)>,
    /// Insert-mode pairs (open, close) — None = the defaults.
    pub auto_pairs: Option<Vec<(char, char)>>,
    /// As people call it (`C++`, `go.mod`).
    pub title: String,
    /// Code fence and shebang names besides its name and extensions (`golang`, `python3`).
    pub aliases: Vec<String>,
    pub doc_comments: bool,
    pub indent_tabs: bool,
    pub indent_after_colon: bool,
    /// `indent-heuristic = "simple"`: a new line takes the line before's indentation as it is (`indent::simple`).
    pub simple_indent: bool,
    pub prose: bool,
    /// LSP's languageId.
    pub language_id: String,
    /// What makes its project's root (none: any language's).
    pub roots: Vec<String>,
    /// Its server starts only in a project with one of these.
    pub needs_project: Vec<String>,
    /// Its server's candidates (the first on `PATH`), and the servers beside it.
    pub lsp: Vec<String>,
    pub also: Vec<String>,
    pub test_files: Vec<String>,
    pub test_dirs: Vec<String>,
    pub grammars_with: Vec<String>,
    /// A runtime folder's pack said something of it — it wins a tie in detection.
    pub yours: bool,
}

/// Every language, grammar and server the packs know, and the lists beside them.
#[derive(Debug, Default)]
pub struct Languages {
    /// Detection order: those a runtime folder touched first, then by name.
    pub langs: Vec<LangSpec>,
    pub grammars: Vec<crate::grammar::Source>,
    /// Grammars installed with every grammar (`comment`: TODO, NOTE in comments).
    pub everywhere: Vec<String>,
    pub servers: Vec<ServerSpec>,
    /// Each server's traits, by its name (`lsp::traits`).
    pub traits: std::collections::HashMap<String, ServerTraits>,
    /// Every language's `roots` — a language with none of its own takes these.
    pub roots_any: Vec<String>,
    pub config_toml: Vec<String>,
    pub config_json: Vec<String>,
    pub outputs: Vec<String>,
    pub test_files: Vec<String>,
    pub test_dirs: Vec<String>,
    /// What was wrong in the packs read (a runtime folder's, mostly) — logged, and said at the start.
    pub said: Vec<String>,
}

/// The table — built at its first use: the built-in packs, and (not in tests) the runtime folders' (their
/// disk read once). `main` asks for it first thing, so that use is never on the key path.
pub fn get() -> &'static Languages {
    static TABLE: OnceLock<Languages> = OnceLock::new();
    TABLE.get_or_init(|| {
        let yours = if cfg!(test) { Vec::new() } else { crate::runtime::pack_manifests_user() };
        let t = build(crate::packs::builtin_manifests(), &yours);
        for s in &t.said {
            crate::log::line("tarae", "warning", s);
        }
        t
    })
}

/// The packs' top-level lists the table holds — every pack's together.
const LISTS: &[&str] = &["config-files", "outputs"];

/// One language's, grammar's or server's entry while the table is put together.
struct Entry {
    name: String,
    pack: String,
    t: Table,
    yours: bool,
}

/// The table from the built-in packs ((pack, its table, read)) and a runtime folder's ((pack, its text)).
pub(crate) fn build(builtin: &[(String, Result<Table, String>)], yours: &[(String, String)]) -> Languages {
    let mut said = Vec::new();
    let (mut langs, mut grammars, mut servers) =
        (Vec::<Entry>::new(), Vec::<Entry>::new(), Vec::<Entry>::new());
    let mut lists = Table::new();
    for (pack, t) in builtin {
        let t = match t {
            Ok(t) => t,
            Err(e) => {
                said.push(e.clone());
                continue;
            }
        };
        for (key, into) in [("language", &mut langs), ("grammar", &mut grammars), ("server", &mut servers)] {
            for (name, entry) in named(t, key, pack, &mut said) {
                match into.iter().any(|e| e.name == name) {
                    true => said.push(format!("the {pack} pack: [{key}.{name}] is in another pack too")),
                    false => into.push(Entry { name, pack: pack.clone(), t: entry, yours: false }),
                }
            }
        }
        for key in LISTS {
            if let Some(v) = t.get(*key) {
                add_list(&mut lists, key, v.clone());
            }
        }
    }
    for (pack, text) in yours {
        let t = match crate::packs::read_pack(pack, text, "yours") {
            Ok(t) => t,
            Err(e) => {
                said.push(e);
                continue;
            }
        };
        for key in t.keys().filter(|k| reach(k) == Some(Reach::BuiltIn)) {
            said.push(format!("your {pack} pack: `{key}` is read from tarae's own packs only"));
        }
        for (key, into) in [("language", &mut langs), ("grammar", &mut grammars), ("server", &mut servers)] {
            if reach(key) != Some(Reach::Yours) {
                continue;
            }
            for (name, mut entry) in named(&t, key, pack, &mut said) {
                if key == "grammar" {
                    for build_key in ["core", "dev", "bundle"] {
                        if entry.remove(build_key).is_some() {
                            said.push(format!(
                                "your {pack} pack: [grammar.{name}] `{build_key}` is what tarae's build embeds — \
                                 left out"
                            ));
                        }
                    }
                }
                match into.iter_mut().find(|e| e.name == name) {
                    // Languages and servers key by key; a grammar whole (its source is one thing)
                    Some(e) if key != "grammar" => {
                        e.t.extend(entry);
                        e.yours = true;
                    }
                    Some(e) => (e.t, e.pack, e.yours) = (entry, pack.clone(), true),
                    None => into.push(Entry { name, pack: pack.clone(), t: entry, yours: true }),
                }
            }
        }
        for key in LISTS {
            let Some(v) = t.get(*key) else { continue };
            match reach(key) {
                Some(Reach::Added) => add_list(&mut lists, key, v.clone()),
                Some(Reach::Yours) => {
                    lists.insert(key.to_string(), v.clone());
                }
                // (said above)
                Some(Reach::BuiltIn) | None => {}
            }
        }
    }

    let mut out = Languages::default();
    for e in &langs {
        match lang_of(e) {
            Ok(l) => out.langs.push(l),
            Err(err) => said.push(format!("the {} pack: [language.{}]: {err}", e.pack, e.name)),
        }
    }
    crate::sorting::by(&mut out.langs, |a, b| (!a.yours, &a.name).cmp(&(!b.yours, &b.name)));
    for e in &grammars {
        match grammar_of(e) {
            Ok((g, everywhere)) => {
                if everywhere {
                    out.everywhere.push(g.name.clone());
                }
                out.grammars.push(g);
            }
            Err(err) => said.push(format!("the {} pack: [grammar.{}]: {err}", e.pack, e.name)),
        }
    }
    for e in &servers {
        match server_of(e) {
            Ok((s, traits)) => {
                out.traits.insert(s.name.clone(), traits);
                out.servers.push(s);
            }
            Err(err) => said.push(format!("the {} pack: [server.{}]: {err}", e.pack, e.name)),
        }
    }
    let mut by_name: Vec<&LangSpec> = out.langs.iter().collect();
    by_name.sort_by(|a, b| a.name.cmp(&b.name));
    for l in by_name {
        union(&mut out.roots_any, &l.roots);
        union(&mut out.test_files, &l.test_files);
        union(&mut out.test_dirs, &l.test_dirs);
    }
    if let Err(e) = lists_of(&lists, &mut out) {
        said.push(e);
    }
    out.said = said;
    out
}

/// A pack's `[<key>.<name>]` entries, each a table (one that isn't is said and left out).
fn named(t: &Table, key: &str, pack: &str, said: &mut Vec<String>) -> Vec<(String, Table)> {
    let Some(v) = t.get(key) else { return Vec::new() };
    let Some(v) = v.as_table() else {
        said.push(format!("the {pack} pack: [{key}] isn't a table of names"));
        return Vec::new();
    };
    v.iter()
        .filter_map(|(name, e)| match e.as_table() {
            Some(e) => Some((name.clone(), e.clone())),
            None => {
                said.push(format!("the {pack} pack: [{key}.{name}] isn't a table"));
                None
            }
        })
        .collect()
}

/// `v` (a pack's `config-files` or `outputs`) added to what `lists` holds under `key`.
fn add_list(lists: &mut Table, key: &str, v: Value) {
    let mut wrapped = Table::new();
    wrapped.insert(key.into(), v);
    let mut all = Value::Table(std::mem::take(lists));
    crate::packs::add_value(&mut all, Value::Table(wrapped));
    if let Value::Table(t) = all {
        *lists = t;
    }
}

/// `config-files`' TOML and JSON names, and `outputs`, into `out`.
fn lists_of(lists: &Table, out: &mut Languages) -> Result<(), String> {
    if let Some(c) = lists.get("config-files") {
        let c = c.as_table().ok_or("`config-files` isn't a table")?;
        known(c, &["toml", "json"], "`config-files`")?;
        out.config_toml = strings(c.get("toml"), "config-files.toml")?.unwrap_or_default();
        out.config_json = strings(c.get("json"), "config-files.json")?.unwrap_or_default();
    }
    out.outputs = strings(lists.get("outputs"), "outputs")?.unwrap_or_default();
    Ok(())
}

fn union(into: &mut Vec<String>, more: &[String]) {
    for m in more {
        if !into.contains(m) {
            into.push(m.clone());
        }
    }
}

const LANGUAGE_KEYS: &[&str] = &[
    "extensions",
    "filenames",
    "path-suffixes",
    "globs",
    "grammar",
    "comment-token",
    "block-comment-tokens",
    "auto-pairs",
    "title",
    "aliases",
    "doc-comments",
    "indent-tabs",
    "indent-after-colon",
    "indent-heuristic",
    "prose",
    "language-id",
    "roots",
    "needs-project",
    "lsp",
    "also",
    "test-files",
    "test-dirs",
    "grammars-with",
];

fn lang_of(e: &Entry) -> Result<LangSpec, String> {
    let t = &e.t;
    known(t, LANGUAGE_KEYS, "it")?;
    let list = |k: &str| strings(t.get(k), k).map(Option::unwrap_or_default);
    let flag = |k: &str| match t.get(k) {
        None => Ok(false),
        Some(v) => v.as_bool().ok_or(format!("`{k}` is true or false")),
    };
    let block_comment = match t.get("block-comment-tokens") {
        None => None,
        Some(b) => {
            let b = b.as_table().ok_or("`block-comment-tokens` isn't a table")?;
            known(b, &["start", "end"], "`block-comment-tokens`")?;
            let need = |k: &str| string(b, k)?.ok_or(format!("`block-comment-tokens` has no `{k}`"));
            Some((need("start")?, need("end")?))
        }
    };
    let auto_pairs = match t.get("auto-pairs") {
        None => None,
        Some(_) => Some(
            list("auto-pairs")?
                .iter()
                .map(|p| {
                    let mut c = p.chars();
                    match (c.next(), c.next(), c.next()) {
                        (Some(a), Some(b), None) => Ok((a, b)),
                        _ => Err(format!("`auto-pairs`: {p:?} isn't two characters")),
                    }
                })
                .collect::<Result<_, _>>()?,
        ),
    };
    let name = e.name.clone();
    Ok(LangSpec {
        grammar: string(t, "grammar")?.unwrap_or_else(|| name.clone()),
        extensions: list("extensions")?,
        filenames: list("filenames")?,
        path_suffixes: list("path-suffixes")?,
        globs: list("globs")?,
        comment_token: string(t, "comment-token")?,
        block_comment,
        auto_pairs,
        title: string(t, "title")?.unwrap_or_else(|| capitalized(&name)),
        aliases: list("aliases")?,
        doc_comments: flag("doc-comments")?,
        indent_tabs: flag("indent-tabs")?,
        indent_after_colon: flag("indent-after-colon")?,
        simple_indent: match string(t, "indent-heuristic")?.as_deref() {
            None => false,
            Some("simple") => true,
            Some(h) => return Err(format!("`indent-heuristic`: {h:?} — only \"simple\" is known")),
        },
        prose: flag("prose")?,
        language_id: string(t, "language-id")?.unwrap_or_else(|| name.clone()),
        roots: list("roots")?,
        needs_project: list("needs-project")?,
        lsp: list("lsp")?,
        also: list("also")?,
        test_files: list("test-files")?,
        test_dirs: list("test-dirs")?,
        grammars_with: list("grammars-with")?,
        yours: e.yours,
        name,
    })
}

/// `go` → `Go`.
fn capitalized(name: &str) -> String {
    let mut c = name.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

fn grammar_of(e: &Entry) -> Result<(crate::grammar::Source, bool), String> {
    let t = &e.t;
    known(t, &["git", "rev", "subpath", "core", "dev", "bundle", "everywhere"], "it")?;
    let flag = |k: &str| t.get(k).is_some_and(|v| v.as_bool() == Some(true));
    let source = crate::grammar::Source {
        name: e.name.clone(),
        bundle: flag("bundle"),
        git: string(t, "git")?.ok_or("has no `git`")?,
        rev: string(t, "rev")?.ok_or("has no `rev`")?,
        subpath: string(t, "subpath")?,
    };
    Ok((source, flag("everywhere")))
}

/// Every key a `[server.<name>]` may have.
pub(crate) const SERVER_KEYS: &[&str] = &[
    "command",
    "args",
    "leave-out",
    "share",
    "quiescent",
    "symbol-params",
    "hide-lines",
    "args-add",
    "args-unless",
    "init",
    "progress",
    "virtual",
    "client-commands",
];

fn server_of(e: &Entry) -> Result<(ServerSpec, ServerTraits), String> {
    let t = &e.t;
    // (`leave-out`: read where the server starts — `packs::leave_out`)
    known(t, SERVER_KEYS, "it")?;
    let spec = ServerSpec {
        name: e.name.clone(),
        command: string(t, "command")?.unwrap_or_else(|| e.name.clone()),
        args: strings(t.get("args"), "args")?.unwrap_or_default(),
        except: Vec::new(),
    };
    Ok((spec, traits_of(t)?))
}

/// A `[server.<name>]`'s traits: `share`, `quiescent`, `symbol-params`, `hide-lines` (`lsp::ServerTraits`).
fn traits_of(t: &Table) -> Result<ServerTraits, String> {
    let table = |k: &str, keys: &[&str]| -> Result<Option<&Table>, String> {
        let Some(v) = t.get(k) else { return Ok(None) };
        let v = v.as_table().ok_or(format!("`{k}` isn't a table"))?;
        known(v, keys, &format!("`{k}`"))?;
        Ok(Some(v))
    };
    let share = match table("share", &["toolchain", "checks", "reload-progress"])? {
        None => None,
        Some(s) => Some(Share {
            toolchain: strings(s.get("toolchain"), "share.toolchain")?.unwrap_or_default(),
            checks: string(s, "checks")?,
            reload_progress: string(s, "reload-progress")?,
        }),
    };
    let quiescent = match table("quiescent", &["method", "capability", "field", "health", "message"])? {
        None => None,
        Some(q) => {
            let need = |k: &str| string(q, k)?.ok_or(format!("`quiescent` has no `{k}`"));
            Some(Quiescent {
                method: need("method")?,
                capability: string(q, "capability")?,
                field: need("field")?,
                health: string(q, "health")?,
                message: string(q, "message")?,
            })
        }
    };
    // (the server's own words: any keys)
    let symbol_params = match t.get("symbol-params") {
        None => serde_json::Value::Null,
        Some(v) if v.is_table() => serde_json::to_value(v).map_err(|e| format!("`symbol-params`: {e}"))?,
        Some(_) => return Err("`symbol-params` isn't a table".into()),
    };
    let hide_lines = strings(t.get("hide-lines"), "hide-lines")?
        .unwrap_or_default()
        .iter()
        .map(|r| regex::Regex::new(r).map_err(|e| format!("`hide-lines`: {e}")))
        .collect::<Result<_, _>>()?;
    let args_add = strings(t.get("args-add"), "args-add")?.unwrap_or_default();
    const ARG_VARS: [&str; 4] = ["cache", "root", "root-name", "root-hash"];
    for a in &args_add {
        let mut rest = a.as_str();
        while let Some(i) = rest.find('{') {
            let end = rest[i..].find('}').ok_or(format!("`args-add`: {a:?} has a `{{` not closed"))? + i;
            if !ARG_VARS.contains(&&rest[i + 1..end]) {
                return Err(format!(
                    "`args-add`: no `{}` (there are {{{}}})",
                    &rest[i..=end],
                    ARG_VARS.join("}, {")
                ));
            }
            rest = &rest[end + 1..];
        }
    }
    // (the server's own words: any keys)
    let init = match t.get("init") {
        None => serde_json::Value::Null,
        Some(v) if v.is_table() => serde_json::to_value(v).map_err(|e| format!("`init`: {e}"))?,
        Some(_) => return Err("`init` isn't a table".into()),
    };
    let progress = match table("progress", &["method", "kind", "text", "done", "done-text", "error"])? {
        None => None,
        Some(p) => {
            let need = |k: &str| string(p, k)?.ok_or(format!("`progress` has no `{k}`"));
            let done_text = string(p, "done-text")?
                .map(|r| regex::Regex::new(&r).map_err(|e| format!("`progress.done-text`: {e}")))
                .transpose()?;
            Some(Status {
                method: need("method")?,
                kind: need("kind")?,
                text: need("text")?,
                done: strings(p.get("done"), "progress.done")?.unwrap_or_default(),
                done_text,
                error: strings(p.get("error"), "progress.error")?.unwrap_or_default(),
            })
        }
    };
    let virtual_docs = match table("virtual", &["scheme", "fetch", "language", "title-join"])? {
        None => None,
        Some(v) => {
            let need = |k: &str| string(v, k)?.ok_or(format!("`virtual` has no `{k}`"));
            Some(Virtual {
                scheme: need("scheme")?,
                fetch: need("fetch")?,
                language: need("language")?,
                title_join: string(v, "title-join")?,
            })
        }
    };
    let mut client_commands = Vec::new();
    if let Some(c) = t.get("client-commands") {
        for (name, what) in c.as_table().ok_or("`client-commands` isn't a table")? {
            let action = match what.as_str() {
                Some("apply-edit") => ClientAction::ApplyEdit,
                _ => return Err(format!("`client-commands.{name}`: what tarae does is \"apply-edit\"")),
            };
            client_commands.push((name.clone(), action));
        }
    }
    Ok(ServerTraits {
        share,
        quiescent,
        symbol_params,
        hide_lines,
        args_add,
        args_unless: string(t, "args-unless")?,
        init,
        progress,
        virtual_docs,
        client_commands,
    })
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::path::Path;

    /// Every language as tarae knows it — recognised, commented, indented, parsed, served, rooted — and the
    /// lists beside them, one text: pinned in `src/snapshots/languages.txt`, so moving where it's said
    /// can't change what's said.
    fn render() -> String {
        let mut s = String::new();
        let config = crate::lsp::LspConfig::default();
        let strings = |v: &[String]| v.to_vec();
        let any = strings(crate::lsp::markers_for("no such language"));
        let line = |s: &mut String, key: &str, items: &[String]| {
            if !items.is_empty() {
                writeln!(s, "  {key}: {}", items.join(" ")).unwrap();
            }
        };
        let server = |l: &crate::lsp::ServerSpec| match l.args.is_empty() {
            true => format!("{} ({})", l.name, l.command),
            false => format!("{} ({} {})", l.name, l.command, l.args.join(" ")),
        };
        let mut names: Vec<&str> = crate::syntax::names().collect();
        names.sort();
        for n in names {
            let l = crate::syntax::spec(n).unwrap();
            writeln!(s, "[{n}] {}", crate::grammar::pretty(n)).unwrap();
            line(&mut s, "grammar", std::slice::from_ref(&l.grammar));
            line(&mut s, "extensions", &l.extensions);
            line(&mut s, "filenames", &l.filenames);
            line(&mut s, "path-suffixes", &l.path_suffixes);
            line(&mut s, "globs", &l.globs);
            let mut comment: Vec<String> = l.comment_token.iter().cloned().collect();
            if let Some((a, b)) = &l.block_comment {
                comment.push(format!("{a} {b}"));
            }
            line(&mut s, "comments", &comment);
            let pairs: Option<Vec<String>> =
                l.auto_pairs.as_ref().map(|p| p.iter().map(|(a, b)| format!("{a}{b}")).collect());
            line(&mut s, "pairs", &pairs.unwrap_or_default());
            let flags = [
                ("doc-comments", crate::doccomment::applies(Some(n))),
                ("tabs", crate::syntax::tabs_by_default(Some(n))),
                ("colon", crate::syntax::colon_indents(Some(n))),
                ("simple-indent", l.simple_indent),
                ("prose", crate::syntax::prose(Some(n))),
            ];
            let flags: Vec<String> = flags.iter().filter(|f| f.1).map(|f| f.0.to_string()).collect();
            line(&mut s, "flags", &flags);
            if crate::lsp::language_id(n) != n {
                line(&mut s, "language-id", &[crate::lsp::language_id(n).to_string()]);
            }
            let roots = strings(crate::lsp::markers_for(n));
            if roots != any {
                line(&mut s, "roots", &roots);
            }
            line(&mut s, "needs-project", &strings(crate::lsp::project_markers(n).unwrap_or_default()));
            let lsp: Vec<String> = crate::lsp::candidates(n, &config).iter().map(server).collect();
            line(&mut s, "lsp", &lsp);
            let also = crate::lsp::also_names(n, &config);
            let also: Vec<String> =
                also.iter().map(|a| crate::lsp::default_spec(a).map_or(a.clone(), |l| server(&l))).collect();
            line(&mut s, "also", &also);
            line(&mut s, "installs", &crate::grammar::for_language(n));
            if let Some(d) = crate::debugpack::read(n) {
                line(&mut s, "debug", &[d.map_or_else(|e| format!("broken: {e}"), |d| d.summary())]);
            }
        }
        let mut any = any;
        any.sort();
        writeln!(s, "\n[any language]\n  roots: {}", any.join(" ")).unwrap();

        // What servers speak beyond the protocol (`lsp::ServerTraits`) — those that speak something
        writeln!(s, "\n[servers]").unwrap();
        let table = super::get();
        let mut names: Vec<&String> = table.traits.keys().collect();
        names.sort();
        for n in names {
            let t = &table.traits[n];
            let mut said = Vec::new();
            if let Some(sh) = &t.share {
                let opt = |k: &str, v: &Option<String>| {
                    v.as_ref().map(|v| format!(" · {k} {v}")).unwrap_or_default()
                };
                said.push(format!(
                    "share: toolchain {}{}{}",
                    sh.toolchain.join(" "),
                    opt("checks", &sh.checks),
                    opt("reload-progress", &sh.reload_progress)
                ));
            }
            if let Some(q) = &t.quiescent {
                let fields = [Some(&q.field), q.health.as_ref(), q.message.as_ref()];
                let fields: Vec<&str> = fields.into_iter().flatten().map(String::as_str).collect();
                let cap = q.capability.as_ref().map(|c| format!(" (asked: {c})")).unwrap_or_default();
                said.push(format!("quiescent: {}{cap} · {}", q.method, fields.join(" ")));
            }
            if !t.symbol_params.is_null() {
                said.push(format!("symbol-params: {}", t.symbol_params));
            }
            if !t.hide_lines.is_empty() {
                let lines: Vec<&str> = t.hide_lines.iter().map(regex::Regex::as_str).collect();
                said.push(format!("hide-lines: {}", lines.join("  ")));
            }
            if !t.args_add.is_empty() {
                let unless = t.args_unless.as_ref().map(|u| format!(" (unless {u})")).unwrap_or_default();
                said.push(format!("args-add: {}{unless}", t.args_add.join(" ")));
            }
            if !t.init.is_null() {
                said.push(format!("init: {}", t.init));
            }
            if let Some(p) = &t.progress {
                let done_text =
                    p.done_text.as_ref().map(|r| format!(" or {}", r.as_str())).unwrap_or_default();
                said.push(format!(
                    "progress: {} · {} {} · done {}{done_text} · error {}",
                    p.method,
                    p.kind,
                    p.text,
                    p.done.join(" "),
                    p.error.join(" ")
                ));
            }
            if let Some(v) = &t.virtual_docs {
                let join =
                    v.title_join.as_ref().map(|j| format!(" · titles joined by {j:?}")).unwrap_or_default();
                said.push(format!("virtual: {} by {} in {}{join}", v.scheme, v.fetch, v.language));
            }
            for (cmd, action) in &t.client_commands {
                said.push(format!("client-command: {cmd} → {action:?}"));
            }
            if !said.is_empty() {
                writeln!(s, "  {n}").unwrap();
                for l in said {
                    writeln!(s, "    {l}").unwrap();
                }
            }
        }

        writeln!(s, "\n[debuggers]").unwrap();
        let mut debuggers: Vec<String> =
            crate::packs::tables_of("debugger").iter().flat_map(|(_, t)| t.keys().cloned()).collect();
        debuggers.sort();
        for d in debuggers {
            let said =
                crate::debugpack::debugger(&d).map_or_else(|e| format!("broken: {e}"), |d| d.summary());
            writeln!(s, "  {said}").unwrap();
        }

        // Tools' build folders (`builds.rs`) — the built-in packs' only
        writeln!(s, "\n[builds]").unwrap();
        for (pack, _, t) in crate::builds::Builds::of(crate::packs::builtin_manifests()) {
            let said = t.map_or_else(|e| format!("broken: {e}"), |t| t.summary());
            writeln!(s, "  {pack}: {said}").unwrap();
        }

        writeln!(s, "\n[grammars]").unwrap();
        let mut grammars = crate::grammar::sources();
        grammars.sort_by(|a, b| a.name.cmp(&b.name));
        for g in grammars {
            let sub = g.subpath.map(|p| format!(" {p}")).unwrap_or_default();
            let bundle = if g.bundle { " bundle" } else { "" };
            writeln!(s, "  {} {} {}{sub}{bundle}", g.name, g.git, g.rev).unwrap();
        }

        writeln!(s, "\n[resolve]").unwrap();
        for n in [
            "sh",
            "shell",
            "zsh",
            "console",
            "shellsession",
            "golang",
            "c++",
            "cxx",
            "py",
            "python3",
            "py3",
            "node",
            "js",
            "mjs",
            "cjs",
            "yml",
            "jsonc",
            "json5",
            "cs",
            "csharp",
            "kt",
            "hs",
            "rb",
            "text",
            "txt",
            "plain",
            "plaintext",
            "",
            "rs",
            "ts",
            "tsx",
            "md",
            "Python3",
            "{.rust}",
            "dockerfile",
            "make",
            "toml",
            "h",
            "go",
            "proto",
            "tf",
            "no-such-language",
        ] {
            let to = crate::syntax::resolve(n).map_or("-", |l| l.name.as_str());
            writeln!(s, "  {n:?} {to}").unwrap();
        }

        writeln!(s, "\n[config files]").unwrap();
        let broken = ropey::Rope::from_str("= [");
        for p in [
            "Cargo.toml",
            "rust-toolchain.toml",
            "pyproject.toml",
            "rustfmt.toml",
            ".rustfmt.toml",
            "clippy.toml",
            ".clippy.toml",
            "deny.toml",
            ".tarae.toml",
            ".tarae.local.toml",
            "Pipfile",
            "package.json",
            "composer.json",
            ".claude.json",
            "p/.cargo/config.toml",
            "p/.cargo/config",
            "p/.cargo/credentials.toml",
            "p/cargo/config.toml",
            "h/.config/tarae/config.toml",
            "h/.config/tarae/themes/x.toml",
            "p/.claude/settings.local.json",
            "p/.claude/other.json",
            "tsconfig.json",
            "go.mod",
            "config.toml",
            "pom.xml",
        ] {
            let checked = crate::disk::unparsed_config(Path::new(p), &broken).is_some();
            writeln!(s, "  {p} {}", if checked { "checked" } else { "-" }).unwrap();
        }

        writeln!(s, "\n[skipped folders]").unwrap();
        for n in [".git", "objects", "target", "node_modules", ".venv", "__pycache__", "build", "dist", "src"]
        {
            let by: Vec<&str> = [
                ("picker", crate::picker::skips(n)),
                ("tokens", crate::tokens::output_dir(n)),
                ("nested-checkouts", crate::lsp::nested_skips(n)),
            ]
            .iter()
            .filter(|b| b.1)
            .map(|b| b.0)
            .collect();
            writeln!(s, "  {n} {}", if by.is_empty() { "-".to_string() } else { by.join(" ") }).unwrap();
        }

        writeln!(s, "\n[made first on Linux: pack — beside — what]").unwrap();
        for m in crate::packs::made_beside() {
            let made: Vec<String> =
                m.folders.iter().map(|f| format!("{f}/")).chain(m.files.clone()).collect();
            writeln!(s, "  {} — {} — {}", m.pack, m.protect.join(" "), made.join(" ")).unwrap();
        }

        writeln!(s, "\n[test files by name]").unwrap();
        for p in [
            "src/a_test.go",
            "src/a.go",
            "test_x.go",
            "tests/x.rs",
            "src/a.rs",
            "src/test/A.java",
            "pkg/test_a.py",
            "pkg/a_test.py",
            "pkg/a.py",
            "spec/a_spec.rb",
            "lib/a_spec.rb",
            "lib/a_test.rb",
            "lib/a.rb",
            "src/ATest.java",
            "src/ATests.kt",
            "src/AT.scala",
            "src/FooTest.cs",
            "src/a.test.ts",
            "src/a.spec.jsx",
            "src/a.test.mjs",
            "src/a.test.cjs",
            "src/a.spec.tsx",
            "src/__tests__/a.js",
            "specs/x.c",
        ] {
            let test = crate::testcode::by_name(Path::new(p));
            writeln!(s, "  {p} {}", if test { "test" } else { "-" }).unwrap();
        }
        s
    }

    /// The language table is said in the packs; what it says is pinned (`TARAE_BLESS=1` rewrites it).
    #[test]
    fn the_language_table_stays() {
        crate::settings::check_generated(
            "src/snapshots/languages.txt",
            &render(),
            "the_language_table_stays",
        );
    }

    use super::*;
    use crate::syntax::detect_in;

    fn yours(packs: &[(&str, &str)]) -> Languages {
        let packs: Vec<(String, String)> =
            packs.iter().map(|(p, t)| (p.to_string(), t.to_string())).collect();
        build(crate::packs::builtin_manifests(), &packs)
    }

    fn lang<'a>(t: &'a Languages, path: &str) -> Option<&'a str> {
        detect_in(&t.langs, Path::new(path)).map(|l| l.name.as_str())
    }

    /// Detection goes tier by tier and takes the first that claims a path — so among the built-in packs
    /// nothing may be claimed twice, or which one wins would be the packs' order. And the packs read without
    /// a word: every language's grammar and servers are there, every grammar and server said once.
    #[test]
    fn no_two_languages_claim_one_file() {
        let t = get();
        assert_eq!(t.said, Vec::<String>::new(), "the built-in packs read cleanly");
        assert!(t.langs.len() >= 56, "{}", t.langs.len());
        type Keys = fn(&LangSpec) -> &[String];
        let tiers: [(&str, Keys); 5] = [
            ("extension", |l| &l.extensions),
            ("file name", |l| &l.filenames),
            ("path suffix", |l| &l.path_suffixes),
            ("glob", |l| &l.globs),
            ("alias", |l| &l.aliases),
        ];
        for (tier, keys) in tiers {
            let mut seen: Vec<(&str, &str)> = Vec::new();
            for l in &t.langs {
                for k in keys(l) {
                    if let Some((_, other)) = seen.iter().find(|(s, _)| s == k) {
                        panic!("{tier} {k:?}: both {other} and {}", l.name);
                    }
                    seen.push((k, &l.name));
                }
            }
        }
        // An alias that's a language's name or an extension would be ambiguous in a code fence
        for l in &t.langs {
            for a in &l.aliases {
                assert!(crate::syntax::spec(a).is_none(), "{}: alias {a} is a language", l.name);
                assert!(
                    t.langs.iter().all(|o| !o.extensions.contains(a)),
                    "{}: alias {a} is an extension",
                    l.name
                );
            }
        }
        for l in &t.langs {
            for g in std::iter::once(&l.grammar).chain(&l.grammars_with) {
                assert_eq!(t.grammars.iter().filter(|s| s.name == *g).count(), 1, "{}: grammar {g}", l.name);
            }
            for s in l.lsp.iter().chain(&l.also) {
                assert!(t.servers.iter().any(|d| d.name == *s), "{}: no [server.{s}]", l.name);
            }
        }
        assert_eq!(t.everywhere, ["comment"]);
    }

    /// A runtime folder's pack adds a language, changes one key by key (wherever the built-in one lives), wins
    /// a tie, points a server elsewhere; a broken one is said and the built-in packs stay.
    #[test]
    fn a_runtime_pack_adds_and_changes_languages() {
        let t = yours(&[
            (
                "mine",
                "[language.mylang]\nextensions = [\"myl\"]\ncomment-token = \";\"\n\
                 [grammar.mylang]\ngit = \"https://example.com/tree-sitter-mylang\"\nrev = \"abc\"\n",
            ),
            ("headers", "[language.cpp]\nextensions = [\"cc\", \"cpp\", \"h\"]\n"),
            ("rust", "[server.rust-analyzer]\ncommand = \"/opt/ra/rust-analyzer\"\n"),
            ("broken", "[language.x\n"),
            ("typo", "[languag.x]\n"),
            ("badkey", "[language.y]\nextension = [\"y\"]\n"),
        ]);
        assert_eq!(lang(&t, "a.myl"), Some("mylang"));
        assert!(t.grammars.iter().any(|g| g.name == "mylang"));
        let cpp = t.langs.iter().find(|l| l.name == "cpp").unwrap();
        assert!(cpp.yours);
        assert_eq!(cpp.comment_token.as_deref(), Some("//"), "the keys it didn't set stay");
        assert_eq!(cpp.title, "C++");
        assert_eq!(lang(&t, "a.h"), Some("cpp"), "a runtime folder's language wins the tie");
        assert_eq!(lang(&t, "a.c"), Some("c"));
        let ra = t.servers.iter().find(|s| s.name == "rust-analyzer").unwrap();
        assert_eq!(ra.command, "/opt/ra/rust-analyzer");
        assert!(t.traits["rust-analyzer"].share.is_some(), "the keys it didn't set stay");
        let said = t.said.join("\n");
        assert!(said.contains("the broken pack (yours)"), "{said}");
        assert!(said.contains("the typo pack (yours) has no `languag`"), "{said}");
        assert!(said.contains("[language.y]: it has no `extension`"), "{said}");
        assert_eq!(lang(&t, "a.rs"), Some("rust"), "the built-in packs stay");
        assert_eq!(t.langs.len(), get().langs.len() + 1);
    }

    /// A server's traits from a runtime folder's pack: one key in place of the built-in one (a table whole —
    /// `share` here), the others kept; a server of one's own gets its own; what can't be read is said and that
    /// server is left out.
    #[test]
    fn a_runtime_pack_says_what_a_server_speaks() {
        let t = yours(&[
            ("rust", "[server.rust-analyzer]\nshare = { toolchain = [\"mine.toml\"] }\n"),
            (
                "mine",
                "[server.myls]\nquiescent = { method = \"my/ready\", field = \"done\" }\n\
                 symbol-params = { all = true }\nhide-lines = ['^note:']\n",
            ),
            ("typo", "[server.a]\nshare = { toolchian = [] }\n"),
            ("noway", "[server.b]\nquiescent = { field = \"x\" }\n"),
            ("badre", "[server.c]\nhide-lines = ['(']\n"),
            ("badarg", "[server.d]\nargs-add = [\"{home}/x\"]\n"),
            ("badcmd", "[server.e]\nclient-commands = { \"x.run\" = \"run\" }\n"),
            ("nofetch", "[server.f]\nvirtual = { scheme = \"x:\", language = \"x\" }\n"),
            ("noinit", "[server.g]\ninit = 3\n"),
        ]);
        let ra = &t.traits["rust-analyzer"];
        let share = ra.share.as_ref().unwrap();
        assert_eq!(share.toolchain, ["mine.toml"]);
        assert_eq!(share.checks, None, "`share` is one key: replaced whole");
        assert!(ra.quiescent.is_some() && !ra.hide_lines.is_empty(), "its other keys stay");
        let my = &t.traits["myls"];
        let q = my.quiescent.as_ref().unwrap();
        assert_eq!((q.method.as_str(), q.field.as_str(), q.capability.as_ref()), ("my/ready", "done", None));
        assert_eq!(my.symbol_params, serde_json::json!({ "all": true }));
        assert_eq!(my.shown_message("unused\nnote: from here"), "unused");
        let said = t.said.join("\n");
        assert!(said.contains("[server.a]: `share` has no `toolchian`"), "{said}");
        assert!(said.contains("[server.b]: `quiescent` has no `method`"), "{said}");
        assert!(said.contains("[server.c]: `hide-lines`"), "{said}");
        assert!(said.contains("[server.d]: `args-add`: no `{home}`"), "{said}");
        assert!(said.contains("[server.e]: `client-commands.x.run`"), "{said}");
        assert!(said.contains("[server.f]: `virtual` has no `fetch`"), "{said}");
        assert!(said.contains("[server.g]: `init` isn't a table"), "{said}");
        for gone in ["a", "b", "c", "d", "e", "f", "g"] {
            assert!(!t.traits.contains_key(gone) && !t.servers.iter().any(|s| s.name == gone), "{gone}");
        }
    }

    /// What widens what Claude's runs may do comes from tarae's own packs only; what narrows can be added
    /// to, never taken from.
    #[test]
    fn a_runtime_pack_cant_widen_or_take_out() {
        let t = yours(&[(
            "rust",
            "outputs = [\"src\"]\nprotect = []\nconfig-files = { toml = [\"mine.toml\"] }\n\
             [grammar.rust]\ngit = \"https://example.com/r\"\nrev = \"abc\"\ncore = true\n",
        )]);
        assert!(!t.outputs.contains(&"src".to_string()), "{:?}", t.outputs);
        assert!(t.outputs.contains(&"target".to_string()));
        for f in ["Cargo.toml", ".cargo/config.toml", "mine.toml"] {
            assert!(t.config_toml.iter().any(|c| c == f), "{f}: {:?}", t.config_toml);
        }
        let said = t.said.join("\n");
        assert!(said.contains("`outputs` is read from tarae's own packs only"), "{said}");
        assert!(said.contains("`protect` is read from tarae's own packs only"), "{said}");
        assert!(said.contains("[grammar.rust] `core`"), "{said}");
        let rust = t.grammars.iter().find(|g| g.name == "rust").unwrap();
        assert_eq!(rust.git, "https://example.com/r", "re-pinned — fetched only after a yes");
        // The per-folder reader (tests, tasks, servers' `leave-out`) goes by the same reach
        let builtin = crate::runtime::pack_builtin("rust/pack.toml");
        let user = "outputs = [\"src\"]\nconfig-files = { toml = [] }\n";
        let m = crate::packs::merged("rust", builtin, Some(user.into())).unwrap().unwrap();
        assert_eq!(m["outputs"].as_array().unwrap().len(), 1, "{m:?}");
        assert!(
            m["config-files"]["toml"].as_array().unwrap().iter().any(|v| v.as_str() == Some("Cargo.toml"))
        );
    }

    /// The table costs one start a fraction of a millisecond: the built-in packs read and put together — a
    /// number for `cargo perf` (the median of 21).
    #[test]
    fn perf_language_table() {
        if crate::test_results::unoptimized("languages::tests::perf_language_table") {
            return;
        }
        let texts: Vec<(&str, &str)> = crate::runtime::pack_manifests_builtin().collect();
        let mut took: Vec<std::time::Duration> = (0..21)
            .map(|_| {
                let t = std::time::Instant::now();
                let read: Vec<(String, Result<Table, String>)> = texts
                    .iter()
                    .map(|(p, s)| (p.to_string(), crate::packs::read_pack(p, s, "built in")))
                    .collect();
                std::hint::black_box(build(&read, &[]));
                t.elapsed()
            })
            .collect();
        took.sort();
        let median = took[took.len() / 2];
        let ms = median.as_secs_f64() * 1e3;
        let line = format!("PERF language_table {ms:.3} budget=5 noise=0.1\n");
        let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
        assert!(ms < 5.0, "the language table took {median:?}");
    }
}
