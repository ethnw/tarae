//! Who pays for a session's Claude (apibill step 5, `target/sb/apibill-design.md` D1/D3): the plan you're
//! logged in to (`Subscription`), or an Anthropic API key (`Api`). Per session (`Session.billing`, kept with it —
//! `worktree::Kept` — so a restart keeps it); a new session takes `llm.billing`; the reviewer follows its session
//! unless `llm.reviewer-billing` says otherwise. Only your own config sets either (`trust::YOURS_ONLY`): a project
//! never decides to spend your credits. `:session-billing api|subscription` switches the session in front — a
//! claude running stops and resumes the same conversation (`--resume`) on the new one.
//!
//! - **The key never passes through tarae.** An API session's claude gets an `apiKeyHelper` in its `--settings`:
//!   `/usr/bin/security find-generic-password -a <you> -s tarae.anthropic-api-key -w`, run by claude itself (its
//!   output is the key — Claude Code's settings reference, `apiKeyHelper`). tarae reads no key, holds none, so none
//!   can reach its state, log or cards. `--settings` outranks the user's and the project's settings files
//!   (Claude Code's settings docs), so this helper is the one that runs — over `claudetrust`'s held-back `""`, and
//!   over a helper of yours. Merged into the one `--settings` object the chat already passes, never beside it:
//!   `llm::with_flag` replaces a flag, so a second would drop the first (`chat::CHAT_SETTINGS`' `ask` rules).
//! - **The chat's cache lives an hour** (`promptCacheTtl: "1h"`, your decision 2026-10-10): an API key's default is
//!   five minutes (Claude Code's env reference, `ENABLE_PROMPT_CACHING_1H`), and a chat's turns are often further
//!   apart — each such turn would write the whole context again. The settings reference, `promptCacheTtl`: "Choose
//!   how long the prompt cache holds the main conversation … `"1h"`: the cache holds for an hour" (Claude Code
//!   v2.1.242+; `FORCE_PROMPT_CACHING_5M` and `CLAUDE_CODE_PROMPT_CACHE_TTL` you export still win). Not for the ask
//!   or the reviewer: one turn each.
//! - **An API session never takes a key from the environment** (`llm::spawn`, whatever `llm.environment-key`
//!   says): `ANTHROPIC_API_KEY`/`ANTHROPIC_AUTH_TOKEN` outrank a helper (Claude Code's authentication docs, the
//!   credential order).
//! - **Checked, not believed** (`checked`): claude says which credential it runs on — `apiKeySource` in its answer
//!   to `initialize` and in each turn's `init` (the SDK's `SDKSystemMessage`). An API session's must be
//!   `apiKeyHelper`, a subscription's `none`; anything else stops it (the chat: the turn interrupted; the reviewer
//!   and the ask: their claude stopped) and says what claude reported. Detection, not prevention: `init` comes
//!   with the turn, so one request may have gone out already — keeping a project's credential out is
//!   `claudetrust`'s.
//! - **macOS only for now**: the key is in the login Keychain. Elsewhere an API session is refused, said why (the
//!   Secret Service's `secret-tool` helper is a later change).

use serde_json::{Map, Value as Json};

use crate::editor::Editor;

/// Who pays for a session's Claude.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Billing {
    /// The plan you're logged in to in Claude Code.
    #[default]
    Subscription,
    /// Your Anthropic API key, from the Keychain (`SERVICE`).
    Api,
}

impl Billing {
    pub fn name(self) -> &'static str {
        match self {
            Billing::Subscription => "subscription",
            Billing::Api => "api",
        }
    }

    pub fn parse(s: &str) -> Option<Billing> {
        match s.trim() {
            "subscription" => Some(Billing::Subscription),
            "api" => Some(Billing::Api),
            _ => None,
        }
    }

    /// The `apiKeySource` claude must report on it. A subscription's is `none` — or, with `llm.environment-key =
    /// "keep"` (`keep_env_key`), the key you pass on from tarae's environment on purpose.
    fn expected(self, keep_env_key: bool) -> &'static [&'static str] {
        match (self, keep_env_key) {
            (Billing::Api, _) => &["apiKeyHelper"],
            (Billing::Subscription, false) => &["none"],
            (Billing::Subscription, true) => &["none", "ANTHROPIC_API_KEY"],
        }
    }
}

