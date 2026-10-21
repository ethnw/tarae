//! The environment a sandboxed run gets (lsbenv, 2026-10-06; measured in `target/sb/lsbenv-measured.md`). Not
//! tarae's own: that holds whatever the user exported — API keys, cloud credentials, the Claude Code session
//! tarae was started from — and every command Claude ran got it. Measured with fake tokens on both systems:
//! every pack's checks (cargo test/clippy/fmt, go test/vet, python, npm test, git, gradle) passed with PATH
//! alone, and npm hands its whole environment on to the scripts it runs. So a run gets only what this module
//! lets through — every other variable of tarae's removed by name (`sandbox::from_run_env`; never `env_clear`:
//! `spawn::setsid` can't see a clear, and std drops the removals made after one) — from `Here`:
//! - the core's floor (`FLOOR`, `FLOOR_FAMILIES`): where things are and how text reads — no tool's name but
//!   git's, which is tarae's own — never the user's proxies (`PROXIES`: tarae's own, with the network open);
//! - every variable the policy itself reads to place things (`Packs::env_names`: the toolchains' homes, their
//!   settings' and caches' variables) — a run that saw another `CARGO_HOME` than the policy would look where
//!   nothing was opened;
//! - the packs' `pass` (names, `PREFIX*` families) — the toolchains' own variables, said by their packs;
//! - the user's `llm.sandbox-env` (names, `PREFIX*`).
//!
//! What looks like a token (`tokens::token_like` on `NAME=value`: a name for a secret with a value, a URL with
//! a password, an option giving one, a known prefix) is left out whole, whatever passed it —
//! `CARGO_REGISTRY_TOKEN` inside `CARGO_*`, `YARN_NPM_AUTH_IDENT` inside `YARN_*`, a `GRADLE_OPTS` holding
//! `-D…Password=…`, a proxy with a password — but a name the user wrote whole in `llm.sandbox-env`. What tarae
//! sets goes on top (`sandbox::command`: `TARAE_SANDBOXED`, `GIT_OPTIONAL_LOCKS`, `TMPDIR`, `TARAE_SANDBOX_STATUS`
//! …). What's left out is said by name (`RunEnv::said`, `:sandbox-env`), never by value; its `Debug` prints names
//! only.

use std::ffi::OsString;

use super::Here;

/// What every run gets as it is: where things are, who runs, how text reads, the temp folder (a run with a
/// folder of its own gets that instead) — and git's settings and identity: git is tarae's own
/// (`tokens::sources_on` reads `GIT_CONFIG_GLOBAL` to find what to copy).
const FLOOR: [&str; 21] = [
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "TZ",
    "TERM",
    "TMPDIR",
    "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "__CF_USER_TEXT_ENCODING",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "GIT_CONFIG_NOSYSTEM",
    "GIT_AUTHOR_NAME",
    "GIT_AUTHOR_EMAIL",
    "GIT_COMMITTER_NAME",
    "GIT_COMMITTER_EMAIL",
];

/// The floor's families: the locale's parts (`LC_CTYPE` alone is common on macOS — no `LANG`).
const FLOOR_FAMILIES: [&str; 1] = ["LC_"];

/// The user's proxies: never passed — while the network is open (`llm.sandbox-network`) tarae's egress proxy's
/// own take their place (`proxy::run_env`), and those aren't said as left out.
pub(crate) const PROXIES: [&str; 8] = [
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
];

/// What the shell keeps up itself (`sh -c` sets them again): not said as left out.
const THE_SHELLS: [&str; 4] = ["PWD", "OLDPWD", "SHLVL", "_"];

/// The loader's variables: code it loads into every program. Never in a pack's `pass` — only the user's own
/// `llm.sandbox-env` (`LD_LIBRARY_PATH`, decided 2026-10-06).
pub(crate) const LOADER: [&str; 2] = ["LD_", "DYLD_"];

/// Why a variable reaches the run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum By {
    /// The core's floor (and the proxies).
    Floor,
    /// The policy reads it to place things (a toolchain's home, a setting's file).
    Policy,
    /// A pack's `pass`.
    Pack,
    /// `llm.sandbox-env`.
    Yours,
}

impl By {
    pub fn said(self) -> &'static str {
        match self {
            By::Floor => "the floor",
            By::Policy => "a toolchain's home or settings",
            By::Pack => "a language pack",
            By::Yours => "llm.sandbox-env",
        }
    }
}

