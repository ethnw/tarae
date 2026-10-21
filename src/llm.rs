//! LLM — runs `claude -p` (or any CLI with the same stream-json protocol) as a subprocess.
//! No HTTP·SDK·API key inside the editor — uses the user's Claude Code login as is.
//!
//! Cold start is slow (measured 2026-09-27, Claude Code 2.1.280, a single "ok"):
//! default 8–10 s → minimal options (`--tools "" --strict-mcp-config --setting-sources ""`) 4.3 s
//! → **a process spawned early and waiting on stdin (stream-json): 2.5 s** (haiku 1.9 s).
//! So the spare process is spawned the moment the ask prompt opens (overlapping typing); a request only
//! writes the message. One process per request — so contexts don't mix.
//!
//! Flow: selections → one request (answer per selection in `<r i="N">` tags) → review (diff, y/n per change)
//! → applied as one transaction. The answer is received as it streams (`--include-partial-messages`) and
//! shown in a preview card. Cancel = kill the process.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

use ropey::Rope;

use crate::billing::Credential;
use crate::document::DocId;
use crate::editor::Editor;
use crate::event::{Apply, Event};
use crate::key::{Code, Key};
use crate::selection::{Range, Selection};
use crate::transaction::{Assoc, Change, Transaction};

/// Speed options: no tools (pure transform), skip MCP·user settings, no conversation kept by claude (keeps /resume clean).
const DEFAULT_ARGS: &[&str] = &[
    "-p",
    "--input-format",
    "stream-json",
    "--output-format",
    "stream-json",
    "--verbose",
    "--include-partial-messages",
    "--tools",
    "",
    "--no-session-persistence",
    "--strict-mcp-config",
    "--setting-sources",
    "",
];

/// A model claude offers (`initialize` → `models`, measured): its alias or id, what it's called, a line
/// about it, the model it stands for now, and the effort levels it takes (none: it has no such knob).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelInfo {
    pub value: String,
    pub name: String,
    pub about: String,
    pub resolved: String,
    pub efforts: Vec<String>,
}

impl ModelInfo {
    pub fn list(v: &serde_json::Value) -> Vec<ModelInfo> {
        let s = |m: &serde_json::Value, k: &str| m[k].as_str().unwrap_or_default().to_string();
        v.as_array()
            .into_iter()
            .flatten()
            .filter(|m| m["value"].is_string())
            .map(|m| ModelInfo {
                value: s(m, "value"),
                name: s(m, "displayName"),
                about: s(m, "description"),
                resolved: s(m, "resolvedModel"),
                efforts: m["supportedEffortLevels"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|e| e.as_str().map(str::to_string))
                    .collect(),
            })
            .collect()
    }
}

/// `llm.sandbox-hosts`: with the network on, the hosts Claude's commands reach without asking (piece C1b).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Hosts {
    /// Every built-in pack's registries, whatever the project's languages (`"registries"`, the default).
    #[default]
    Registries,
    /// Any host (`"*"`; `"any"` read the same — configs written before C1b).
    Any,
    /// That list alone — the registries aren't added (`[]`: every host asked).
    List(Vec<crate::sandbox::proxy::egress::Pattern>),
}

impl Hosts {
    /// The rules a run's proxy holds — None: any host.
    pub fn rules(&self) -> Option<Vec<crate::sandbox::proxy::egress::Pattern>> {
        match self {
            Hosts::Any => None,
            Hosts::List(l) => Some(l.clone()),
            Hosts::Registries => {
                let mut v: Vec<crate::sandbox::proxy::egress::Pattern> = Vec::new();
                for (_, p) in &crate::sandboxpack::get().registries {
                    if !v.contains(p) {
                        v.push(p.clone());
                    }
                }
                Some(v)
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct LlmConfig {
    pub command: String,
    pub args: Vec<String>,
    pub model: Option<String>,
    /// `--effort` ("" = the model's default).
    pub effort: String,
    /// Lines of context sent before and after the selection.
    pub context_lines: usize,
    /// Chat: the editor follows where Claude is reading (`C-f` in the chat toggles it for that chat).
    pub follow: bool,
    /// Chat: seconds after your last key that follow takes over again while Claude works (0 = only `C-f`).
    pub follow_resume: u64,
    /// Chat: Claude's edits go in as it makes them, the turn reviewed when it ends — every new chat starts
    /// so (`space v a` turns it on and off, and keeps it here).
    pub review_at_end: bool,
    /// Chat: the tools Claude edits with (`llm.edit-tools` — `edittool::Arm`), fixed for a chat process when it
    /// starts. Only your own config sets it (`trust::YOURS_ONLY`).
    pub edit_tools: crate::edittool::Arm,
    /// Chat: load CLAUDE.md files and Claude Code settings like `claude` does (hooks off, edits always ask).
    pub chat_settings: bool,
    /// Keep every card asked about Claude's work, and every run of its refused with no card, in the record
    /// (`cards.rs`: `$state/cards/`). Only your own config sets it (`trust::YOURS_ONLY`).
    pub card_record: bool,
    /// An `ANTHROPIC_API_KEY`/`ANTHROPIC_AUTH_TOKEN` in tarae's environment goes on to claude (`llm.environment-key
    /// = "keep"`) — but inside a Claude Code session, where it's the host's (`claude_env`).
    pub keep_env_key: bool,
    /// Who pays for a new session's Claude (`llm.billing` — `billing.rs`); a session keeps its own.
    pub billing: crate::billing::Billing,
    /// Who pays for the reviewer's (`llm.reviewer-billing`): None — as its session.
    pub reviewer_billing: Option<crate::billing::Billing>,
    /// Chat: commands Claude's `run` starts without asking, past the built-in checks (`tools::run_allow`) —
    /// each a prefix of whole words (`"npm test"`).
    pub run_allow: Vec<String>,
    /// Chat, review at the end (`edits.rs`): more files whose edits still ask first, past the protected ones
    /// (`sandbox::protected_names`, which always ask) — a name matches anywhere, a path (with `/`) from the
    /// project's root.
    pub review_ask: Vec<String>,
    /// Chat: Claude's commands run in a sandbox (`sandbox.rs`) — writes in the project, temp and the
    /// toolchain's folders; no network but this machine.
    pub sandbox: bool,
    pub sandbox_network: bool,
    /// With the network on, the hosts reached without asking (`llm.sandbox-hosts`); another is held and asked on a
    /// card (`hosts.rs`).
    pub sandbox_hosts: Hosts,
    /// With the network on, whether tarae's proxy goes on through the proxy tarae's own environment names
    /// (`HTTPS_PROXY`…, `NO_PROXY` — `proxy::upstream`), or straight out.
    pub sandbox_upstream: crate::sandbox::proxy::upstream::Setting,
    /// More folders the sandbox may write to (`~/` for home).
    pub sandbox_write: Vec<String>,
    /// Paths in the project the sandbox keeps as they are, besides the names it always keeps
    /// (`sandbox::protected_names`).
    pub sandbox_protect: Vec<String>,
    /// Ports on this machine the sandbox lets Claude's commands connect to (none else, the network open or not).
    pub sandbox_ports: Vec<u16>,
    /// Every port on this machine open to them — the risky ones too (Seatbelt can't leave some out), and on
    /// macOS other runs' ports to tarae's proxy.
    pub sandbox_localhost: bool,
    /// More of tarae's environment their commands get (`sandbox::runenv`): names, or `PREFIX*` families — a
    /// token-like one only by its whole name.
    pub sandbox_env: Vec<String>,
    /// Days a kept run home (a toolchain's home of the runs', kept across them) stays with no run using it before
    /// tarae's start removes it (`runhomes.rs`).
    pub sandbox_kept_days: u64,
    /// Chat: Claude's `screen` tool shows the terminal panel's contents too (`seeing.rs`) — off, its rows go
    /// blank.
    pub screen_terminal: bool,
    /// Chat: minutes a run of Claude's prints nothing at next to no CPU before a card asks whether it's
    /// stuck (`stuck.rs`; 0 = never).
    pub stuck_after: u64,
    /// …and one whose work may go on elsewhere — granted a daemon's socket, run outside the sandbox, connected
    /// outside its processes — with no time limit of its own: minutes before that card (cardburden 5; 0 = as
    /// `stuck_after`).
    pub stuck_after_outside: u64,
    /// Chat: a big conversation that waits on you is kept warm by a ping before its cache's TTL (`keepwarm.rs`) —
    /// when it's expected back, only when you marked it, or never. Only your own config sets these
    /// (`trust::YOURS_ONLY`: a ping spends your usage).
    pub keep_warm: crate::keepwarm::Mode,
    /// …a conversation of at least these many tokens · at most these many pings while it waits.
    pub keep_warm_min_tokens: u64,
    pub keep_warm_max: u32,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            command: "claude".into(),
            args: DEFAULT_ARGS.iter().map(|s| s.to_string()).collect(),
            model: None,
            effort: String::new(),
            context_lines: 20,
            follow: true,
            follow_resume: 20,
            review_at_end: false,
            edit_tools: crate::edittool::Arm::Tarae,
            chat_settings: true,
            card_record: true,
            keep_env_key: false,
            billing: Default::default(),
            reviewer_billing: None,
            run_allow: Vec::new(),
            review_ask: Vec::new(),
            sandbox: true,
            sandbox_network: false,
            sandbox_hosts: Hosts::default(),
            sandbox_upstream: Default::default(),
            sandbox_write: Vec::new(),
            sandbox_protect: Vec::new(),
            sandbox_ports: Vec::new(),
            sandbox_localhost: false,
            sandbox_env: Vec::new(),
            sandbox_kept_days: 14,
            screen_terminal: true,
            stuck_after: 10,
            stuck_after_outside: 30,
            keep_warm: crate::keepwarm::Mode::Expected,
            keep_warm_min_tokens: 100_000,
            keep_warm_max: 3,
        }
    }
}

// ── Spare process ───────────────────────────────────────────────────────

/// A pre-spawned process for the next request (at most one), the folder it runs in and the credential it was
/// started with.
#[derive(Default)]
pub struct Warm {
    child: Option<(Child, PathBuf, Credential)>,
}

impl Warm {
    /// Spawns one in `dir` on `cred` if there's no live spare there on it (one elsewhere, or on another billing,
    /// goes). spawn itself returns at once (slow init happens in the child).
    pub fn ensure(
        &mut self,
        cfg: &LlmConfig,
        dir: &Path,
        given: Option<&Env>,
        cred: &Credential,
    ) -> Result<(), String> {
        if let Some((c, at, on)) = &mut self.child
            && matches!(c.try_wait(), Ok(None))
            && at == dir
            && on == cred
        {
            return Ok(());
        }
        // Spared for another folder or billing, or gone: it goes
        self.drop_spare();
        let child = spawn(cfg, &[], dir, given, cred).map_err(|e| e.said(&cfg.command, false))?;
        self.child = Some((child, dir.to_path_buf(), cred.clone()));
        Ok(())
    }

    /// The spare's process (`:procs`).
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(|(c, ..)| c.id())
    }

    /// The spare goes (reaped off the main thread — else a zombie until tarae exits).
    pub fn drop_spare(&mut self) {
        if let Some((mut c, ..)) = self.child.take() {
            crate::event::thread("tarae-reap", move || {
                let _ = c.kill();
                let _ = c.wait();
            });
        }
    }

    fn take(
        &mut self,
        cfg: &LlmConfig,
        dir: &Path,
        given: Option<&Env>,
        cred: &Credential,
    ) -> Result<Child, String> {
        self.ensure(cfg, dir, given, cred)?;
        Ok(self.child.take().expect("ensured").0)
    }
}

impl Drop for Warm {
    fn drop(&mut self) {
        self.drop_spare();
    }
}

/// The session in front's billing changed (`:session-billing`): a spare there goes, and comes back on it.
pub fn spare_billing_changed(ed: &mut Editor) {
    if ed.llm_warm.child.is_none() {
        return;
    }
    ed.llm_warm.drop_spare();
    prewarm(ed);
}

/// What a failed start of `command` (`llm.command`) says. No `claude` at all: that Claude Code isn't installed,
/// and how to get it and log in — in its own docs' words (code.claude.com/docs/en/setup); `whole`: every line
/// (the chat), else one (a toast).
pub fn spawn_failed(command: &str, e: &std::io::Error, whole: bool) -> String {
    if e.kind() != std::io::ErrorKind::NotFound {
        return format!("{command}: {e}");
    }
    if Path::new(command).file_name().is_none_or(|n| n != "claude") {
        return match command.contains('/') {
            true => format!("{command} isn't there (llm.command)"),
            false => format!("{command} isn't on PATH (llm.command)"),
        };
    }
    if !whole {
        return "Claude Code isn't installed, or `claude` isn't on your PATH — install it with \
                `curl -fsSL https://claude.ai/install.sh | bash`, then log in by running `claude`"
            .into();
    }
    "Claude Code isn't installed, or `claude` isn't on your PATH. Install it (macOS, Linux, WSL):\n\
     curl -fsSL https://claude.ai/install.sh | bash\n\
     After installing, log in by running `claude` and following the browser prompts.\n\
     If `claude` still isn't found, ~/.local/bin isn't on your PATH yet — add it, and start tarae from a \
     new terminal.\n\
     https://code.claude.com/docs/en/setup"
        .into()
}

/// Claude, started in `dir` (the project root — its relative paths, its tools and its kept conversations are there),
/// with tarae's environment less what `claude_env` leaves out, on `cred` (`billing.rs`: an API session's key helper
/// laid into its `--settings`). `given`: `Editor::claude_env` — a test's stand-in for tarae's environment, laid
/// over the process's as if inherited (None: the process's own); what's left out is judged on it alone, and the
/// process's names those rules judge aren't inherited (`judged`). As you, as every other tool runs claude: no
/// sandbox of its own (user decision 2026-10-17) — what it does through its tools goes through tarae.
pub fn spawn(
    cfg: &LlmConfig,
    extra: &[&str],
    dir: &Path,
    given: Option<&Env>,
    cred: &Credential,
) -> Result<Child, SpawnError> {
    let mut cmd = command(cfg, extra, dir, given, cred)?;
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.spawn().map_err(SpawnError::Start)
}

/// `spawn`'s command, not started: its program (`llm.command`, itself), arguments and environment.
pub fn command(
    cfg: &LlmConfig,
    extra: &[&str],
    dir: &Path,
    given: Option<&Env>,
    cred: &Credential,
) -> Result<Command, SpawnError> {
    // (tests never start the real claude — a stand-in in `sh` answers for it; card model P2 found a test
    // whose claude read ~/.claude.json and the keychain)
    #[cfg(test)]
    assert!(
        Path::new(&cfg.command).file_name().is_none_or(|n| n != "claude"),
        "a test started the real claude — give llm.command a stand-in (`sh -c 'cat > /dev/null'`)"
    );
    let args = crate::billing::args(&cfg.args, cred).map_err(SpawnError::Billing)?;
    let mut cmd = Command::new(&cfg.command);
    cmd.current_dir(dir).args(&args).args(extra);
    if let Some(given) = given {
        // (a stand-in is the whole of what the rules judge — `ClaudeEnv::of`: the process's own such names stay out)
        for name in process_names().into_iter().filter(|n| judged(n) && !given.contains_key(n)) {
            cmd.env_remove(name);
        }
        cmd.envs(given);
    }
    // claude doesn't update itself from tarae (decision A): the user updates it (`:claude-update`), never a
    // program swapped under a chat that's running — `DISABLE_UPDATES` stops the background updater and
    // `claude update`/`install` alike (its env-vars reference). And its `git status` takes no index lock under
    // you (`.git/index.lock`)
    cmd.env("DISABLE_UPDATES", "1").env("GIT_OPTIONAL_LOCKS", "0");
    // Nor does it read Claude Code's auto memory (threat model G21): its folder is the repository's (every
    // checkout's), and what's there is another tool's notes — instructions not meant for it, paid for in every
    // request; a note this claude wrote there would later be obeyed by your own Claude Code. The variable
    // outranks the setting `autoMemoryEnabled` (2.1.296), so a project's settings can't turn it back on. Team
    // memory (`CLAUDE_MEMORY_STORES`) is memory too: never passed
    cmd.env("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1").env_remove("CLAUDE_MEMORY_STORES");
    // Nor does it reach your SSH agent (threat model G20): through its socket the agent signs with your keys for
    // whoever asks, no key read. claude's own git only reads status, it never needs one; Claude's runs never got
    // it (`runenv`). A path it finds elsewhere is reached: claude runs as you (threat model G20)
    cmd.env_remove("SSH_AUTH_SOCK");
    // (removed by name, never `env_clear`: the rest of tarae's environment is claude's as it always was. An API
    // session never keeps a key from it: one there outranks its helper — Claude Code's credential order)
    let keep_keys = cfg.keep_env_key && *cred == Credential::Subscription;
    for name in ClaudeEnv::of(given, keep_keys).removed {
        cmd.env_remove(name);
    }
    if let Some(m) = &cfg.model {
        cmd.args(["--model", m]);
    }
    if !cfg.effort.is_empty() {
        cmd.args(["--effort", &cfg.effort]);
    }
    Ok(cmd)
}

/// Why a claude didn't start.
#[derive(Debug)]
pub enum SpawnError {
    /// Its program didn't start (`spawn_failed` says it).
    Start(std::io::Error),
    /// Its billing can't be laid into its arguments (`billing::args`).
    Billing(String),
}

impl SpawnError {
    /// What the chat (`whole`: every line) or a toast says of it.
    pub fn said(&self, command: &str, whole: bool) -> String {
        match self {
            SpawnError::Start(e) => spawn_failed(command, e, whole),
            SpawnError::Billing(why) => why.clone(),
        }
    }
}

// ── Claude's environment ────────────────────────────────────────────────
//
// tarae's environment can hold another Claude Code session's variables (apibill L0: tarae started from a shell a
// desktop-app session started — `CLAUDECODE`, `CLAUDE_CODE_CHILD_SESSION`, its session id, entrypoint, effort, pid,
// its host's account hints), and an exported API key. Passed on, a top-level chat is taken for a nested one —
// "transcript persistence, prompt history, and `claude agents` registration" off (Claude Code's env-vars
// reference, `CLAUDE_CODE_FORCE_SESSION_PERSISTENCE`) — and a key in the environment bills in your login's place.
// A named list, not a `CLAUDE*` prefix: most such names are settings you may export on purpose
// (`CLAUDE_CODE_MAX_OUTPUT_TOKENS`, `MCP_TIMEOUT`, `ANTHROPIC_BASE_URL`) — those stay.

/// Credentials Claude Code takes from its environment over your login (its authentication docs: under `-p` the
/// key "is always used when present"): not passed — unless you keep them (`llm.environment-key = "keep"`: a
/// login by key alone, a gateway's `ANTHROPIC_BASE_URL` with its token), and never inside a Claude Code session,
/// where they're the host's. `CLAUDE_CODE_OAUTH_TOKEN` is the host's there too (`THE_HOSTS`), else a subscription
/// login you chose (`claude setup-token`) — passed.
const CREDENTIALS: [&str; 2] = ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"];

/// What Claude Code (or its Agent SDK) sets in the processes it starts — inherited, always a parent session's,
/// never passed. Claude Code's env-vars reference says so of each, but where noted.
const SET_FOR_CHILDREN: [&str; 12] = [
    // (the SDK drops it from what it inherits as well)
    "CLAUDECODE",
    // ("reliably distinguishes a nested session")
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_REMOTE",
    "CLAUDE_CODE_REMOTE_SESSION_ID",
    "CLAUDE_EFFORT",
    "CLAUDE_PID",
    "CLAUDE_JOB_DIR",
    // (hooks' and file suggestions' — the settings reference; apibill L5 saw claude set it for its children)
    "CLAUDE_PROJECT_DIR",
    // (the SDK's transport sets both: how its host started claude — L5 again for the entrypoint)
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_AGENT_SDK_VERSION",
    // (the changelog: "for subprocesses so `gh` can attribute traffic to Claude Code")
    "AI_AGENT",
];

/// …and whole families of them: Remote Control's, and the session inbox's ("Set by Claude Code, not by you").
const SET_FOR_CHILDREN_FAMILIES: [&str; 2] = ["CLAUDE_CODE_BRIDGE_", "CLAUDE_CODE_MESSAGING_"];

/// A Claude Code host's own, seen in tarae's environment (L0) or named by the SDK as what its host passes — no doc
/// lists them as yours to set. Left out only when tarae itself runs inside Claude Code (`ClaudeEnv::nested`).
const THE_HOSTS: [&str; 10] = [
    // (the SDK: what a host passes the claude it starts — its login among them)
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_HOST_SESSION_ID",
    "CLAUDE_CODE_SESSION_ACCESS_TOKEN",
    "CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING",
    // (L0's, undocumented)
    "CLAUDE_CODE_SDK_HAS_HOST_AUTH_REFRESH",
    "CLAUDE_CODE_DESKTOP_APP_VERSION",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_CODE_WORKER_EPOCH",
    // (the SDK passes its span's W3C context, and a `-p` claude parents its own spans under it — the
    // monitoring docs)
    "TRACEPARENT",
    "TRACESTATE",
];

/// What tells that tarae itself runs inside a Claude Code session: a parent session's own marks, set only by
/// Claude Code (or its host) in what it starts — `CLAUDE_CODE_CHILD_SESSION` ("only set by Claude Code itself …
/// and not by IDE extensions") and `CLAUDE_PID` in its tools' and hooks' processes, `CLAUDE_CODE_SESSION_ID` in
/// those and its stdio MCP servers', `CLAUDE_CODE_HOST_SESSION_ID` what an SDK host passes the claude it starts
/// (L0 found it in tarae's). Not `CLAUDECODE`: an IDE's terminal sets it too, and a shell there is no session's —
/// your own `CLAUDE_CODE_OAUTH_TOKEN` stays.
const NESTED_BY: [&str; 4] =
    ["CLAUDE_CODE_CHILD_SESSION", "CLAUDE_PID", "CLAUDE_CODE_SESSION_ID", "CLAUDE_CODE_HOST_SESSION_ID"];

/// The OAuth scopes go with their refresh token ("Required when `CLAUDE_CODE_OAUTH_REFRESH_TOKEN` is set"): with
/// it, they're yours; without it, inside Claude Code, the host's.
const SCOPES: (&str, &str) = ("CLAUDE_CODE_OAUTH_SCOPES", "CLAUDE_CODE_OAUTH_REFRESH_TOKEN");

/// What claude gets of tarae's environment: the names left out, and why — names only, never a value.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClaudeEnv {
    /// Left out (`env_remove`), sorted.
    pub removed: Vec<String>,
    /// tarae itself runs inside a Claude Code session (`NESTED_BY`).
    pub nested: bool,
    /// Nested: the `CLAUDE*`/`ANTHROPIC_*` names passed as they are — your settings, or the host's under a name
    /// tarae doesn't know yet; said, so the lists can grow. Sorted.
    pub passed: Vec<String>,
}

