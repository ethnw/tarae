//! Settings schema — one setting = one row of (path, kind, description, get, set).
//! File parsing, `:set`/`:toggle`, `:config-show` and (M4) the settings picker all use this one table.
//! New setting = one row here + an `EditorConfig` field.

use toml::Value;

use crate::config::{CursorShape, EditorConfig, LineNumber};

pub enum Kind {
    Bool,
    Int {
        min: i64,
        max: i64,
    },
    Enum(&'static [&'static str]),
    Str,
    StrList,
    /// Port numbers, at most `max` of them.
    Ports {
        max: usize,
    },
    /// `"any"`, or a list of host rules (`github.com`, `*.crates.io`, `host:8443` — `egress::Pattern`).
    Hosts,
}

impl Kind {
    pub fn check(&self, v: &Value) -> Result<(), String> {
        if let (Kind::Ports { max }, Value::Array(a)) = (self, v)
            && a.len() > *max
        {
            return Err(format!(
                "{} ports, at most {max}: the sandbox checks one rule per port on every connect, and many \
                 slow every command (measured: 1000 cost 20 ms, 5000 0.4 s) — llm.sandbox-localhost opens them all",
                a.len()
            ));
        }
        // (a rule the proxy can't read is said with the parser's words — never taken as some other host)
        if let (Kind::Hosts, Value::Array(a)) = (self, v) {
            for r in a {
                let Some(s) = r.as_str() else { continue };
                if let Err(why) = crate::sandbox::proxy::egress::Pattern::parse(s) {
                    return Err(format!("\"{s}\": {why}"));
                }
            }
        }
        let ok = match (self, v) {
            (Kind::Hosts, Value::String(s)) => s == "any",
            (Kind::Hosts, Value::Array(a)) => a.iter().all(Value::is_str),
            (Kind::Ports { .. }, Value::Array(a)) => {
                a.iter().all(|p| p.as_integer().is_some_and(|n| (1..=65535).contains(&n)))
            }
            (Kind::Bool, Value::Boolean(_)) => true,
            (Kind::Int { min, max }, Value::Integer(n)) => (min..=max).contains(&n),
            (Kind::Enum(opts), Value::String(s)) => opts.contains(&s.as_str()),
            (Kind::Str, Value::String(_)) => true,
            (Kind::StrList, Value::Array(a)) => a.iter().all(Value::is_str),
            _ => false,
        };
        if ok { Ok(()) } else { Err(format!("expected {}, got {v}", self.describe())) }
    }

    pub fn describe(&self) -> String {
        match self {
            Kind::Bool => "true | false".into(),
            Kind::Int { min, max } => format!("integer {min}..={max}"),
            Kind::Enum(opts) => opts.iter().map(|o| format!("\"{o}\"")).collect::<Vec<_>>().join(" | "),
            Kind::Str => "string".into(),
            Kind::StrList => "list of strings".into(),
            Kind::Ports { max } => format!("list of ports 1..=65535, at most {max}"),
            Kind::Hosts => "\"any\" | list of hosts (\"github.com\", \"*.crates.io\", \"host:8443\")".into(),
        }
    }
}

pub struct Setting {
    pub path: &'static str,
    pub kind: Kind,
    pub doc: &'static str,
    pub get: fn(&EditorConfig) -> Value,
    /// Values come in only after passing `kind.check`.
    pub set: fn(&mut EditorConfig, &Value),
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or_default()
}

fn b(v: &Value) -> bool {
    v.as_bool().unwrap_or_default()
}

fn n(v: &Value) -> usize {
    v.as_integer().unwrap_or_default().max(0) as usize
}

const SHAPES: &[&str] = &["block", "bar", "underline"];

fn shape_str(c: CursorShape) -> Value {
    Value::from(match c {
        CursorShape::Block => "block",
        CursorShape::Bar => "bar",
        CursorShape::Underline => "underline",
    })
}

fn shape(v: &Value) -> CursorShape {
    match s(v) {
        "bar" => CursorShape::Bar,
        "underline" => CursorShape::Underline,
        _ => CursorShape::Block,
    }
}

