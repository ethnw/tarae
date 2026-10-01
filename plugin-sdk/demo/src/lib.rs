//! A tarae plugin with a command for every action, and some that misbehave on purpose.
wit_bindgen::generate!({ path: "../wit", world: "plugin" });

use tarae::plugin::types::{Connect, Edit, Hint, Hints, Panel, Pick, Side, SideRow, Spawn, Tone, View};

struct Demo;

fn cmd(name: &str, doc: &str, text: bool) -> Command {
    Command { name: name.into(), doc: doc.into(), text }
}

impl Guest for Demo {
    fn commands() -> Vec<Command> {
        vec![
            cmd("shout", "Upper-case the selections", false),
            cmd("trim-trailing", "Remove spaces at line ends", true),
            cmd("notify", "One message of each kind", false),
            cmd("grab", "Select every occurrence of the argument", true),
            cmd("send", "Run the argument as a tarae command", false),
            cmd("visit", "Open the argument", false),
            cmd("echo", "Show what the plugin was given", false),
            cmd("spin", "Never finish", false),
            cmd("boom", "Panic", false),
            cmd("deep", "Recurse until something gives", false),
            cmd("stray", "Edit outside the buffer", false),
            cmd("requests", "Every kind of request, and a panel", false),
            cmd("exec", "Run `true`", false),
            cmd("exec-with", "Run `true` with NAME=value set", false),
            cmd("hint", "Hints beside lines 1 and 2", false),
            cmd("side", "Its tab in the side panel, and a badge", false),
            cmd("find", "The project's files the argument matches", false),
            cmd("keep", "Writes notes/a.txt in its own folder", false),
            cmd("install", "Offers to install the argument", false),
            cmd("unside", "No tab, no badge", false),
            cmd("choose", "A pick of two", false),
            cmd("view-and-panel", "Its buffer, then its panel, in one answer", false),
            cmd("fetch", "Read the file in the argument", false),
            cmd("getenv", "Read the environment variable in the argument", false),
            cmd("dial", "Connect to 127.0.0.1 at the port in the argument, and say ping", false),
            cmd("hang-up", "Close that connection", false),
            cmd("later", "A tick after the milliseconds in the argument", false),
            cmd("nag", "Asks for a line on its own, a moment later (refused)", false),
            cmd("quiet", "A tick a moment later, answered with I/O only", false),
            cmd("never", "Stop that tick", false),
            cmd("table", "A panel of 30 pods", false),
            cmd("question", "A line to type at", false),
            cmd("view", "A buffer of its own", false),
            cmd("more", "Add to that buffer", false),
        ]
    }