/// The rules over `names` (tarae's environment's — names, not values: none is needed, so none is read).
/// `keep_keys`: `llm.environment-key = "keep"`.
pub fn claude_env<'a>(names: impl IntoIterator<Item = &'a str>, keep_keys: bool) -> ClaudeEnv {
    let mut names: Vec<&str> = names.into_iter().collect();
    crate::sorting::sort(&mut names);
    names.dedup();
    let has = |n: &str| names.contains(&n);
    let nested = NESTED_BY.iter().any(|n| has(n));
    let mut out = ClaudeEnv { nested, ..Default::default() };
    for &name in &names {
        let removed = CREDENTIALS.contains(&name) && (nested || !keep_keys)
            || SET_FOR_CHILDREN.contains(&name)
            || SET_FOR_CHILDREN_FAMILIES.iter().any(|f| name.starts_with(f))
            || nested && THE_HOSTS.contains(&name)
            || nested && name == SCOPES.0 && !has(SCOPES.1);
        if removed {
            out.removed.push(name.to_string());
        } else if nested && (name.starts_with("CLAUDE") || name.starts_with("ANTHROPIC_")) {
            out.passed.push(name.to_string());
        }
    }
    out
}

/// A stand-in for tarae's environment, by name (`Editor::claude_env`).
pub type Env = std::collections::HashMap<String, String>;

/// The process's environment's names (a name that isn't UTF-8 is none of the rules').
fn process_names() -> Vec<String> {
    std::env::vars_os().filter_map(|(k, _)| k.into_string().ok()).collect()
}

/// Whether a rule of `claude_env` judges `name` — what a stand-in's claude never inherits from the process
/// (`command`): the process's own such names (tarae's tests run inside a Claude Code session too) would be judged
/// on nothing.
fn judged(name: &str) -> bool {
    CREDENTIALS.contains(&name)
        || SET_FOR_CHILDREN.contains(&name)
        || SET_FOR_CHILDREN_FAMILIES.iter().any(|f| name.starts_with(f))
        || THE_HOSTS.contains(&name)
        || NESTED_BY.contains(&name)
        || name == SCOPES.0
        || name == SCOPES.1
}

impl ClaudeEnv {
    /// The rules over tarae's environment — the process's, or `given`, a test's stand-in (`Editor::claude_env`),
    /// alone: what's left out, what's said and what the key card offers never depend on where the tests run, and
    /// what claude inherits of the process's names the rules judge goes (`command`), so a stand-in is the whole of
    /// it there too.
    pub fn of(given: Option<&Env>, keep_keys: bool) -> ClaudeEnv {
        let names = given.map_or_else(process_names, |g| g.keys().cloned().collect());
        claude_env(names.iter().map(String::as_str), keep_keys)
    }

    /// The keys left out (`CREDENTIALS`).
    pub(crate) fn keys(&self) -> Vec<&str> {
        self.removed.iter().map(String::as_str).filter(|n| CREDENTIALS.contains(n)).collect()
    }

    /// Why a key left out stays out, and what passes it: the setting — or, inside a Claude Code session, nothing.
    fn keys_why(&self, keys: &[&str]) -> String {
        let (it, its) = if keys.len() == 1 { ("it", "it's") } else { ("them", "they're") };
        match self.nested {
            true => {
                format!("inside a Claude Code session {its} the host's, whatever llm.environment-key says")
            }
            false => format!("llm.environment-key = \"keep\" in your config passes {it}"),
        }
    }

    /// What the chat says of it, names only — None: nothing left out.
    pub fn said(&self) -> Option<String> {
        let keys = self.keys();
        let rest: Vec<&str> = self.removed.iter().map(String::as_str).filter(|n| !keys.contains(n)).collect();
        let mut said = Vec::new();
        if !rest.is_empty() {
            let whose = match self.nested {
                true => "tarae runs inside Claude Code — that session's variables aren't passed to Claude",
                false => "Variables Claude Code sets for the processes it starts aren't passed to Claude",
            };
            said.push(format!("{whose}: {}.", rest.join(", ")));
        }
        if !keys.is_empty() {
            let verb = if keys.len() == 1 { "isn't" } else { "aren't" };
            let to = if rest.is_empty() { "to Claude" } else { "either" };
            said.push(format!(
                "{} {verb} passed {to}: Claude uses your Claude Code login ({}).",
                keys.join(" and "),
                self.keys_why(&keys)
            ));
        }
        if said.is_empty() {
            return None;
        }
        if !self.passed.is_empty() {
            said.push(format!("Passed as they are: {}.", self.passed.join(", ")));
        }
        Some(said.join(" "))
    }
}

/// Whether the session in front's chat passes a key from tarae's environment on: `llm.environment-key = "keep"`,
/// or `y` on the key card until the session ends (`envkey.rs`).
pub fn keeps_key(ed: &Editor) -> bool {
    ed.config.llm.keep_env_key || ed.session.env_key == Some(true)
}

/// What claude's environment leaves out, said once since tarae started: one log line, and the chat's note — None:
/// nothing left out, or said already.
pub fn env_told(ed: &mut Editor) -> Option<String> {
    if ed.claude_env_told {
        return None;
    }
    ed.claude_env_told = true;
    let said = ClaudeEnv::of(ed.claude_env.as_ref(), keeps_key(ed)).said()?;
    crate::log::line("claude", "info", &said);
    Some(said)
}

/// Claude Code's words when it has no login (its errors reference, under `-p`: "Not logged in · Please run /login",
/// "Failed to authenticate…", "Please run /login · API Error: 401…").
pub const NOT_LOGGED_IN: [&str; 3] = ["Not logged in", "Please run /login", "Failed to authenticate"];

/// What a claude that failed with `error` for want of a login is told when tarae left a key out — None: not a
/// login failure, or none left out (claude's own words say `/login`, as your own claude's do). With the key card
/// up for it (`envkey::offered`), one line pointing there.
pub fn login_hint(ed: &mut Editor, error: &str) -> Option<String> {
    if !NOT_LOGGED_IN.iter().any(|l| error.contains(l)) {
        return None;
    }
    let env = ClaudeEnv::of(ed.claude_env.as_ref(), keeps_key(ed));
    let keys = env.keys();
    if keys.is_empty() {
        return None;
    }
    // (the key card says the rest — `envkey.rs`: the chat only points at it)
    if crate::envkey::offered(ed, &env) {
        return Some(format!(
            "A card asks whether Claude takes {} from tarae's environment.",
            keys.join(" and ")
        ));
    }
    Some(format!(
        "Claude has no login of its own, and tarae didn't pass on {} from its environment ({}). Or log in: run \
         `claude` in a terminal, then /login.",
        keys.join(" and "),
        env.keys_why(&keys)
    ))
}

/// One user message line (stream-json input).
pub fn user_message(content: &str) -> String {
    user_content(serde_json::json!(content))
}