pub static SETTINGS: &[Setting] = &[
    Setting {
        path: "theme",
        kind: Kind::Str,
        doc: "Theme name — built-in meok, hanji, meok-transparent, hanji-transparent, or a file in ~/.config/tarae/themes; \"default\" = meok/hanji by terminal background",
        get: |c| Value::from(c.theme.as_str()),
        set: |c, v| c.theme = s(v).to_string(),
    },
    Setting {
        path: "editor.line-numbers",
        kind: Kind::Enum(&["absolute", "relative"]),
        doc: "Gutter line numbers",
        get: |c| {
            Value::from(match c.line_number {
                LineNumber::Absolute => "absolute",
                LineNumber::Relative => "relative",
            })
        },
        set: |c, v| {
            c.line_number = if s(v) == "relative" { LineNumber::Relative } else { LineNumber::Absolute }
        },
    },
    Setting {
        path: "editor.scrolloff",
        kind: Kind::Int { min: 0, max: 100 },
        doc: "Lines kept visible above/below the cursor",
        get: |c| Value::from(c.scrolloff as i64),
        set: |c, v| c.scrolloff = n(v),
    },
    Setting {
        path: "editor.tab-width",
        kind: Kind::Int { min: 1, max: 16 },
        doc: "Display width of a tab; the indent step when a file doesn't show its own",
        get: |c| Value::from(c.tab_width as i64),
        set: |c, v| c.tab_width = n(v),
    },
    Setting {
        path: "editor.color-modes",
        kind: Kind::Bool,
        doc: "Color the mode indicator by mode",
        get: |c| Value::from(c.color_modes),
        set: |c, v| c.color_modes = b(v),
    },
    Setting {
        path: "editor.cursorline",
        kind: Kind::Bool,
        doc: "Highlight the line of the primary cursor (theme: ui.cursorline.primary)",
        get: |c| Value::from(c.cursorline),
        set: |c, v| c.cursorline = b(v),
    },
    Setting {
        path: "editor.header",
        kind: Kind::Bool,
        doc: "Top bar: path › definition under the cursor, and open buffers",
        get: |c| Value::from(c.header),
        set: |c, v| c.header = b(v),
    },
    Setting {
        path: "editor.inlay-hints",
        kind: Kind::Bool,
        doc: "Show language server inlay hints (types, parameter names) as dim virtual text",
        get: |c| Value::from(c.inlay_hints),
        set: |c, v| c.inlay_hints = b(v),
    },
    Setting {
        path: "editor.auto-save",
        kind: Kind::Enum(&["off", "focus", "idle"]),
        doc: "Save modified files automatically: when the terminal loses focus, or also after 2 s idle",
        get: |c| Value::from(c.auto_save.as_str()),
        set: |c, v| c.auto_save = v.as_str().unwrap_or("focus").to_string(),
    },
    Setting {
        path: "editor.terminal-escape",
        kind: Kind::Str,
        doc: "The key that gives the keys back to the code from the terminal (space T)",
        get: |c| Value::from(c.terminal_escape.as_str()),
        set: |c, v| c.terminal_escape = v.as_str().unwrap_or("C-\\").to_string(),
    },
    Setting {
        path: "editor.persistent-undo",
        kind: Kind::Bool,
        doc: "Keep undo history across restarts (saved next to your state, dropped if the file changed elsewhere)",
        get: |c| Value::from(c.persistent_undo),
        set: |c, v| c.persistent_undo = b(v),
    },
    Setting {
        path: "editor.recovery",
        kind: Kind::Bool,
        doc: "Keep a copy of each buffer with unsaved edits (after a pause in typing, in your state folder) — after a crash or a kill, the next start offers them back",
        get: |c| Value::from(c.recovery),
        set: |c, v| c.recovery = b(v),
    },
    Setting {
        path: "editor.recovery-skip",
        kind: Kind::StrList,
        doc: "Files whose unsaved edits are never copied — a name matches anywhere, in any case (\"*.secret\"), a path is from the project root (\"private/\") or ~/; keys and tokens never are (~/.ssh, .env, *.pem…)",
        get: |c| Value::Array(c.recovery_skip.iter().map(|a| Value::from(a.as_str())).collect()),
        set: |c, v| {
            c.recovery_skip =
                v.as_array().into_iter().flatten().filter_map(|a| a.as_str().map(str::to_string)).collect()
        },
    },
    Setting {
        path: "editor.restart-build",
        kind: Kind::Str,
        doc: "For working on tarae itself: a command :restart runs first, e.g. \"cargo build --release\" (\"\" = none; :restart! skips it)",
        get: |c| Value::from(c.restart_build.as_str()),
        set: |c, v| c.restart_build = s(v).to_string(),
    },
    Setting {
        path: "editor.restart-binary",
        kind: Kind::Str,
        doc: "The tarae :restart runs, from the project root if relative (\"\" = the same path as the one running now)",
        get: |c| Value::from(c.restart_binary.as_str()),
        set: |c, v| c.restart_binary = s(v).to_string(),
    },
    Setting {
        path: "editor.offer-grammars",
        kind: Kind::Bool,
        doc: "When a file's language has no syntax grammar yet, offer to download and build it (y/n)",
        get: |c| Value::from(c.offer_grammars),
        set: |c, v| c.offer_grammars = b(v),
    },
    Setting {
        path: "editor.offer-installs",
        kind: Kind::Bool,
        doc: "When a language server or debugger is missing, offer to install it the way its pack says (y/n/N)",
        get: |c| Value::from(c.offer_installs),
        set: |c, v| c.offer_installs = b(v),
    },
    Setting {
        path: "editor.render-doc-comments",
        kind: Kind::Bool,
        doc: "Show doc comments (///, //!, /** */) rendered as Markdown; raw when the cursor enters the block",
        get: |c| Value::from(c.render_doc_comments),
        set: |c, v| c.render_doc_comments = b(v),
    },
    Setting {
        path: "editor.restore-files",
        kind: Kind::Bool,
        doc: "Reopen the files you had open in this folder (with cursor positions) when started without files",
        get: |c| Value::from(c.restore_files),
        set: |c, v| c.restore_files = b(v),
    },
    Setting {
        path: "editor.inline-diagnostics",
        kind: Kind::Bool,
        doc: "Show diagnostic messages at the end of their line and tint error/warning lines",
        get: |c| Value::from(c.inline_diagnostics),
        set: |c, v| c.inline_diagnostics = b(v),
    },
    Setting {
        path: "editor.cursor-diagnostics",
        kind: Kind::Bool,
        doc: "Cursor on an underlined problem whose message doesn't fit at the line end: show it in full in a card (Esc hides it)",
        get: |c| Value::from(c.cursor_diagnostics),
        set: |c, v| c.cursor_diagnostics = b(v),
    },
    Setting {
        path: "editor.soft-wrap",
        kind: Kind::Enum(&["always", "prose", "never"]),
        doc: "Show long lines as several rows: always, in prose only (Markdown, commit messages, plain text), or never",
        get: |c| Value::from(c.soft_wrap.as_str()),
        set: |c, v| c.soft_wrap = v.as_str().unwrap_or("always").to_string(),
    },
    Setting {
        path: "editor.format-on-save",
        kind: Kind::Bool,
        doc: ":w formats with the language server first (saves as is if it can't within 2 s)",
        get: |c| Value::from(c.format_on_save),
        set: |c, v| c.format_on_save = b(v),
    },
    Setting {
        path: "editor.git-blame",
        kind: Kind::Bool,
        doc: "Show who last changed the cursor line, when, and why at its end (b in the git tab flips it)",
        get: |c| Value::from(c.git_blame),
        set: |c, v| c.git_blame = b(v),
    },
    Setting {
        path: "editor.auto-pairs",
        kind: Kind::Bool,
        doc: "Typing ( [ { \" ' ` adds the closer; typing the closer steps over it; backspace removes both",
        get: |c| Value::from(c.auto_pairs),
        set: |c, v| c.auto_pairs = b(v),
    },
    Setting {
        path: "editor.indent-guides",
        kind: Kind::Bool,
        doc: "Faint vertical guides in leading indentation (theme: ui.virtual.indent-guide)",
        get: |c| Value::from(c.indent_guides),
        set: |c, v| c.indent_guides = b(v),
    },
    Setting {
        path: "editor.scrollbar",
        kind: Kind::Bool,
        doc: "Scrollbar on the right edge, with error/warning marks for the whole file",
        get: |c| Value::from(c.scrollbar),
        set: |c, v| c.scrollbar = b(v),
    },
    Setting {
        path: "editor.lsp",
        kind: Kind::Bool,
        doc: "Start language servers (servers: [lsp.<name>] command/args, per language: [lang.<lang>] lsp = [...])",
        get: |c| Value::from(c.lsp.enabled),
        set: |c, v| c.lsp.enabled = b(v),
    },
    Setting {
        path: "editor.lsp-share",
        kind: Kind::StrList,
        doc: "Language servers one of which serves every checkout of a repository (a session each) on the same toolchain — swapped as checkouts come and go; unset: those whose pack says `share`",
        // (unset: the servers whose pack says `share`)
        get: |c| {
            let list = c.lsp.share.clone().unwrap_or_else(crate::lsp::shared_by_packs);
            Value::Array(list.into_iter().map(Value::from).collect())
        },
        set: |c, v| {
            c.lsp.share = Some(
                v.as_array().into_iter().flatten().filter_map(|a| a.as_str().map(str::to_string)).collect(),
            )
        },
    },
    Setting {
        path: "editor.cursor.normal",
        kind: Kind::Enum(SHAPES),
        doc: "Cursor shape in normal mode",
        get: |c| shape_str(c.cursor_shape.0),
        set: |c, v| c.cursor_shape.0 = shape(v),
    },
    Setting {
        path: "editor.cursor.insert",
        kind: Kind::Enum(SHAPES),
        doc: "Cursor shape in insert mode",
        get: |c| shape_str(c.cursor_shape.1),
        set: |c, v| c.cursor_shape.1 = shape(v),
    },
    Setting {
        path: "editor.cursor.select",
        kind: Kind::Enum(SHAPES),
        doc: "Cursor shape in select mode",
        get: |c| shape_str(c.cursor_shape.2),
        set: |c, v| c.cursor_shape.2 = shape(v),
    },
    Setting {
        path: "llm.command",
        kind: Kind::Str,
        doc: "LLM CLI speaking claude's stream-json protocol",
        get: |c| Value::from(c.llm.command.as_str()),
        set: |c, v| c.llm.command = s(v).to_string(),
    },
    Setting {
        path: "llm.args",
        kind: Kind::StrList,
        doc: "Arguments for llm.command (default: claude -p with speed flags)",
        get: |c| Value::Array(c.llm.args.iter().map(|a| Value::from(a.as_str())).collect()),
        set: |c, v| {
            c.llm.args =
                v.as_array().into_iter().flatten().filter_map(|a| a.as_str().map(str::to_string)).collect()
        },
    },
    Setting {
        path: "llm.model",
        kind: Kind::Str,
        doc: "Model for llm.command, e.g. \"haiku\" (\"\" = the CLI's default)",
        get: |c| Value::from(c.llm.model.as_deref().unwrap_or("")),
        set: |c, v| c.llm.model = Some(s(v).to_string()).filter(|m| !m.is_empty()),
    },
    Setting {
        path: "llm.effort",
        kind: Kind::Enum(&["", "low", "medium", "high", "xhigh", "max"]),
        doc: "How hard Claude thinks, for models that take it (\"\" = the model's default)",
        get: |c| Value::from(c.llm.effort.as_str()),
        set: |c, v| c.llm.effort = s(v).to_string(),
    },
    Setting {
        path: "llm.follow",
        kind: Kind::Bool,
        doc: "Chat: the editor follows the code Claude is reading, with its thoughts beside it (C-f toggles it in a chat)",
        get: |c| Value::from(c.llm.follow),
        set: |c, v| c.llm.follow = b(v),
    },
    Setting {
        path: "llm.follow-resume",
        kind: Kind::Int { min: 0, max: 3600 },
        doc: "Chat: seconds after your last key that follow takes over again while Claude still works (0 = only C-f)",
        get: |c| Value::from(c.llm.follow_resume as i64),
        set: |c, v| c.llm.follow_resume = n(v) as u64,
    },
    Setting {
        path: "llm.stuck-after",
        kind: Kind::Int { min: 0, max: 1440 },
        doc: "Chat: minutes a run of Claude's may print nothing at next to no CPU before a card asks whether it's stuck (0 = never)",
        get: |c| Value::from(c.llm.stuck_after as i64),
        set: |c, v| c.llm.stuck_after = n(v) as u64,
    },
    Setting {
        path: "llm.stuck-after-outside",
        kind: Kind::Int { min: 0, max: 1440 },
        doc: "Chat: …and for a run whose work may go on elsewhere — granted a daemon's socket (Docker's), run outside the sandbox on your y, connected outside its processes — with no time limit of its own (a run all under its own `timeout` isn't asked before that ends, two hours at most) (0 = as llm.stuck-after)",
        get: |c| Value::from(c.llm.stuck_after_outside as i64),
        set: |c, v| c.llm.stuck_after_outside = n(v) as u64,
    },
    Setting {
        path: "llm.review-at-end",
        kind: Kind::Bool,
        doc: "Chat: Claude's edits go in as it makes them, and you review its turn when it ends — every new chat starts so (space v a turns it on and off, and saves it here)",
        get: |c| Value::from(c.llm.review_at_end),
        set: |c, v| c.llm.review_at_end = b(v),
    },
    Setting {
        path: "llm.chat-settings",
        kind: Kind::Bool,
        doc: "Chat: read your CLAUDE.md files and Claude Code settings, like `claude` does (hooks stay off, edits always ask)",
        get: |c| Value::from(c.llm.chat_settings),
        set: |c, v| c.llm.chat_settings = b(v),
    },
    Setting {
        path: "llm.environment-key",
        kind: Kind::Enum(&["drop", "keep"]),
        doc: "Claude: an ANTHROPIC_API_KEY or ANTHROPIC_AUTH_TOKEN in tarae's environment — \"drop\": Claude runs on your Claude Code login; \"keep\": passed on, for a login by key alone or a gateway (ANTHROPIC_BASE_URL with its token). Inside a Claude Code session always dropped (the host's). Only your own config sets it",
        get: |c| Value::from(if c.llm.keep_env_key { "keep" } else { "drop" }),
        set: |c, v| c.llm.keep_env_key = s(v) == "keep",
    },
    Setting {
        path: "llm.billing",
        kind: Kind::Enum(&["subscription", "api"]),
        doc: "Who pays for a new session's Claude — \"subscription\": your Claude Code login; \"api\": your Anthropic API key, from the macOS Keychain (item tarae.anthropic-api-key — tarae never reads it). A session keeps its own; :session-billing switches it. Only your own config sets it",
        get: |c| Value::from(c.llm.billing.name()),
        set: |c, v| c.llm.billing = crate::billing::Billing::parse(s(v)).unwrap_or_default(),
    },
    Setting {
        path: "llm.reviewer-billing",
        kind: Kind::Enum(&["same", "subscription", "api"]),
        doc: "Who pays for the reviewer's Claude (space v c) — \"same\": as its session. Only your own config sets it",
        get: |c| Value::from(c.llm.reviewer_billing.map_or("same", crate::billing::Billing::name)),
        set: |c, v| c.llm.reviewer_billing = crate::billing::Billing::parse(s(v)),
    },
    Setting {
        path: "llm.screen-terminal",
        kind: Kind::Bool,
        doc: "Chat: when Claude looks at your screen (you say yes on a card), the terminal panel's contents go too — off: its rows go blank",
        get: |c| Value::from(c.llm.screen_terminal),
        set: |c, v| c.llm.screen_terminal = b(v),
    },
    Setting {
        path: "llm.run-allow",
        kind: Kind::StrList,
        doc: "Chat: commands Claude runs without asking, past the checks (cargo build/test/clippy/fmt, git status/diff/log…) — word prefixes, like \"npm test\"",
        get: |c| Value::Array(c.llm.run_allow.iter().map(|a| Value::from(a.as_str())).collect()),
        set: |c, v| {
            c.llm.run_allow =
                v.as_array().into_iter().flatten().filter_map(|a| a.as_str().map(str::to_string)).collect()
        },
    },
    Setting {
        path: "llm.review-ask",
        kind: Kind::StrList,
        doc: "Chat, review at the end (space v a): more files whose edits still ask first — a name matches anywhere, in any case (\"schema.sql\"), a path from the project root (\"src/secret/\"); the protected ones (.git, .tarae.toml, build.rs, pom.xml… — every language pack's) always ask",
        get: |c| Value::Array(c.llm.review_ask.iter().map(|a| Value::from(a.as_str())).collect()),
        set: |c, v| {
            c.llm.review_ask =
                v.as_array().into_iter().flatten().filter_map(|a| a.as_str().map(str::to_string)).collect()
        },
    },
    Setting {
        path: "llm.sandbox",
        kind: Kind::Bool,
        doc: "Chat: Claude's commands run in a sandbox — they write only in the project (not .git, build.rs, pom.xml… — the protected names, every language pack's), temp and the build's folders, and connect to nothing but the ports you open (macOS)",
        get: |c| Value::from(c.llm.sandbox),
        set: |c, v| c.llm.sandbox = b(v),
    },
    Setting {
        path: "llm.sandbox-network",
        kind: Kind::Bool,
        doc: "Chat: the sandbox lets Claude's commands reach the network beyond this machine, only through tarae's proxy — HTTPS_PROXY, set for every run, where a tool that ignores it can't go out; on Linux where the session has a network of its own, no variable: the proxy takes every connection and answers the names; never link-local addresses, the cloud metadata servers tarae knows of, multicast or broadcast. This machine's ports — on loopback or its own addresses — only as llm.sandbox-ports and -localhost open them",
        get: |c| Value::from(c.llm.sandbox_network),
        set: |c, v| c.llm.sandbox_network = b(v),
    },
    Setting {
        path: "llm.sandbox-hosts",
        kind: Kind::Hosts,
        doc: "Chat, with llm.sandbox-network on (unused while it's off): the hosts Claude's commands reach without asking — \"any\", the default, is every host; with a list (\"github.com\", \"*.crates.io\" — its subdomains, \"host:8443\"; else 443, or 80 for plain http) another host waits up to 20 s on a card: this run, this session, always here, or no",
        get: |c| match &c.llm.sandbox_hosts {
            None => Value::from("any"),
            Some(h) => Value::Array(h.iter().map(|p| Value::from(p.to_string())).collect()),
        },
        set: |c, v| {
            c.llm.sandbox_hosts = v.as_array().map(|a| {
                let rules = a.iter().filter_map(Value::as_str);
                rules.filter_map(|s| crate::sandbox::proxy::egress::Pattern::parse(s).ok()).collect()
            })
        },
    },
    Setting {
        path: "llm.sandbox-upstream",
        kind: Kind::Enum(&["env", "none"]),
        doc: "Chat, with llm.sandbox-network on: \"env\" — where tarae's own environment names a proxy (HTTPS_PROXY, HTTP_PROXY, ALL_PROXY; NO_PROXY's hosts direct), tarae's proxy goes on through it — http://, or SOCKS5 (socks5:// and socks5h:// alike: the name goes to it): hosts and TLS server names are still checked, the addresses they lead to are that proxy's to look up and aren't; an https:// or SOCKS4 one isn't chained to, and nothing goes that way. \"none\": straight out, every address checked",
        get: |c| Value::from(c.llm.sandbox_upstream.as_str()),
        set: |c, v| {
            if let Some(s) = v.as_str().and_then(crate::sandbox::proxy::upstream::Setting::parse) {
                c.llm.sandbox_upstream = s
            }
        },
    },
    Setting {
        path: "llm.sandbox-ports",
        kind: Kind::Ports { max: crate::sandbox::PORTS_MAX },
        doc: "Chat: ports on this machine Claude's commands may connect to in the sandbox — a dev server, a test's database ([3000, 5432])",
        get: |c| Value::Array(c.llm.sandbox_ports.iter().map(|p| Value::from(i64::from(*p))).collect()),
        set: |c, v| {
            c.llm.sandbox_ports = v
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| p.as_integer().and_then(|n| u16::try_from(n).ok()))
                .collect()
        },
    },
    Setting {
        path: "llm.sandbox-localhost",
        kind: Kind::Bool,
        doc: "Chat: every port on this machine open to Claude's commands (tests that talk to their own servers) — the risky ones too (Docker, Jupyter…): the sandbox can't leave some out. On macOS that opens other runs' ports to tarae's proxy too, which then only their tokens guard",
        get: |c| Value::from(c.llm.sandbox_localhost),
        set: |c, v| c.llm.sandbox_localhost = b(v),
    },
    Setting {
        path: "llm.sandbox-kept-days",
        kind: Kind::Int { min: 1, max: 3650 },
        doc: "Chat: days a toolchain's home kept for Claude's commands (Go's build cache, Gradle's home and daemon) may go unused before tarae's start removes it",
        get: |c| Value::from(c.llm.sandbox_kept_days as i64),
        set: |c, v| c.llm.sandbox_kept_days = n(v) as u64,
    },
    Setting {
        path: "llm.sandbox-write",
        kind: Kind::StrList,
        doc: "Chat: more folders the sandbox lets Claude's commands write to (\"~/scratch\") — never the project's own folders above it",
        get: |c| Value::Array(c.llm.sandbox_write.iter().map(|a| Value::from(a.as_str())).collect()),
        set: |c, v| {
            c.llm.sandbox_write =
                v.as_array().into_iter().flatten().filter_map(|a| a.as_str().map(str::to_string)).collect()
        },
    },
    Setting {
        path: "llm.sandbox-env",
        kind: Kind::StrList,
        doc: "Chat: more of your environment Claude's commands get (\"DATABASE_URL\", \"MYAPP_*\") — they start with PATH, HOME, the locale and the toolchains' own; a token-like one only by its whole name (:sandbox-env lists them)",
        get: |c| Value::Array(c.llm.sandbox_env.iter().map(|a| Value::from(a.as_str())).collect()),
        set: |c, v| {
            c.llm.sandbox_env =
                v.as_array().into_iter().flatten().filter_map(|a| a.as_str().map(str::to_string)).collect()
        },
    },
    Setting {
        path: "llm.sandbox-protect",
        kind: Kind::StrList,
        doc: "Chat: paths in the project the sandbox keeps Claude's commands from changing (\"migrations/\"); the protected names (.git, .tarae.toml, build.rs, pom.xml… — every language pack's) always, anywhere in it",
        get: |c| Value::Array(c.llm.sandbox_protect.iter().map(|a| Value::from(a.as_str())).collect()),
        set: |c, v| {
            c.llm.sandbox_protect =
                v.as_array().into_iter().flatten().filter_map(|a| a.as_str().map(str::to_string)).collect()
        },
    },
    Setting {
        path: "llm.context-lines",
        kind: Kind::Int { min: 0, max: 1000 },
        doc: "Lines of context sent around each selection",
        get: |c| Value::from(c.llm.context_lines as i64),
        set: |c, v| c.llm.context_lines = n(v),
    },
    Setting {
        path: "session.worktrees",
        kind: Kind::Str,
        doc: "Where sessions' checkouts go, a folder per repository (\"\" = $XDG_DATA_HOME/tarae/worktrees)",
        get: |c| Value::from(c.session_worktrees.as_str()),
        set: |c, v| c.session_worktrees = s(v).to_string(),
    },
    Setting {
        path: "session.ended",
        kind: Kind::StrList,
        doc: "A command, its words, run once a session's checkout is removed (:session-end) — in the checkout tarae started in, outside the sandbox, TARAE_ENDED_CHECKOUT naming the one removed; stopped after 2 minutes; how it ended goes to the log (e.g. [\"cargo\", \"xtask\", \"linux\", \"prune\"]; [] = none)",
        get: |c| Value::Array(c.session_ended.iter().map(|a| Value::from(a.as_str())).collect()),
        set: |c, v| {
            c.session_ended =
                v.as_array().into_iter().flatten().filter_map(|a| a.as_str().map(str::to_string)).collect()
        },
    },
    Setting {
        path: "review.risky",
        kind: Kind::StrList,
        doc: "Code review: files whose every change is high-risk — looked at by you (space v h) before the review ends or the session is brought. A name matches anywhere, in any case, one * per part (\"sandbox*\"); a path from the project root (\".github/\"). The language packs' patterns (unsafe, spawning…) and the protected names count too. Read from the checkout tarae started in",
        get: |c| Value::Array(c.review_risky.iter().map(|a| Value::from(a.as_str())).collect()),
        set: |c, v| {
            c.review_risky =
                v.as_array().into_iter().flatten().filter_map(|a| a.as_str().map(str::to_string)).collect()
        },
    },
    Setting {
        path: "review.quiet",
        kind: Kind::StrList,
        doc: "Code review: files that are tests, docs or snapshots — their changes graded quiet (never over a high-risk one). Names and paths as review.risky",
        get: |c| Value::Array(c.review_quiet.iter().map(|a| Value::from(a.as_str())).collect()),
        set: |c, v| {
            c.review_quiet =
                v.as_array().into_iter().flatten().filter_map(|a| a.as_str().map(str::to_string)).collect()
        },
    },
];