/// The Keychain item's service name (D3: `security add-generic-password -a "$USER" -s tarae.anthropic-api-key
/// -w` adds it).
pub const SERVICE: &str = "tarae.anthropic-api-key";

/// Which claude is started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Chat,
    Reviewer,
    Ask,
}

impl Role {
    fn whose(self) -> &'static str {
        match self {
            Role::Chat => "the chat's claude",
            Role::Reviewer => "the reviewer's claude",
            Role::Ask => "the ask's claude",
        }
    }
}

/// What a claude is started with for its billing (`llm::spawn`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Credential {
    /// Your Claude Code login: nothing added.
    Subscription,
    /// The key through `helper` (a command line claude runs; its output is the key), and the hour-long cache for
    /// the chat.
    Api { helper: String, hour_cache: bool },
}

impl Credential {
    pub fn billing(&self) -> Billing {
        match self {
            Credential::Subscription => Billing::Subscription,
            Credential::Api { .. } => Billing::Api,
        }
    }
}

/// A login name the helper's command line carries as it is — no quoting needed (`id -un`'s, as the live checks'
/// scripts took it).
fn plain_account(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// The helper for `account` on this system — macOS: `security` reading the Keychain item; elsewhere refused.
pub fn helper_for(macos: bool, account: &str) -> Result<String, String> {
    if !macos {
        return Err("API billing reads your key from the macOS Keychain — not on this system yet".into());
    }
    if !plain_account(account) {
        return Err(format!("your login name {account:?} isn't one tarae puts on a command line as it is"));
    }
    Ok(format!("/usr/bin/security find-generic-password -a {account} -s {SERVICE} -w"))
}

/// How `account`'s key is looked for in the Keychain: `security` without `-w` — the item's attributes, not its
/// secret, and `key_found` reads only the exit status.
fn key_query(account: &str) -> Vec<String> {
    ["/usr/bin/security", "find-generic-password", "-a", account, "-s", SERVICE].map(String::from).to_vec()
}

/// Runs `query` (`key_query`, or a test's stand-in) on this thread, everything it says thrown away — Ok: the
/// key's item is there; Err: why not, and how to add it.
fn key_found(query: &[String]) -> Result<(), String> {
    use std::process::Stdio;
    let (program, args) = query.split_first().ok_or("no Keychain query")?;
    let status = std::process::Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(_) => Err(format!(
            "your API key isn't in the Keychain — add it in a terminal: security add-generic-password -a \
             \"$USER\" -s {SERVICE} -w"
        )),
        Err(e) => Err(format!("couldn't look for your API key in the Keychain: {e}")),
    }
}

/// Your login name (`id -un`: the effective user's record) — read once.
fn account() -> Result<String, String> {
    static ACCOUNT: std::sync::OnceLock<Result<String, String>> = std::sync::OnceLock::new();
    ACCOUNT
        .get_or_init(|| {
            let mut buf = vec![0 as libc::c_char; 4096];
            // SAFETY: a zeroed passwd is a valid out-parameter; getpwuid_r fills it with pointers into `buf`,
            // which outlives every read of them below.
            let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
            let mut found: *mut libc::passwd = std::ptr::null_mut();
            // SAFETY: every pointer is valid for the lengths given; `found` is set to `&mut pwd` or null.
            let r = unsafe {
                libc::getpwuid_r(libc::geteuid(), &mut pwd, buf.as_mut_ptr(), buf.len(), &mut found)
            };
            if r != 0 || found.is_null() || pwd.pw_name.is_null() {
                return Err("couldn't read your login name".to_string());
            }
            // SAFETY: `pw_name` points to a NUL-terminated string in `buf`.
            let name = unsafe { std::ffi::CStr::from_ptr(pwd.pw_name) };
            name.to_str().map(String::from).map_err(|_| "your login name isn't UTF-8".to_string())
        })
        .clone()
}