/// One user message line whose content is a string or a list of content blocks.
pub fn user_content(content: serde_json::Value) -> String {
    serde_json::json!({"type": "user", "message": {"role": "user", "content": content}}).to_string()
}

/// The first line to a chat process: registers tarae's own MCP server (`sdkMcpServers` — its messages come
/// back as `mcp_message` control requests).
pub fn initialize_message(servers: &[&str]) -> String {
    serde_json::json!({"type": "control_request", "request_id": "tarae-init",
        "request": {"subtype": "initialize", "sdkMcpServers": servers}})
    .to_string()
}

/// Our answer to a control request from claude.
pub fn control_response(request_id: &str, response: serde_json::Value) -> String {
    serde_json::json!({"type": "control_response",
        "response": {"subtype": "success", "request_id": request_id, "response": response}})
    .to_string()
}

/// Control request that stops the running turn — the process and conversation memory remain.
pub fn interrupt_message() -> String {
    serde_json::json!({"type": "control_request", "request_id": "tarae-stop", "request": {"subtype": "interrupt"}})
        .to_string()
}

/// The claude processes `pump` reads that aren't reaped yet — the only pids `kill` signals. A reaped one's pid
/// is the system's to give anyone: `pump` reaped it, then waited for stderr before its exit's event cleared the
/// pid where it's kept, and a kill meanwhile (a chat reset, an editor dropped) went to whoever had it by then
/// (frictions28 T1: an e2e test's `tarae state` got a SIGKILL from nowhere in it).
static UNREAPED: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());

/// Force-kills the process (cancel) — the reader thread ends on EOF. Only one `pump` hasn't reaped.
pub fn kill(pid: u32) {
    let unreaped = UNREAPED.lock().unwrap_or_else(|e| e.into_inner());
    if unreaped.contains(&pid) {
        // SAFETY: a child of tarae's not reaped yet — `pump` reaps it only under this lock: the pid is still its.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    }
}

/// `child` reaped once it has ended: waited for without reaping (`WNOWAIT`), then taken out of `UNREAPED` and
/// reaped under its lock — so a `kill` either reaches it or sends nothing.
fn reap(child: &mut Child) {
    let pid = child.id();
    // SAFETY: a zeroed siginfo_t is a valid one to fill; waitid only waits for our own child here.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let flags = libc::WEXITED | libc::WNOWAIT;
    // SAFETY: as above.
    while unsafe { libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, flags) } != 0
        && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
    {}
    let mut unreaped = UNREAPED.lock().unwrap_or_else(|e| e.into_inner());
    unreaped.retain(|&p| p != pid);
    let _ = child.wait();
}

// ── Writing to it ───────────────────────────────────────────────────────

/// A claude's stdin, written on a thread of its own, lines in the order sent. A claude still starting (a
/// cold start is 8–10 s) reads nothing yet, and a pipe takes 64 KB before a write waits — one message can
/// carry the file, other unsaved buffers and a screen: measured, a 200 KB message held the main loop as long
/// as the reader didn't read (2.0 s for a stand-in sleeping 2 s). Dropped: what's queued still goes, then
/// stdin closes.
pub struct Lines(std::sync::mpsc::Sender<String>);

impl Lines {
    /// Tests: lines that go to `tx`, no process, no thread (`Editor.claude_lines`).
    #[cfg(test)]
    pub fn to(tx: std::sync::mpsc::Sender<String>) -> Lines {
        Lines(tx)
    }

    pub fn new(mut stdin: std::process::ChildStdin) -> Lines {
        let (tx, rx) = channel::<String>();
        crate::event::thread("tarae-llm-write", move || {
            while let Ok(line) = rx.recv() {
                // (a failed write: the process is gone — `pump` tells its end)
                if writeln!(stdin, "{line}").and_then(|_| stdin.flush()).is_err() {
                    break;
                }
            }
        });
        Lines(tx)
    }

    /// Err: an earlier write failed — the process doesn't read anymore.
    pub fn send(&self, line: String) -> std::io::Result<()> {
        self.0.send(line).map_err(|_| std::io::Error::from(std::io::ErrorKind::BrokenPipe))
    }

    /// A stand-in claude's stdin (no process, no thread — a simulation's): what's sent, read from the receiver.
    #[cfg(test)]
    pub fn stand_in() -> (Lines, std::sync::mpsc::Receiver<String>) {
        let (tx, rx) = channel::<String>();
        (Lines(tx), rx)
    }
}

// ── Reading the streamed answer ─────────────────────────────────────────

/// What one stdout line means.
pub enum StreamEv {
    /// Thinking (thinking fragments·token estimate).
    Thinking,
    /// Summarized thinking text (chat with `--thinking-display summarized` — batched like `Text`).
    Thought(String),
    /// Answer text fragment (batched every 25 ms).
    Text(String),
    /// A tool call (chat panel — Read·Grep·Glob with its full input).
    Tool { id: String, name: String, input: serde_json::Value },
    /// What a tool returned (its first 4 KB — enough to see where a search landed).
    ToolResult { id: String, text: String, error: bool },
    /// claude asks the host something (`mcp_message` for tarae's own tools) — needs a `control_response`.
    Control { request_id: String, request: serde_json::Value },
    /// The process's conversation (claude's `session_id`), the model it runs, Claude Code's version and the
    /// credential it reported it runs on (`apiKeySource` — `limits.rs`) (`system init`, None: not sent) — the
    /// chat resumes it later (`--resume`).
    Conversation { id: String, model: String, version: Option<String>, key_source: Option<String> },
    /// The model that wrote an answer (an assistant message's `message.model`, or a stream's
    /// `message_start`) — sent when it changes.
    Answered(String),
    /// Its answer to our `initialize`: the models it offers (the chat's model picker), and the credential it runs
    /// on (`account.apiKeySource` — `limits.rs`; None: not said), known as it starts rather than with a turn.
    Initialized { models: Vec<ModelInfo>, key_source: Option<String>, account: Option<crate::usage::Account> },
    /// A request goes out (`system` `status: "requesting"`), when it was read (unix ms) — `usage.rs`.
    Requesting(u64),
    /// A request's usage, as a line reported it (`message_start`, `message_delta`, an assistant message).
    Usage(crate::usage::Seen),
    /// The turn's result as far as usage goes — sent before its `Done`.
    Spent(crate::usage::Spent),
    /// claude compacting the conversation: begun, over, the compaction itself (`compaction.rs`).
    Compaction(crate::compaction::Ev),
    /// Where the plan's limits stand — a `rate_limit_event`, or `get_usage`'s answer (`limits.rs`).
    Limits(crate::limits::Report),
    /// A request failed and claude retries it (`system/api_retry`).
    Retry(crate::limits::Retry),
    /// A request refused for money — the key's credits or spend limit (`billing::refusal`); before its `Done`.
    Money(crate::billing::Money),
    /// End of a turn — final text or error (not logged in·stopped etc.).
    Done(Result<String, String>),
    /// The process exited (for the chat panel — last stderr line).
    Exit(String),
}

/// Reader thread body: parses stdout line by line and sends closures built by `route` to the main loop.
/// With `once`, reaps the process after the first turn (ask). Otherwise runs until the process exits (chat).
/// stderr is drained separately (so a full pipe doesn't stall the child) — its last line is the error text.
pub fn pump(
    mut child: Child,
    once: bool,
    tx: crate::event::Tx,
    route: impl Fn(StreamEv) -> Option<Apply> + Send + 'static,
) {
    UNREAPED.lock().unwrap_or_else(|e| e.into_inner()).push(child.id());
    crate::event::thread("tarae-llm-read", move || {
        // stderr's last line arrives when the pipe closes
        let (err_tx, err_rx) = channel::<String>();
        if let Some(err) = child.stderr.take() {
            crate::event::thread("tarae-llm-err", move || {
                let mut last = String::new();
                for line in BufReader::new(err).lines().map_while(Result::ok) {
                    if !line.trim().is_empty() {
                        last = line;
                    }
                }
                let _ = err_tx.send(last);
            });
        }
        let send = |ev: StreamEv| route(ev).is_none_or(|apply| tx.send(Event::Job(apply)).is_ok());
        // Streamed text not yet sent — answer text or (`pending_thought`) thinking summary
        let mut pending = String::new();
        let mut pending_thought = false;
        let flush = |pending: &mut String, thought: bool| {
            let t = std::mem::take(pending);
            t.is_empty() || send(if thought { StreamEv::Thought(t) } else { StreamEv::Text(t) })
        };
        let mut last_flush = Instant::now();
        let mut thinking_sent = false;
        let mut done = false;
        let mut model = String::new();
        if let Some(out) = child.stdout.take() {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let Ok(ev) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
                // (what it costs — the same line may carry a tool call or the turn's end too)
                if let Some(u) = usage_line(&ev, crate::usage::now_ms())
                    && !send(u)
                {
                    break;
                }
                if let Some(m) = crate::billing::refusal(&ev)
                    && !send(StreamEv::Money(m))
                {
                    break;
                }
                if let Some(m) = answered_by(&ev).filter(|m| *m != model) {
                    model = m.to_string();
                    if !send(StreamEv::Answered(model.clone())) {
                        break;
                    }
                }
                // Tool calls arrive whole in the assistant message — text streamed before them goes first
                let tools = tool_events(&ev);
                if !tools.is_empty() {
                    if !flush(&mut pending, pending_thought) {
                        break;
                    }
                    thinking_sent = false;
                    if !tools.into_iter().all(&send) {
                        break;
                    }
                    continue;
                }
                match parse_line(&ev) {
                    Some(ev @ (StreamEv::Text(_) | StreamEv::Thought(_))) => {
                        let (t, thought) = match ev {
                            StreamEv::Thought(t) => (t, true),
                            StreamEv::Text(t) => (t, false),
                            _ => unreachable!(),
                        };
                        if thought != pending_thought {
                            if !flush(&mut pending, pending_thought) {
                                break;
                            }
                            pending_thought = thought;
                        }
                        pending.push_str(&t);
                        thinking_sent = thought;
                        if last_flush.elapsed() >= Duration::from_millis(25) {
                            last_flush = Instant::now();
                            if !flush(&mut pending, pending_thought) {
                                break;
                            }
                        }
                    }
                    // Answered on the main loop · said where it happened (text streamed before it goes first)
                    Some(c @ (StreamEv::Control { .. } | StreamEv::Compaction(_))) => {
                        if !flush(&mut pending, pending_thought) || !send(c) {
                            break;
                        }
                    }
                    Some(
                        e @ (StreamEv::Conversation { .. }
                        | StreamEv::Initialized { .. }
                        | StreamEv::Limits(_)),
                    ) => {
                        if !send(e) {
                            break;
                        }
                    }
                    Some(StreamEv::Thinking) if !thinking_sent => {
                        thinking_sent = true;
                        send(StreamEv::Thinking);
                    }
                    Some(d @ StreamEv::Done(_)) => {
                        flush(&mut pending, pending_thought);
                        thinking_sent = false;
                        let ok = send(d);
                        if once || !ok {
                            done = true;
                            break;
                        }
                    }
                    _ => {}
                }
            }
        }
        if once {
            let _ = child.kill();
        }
        reap(&mut child);
        // Let stderr drain after exit (else the real error is lost) — bounded, in case a grandchild holds it
        let err = err_rx.recv_timeout(Duration::from_millis(500)).unwrap_or_default();
        if once && !done {
            let msg = if err.is_empty() { "exited without a result".to_string() } else { err };
            send(StreamEv::Done(Err(msg)));
        } else if !once {
            send(StreamEv::Exit(err));
        }
    });
}

/// `tool_use` blocks of a complete assistant message · `tool_result` blocks of the user message that
/// answers them.
fn tool_events(ev: &serde_json::Value) -> Vec<StreamEv> {
    let Some(content) = ev["message"]["content"].as_array() else { return Vec::new() };
    let id = |b: &serde_json::Value, k: &str| b[k].as_str().unwrap_or_default().to_string();
    match ev["type"].as_str() {
        Some("assistant") => content
            .iter()
            .filter(|b| b["type"] == "tool_use")
            .map(|b| StreamEv::Tool { id: id(b, "id"), name: id(b, "name"), input: b["input"].clone() })
            .collect(),
        Some("user") => content
            .iter()
            .filter(|b| b["type"] == "tool_result")
            .map(|b| {
                // A string, or text blocks
                let mut text = match &b["content"] {
                    serde_json::Value::String(s) => s.clone(),
                    serde_json::Value::Array(parts) => {
                        parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n")
                    }
                    _ => String::new(),
                };
                if text.len() > 4096 {
                    let cut = (0..=4096).rev().find(|&i| text.is_char_boundary(i)).unwrap_or(0);
                    text.truncate(cut);
                }
                let error = b["is_error"].as_bool() == Some(true);
                StreamEv::ToolResult { id: id(b, "tool_use_id"), text, error }
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The model that wrote this answer, if the line says — not claude's own stand-ins (`<synthetic>`, for an
/// answer it made up after an error).
fn answered_by(ev: &serde_json::Value) -> Option<&str> {
    let m = match ev["type"].as_str()? {
        "assistant" => &ev["message"]["model"],
        "stream_event" if ev["event"]["type"] == "message_start" => &ev["event"]["message"]["model"],
        _ => return None,
    };
    m.as_str().filter(|m| !m.is_empty() && !m.starts_with('<'))
}

/// What a line says about usage (`usage.rs` has what's where), the plan's limits or a retry (`limits.rs`), read
/// at `at_ms` — None: nothing.
pub fn usage_line(ev: &serde_json::Value, at_ms: u64) -> Option<StreamEv> {
    use crate::usage::{Counts, Seen, Spent, Ttl};
    let seen = |id: Option<&serde_json::Value>, model: &serde_json::Value, u: &serde_json::Value| {
        Some(StreamEv::Usage(Seen {
            id: match id {
                Some(id) => Some(id.as_str()?.to_string()),
                None => None,
            },
            model: model.as_str().unwrap_or_default().to_string(),
            counts: Some(Counts::of(u)).filter(Counts::any)?,
            ttl: Ttl::of(u),
            at_ms,
        }))
    };
    match ev["type"].as_str()? {
        "system" if ev["subtype"] == "status" && ev["status"] == "requesting" => {
            Some(StreamEv::Requesting(at_ms))
        }
        "system" if ev["subtype"] == "api_retry" => crate::limits::of_retry(ev).map(StreamEv::Retry),
        "rate_limit_event" => crate::limits::of_event(ev, at_ms).map(StreamEv::Limits),
        "stream_event" => {
            let e = &ev["event"];
            match e["type"].as_str()? {
                "message_start" => {
                    seen(Some(&e["message"]["id"]), &e["message"]["model"], &e["message"]["usage"])
                }
                "message_delta" => seen(None, &serde_json::Value::Null, &e["usage"]),
                _ => None,
            }
        }
        // (claude's own stand-in after an error, `<synthetic>`, cost nothing)
        "assistant" if !ev["message"]["model"].as_str().unwrap_or_default().starts_with('<') => {
            let m = &ev["message"];
            seen(Some(&m["id"]), &m["model"], &m["usage"])
        }
        "result" => {
            let models = ev["modelUsage"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(name, m)| (name.clone(), m["contextWindow"].as_u64()))
                .collect();
            // (the tokens `modelUsage` counts, the process's so far, against this result's)
            const OWN: [&str; 4] =
                ["input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens", "output_tokens"];
            const MODEL: [&str; 4] =
                ["inputTokens", "cacheReadInputTokens", "cacheCreationInputTokens", "outputTokens"];
            let sum = |v: &serde_json::Value, keys: [&str; 4]| {
                keys.iter().map(|k| v[*k].as_u64().unwrap_or(0)).sum::<u64>()
            };
            let alone = ev["modelUsage"]
                .as_object()
                .map(|m| m.values().map(|v| sum(v, MODEL)).sum::<u64>() == sum(&ev["usage"], OWN));
            Some(StreamEv::Spent(Spent {
                conversation: ev["session_id"].as_str().unwrap_or_default().to_string(),
                counts: Counts::of(&ev["usage"]),
                total_cost: ev["total_cost_usd"].as_f64(),
                models,
                alone,
                ms: ev["duration_ms"].as_u64(),
                at_ms,
            }))
        }
        _ => None,
    }
}

/// `args` with `flag`'s value replaced (the flag appended if it wasn't there).
pub fn with_flag(args: &[String], flag: &str, value: &str) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len() + 2);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == flag {
            it.next();
        } else {
            out.push(a.clone());
        }
    }
    out.extend([flag.to_string(), value.to_string()]);
    out
}

/// `args` with its `--tools <list>` swapped for `tools` (read-only ones for the chat panel), and `allowed`
/// (those + tarae's own MCP tools) allowed up front, so `-p` never stops to ask.
pub fn with_tools(args: &[String], tools: &str, allowed: &str) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len() + 4);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--tools" {
            it.next();
        } else {
            out.push(a.clone());
        }
    }
    out.extend(["--tools", tools, "--allowedTools", allowed].map(String::from));
    out
}