    fn run(name: String, cx: Context) -> Vec<Action> {
        match name.as_str() {
            "shout" => {
                vec![Action::ReplaceSelections(cx.selections.iter().map(|s| s.text.to_uppercase()).collect())]
            }
            "trim-trailing" => {
                let Some(text) = cx.buffer.text else { return vec![Action::Error("no text".into())] };
                let edits = trim(&text);
                vec![Action::Success(format!("{} lines trimmed", edits.len())), Action::Edits(edits)]
            }
            "notify" => vec![
                Action::Status(format!("status {}", cx.args)),
                Action::Success(format!("success {}", cx.args)),
                Action::Warning(format!("warning {}", cx.args)),
                Action::Error(format!("error {}", cx.args)),
            ],
            "grab" => {
                let text = cx.buffer.text.unwrap_or_default();
                if cx.args.is_empty() {
                    return vec![Action::Error("grab what?".into())];
                }
                let found: Vec<(u64, u64)> =
                    text.match_indices(&cx.args).map(|(i, m)| (i as u64, (i + m.len()) as u64)).collect();
                vec![Action::Select(found)]
            }
            "send" => vec![Action::Run(cx.args)],
            "exec" => vec![Action::Spawn(Spawn { id: 1, program: "true".into(), args: vec![], env: vec![] })],
            // `NAME=value`: a variable set for the program
            "exec-with" => {
                let (k, v) = cx.args.split_once('=').unwrap_or((&cx.args, ""));
                vec![Action::Spawn(Spawn { id: 1, program: "true".into(), args: vec![], env: vec![(k.into(), v.into())] })]
            }
            "fetch" => vec![Action::ReadFile((10, cx.args))],
            "getenv" => vec![Action::ReadEnv((11, cx.args))],
            "dial" => vec![Action::Connect(Connect {
                id: 20,
                host: "127.0.0.1".into(),
                port: cx.args.parse().unwrap_or(0),
            })],
            "hang-up" => vec![Action::Stop(20)],
            "later" => vec![Action::Timer((30, cx.args.parse().unwrap_or(0)))],
            "nag" => vec![Action::Timer((31, 10))],
            // A tick it answers with I/O only (says nothing)
            "quiet" => vec![Action::Timer((32, 10))],
            "hint" => vec![Action::Hints(Hints {
                path: cx.buffer.path.clone().unwrap_or_default(),
                version: cx.buffer.version,
                hints: vec![
                    Hint { line: 0, text: "all good".into(), tone: Tone::Ok },
                    Hint { line: 1, text: "2/3 ready".into(), tone: Tone::Error },
                ],
            })],
            "side" => {
                let path = cx.buffer.path.clone().unwrap_or_default();
                let row = |id: &str, depth, text: &str, tone, at: Option<(u64, u64)>| SideRow {
                    id: id.into(),
                    depth,
                    text: text.into(),
                    detail: format!("{id} detail"),
                    tone,
                    at: at.map(|(a, b)| (path.clone(), a, b)),
                    note: false,
                };
                vec![
                    Action::Side(Some(Side {
                        title: "demo".into(),
                        note: "⎈ test".into(),
                        rows: vec![
                            row("file", 0, "web.txt", Tone::Warning, Some((0, 1))),
                            row("deploy", 1, "payment-gateway-reconciler-canary", Tone::Ok, Some((0, 0))),
                            row("pod", 1, "web-1", Tone::Error, None),
                            SideRow { note: true, ..row("why", 2, "CrashLoopBackOff", Tone::Error, None) },
                        ],
                        keys: vec![("e".into(), "events".into())],
                        open: true,
                        focus: cx.args == "focus",
                    })),
                    Action::Badge(Some(("⎈ test".into(), Tone::Warning))),
                ]
            }
            "unside" => vec![Action::Side(None), Action::Badge(None)],
            "find" => vec![Action::Find((50, cx.args))],
            "keep" => vec![Action::WriteOwn((51, "notes/a.txt".into(), b"hello".to_vec()))],
            "install" => vec![Action::OfferInstall((52, cx.args))],
            "view-and-panel" => vec![
                Action::Show(View { id: 7, title: "web-1".into(), text: "kind: Pod\n".into(), language: None }),
                Action::Panel(Some(Panel {
                    title: "pods".into(),
                    columns: vec![],
                    rows: vec![vec!["web-1".into()]],
                    ids: vec![],
                    status: String::new(),
                    keys: vec![],
                })),
            ],
            "choose" => vec![Action::Pick(Pick {
                id: 41,
                title: "choose".into(),
                items: vec![("x".into(), "ex".into()), ("y".into(), "why".into())],
            })],
            "never" => vec![Action::Stop(30)],
            "table" => vec![Action::Panel(Some(Panel {
                title: "pods · default".into(),
                columns: vec!["NAME".into(), "READY".into(), "STATUS".into()],
                rows: (0..30).map(|i| vec![format!("web-{i}"), "1/1".into(), "Running".into()]).collect(),
                ids: (0..30).map(|i| format!("default/web-{i}")).collect(),
                status: "30 pods · watching".into(),
                keys: vec![("d".into(), "describe".into()), ("l".into(), "logs".into())],
            }))],
            "question" => vec![Action::Prompt((6, "filter".into()))],
            "view" => vec![Action::Show(View {
                id: 7,
                title: "web-1".into(),
                text: "kind: Pod\n".into(),
                language: Some("yaml".into()),
            })],
            "more" => vec![Action::Append((7, "status: Running\n".into()))],
            "requests" => vec![
                Action::Spawn(Spawn {
                    id: 1,
                    program: "true".into(),
                    args: vec!["a b".into()],
                    env: vec![("TARAE_DEMO".into(), "1".into())],
                }),
                Action::Connect(Connect { id: 2, host: "127.0.0.1".into(), port: 6443 }),
                Action::Send((2, vec![0, 1, 255])),
                Action::Stop(1),
                Action::ReadFile((3, "~/.kube/config".into())),
                Action::ReadEnv((4, "KUBECONFIG".into())),
                Action::Timer((5, 1000)),
                Action::Panel(Some(Panel {
                    title: "pods".into(),
                    columns: vec!["NAME".into()],
                    rows: vec![vec!["web-1".into()], vec!["web-2".into()]],
                    ids: vec![],
                    status: "2 pods".into(),
                    keys: vec![("d".into(), "describe".into())],
                })),
                Action::Prompt((6, "filter".into())),
                Action::Show(View { id: 7, title: "web-1".into(), text: "kind: Pod".into(), language: Some("yaml".into()) }),
                Action::Append((7, "\nmore".into())),
                Action::Panel(None),
                Action::Status(format!("now {} · {} random bytes", cx.now.millis, cx.now.entropy.len())),
            ],
            "visit" => vec![Action::Open(cx.args)],
            "echo" => {
                let b = &cx.buffer;
                let sels: Vec<String> = cx
                    .selections
                    .iter()
                    .map(|s| format!("{}..{}@{}:{:?}", s.from, s.to, s.line, s.text))
                    .collect();
                vec![Action::Status(format!(
                    "args={:?} path={:?} name={} language={:?} text={} version={} modified={} primary={} sels={}",
                    cx.args,
                    b.path,
                    b.name,
                    b.language,
                    b.text.is_some(),
                    b.version,
                    b.modified,
                    cx.primary,
                    sels.join(",")
                ))]
            }
            "spin" => loop {
                std::hint::black_box(());
            },
            "boom" => panic!("boom"),
            "deep" => vec![Action::Status(format!("{}", deep(std::hint::black_box(1))))],
            "stray" => vec![Action::Edits(vec![Edit { from: 1 << 40, to: 1 << 40, insert: "x".into() }])],
            _ => vec![Action::Error(format!("unknown command {name}"))],
        }
    }