/// `args` with the credential laid into their `--settings` object (one made if there's none) — merged, never
/// replacing what's there. Err: a `--settings` that names a file (tarae can't add to it unread).
pub fn args(args: &[String], cred: &Credential) -> Result<Vec<String>, String> {
    let Credential::Api { helper, hour_cache } = cred else { return Ok(args.to_vec()) };
    let given = args.iter().rposition(|a| a == "--settings").and_then(|i| args.get(i + 1));
    let mut json = match given.map(|v| serde_json::from_str::<Json>(v)) {
        None => Map::new(),
        Some(Ok(Json::Object(m))) => m,
        Some(_) => {
            return Err(
                "llm.args passes --settings as a file — tarae can't add the API key's helper to it".into()
            );
        }
    };
    json.insert("apiKeyHelper".into(), Json::String(helper.clone()));
    if *hour_cache {
        json.insert("promptCacheTtl".into(), Json::String("1h".into()));
    }
    Ok(crate::llm::with_flag(args, "--settings", &Json::Object(json).to_string()))
}

/// What claude reported (`apiKeySource`) against what `billing` needs — None: as it should be (or not said);
/// else what the chat says, naming where it likely came from.
pub fn checked(billing: Billing, keep_env_key: bool, role: Role, reported: Option<&str>) -> Option<String> {
    let reported = reported?;
    if billing.expected(keep_env_key).contains(&reported) {
        return None;
    }
    let from = match (billing, reported) {
        (_, "ANTHROPIC_API_KEY") => {
            "a key in claude's environment — tarae passes none, so a project's Claude settings' env block \
             (:claude-settings)"
        }
        (Billing::Subscription, "apiKeyHelper") => {
            "a key helper in your own Claude settings, or one of the project's you trusted (:claude-settings)"
        }
        (_, "/login managed key") => "a key Claude Code made when you logged in with a Console account",
        (Billing::Api, "none") => {
            "the login won over tarae's helper — a managed setting, a token in claude's environment or a cloud \
             provider"
        }
        _ => "a credential tarae didn't give it",
    };
    let wanted = billing.expected(keep_env_key).join(" or ");
    Some(format!(
        "Stopped: {} reported its credential as {reported}, but it bills {} here (it should be {wanted}) — likely \
         {from}. Nothing more goes out on it.",
        role.whose(),
        billing.name()
    ))
}

/// The plan's weekly window at this percent or more: a new session's card asks who pays (D2) · from this, your
/// key comes first (still a choice).
pub const LIMIT_ASK: u32 = 80;
pub const LIMIT_SUGGEST: u32 = 95;

/// Said once, as API billing first goes on — on the card that turns it on, else as a warning (D3).
pub const WEAKNESS: &str = "Your key is in the Keychain. Sandboxed commands can't reach it. A command you allow \
                            outside the sandbox can — tarae warns when one asks for it, but can't see every way.";

/// What tarae keeps of billing across starts (`$state/billing.json`): new sessions and the reviewer on the key
/// until the plan's weekly window resets (`a` on the limit card), and whether `WEAKNESS` was said. No key, no
/// account — a time and a flag.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Kept {
    /// Where it's kept — None in tests (nothing read or written unless a test sets it).
    pub path: Option<std::path::PathBuf>,
    /// Until when (unix ms) new sessions and the reviewer bill the key.
    pub api_until_ms: Option<u64>,
    pub weakness_told: bool,
}

impl Kept {
    pub fn path() -> Option<std::path::PathBuf> {
        crate::config::state_dir().map(|d| d.join("billing.json"))
    }