pub(crate) fn parse_line(ev: &serde_json::Value) -> Option<StreamEv> {
    match ev["type"].as_str()? {
        "control_request" => Some(StreamEv::Control {
            request_id: ev["request_id"].as_str()?.to_string(),
            request: ev["request"].clone(),
        }),
        "stream_event" => {
            let delta = &ev["event"]["delta"];
            match delta["type"].as_str()? {
                "text_delta" => Some(StreamEv::Text(delta["text"].as_str()?.to_string())),
                "thinking_delta" => Some(match delta["thinking"].as_str().filter(|t| !t.is_empty()) {
                    Some(t) => StreamEv::Thought(t.to_string()),
                    None => StreamEv::Thinking,
                }),
                _ => None,
            }
        }
        "system" if ev["subtype"] == "thinking_tokens" => Some(StreamEv::Thinking),
        // (`status: "requesting"` is usage's — `usage_line`)
        "system" if ev["subtype"] == "compact_boundary" || ev["subtype"] == "status" => {
            crate::compaction::Ev::of(ev, crate::usage::now_ms()).map(StreamEv::Compaction)
        }
        "system" if ev["subtype"] == "init" => Some(StreamEv::Conversation {
            id: ev["session_id"].as_str()?.to_string(),
            model: ev["model"].as_str().unwrap_or_default().to_string(),
            version: ev["claude_code_version"].as_str().filter(|v| !v.is_empty()).map(String::from),
            key_source: ev["apiKeySource"].as_str().map(String::from),
        }),
        "control_response" if ev["response"]["request_id"] == "tarae-init" => {
            let r = &ev["response"]["response"];
            Some(StreamEv::Initialized {
                models: ModelInfo::list(&r["models"]),
                key_source: r["account"]["apiKeySource"].as_str().map(String::from),
                account: crate::usage::Account::of(&r["account"]),
            })
        }
        "control_response" if ev["response"]["request_id"] == crate::limits::REQUEST_ID => {
            Some(StreamEv::Limits(crate::limits::of_answer(&ev["response"], crate::usage::now_ms())))
        }
        "result" => {
            let text = ev["result"].as_str().unwrap_or_default().to_string();
            let failed = ev["is_error"].as_bool() == Some(true)
                || ev["subtype"].as_str().is_some_and(|s| s.starts_with("error"));
            Some(StreamEv::Done(if failed {
                Err(if text.is_empty() { "stopped".into() } else { text })
            } else {
                Ok(text)
            }))
        }
        _ => None,
    }
}

// ── Prompt·response ─────────────────────────────────────────────────────

pub fn build_prompt(
    instruction: &str,
    file: &str,
    text: &Rope,
    ranges: &[(usize, usize)],
    ctx: usize,
) -> String {
    let mut p = String::from(
        "You are the editing engine inside a text editor. Rewrite each <selection> according to the \
         instruction.\nReply with ONLY the replacement text of every selection, each wrapped in \
         <r i=\"N\">...</r> in the same order — no explanations, no markdown fences, nothing else. \
         Keep the surrounding code style and indentation. If a selection needs no change, return it \
         unchanged.\n\n",
    );
    p.push_str(&format!("Instruction: {instruction}\nFile: {file}\n"));
    let last_line = text.len_lines().saturating_sub(1);
    for (i, &(from, to)) in ranges.iter().enumerate() {
        // Last line = that of the last selected byte (a line-wise selection ends at the next line's start)
        let (l0, l1) = (text.byte_to_line(from), text.byte_to_line(to.saturating_sub(1).max(from)));
        let c0 = text.line_to_byte(l0.saturating_sub(ctx));
        let c1 = text.line_to_byte((l1 + ctx + 1).min(last_line + 1));
        p.push_str(&format!(
            "\n<selection i=\"{}\" lines=\"{}-{}\">\n<before>{}</before>\n<text>{}</text>\n<after>{}</after>\n</selection>\n",
            i + 1,
            l0 + 1,
            l1 + 1,
            text.byte_slice(c0..from),
            text.byte_slice(from..to),
            text.byte_slice(to..c1.max(to)),
        ));
    }
    p
}

/// Extracts N `<r i="N">…</r>`. With a single selection, an untagged answer is accepted too.
pub fn parse_replies(out: &str, n: usize) -> Result<Vec<String>, String> {
    let mut replies = Vec::with_capacity(n);
    for i in 1..=n {
        let open = format!("<r i=\"{i}\">");
        let found = out.find(&open).and_then(|s| {
            let body = &out[s + open.len()..];
            body.find("</r>").map(|e| &body[..e])
        });
        match found {
            Some(body) => {
                let body = body.strip_prefix('\n').unwrap_or(body);
                replies.push(body.strip_suffix('\n').unwrap_or(body).to_string());
            }
            None if n == 1 => replies.push(strip_fences(out.trim_matches('\n')).to_string()),
            None => return Err(format!("reply {i}/{n} missing")),
        }
    }
    Ok(replies)
}

fn strip_fences(s: &str) -> &str {
    let Some(rest) = s.strip_prefix("```") else { return s };
    let rest = rest.split_once('\n').map_or("", |(_, r)| r);
    rest.strip_suffix("```").map(|r| r.strip_suffix('\n').unwrap_or(r)).unwrap_or(rest)
}

/// If the original ended with a newline (line-wise selection), end the answer with one; otherwise strip it.
pub fn match_trailing_newline(old: &str, new: String) -> String {
    match (old.ends_with('\n'), new.ends_with('\n')) {
        (true, false) => new + "\n",
        (false, true) => new.trim_end_matches('\n').to_string(),
        _ => new,
    }
}

/// Line-level diff → (' ' | '-' | '+', line).
pub fn line_diff<'a>(old: &'a str, new: &'a str) -> Vec<(char, &'a str)> {
    use imara_diff::intern::InternedInput;
    use imara_diff::{Algorithm, diff};
    // imara's `&str` tokens = `str::lines` (terminator excluded) — indexes line up
    let (a, b): (Vec<&str>, Vec<&str>) = (old.lines().collect(), new.lines().collect());
    let input = InternedInput::new(old, new);
    let mut out = Vec::with_capacity(a.len().max(b.len()));
    let mut next = 0; // first old line not yet emitted
    diff(Algorithm::Histogram, &input, |before: std::ops::Range<u32>, after: std::ops::Range<u32>| {
        let (bs, be) = (before.start as usize, before.end as usize);
        out.extend(a[next..bs].iter().map(|l| (' ', *l)));
        out.extend(a[bs..be].iter().map(|l| ('-', *l)));
        out.extend(b[after.start as usize..after.end as usize].iter().map(|l| ('+', *l)));
        next = be;
    });
    out.extend(a[next..].iter().map(|l| (' ', *l)));
    out
}

// ── Review state ────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Undecided,
    Accept,
    Reject,
}

pub struct Hunk {
    pub from: usize,
    pub to: usize,
    pub old: String,
    pub new: String,
    pub decision: Decision,
    /// The original changed while waiting and can't be located — cannot apply.
    pub conflict: bool,
}

pub struct Review {
    /// Its document's doc comments laid out folded, for a version and width (`term::review_folds`) — the review
    /// shows the whole file, and folding all of it each frame took milliseconds on a big one.
    pub folds: Option<crate::doccomment::Folds>,
    /// Its rows, folded, while what they're built from holds (`term::review_rows`).
    pub kept: Option<crate::codeview::Kept>,
    pub generation: u64,
    pub doc_id: DocId,
    pub instruction: String,
    pub hunks: Vec<Hunk>,
    pub ready: bool,
    pub current: usize,
    pub scroll: usize,
    /// Columns scrolled sideways (`h`/`l`) — lines not wrapped: a long one was cut at the edge, readable only
    /// with the chat panel closed.
    pub left: usize,
    /// Whether the view follows the current change (off when scrolling directly with j/k).
    pub follow: bool,
    pub started: Instant,
    /// When what it reviews was asked for (a chat edit waits in its queue first) — its place in the inbox.
    pub asked: Instant,
    /// Streamed answer (preview before it's done) · whether thinking.
    pub streamed: String,
    pub thinking: bool,
    /// What to call when done (applied?) — a chat edit or a change set waiting for its answer.
    pub on_done: Option<OnDone>,
    /// Why, in Claude's words: what it thought or said just before it asked for this edit (the chat's — a
    /// row or two under the title, the rest is in the chat).
    pub reason: String,
    /// What the change warns of (`cardwarn.rs`, a fact of the edit's — `edit-protected`: a file that runs later
    /// outside the sandbox): a row each under the title, in the warning color, saying whose.
    pub warnings: Vec<crate::cardwarn::Warned>,
    /// Accepted, it's a step of its own for `space u` (`space i`'s) — a chat edit is part of its turn instead
    /// (`checkpoint.rs`).
    pub own_turn: bool,
    /// The answer's lines wrapped at this width when it lands (a commit message's body at 72 — `wrap_message`).
    pub wrap: Option<usize>,
    /// It took the keys from the chat (a chat edit asked while you were in it): Esc gives them back there —
    /// else to the code (#118).
    pub from_chat: bool,
    /// Esc gave the keys back to the code, the change unanswered: they're the editor's until `C-q`.
    pub away: bool,
    /// Claude's call it answers (its id, `chat::claim_call`): that call's row takes the keys that answered it
    /// (`chat::stamp_call`) — None for your own changes, `space i`'s, or a call not found.
    pub answers: Option<String>,
}

/// Called when the review ends (applied?).
pub type OnDone = Box<dyn FnOnce(&mut Editor, bool)>;

/// Closes the open review as rejected (before another review takes its place — so the waiter gets an answer).
/// An ask still waiting for its answer is killed (its result would no longer find its review).
pub fn close_review(ed: &mut Editor) {
    let Some(mut rev) = ed.session.review.take() else { return };
    if !rev.ready
        && let Some(pid) = ed.llm_pid.take()
    {
        kill(pid);
    }
    crate::reviews::edit_ended(ed, &rev, true, false);
    if let Some(cb) = rev.on_done.take() {
        cb(ed, false);
    }
}

impl Review {
    /// Waiting for an answer (`ready: false`), proposed by Claude, nobody to call when done.
    pub(crate) fn new(generation: u64, doc_id: DocId, instruction: &str, hunks: Vec<Hunk>) -> Self {
        Review {
            generation,
            folds: None,
            kept: None,
            doc_id,
            instruction: instruction.to_string(),
            hunks,
            ready: false,
            current: 0,
            scroll: 0,
            left: 0,
            follow: true,
            started: Instant::now(),
            asked: Instant::now(),
            streamed: String::new(),
            thinking: false,
            on_done: None,
            reason: String::new(),
            warnings: Vec::new(),
            own_turn: true,
            wrap: None,
            from_chat: false,
            away: false,
            answers: None,
        }
    }

    fn next_undecided(&self) -> Option<usize> {
        let n = self.hunks.len();
        (1..=n)
            .map(|d| (self.current + d) % n)
            .find(|&i| self.hunks[i].decision == Decision::Undecided && !self.hunks[i].conflict)
    }

    /// Rows of the in-buffer review (like Cursor): shows the whole document, expanding only the changes —
    /// unwrapped, unfolded (`term::render_frame` folds them, `draw_review` wraps the ones on screen).
    /// Undecided = both removed and added lines, accepted = new text only, rejected = original only.
    pub fn rows(&self, text: &Rope) -> Vec<crate::codeview::Row> {
        use crate::codeview::{Edit, Mark, OtherKind, Row};
        // (the empty line after a final newline isn't one: the drawer shows it only with the cursor there)
        let last = crate::movement::last_line(text);
        let line_of = |b: usize| text.byte_to_line(b.min(text.len_bytes()));
        let doc_row = |line| Row::Text { line, part: None, edit: None };
        let mut order: Vec<usize> = (0..self.hunks.len()).collect();
        crate::sorting::by_key(&mut order, |&i| self.hunks[i].from);
        let mut out = Vec::new();
        let mut next = 0; // first document line not yet emitted
        for i in order {
            let h = &self.hunks[i];
            let lf = line_of(h.from);
            let lt = if h.to > h.from { line_of(h.to - 1) } else { lf };
            out.extend((next..lf.max(next)).map(doc_row));
            out.push(Row::Header { hunk: i, at: lf });
            next = next.max(lt + 1);
            if h.conflict {
                out.extend((lf..=lt).map(doc_row));
                continue;
            }
            // The lines it takes out or keeps are the document's own rows while it holds what the change replaces;
            // edited since (the editor's keys work while the review waits — #61), the text it was asked on
            let holds = text.get_byte_slice(h.from..h.to).is_some_and(|s| s == h.old.as_str());
            // Changes line-wise: attach same-line pieces around the selection so whole lines are compared
            let ls = text.line_to_byte(lf);
            let le = text.line_to_byte((lt + 1).min(last + 1)).max(h.to);
            let (pre, post) =
                (text.byte_slice(ls..h.from.max(ls)).to_string(), text.byte_slice(h.to..le).to_string());
            let old = format!("{pre}{}{post}", h.old);
            let new = format!("{pre}{}{post}", h.new);
            // (the document line the next old line is)
            let mut old_no = lf;
            for (kind, line) in line_diff(&old, &new) {
                let mark = match (h.decision, kind) {
                    (Decision::Accept, '-') => {
                        old_no += 1;
                        continue;
                    }
                    (Decision::Reject, '+') => continue,
                    (_, '+') => Mark::Added { washed: h.decision == Decision::Undecided },
                    (Decision::Reject, '-') | (_, ' ') => Mark::Kept,
                    _ => Mark::Taken,
                };
                let edit = Edit { hunk: i, mark };
                let other =
                    |at| Row::Other { at, text: line.to_string(), kind: OtherKind::Edit(edit), part: None };
                out.push(match mark {
                    Mark::Added { .. } => other(old_no),
                    _ if holds => Row::Text { line: old_no, part: None, edit: Some(edit) },
                    _ => other(old_no),
                });
                if !matches!(mark, Mark::Added { .. }) {
                    old_no += 1;
                }
            }
        }
        out.extend((next..=last).map(doc_row));
        out
    }
}