    fn subscriptions() -> Vec<Subscription> {
        let sub = |event, text| Subscription { event, text, languages: vec![] };
        vec![
            sub(Event::Opened, false),
            sub(Event::Saved, false),
            sub(Event::Closed, false),
            sub(Event::Changed, true),
            sub(Event::BeforeSave, true),
            sub(Event::Shown, false),
            // (and the text of Markdown ones: an event subscribed twice, for different languages)
            Subscription { event: Event::Shown, text: true, languages: vec!["markdown".into()] },
        ]
    }

    /// What it may do beyond the buffer: a little of each, for the tests.
    fn permissions() -> Vec<Permission> {
        vec![
            Permission::Run("true".into()),
            Permission::Read("/tmp/tarae-demo/**".into()),
            Permission::Env("TARAE_DEMO".into()),
            Permission::Network,
            Permission::Project,
        ]
    }

    /// What came back from what it asked for (programs, connections, files, timers, its panel's keys) —
    /// it asks for none.
    fn on_events(events: Vec<HostEvent>, now: Now) -> Vec<Action> {
        if events.iter().all(|e| matches!(e, HostEvent::Tick(32))) {
            return vec![Action::Stop(99)];
        }
        // Tells what it got, so tests can see each kind arrive whole
        let each: Vec<String> = events
            .iter()
            .map(|e| match e {
                HostEvent::Lines((id, l)) => format!("lines {id} {l:?}"),
                HostEvent::Exit((id, code, err)) => format!("exit {id} {code:?} {err:?}"),
                HostEvent::Connected(id) => format!("connected {id}"),
                HostEvent::Data((id, b)) => format!("data {id} {b:?}"),
                HostEvent::Closed((id, why)) => format!("closed {id} {why:?}"),
                HostEvent::File((id, b)) => format!("file {id} {}", b.len()),
                HostEvent::Env((id, v)) => format!("env {id} {v:?}"),
                HostEvent::Tick(id) => format!("tick {id}"),
                HostEvent::Key((k, row, id)) => format!("key {k} {row:?} {id:?}"),
                HostEvent::Answer((id, a)) => format!("answer {id} {a:?}"),
                HostEvent::Failed((id, why)) => format!("failed {id} {why}"),
                HostEvent::Found((id, paths)) => {
                    let names: Vec<&str> = paths.iter().map(|p| p.rsplit('/').next().unwrap_or(p)).collect();
                    format!("found {id} {names:?}")
                }
                HostEvent::Written((id, path)) => format!("written {id} {path}"),
                HostEvent::Installed(id) => format!("installed {id}"),
            })
            .collect();
        let mut out = vec![Action::Status(format!("{} · {} · {}", each.join(" | "), now.millis, now.entropy.len()))];
        // Connected: says ping
        if events.iter().any(|e| matches!(e, HostEvent::Connected(20))) {
            out.push(Action::Send((20, b"ping".to_vec())));
        }
        // `/` in its panel (an answer to you), or on its own (`nag`): asks for a line
        if events.iter().any(|e| matches!(e, HostEvent::Tick(31)) || matches!(e, HostEvent::Key((k, ..)) if k == "/")) {
            out.push(Action::Prompt((6, "filter".into())));
        }
        out
    }