/// A run's environment from tarae's: what goes in, and the names of what doesn't.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct RunEnv {
    /// Passed, by name (sorted), with why.
    pub vars: Vec<(String, OsString, By)>,
    /// Left out, token-like (by name or value) — names.
    pub tokens: Vec<String>,
    /// Left out, the rest — names (but what the shell or tarae sets anyway).
    pub others: Vec<String>,
}

/// Names only: a value never reaches a log through a policy's `Debug`.
impl std::fmt::Debug for RunEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.vars.iter().map(|(k, ..)| k.as_str()).collect();
        f.debug_struct("RunEnv")
            .field("vars", &names)
            .field("tokens", &self.tokens)
            .field("others", &self.others)
            .finish()
    }
}

/// Whether `name` is one `entry` says: the name itself, or a `PREFIX*` family's.
pub(crate) fn matches(entry: &str, name: &str) -> bool {
    match entry.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => name == entry,
    }
}

/// Whether `entry` is a name or a family a list may hold: letters, digits and `_`, then an optional `*` — a
/// family with at least two characters before it (a bare `*` would pass everything).
pub(crate) fn entry_ok(entry: &str) -> bool {
    let stem = entry.strip_suffix('*').unwrap_or(entry);
    let family = stem.len() < entry.len();
    !stem.is_empty()
        && (!family || stem.len() >= 2)
        && stem.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `NAME=value` looks like a token (`tokens::token_like`).
fn token_like(name: &str, value: &OsString) -> bool {
    crate::tokens::token_like(&format!("{name}={}", value.to_string_lossy()))
}

/// What a run gets of `here`'s variables: the floor, what the policy reads (`policy`), the packs' `pass`, the
/// user's `yours` — no token-like one but a name of `yours` written whole. `set`: what tarae sets on top (not
/// said as left out).
pub fn of(here: &Here, policy: &[&str], pass: &[String], yours: &[String], set: &[&str]) -> RunEnv {
    let yours: Vec<&String> = yours.iter().filter(|e| entry_ok(e)).collect();
    let by = |name: &str| -> Option<By> {
        if FLOOR.contains(&name) || FLOOR_FAMILIES.iter().any(|f| name.starts_with(f)) {
            Some(By::Floor)
        } else if policy.contains(&name) {
            Some(By::Policy)
        } else if pass.iter().any(|e| matches(e, name)) {
            Some(By::Pack)
        } else if yours.iter().any(|e| matches(e, name)) {
            Some(By::Yours)
        } else {
            None
        }
    };
    let mut names: Vec<&String> = here.env.keys().collect();
    crate::sorting::sort(&mut names);
    let mut out = RunEnv::default();
    for name in names {
        let value = &here.env[name];
        let named_whole = yours.contains(&name);
        match by(name) {
            _ if token_like(name, value) && !named_whole => out.tokens.push(name.clone()),
            Some(why) => out.vars.push((name.clone(), value.clone(), why)),
            None if THE_SHELLS.contains(&name.as_str()) || set.contains(&name.as_str()) => {}
            None => out.others.push(name.clone()),
        }
    }
    out
}

/// Names grouped for a report: each name's first part (up to its first `_`) shared by two or more is one
/// `PART_* (n)` — `CLAUDE_* (28)`, `ZELLIJ* (3)` when one is the part itself.
fn grouped(names: &[String]) -> Vec<String> {
    let mut parts: std::collections::BTreeMap<&str, Vec<&str>> = Default::default();
    for n in names {
        let part = match n.find('_') {
            Some(i) if i > 0 => &n[..i],
            _ => n.as_str(),
        };
        parts.entry(part).or_default().push(n);
    }
    parts
        .into_iter()
        .map(|(part, of)| match of.len() {
            1 => of[0].to_string(),
            n if of.contains(&part) => format!("{part}* ({n})"),
            n => format!("{part}_* ({n})"),
        })
        .collect()
}

/// What Claude is told once a conversation on macOS (F2, spikes 2b and 2c): no rule of Seatbelt's keeps a
/// command from another process's command line and starting environment (KERN_PROCARGS2 — not the
/// `process-info*` rules, not a settings name, not every settings read denied), so a variable left out of its
/// own is still within reach. Linux keeps environments in (Landlock: `landlock::tests::
/// a_confined_run_reads_no_environment_outside_it`), so it isn't said there.
pub(super) const OTHERS_READ: &str = " On macOS left out isn't out of reach: a command can read the command line and starting \
     environment of the user's other processes (tarae's, their shell's, other sessions' commands) — no rule of \
     macOS's sandbox closes it. Don't read them for a value left out here; ask the user.";

impl RunEnv {
    /// A variable as the run gets it, in UTF-8 — None: left out, or not set.
    pub fn var(&self, name: &str) -> Option<String> {
        let (_, value, _) = self.vars.iter().find(|(k, ..)| k == name)?;
        value.clone().into_string().ok()
    }

    /// Every name left out.
    pub fn left_out(&self) -> impl Iterator<Item = &String> {
        self.tokens.iter().chain(&self.others)
    }

    /// What Claude reads of the names left out that `said` doesn't hold yet — which it then does (once a
    /// chat: `Chat::env_said`). Token-like ones by name, the rest grouped (`grouped`). None: nothing new.
    /// On macOS (`os`) the first time also says left out isn't out of reach (`OTHERS_READ`).
    pub fn said(&self, said: &mut Vec<String>, os: super::Os) -> Option<String> {
        let new = |names: &[String]| -> Vec<String> {
            names.iter().filter(|n| !said.contains(n)).cloned().collect()
        };
        let (tokens, others) = (new(&self.tokens), new(&self.others));
        if tokens.is_empty() && others.is_empty() {
            return None;
        }
        let mut parts = Vec::new();
        if !tokens.is_empty() {
            parts.push(format!("token-like: {}", tokens.join(", ")));
        }
        if !others.is_empty() {
            let label = if tokens.is_empty() { "" } else { "the rest: " };
            parts.push(format!("{label}{}", grouped(&others).join(", ")));
        }
        let first = said.is_empty();
        said.extend(tokens.into_iter().chain(others));
        let others_read = if first && os == super::Os::Mac { OTHERS_READ } else { "" };
        Some(format!(
            "Your commands' environment holds PATH, HOME, the locale and the toolchains' own variables — not the \
             rest of the user's. Left out (names only) — {}. A command that needs one of them fails without it: \
             run it again with also {{\"env\": NAME}} and your reason — the user decides on a card (:sandbox-env \
             lists them all); don't work around it.{}",
            parts.join("; "),
            others_read
        ))
    }

    /// The names left out that the run's `output` names as a whole word (`DATABASE_URL not set`, `$MYAPP_KEY`)
    /// — said every time: it may be why the run failed. None: none.
    pub fn named_in(&self, output: &[(bool, String)]) -> Option<String> {
        let named = self.named(output);
        let (asked, told): (Vec<&String>, Vec<&String>) =
            named.iter().partition(|n| crate::readonly::env_allowed(n));
        let mut parts = Vec::new();
        if !asked.is_empty() {
            let also: Vec<String> = asked.iter().map(|n| format!("{{\"env\": \"{n}\"}}")).collect();
            parts.push(format!(
                "if that's why it failed, run it again with also {} and your reason — the user decides on a card",
                also.join(" · ")
            ));
        }
        if !told.is_empty() {
            parts.push(format!(
                "no card passes {} — if the command needs it, tell the user",
                told.iter().map(|n| n.as_str()).collect::<Vec<_>>().join(", ")
            ));
        }
        (!named.is_empty()).then(|| {
            format!(
                "Its output names {}, left out of your commands' environment — {}.",
                named.join(", "),
                parts.join("; ")
            )
        })
    }

    /// The names left out that `output` names as a whole word, in `left_out`'s order.
    pub fn named(&self, output: &[(bool, String)]) -> Vec<String> {
        let word = |c: char| c.is_ascii_alphanumeric() || c == '_';
        let names = |l: &str, n: &str| {
            l.match_indices(n).any(|(i, _)| {
                !l[..i].chars().next_back().is_some_and(word)
                    && !l[i + n.len()..].chars().next().is_some_and(word)
            })
        };
        self.left_out().filter(|n| output.iter().any(|(_, l)| names(l, n))).cloned().collect()
    }

    /// `:sandbox-env`'s page: what goes in and why, then what's left out — names only.
    pub fn page(&self) -> String {
        let mut s = String::from(
            "# What Claude's sandboxed commands get of tarae's environment — names only, never a value\n\n## Passed\n",
        );
        let w = self.vars.iter().map(|(k, ..)| k.len()).max().unwrap_or(0);
        for (k, _, by) in &self.vars {
            s.push_str(&format!("{k:<w$}  {}\n", by.said()));
        }
        s.push_str(
            "\n## Left out — token-like (a name for a secret, a URL with a password, a token's prefix)\n",
        );
        for k in &self.tokens {
            s.push_str(&format!("{k}\n"));
        }
        s.push_str("\n## Left out — the rest\n");
        for k in &self.others {
            s.push_str(&format!("{k}\n"));
        }
        s.push_str(
            "\nllm.sandbox-env passes more: a name, or a family (\"MYAPP_*\") — a token-like one only by its whole \
             name.\n",
        );
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::Os;

    const FAKE: &str = "tarae-fake-token";

    fn here(vars: &[(&str, &str)]) -> Here {
        Here {
            env: vars.iter().map(|(k, v)| (k.to_string(), OsString::from(v))).collect(),
            ..Here::default()
        }
    }

    fn names(e: &RunEnv) -> Vec<&str> {
        e.vars.iter().map(|(k, ..)| k.as_str()).collect()
    }

    /// The floor, what the policy reads, a pack's family and the user's own go in; fake tokens — by name,
    /// inside a passed family, a proxy with a password — and the rest stay out, said by name; a name the user
    /// wrote whole passes though token-like; the proxies only with the network open; no value in `Debug`.
    #[test]
    fn the_runs_environment_is_the_floor_the_packs_and_yours() {
        let h = here(&[
            ("PATH", "/usr/bin:/bin"),
            ("HOME", "/Users/u"),
            ("LC_CTYPE", "UTF-8"),
            ("CARGO_HOME", "/Users/u/.cargo"),
            ("RUSTFLAGS", "--cfg tarae_fake"),
            ("CARGO_REGISTRY_TOKEN", FAKE),
            ("GITHUB_TOKEN", FAKE),
            ("AWS_SECRET_ACCESS_KEY", FAKE),
            ("HTTPS_PROXY", "http://u:tarae-fake-token@proxy.example.com:3128"),
            ("NO_PROXY", "localhost"),
            ("DATABASE_URL", "postgres://db.example.com/x"),
            ("MY_API_KEY", FAKE),
            ("CLAUDE_CODE_SESSION_ID", "x"),
            ("CLAUDE_PID", "1"),
            ("PWD", "/x"),
            ("NO_COLOR", "1"),
        ]);
        let pass = vec!["CARGO_*".to_string(), "RUST*".to_string()];
        let yours = vec!["DATABASE_URL".to_string(), "MY_API_KEY".to_string()];
        let e = of(&h, &["CARGO_HOME"], &pass, &yours, &["NO_COLOR"]);
        assert_eq!(
            names(&e),
            ["CARGO_HOME", "DATABASE_URL", "HOME", "LC_CTYPE", "MY_API_KEY", "PATH", "RUSTFLAGS"]
        );
        let by = |n: &str| e.vars.iter().find(|(k, ..)| k == n).map(|(.., b)| *b);
        assert_eq!(
            [by("PATH"), by("CARGO_HOME"), by("RUSTFLAGS"), by("DATABASE_URL")],
            [Some(By::Floor), Some(By::Policy), Some(By::Pack), Some(By::Yours)]
        );
        assert_eq!(
            e.tokens,
            ["AWS_SECRET_ACCESS_KEY", "CARGO_REGISTRY_TOKEN", "GITHUB_TOKEN", "HTTPS_PROXY"]
        );
        assert_eq!(
            e.others,
            ["CLAUDE_CODE_SESSION_ID", "CLAUDE_PID", "NO_PROXY"],
            "PWD and NO_COLOR: set anyway"
        );
        assert!(!format!("{e:?}").contains(FAKE) && !format!("{e:?}").contains("/usr/bin"), "{e:?}");
        // (the user's proxies never pass: with the network open tarae's own take their place — `run_env_of`)
        // A family of the user's carries no token-like name; a bare `*` is no family
        let fam = of(&h, &[], &[], &["MY_*".into(), "*".into()], &[]);
        assert!(fam.tokens.contains(&"MY_API_KEY".to_string()) && !names(&fam).contains(&"CLAUDE_PID"));
    }

    /// A passed family carries no credential in a form it's written in: yarn's auth ident, an option variable
    /// giving a password (`-D…Password=`) — the whole variable left out, never rewritten; options giving none
    /// pass, and so do git's author variables (`AUTHOR` isn't `AUTH`).
    #[test]
    fn credentials_in_passed_families_and_options_stay_out() {
        let h = here(&[
            ("GIT_AUTHOR_NAME", "Stand In"),
            ("GIT_AUTHOR_EMAIL", "stand.in@example.com"),
            ("YARN_NPM_AUTH_IDENT", "stand-in:tarae-fake-token"),
            ("YARN_ENABLE_COLORS", "false"),
            ("JAVA_OPTS", "-Xmx2g -Dorg.gradle.project.repoPassword=tarae-fake-token"),
            ("MAVEN_OPTS", "-Dpassword=tarae-fake-token"),
            ("GRADLE_OPTS", "-Xmx2g -Dfile.encoding=UTF-8"),
        ]);
        let pass = ["YARN_*", "JAVA_OPTS", "MAVEN_OPTS", "GRADLE_*"].map(String::from);
        let e = of(&h, &[], &pass, &[], &[]);
        assert_eq!(names(&e), ["GIT_AUTHOR_EMAIL", "GIT_AUTHOR_NAME", "GRADLE_OPTS", "YARN_ENABLE_COLORS"]);
        assert_eq!(e.tokens, ["JAVA_OPTS", "MAVEN_OPTS", "YARN_NPM_AUTH_IDENT"]);
        assert!(!format!("{e:?}").contains(FAKE));
    }

    /// The report: token-like names by name, the rest grouped by their first part; a chat hears each name
    /// once — a new one is said, an old one isn't; never a value.
    #[test]
    fn the_report_names_what_was_left_out_once_a_chat() {
        let e = RunEnv {
            vars: vec![],
            tokens: vec!["GEMINI_API_KEY".into(), "GITHUB_TOKEN".into()],
            others: [
                "CLAUDE_CODE_A",
                "CLAUDE_CODE_B",
                "CLAUDE_PID",
                "ZELLIJ",
                "ZELLIJ_PANE_ID",
                "BAGGAGE",
                "__CFBundleIdentifier",
            ]
            .map(String::from)
            .into(),
        };
        let mut said = Vec::new();
        let first = e.said(&mut said, Os::Mac).unwrap();
        assert!(
            first.contains(
                "Left out (names only) — token-like: GEMINI_API_KEY, GITHUB_TOKEN; the rest: BAGGAGE, CLAUDE_* (3), \
                 ZELLIJ* (2), __CFBundleIdentifier."
            ),
            "{first}"
        );
        assert!(first.contains("also {\"env\": NAME}") && first.contains(":sandbox-env"), "{first}");
        // (F2: on macOS the first time also says other processes' environments are within reach — never on
        // Linux, where Landlock keeps them in)
        assert!(first.ends_with(OTHERS_READ), "{first}");
        let linux = e.said(&mut Vec::new(), Os::Linux).unwrap();
        assert!(!linux.contains("On macOS") && linux.ends_with("don't work around it."), "{linux}");
        assert_eq!(e.said(&mut said, Os::Mac), None, "said again in the chat");
        let more = RunEnv { others: vec!["NEW_ONE".into()], ..e.clone() };
        let next = more.said(&mut said, Os::Mac).unwrap();
        assert!(next.contains("Left out (names only) — NEW_ONE.") && !next.contains("GITHUB"), "{next}");
        assert!(!next.contains("On macOS"), "once a conversation: {next}");
        let page = e.page();
        assert!(page.contains("GITHUB_TOKEN\n") && page.contains("CLAUDE_CODE_A\n"), "{page}");
        // A name the output says, as a word, is said every time — not one inside another word
        let out = |l: &str| vec![(true, l.to_string())];
        let named = more.named_in(&out("error: NEW_ONE not set")).unwrap();
        assert!(named.starts_with("Its output names NEW_ONE, left out"), "{named}");
        assert_eq!(more.named_in(&out("X_NEW_ONE and NEW_ONES")), None);
        assert!(more.named_in(&out("$GITHUB_TOKEN is empty")).is_some_and(|s| s.contains("GITHUB_TOKEN")));
        // (piece D) what a card passes is asked for with also; what none passes is the user's to tell
        assert!(named.contains("also {\"env\": \"NEW_ONE\"}"), "{named}");
        let never = RunEnv { others: vec!["NEW_ONE".into(), "RUSTFLAGS".into()], ..e.clone() };
        let named = never.named_in(&out("NEW_ONE, RUSTFLAGS")).unwrap();
        assert!(!named.contains("\"RUSTFLAGS\"") && named.contains("no card passes RUSTFLAGS"), "{named}");
        assert_eq!(never.named(&out("RUSTFLAGS")), ["RUSTFLAGS"]);
    }

    #[test]
    fn what_an_entry_may_be() {
        for ok in ["CARGO_*", "PATH", "npm_config_*", "GO111MODULE"] {
            assert!(entry_ok(ok), "{ok}");
        }
        for no in ["*", "A*", "", "A-B", "A*B", "A B"] {
            assert!(!entry_ok(no), "{no}");
        }
    }
}