/// Settings that had another name: (old path, new path). A config file's old name still applies, and a
/// warning names the new one.
const RENAMED: [(&str, &str); 1] = [("editor.restore-session", "editor.restore-files")];

pub fn find(path: &str) -> Option<&'static Setting> {
    SETTINGS.iter().find(|s| s.path == path)
}

pub fn apply(cfg: &mut EditorConfig, path: &str, v: &Value) -> Result<(), String> {
    let setting = find(path).ok_or_else(|| format!("unknown setting '{path}'"))?;
    setting.kind.check(v).map_err(|e| format!("{path}: {e}"))?;
    (setting.set)(cfg, v);
    Ok(())
}

/// `:set` argument → value. As-is if it parses as TOML (`8`, `true`, `["a"]`), else a string (`relative`).
pub fn parse_value(raw: &str) -> Value {
    toml::from_str::<toml::Table>(&format!("v = {raw}"))
        .ok()
        .and_then(|mut t| t.remove("v"))
        .unwrap_or_else(|| Value::from(raw))
}

/// Apply a config file table (`keys` belongs to the keymap, skipped). Problems as `file:line: msg` warnings.
#[cfg(test)]
pub fn apply_table(
    cfg: &mut EditorConfig,
    table: &toml::Table,
    src: &str,
    file: &str,
    warnings: &mut Vec<String>,
) {
    walk(cfg, "", table, src, file, warnings, &mut |_, _| true);
}

