//! Claude's sandbox and allowances as the packs say them (core audit E1–E13, 2026-10-06): what a toolchain
//! needs from a sandboxed run, and what Claude may run and read unasked for it. Read once
//! (`get`), from `[sandbox]` and `[claude]`; `sandbox.rs` (the policy both systems' rules are made from — the
//! Seatbelt profile, Landlock's rules, where a run writes), `tokens.rs`, `allow.rs`, `readonly.rs`, `tools.rs`
//! and `sinceyes.rs` read it. No language's name is in the code that reads it.
//!
//! **Each key has a reach** (`SANDBOX_KEYS`, `CLAUDE_KEYS`; `packs::Reach::Keys`): what widens what Claude's
//! runs may do — read, write, set, run, be told — is read from the built-in packs only, from the binary; a
//! runtime folder's is said and left out (`languages::build`). What narrows (`kept`, `builders`) a runtime
//! folder's pack adds to, and the core's own floor stays under both (`sandbox::KEPT_FLOOR`,
//! `sinceyes::BUILDERS_FLOOR`): nothing configured takes one out. The deny lists stay in the core.
//!
//! `[sandbox]`:
//! - `homes.<name> = { env, else }`: a tool's home — `env` when it's set to an absolute path, else `else`
//!   (`~/…`); `{<name>}` in the places below. A place that is a home alone counts only where it's there.
//! - `reads`: places in home a run reads, whole. `reads-above`: files a run reads in every folder between the
//!   project and home (`.cargo/config.toml`).
//! - `kept`: places a run never reads, whatever is granted — at a home's default place and where its variable
//!   moved it.
//! - `caches = { home, dirs, files, xattr, rule, said }`: the tool's shared caches, in its home — or `asked`
//!   for `home` and `dirs`: the folder an `ask`'s `caches` key named, by default at `places` or where the
//!   variable `var` points. Wherever they are by default or where a home moved them, no grant nor asked folder
//!   opens them (`sandbox::RunsLater`), project or not. Written only with the network open;
//!   `files` (its locks, its database) always; `xattr` the one attribute it may set on them (Seatbelt's only);
//!   `rule` how the rules say them, `said` what Claude reads when a write there was kept (`{caches}`: where
//!   they are).
//! - `write-files`: single files a run writes; `{a}`, `{b}`… each a folder that's there, the same name for
//!   the same letter.
//! - `ask = [{ when, run, json, caches, kept }]`: what the toolchain names, asked in the project inside the
//!   sandbox it's for, with the packs' `env` — by JSON key, the folders it writes outside it (`json`), its
//!   shared caches (`caches`, said by a `[sandbox.caches] asked`), what no run reads (`kept`).
//! - `builds-said = { who, target, build }`: how the sandbox names the `[builds]` folders it was asked.
//! - `run-home.<name> = { env, keep, when, make, link, set, append, daemon, fresh, said }`: a home of the
//!   run's own for the tool named, made in the run's folder (`{run}/<name>`) — or, `keep`, kept across the
//!   project's runs in tarae's cache folder (`run-homes/<name>/<the project's>`) where it has one of `when` —
//!   `env` pointing at it: folders made in it, links from it into the tool's home (read only), variables `set`
//!   (`{ value, if }`: when `if` is there) and `append`ed to the run's own (`{home}`: this home's folder);
//!   `daemon = { make, append }` (kept ones): the tool leaves a daemon serving later runs from it — kept per
//!   project *and policy* where tarae ends what runs leave (Linux; elsewhere the run's own), and that made and
//!   appended then; `fresh = { append }`: appended only to a home of the run's own; `said` what Claude reads
//!   when a write in the user's home of the tool was kept.
//!   `proxy = { file, lines }`: a file in the home that's tarae's whole, written for a run with a proxy URL
//!   (`sandbox::Policy::proxy_url`) and removed for one without — `{proxy.<value>}` in its lines
//!   (`proxy::URL_VALUES`; a line whose value is empty for the run is left out). A run whose URL is its own
//!   alone (a port and token of its own) gets the run's own home, never a kept one (step 5f).
//! - `proxy = { env, append, files }`: what a run with a proxy URL gets besides the core's proxy variables —
//!   variables set and appended to the run's own (no `{proxy.…}`: a variable may end up on a command line), and
//!   `files."<name>" = { lines, append }`: a file in the run's own folder, `{proxy.<value>}` in its lines, `{file}`
//!   (its path) in its `append`.
//! - `registries`: the hosts the pack's tools fetch from, as `llm.sandbox-hosts` writes them — words only, never
//!   a grant: the host card names them (Q6).
//! - `env`: variables in every run's environment (`{run}`: the run's own folder) — or `{ value, on }`: only where
//!   the fact `on` names holds on the system tarae runs on or for the run's policy (`On`: `v4-mapped-refused`,
//!   `each-host-asked`).
//! - `pass`: what of the user's environment a run gets besides the core's floor (`sandbox::runenv`) — the
//!   toolchain's own variables: names, or `PREFIX*` families (two characters at least before the `*`); never
//!   the loader's (`LD_*`, `DYLD_*`), nor a name only a secret has (`NPM_TOKEN`). A token-like one is left out
//!   all the same.
//! - `bins`: where a toolchain puts programs the user runs — never written, nor what holds them (with the
//!   folders on `PATH`, `~/.local/bin`, and the homes themselves: `sandbox::RunsLater`).
//! - `quiet-writes`: where a denied write is expected — matched only at that place, for the user's home on
//!   the system tarae runs on (`sandbox::quiet_places`).
//! - `[[sandbox.settings]]`: a tool's settings that can hold a token (`tokens.rs`): `tool`, `var` (what points
//!   it at a file — `var-also` too; `off`: the value that means none; `var-if-there`: only when that file is
//!   there), `files` (in the order it reads them; `{ first = [folders], file }`: in the first folder that's
//!   there, else the last), `form` (`lines`, `ini`), `copy` (the copy's name in the run's folder); or no
//!   `copy` and `tell`: it takes none — the user is told once (`{them}`, the files).
//! - `refused-port = { process, said }`: what Claude reads when that process was refused a port here.
//!
//! `[claude]`:
//! - `run-allow`: commands Claude runs unasked, by whole words (git's read-only ones are the core's).
//! - `code-trees`: folders of code Claude reads unasked (`*`: every folder there).
//! - `expand.<word> = { run, env, first-word-in }`: a `$(…)` a command line may hold, filled in where it's
//!   judged with the folder `run`'s first word names directly in `first-word-in`.
//! - `builders`: programs that build or test the project (a script calling one runs its code).
//! - `[[claude.warn]]`: what a run card warns of, in the pack's words (`cardwarn.rs`); `wrappers`: programs that
//!   run what follows their words (`timeout`), a warning's command found after one.
//! - `time-limits = [{ run, option-values, grace }]`: programs that end what they run after a time their next
//!   word gives (`timeout 2700 …`; `grace`: an option whose value is added — `-k 5`) — a run of Claude's under one
//!   isn't asked whether it's stuck before then (`stuck::limit`).
//! - `entry = [{ run, files }]`: what a command runs that its words don't name — a line with `run`'s words in
//!   a row (after a wrapper too: `timeout 1800 cargo xtask`) runs the project's `files` (folders end in `/`):
//!   the card for it again says whether they changed since its last answer, and shows how (`sinceyes.rs`).
//! - `test-filters`: commands that take test names after their `--` (`cargo test`) — `a` on one, with `test`
//!   before its `--`, covers other test names there (`tools::Again`, #210); never a command whose words after
//!   `--` are its own (`cargo run`).
//! - `said = { rank, run, code, tests, protect }`: the pack's words in Claude's prompt and tools, joined by
//!   `rank` (lower first).
//!
//! A place is written from home (`~/…`) or a variable — `{<home>}`, `{user-config}` (macOS's `~/Library/
//! Application Support`, else `$XDG_CONFIG_HOME` or `~/.config`), `{user-cache}` (`~/Library/Caches`, else
//! `$XDG_CACHE_HOME` or `~/.cache`), `{xdg-config}` (`$XDG_CONFIG_HOME` or `~/.config`, anywhere), `{run}` —
//! with no `..`. A built-in pack's entry that widens past what the core allows — home itself or above it,
//! keys (`allow::SECRETS`), Claude's or tarae's own, a write where `allow::NEVER_WRITE` is, a variable
//! `ENV_NEVER` names, a lone program that runs what follows it — is left out and logged
//! (`every_built_in_table_reads_clean`).

