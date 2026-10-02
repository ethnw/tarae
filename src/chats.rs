//! The chat's conversations, kept per project — so closing tarae doesn't end one.
//!
//! - `$XDG_STATE_HOME/tarae/chats/<project hash>.json`: the latest `KEEP` conversations, newest first — each
//!   its claude session id, a title (the first question), when it was last used, and its rows as the chat
//!   showed them. Written after each turn, read when the chat opens — both on worker threads.
//! - Claude's memory of it is claude's own session (`--resume <id>`, measured: stream-json keeps the id and
//!   the context); ours is only what the panel shows. A session claude no longer has ("No conversation
//!   found") starts a fresh one with the rows still shown.
//! - Opening the chat continues the latest conversation if it's recent (`FRESH`); `C-p` in the chat or
//!   `:chat-resume` picks an earlier one.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::chat::{Link, Look, Msg, Role};

/// Conversations kept per project.
const KEEP: usize = 20;
/// Rows kept per conversation (the oldest go).
const ROWS: usize = 400;
/// The latest conversation is continued when the chat opens if it was used this recently.
pub const FRESH: u64 = 8 * 3600;

/// One conversation as stored.
#[derive(Clone, Debug)]
pub struct Saved {
    /// claude's session id.
    pub id: String,
    pub title: String,
    /// Unix seconds.
    pub updated: u64,
    pub rows: Vec<Value>,
    /// Its plan (`plan.rs`), if Claude made one.
    pub plan: Option<Value>,
    /// When this snapshot was taken (ns) — saves run on workers and may finish out of order; an older one
    /// never replaces a newer (`save`).
    pub seq: u64,
}

/// A snapshot's `seq` — taken on the main loop, in order.
pub fn seq() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64)
}

/// Saves read, merge and write the file one at a time.
static SAVING: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// The file for the project at `root`, in `dir` (the state dir's `chats/`).
pub fn file(dir: &Path, root: &Path) -> PathBuf {
    let h = root
        .to_string_lossy()
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3));
    dir.join(format!("{h:016x}.json"))
}

pub fn default_dir() -> Option<PathBuf> {
    Some(crate::config::state_dir()?.join("chats"))
}