/// `apply_table`, with each setting first put to `pass` (path, value) — false keeps it out (`trust.rs`).
pub fn apply_table_gated(
    cfg: &mut EditorConfig,
    table: &toml::Table,
    src: &str,
    file: &str,
    warnings: &mut Vec<String>,
    pass: &mut dyn FnMut(&str, &Value) -> bool,
) {
    walk(cfg, "", table, src, file, warnings, pass);
}

fn walk(
    cfg: &mut EditorConfig,
    prefix: &str,
    t: &toml::Table,
    src: &str,
    file: &str,
    w: &mut Vec<String>,
    pass: &mut dyn FnMut(&str, &Value) -> bool,
) {
    for (k, v) in t {
        let path = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
        // Free-form-named tables (keymap, language servers, per-language settings, `[[attach]]`, `:preflight`'s
        // steps) are read by config.rs.
        if prefix.is_empty()
            && matches!(path.as_str(), "keys" | "lsp" | "lang" | "attach" | "task" | "preflight" | "cards")
        {
            continue;
        }
        let at = || match locate(src, &path) {
            Some(line) => format!("{file}:{line}: "),
            None => format!("{file}: "),
        };
        if let Some(&(_, new)) = RENAMED.iter().find(|r| r.0 == path) {
            if pass(new, v)
                && let Err(e) = apply(cfg, new, v)
            {
                w.push(format!("{}{e}", at()));
            }
            w.push(format!("{}'{path}' is called '{new}' now — rename it there", at()));
        } else if find(&path).is_some() {
            if !pass(&path, v) {
                continue;
            }
            if let Err(e) = apply(cfg, &path, v) {
                w.push(format!("{}{e}", at()));
            }
        } else if let Value::Table(sub) = v
            && SETTINGS.iter().any(|s| s.path.starts_with(&format!("{path}.")))
        {
            walk(cfg, &path, sub, src, file, w, pass);
        } else {
            w.push(format!("{}unknown setting '{path}'", at()));
        }
    }
}