use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use toml::{Table, Value};

use crate::packs::{Reach, known, list_in, need, string, strings};
use crate::sandbox::Os;

/// `[sandbox]`'s keys and how a runtime folder's pack counts for each.
pub(crate) const SANDBOX_KEYS: &[(&str, Reach)] = &[
    ("homes", Reach::BuiltIn),
    ("reads", Reach::BuiltIn),
    ("reads-above", Reach::BuiltIn),
    // (narrows)
    ("kept", Reach::Added),
    ("caches", Reach::BuiltIn),
    ("write-files", Reach::BuiltIn),
    ("ask", Reach::BuiltIn),
    ("builds-said", Reach::BuiltIn),
    ("run-home", Reach::BuiltIn),
    ("env", Reach::BuiltIn),
    // (what of the user's environment a run gets)
    ("pass", Reach::BuiltIn),
    // (narrows: never written)
    ("bins", Reach::Added),
    // (it hides a denial from the report)
    ("quiet-writes", Reach::BuiltIn),
    // (tarae reads and copies those files into the run's folder)
    ("settings", Reach::BuiltIn),
    // (what Claude reads)
    ("refused-port", Reach::BuiltIn),
    // (what a run with a proxy URL is told: where its tools go out)
    ("proxy", Reach::BuiltIn),
    // (what the user reads on the host card)
    ("registries", Reach::BuiltIn),
    // (narrows — a grant of one is never a chat's — but its words are on the card: built-in only)
    ("engine-sockets", Reach::BuiltIn),
    // (widens: files never looked in for a token)
    ("not-settings", Reach::BuiltIn),
];

/// `[claude]`'s keys and how a runtime folder's pack counts for each.
pub(crate) const CLAUDE_KEYS: &[(&str, Reach)] = &[
    ("run-allow", Reach::BuiltIn),
    ("code-trees", Reach::BuiltIn),
    ("expand", Reach::BuiltIn),
    // (narrows: a script calling one is said to run the project's code)
    ("builders", Reach::Added),
    // (narrows: more of what a command runs is shown on its card again)
    ("entry", Reach::Added),
    // (widens: `a` on one covers other test names after its `--` — #210)
    ("test-filters", Reach::BuiltIn),
    // (narrows: a card says more — and with `asks`, `a` lets less through; never less said)
    ("warn", Reach::Added),
    ("wrappers", Reach::Added),
    // (a run under one isn't asked whether it's stuck before its limit: built-in only — it holds a card back)
    ("time-limits", Reach::BuiltIn),
    // (Claude's prompt)
    ("said", Reach::BuiltIn),
    // (what a switch is said to cost you — a runtime folder's prices would be its words, not the price list's)
    ("price", Reach::BuiltIn),
];

/// A tool's home.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Home {
    pub name: String,
    pub env: String,
    pub default: String,
    /// Its variable may be a list of folders (`GOPATH=/a:/b`, the system's separator): the home is the first,
    /// every one is refused like a home. Only where the pack says so — no other variable is split.
    pub list: bool,
}

/// A tool's shared caches (`caches`): `dirs` in its `home`, or the folder an `ask`'s `caches` key named
/// (`asked`) — by default at `places`, or where the variable `var` points.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Caches {
    pub home: String,
    pub dirs: Vec<String>,
    pub asked: Option<String>,
    pub places: Vec<String>,
    pub var: Option<String>,
    pub files: Vec<String>,
    pub xattr: Option<String>,
    pub rule: String,
    pub said: String,
    /// The pack it's of: a host grant for one of that pack's `registries` opens it to the runs under it (piece C).
    pub pack: String,
}

/// What a toolchain is asked (`ask`): by JSON key, the folders it writes (`json`), its shared caches
/// (`caches`), and what no run reads (`kept`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ask {
    pub when: Vec<String>,
    pub run: Vec<String>,
    pub json: Vec<String>,
    pub caches: Vec<String>,
    pub kept: Vec<String>,
}

impl Ask {
    /// Every key it reads from the answer.
    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.json.iter().chain(&self.caches).chain(&self.kept)
    }
}

/// How the `[builds]` folders of its pack's are named (`builds-said`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildsSaid {
    pub pack: String,
    pub who: String,
    pub target: String,
    pub build: String,
}

/// A run's own home for a tool (`run-home`) — kept across the project's runs (`keep`) where it has one of
/// `when` (none: every project).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunHome {
    pub name: String,
    pub env: String,
    pub keep: bool,
    pub when: Vec<String>,
    pub make: Vec<String>,
    pub link: Vec<(String, String)>,
    /// (variable, value, only when this is there)
    pub set: Vec<(String, String, Option<String>)>,
    /// Appended to the run's own value, whatever the home (`{home}`: the home's folder).
    pub append: Vec<(String, String)>,
    /// The tool leaves a daemon serving later runs from it (`daemon`): kept per project and policy where tarae
    /// ends what runs leave — and this made and appended then.
    pub daemon: Option<Daemon>,
    /// Appended only to a home of the run's own (`fresh` — not kept).
    pub fresh: Vec<(String, String)>,
    /// A file in it that's tarae's whole, its lines for a run with a proxy URL (`proxy`).
    pub proxy: Option<ProxyLines>,
    pub said: String,
}

/// A file's lines for a run with a proxy URL: `{proxy.<value>}` in them (`proxy::URL_VALUES`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProxyLines {
    /// Its name, in a run home or in the run's own folder.
    pub file: String,
    pub lines: Vec<String>,
    /// Appended to the run's own value (`{file}`: its path) — a file in the run's own folder's.
    pub append: Vec<(String, String)>,
}

/// What a run with a proxy URL gets besides the core's proxy variables (`[sandbox.proxy]`, every pack's).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Proxy {
    pub env: Vec<(String, String)>,
    pub append: Vec<(String, String)>,
    /// Files in the run's own folder.
    pub files: Vec<ProxyLines>,
}

/// What a home a daemon lives from gets besides (`run-home`'s `daemon`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Daemon {
    pub make: Vec<String>,
    pub append: Vec<(String, String)>,
}

/// One of a setting's files: a place, or the file in the first of some folders that's there (else the last).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettingFile {
    Place(String),
    First { dirs: Vec<String>, file: String },
}

/// A tool's settings that can hold a token (`settings`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Setting {
    pub tool: String,
    pub var: Option<String>,
    pub var_also: Vec<String>,
    pub off: Option<String>,
    pub var_if_there: bool,
    pub files: Vec<SettingFile>,
    /// The project's `reads-above` files too.
    pub above: bool,
    pub form: String,
    pub copy: Option<String>,
    pub tell: Option<String>,
}

/// A `$(…)` a command line may hold (`expand`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expand {
    pub word: String,
    pub run: Vec<String>,
    pub env: Vec<(String, String)>,
    pub first_word_in: String,
}

/// What a command runs that its words don't name (`entry`): a line starting with `run`'s words runs the
/// project's `files` (from its root; a folder ends in `/`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    pub run: Vec<String>,
    pub files: Vec<String>,
}

/// A program that ends what it runs after a time (`time-limits`): its name, its options that take a value (the
/// next word), and of those the ones whose value is a grace added after the time (`-k 5`: killed 5 s later).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TimeLimit {
    pub run: String,
    pub option_values: Vec<String>,
    pub grace: Vec<String>,
}

/// A container engine's socket (`engine-sockets`): by its file's name, or a folder on its path — a grant of it is
/// as much as running outside the sandbox (`allow::Grant::warning`), `say` on its card.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EngineSocket {
    pub names: Vec<String>,
    pub folders: Vec<String>,
    pub say: String,
}

/// A pack's words in Claude's prompt and tools (`said`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Said {
    pub pack: String,
    pub rank: i64,
    pub run: Option<String>,
    pub code: Option<String>,
    pub tests: Option<String>,
    pub protect: Option<String>,
}