    /// From `path` (missing or unreadable: nothing kept).
    pub fn read(path: Option<std::path::PathBuf>) -> Kept {
        let v = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str::<Json>(&t).ok())
            .unwrap_or(Json::Null);
        Kept { api_until_ms: v["api_until"].as_u64(), weakness_told: v["weakness_told"] == true, path }
    }

    /// Written on a worker — the last one asked for wins (one finishing late is skipped).
    fn save(&self, jobs: &crate::event::Jobs) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static ASKED: AtomicU64 = AtomicU64::new(0);
        static WRITTEN: std::sync::Mutex<u64> = std::sync::Mutex::new(0);
        let Some(path) = self.path.clone() else { return };
        let seq = ASKED.fetch_add(1, Ordering::Relaxed) + 1;
        let text = serde_json::json!({ "api_until": self.api_until_ms, "weakness_told": self.weakness_told })
            .to_string();
        jobs.background(move || {
            let Ok(mut last) = WRITTEN.lock() else { return };
            if seq < *last {
                return;
            }
            *last = seq;
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = crate::disk::write_atomic(&path, |w| std::io::Write::write_all(w, text.as_bytes()));
        });
    }
}

/// The limit card's question (D2): session `session`'s claude was refused by the plan's weekly window `window`,
/// which resets at `resets_at_ms` (None: not said).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LimitAsk {
    pub session: crate::event::SessionId,
    pub window: String,
    pub resets_at_ms: Option<u64>,
}

impl Editor {
    /// Who pays for a new session's Claude: your key while `a` on the limit card holds (until its window
    /// resets), else `llm.billing`.
    pub fn new_session_billing(&self) -> Billing {
        match self.api_for_new() {
            true => Billing::Api,
            false => self.config.llm.billing,
        }
    }

    /// `a` on the limit card holds: new sessions and the reviewer bill the key until the window's reset — while
    /// the key can be used (`api_usable`).
    fn api_for_new(&self) -> bool {
        self.billing_kept.api_until_ms.is_some_and(|t| t > crate::usage::now_ms()) && self.api_usable()
    }

    /// API billing can start here — looked for on a worker of `jobs`, then `then` with the answer: macOS and a
    /// plain account (`credential`), and your key's Keychain item there (`key_found` — never its value). Kept in
    /// `api_ready` for what can't wait (`api_for_new`, a new session's ask). Every place that offers or sets the
    /// key goes through it.
    pub fn api_check(
        &mut self,
        jobs: crate::event::Jobs,
        then: impl FnOnce(&mut Editor, Result<(), String>) + Send + 'static,
    ) {
        let query = self.credential(Billing::Api, Role::Chat).and_then(|_| match &self.api_key_query {
            Some(q) => Ok(q.clone()),
            None => account().map(|a| key_query(&a)),
        });
        let query = match query {
            Ok(q) => q,
            Err(e) => {
                self.api_ready = Some(Err(e.clone()));
                return then(self, Err(e));
            }
        };
        jobs.spawn(move || {
            let found = key_found(&query);
            move |ed: &mut Editor| {
                ed.api_ready = Some(found.clone());
                then(ed, found)
            }
        });
    }

    /// The last `api_check` found the key usable.
    fn api_usable(&self) -> bool {
        self.api_ready == Some(Ok(()))
    }

    /// At start, with `a` on the limit card kept and holding: whether the key can be used is looked for — new
    /// sessions and the reviewer bill it once it's found.
    pub(crate) fn api_check_kept(&mut self) {
        if self.billing_kept.api_until_ms.is_some_and(|t| t > crate::usage::now_ms()) {
            self.api_check(self.events.jobs(), |_, _| {});
        }
    }

    /// API billing went on in the session in front: `WEAKNESS` said once ever — `shown`: a card said it already,
    /// else a warning does, and its chat (if open) keeps it whole (a toast cuts it at three rows).
    fn weakness_said(&mut self, shown: bool) {
        if self.billing_kept.weakness_told {
            return;
        }
        if !shown {
            self.set_warning(WEAKNESS);
            if let Some(c) = self.session.chat.as_mut() {
                c.note(WEAKNESS);
            }
        }
        self.billing_kept.weakness_told = true;
        self.billing_kept.save(&self.events.jobs());
    }

    /// Whether a card turning API billing on says `WEAKNESS` (not said yet).
    pub fn weakness_due(&self) -> bool {
        !self.billing_kept.weakness_told
    }

    /// API billing went on from a card that said `WEAKNESS` (a new session's).
    pub(crate) fn weakness_shown(&mut self) {
        self.weakness_said(true);
    }