/// Line number (from 1) where a setting path is written. Lightweight tracking of only `[section]` headers and
/// `key =` lines — keys inside an inline table (`cursor = { … }`) resolve to that line.
pub fn locate(src: &str, path: &str) -> Option<usize> {
    let mut section = String::new();
    for (i, line) in src.lines().enumerate() {
        let line = line.trim();
        if let Some(h) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = h.trim_matches(['[', ']']).trim().to_string();
            if section == path {
                return Some(i + 1);
            }
            continue;
        }
        let Some((key, _)) = line.split_once('=') else { continue };
        let key = key.trim().trim_matches('"');
        let full = if section.is_empty() { key.to_string() } else { format!("{section}.{key}") };
        if full == path || path.starts_with(&format!("{full}.")) {
            return Some(i + 1);
        }
    }
    None
}

/// The whole effective config as TOML (with description comments) — `:config-show`.
pub fn show(cfg: &EditorConfig) -> String {
    let mut out = String::from(
        "# tarae — effective settings\n# :set <path> <value> changes it until you quit, :set! also saves it\n",
    );
    let mut section = "";
    for s in SETTINGS {
        let (sec, key) = s.path.rsplit_once('.').unwrap_or(("", s.path));
        if sec != section && !sec.is_empty() {
            out.push_str(&format!("\n[{sec}]\n"));
            section = sec;
        }
        out.push_str(&format!("# {} ({})\n{key} = {}\n", s.doc, s.kind.describe(), (s.get)(cfg)));
    }
    out
}