/// A fact of the system a run's sandbox is on, or of the run's policy, that a pack's variable may wait for
/// (`env`'s `on`) — the core tells it (`sandbox::Here`, `Policy`), never the pack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum On {
    /// `v4-mapped-refused`: the sandbox refuses an IPv6 socket on this machine's loopback reached through a
    /// v4-mapped address (`::ffff:127.0.0.1`) — macOS's Seatbelt below 27 (`Here::v4_mapped_refused`).
    V4MappedRefused,
    /// `each-host-asked`: the run's network is closed, each host it reaches for asked on a card
    /// (`Policy.asks_hosts`) — a tool's own retries of a host refused only keep the user waiting; with the
    /// network open, each tool as it is.
    EachHostAsked,
}

impl On {
    fn read(word: &str) -> Option<On> {
        match word {
            "v4-mapped-refused" => Some(On::V4MappedRefused),
            "each-host-asked" => Some(On::EachHostAsked),
            _ => None,
        }
    }
}

/// A variable set in every run only where `on` holds (`env = { NAME = { value, on } }`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvOn {
    pub name: String,
    pub value: String,
    pub on: On,
}

/// Every pack's `[sandbox]` and `[claude]` together.
#[derive(Clone, Debug, Default)]
pub struct Packs {
    pub homes: Vec<Home>,
    pub reads: Vec<String>,
    pub reads_above: Vec<String>,
    pub kept: Vec<String>,
    pub caches: Vec<Caches>,
    pub write_files: Vec<String>,
    pub ask: Vec<Ask>,
    pub builds_said: Vec<BuildsSaid>,
    pub run_homes: Vec<RunHome>,
    pub env: Vec<(String, String)>,
    /// `env`'s entries that wait for a fact of the system (`on`) — not among `env_names`: those a run gets from
    /// the user's environment, and where the fact doesn't hold the user's own must stay out (`JAVA_TOOL_OPTIONS`:
    /// agents). No command of Claude's sets one unasked either (`readonly::env_allowed`).
    pub env_on: Vec<EnvOn>,
    pub pass: Vec<String>,
    pub bins: Vec<String>,
    pub quiet_writes: Vec<String>,
    pub settings: Vec<Setting>,
    pub refused_port: Vec<(String, String)>,
    pub proxy: Proxy,
    /// The hosts each pack's tools fetch from: (pack, the hosts) — words on the host card, never a grant.
    pub registries: Vec<(String, crate::sandbox::proxy::egress::Pattern)>,
    pub run_allow: Vec<String>,
    pub code_trees: Vec<String>,
    pub expand: Vec<Expand>,
    pub builders: Vec<String>,
    pub entry: Vec<Entry>,
    /// Commands that take test names after their `--` (`test-filters`), each its words.
    pub test_filters: Vec<Vec<String>>,
    /// What a run card warns of (`cardwarn.rs`), every pack's — a runtime folder's adds.
    pub warn: Vec<crate::cardwarn::Warn>,
    /// Programs that run what follows their words (`timeout 600 docker …`) — a warning's command is found after
    /// one, and `missing`'s wrappers are among them.
    pub wrappers: Vec<String>,
    /// Programs that end what they run after a time their words give (`timeout 2700 sh x.sh`) — a run of
    /// Claude's under one isn't asked whether it's stuck before then (`stuck::limit`).
    pub time_limits: Vec<TimeLimit>,
    pub said: Vec<Said>,
    /// What writing a model's prompt cache costs (`switchcost.rs`) — what a switch of billing, model or effort is
    /// said to cost.
    pub price: Vec<crate::switchcost::Price>,
    /// The container engines' sockets (`engine-sockets`).
    pub engine_sockets: Vec<EngineSocket>,
    /// The extensions of files that are code, never settings (`not-settings`, `tokens::not_settings`): a
    /// token-like line there is code. Each one of its pack's languages' (a test says).
    pub not_settings: Vec<String>,
    /// What was wrong: a built-in pack's entry left out (a mistake in tarae's pack — a test fails), a runtime
    /// folder's key that's the built-in packs' only.
    pub problems: Vec<String>,
}

impl Packs {
    /// The variables the packs set or read for a run: their homes', their run homes' (and what those set and
    /// append to), their `env` (and their `proxy`'s), their settings', their caches' `var`, what their `ask` reads (a toolchain's
    /// variable of the same name moves that folder) — no command of Claude's sets one unasked
    /// (`readonly::env_allowed`).
    pub fn env_names(&self) -> impl Iterator<Item = &str> {
        let homes = self.homes.iter().map(|h| h.env.as_str());
        let runs = self.run_homes.iter().flat_map(|r| {
            let daemon = r.daemon.iter().flat_map(|d| &d.append);
            let appended = r.append.iter().chain(daemon).chain(&r.fresh).map(|(k, _)| k.as_str());
            let set = r.set.iter().map(|(k, ..)| k.as_str()).chain(appended);
            std::iter::once(r.env.as_str()).chain(set)
        });
        let proxy = self.proxy.env.iter().chain(&self.proxy.append);
        let proxy = proxy.chain(self.proxy.files.iter().flat_map(|f| &f.append));
        let env = self.env.iter().chain(proxy).map(|(k, _)| k.as_str());
        let settings = self.settings.iter().flat_map(|s| s.var.iter().chain(&s.var_also).map(String::as_str));
        let caches = self.caches.iter().filter_map(|c| c.var.as_deref());
        let asked = self.ask.iter().flat_map(Ask::keys).map(String::as_str);
        homes.chain(runs).chain(env).chain(settings).chain(caches).chain(asked)
    }
}

/// The table: the built-in packs', and (not in tests) what a runtime folder's adds where its reach lets it —
/// its disk read once (`languages::get` says what it left out).
pub fn get() -> &'static Packs {
    static TABLE: OnceLock<Packs> = OnceLock::new();
    TABLE.get_or_init(|| {
        let yours = if cfg!(test) { Vec::new() } else { crate::runtime::pack_manifests_user() };
        let t = build(crate::packs::builtin_manifests(), &yours);
        for p in t.problems.iter().filter(|p| !p.starts_with("your ")) {
            crate::log::line("tarae", "warning", p);
        }
        t
    })
}

/// The table from the built-in packs ((pack, its table, read)) and a runtime folder's ((pack, its text)).
pub(crate) fn build(builtin: &[(String, Result<Table, String>)], yours: &[(String, String)]) -> Packs {
    let mut out = Packs::default();
    // Every home first: a place is judged where its home's `else` is, whichever pack names it
    for (pack, t) in builtin {
        let Ok(t) = t else { continue };
        if let Some(h) = t.get("sandbox").and_then(Value::as_table).and_then(|s| s.get("homes"))
            && let Err(e) = read_key(&mut out, pack, "sandbox", "homes", h)
        {
            out.problems.push(format!("the {pack} pack: `sandbox.homes`: {e}"));
        }
    }
    for (pack, t) in builtin {
        let Ok(t) = t else { continue };
        read_into(&mut out, pack, t, true);
    }
    // An asked cache is said by a `[sandbox.caches] asked` (its words), and that names an `ask`'s `caches` key
    let asked: Vec<String> = out.ask.iter().flat_map(|a| a.caches.clone()).collect();
    for key in asked.iter().filter(|k| !out.caches.iter().any(|c| c.asked.as_ref() == Some(k))) {
        out.problems.push(format!("`sandbox.ask`'s cache `{key}` has no `[sandbox.caches] asked` saying it"));
    }
    for a in &mut out.ask {
        let said = |k: &String| out.caches.iter().any(|c| c.asked.as_ref() == Some(k));
        a.caches.retain(said);
    }
    out.caches.retain(|c| {
        let named = c.asked.as_ref().is_none_or(|k| asked.contains(k));
        if !named {
            out.problems.push(format!(
                "`sandbox.caches` asked `{}` names no `ask`'s cache",
                c.asked.as_deref().unwrap_or("")
            ));
        }
        named
    });
    for (pack, text) in yours {
        let Ok(t) = crate::packs::read_pack(pack, text, "yours") else { continue };
        read_into(&mut out, pack, &t, false);
    }
    crate::sorting::by(&mut out.said, |a, b| (a.rank, &a.pack).cmp(&(b.rank, &b.pack)));
    out
}