    /// Who pays for a session made now, and what its card says of it (D2): the plan's weekly window — the
    /// account's, or the chosen model's family's (`seven_day_opus` for an Opus session), the higher — at
    /// `LIMIT_ASK` or more (or refused): asked, your key first from `LIMIT_SUGGEST`; else as `new_session_billing`
    /// says, with a line when that's your key until a reset, or when the 5-hour window presses (wait — the 5-hour
    /// window never offers the key). Unknown windows ask nothing; neither does `llm.billing = "api"` (the key from
    /// the start) nor a place where the key can't be used (`api_check` before — the line says why when `a` holds).
    pub fn new_session_ask(&self) -> (Billing, Option<String>, Option<String>) {
        use crate::limits::{Status, said};
        let now = crate::usage::now_ms();
        let kept = self.billing_kept.api_until_ms.filter(|&t| t > now);
        if let Some(until) = kept
            && self.api_usable()
        {
            let note = format!(
                "Its Claude runs on your API key until {} (a on the limit card).",
                crate::limits::resets(until)
            );
            return (Billing::Api, None, Some(note));
        }
        let model = self.config.llm.model.clone().unwrap_or_default().to_lowercase();
        let family =
            ["opus", "sonnet"].into_iter().find(|f| model.contains(f)).map(|f| format!("seven_day_{f}"));
        let weekly = std::iter::once(crate::limits::WEEKLY.to_string())
            .chain(family)
            .filter_map(|w| self.limits.get(&w, now))
            .max_by_key(|r| (r.status == Some(Status::Rejected), r.percent));
        let asks = self.config.llm.billing == Billing::Subscription && self.api_usable();
        match weekly {
            Some(r)
                if asks
                    && (r.status == Some(Status::Rejected) || r.percent.is_some_and(|p| p >= LIMIT_ASK)) =>
            {
                let first =
                    r.status == Some(Status::Rejected) || r.percent.is_some_and(|p| p >= LIMIT_SUGGEST);
                (if first { Billing::Api } else { Billing::Subscription }, Some(said(r)), None)
            }
            _ => {
                let five = self.limits.get("five_hour", now).filter(|r| crate::limits::pressing(r));
                let note = five.map(|r| format!("The plan's {} — wait for it to pass.", said(r)));
                let unusable = match &self.api_ready {
                    Some(Err(e)) => Some(e),
                    _ => None,
                };
                let note = note.or(kept
                    .and(unusable)
                    .map(|e| format!("Not on your API key, though a on the limit card said so: {e}.")));
                (self.config.llm.billing, None, note)
            }
        }
    }

    /// What a claude of `role` is started with on `billing`: the key's helper for `Api` (a test's stand-in,
    /// `api_key_helper`, if it gives one) — Err: why an API session can't start here.
    pub fn credential(&self, billing: Billing, role: Role) -> Result<Credential, String> {
        match billing {
            Billing::Subscription => Ok(Credential::Subscription),
            Billing::Api => {
                let helper = match &self.api_key_helper {
                    Some(h) => h.clone(),
                    None => helper_for(cfg!(target_os = "macos"), &account()?)?,
                };
                Ok(Credential::Api { helper, hour_cache: role == Role::Chat })
            }
        }
    }

    /// Who pays for the session in front's reviewer: your key while `a` on the limit card holds, else
    /// `llm.reviewer-billing`, else the session's.
    pub fn reviewer_billing(&self) -> Billing {
        match self.api_for_new() {
            true => Billing::Api,
            false => self.config.llm.reviewer_billing.unwrap_or(self.session.billing),
        }
    }

    /// `:session-billing api|subscription` — the session in front's (`billing_set`). A project's key binding never
    /// runs it: `config.rs` leaves such a binding out of `.tarae.toml` and `.tarae.local.toml`, said.
    pub fn session_billing(&mut self, arg: &str) -> Result<(), String> {
        let arg = arg.trim();
        if arg.is_empty() {
            let b = self.session.billing.name();
            self.set_status(format!("{} bills {b} — :session-billing api|subscription", self.session.name));
            return Ok(());
        }
        let billing = Billing::parse(arg).ok_or("usage: :session-billing api|subscription")?;
        self.billing_set(self.session.id, billing, false)
    }