// ── Commands ────────────────────────────────────────────────────────────

/// When the ask prompt opens or "ask" starts being typed — spawns the spare process (overlapping typing).
pub fn prewarm(ed: &mut Editor) {
    let cfg = ed.config.llm.clone();
    let warmed = ed
        .credential(ed.session.billing, crate::billing::Role::Ask)
        .and_then(|cred| ed.llm_warm.ensure(&cfg, &ed.session.root, ed.claude_env.as_ref(), &cred));
    if let Err(e) = warmed {
        ed.set_error(e);
    }
}

pub fn ask(ed: &mut Editor, instruction: &str) -> Result<(), String> {
    let instruction = instruction.trim();
    if instruction.is_empty() {
        return Err("usage: :ask <instruction>".into());
    }
    if ed.session.review.is_some() {
        return Err("an ask is already in progress (:ask-cancel)".into());
    }
    let cfg = ed.config.llm.clone();
    let doc = ed.doc();
    let ranges: Vec<(usize, usize)> = doc
        .selection()
        .ranges()
        .iter()
        .map(|r| {
            let r = r.min_width_1(&doc.text);
            (r.from(), r.to())
        })
        .collect();
    let prompt =
        build_prompt(instruction, &doc.display_name(&ed.session.root), &doc.text, &ranges, cfg.context_lines);
    let hunks = ranges
        .iter()
        .map(|&(from, to)| Hunk {
            from,
            to,
            old: doc.text.byte_slice(from..to).to_string(),
            new: String::new(),
            decision: Decision::Undecided,
            conflict: false,
        })
        .collect::<Vec<_>>();
    let doc_id = doc.id;
    start(ed, doc_id, instruction, &prompt, hunks, "ask")
}

/// Claude writes what `from..to` of document `doc_id` should be, from `prompt` alone (one answer, no tools) —
/// streamed, then the in-buffer review like `space i`'s (a commit message's draft).
pub fn draft(
    ed: &mut Editor,
    doc_id: DocId,
    label: &str,
    prompt: &str,
    from: usize,
    to: usize,
) -> Result<(), String> {
    if ed.session.review.is_some() {
        return Err("Claude is already answering (:ask-cancel)".into());
    }
    let doc = ed.docs.iter().find(|d| d.id == doc_id).ok_or("the buffer is gone")?;
    if from > to || to > doc.text.len_bytes() {
        return Err("the buffer changed — draft again".into());
    }
    let old = doc.text.byte_slice(from..to).to_string();
    let hunk = Hunk { from, to, old, new: String::new(), decision: Decision::Undecided, conflict: false };
    start(ed, doc_id, label, prompt, vec![hunk], "draft")?;
    // (its time limit: a draft still being written then is stopped, and said)
    let generation = ed.llm_generation;
    ed.events.jobs().spawn(move || {
        std::thread::sleep(DRAFT_LIMIT);
        move |ed: &mut Editor| stop_waiting(ed, Some(generation), DRAFT_LIMIT_SAID)
    });
    Ok(())
}

/// The longest a draft is waited for (a commit message takes seconds): one still being written then is
/// stopped — one ran 6 minutes, its output going on, nothing on screen to stop it with.
pub const DRAFT_LIMIT: std::time::Duration = std::time::Duration::from_secs(120);
const DRAFT_LIMIT_SAID: &str = "still writing after 2 minutes — stopped, nothing changed";

/// Whether Esc now stops the answer being waited for (a draft, `space i`) — the code has the keys in normal
/// mode (`review_key`; the waiting card says `esc stop` then).
pub fn esc_stops(ed: &Editor) -> bool {
    ed.session.review.as_ref().is_some_and(|r| !r.ready)
        && ed.mode == crate::editor::Mode::Normal
        && ed.picker.is_none()
        && ed.prompt.is_none()
        && ed.pending.is_empty()
        && !ed.session.chat.as_ref().is_some_and(|c| c.focused)
}

/// The answer being waited for (of `generation`, or any) is stopped — its process killed, nothing applied —
/// and `said` says why. Nothing waiting (it answered first): nothing happens.
pub fn stop_waiting(ed: &mut Editor, generation: Option<u64>, said: &str) {
    let waiting =
        ed.session.review.as_ref().filter(|r| !r.ready && generation.is_none_or(|g| r.generation == g));
    let Some(what) = waiting.map(|r| r.instruction.clone()) else {
        ed.unchanged = true;
        return;
    };
    if let Some(pid) = ed.llm_pid.take() {
        kill(pid);
    }
    close_review(ed);
    ed.llm_generation += 1;
    ed.set_warning(format!("Claude's {what}: {said}"));
}

/// One answer for `hunks` of `doc_id` (the prompt asks for a `<r i="N">` each): the process, the review
/// waiting for it, the stream into it. `what` asked it (`ask`: `space i` · `draft`: a commit message) — its
/// turn's name in `:usage` and the call-order record.
fn start(
    ed: &mut Editor,
    doc_id: DocId,
    instruction: &str,
    prompt: &str,
    hunks: Vec<Hunk>,
    what: &'static str,
) -> Result<(), String> {
    let cfg = ed.config.llm.clone();
    let cred = ed.credential(ed.session.billing, crate::billing::Role::Ask)?;
    let mut child = ed.llm_warm.take(&cfg, &ed.session.root, ed.claude_env.as_ref(), &cred)?;
    let (billing, keep) = (cred.billing(), cfg.keep_env_key);
    // Write the message and close stdin — the process exits on its own after this one turn (written off the
    // main loop: a commit draft's prompt can be 80 KB, and a spare still starting reads nothing yet).
    // Before anything is installed: a process without stdin leaves no review waiting forever.
    let sent =
        child.stdin.take().ok_or_else(|| "claude: no stdin".to_string()).and_then(|stdin| {
            Lines::new(stdin).send(user_message(prompt)).map_err(|e| format!("claude: {e}"))
        });
    if let Err(e) = sent {
        // Reaped off the main thread
        crate::event::thread("tarae-reap", move || {
            let _ = child.kill();
            let _ = child.wait();
        });
        return Err(e);
    }
    ed.llm_generation += 1;
    let generation = ed.llm_generation;
    let n = hunks.len();
    ed.session.review = Some(Review::new(generation, doc_id, instruction, hunks));
    ed.llm_pid = Some(child.id());
    pump(child, true, ed.events.sender(), move |ev| {
        Some(match ev {
            StreamEv::Thinking | StreamEv::Thought(_) => Box::new(move |ed: &mut Editor| {
                if let Some(r) = ed.session.review.as_mut().filter(|r| r.generation == generation) {
                    r.thinking = true;
                }
            }),
            StreamEv::Text(t) => Box::new(move |ed: &mut Editor| {
                if let Some(r) = ed.session.review.as_mut().filter(|r| r.generation == generation) {
                    r.thinking = false;
                    r.streamed.push_str(&t);
                }
            }),
            StreamEv::Done(r) => {
                let result = r.and_then(|out| parse_replies(&out, n));
                Box::new(move |ed: &mut Editor| on_result(ed, generation, result))
            }
            // (what it cost is the session's too — a turn of its own, beside the chat's conversation; a
            // result that reports nothing has nothing to add)
            StreamEv::Spent(s) if !s.counts.any() && s.total_cost.is_none() => return None,
            StreamEv::Spent(s) => Box::new(move |ed: &mut Editor| {
                ed.session.usage.aside(s, what);
                ed.calls_record();
                crate::usage::view_sync(ed);
            }),
            // (the account's limits, whichever claude says them)
            StreamEv::Limits(r) => Box::new(move |ed: &mut Editor| crate::limits::seen(ed, r)),
            // (the credential it says it runs on — another than its billing's stops it: `billing.rs`)
            StreamEv::Conversation { key_source, .. } => {
                let role = crate::billing::Role::Ask;
                let said = crate::billing::checked(billing, keep, role, key_source.as_deref())?;
                Box::new(move |ed: &mut Editor| {
                    crate::log::line("claude", "warn", &said);
                    stop_waiting(ed, Some(generation), &said);
                })
            }
            // ask runs without tools, and its conversation isn't kept
            StreamEv::Requesting(_)
            | StreamEv::Retry(_)
            | StreamEv::Money(_)
            | StreamEv::Usage(_)
            | StreamEv::Answered(_)
            | StreamEv::Initialized { .. }
            | StreamEv::Tool { .. }
            | StreamEv::ToolResult { .. }
            | StreamEv::Control { .. }
            | StreamEv::Compaction(_)
            | StreamEv::Exit(_) => {
                return None;
            }
        })
    });
    Ok(())
}

/// Cancel: if a request is pending, kill the process (stops tokens too); if reviewing, reject all.
pub fn cancel(ed: &mut Editor) {
    if let Some(pid) = ed.llm_pid.take() {
        kill(pid);
    }
    if ed.session.review.is_some() {
        close_review(ed);
        ed.llm_generation += 1;
        ed.set_status("claude: cancelled");
    }
}

/// Opens a review directly with changes in hand (chat code apply, chat edits, change sets — same y/n flow·one undo as
/// ask) + what to call when done (applied?).
pub fn review_with(
    ed: &mut Editor,
    doc_id: DocId,
    instruction: &str,
    changes: Vec<(usize, usize, String, String)>,
    on_done: Option<OnDone>,
) {
    close_review(ed);
    ed.llm_generation += 1;
    let hunks: Vec<Hunk> = changes
        .into_iter()
        .map(|(from, to, old, new)| Hunk {
            from,
            to,
            decision: if old == new { Decision::Reject } else { Decision::Undecided },
            old,
            new,
            conflict: false,
        })
        .collect();
    if hunks.iter().all(|h| h.decision == Decision::Reject) {
        if let Some(cb) = on_done {
            cb(ed, false);
        }
        return ed.set_status("claude: no changes");
    }
    let mut rev =
        Review { ready: true, on_done, ..Review::new(ed.llm_generation, doc_id, instruction, hunks) };
    // (on the first change still to answer — a no-op change before it is decided already, and `y` there
    // answered nothing: #216)
    rev.current = rev.hunks.iter().position(|h| h.decision == Decision::Undecided).unwrap_or(0);
    ed.session.review = Some(rev);
}

/// A commit message with its body wrapped at `width` (git's convention, 72): the subject line stays as it
/// is; a longer body line breaks between words, a list item's continuation under its text; indented lines
/// (code), trailers (`Co-Authored-By: …`) and words longer than the width stay whole.
pub fn wrap_message(text: &str, width: usize) -> String {
    let mut out = String::new();
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let trailer = line.split_once(": ").is_some_and(|(k, _)| !k.is_empty() && !k.contains(' '));
        if i == 0 || line.chars().count() <= width || line.starts_with([' ', '\t']) || trailer {
            out.push_str(line);
            continue;
        }
        // A list item's lines go on under its text
        let hang = ["- ", "* "].iter().find(|b| line.starts_with(**b)).map_or(0, |b| b.len());
        let mut row = String::new();
        for word in line.split(' ').filter(|w| !w.is_empty()) {
            if !row.is_empty() && row.chars().count() + 1 + word.chars().count() > width {
                out.push_str(&row);
                out.push('\n');
                row = " ".repeat(hang);
            } else if !row.is_empty() && !row.ends_with(' ') {
                row.push(' ');
            }
            row.push_str(word);
        }
        out.push_str(&row);
    }
    out
}

/// Preview of the streaming answer: the last few lines inside the `<r i="N">` being written.
pub fn preview(streamed: &str, rows: usize) -> Vec<&str> {
    let body = match streamed.rfind("<r i=\"") {
        Some(i) => {
            let rest = &streamed[i..];
            rest.find('>').map_or("", |g| &rest[g + 1..])
        }
        None => streamed,
    };
    let body = body.split("</r>").next().unwrap_or_default().trim_start_matches('\n');
    let lines: Vec<&str> = body.lines().collect();
    lines[lines.len().saturating_sub(rows)..].to_vec()
}

fn on_result(ed: &mut Editor, generation: u64, result: Result<Vec<String>, String>) {
    let Some(rev) = ed.session.review.as_mut().filter(|r| r.generation == generation) else { return };
    ed.llm_pid = None;
    let secs = rev.started.elapsed().as_secs_f32();
    let replies = match result {
        Ok(r) => r,
        Err(e) => {
            ed.session.review = None;
            ed.set_error(format!("claude: {e}"));
            return;
        }
    };
    let Some(doc_idx) = ed.docs.iter().position(|d| d.id == rev.doc_id) else {
        ed.session.review = None;
        ed.set_error("claude: buffer was closed");
        return;
    };
    let text = &ed.docs[doc_idx].text;
    let wrap = rev.wrap;
    for (h, new) in rev.hunks.iter_mut().zip(replies) {
        let new = match wrap {
            Some(w) => wrap_message(&new, w),
            None => new,
        };
        h.new = match_trailing_newline(&h.old, new);
        if h.new == h.old {
            h.decision = Decision::Reject;
        } else if !relocate(h, text) {
            h.conflict = true;
        }
    }
    if rev.hunks.iter().all(|h| h.decision == Decision::Reject || h.conflict) {
        let msg = if rev.hunks.iter().any(|h| h.conflict) {
            "claude: text changed while waiting — nothing applied"
        } else {
            "claude: no changes"
        };
        ed.session.review = None;
        ed.set_status(msg);
        return;
    }
    rev.ready = true;
    rev.current =
        rev.hunks.iter().position(|h| h.decision == Decision::Undecided && !h.conflict).unwrap_or(0);
    ed.current = doc_idx;
    ed.set_status(format!("claude answered in {secs:.1}s"));
}