/// One pack's `[sandbox]` and `[claude]` into `out` — a runtime folder's (`builtin` false) only where the key
/// is `Added`.
fn read_into(out: &mut Packs, pack: &str, t: &Table, builtin: bool) {
    for (table, keys) in [("sandbox", SANDBOX_KEYS), ("claude", CLAUDE_KEYS)] {
        let Some(v) = t.get(table) else { continue };
        let Some(v) = v.as_table() else {
            out.problems.push(format!("the {pack} pack: [{table}] isn't a table"));
            continue;
        };
        for (key, value) in v {
            let reach = keys.iter().find(|(k, _)| k == key).map(|(_, r)| *r);
            if !builtin && reach != Some(Reach::Added) {
                out.problems
                    .push(format!("your {pack} pack: `{table}.{key}` is read from tarae's own packs only"));
                continue;
            }
            if builtin && (table, key.as_str()) == ("sandbox", "homes") {
                continue; // (read first, `build`)
            }
            let whose = if builtin { format!("the {pack} pack") } else { format!("your {pack} pack") };
            if let Err(e) = read_key_of(out, pack, builtin, table, key, value) {
                out.problems.push(format!("{whose}: `{table}.{key}`: {e}"));
            }
        }
    }
}

/// One key's value into `out`; Err why it (or an entry of it, the rest kept) was left out.
fn read_key(out: &mut Packs, pack: &str, table: &str, key: &str, v: &Value) -> Result<(), String> {
    read_key_of(out, pack, true, table, key, v)
}