    /// Session `sid` bills `to` from now: kept with it (the session list), its claude — if it runs — stopped and
    /// started again on the same conversation (`--resume`), the ask's spare in front going with it; the toast and
    /// the line over its chat's input say what the next message costs (`switchcost.rs`). The first time API goes
    /// on, `WEAKNESS` is said — `shown`: by the card answered. Err (nothing changed): API can't start here, or its
    /// Claude is answering.
    pub fn billing_set(
        &mut self,
        sid: crate::event::SessionId,
        to: Billing,
        shown: bool,
    ) -> Result<(), String> {
        // (where an API session can't start, said before anything changes)
        self.credential(to, Role::Chat)?;
        let mut done = Ok(false);
        self.with_session(sid, |ed| done = ed.billing_here(to));
        if done.clone()? && to == Billing::Api {
            self.with_session(sid, |ed| ed.weakness_said(shown));
        }
        done.map(|_| ())
    }

    /// `billing_set` in the session in front — whether it changed anything.
    fn billing_here(&mut self, to: Billing) -> Result<bool, String> {
        let name = self.session.name.clone();
        if to == self.session.billing {
            self.set_status(format!("{name} bills {} already", to.name()));
            return Ok(false);
        }
        if self.session.chat.as_ref().is_some_and(|c| c.busy()) {
            return Err(format!("{name}'s Claude is answering — C-c stops it, or switch after its turn"));
        }
        self.switching();
        self.session.billing = to;
        self.sessions_keep();
        crate::chat::billing_changed(self);
        crate::llm::spare_billing_changed(self);
        let said = match to {
            Billing::Api => "on your API key",
            Billing::Subscription => "on your plan",
        };
        let cost = self.switched_said();
        self.set_success(format!("{name} bills {} — Claude runs {said} from now{cost}", to.name()));
        Ok(true)
    }

    /// `$` in the session list (`space W`): session `sid`'s billing the other way — asked first on a card when
    /// it has a conversation going (its cache, warm or cold, makes the cost), else switched at once. To the key:
    /// only once `api_check` found it usable (else an error says why).
    pub fn billing_toggle(&mut self, sid: crate::event::SessionId) -> Result<(), String> {
        let l = std::iter::once(&self.session)
            .chain(&self.sessions)
            .find(|l| l.id == sid)
            .ok_or("no such session")?;
        let to = match l.billing {
            Billing::Subscription => Billing::Api,
            Billing::Api => Billing::Subscription,
        };
        if l.chat.as_ref().is_some_and(|c| c.busy()) {
            return Err(format!("{}'s Claude is answering — C-c stops it, or switch after its turn", l.name));
        }
        match to {
            Billing::Api => self.api_check(self.events.jobs(), move |ed, usable| {
                if let Err(e) = usable.and_then(|()| ed.billing_toggled(sid, to)) {
                    ed.set_error(e);
                }
            }),
            Billing::Subscription => self.billing_toggled(sid, to)?,
        }
        Ok(())
    }

    /// `billing_toggle` going on: the card, or switched at once.
    fn billing_toggled(&mut self, sid: crate::event::SessionId, to: Billing) -> Result<(), String> {
        let l = std::iter::once(&self.session)
            .chain(&self.sessions)
            .find(|l| l.id == sid)
            .ok_or("no such session")?;
        let talking = l.chat.as_ref().is_some_and(|c| c.conversation.is_some());
        if !talking {
            return self.billing_set(sid, to, false);
        }
        let mut card = None;
        self.with_session(sid, |ed| card = Some(ed.switch_card(to)));
        if let Some(card) = card {
            self.push_offer(card);
        }
        Ok(())
    }