/// If edits happened while waiting, relocates the original (nearest match). False if not found.
fn relocate(h: &mut Hunk, text: &Rope) -> bool {
    // Edits while waiting may leave the old position mid-character — check boundaries before slicing.
    let boundary = |b: usize| b <= text.len_bytes() && text.char_to_byte(text.byte_to_char(b)) == b;
    if boundary(h.from) && boundary(h.to) && h.from <= h.to && text.byte_slice(h.from..h.to) == h.old.as_str()
    {
        return true;
    }
    if h.old.is_empty() {
        return false;
    }
    let s = text.to_string();
    let best = s.match_indices(&h.old).map(|(b, _)| b).min_by_key(|&b| b.abs_diff(h.from));
    match best {
        Some(from) => {
            h.to = from + h.old.len();
            h.from = from;
            true
        }
        None => false,
    }
}

/// Keys during review. True if handled. While it waits, the editor's own keys still work over it (#61): a
/// picker or a prompt open, a key sequence under way (`space W`, `space m c`), `space`, `:` and `C-w` that
/// start one — and `C-d` `C-u` `C-f` `C-b` `C-e` `C-y` (page keys too) scroll it. Esc gives the keys back
/// where they were before it took them, the change unanswered (#70, #118): to the chat if it had them,
/// else to the code — `C-q` brings them back to it.
pub fn review_key(ed: &mut Editor, key: Key) -> bool {
    if ed.picker.is_some() || ed.prompt.is_some() || !ed.pending.is_empty() {
        return false;
    }
    let page = ed.viewport.0.max(2) as isize;
    let chat = ed.session.chat.as_ref().map(|c| c.focused);
    // Still waiting for the answer: Esc stops it (the card says so)
    if key == Key::plain(Code::Esc) && esc_stops(ed) {
        stop_waiting(ed, None, "stopped — nothing changed");
        return true;
    }
    let Some(rev) = ed.session.review.as_mut().filter(|r| r.ready) else { return false };
    let c_q = key.ctrl && !key.alt && key.code == Code::Char('q');
    // Back in the chat: its keys are the chat's until `C-q`
    if chat == Some(true) {
        if c_q {
            rev.away = false;
            if let Some(c) = ed.session.chat.as_mut() {
                c.focused = false;
            }
            return true;
        }
        return false;
    }
    // Back in the code: the editor's until `C-q`
    if rev.away {
        rev.away = !c_q;
        return c_q;
    }
    let scroll = |rev: &mut Review, by: isize| {
        rev.scroll = rev.scroll.saturating_add_signed(by);
        rev.follow = false;
        false
    };
    let done = match (key.code, key.ctrl, key.alt) {
        (Code::Char(' ' | ':'), false, false) | (Code::Char('w'), true, false) => return false,
        (Code::Char('d'), true, false) => scroll(rev, page / 2),
        (Code::Char('u'), true, false) => scroll(rev, -page / 2),
        (Code::Char('f'), true, false) | (Code::PageDown, false, false) => scroll(rev, page),
        (Code::Char('b'), true, false) | (Code::PageUp, false, false) => scroll(rev, -page),
        (Code::Char('e'), true, false) => scroll(rev, 1),
        (Code::Char('y'), true, false) => scroll(rev, -1),
        (Code::Esc, ..) => {
            match ed.session.chat.as_mut().filter(|_| rev.from_chat) {
                Some(c) => {
                    c.focused = true;
                    let key = ed.key_said("C-q");
                    ed.set_status(format!(
                        "back to the chat — the change waits unanswered, {key} answers it"
                    ));
                }
                None => {
                    rev.away = true;
                    ed.mode = crate::editor::Mode::Normal;
                    let key = ed.key_said("C-q");
                    ed.set_status(format!(
                        "back to the code — the change waits unanswered, {key} answers it"
                    ));
                }
            }
            return true;
        }
        _ => return review_answer(ed, key),
    };
    if done {
        finish(ed);
    }
    true
}

/// Columns `h`/`l` scroll a review sideways.
const SIDEWAYS: usize = 8;

/// The review's own keys: `y` `n` `a` answer, `q` ends it, `j` `k` scroll, `h` `l` sideways (lines not
/// wrapped), `tab` goes to the next change.
fn review_answer(ed: &mut Editor, key: Key) -> bool {
    let doc = ed.session.review.as_ref().and_then(|r| ed.docs.iter().find(|d| d.id == r.doc_id));
    let wraps = doc.is_some_and(|d| ed.wraps(d));
    let Some(rev) = ed.session.review.as_mut().filter(|r| r.ready) else { return false };
    let decide = |rev: &mut Review, d: Decision| {
        rev.hunks[rev.current].decision = d;
        rev.follow = true;
        match rev.next_undecided() {
            Some(i) => {
                rev.current = i;
                false
            }
            None => true,
        }
    };
    let done = match (key.code, key.ctrl || key.alt) {
        (Code::Char('y'), false) => decide(rev, Decision::Accept),
        (Code::Char('n'), false) => decide(rev, Decision::Reject),
        (Code::Char('a'), false) => {
            rev.hunks
                .iter_mut()
                .filter(|h| h.decision == Decision::Undecided)
                .for_each(|h| h.decision = Decision::Accept);
            true
        }
        (Code::Char('q'), false) => true,
        (Code::Char('j') | Code::Down, false) => {
            rev.scroll += 1;
            rev.follow = false;
            false
        }
        (Code::Char('k') | Code::Up, false) => {
            rev.scroll = rev.scroll.saturating_sub(1);
            rev.follow = false;
            false
        }
        (Code::Tab, _) => {
            rev.current = rev.next_undecided().unwrap_or(rev.current);
            rev.follow = true;
            false
        }
        // Sideways, where lines aren't wrapped (`editor.soft-wrap`, `z w`): a long one read to its end
        (Code::Char('h') | Code::Left, false)
        | (Code::Char('l') | Code::Right, false)
        | (Code::Home, false)
            if !wraps =>
        {
            rev.left = match key.code {
                Code::Char('h') | Code::Left => rev.left.saturating_sub(SIDEWAYS),
                Code::Home => 0,
                _ => rev.left + SIDEWAYS,
            };
            false
        }
        _ => {
            let out = match wraps {
                true => "y takes it, n leaves it, a takes all, q ends — esc goes back, unanswered",
                false => {
                    "y takes it, n leaves it, a takes all, q ends, h/l scroll sideways — esc goes back, unanswered"
                }
            };
            ed.key_unused(key, "the review of this change", out);
            return true;
        }
    };
    if done {
        finish(ed);
    }
    true
}

/// Accepted changes as one transaction (= one undo).
fn finish(ed: &mut Editor) {
    let Some(mut rev) = ed.session.review.take() else { return };
    let on_done = rev.on_done.take();
    let before = ed.docs.iter().find(|d| d.id == rev.doc_id).map(|d| d.text.clone());
    // (the editor's keys work while it waits — #61: a change of the text since is found again, or skipped)
    if let Some(text) = &before {
        for h in rev.hunks.iter_mut().filter(|h| h.decision == Decision::Accept && !h.conflict) {
            h.conflict = !relocate(h, text);
        }
    }
    // (Claude's change — a chat's edit, its change set's file: its call's row takes the keys that answered it)
    if let Some(id) = rev.answers.as_deref() {
        let keys: Vec<_> = rev
            .hunks
            .iter()
            .filter(|h| h.old != h.new)
            .filter_map(|h| crate::stamp::Stamp::of(h.decision))
            .collect();
        crate::chat::stamp_call(ed, id, &keys);
    }
    let applied = apply_accepted(ed, &rev);
    crate::reviews::edit_ended(ed, &rev, false, applied);
    // Answered here, the keys go back to the chat it took them from — as its Esc gives them (they stayed in
    // the code: `keys → pane#29 (commands)` after an edit review's `y`)
    if rev.from_chat
        && !rev.away
        && let Some(c) = ed.session.chat.as_mut()
    {
        c.focused = true;
    }
    // A step `space u` takes back on its own
    if applied
        && rev.own_turn
        && let Some(before) = before
    {
        crate::checkpoint::single(ed, rev.doc_id, before, &rev.instruction);
    }
    if let Some(cb) = on_done {
        cb(ed, applied);
    }
}