/// `read_key`, for a built-in pack's table or a runtime folder's (`builtin`) — a warning says whose it is.
fn read_key_of(
    out: &mut Packs,
    pack: &str,
    builtin: bool,
    table: &str,
    key: &str,
    v: &Value,
) -> Result<(), String> {
    let list = |v: &Value| strings(Some(v), key).map(Option::unwrap_or_default);
    let tab = |v: &Value| v.as_table().cloned().ok_or(format!("`{key}` isn't a table"));
    // (every built-in pack's homes, read first — `build`)
    let homes = out.homes.clone();
    let mut wrong = Vec::new();
    // Each entry of a list of places, checked; a wrong one is left out and said
    let mut places = |into: &mut Vec<String>, v: &Value, kind: Kind| -> Result<(), String> {
        for p in list(v)? {
            match check_place(&p, kind, &homes) {
                Ok(()) => into.push(p),
                Err(e) => wrong.push(format!("`{p}` {e}")),
            }
        }
        Ok(())
    };
    match (table, key) {
        ("sandbox", "homes") => {
            for (name, h) in tab(v)? {
                let h = tab(&h)?;
                known(&h, &["env", "else", "list"], &format!("`homes.{name}`"))?;
                let env = need(&h, "env", &format!("`homes.{name}`"))?;
                let default = need(&h, "else", &format!("`homes.{name}`"))?;
                let list =
                    h.get("list").map_or(Ok(false), |l| l.as_bool().ok_or("`list` isn't true or false"))?;
                if !plain_word(&env) || env.contains(['.', '-', '+']) || NOT_A_HOME.contains(&env.as_str()) {
                    return Err(format!("`homes.{name}`: `{env}` isn't a variable a home may come from"));
                }
                if !default.starts_with("~/") {
                    return Err(format!("`homes.{name}`: `else` is a place from home (`~/…`)"));
                }
                check_place(&default, Kind::Read, &[])?;
                if out.homes.iter().any(|o| o.name == name) || CORE_VARS.contains(&name.as_str()) {
                    return Err(format!("`homes.{name}` is named twice"));
                }
                out.homes.push(Home { name, env, default, list });
            }
        }
        ("sandbox", "reads") => places(&mut out.reads, v, Kind::Read)?,
        ("sandbox", "reads-above") => {
            for p in list(v)? {
                match plain(&p) {
                    true => out.reads_above.push(p),
                    false => wrong.push(format!("`{p}` isn't a path of plain names")),
                }
            }
        }
        ("sandbox", "kept") => places(&mut out.kept, v, Kind::Kept)?,
        ("sandbox", "caches") => {
            let c = tab(v)?;
            let keys = ["home", "dirs", "asked", "places", "var", "files", "xattr", "rule", "said"];
            known(&c, &keys, "`caches`")?;
            let c = Caches {
                pack: pack.to_string(),
                home: string(&c, "home")?.unwrap_or_default(),
                dirs: list_in(&c, "dirs")?,
                asked: string(&c, "asked")?,
                places: list_in(&c, "places")?,
                var: string(&c, "var")?,
                files: list_in(&c, "files")?,
                xattr: string(&c, "xattr")?,
                rule: string(&c, "rule")?.unwrap_or_default(),
                said: string(&c, "said")?.unwrap_or_default(),
            };
            // (an asked cache's own files: none needed yet — Go's module cache takes no write offline)
            if c.asked.is_some() && !c.files.is_empty() {
                return Err("an `asked` cache has no `files` (its folder is the toolchain's answer)".into());
            }
            if c.asked.is_some() == !c.home.is_empty() || c.asked.is_some() && !c.dirs.is_empty() {
                return Err(
                    "`caches` names a `home` and its `dirs`, or the folder an `ask` named (`asked`)".into()
                );
            }
            // (an asked cache's usual places — never written by a grant, project or not)
            if c.asked.is_none() && (!c.places.is_empty() || c.var.is_some()) {
                return Err("`places` and `var` say where an `asked` cache is by default".into());
            }
            if let Some(v) = &c.var
                && (!plain_word(v) || v.contains(['.', '-', '+']) || NOT_A_HOME.contains(&v.as_str()))
            {
                return Err(format!("`{v}` isn't a variable a cache's place may come from"));
            }
            for p in &c.places {
                check_place(p, Kind::Read, &homes)?;
            }
            for f in c.dirs.iter().chain(&c.files) {
                if !plain(f) {
                    return Err(format!("`{f}` isn't a path of plain names in the home"));
                }
            }
            // (a file of the cache's own may be inside it; any other is a write like any other)
            let inside =
                |f: &&String| c.asked.is_some() || c.dirs.iter().any(|d| Path::new(f).starts_with(d));
            for f in c.files.iter().filter(|f| !inside(f)) {
                check_place(&format!("{{{}}}/{f}", c.home), Kind::Write, &homes)?;
            }
            out.caches.push(c);
        }
        ("sandbox", "write-files") => places(&mut out.write_files, v, Kind::WriteFile)?,
        ("sandbox", "ask") => {
            for a in v.as_array().ok_or("`ask` isn't a list")? {
                let a = tab(a)?;
                known(&a, &["when", "run", "json", "caches", "kept"], "an `ask`")?;
                let ask = Ask {
                    when: list_in(&a, "when")?,
                    run: list_in(&a, "run")?,
                    json: list_in(&a, "json")?,
                    caches: list_in(&a, "caches")?,
                    kept: list_in(&a, "kept")?,
                };
                if ask.when.is_empty() || ask.run.is_empty() || ask.keys().next().is_none() {
                    return Err("an `ask` needs `when`, `run` and a key (`json`, `caches`, `kept`)".into());
                }
                out.ask.push(ask);
            }
        }
        ("sandbox", "builds-said") => {
            let b = tab(v)?;
            known(&b, &["who", "target", "build"], "`builds-said`")?;
            let need = |k: &str| need(&b, k, "`builds-said`");
            out.builds_said.push(BuildsSaid {
                pack: pack.to_string(),
                who: need("who")?,
                target: need("target")?,
                build: need("build")?,
            });
        }
        ("sandbox", "run-home") => {
            for (name, h) in tab(v)? {
                out.run_homes.push(run_home(&name, &tab(&h)?, &homes)?);
            }
        }
        ("sandbox", "env") => {
            let t = v.as_table().ok_or("`env` isn't a table")?;
            for (k, val) in t {
                check_var(k)?;
                let what = format!("`env.{k}`");
                match val {
                    Value::String(s) => out.env.push((k.clone(), s.clone())),
                    Value::Table(e) => {
                        known(e, &["value", "on"], &what)?;
                        let value = need(e, "value", &what)?;
                        let on = need(e, "on", &what)?;
                        let on =
                            On::read(&on).ok_or(format!("{what}: `on` is no fact tarae knows (`{on}`)"))?;
                        out.env_on.push(EnvOn { name: k.clone(), value, on });
                    }
                    _ => return Err(format!("{what} isn't a string, nor `{{ value, on }}`")),
                }
            }
        }
        ("sandbox", "pass") => {
            for e in list(v)? {
                match check_pass(&e) {
                    Ok(()) if !out.pass.contains(&e) => out.pass.push(e),
                    Ok(()) => {}
                    Err(why) => wrong.push(format!("`{e}` {why}")),
                }
            }
        }
        ("sandbox", "bins") => places(&mut out.bins, v, Kind::Read)?,
        ("sandbox", "quiet-writes") => places(&mut out.quiet_writes, v, Kind::Read)?,
        ("sandbox", "settings") => {
            for s in v.as_array().ok_or("`settings` isn't a list ([[sandbox.settings]])")? {
                out.settings.push(setting(&tab(s)?, &homes)?);
            }
        }
        ("sandbox", "proxy") => {
            let p = tab(v)?;
            known(&p, &["env", "append", "files"], "`proxy`")?;
            for key in ["env", "append"] {
                for (k, val) in p.get(key).map_or(Ok(Vec::new()), |e| pairs(e, key))? {
                    match check_proxy_var(&k).and_then(|()| check_braces(&val, &[], &format!("`{key}.{k}`")))
                    {
                        Ok(()) if key == "env" => out.proxy.env.push((k, val)),
                        Ok(()) => out.proxy.append.push((k, val)),
                        Err(e) => wrong.push(e),
                    }
                }
            }
            for (name, f) in p.get("files").map_or(Ok(Table::new()), tab)? {
                match proxy_file(&name, &f) {
                    Ok(f) if !out.proxy.files.iter().any(|o| o.file == f.file) => out.proxy.files.push(f),
                    Ok(f) => wrong.push(format!("`files.\"{}\"` is named twice", f.file)),
                    Err(e) => wrong.push(e),
                }
            }
        }
        ("sandbox", "registries") => {
            for r in list(v)? {
                match crate::sandbox::proxy::egress::Pattern::parse(&r) {
                    Ok(p) => out.registries.push((pack.to_string(), p)),
                    Err(e) => wrong.push(format!("`{r}` {e}")),
                }
            }
        }
        ("sandbox", "refused-port") => {
            let r = tab(v)?;
            known(&r, &["process", "said"], "`refused-port`")?;
            out.refused_port
                .push((need(&r, "process", "`refused-port`")?, need(&r, "said", "`refused-port`")?));
        }
        ("claude", "run-allow") => {
            for c in list(v)? {
                match check_command(&c) {
                    Ok(()) => out.run_allow.push(c),
                    Err(e) => wrong.push(format!("`{c}` {e}")),
                }
            }
        }
        ("claude", "code-trees") => places(&mut out.code_trees, v, Kind::Tree)?,
        ("claude", "expand") => {
            for (word, e) in tab(v)? {
                let e = tab(&e)?;
                known(&e, &["run", "env", "first-word-in"], &format!("`expand.\"{word}\"`"))?;
                check_word(&word)?;
                let first_word_in = need(&e, "first-word-in", "an `expand`")?;
                check_place(&first_word_in, Kind::Read, &homes)?;
                let env = e.get("env").map_or(Ok(Vec::new()), |env| pairs(env, "env"))?;
                let run = list_in(&e, "run")?;
                if run.is_empty() {
                    return Err("an `expand` has no `run`".into());
                }
                out.expand.push(Expand { word, run, env, first_word_in });
            }
        }
        ("claude", "builders") => {
            for b in list(v)? {
                match plain_word(&b) {
                    true if !out.builders.contains(&b) => out.builders.push(b),
                    true => {}
                    false => wrong.push(format!("`{b}` isn't a program's name")),
                }
            }
        }
        ("claude", "entry") => {
            let Some(entries) = v.as_array() else { return Err("`entry` isn't a list".into()) };
            for e in entries {
                let e = tab(e)?;
                known(&e, &["run", "files"], "an `entry`")?;
                let run: Vec<String> =
                    need(&e, "run", "an `entry`")?.split_whitespace().map(String::from).collect();
                let files = list_in(&e, "files")?;
                let bad = files.iter().find(|f| !plain(f.strip_suffix('/').unwrap_or(f)));
                match (run.iter().all(|w| plain_word(w)) && !run.is_empty(), bad, files.is_empty()) {
                    (false, ..) => wrong.push(format!("`{}` isn't a command's words", run.join(" "))),
                    (_, Some(f), _) => wrong.push(format!("`{f}` isn't a path in the project")),
                    (_, _, true) => wrong.push(format!("`{}` names no files", run.join(" "))),
                    _ => out.entry.push(Entry { run, files }),
                }
            }
        }
        ("claude", "test-filters") => {
            for t in list(v)? {
                let run: Vec<String> = t.split_whitespace().map(String::from).collect();
                match !run.is_empty() && run.iter().all(|w| plain_word(w)) {
                    true if !out.test_filters.contains(&run) => out.test_filters.push(run),
                    true => {}
                    false => wrong.push(format!("`{t}` isn't a command's words")),
                }
            }
        }
        ("claude", "warn") => {
            let whose = if builtin { format!("{pack} pack") } else { format!("your {pack} pack") };
            out.warn.extend(crate::cardwarn::read(v, &whose, &mut wrong));
        }
        ("claude", "price") => out.price.extend(crate::switchcost::read(v)?),
        ("sandbox", "not-settings") => {
            for x in list(v)? {
                match !x.is_empty() && x.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()) {
                    true if !out.not_settings.contains(&x) => out.not_settings.push(x),
                    true => {}
                    false => wrong.push(format!("`{x}` isn't a file's extension")),
                }
            }
        }
        ("sandbox", "engine-sockets") => {
            let Some(entries) = v.as_array() else { return Err("`engine-sockets` isn't a list".into()) };
            for e in entries {
                let e = tab(e)?;
                known(&e, &["names", "folders", "say"], "an `engine-sockets` entry")?;
                let (names, folders) = (list_in(&e, "names")?, list_in(&e, "folders")?);
                let say = need(&e, "say", "an `engine-sockets` entry")?;
                match names.iter().chain(&folders).find(|n| !plain(n) || n.contains('/')) {
                    Some(n) => wrong.push(format!("`{n}` isn't a file's or a folder's name")),
                    None if names.is_empty() && folders.is_empty() => {
                        wrong.push("an entry names nothing".into())
                    }
                    None => out.engine_sockets.push(EngineSocket { names, folders, say }),
                }
            }
        }
        ("claude", "time-limits") => {
            let Some(entries) = v.as_array() else { return Err("`time-limits` isn't a list".into()) };
            for e in entries {
                let e = tab(e)?;
                known(&e, &["run", "option-values", "grace"], "a `time-limits` entry")?;
                let run = need(&e, "run", "a `time-limits` entry")?;
                let (option_values, grace) = (list_in(&e, "option-values")?, list_in(&e, "grace")?);
                match (plain_word(&run), grace.iter().find(|g| !option_values.contains(g))) {
                    (false, _) => wrong.push(format!("`{run}` isn't a program's name")),
                    (_, Some(g)) => wrong.push(format!("`{g}` is a `grace` but not among `option-values`")),
                    _ => out.time_limits.push(TimeLimit { run, option_values, grace }),
                }
            }
        }
        ("claude", "wrappers") => {
            for w in list(v)? {
                match plain_word(&w) {
                    true if !out.wrappers.contains(&w) => out.wrappers.push(w),
                    true => {}
                    false => wrong.push(format!("`{w}` isn't a program's name")),
                }
            }
        }
        ("claude", "said") => {
            let s = tab(v)?;
            known(&s, &["rank", "run", "code", "tests", "protect"], "`said`")?;
            let rank = s.get("rank").map_or(Ok(50), |r| r.as_integer().ok_or("`rank` isn't a number"))?;
            out.said.push(Said {
                pack: pack.to_string(),
                rank,
                run: string(&s, "run")?,
                code: string(&s, "code")?,
                tests: string(&s, "tests")?,
                protect: string(&s, "protect")?,
            });
        }
        _ => return Err("isn't a key tarae knows".into()),
    }
    match wrong.is_empty() {
        true => Ok(()),
        false => Err(wrong.join("; ")),
    }
}

