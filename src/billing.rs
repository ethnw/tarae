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

impl Editor {
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

    /// Who pays for the session in front's reviewer: `llm.reviewer-billing`, else the session's.
    pub fn reviewer_billing(&self) -> Billing {
        self.config.llm.reviewer_billing.unwrap_or(self.session.billing)
    }

    /// `:session-billing api|subscription` — the session in front's; its claude, if it runs, stops and resumes
    /// the same conversation on it; the ask's spare goes and comes back on it. Refused while Claude answers.
    pub fn session_billing(&mut self, arg: &str) -> Result<(), String> {
        let arg = arg.trim();
        if arg.is_empty() {
            let b = self.session.billing.name();
            self.set_status(format!("{} bills {b} — :session-billing api|subscription", self.session.name));
            return Ok(());
        }
        let billing = Billing::parse(arg).ok_or("usage: :session-billing api|subscription")?;
        if billing == self.session.billing {
            self.set_status(format!("{} bills {} already", self.session.name, billing.name()));
            return Ok(());
        }
        // (where an API session can't start, said before anything changes)
        self.credential(billing, Role::Chat)?;
        if self.session.chat.as_ref().is_some_and(|c| c.busy()) {
            return Err("Claude is answering — C-c stops it, or switch after its turn".into());
        }
        self.session.billing = billing;
        self.sessions_keep();
        crate::chat::billing_changed(self);
        crate::llm::spare_billing_changed(self);
        let said = match billing {
            Billing::Api => "on your API key",
            Billing::Subscription => "on your plan",
        };
        self.set_success(format!(
            "{} bills {} — Claude runs {said} from now",
            self.session.name,
            billing.name()
        ));
        Ok(())
    }
}

#[cfg(test)]
mod tests;