/// Generated docs (`docs/reference/*.md`) are compared with a fresh render; `TARAE_BLESS=1` rewrites
/// the file instead. Shared by the settings and keymap reference tests.
#[cfg(test)]
pub(crate) fn check_generated(path: &str, fresh: &str, test: &str) {
    if std::env::var_os("TARAE_BLESS").is_some_and(|v| v == "1") {
        std::fs::write(path, fresh).unwrap_or_else(|e| panic!("writing {path}: {e}"));
        return;
    }
    let on_disk = std::fs::read_to_string(path).unwrap_or_default();
    assert!(on_disk == fresh, "{path} is out of date — regenerate it with `TARAE_BLESS=1 cargo test {test}`");
}

/// Prose for a Markdown table cell — a `|` would end the cell, and `<n>` would be taken for HTML.
#[cfg(test)]
pub(crate) fn md_cell(s: &str) -> String {
    s.replace('|', "\\|").replace('<', "&lt;")
}

/// Inline code for a table cell; survives backticks inside (`` ` `` → a double-backtick span).
#[cfg(test)]
pub(crate) fn md_code(s: &str) -> String {
    let s = s.replace('|', "\\|");
    if s.contains('`') { format!("`` {s} ``") } else { format!("`{s}`") }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `docs/reference/settings.md` — one table per config section, straight from `SETTINGS`.
    fn reference() -> String {
        let defaults = EditorConfig::default();
        let mut out = String::from(
            "<!-- Generated from src/settings.rs — do not edit; regenerate with \
             `TARAE_BLESS=1 cargo test settings_reference`. -->\n\
             # Settings reference\n\n\
             Every setting tarae reads from `~/.config/tarae/config.toml` (or a project `.tarae.toml`).\n\
             The name is also the `:set` path: `:set editor.scrolloff 8` changes it until you quit,\n\
             `:set!` also writes it to your config file. How files and layers work: \
             [configuration guide](../configuration.md).\n",
        );
        let mut section = None;
        for s in SETTINGS {
            let sec = s.path.rsplit_once('.').map(|(sec, _)| sec);
            if section != Some(sec) {
                section = Some(sec);
                let title = sec.map(|t| format!("`[{t}]`")).unwrap_or_else(|| "Top level".into());
                out.push_str(&format!(
                    "\n## {title}\n\n| Setting | Type | Default | Description |\n|---|---|---|---|\n"
                ));
            }
            let kind = match &s.kind {
                Kind::Bool => "bool".to_string(),
                Kind::Int { min, max } => format!("integer {min}–{max}"),
                Kind::Enum(opts) => {
                    opts.iter().map(|o| md_code(&format!("\"{o}\""))).collect::<Vec<_>>().join(" · ")
                }
                Kind::Str => "string".into(),
                Kind::StrList => "list of strings".into(),
                Kind::Ports { max } => format!("list of ports (at most {max})"),
                Kind::Hosts => md_code("\"any\"") + " or a list of hosts",
            };
            let default = md_code(&(s.get)(&defaults).to_string());
            out.push_str(&format!("| {} | {kind} | {default} | {} |\n", md_code(s.path), md_cell(s.doc)));
        }
        out.push_str(
            "\n## Tables you name yourself\n\n\
             These are read alongside the settings above; see the [configuration guide](../configuration.md).\n\n\
             | Table | What it holds |\n|---|---|\n\
             | `[keys.normal]` · `[keys.select]` · `[keys.insert]` | Key bindings on top of the \
             [default keymap](keymap.md) |\n\
             | `[lsp.<name>]` | A language server: `command`, `args`, `except` (features it's not asked for) |\n\
             | `[lang.<language>]` | Per language: `lsp = [\"name\", …]` — which server to try, in order; `also = […]` — \
             servers that run beside it |\n\
             | `[[attach]]` | A named debugger attach target: `name`, `lang`, `host`, `port` or `pid`, \
             `before`, `remote-root`, `program` |\n\
             | `[[cards.warn]]` | What a card for one of Claude's commands warns of, besides the language packs' — \
             only adds (an id a pack or your config has is left out): `id`, `run`, `options`, `option-values`, \
             `missing`, `unless-part`, `unless-all`, `same`, `unless-in`, `fact`, `say`, `asks` — see \
             [Claude integration](../claude-integration.md) |\n",
        );
        out
    }

    #[test]
    fn settings_reference_is_up_to_date() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/reference/settings.md");
        check_generated(path, &reference(), "settings_reference");
    }

    #[test]
    fn every_setting_roundtrips_its_default() {
        let mut cfg = EditorConfig::default();
        for s in SETTINGS {
            let v = (s.get)(&cfg);
            s.kind.check(&v).unwrap_or_else(|e| panic!("{}: default fails its own kind: {e}", s.path));
            apply(&mut cfg, s.path, &v).unwrap();
            assert_eq!((s.get)(&cfg), v, "{}", s.path);
        }
    }

    #[test]
    fn parse_value_accepts_bare_words() {
        assert_eq!(parse_value("8"), Value::Integer(8));
        assert_eq!(parse_value("true"), Value::Boolean(true));
        assert_eq!(parse_value("relative"), Value::from("relative"));
        assert_eq!(parse_value("\"a b\""), Value::from("a b"));
        assert_eq!(parse_value("[\"-p\"]"), Value::Array(vec![Value::from("-p")]));
    }

    #[test]
    fn warnings_carry_line_numbers() {
        let src = "[editor]\nscrolloff = 500\nnope = 1\ncursor = { insert = \"wiggle\" }\n";
        let table: toml::Table = toml::from_str(src).unwrap();
        let mut cfg = EditorConfig::default();
        let mut w = Vec::new();
        apply_table(&mut cfg, &table, src, "config.toml", &mut w);
        w.sort();
        assert_eq!(w.len(), 3, "{w:?}");
        assert!(w.iter().any(|m| m.starts_with("config.toml:2: editor.scrolloff: expected integer 0..=100")));
        assert!(w.iter().any(|m| m.starts_with("config.toml:3: unknown setting 'editor.nope'")));
        assert!(w.iter().any(|m| m.starts_with("config.toml:4: editor.cursor.insert")));
    }

    /// A setting under its old name still applies, and the warning names the new one — nothing a user set
    /// goes quietly unread. Every new name is a setting.
    #[test]
    fn an_old_name_still_applies_and_says_the_new_one() {
        let src = "[editor]\nrestore-session = true\n";
        let table: toml::Table = toml::from_str(src).unwrap();
        let mut cfg = EditorConfig::default();
        let mut w = Vec::new();
        apply_table(&mut cfg, &table, src, "config.toml", &mut w);
        assert!(cfg.restore_files);
        assert_eq!(
            w,
            [
                "config.toml:2: 'editor.restore-session' is called 'editor.restore-files' now — rename it there"
            ]
        );
        assert!(RENAMED.iter().all(|r| find(r.1).is_some() && find(r.0).is_none()));
    }

    /// `llm.sandbox-hosts` takes `"any"` or a list of rules the proxy reads (`[]`: every host asked); a rule it
    /// can't read is refused where it's set, in the parser's words — never kept as some other host, never dropped
    /// in silence (a dropped rule would ask for a host the user meant to let go; a mangled one could let another).
    #[test]
    fn a_host_rule_is_checked_where_it_is_set() {
        let set = |v: Value| {
            let mut cfg = EditorConfig::default();
            apply(&mut cfg, "llm.sandbox-hosts", &v).map(|_| cfg.llm.sandbox_hosts)
        };
        assert_eq!(set(Value::from("any")), Ok(None));
        let list = ["github.com", "*.crates.io", "api.test:8443"];
        let rules = set(Value::Array(list.iter().map(|s| Value::from(*s)).collect())).unwrap().unwrap();
        assert_eq!(rules.iter().map(ToString::to_string).collect::<Vec<_>>(), list);
        assert_eq!(set(Value::Array(Vec::new())), Ok(Some(Vec::new())));
        for (bad, why) in [
            ("1.2.3.4", "an address — a rule names hosts"),
            ("*", "a `*` but as `*.` at its start"),
            ("a.test:0", "a port that isn't 1–65535"),
        ] {
            let e = set(Value::Array(vec![Value::from("github.com"), Value::from(bad)])).unwrap_err();
            assert!(e.contains(&format!("\"{bad}\": {why}")), "{bad}: {e}");
        }
        assert!(set(Value::from("all")).is_err(), "a string but \"any\"");
    }

    #[test]
    fn show_is_loadable_config() {
        let text = show(&EditorConfig::default());
        let table: toml::Table = toml::from_str(&text).unwrap();
        let mut w = Vec::new();
        apply_table(&mut EditorConfig::default(), &table, &text, "x", &mut w);
        assert!(w.is_empty(), "{w:?}");
    }
}