/// The proxy's values a line may name: `{proxy.host}`, … (`proxy::URL_VALUES`).
fn proxy_values() -> Vec<String> {
    crate::sandbox::proxy::URL_VALUES.iter().map(|v| format!("proxy.{v}")).collect()
}

/// Every `{…}` in `s` is one of `allowed` — Err in `what`'s words: a value of the proxy's where none goes, or a
/// name that's no value.
fn check_braces(s: &str, allowed: &[String], what: &str) -> Result<(), String> {
    let mut rest = s;
    while let Some(start) = rest.find('{') {
        let end = rest[start..].find('}').ok_or(format!("{what} has an unclosed `{{`"))? + start;
        let name = &rest[start + 1..end];
        if !allowed.iter().any(|a| a == name) {
            let values = proxy_values().iter().map(|v| format!("`{{{v}}}`")).collect::<Vec<_>>().join(", ");
            return Err(match name {
                "file" => {
                    format!("{what}: `{{file}}` names a `proxy.files` entry's path — only in its `append`")
                }
                _ if name.starts_with("proxy.") && allowed.is_empty() => format!(
                    "{what} holds `{{{name}}}` — the proxy's values go only in a file's lines (a variable can end \
                     up on a command line)"
                ),
                _ => format!("{what}: `{{{name}}}` isn't a value tarae fills in ({values})"),
            });
        }
        rest = &rest[end + 1..];
    }
    Ok(())
}

/// A variable a pack's proxy part may set: one a pack may set, and none of the core's proxy variables (any
/// case — `runenv::PROXIES`).
fn check_proxy_var(name: &str) -> Result<(), String> {
    check_var(name)?;
    match crate::sandbox::runenv::PROXIES.iter().any(|p| p.eq_ignore_ascii_case(name)) {
        true => Err(format!("`{name}` is the core's own: tarae sets the proxy's variables")),
        false => Ok(()),
    }
}

/// A file's lines for a run with a proxy URL — `what` names it in what's said: plain lines, `{proxy.<value>}` in
/// them.
fn proxy_lines(lines: &[String], what: &str) -> Result<Vec<String>, String> {
    let allowed = proxy_values();
    for l in lines {
        if l.contains(['\n', '\r']) {
            return Err(format!("{what}: a line holds no newline"));
        }
        check_braces(l, &allowed, what)?;
    }
    Ok(lines.to_vec())
}

/// `[sandbox.proxy.files."<name>"]`: a file in the run's own folder.
fn proxy_file(name: &str, f: &Value) -> Result<ProxyLines, String> {
    let what = format!("`files.\"{name}\"`");
    let f = f.as_table().ok_or(format!("{what} isn't a table"))?;
    known(f, &["lines", "append"], &what)?;
    if !plain_word(name) {
        return Err(format!("{what}: a file's name in the run's folder"));
    }
    let lines = proxy_lines(&list_in(f, "lines")?, &what)?;
    let append = f.get("append").map_or(Ok(Vec::new()), |a| pairs(a, "append"))?;
    for (k, v) in &append {
        check_proxy_var(k)?;
        check_braces(v, &["file".to_string()], &format!("{what}'s `append.{k}`"))?;
    }
    Ok(ProxyLines { file: name.to_string(), lines, append })
}

fn run_home(name: &str, h: &Table, homes: &[Home]) -> Result<RunHome, String> {
    let keys = ["env", "keep", "when", "make", "link", "set", "append", "daemon", "fresh", "proxy", "said"];
    known(h, &keys, &format!("`run-home.{name}`"))?;
    if !plain_word(name) {
        return Err(format!("`run-home.{name}`: a home's name is a folder's"));
    }
    let env = need(h, "env", &format!("`run-home.{name}`"))?;
    check_var(&env)?;
    let keep = h.get("keep").map_or(Ok(false), |k| k.as_bool().ok_or("`keep` isn't true or false"))?;
    let mut r = RunHome { name: name.to_string(), env, keep, ..Default::default() };
    r.when = list_in(h, "when")?;
    if r.when.iter().any(|w| !plain(w)) {
        return Err(format!("`run-home.{name}`: `when` names files in the project"));
    }
    r.make = list_in(h, "make")?;
    for (k, v) in h.get("link").and_then(Value::as_table).into_iter().flatten() {
        let to = v.as_str().ok_or(format!("`link.{k}` isn't a string"))?;
        check_place(to, Kind::Read, homes)?;
        r.link.push((k.clone(), to.to_string()));
    }
    for (k, v) in h.get("set").and_then(Value::as_table).into_iter().flatten() {
        check_var(k)?;
        let (value, when) = match v {
            Value::String(s) => (s.clone(), None),
            Value::Table(t) => {
                known(t, &["value", "if"], &format!("`set.{k}`"))?;
                (need(t, "value", &format!("`set.{k}`"))?, string(t, "if")?)
            }
            _ => return Err(format!("`set.{k}` isn't a string or a table")),
        };
        r.set.push((k.clone(), value, when));
    }
    let appended = |t: &Table| -> Result<Vec<(String, String)>, String> {
        let a = t.get("append").map_or(Ok(Vec::new()), |a| pairs(a, "append"))?;
        a.iter().try_for_each(|(k, _)| check_var(k))?;
        Ok(a)
    };
    r.append = appended(h)?;
    if let Some(d) = h.get("daemon") {
        let d = d.as_table().ok_or(format!("`run-home.{name}.daemon` isn't a table"))?;
        known(d, &["make", "append"], &format!("`run-home.{name}.daemon`"))?;
        if !r.keep {
            return Err(format!(
                "`run-home.{name}`: a home a daemon lives from is a kept one (`keep = true`)"
            ));
        }
        r.daemon = Some(Daemon { make: list_in(d, "make")?, append: appended(d)? });
    }
    if let Some(f) = h.get("fresh") {
        let f = f.as_table().ok_or(format!("`run-home.{name}.fresh` isn't a table"))?;
        known(f, &["append"], &format!("`run-home.{name}.fresh`"))?;
        r.fresh = appended(f)?;
    }
    let daemon_made = r.daemon.iter().flat_map(|d| &d.make);
    if r.make.iter().chain(daemon_made).chain(r.link.iter().map(|(k, _)| k)).any(|p| !plain(p)) {
        return Err(format!("`run-home.{name}`: what's made in it is a path of plain names"));
    }
    if let Some(p) = h.get("proxy") {
        let what = format!("`run-home.{name}.proxy`");
        let p = p.as_table().ok_or(format!("{what} isn't a table"))?;
        known(p, &["file", "lines"], &what)?;
        let file = need(p, "file", &what)?;
        if !plain_word(&file) {
            return Err(format!("{what}: `file` is a file's name in the home"));
        }
        let lines = proxy_lines(&list_in(p, "lines")?, &what)?;
        r.proxy = Some(ProxyLines { file, lines, append: Vec::new() });
    }
    r.said = string(h, "said")?.unwrap_or_default();
    Ok(r)
}

