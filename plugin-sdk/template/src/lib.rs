//! A tarae plugin. `tarae plugin dev` builds it on every save, and your running tarae reloads it.
//! What a plugin can see and do: `wit/tarae.wit`.
wit_bindgen::generate!({ path: "wit", world: "plugin" });

struct Plugin;

impl Guest for Plugin {
    /// Called once, when tarae loads the plugin.
    fn commands() -> Vec<Command> {
        vec![
            Command { name: "words".into(), doc: "Count the words in the selections".into(), text: false },
            Command { name: "title".into(), doc: "Title Case the selections".into(), text: false },
        ]
    }

    /// Called for `:words` and `:title` (and keys or palette entries that run them).
    fn run(name: String, cx: Context) -> Vec<Action> {
        match name.as_str() {
            "words" => {
                let n: usize = cx.selections.iter().map(|s| s.text.split_whitespace().count()).sum();
                vec![Action::Status(format!("{n} words"))]
            }
            "title" => vec![Action::ReplaceSelections(cx.selections.iter().map(|s| title_case(&s.text)).collect())],
            _ => vec![Action::Error(format!("unknown command {name}"))],
        }
    }

    /// Called when the plugin loads: the events it wants to hear about.
    fn subscriptions() -> Vec<Subscription> {
        // (`languages`: only buffers in these — none, every buffer)
        vec![Subscription { event: Event::Changed, text: true, languages: Vec::new() }]
    }

    /// An event: here, a buffer was edited and then left alone for a moment.
    /// What it may do beyond the buffer (run programs, read files, connect) — nothing, like most plugins.
    fn permissions() -> Vec<Permission> {
        Vec::new()
    }

    /// What came back from what it asked for (programs, connections, files, timers, its panel's keys) —
    /// it asks for none.
    fn on_events(_events: Vec<HostEvent>, _now: Now) -> Vec<Action> {
        Vec::new()
    }

    fn on(event: Event, cx: Context) -> Vec<Action> {
        match event {
            Event::Changed => {
                let words = cx.buffer.text.unwrap_or_default().split_whitespace().count();
                vec![Action::StatusBar(format!("{words} words"))]
            }
            _ => Vec::new(),
        }
    }
}

fn title_case(s: &str) -> String {
    let mut start = true;
    s.chars()
        .flat_map(|c| {
            let up = start;
            start = c.is_whitespace();
            let out: Vec<char> = if up { c.to_uppercase().collect() } else { c.to_lowercase().collect() };
            out
        })
        .collect()
}

export!(Plugin);