    /// The card `$` puts up for the session in front's conversation going on `to`: what its next message costs
    /// by the cache's state, when the cache goes cold (wait, and it costs nothing extra), the weakness the first
    /// time API goes on.
    fn switch_card(&self, to: Billing) -> crate::offer::Offer {
        let name = &self.session.name;
        let cost = self.billing_cost(to);
        let (title, on, other) = match to {
            Billing::Api => (
                format!("{name}: bill your API key?"),
                "on your API key",
                "your plan's cache isn't your Console org's",
            ),
            Billing::Subscription => {
                (format!("{name}: back to your plan?"), "on your plan", "the key's cache isn't your plan's")
            }
        };
        let mut q = format!(
            "Its conversation goes on as it is — resumed {on}.\n\nThe next message {}",
            cost.said(to)
        );
        match cost {
            crate::switchcost::Cost::Warm { until_ms, .. } => {
                q += &format!(
                    " — {other}. The cache is warm until {}: switch after then and it costs nothing extra.",
                    crate::usage::until(until_ms)
                )
            }
            _ => q += ".",
        }
        let mut o = self.weakness_on(q, to);
        o.what = crate::offer::What::BillingSwitch { session: self.session.id, to };
        o.title = title;
        o
    }

    /// An offer saying `q` — with `WEAKNESS` after it, in the warning colour, when it turns API on (`to`) for the
    /// first time. Its title and what it is are the caller's.
    fn weakness_on(&self, mut q: String, to: Billing) -> crate::offer::Offer {
        let warn = (to == Billing::Api && self.weakness_due()).then(|| {
            let at = q.split('\n').count() + 1;
            q += &format!("\n\n{WEAKNESS}");
            at..at + 1
        });
        let what = crate::offer::What::BillingSwitch { session: self.session.id, to };
        let mut o = crate::offer::Offer::new(String::new(), &q, "Switching…", "Couldn't switch".into(), what)
            .asked(crate::cards::Asked::new(crate::cards::Why::Spend, crate::cards::Request::Own));
        if let Some(lines) = warn {
            o = o.warning(lines);
        }
        o
    }

    /// What the session in front's next message costs if it went on `to` now, on the model it runs.
    pub fn billing_cost(&self, to: Billing) -> crate::switchcost::Cost {
        let now = self.setup();
        let model = self.model_id(&now.model, &now);
        let prices = &crate::sandboxpack::get().price;
        crate::switchcost::cost(&self.session.usage, crate::usage::now_ms(), to, &model, prices)
    }

    /// `y` on the `$` card: session `sid` goes on `to`.
    pub(crate) fn switch_answer(&mut self, id: u64, sid: crate::event::SessionId, to: Billing) {
        self.offer_finished(id, Ok(()));
        if let Err(e) = self.billing_set(sid, to, true) {
            self.set_error(e);
        }
    }

    // ── The plan's limit, hit ───────────────────────────────────────────────

    /// The session in front's own claude said the plan refuses it (`rate_limit_event` `status: rejected`, or a
    /// turn ending on Claude Code's limit words): the 5-hour window — a toast, wait (never the key: your rule);
    /// a weekly one — the limit card, once for that window's period, unless the session bills the key already.
    pub fn limit_hit(&mut self, window: &str, resets_at_ms: Option<u64>) {
        let resets =
            resets_at_ms.map(|at| format!(" · resets {}", crate::limits::resets(at))).unwrap_or_default();
        let period = resets_at_ms.unwrap_or(0);
        if window == "five_hour" {
            let told = (window.to_string(), period);
            if !self.limits.said_rejected.contains(&told) {
                self.limits.said_rejected.push(told);
                self.set_warning(format!(
                    "5-hour limit{resets} — wait for it; the plan's weekly window is fine"
                ));
            }
            return;
        }
        if !window.starts_with("seven_day") || self.session.billing == Billing::Api {
            return;
        }
        if self.session.limit_asked == Some(period) {
            return;
        }
        self.session.limit_asked = Some(period);
        let ask = LimitAsk { session: self.session.id, window: window.to_string(), resets_at_ms };
        // (the key offered only where it can be used — else wait, said why)
        self.api_check(self.events.jobs(), move |ed, usable| match usable {
            Ok(()) => {
                let card = ed.limit_card(&ask);
                ed.push_offer(card);
            }
            Err(e) => {
                let label = crate::limits::label(&ask.window);
                ed.set_warning(format!(
                    "The plan's {label} limit{resets} — wait for it; the key can't take over: {e}"
                ));
            }
        });
    }