fn setting(s: &Table, homes: &[Home]) -> Result<Setting, String> {
    let keys = ["tool", "var", "var-also", "off", "var-if-there", "files", "above", "form", "copy", "tell"];
    known(s, &keys, "a `settings` entry")?;
    let tool = need(s, "tool", "a `settings` entry")?;
    let flag =
        |k: &str| s.get(k).map_or(Ok(false), |v| v.as_bool().ok_or(format!("`{k}` isn't true or false")));
    let mut out = Setting {
        var: string(s, "var")?,
        var_also: list_in(s, "var-also")?,
        off: string(s, "off")?,
        var_if_there: flag("var-if-there")?,
        above: flag("above")?,
        form: string(s, "form")?.unwrap_or_else(|| "lines".into()),
        copy: string(s, "copy")?,
        tell: string(s, "tell")?,
        tool,
        ..Default::default()
    };
    for v in out.var.iter().chain(&out.var_also) {
        check_var(v)?;
    }
    if !["lines", "ini"].contains(&out.form.as_str()) {
        return Err(format!("`{}`: `form` is `lines` or `ini`", out.tool));
    }
    if out.copy.as_deref().is_some_and(|c| !plain_word(c)) || (out.copy.is_some() && out.var.is_none()) {
        return Err(format!("`{}`: a `copy` is a file's name, given by `var`", out.tool));
    }
    for f in s.get("files").and_then(Value::as_array).into_iter().flatten() {
        let f = match f {
            Value::String(p) => SettingFile::Place(p.clone()),
            Value::Table(t) => {
                known(t, &["first", "file"], "a `files` entry")?;
                let dirs = list_in(t, "first")?;
                let file = need(t, "file", "a `files` entry")?;
                if dirs.is_empty() || !plain(&file) {
                    return Err(format!("`{}`: `first` names folders, `file` a file in them", out.tool));
                }
                SettingFile::First { dirs, file }
            }
            _ => return Err(format!("`{}`: a `files` entry isn't a place", out.tool)),
        };
        let all = match &f {
            SettingFile::Place(p) => vec![p.clone()],
            SettingFile::First { dirs, file } => dirs.iter().map(|d| format!("{d}/{file}")).collect(),
        };
        for p in all {
            check_place(&p, Kind::Setting, homes)?;
        }
        out.files.push(f);
    }
    Ok(out)
}

/// A table of strings (`env = { NAME = "value" }`), as pairs.
fn pairs(v: &Value, what: &str) -> Result<Vec<(String, String)>, String> {
    let t = v.as_table().ok_or(format!("`{what}` isn't a table"))?;
    let each = |(k, v): (&String, &Value)| {
        Ok((k.clone(), v.as_str().ok_or(format!("`{what}.{k}` isn't a string"))?.into()))
    };
    t.iter().map(each).collect()
}

/// The core's own variables (`Vars`, and a run home's `{home}` in what it appends) — no home may take one's
/// name.
const CORE_VARS: [&str; 5] = ["user-config", "user-cache", "xdg-config", "run", "home"];

/// Variables no tool's home comes from: they'd make it home, the folders on `PATH`, or the temp folders.
const NOT_A_HOME: [&str; 4] = ["HOME", "PATH", "TMPDIR", "PWD"];

/// What a place is for, for its check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Read,
    /// Never read: anything in home.
    Kept,
    /// Written.
    Write,
    /// A file written (`{a}` placeholders).
    WriteFile,
    /// Read unasked, `*` a whole part.
    Tree,
    /// A tool's settings, read by tarae and copied without their tokens: never where keys are.
    Setting,
}

/// A home that stands for every home in checks — its own place, the homes' defaults, `{user-config}` both
/// systems' — so a place is judged wherever it lands.
const SOME_HOME: &str = "/home-of-the-check";

/// Whether place `p` (as written) may be one of `kind`'s — Err why not: not from home or a variable, a `..`, a
/// `*` or `{a}` where none goes, or (but kept) home itself or above it, keys, Claude's or tarae's own, a write
/// where nothing is written (`allow::NEVER_WRITE`) — a home's place judged where its `else` is (`homes`).
fn check_place(p: &str, kind: Kind, homes: &[Home]) -> Result<(), String> {
    let rest = match p.strip_prefix("~/") {
        Some(rest) => rest,
        None if p.starts_with('{') => match p.find('}') {
            Some(end) => p[end + 1..].trim_start_matches('/'),
            None => return Err("has an unclosed `{`".into()),
        },
        None => return Err("isn't from home (`~/…`) or a variable (`{…}`)".into()),
    };
    let parts: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    if parts.iter().any(|s| *s == ".." || *s == ".") {
        return Err("has a `..` or `.`".into());
    }
    let star = |s: &&str| s.contains('*');
    let letter = |s: &&str| s.contains('{');
    match kind {
        Kind::Tree if parts.iter().any(|s| star(s) && *s != "*") => {
            return Err("has a `*` in part of a name".into());
        }
        Kind::Tree => {}
        _ if parts.iter().any(star) => return Err("has a `*`".into()),
        _ => {}
    }
    if kind != Kind::WriteFile && parts.iter().any(letter) {
        return Err("has a `{…}` past its start".into());
    }
    if kind == Kind::Kept {
        return Ok(());
    }
    if p.starts_with("~/") && parts.is_empty() {
        return Err("is home itself".into());
    }
    // Where it lands, from home: a variable is each system's place for it (a pack's home: its `else` — where
    // its variable moves it the user put it)
    let home = Path::new(SOME_HOME);
    let leads = leads(p, homes).ok_or("starts with a variable that's no home (`homes`) nor the core's")?;
    let at: Vec<PathBuf> = leads.iter().map(|lead| home.join(lead).join(parts.join("/"))).collect();
    let near = |s: &str| {
        let s = home.join(s);
        at.iter().any(|a| a.starts_with(&s) || s.starts_with(a))
    };
    // (a tool's settings file may be one kept for its tokens, itself — its copy without them stands in)
    let itself = |s: &str| kind == Kind::Setting && at.iter().all(|a| *a == home.join(s));
    if crate::allow::SECRETS.iter().any(|s| near(s) && !itself(s)) || OWN.iter().any(|s| near(s)) {
        return Err("is where keys or Claude's and tarae's own state are".into());
    }
    let writes = matches!(kind, Kind::Write | Kind::WriteFile);
    if writes && crate::allow::NEVER_WRITE.iter().any(|s| near(s)) {
        return Err("is where nothing is written (it runs later, or every project's build reads it)".into());
    }
    Ok(())
}

/// Claude's own and tarae's state, from home — never in a pack's place.
const OWN: [&str; 4] = [".claude", ".claude.json", ".local/state/tarae", ".config/tarae"];

/// Where place `p`'s start lands from home on each system: `~/` home itself, `{user-config}` macOS's and
/// Linux's, a pack's home its `else` — None: a variable that's neither (`{run}` isn't in home).
fn leads(p: &str, homes: &[Home]) -> Option<Vec<String>> {
    if p.starts_with("~/") {
        return Some(vec![String::new()]);
    }
    let var = &p[1..p.find('}').unwrap_or(1)];
    let both = |a: &str, b: &str| vec![a.to_string(), b.to_string()];
    Some(match var {
        "user-config" => both("Library/Application Support", ".config"),
        "user-cache" => both("Library/Caches", ".cache"),
        "xdg-config" => vec![".config".into()],
        name => vec![homes.iter().find(|h| h.name == name)?.default.strip_prefix("~/")?.to_string()],
    })
}

/// A path of plain names: no root, `.`, `..`, `*` or empty part.
fn plain(p: &str) -> bool {
    !p.is_empty() && p.split('/').all(|s| !s.is_empty() && s != "." && s != ".." && !s.contains(['*', '{']))
}

/// A program's or a file's name: letters, digits, `.`, `_`, `-`, `+`, not starting with `-` or `.`.
fn plain_word(w: &str) -> bool {
    !w.is_empty()
        && !w.starts_with(['-', '.'])
        && w.chars().all(|c| c.is_ascii_alphanumeric() || "._-+".contains(c))
}

/// A variable a pack may set: a name `ENV_NEVER` doesn't keep off.
fn check_var(name: &str) -> Result<(), String> {
    match crate::readonly::env_core(name) {
        true => Ok(()),
        false => Err(format!("`{name}` is a variable a pack may not set")),
    }
}

/// A `pass` entry: a name or a family (`sandbox::runenv::entry_ok`), not the loader's, not a name only a secret
/// has (what `tokens::token_like` says of it with a value).
fn check_pass(e: &str) -> Result<(), String> {
    use crate::sandbox::runenv::{LOADER, entry_ok};
    let stem = e.trim_end_matches('*');
    if !entry_ok(e) {
        return Err("isn't a variable's name, or a family of two characters or more before its `*`".into());
    }
    if LOADER.iter().any(|l| stem.starts_with(l) || l.starts_with(stem)) {
        return Err("is the loader's — only the user's own llm.sandbox-env passes it".into());
    }
    if crate::tokens::token_like(&format!("{stem}=a-value-of-some-length")) {
        return Err("names a secret".into());
    }
    Ok(())
}

