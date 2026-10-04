//! The plan — Claude lays out work of several steps before it starts, and keeps it current as it goes
//! (tarae's `plan` tool, `tools.rs`; this claude has no todo tool of its own — measured).
//!
//! - One call carries the whole list (title, steps with `todo`/`doing`/`done`) — the latest wins.
//! - It belongs to the conversation: pinned under the chat's title (done `●` receding, the one in hand `›` in
//!   the accent, the rest `◦`), one row in the minimized chat while Claude works, kept and continued with
//!   the conversation (`chats.rs`).

use serde_json::{Value, json};

use crate::editor::Editor;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Todo,
    Doing,
    Done,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub text: String,
    pub status: Status,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub title: String,
    pub steps: Vec<Step>,
}

impl Plan {
    pub fn done(&self) -> usize {
        self.steps.iter().filter(|s| s.status == Status::Done).count()
    }

    /// The step in hand — the first `doing`, else the first not done.
    pub fn current(&self) -> Option<usize> {
        self.steps
            .iter()
            .position(|s| s.status == Status::Doing)
            .or_else(|| self.steps.iter().position(|s| s.status != Status::Done))
    }

    pub fn finished(&self) -> bool {
        !self.steps.is_empty() && self.done() == self.steps.len()
    }

    /// Which steps `max` rows show: (hidden before, the range shown, hidden after) — around the step in
    /// hand, one finished step before it for context.
    pub fn window(&self, max: usize) -> (usize, std::ops::Range<usize>, usize) {
        let n = self.steps.len();
        if n <= max {
            return (0, 0..n, 0);
        }
        // (a row each for the hidden counts)
        let room = max.saturating_sub(2).max(1);
        let at = self.current().unwrap_or(n);
        let start = at.saturating_sub(1).min(n.saturating_sub(room));
        let end = (start + room).min(n);
        (start, start..end, n - end)
    }

    pub fn to_json(&self) -> Value {
        let steps: Vec<Value> =
            self.steps.iter().map(|s| json!({ "text": s.text, "status": status_name(s.status) })).collect();
        json!({ "title": self.title, "steps": steps })
    }

    pub fn from_json(v: &Value) -> Option<Plan> {
        let steps = v["steps"]
            .as_array()?
            .iter()
            .filter_map(|s| {
                let text = s["text"].as_str()?.trim();
                let status = match s["status"].as_str().unwrap_or("todo") {
                    "doing" | "in_progress" => Status::Doing,
                    "done" | "completed" => Status::Done,
                    _ => Status::Todo,
                };
                (!text.is_empty()).then(|| Step { text: text.to_string(), status })
            })
            .collect::<Vec<_>>();
        (!steps.is_empty())
            .then(|| Plan { title: v["title"].as_str().unwrap_or_default().trim().to_string(), steps })
    }
}

fn status_name(s: Status) -> &'static str {
    match s {
        Status::Todo => "todo",
        Status::Doing => "doing",
        Status::Done => "done",
    }
}

/// The `plan` tool as the MCP server lists it.
pub fn tool_schema() -> Value {
    json!({
        "name": "plan",
        "description": "Show the user your plan in their editor, pinned above the chat. When the work takes \
    several steps, lay them out first — a handful of short steps, in order — then keep it current as you go: \
    mark the step you're on `doing` and finished ones `done`. Each call sends the whole list (it replaces the \
    last). Skip it for one-step answers.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "title": { "type": "string", "description": "What the plan is for, a few words" },
                "steps": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "text": { "type": "string", "description": "The step, a short line" },
                            "status": { "type": "string", "enum": ["todo", "doing", "done"] }
                        },
                        "required": ["text", "status"]
                    }
                }
            },
            "required": ["steps"]
        }
    })
}

/// `plan` called by Claude.
pub fn tool_call(ed: &mut Editor, args: &Value) -> Result<String, String> {
    let plan = Plan::from_json(args).ok_or("steps are required — at least one, each with text and status")?;
    let c = ed.lane.chat.as_mut().ok_or("the chat is closed")?;
    let reply = format!("plan shown ({}/{} done)", plan.done(), plan.steps.len());
    c.plan = Some(plan);
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(statuses: &str) -> Plan {
        let steps = statuses
            .chars()
            .enumerate()
            .map(|(i, c)| Step {
                text: format!("step {i}"),
                status: match c {
                    'x' => Status::Done,
                    '>' => Status::Doing,
                    _ => Status::Todo,
                },
            })
            .collect();
        Plan { title: "t".into(), steps }
    }

    #[test]
    fn the_window_follows_the_step_in_hand() {
        assert_eq!(plan("x>..").window(6), (0, 0..4, 0), "all fit");
        // Ten steps, six rows: two for the counts, four steps from one done before the current
        assert_eq!(plan("xxxxx>....").window(6), (4, 4..8, 2));
        assert_eq!(plan(">.........").window(6), (0, 0..4, 6));
        assert_eq!(plan("xxxxxxxxx>").window(6), (6, 6..10, 0));
        assert_eq!(plan("xxx.......").current(), Some(3), "no doing: the first not done");
        assert!(plan("xxx").finished() && !plan("xx>").finished());
        // Round trip, and claude's todo words understood
        let p = plan("x>.");
        assert_eq!(Plan::from_json(&p.to_json()), Some(p));
        let v = json!({"steps": [{"text": "a", "status": "completed"}, {"text": "b", "status": "in_progress"}, {"text": " "}]});
        let p = Plan::from_json(&v).unwrap();
        assert_eq!(p.steps.iter().map(|s| s.status).collect::<Vec<_>>(), [Status::Done, Status::Doing]);
    }
}