    /// The limit card: continue on the key — `y` this session · `a` new sessions and the reviewer too, until the
    /// reset (offered only when the reset is known) · `n` wait. What the first message on the key costs, by the
    /// cache's state; whether the plan's usage credits are paying already.
    fn limit_card(&self, ask: &LimitAsk) -> crate::offer::Offer {
        let label = crate::limits::label(&ask.window);
        let resets =
            ask.resets_at_ms.map(|at| format!(" · resets {}", crate::limits::resets(at))).unwrap_or_default();
        let cost = self.billing_cost(Billing::Api);
        let mut q = format!(
            "The plan refuses this conversation until its {label} window resets. Continue it on your API key?\n\n\
             The first message {}",
            cost.said(Billing::Api)
        );
        q += match cost {
            crate::switchcost::Cost::Warm { .. } => " — your plan's cache isn't your Console org's.",
            _ => ".",
        };
        if self.limits.overage == Some(true) {
            q += "\nYour plan's usage credits are paying now — the choice is which balance pays.";
        }
        let mut o = self.weakness_on(q, Billing::Api);
        let first = label[..1].to_uppercase() + &label[1..];
        o.title = format!("{first} limit reached{resets}");
        o.what = crate::offer::What::Limit(Box::new(ask.clone()));
        o
    }

    /// `y` (`all` false) or `a` on the limit card: its session goes on the key — now, or at its turn's end when
    /// Claude is still answering; with `a`, new sessions and the reviewer too, until the window resets (kept) —
    /// once `api_check` finds the key usable still, else nothing changes or is kept, said why.
    pub(crate) fn limit_answer(&mut self, id: u64, ask: LimitAsk, all: bool) {
        self.offer_finished(id, Ok(()));
        self.api_check(self.events.jobs(), move |ed, usable| match usable {
            Ok(()) => ed.limit_switch(ask, all),
            Err(e) => ed.set_error(format!("Still on your plan: {e}")),
        });
    }

    /// `limit_answer` with the key found usable.
    fn limit_switch(&mut self, ask: LimitAsk, all: bool) {
        if all && let Some(at) = ask.resets_at_ms {
            self.billing_kept.api_until_ms = Some(at);
            self.billing_kept.save(&self.events.jobs());
        }
        let mut later = false;
        self.with_session(ask.session, |ed| {
            if ed.session.chat.as_ref().is_some_and(|c| c.busy()) {
                ed.session.billing_next = Some(Billing::Api);
                later = true;
            }
        });
        let until = match (all, ask.resets_at_ms) {
            (true, Some(at)) => {
                format!(" — new sessions and the reviewer too, until {}", crate::limits::resets(at))
            }
            _ => String::new(),
        };
        match later {
            true => {
                self.weakness_said(true);
                self.set_success(format!("Your API key pays once this turn is over{until}"));
            }
            false => match self.billing_set(ask.session, Billing::Api, true) {
                Ok(()) if !until.is_empty() => self.set_success(format!("On your API key{until}")),
                Ok(()) => {}
                Err(e) => self.set_error(e),
            },
        }
    }

    /// `n` on the limit card: nothing changes — waiting for the reset.
    pub(crate) fn limit_wait(&mut self, ask: &LimitAsk) {
        let resets = ask
            .resets_at_ms
            .map(|at| format!(" — it resets {}", crate::limits::resets(at)))
            .unwrap_or_default();
        self.set_status(format!(
            "Waiting for the plan's {} window{resets}; $ in space W moves a session to your key",
            crate::limits::label(&ask.window)
        ));
    }
}

/// The session in front's turn is over: a billing that waited for it (the limit card's `y` mid-turn) goes on.
pub fn turn_over(ed: &mut Editor) {
    let Some(to) = ed.session.billing_next.take() else { return };
    if let Err(e) = ed.billing_set(ed.session.id, to, true) {
        ed.set_error(e);
    }
}

#[cfg(test)]
mod tests;