    fn on(event: Event, cx: Context) -> Vec<Action> {
        let name = cx.buffer.name;
        match event {
            Event::Opened => vec![Action::Status(format!("opened {name}"))],
            Event::Saved => vec![Action::StatusBar(format!("saved v{}", cx.buffer.version))],
            Event::Closed => vec![Action::Status(format!("closed {name}"))],
            // `.loop` files get a `!` on each change — which, reported back, would never stop
            Event::Changed if name.ends_with(".loop") => {
                let end = cx.buffer.text.unwrap_or_default().len() as u64;
                vec![Action::Edits(vec![Edit { from: end, to: end, insert: "!".into() }])]
            }
            // A word count, quietly
            Event::Changed => {
                let words = cx.buffer.text.unwrap_or_default().split_whitespace().count();
                vec![Action::StatusBar(format!("{words} words"))]
            }
            // `.trim` files lose their trailing spaces on save; `.slow` ones make saving wait too long
            Event::BeforeSave if name.ends_with(".trim") => {
                vec![Action::Edits(trim(&cx.buffer.text.unwrap_or_default()))]
            }
            Event::BeforeSave if name.ends_with(".slow") => loop {
                std::hint::black_box(());
            },
            Event::BeforeSave => Vec::new(),
            // In front again: said for `.front` files (others' tests keep their messages)
            // (with the time: each its own message, so a test can count them)
            Event::Shown if name.contains(".text.") => {
                vec![Action::Status(format!("shown with text: {} {}", cx.buffer.text.is_some(), cx.now.millis))]
            }
            Event::Shown if name.ends_with(".front") => {
                vec![Action::Status(format!("shown {name} {}", cx.now.millis))]
            }
            Event::Shown => Vec::new(),
        }
    }
}

/// Edits removing the spaces at the ends of lines.
fn trim(text: &str) -> Vec<Edit> {
    let mut edits = Vec::new();
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line);
        let kept = body.trim_end_matches(' ').len();
        if kept < body.len() {
            edits.push(Edit {
                from: (at + kept) as u64,
                to: (at + body.len()) as u64,
                insert: String::new(),
            });
        }
        at += line.len();
    }
    edits
}

fn deep(n: u64) -> u64 {
    if n == 0 { 0 } else { deep(std::hint::black_box(n + 1)) + 1 }
}

export!(Demo);