fn apply_accepted(ed: &mut Editor, rev: &Review) -> bool {
    let accepted: Vec<&Hunk> =
        rev.hunks.iter().filter(|h| h.decision == Decision::Accept && !h.conflict).collect();
    if accepted.is_empty() {
        ed.set_status("claude: nothing applied");
        return false;
    }
    let Some(idx) = ed.docs.iter().position(|d| d.id == rev.doc_id) else { return false };
    ed.current = idx;
    let tx = Transaction::new(
        accepted.iter().map(|h| Change { from: h.from, to: h.to, insert: h.new.clone() }).collect(),
    );
    let ranges: Vec<Range> = accepted
        .iter()
        .map(|h| {
            let start = tx.map_pos(h.from, Assoc::Before);
            Range::new(start, start + h.new.len())
        })
        .collect();
    let n = ranges.len();
    ed.with_group(|cx| {
        let doc = cx.editor.doc_mut();
        doc.apply_with(&tx, Selection::new(ranges, 0));
        // (on your `y`, Claude's lines — not your edit)
        doc.own.not_yours();
    });
    ed.set_status(format!("claude: applied {n} change(s) — u to undo"));
    true
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// frictions28 T1: `kill` signals only a claude `pump` still holds unreaped — a stand-in of the test's own
    /// that no `pump` has is left running; one `pump` reads is ended, and once reaped its pid is out of the list
    /// (a kill then sends nothing: the pid may be anyone's).
    #[test]
    fn kill_reaches_only_a_child_pump_has_not_reaped() {
        let stand_in = || {
            Command::new("/bin/sh")
                .args(["-c", "read x"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        };
        let mut theirs = stand_in();
        kill(theirs.id());
        std::thread::sleep(Duration::from_millis(100));
        assert!(matches!(theirs.try_wait(), Ok(None)), "no pump has it: not signalled");
        let _ = theirs.kill();
        let _ = theirs.wait();
        let ed = crate::editor::Editor::new(crate::config::Config::default());
        let mut ours = stand_in();
        let (pid, stdin) = (ours.id(), ours.stdin.take());
        pump(ours, false, ed.events.sender(), |_| None);
        kill(pid);
        let end = std::time::Instant::now() + Duration::from_secs(5);
        while UNREAPED.lock().unwrap().contains(&pid) {
            assert!(std::time::Instant::now() < end, "killed and reaped");
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(stdin);
    }

    /// Tests never start the real claude (card model P2 caught one reading ~/.claude.json and the keychain):
    /// the default command, by name or path, is refused before anything starts.
    #[test]
    #[should_panic(expected = "a test started the real claude")]
    fn a_test_never_starts_the_real_claude() {
        // (a path that isn't there: without the guard, nothing starts and nothing panics)
        let cfg = LlmConfig { command: "/nonexistent/claude".into(), ..LlmConfig::default() };
        let _ = spawn(&cfg, &[], &std::env::temp_dir(), None, &Credential::Subscription);
    }

    /// No claude: Claude Code isn't installed — how to get it and log in, as its docs say (#163); any other
    /// failure as it is.
    #[test]
    fn a_missing_claude_says_how_to_get_it() {
        let gone = std::io::Error::from(std::io::ErrorKind::NotFound);
        for command in ["claude", "/opt/x/claude"] {
            let whole = spawn_failed(command, &gone, true);
            assert!(whole.starts_with("Claude Code isn't installed"), "{whole}");
            assert!(whole.contains("curl -fsSL https://claude.ai/install.sh | bash"), "{whole}");
            assert!(whole.contains("log in by running `claude`"), "{whole}");
            assert!(!whole.contains("os error"), "{whole}");
            let one = spawn_failed(command, &gone, false);
            assert!(!one.contains('\n') && one.contains("install.sh"), "a toast's one line: {one}");
        }
        assert_eq!(spawn_failed("my-agent", &gone, true), "my-agent isn't on PATH (llm.command)");
        assert_eq!(spawn_failed("/x/my-agent", &gone, true), "/x/my-agent isn't there (llm.command)");
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert!(spawn_failed("claude", &denied, true).starts_with("claude: "), "not a missing one");
    }

    /// Every claude tarae starts has its own updates off (`DISABLE_UPDATES` — its updater writes a folder of programs
    /// on PATH, closed to its profile) and its git's optional locks off (`.git` isn't its to write) — over what it
    /// would inherit, an environment that says otherwise included.
    #[test]
    fn every_claude_tarae_starts_has_its_updates_off() {
        let cfg = LlmConfig {
            command: "sh".into(),
            args: vec!["-c".into(), "printf '%s %s' \"$DISABLE_UPDATES\" \"$GIT_OPTIONAL_LOCKS\"".into()],
            ..Default::default()
        };
        let given = Env::from([("DISABLE_UPDATES".to_string(), "0".to_string())]);
        let sub = Credential::Subscription;
        let child = spawn(&cfg, &[], &std::env::temp_dir(), Some(&given), &sub).unwrap();
        let out = child.wait_with_output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "1 0");
    }

    /// Every claude tarae starts has Claude Code's auto memory off and no team memory stores — over what it would
    /// inherit, an environment that turns memory back on included (threat model G21).
    #[test]
    fn every_claude_tarae_starts_has_its_memory_off() {
        let script = "printf '%s %s' \"$CLAUDE_CODE_DISABLE_AUTO_MEMORY\" \"${CLAUDE_MEMORY_STORES-unset}\"";
        let cfg =
            LlmConfig { command: "sh".into(), args: vec!["-c".into(), script.into()], ..Default::default() };
        let given = Env::from([
            ("CLAUDE_CODE_DISABLE_AUTO_MEMORY".to_string(), "0".to_string()),
            ("CLAUDE_MEMORY_STORES".to_string(), "team".to_string()),
        ]);
        let sub = Credential::Subscription;
        let child = spawn(&cfg, &[], &std::env::temp_dir(), Some(&given), &sub).unwrap();
        let out = child.wait_with_output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "1 unset");
    }

    /// Every claude tarae starts is without `SSH_AUTH_SOCK` — whether it would inherit it from tarae's own
    /// environment or be given it — so it can't have your SSH agent sign with your keys (threat model G20).
    #[test]
    fn every_claude_tarae_starts_has_no_ssh_agent() {
        let cfg = LlmConfig {
            command: "sh".into(),
            args: vec!["-c".into(), "printf '%s' \"${SSH_AUTH_SOCK-unset}\"".into()],
            ..Default::default()
        };
        // (given, as `given` passes tarae's environment on; the test binary's own isn't changed — a stand-in path)
        let given = Env::from([("SSH_AUTH_SOCK".to_string(), "/tarae-test/agent.sock".to_string())]);
        let sub = Credential::Subscription;
        let child = spawn(&cfg, &[], &std::env::temp_dir(), Some(&given), &sub).unwrap();
        let out = child.wait_with_output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "unset");
    }

    /// Every claude tarae starts is its own program, `llm.command`, as every other tool runs it — no sandbox of
    /// its own wrapped around it (user decision 2026-10-17: V1d and claudeown B reversed); what keeps it is its
    /// tools going through tarae.
    #[test]
    fn every_claude_starts_as_its_own_program() {
        let dir = std::env::temp_dir();
        let cfg = LlmConfig { command: "tarae-stand-in-claude".into(), ..Default::default() };
        let cmd = command(&cfg, &[], &dir, None, &Credential::Subscription).unwrap();
        assert_eq!(cmd.get_program(), "tarae-stand-in-claude");
        let args: Vec<_> = cmd.get_args().collect();
        assert!(!args.iter().any(|a| *a == "tarae-stand-in-claude"), "nothing before it: {args:?}");
    }

    // ── Claude's environment ──

    /// The `ANTHROPIC_*` / `CLAUDE*` / `AI_AGENT` / `MCP_*` names apibill's L0 found in tarae's environment —
    /// tarae started from a shell a desktop-app Claude Code session started.
    const L0: [&str; 20] = [
        "AI_AGENT",
        "ANTHROPIC_BASE_URL",
        "CLAUDECODE",
        "CLAUDE_AGENT_SDK_VERSION",
        "CLAUDE_CODE_BRIDGE_MCP_CARRIER",
        "CLAUDE_CODE_BRIDGE_OWNER_ACCOUNT_UUID",
        "CLAUDE_CODE_BRIDGE_OWNER_ORG_UUID",
        "CLAUDE_CODE_CHILD_SESSION",
        "CLAUDE_CODE_DESKTOP_APP_VERSION",
        "CLAUDE_CODE_ENTRYPOINT",
        "CLAUDE_CODE_EXECPATH",
        "CLAUDE_CODE_HOST_SESSION_ID",
        "CLAUDE_CODE_OAUTH_SCOPES",
        "CLAUDE_CODE_SDK_HAS_HOST_AUTH_REFRESH",
        "CLAUDE_CODE_SESSION_ID",
        "CLAUDE_CODE_WORKER_EPOCH",
        "CLAUDE_EFFORT",
        "CLAUDE_PID",
        "MCP_CONNECTION_NONBLOCKING",
        "MCP_SERVER_CONNECTION_BATCH_SIZE",
    ];

    /// Inside a Claude Code session, everything of that session's goes — what it sets for its children, its
    /// host's own, an exported key — and what you may have set yourself stays: documented settings
    /// (`CLAUDE_CODE_MAX_OUTPUT_TOKENS`, `MCP_*`, `ANTHROPIC_BASE_URL`), and the rest of the environment. What stays
    /// under a Claude name is listed, so a host's next undocumented variable is seen.
    #[test]
    fn inside_claude_code_its_sessions_variables_go_and_your_settings_stay() {
        let yours = ["PATH", "CLAUDE_CODE_MAX_OUTPUT_TOKENS", "ANTHROPIC_API_KEY"];
        let env = dropped(L0.iter().chain(&yours).copied());
        assert!(env.nested);
        let kept = ["ANTHROPIC_BASE_URL", "MCP_CONNECTION_NONBLOCKING", "MCP_SERVER_CONNECTION_BATCH_SIZE"];
        let mut gone: Vec<&str> = L0.iter().copied().filter(|n| !kept.contains(n)).collect();
        gone.push("ANTHROPIC_API_KEY");
        crate::sorting::sort(&mut gone);
        assert_eq!(env.removed, gone);
        assert_eq!(env.passed, ["ANTHROPIC_BASE_URL", "CLAUDE_CODE_MAX_OUTPUT_TOKENS"]);
    }

    /// The rules as tarae has them by default (`llm.environment-key = "drop"`).
    fn dropped<'a>(names: impl IntoIterator<Item = &'a str>) -> ClaudeEnv {
        claude_env(names, false)
    }

    /// Outside Claude Code, a host's names may be your own setup (a self-hosted environment's session token, your
    /// tracing, OAuth scopes) — tarae can't tell, so they stay. Only a key, which bills in your login's place,
    /// and what Claude Code sets for its children (a stray one is always a parent's) go.
    #[test]
    fn outside_claude_code_only_keys_and_what_it_sets_for_children_go() {
        let names = [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_PROJECT_DIR",
            "CLAUDE_CODE_MESSAGING_SOCKET",
            "CLAUDE_CODE_SESSION_ACCESS_TOKEN",
            "CLAUDE_CODE_OAUTH_SCOPES",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CODE_USE_BEDROCK",
            "TRACEPARENT",
        ];
        let env = dropped(names);
        assert!(!env.nested);
        assert_eq!(
            env.removed,
            [
                "ANTHROPIC_API_KEY",
                "ANTHROPIC_AUTH_TOKEN",
                "CLAUDE_CODE_MESSAGING_SOCKET",
                "CLAUDE_PROJECT_DIR"
            ]
        );
        assert!(env.passed.is_empty(), "nothing to say outside: {:?}", env.passed);
    }

    /// The OAuth scopes are "required when `CLAUDE_CODE_OAUTH_REFRESH_TOKEN` is set": beside it they're yours, even
    /// inside Claude Code; alone there, the host's.
    #[test]
    fn the_oauth_scopes_stay_with_their_refresh_token() {
        let with = dropped([
            "CLAUDE_CODE_CHILD_SESSION",
            "CLAUDE_CODE_OAUTH_SCOPES",
            "CLAUDE_CODE_OAUTH_REFRESH_TOKEN",
        ]);
        assert_eq!(with.removed, ["CLAUDE_CODE_CHILD_SESSION"]);
        let alone = dropped(["CLAUDE_CODE_CHILD_SESSION", "CLAUDE_CODE_OAUTH_SCOPES"]);
        assert_eq!(alone.removed, ["CLAUDE_CODE_CHILD_SESSION", "CLAUDE_CODE_OAUTH_SCOPES"]);
    }

    /// An IDE's terminal sets `CLAUDECODE` too ("IDE extensions also set this in their integrated terminals"), and
    /// a shell there is no Claude Code session's: tarae started from it isn't nested — your login and your tracing
    /// stay; `CLAUDECODE` itself still goes.
    #[test]
    fn an_ides_terminal_isnt_a_claude_code_session() {
        let ide = dropped(["CLAUDECODE", "CLAUDE_CODE_OAUTH_TOKEN", "TRACEPARENT"]);
        assert!(!ide.nested);
        assert_eq!(ide.removed, ["CLAUDECODE"], "only what Claude Code sets for its children");
    }

    /// Each of a parent session's own marks alone says tarae runs inside one — a stdio MCP server's process gets
    /// no `CLAUDE_CODE_CHILD_SESSION` but the session's id, an SDK host's claude its host's session id — and the
    /// host's login goes with it.
    #[test]
    fn each_mark_of_a_parent_session_alone_makes_it_nested() {
        for mark in NESTED_BY {
            let env = dropped([mark, "CLAUDE_CODE_OAUTH_TOKEN"]);
            assert!(env.nested, "{mark}");
            assert!(env.removed.iter().any(|n| n == "CLAUDE_CODE_OAUTH_TOKEN"), "{mark}: {:?}", env.removed);
        }
    }

    /// The keys are dropped unless your config keeps them (a login by key alone, a gateway's token) — and inside a
    /// Claude Code session even then: there they're the host's. What's said names the setting where it helps.
    #[test]
    fn keys_go_to_claude_only_when_you_keep_them_outside_claude_code() {
        let keys = ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL"];
        assert!(claude_env(keys, true).removed.is_empty(), "kept: a gateway's URL and token both go on");
        assert_eq!(dropped(keys).removed, ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"]);
        let inside = claude_env(["CLAUDE_CODE_SESSION_ID", "ANTHROPIC_API_KEY"], true);
        assert_eq!(
            inside.removed,
            ["ANTHROPIC_API_KEY", "CLAUDE_CODE_SESSION_ID"],
            "the host's, kept or not"
        );
        assert!(dropped(["ANTHROPIC_API_KEY"]).said().unwrap().contains("llm.environment-key = \"keep\""));
        assert!(inside.said().unwrap().contains("the host's, whatever llm.environment-key says"));
    }

    /// `CLAUDE_CODE_OAUTH_TOKEN` is a login: outside Claude Code it's yours (`claude setup-token`) and Claude uses
    /// it; inside, it's the host session's (an SDK host passes the claude it starts its token), and Claude would
    /// answer on that host's login instead of yours.
    #[test]
    fn the_oauth_token_is_yours_outside_claude_code_and_the_hosts_inside() {
        assert!(dropped(["CLAUDE_CODE_OAUTH_TOKEN", "PATH"]).removed.is_empty(), "yours: passed");
        let inside = dropped(["CLAUDE_CODE_CHILD_SESSION", "CLAUDE_CODE_OAUTH_TOKEN"]);
        assert_eq!(inside.removed, ["CLAUDE_CODE_CHILD_SESSION", "CLAUDE_CODE_OAUTH_TOKEN"]);
    }

    /// What's left out is said by name — and, inside Claude Code, what's passed under a Claude name that no list
    /// holds (`CLAUDE_CODE_NEW_THING`), so it can be added. A key alone is said in words of its own; nothing left
    /// out says nothing.
    #[test]
    fn what_claude_doesnt_get_is_said_by_name() {
        let said = dropped([
            "CLAUDE_CODE_CHILD_SESSION",
            "CLAUDE_PID",
            "ANTHROPIC_API_KEY",
            "CLAUDE_CODE_NEW_THING",
        ])
        .said()
        .unwrap();
        assert!(said.starts_with("tarae runs inside Claude Code"), "{said}");
        assert!(said.contains(": CLAUDE_CODE_CHILD_SESSION, CLAUDE_PID."), "{said}");
        assert!(said.contains("ANTHROPIC_API_KEY isn't passed either"), "{said}");
        assert!(said.ends_with("Passed as they are: CLAUDE_CODE_NEW_THING."), "{said}");
        let key = dropped(["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "PATH"]).said().unwrap();
        assert!(
            key.starts_with("ANTHROPIC_API_KEY and ANTHROPIC_AUTH_TOKEN aren't passed to Claude:"),
            "{key}"
        );
        assert_eq!(dropped(["PATH", "CLAUDE_CODE_MAX_OUTPUT_TOKENS"]).said(), None);
    }

    /// tarae's environment as a test gives it: L0's names, an exported key and a setting of yours — each with a
    /// stand-in value (`zq7`: in nothing of tarae's own).
    pub(crate) fn nested_env() -> Env {
        let mut env: Env = L0.iter().map(|n| (n.to_string(), format!("{n}-zq7"))).collect();
        env.insert("ANTHROPIC_API_KEY".into(), "tarae-fake-key-zq7".into());
        env.insert("CLAUDE_CODE_MAX_OUTPUT_TOKENS".into(), "1234".into());
        env
    }

    /// A stand-in claude that writes its environment's names (no value) to `env.names` in the folder it's started
    /// in, then reads what it's sent.
    pub(crate) fn recording(cfg: &mut LlmConfig) {
        cfg.command = "sh".into();
        cfg.args = vec![
            "-c".into(),
            "awk 'BEGIN { for (k in ENVIRON) print k }' > env.tmp && mv env.tmp env.names; cat > /dev/null"
                .into(),
        ];
    }

    /// The names the stand-in wrote in `dir`, once it has (5 s at most), the file taken away for the next.
    fn names_in(ed: &mut Editor, dir: &Path) -> Vec<String> {
        let file = dir.join("env.names");
        let end = Instant::now() + Duration::from_secs(5);
        while !file.exists() {
            assert!(Instant::now() < end, "the stand-in claude wrote no env.names");
            if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
                ed.handle_event(ev);
            }
        }
        let names = std::fs::read_to_string(&file).unwrap().lines().map(String::from).collect();
        std::fs::remove_file(&file).unwrap();
        names
    }

    /// What claude would inherit from the process itself is left out too, not only what a test gives: tarae's
    /// own checks run from inside a Claude Code session, its variables in the test binary's environment, and a
    /// stand-in claude got them all while every test passed. The variables are set only in a process of its own
    /// (`process_env_in_its_own_process`) — this one's is never changed.
    /// The same from a process outside any Claude Code session (its marks taken out of the child's environment,
    /// wherever the tests run) with `llm.environment-key = "keep"`: the stand-in's key goes on — `spawn` reads the
    /// setting — and `CLAUDECODE` (an IDE terminal's, say) still doesn't. A stand-in is the whole of what's
    /// judged (`ClaudeEnv::of`): the process's own key never reaches its claude.
    #[test]
    fn what_the_process_itself_holds_never_reaches_claude() {
        let run = |how: &str, env: &[(&str, &str)]| {
            let mut child = Command::new(std::env::current_exe().unwrap());
            child
                .args(["--exact", "llm::tests::process_env_in_its_own_process", "--ignored", "--nocapture"])
                .env("TARAE_CLAUDE_ENV_CHILD", how)
                .envs(env.iter().copied());
            if how == "keep" {
                for mark in NESTED_BY {
                    child.env_remove(mark);
                }
            }
            let out = child.output().unwrap();
            let said =
                String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
            assert!(out.status.success(), "{how}: {said}");
            assert!(said.contains("1 passed"), "{how}: the child ran it: {said}");
        };
        run("drop", &PROCESS_ENV);
        run("keep", &[("CLAUDECODE", "1"), ("ANTHROPIC_API_KEY", "tarae-fake-key-1")]);
    }

    /// A Claude Code session's variables, a key and a setting of yours — the process's own in the child.
    const PROCESS_ENV: [(&str, &str); 5] = [
        ("CLAUDECODE", "1"),
        ("CLAUDE_CODE_SESSION_ID", "x"),
        ("CLAUDE_CODE_HOST_SESSION_ID", "host-zq7"),
        ("ANTHROPIC_API_KEY", "tarae-fake-key-1"),
        ("CLAUDE_CODE_MAX_OUTPUT_TOKENS", "1234"),
    ];

    /// `what_the_process_itself_holds_never_reaches_claude`'s child: a stand-in claude started as every test
    /// starts one (`given` empty), from a process holding `PROCESS_ENV` (`drop`) — or, with the key kept, the
    /// stand-in holding one (`keep`).
    #[test]
    #[ignore]
    fn process_env_in_its_own_process() {
        let Some(how) = std::env::var_os("TARAE_CLAUDE_ENV_CHILD") else { return };
        let scratch = crate::scratch::Scratch::new("llm-process-env");
        let dir = scratch.to_path_buf();
        let mut cfg = LlmConfig { keep_env_key: how == "keep", ..LlmConfig::default() };
        recording(&mut cfg);
        let given = match how == "keep" {
            true => Env::from([("ANTHROPIC_API_KEY".into(), "tarae-fake-key-2".into())]),
            false => Env::new(),
        };
        let mut child = spawn(&cfg, &[], &dir, Some(&given), &Credential::Subscription).unwrap();
        drop(child.stdin.take());
        child.wait().unwrap();
        let names = std::fs::read_to_string(dir.join("env.names")).unwrap();
        let names: Vec<&str> = names.lines().collect();
        if how == "keep" {
            assert!(names.contains(&"ANTHROPIC_API_KEY"), "kept: {names:?}");
            assert!(!names.contains(&"CLAUDECODE"), "CLAUDECODE, the process's, reached claude");
        } else {
            for (gone, _) in &PROCESS_ENV[..4] {
                assert!(!names.contains(gone), "{gone}, the process's, reached claude");
            }
            assert!(names.contains(&"CLAUDE_CODE_MAX_OUTPUT_TOKENS"), "yours stays: {names:?}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// claude failing for want of a login, with a key tarae left out and no key card for it (here: answered `n`),
    /// is told why and what passes it — in the chat, after claude's own words — and nothing of the key's value. A
    /// key you keep, or another failure, adds nothing.
    #[test]
    fn a_login_failure_with_a_key_left_out_names_the_setting() {
        let scratch = crate::scratch::Scratch::new("llm-login");
        let root = scratch.to_path_buf();
        let mut ed = Editor::with_root(crate::config::Config::default(), root.clone());
        ed.claude_env = Some(Env::from([("ANTHROPIC_API_KEY".into(), "tarae-fake-key-zq7".into())]));
        ed.session.env_key = Some(false);
        // (a stand-in claude with no login: each message answered as Claude Code does under `-p`)
        let failed = r#"{"type":"result","subtype":"success","is_error":true,"result":"Not logged in · Please run /login"}"#;
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec![
            "-c".into(),
            format!(
                "while read -r line; do case \"$line\" in *'\"type\":\"user\"'*) printf '%s\\n' '{failed}';; esac; done"
            ),
        ];
        crate::chat::open(&mut ed);
        ed.session.chat.as_mut().unwrap().input = "hello".into();
        crate::chat::send(&mut ed);
        let hint = |ed: &Editor| {
            ed.session
                .chat
                .as_ref()
                .unwrap()
                .msgs
                .iter()
                .find(|m| m.text.contains("no login"))
                .map(|m| m.text.clone())
        };
        let end = Instant::now() + Duration::from_secs(5);
        while hint(&ed).is_none() {
            assert!(
                Instant::now() < end,
                "no hint: {:?}",
                ed.session.chat.as_ref().unwrap().msgs.iter().map(|m| &m.text).collect::<Vec<_>>()
            );
            if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
                ed.handle_event(ev);
            }
        }
        let hint = hint(&ed).unwrap();
        assert!(
            hint.contains("ANTHROPIC_API_KEY") && hint.contains("llm.environment-key = \"keep\""),
            "{hint}"
        );
        assert!(!hint.contains("zq7"), "names only: {hint}");
        assert!(hint.ends_with("Or log in: run `claude` in a terminal, then /login."), "yours: {hint}");
        assert_eq!(login_hint(&mut ed, "Overloaded"), None, "not a login failure");
        ed.config.llm.keep_env_key = true;
        assert_eq!(
            login_hint(&mut ed, "Not logged in · Please run /login"),
            None,
            "the key went on: nothing to add"
        );
        drop(ed);
        std::fs::remove_dir_all(&root).ok();
    }

    /// The chat's claude and the ask's spare (which the ask takes, or starts the same way) get tarae's environment
    /// less what `claude_env` says — the same names left out, the rest as it was (the reviewer's: its own test).
    /// The chat says so once since tarae started, by name: a new conversation's claude doesn't say it again.
    #[test]
    fn the_chats_and_the_asks_claude_leave_out_a_parent_sessions_variables() {
        let scratch = crate::scratch::Scratch::new("llm-claude-env");
        let root = scratch.to_path_buf();
        let mut ed = Editor::with_root(crate::config::Config::default(), root.clone());
        recording(&mut ed.config.llm);
        ed.claude_env = Some(nested_env());
        let log = crate::log::capture();
        let gone = ClaudeEnv::of(ed.claude_env.as_ref(), false).removed;
        let check = |names: Vec<String>, who: &str| {
            for name in &gone {
                assert!(!names.contains(name), "{who}: {name} reached claude");
            }
            for kept in ["CLAUDE_CODE_MAX_OUTPUT_TOKENS", "ANTHROPIC_BASE_URL", "MCP_CONNECTION_NONBLOCKING"]
            {
                assert!(names.iter().any(|n| n == kept), "{who}: {kept} left out — {names:?}");
            }
        };
        assert!(gone.iter().any(|n| n == "ANTHROPIC_API_KEY") && gone.iter().any(|n| n == "CLAUDECODE"));
        crate::chat::open(&mut ed);
        check(names_in(&mut ed, &root), "the chat");
        let notes = |ed: &Editor| -> Vec<String> {
            let c = ed.session.chat.as_ref().unwrap();
            c.msgs
                .iter()
                .filter(|m| m.text.contains("CLAUDE_CODE_CHILD_SESSION"))
                .map(|m| m.text.clone())
                .collect()
        };
        assert_eq!(notes(&ed).len(), 1, "said in the chat");
        assert!(!notes(&ed)[0].contains("zq7"), "names only: {}", notes(&ed)[0]);
        let logged =
            log.lines().into_iter().filter(|l| l.starts_with("claude  info  tarae runs inside")).count();
        assert_eq!(logged, 1, "one log line");
        crate::chat::reset(&mut ed);
        names_in(&mut ed, &root);
        assert!(notes(&ed).is_empty(), "a new conversation: not again");
        prewarm(&mut ed);
        check(names_in(&mut ed, &root), "the ask's spare");
        drop(ed);
        std::fs::remove_dir_all(&root).ok();
    }

    /// A commit message's body wrapped at 72: the subject and short lines as they are, a long line between
    /// words, a list item's rest under its text; code, trailers and an over-long word whole.
    #[test]
    fn commit_messages_wrap_at_72() {
        let long = "word ".repeat(20).trim_end().to_string(); // 99 columns
        let msg = format!(
            "{long}\n\nshort line\n{long}\n- {long}\n    {long}\nCo-Authored-By: {long}\n{}",
            "x".repeat(80)
        );
        let out = wrap_message(&msg, 72);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], long, "the subject stays");
        assert_eq!(lines[2], "short line");
        let row = "word ".repeat(14).trim_end().to_string(); // 69 columns — one more word would be 74
        assert_eq!((lines[3], lines[4]), (row.as_str(), "word word word word word word"));
        assert_eq!(lines[5], format!("- {}", "word ".repeat(14).trim_end()), "a list item (71 columns)");
        assert!(lines[6].starts_with("  word"), "its rest under its text: {:?}", lines[6]);
        assert!(lines.contains(&format!("    {long}").as_str()), "code stays");
        assert!(lines.contains(&format!("Co-Authored-By: {long}").as_str()), "a trailer stays");
        assert_eq!(*lines.last().unwrap(), "x".repeat(80), "a word longer than the width stays whole");
        assert!(
            lines[2..]
                .iter()
                .all(|l| l.len() <= 72 || l.starts_with(' ') || !l.contains(' ') || l.contains(": "))
        );
    }

    #[test]
    fn stream_lines_and_preview() {
        let delta = serde_json::json!({"type": "stream_event", "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": "ab"}}});
        assert!(matches!(parse_line(&delta), Some(StreamEv::Text(t)) if t == "ab"));
        let think =
            serde_json::json!({"type": "system", "subtype": "thinking_tokens", "estimated_tokens": 5});
        assert!(matches!(parse_line(&think), Some(StreamEv::Thinking)));
        let stopped =
            serde_json::json!({"type": "result", "subtype": "error_during_execution", "result": ""});
        assert!(matches!(parse_line(&stopped), Some(StreamEv::Done(Err(e))) if e == "stopped"));
        // Preview: the last lines inside the tag being written
        assert_eq!(preview("<r i=\"1\">a</r>\n<r i=\"2\">\nx\ny\nz", 2), vec!["y", "z"]);
        assert_eq!(preview("plain\ntext", 5), vec!["plain", "text"]);
    }

    #[test]
    fn parses_tagged_replies() {
        let out = "<r i=\"1\">\nfoo\n</r>\n<r i=\"2\">bar</r>";
        assert_eq!(parse_replies(out, 2).unwrap(), vec!["foo", "bar"]);
        assert!(parse_replies("<r i=\"1\">x</r>", 2).is_err());
        // With one selection, also an answer without tags·fences
        assert_eq!(parse_replies("```rust\nlet x = 1;\n```", 1).unwrap(), vec!["let x = 1;"]);
    }

    #[test]
    fn review_rows_expand_changes_in_place() {
        let text = Rope::from_str("a\nb\nc\n");
        let hunk = |decision| Hunk {
            from: 2,
            to: 4,
            old: "b\n".into(),
            new: "B\nB2\n".into(),
            decision,
            conflict: false,
        };
        let rev = |h| Review::new(0, 0, "", vec![h]);
        let rows = |h, text: &Rope| rev(h).rows(text).iter().map(shape).collect::<Vec<_>>();
        // (a line the change takes out or keeps is the document's own: its number)
        assert_eq!(
            rows(hunk(Decision::Undecided), &text),
            ["doc 1", "head 0", "- 2", "+ B (washed)", "+ B2 (washed)", "doc 3"]
        );
        // Accept = new text only, reject = original only
        assert_eq!(rows(hunk(Decision::Accept), &text), ["doc 1", "head 0", "+ B", "+ B2", "doc 3"]);
        assert_eq!(rows(hunk(Decision::Reject), &text), ["doc 1", "head 0", "  2", "doc 3"]);
        // Edited while the review waits: what it takes out comes from the text it was asked on
        let moved = Rope::from_str("a\nx\nc\n");
        assert_eq!(
            rows(hunk(Decision::Undecided), &moved),
            ["doc 1", "head 0", "- b (asked)", "+ B (washed)", "+ B2 (washed)", "doc 3"]
        );
    }

    /// A review row in words: `doc N` a line as is, `head K` change K's head, `- N`/`  N` a line it takes out or
    /// keeps, `+ text` one it proposes; `(asked)` from the text it was asked on.
    fn shape(r: &crate::codeview::Row) -> String {
        use crate::codeview::{Edit, Mark, OtherKind, Row};
        let sign = |m: Mark| match m {
            Mark::Added { .. } => "+",
            Mark::Taken => "-",
            Mark::Kept => " ",
        };
        match r {
            Row::Text { line, edit: None, .. } => format!("doc {}", line + 1),
            Row::Text { line, edit: Some(e), .. } => format!("{} {}", sign(e.mark), line + 1),
            Row::Header { hunk, .. } => format!("head {hunk}"),
            Row::Other {
                text, kind: OtherKind::Edit(Edit { mark: m @ Mark::Added { washed }, .. }), ..
            } => {
                format!("{} {text}{}", sign(*m), if *washed { " (washed)" } else { "" })
            }
            Row::Other { text, kind: OtherKind::Edit(e), .. } => format!("{} {text} (asked)", sign(e.mark)),
            Row::Other { text, .. } => format!("gone {text}"),
            Row::Doc { start, index, .. } => format!("folded {} {index}", start + 1),
        }
    }

    #[test]
    fn trailing_newline_follows_original() {
        assert_eq!(match_trailing_newline("a\n", "b".into()), "b\n");
        assert_eq!(match_trailing_newline("a", "b\n\n".into()), "b");
    }

    #[test]
    fn line_diff_marks_changes() {
        let d = line_diff("a\nb\nc", "a\nB\nc");
        assert_eq!(d, vec![(' ', "a"), ('-', "b"), ('+', "B"), (' ', "c")]);
    }

    #[test]
    fn prompt_carries_selection_and_context() {
        let text = Rope::from_str("one\ntwo\nthree\n");
        let p = build_prompt("upper", "x.txt", &text, &[(4, 7)], 1);
        assert!(p.contains("Instruction: upper"));
        assert!(p.contains("<text>two</text>"));
        assert!(p.contains("<before>one\n</before>"));
        assert!(p.contains("lines=\"2-2\""));
        // A line-wise selection (up to the next line's start) is still that one line
        let p = build_prompt("upper", "x.txt", &text, &[(4, 8)], 1);
        assert!(p.contains("lines=\"2-2\""), "{p}");
        assert!(p.contains("<after>three\n</after>"), "{p}");
    }

    #[test]
    fn tool_calls_come_from_assistant_messages() {
        let ev = serde_json::json!({"type": "assistant", "message": {"content": [
            {"type": "thinking", "thinking": ""},
            {"type": "tool_use", "name": "Read", "input": {"file_path": "/p/src/lib.rs", "offset": 10}},
            {"type": "tool_use", "name": "Grep", "input": {"pattern": "fn add"}},
        ]}});
        let got: Vec<(String, serde_json::Value)> = tool_events(&ev)
            .into_iter()
            .map(|e| match e {
                StreamEv::Tool { name, input, .. } => (name, input),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, "Read");
        assert_eq!(got[0].1["offset"], 10);
        assert_eq!(got[1].1["pattern"], "fn add");
        let text = serde_json::json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "hi"}]}});
        assert!(tool_events(&text).is_empty());
        let result = serde_json::json!({"type": "user", "message": {"content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": [{"type": "text", "text": "a.rs:3:fn a"}]}]}});
        assert!(matches!(&tool_events(&result)[..],
            [StreamEv::ToolResult { id, text, error: false }] if id == "t1" && text == "a.rs:3:fn a"));
        // Summarized thinking carries text; the omitted kind is just a signal
        let delta = |t: &str| {
            serde_json::json!({"type": "stream_event", "event": {"type": "content_block_delta",
            "delta": {"type": "thinking_delta", "thinking": t}}})
        };
        assert!(matches!(parse_line(&delta("hmm")), Some(StreamEv::Thought(t)) if t == "hmm"));
        assert!(matches!(parse_line(&delta("")), Some(StreamEv::Thinking)));
    }

    #[test]
    fn flags_are_replaced_not_repeated() {
        let args = with_flag(&LlmConfig::default().args, "--setting-sources", "user,project,local");
        let at = args.iter().position(|a| a == "--setting-sources").unwrap();
        assert_eq!(args[at + 1], "user,project,local");
        assert_eq!(args.iter().filter(|a| *a == "--setting-sources").count(), 1);
        assert_eq!(with_flag(&[], "--x", "1"), ["--x", "1"]);
    }

    #[test]
    fn chat_tools_replace_the_no_tools_default() {
        let args =
            with_tools(&LlmConfig::default().args, "Read,Grep,Glob", "Read,Grep,Glob,mcp__tarae__note");
        let at = |f: &str| args.iter().position(|a| a == f).unwrap();
        assert_eq!(args.iter().filter(|a| *a == "--tools").count(), 1);
        assert_eq!(args[at("--tools") + 1], "Read,Grep,Glob");
        assert_eq!(args[at("--allowedTools") + 1], "Read,Grep,Glob,mcp__tarae__note");
        assert!(args.windows(2).all(|w| w[0] != "--setting-sources" || w[1].is_empty()), "rest kept");
    }
}