/// A command run unasked: plain words (`gofmt -l`), the first a program's name that doesn't run what follows it.
fn check_command(c: &str) -> Result<(), String> {
    let words: Vec<&str> = c.split(' ').collect();
    let word =
        |w: &&str| !w.is_empty() && w.chars().all(|c| c.is_ascii_alphanumeric() || "._-+=/:".contains(c));
    if !words.iter().all(word) || !plain_word(words[0]) {
        return Err("isn't plain words".into());
    }
    if crate::tools::runner(words[0]) {
        return Err("starts with a program that runs whatever follows it".into());
    }
    Ok(())
}

/// A `$(…)` a line may hold: plain words in it — no quote, `$`, `;`, `|`, `&`, `<`, `>`, backtick.
fn check_word(w: &str) -> Result<(), String> {
    let inner = w.strip_prefix("$(").and_then(|r| r.strip_suffix(')')).ok_or("isn't a `$(…)`")?;
    let ok = !inner.trim().is_empty()
        && inner.split(' ').all(|p| {
            !p.is_empty()
                && p.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "-.".contains(c))
        });
    match ok {
        true => Ok(()),
        false => Err(format!("`{w}` isn't plain words in a `$(…)`")),
    }
}

// ── Places, for a home ───────────────────────────────────────────────────────

/// The table's variables for a home on a system — what each place in it is.
#[derive(Clone, Debug)]
pub struct Vars {
    pub home: PathBuf,
    /// Each home: (name, where it is — its variable's when that's an absolute path (a list home's: its first
    /// entry's), else its default — its default).
    pub homes: Vec<(String, PathBuf, PathBuf)>,
    /// A list home's other entries (absolute) — homes too, with none of its places in them.
    pub listed: Vec<PathBuf>,
    pub user_config: PathBuf,
    pub user_cache: PathBuf,
    pub xdg_config: PathBuf,
    pub run: Option<PathBuf>,
    /// Where the tools' shared caches are (`caches`): a home's `dirs` where the home is and at its default, an
    /// asked one's `places` both ways and where its `var` points — never written but through the caches rule.
    pub caches: Vec<CachePlace>,
}

/// One place of a shared cache's (`Vars::caches`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CachePlace {
    pub at: PathBuf,
    /// Its cache's `asked` key (`GOMODCACHE`) — None: a cache no toolchain is asked for.
    pub asked: Option<String>,
    /// A place the pack gives (`dirs`, `places`: in a home) — false: where its `var` points.
    pub given: bool,
}

impl Vars {
    /// For `home` (as it's set), the variables as `var` gives them (set and not empty), on `os`.
    pub fn new(t: &Packs, home: &Path, var: &dyn Fn(&str) -> Option<PathBuf>, os: Os) -> Vars {
        let abs = |name: &str| var(name).filter(|p| p.is_absolute());
        let at = |p: &str| match p.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None => PathBuf::from(p),
        };
        // (a list home's entries: the first is where the tool puts its places)
        let entries = |h: &Home| -> Vec<PathBuf> {
            let v = var(&h.env);
            v.map(|v| std::env::split_paths(&v).collect()).unwrap_or_default()
        };
        let first = |h: &Home| match h.list {
            true => entries(h).into_iter().next().filter(|p| p.is_absolute()),
            false => abs(&h.env),
        };
        let homes = t
            .homes
            .iter()
            .map(|h| (h.name.clone(), first(h).unwrap_or_else(|| at(&h.default)), at(&h.default)))
            .collect();
        let listed = t.homes.iter().filter(|h| h.list).flat_map(|h| entries(h).into_iter().skip(1));
        let listed = listed.filter(|p| p.is_absolute()).collect();
        let xdg_config = abs("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));
        let (user_config, user_cache) = match os {
            Os::Mac => (home.join("Library/Application Support"), home.join("Library/Caches")),
            Os::Linux => (xdg_config.clone(), abs("XDG_CACHE_HOME").unwrap_or_else(|| home.join(".cache"))),
        };
        let mut v = Vars {
            home: home.to_path_buf(),
            homes,
            listed,
            user_config,
            user_cache,
            xdg_config,
            run: None,
            caches: Vec::new(),
        };
        let mut caches: Vec<CachePlace> = Vec::new();
        for c in &t.caches {
            let place = |at, given| CachePlace { at, asked: c.asked.clone(), given };
            let dirs = c.dirs.iter().map(|d| format!("{{{}}}/{d}", c.home));
            let given = dirs.chain(c.places.iter().cloned()).flat_map(|p| v.places_both(&p));
            caches.extend(given.map(|at| place(at, true)));
            caches.extend(c.var.as_deref().and_then(abs).map(|at| place(at, false)));
        }
        crate::sorting::sort(&mut caches);
        caches.dedup();
        v.caches = caches;
        v
    }

    /// A home's place: where it is, and its default.
    pub fn home_of(&self, name: &str) -> Option<(&Path, &Path)> {
        self.homes.iter().find(|(n, ..)| n == name).map(|(_, at, default)| (at.as_path(), default.as_path()))
    }

    /// Where place `p` is — None: a variable that isn't one (or `{run}` without a run).
    pub fn place(&self, p: &str) -> Option<PathBuf> {
        self.place_with(p, false)
    }

    /// Where `p` is — a home's at its default place with `default`.
    fn place_with(&self, p: &str, default: bool) -> Option<PathBuf> {
        if let Some(rest) = p.strip_prefix("~/") {
            return Some(self.home.join(rest));
        }
        let end = p.find('}')?;
        let rest = p[end + 1..].trim_start_matches('/');
        let base = match &p[1..end] {
            "user-config" => self.user_config.clone(),
            "user-cache" => self.user_cache.clone(),
            "xdg-config" => self.xdg_config.clone(),
            "run" => self.run.clone()?,
            name => {
                let (at, def) = self.home_of(name)?;
                if default { def } else { at }.to_path_buf()
            }
        };
        Some(if rest.is_empty() { base } else { base.join(rest) })
    }

    /// Where `p` is at its home's place and at its default (the same place once, when not moved) — what a
    /// kept place is.
    pub fn places_both(&self, p: &str) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> =
            [self.place_with(p, true), self.place_with(p, false)].into_iter().flatten().collect();
        v.dedup();
        v
    }

    /// Whether `p` is a home alone (`{cargo}`) — counted only where it's there.
    pub fn is_home(&self, p: &str) -> bool {
        p.strip_prefix('{').and_then(|r| r.strip_suffix('}')).is_some_and(|n| self.home_of(n).is_some())
    }
}

/// Every `{a}`-style placeholder of `p` filled with the folders that are there, the same name for the same
/// letter (`{gradle}/wrapper/dists/{a}/{b}/{a}.zip.lck`, `p` already resolved but the letters) — the files that
/// could be: one for each such folder path, none through a link past the first letter. Blocks (a worker's).
pub fn filled(p: &Path) -> Vec<PathBuf> {
    let mut found = vec![(PathBuf::new(), Vec::<(String, String)>::new())];
    for c in p.components() {
        let Component::Normal(name) = c else {
            for (at, _) in &mut found {
                at.push(c);
            }
            continue;
        };
        let name = name.to_string_lossy();
        let mut next = Vec::new();
        for (at, letters) in found {
            let with = |s: &str, letters: &[(String, String)]| {
                letters.iter().fold(s.to_string(), |s, (l, v)| s.replace(&format!("{{{l}}}"), v))
            };
            let whole = name.strip_prefix('{').and_then(|r| r.strip_suffix('}'));
            // (never through a link: a rule on the path would land where it points, outside the home)
            match whole.filter(|l| !letters.iter().any(|(k, _)| k == l)) {
                Some(letter) => {
                    for e in std::fs::read_dir(&at).into_iter().flatten().flatten() {
                        if e.file_type().is_ok_and(|t| t.is_dir()) {
                            let mut l = letters.clone();
                            l.push((letter.to_string(), e.file_name().to_string_lossy().into_owned()));
                            next.push((e.path(), l));
                        }
                    }
                }
                None => {
                    let p = at.join(with(&name, &letters));
                    let link = std::fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink());
                    if letters.is_empty() || !link {
                        next.push((p, letters));
                    }
                }
            }
        }
        found = next;
    }
    let mut out: Vec<PathBuf> = found.into_iter().map(|(p, _)| p).collect();
    crate::sorting::sort(&mut out);
    out
}

#[cfg(test)]
mod tests;