/// The project's conversations, newest first (on a worker).
pub fn load(dir: &Path, root: &Path) -> Vec<Saved> {
    let Ok(body) = std::fs::read_to_string(file(dir, root)) else { return Vec::new() };
    let Ok(v) = serde_json::from_str::<Value>(&body) else { return Vec::new() };
    v["chats"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|c| {
                    Some(Saved {
                        id: c["id"].as_str()?.to_string(),
                        title: c["title"].as_str().unwrap_or_default().to_string(),
                        updated: c["updated"].as_u64().unwrap_or(0),
                        rows: c["rows"].as_array().cloned().unwrap_or_default(),
                        plan: c.get("plan").filter(|p| !p.is_null()).cloned(),
                        seq: c["seq"].as_u64().unwrap_or(0),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `chat` in front, replacing its older self (on a worker — read, merge, write).
pub fn save(dir: &Path, root: &Path, chat: Saved) {
    let _one = SAVING.lock().unwrap_or_else(|e| e.into_inner());
    let mut all = load(dir, root);
    if all.iter().any(|c| c.id == chat.id && c.seq > chat.seq) {
        return;
    }
    all.retain(|c| c.id != chat.id);
    all.insert(0, chat);
    all.truncate(KEEP);
    let chats: Vec<Value> = all
        .into_iter()
        .map(
            |c| json!({ "id": c.id, "title": c.title, "updated": c.updated, "rows": c.rows, "plan": c.plan, "seq": c.seq }),
        )
        .collect();
    let body = json!({ "root": root, "chats": chats }).to_string();
    let _ = std::fs::create_dir_all(dir);
    let _ = crate::disk::write_atomic(&file(dir, root), |w| w.write_all(body.as_bytes()));
}

fn role_name(r: Role) -> &'static str {
    match r {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Note => "note",
        Role::Tool => "tool",
        Role::Thought => "thought",
    }
}

/// The panel's rows as stored (the newest `ROWS`).
pub fn rows_of(msgs: &[Msg]) -> Vec<Value> {
    msgs[msgs.len().saturating_sub(ROWS)..]
        .iter()
        .map(|m| {
            let mut v = json!({ "role": role_name(m.role), "text": m.text });
            if !m.chip.is_empty() {
                v["chip"] = json!(m.chip);
            }
            if let Some(l) = &m.look {
                v["look"] = json!({
                    "verb": l.verb, "what": l.what, "pattern": l.pattern, "range": l.range,
                    "path": l.link.as_ref().map(|k| k.path.clone()), "line": l.link.as_ref().map(|k| k.line),
                });
            }
            v
        })
        .collect()
}

/// Stored rows back as the panel's (unknown roles or verbs are skipped — a newer tarae wrote them).
pub fn msgs_of(rows: &[Value]) -> Vec<Msg> {
    const VERBS: [&str; 12] =
        ["read", "search", "find", "uses", "edit", "write", "note", "reply", "test", "show", "look", "plan"];
    rows.iter()
        .filter_map(|r| {
            let s = |k: &str| r[k].as_str().unwrap_or_default().to_string();
            let role = match r["role"].as_str()? {
                "user" => Role::User,
                "assistant" => Role::Assistant,
                "note" => Role::Note,
                "tool" => Role::Tool,
                "thought" => Role::Thought,
                _ => return None,
            };
            let mut m = Msg::new(role, s("text"), s("chip"));
            if role == Role::Tool {
                let l = &r["look"];
                let verb = VERBS.into_iter().find(|v| l["verb"] == *v).unwrap_or("use");
                let link = l["path"]
                    .as_str()
                    .map(|p| Link { path: PathBuf::from(p), line: l["line"].as_u64().unwrap_or(0) as usize });
                m.look = Some(Look {
                    verb,
                    what: l["what"].as_str().unwrap_or_default().to_string(),
                    pattern: l["pattern"].as_bool().unwrap_or(false),
                    range: l["range"].as_str().unwrap_or_default().to_string(),
                    link,
                });
            }
            Some(m)
        })
        .collect()
}

/// Conversation `id` of the `store` file as Markdown — the picker's preview: your questions quoted, its
/// answers, its lookups as a list (thinking and notes left out).
pub fn transcript(store: &Path, id: &str) -> Option<String> {
    let body = std::fs::read_to_string(store).ok()?;
    let v: Value = serde_json::from_str(&body).ok()?;
    let chat = v["chats"].as_array()?.iter().find(|c| c["id"] == id)?;
    let mut out = String::new();
    for r in chat["rows"].as_array()? {
        let text = r["text"].as_str().unwrap_or_default().trim();
        let role = r["role"].as_str().unwrap_or_default();
        // (a list of lookups ends before the words that follow)
        if matches!(role, "user" | "assistant") && !out.is_empty() && !out.ends_with("\n\n") {
            out.push('\n');
        }
        match role {
            "user" => {
                for l in text.lines() {
                    out.push_str(&format!("> {l}\n"));
                }
                out.push('\n');
            }
            "assistant" => {
                out.push_str(&crate::chat::displayed(text));
                out.push_str("\n\n");
            }
            "tool" => {
                let l = &r["look"];
                let (verb, what) = (l["verb"].as_str().unwrap_or("use"), l["what"].as_str().unwrap_or(""));
                out.push_str(&format!("- {verb} `{what}`\n"));
            }
            _ => {}
        }
    }
    Some(out)
}

/// How long ago, in a few characters (`just now` · `12 min ago` · `3 h ago` · `2 days ago`).
pub fn ago(then: u64, now: u64) -> String {
    let s = now.saturating_sub(then);
    match s {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", s / 60),
        3600..86400 => format!("{} h ago", s / 3600),
        86400..172800 => "yesterday".into(),
        _ => format!("{} days ago", s / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_go_and_come_back_and_the_newest_conversation_leads() {
        let dir = std::env::temp_dir().join(format!("tarae-chats-{}", std::process::id()));
        let root = Path::new("/p/project");
        let mut tool = Msg::new(Role::Tool, "", "");
        tool.look = Some(Look {
            verb: "read",
            what: "src/a.rs".into(),
            pattern: false,
            range: "L1–9".into(),
            link: Some(Link { path: "/p/project/src/a.rs".into(), line: 0 }),
        });
        let msgs =
            vec![Msg::new(Role::User, "why?", "a.rs · L3"), tool, Msg::new(Role::Assistant, "Because.", "")];
        let saved = |id: &str, t: u64| Saved {
            id: id.into(),
            title: "why?".into(),
            updated: t,
            rows: rows_of(&msgs),
            plan: Some(json!({"title": "t", "steps": [{"text": "a", "status": "doing"}]})),
            seq: t,
        };
        save(&dir, root, saved("s1", 10));
        save(&dir, root, saved("s2", 20));
        save(&dir, root, saved("s1", 30));
        // A save that finishes late with an older snapshot doesn't replace the newer one
        save(&dir, root, Saved { rows: Vec::new(), ..saved("s1", 25) });
        let all = load(&dir, root);
        assert_eq!(
            all.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["s1", "s2"],
            "one each, newest first"
        );
        assert_eq!(all[0].rows.len(), 3, "the newer snapshot stays");
        assert_eq!(all[0].plan.as_ref().unwrap()["steps"][0]["status"], "doing", "its plan kept too");
        let back = msgs_of(&all[0].rows);
        assert_eq!(back.len(), 3);
        assert_eq!((back[0].role, back[0].chip.as_str()), (Role::User, "a.rs · L3"));
        assert_eq!(back[1].look, msgs[1].look);
        assert!(load(&dir, Path::new("/elsewhere")).is_empty(), "per project");
        assert_eq!(
            transcript(&file(&dir, root), "s1").unwrap(),
            "> why?\n\n- read `src/a.rs`\n\nBecause.\n\n",
            "the picker's preview"
        );
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(ago(100, 100), "just now");
        assert_eq!(ago(0, 7300), "2 h ago");
    }
}
