//! `cargo xtask scenes --docs [picture …]` — the pictures the docs show (`docs/screenshots/`), each retaken from the
//! scene its row in `DOCS` names, at its size, theme and scale, into `target/docshots/` (`--out DIR`), then
//! compared with the committed ones pixel for pixel (`scenes --compare`; one that can't be read is said and the
//! run goes on). A row's old picture in `out` goes before its shot, and the run writes each row's result last, in
//! `<out>/manifest.tsv`. `--keep` copies only what that manifest says was taken and found different (or new) —
//! once you've looked at them; it takes nothing itself, and refuses with no manifest or with a picture written
//! after it. `--list` prints the table. A row with no scene yet is named apart; one whose `needs` aren't on
//! `PATH` is skipped and named.
//!
//! A docs shot differs from a scenes one four ways: the `shot-docs` profile's tarae (the shot profile with every
//! bundled grammar); homes of its own — `HOME` (its projects are in it: `~/tarae-calc`, never your paths),
//! `XDG_DATA_HOME` (grammars, packs, sessions' checkouts), `XDG_CACHE_HOME` (runs' folders and homes), all empty
//! but what the scene puts there: nothing downloaded, cached or kept on this machine shows (`homes`); a folder of a
//! fixed name, `<temp>/tarae-docs/<scene>`, so the paths an output panel prints stay put between retakes — there
//! already, the shot fails rather than clear it; and its scene may be one of the docs' own (`DOC_SCENES`: a project
//! of `fixtures.rs`, a seeded state, settings of its own) as well as one of `scenes.rs`'s. `--both` also takes each
//! picture in the other theme (`<name>.hanji.png` / `.meok.png`, to look at; never in the manifest, never kept).
//!
//! A new picture in the docs = a row here (the tests below: every picture a page references has a row and every
//! row's picture is referenced, every PNG in `docs/screenshots/` has a row, every row's scene exists).

use std::ffi::CString;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use serde_json::json;

use super::{Compared, SCENES, compared_but, config_toml, write};
use crate::shot::{self, Setup};
use crate::{Result, root};

// (the docs' own projects and seeded state)
mod fixtures;
use fixtures::{Fixture, Seed};

const USAGE: &str = "usage: cargo xtask scenes --docs [--out DIR] [--both] [picture …] | --docs --keep [picture …] | --docs --list";

/// The theme a picture is in: meok on a dark ground, hanji on a light one.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Bg {
    Dark,
    Light,
}

impl Bg {
    fn word(self) -> &'static str {
        match self {
            Bg::Dark => "dark",
            Bg::Light => "light",
        }
    }

    fn other(self) -> Bg {
        match self {
            Bg::Dark => Bg::Light,
            Bg::Light => Bg::Dark,
        }
    }

    /// The theme tarae picks on it.
    fn theme(self) -> &'static str {
        match self {
            Bg::Dark => "meok",
            Bg::Light => "hanji",
        }
    }
}

/// `--both`'s file for `picture` in the other theme than its row's `bg`: `chat-run.png` → `chat-run.hanji.png`.
fn twin(picture: &str, bg: Bg) -> String {
    format!("{}.{}.png", picture.strip_suffix(".png").unwrap_or(picture), bg.other().theme())
}

/// One picture the docs show: its file under `docs/screenshots/`, the scene that makes it (None: none yet), its
/// size in cells, theme and scale, the programs it runs for real (skipped where they aren't on `PATH`), and the
/// cells that may differ between takes, left out when it's compared (`varies`).
#[derive(Debug)]
struct DocShot {
    picture: &'static str,
    scene: Option<&'static str>,
    size: (u16, u16),
    bg: Bg,
    scale: usize,
    needs: &'static [&'static str],
    varies: &'static [Varies],
}

/// Cells on one row of a picture whose text honestly differs from take to take, and what they hold: checked at
/// every take (`holds`), so a layout that moved them fails the shot rather than hide what's there now.
#[derive(Debug)]
struct Varies {
    /// Row, first column, the column after the last — from the picture's top left, in cells.
    row: u16,
    cols: (u16, u16),
    what: &'static str,
    holds: fn(&str) -> bool,
}

/// A turn's time as the chat says it, right-aligned in `cols` of `row`: `2.0s` — tarae's clock, a tenth either way.
const fn turn_time(row: u16, cols: (u16, u16)) -> Varies {
    Varies { row, cols, what: "a turn's time (`2.0s`)", holds: seconds }
}

/// `2.0s`, `12.3s`: seconds to a tenth.
fn seconds(t: &str) -> bool {
    t.strip_suffix('s').and_then(|n| n.split_once('.')).is_some_and(|(a, b)| {
        !a.is_empty()
            && a.bytes().all(|c| c.is_ascii_digit())
            && b.len() == 1
            && b.bytes().all(|c| c.is_ascii_digit())
    })
}

/// `cargo xtask shot`'s default size, most pictures'.
const S: (u16, u16) = (112, 30);

const fn row(picture: &'static str, scene: Option<&'static str>, size: (u16, u16)) -> DocShot {
    DocShot { picture, scene, size, bg: Bg::Dark, scale: 1, needs: &[], varies: &[] }
}

impl DocShot {
    const fn light(self) -> DocShot {
        DocShot { bg: Bg::Light, ..self }
    }
    const fn at(self, scale: usize) -> DocShot {
        DocShot { scale, ..self }
    }
    const fn needs(self, needs: &'static [&'static str]) -> DocShot {
        DocShot { needs, ..self }
    }
    /// What tarae shows there honestly differs from take to take (a turn's time): the comparison passes over it,
    /// so a retake isn't "differs" for it alone — and `--keep` doesn't copy it each time.
    const fn varies(self, varies: &'static [Varies]) -> DocShot {
        DocShot { varies, ..self }
    }

    /// Each `varies` holds what it says on the screen just taken (`screen`: its cells' text, row by row) — else
    /// the layout moved, and its mask would hide something else.
    fn varies_hold(&self, screen: &[Vec<String>]) -> Result<()> {
        for v in self.varies {
            let cells = screen.get(usize::from(v.row)).map(|r| {
                let (a, b) = (usize::from(v.cols.0).min(r.len()), usize::from(v.cols.1).min(r.len()));
                r[a..b].concat()
            });
            let text = cells.unwrap_or_default();
            if !(v.holds)(text.trim()) {
                return Err(format!(
                    "row {} cols {}..{} holds {:?}, not {} — the layout moved: its `varies` would hide what's there",
                    v.row,
                    v.cols.0,
                    v.cols.1,
                    text.trim(),
                    v.what
                ));
            }
        }
        Ok(())
    }

    /// Whether pixel (`x`, `y`) of a `w`×`h` picture of this row lies in one of its `varies` cells: a cell's size
    /// from the picture's own and the row's size and scale, as `shot` lays them out (its `PAD` around them).
    fn varying(&self, x: usize, y: usize, w: usize, h: usize) -> bool {
        let ((cols, rows), pad) = (self.size, shot::PAD * self.scale);
        let cw = w.saturating_sub(2 * pad) / usize::from(cols.max(1));
        let ch = h.saturating_sub(2 * pad) / usize::from(rows.max(1));
        self.varies.iter().any(|v| {
            let (c0, c1, row) = (usize::from(v.cols.0), usize::from(v.cols.1), usize::from(v.row));
            let (x0, x1, y0) = (pad + c0 * cw, pad + c1 * cw, pad + row * ch);
            (x0..x1).contains(&x) && (y0..y0 + ch).contains(&y)
        })
    }
}

/// A scene the docs alone need: its project, the stand-in claude's script (`scenes::claude`, or the docs' own —
/// `answered`), the files tarae opens and the keys sent (as `scenes::Scene`'s), what tarae's state holds at its
/// start, and settings added to the scenes' config (`config_toml`).
#[derive(Debug, Clone, Copy)]
struct DocScene {
    name: &'static str,
    fixture: Fixture,
    claude: &'static str,
    files: &'static [&'static str],
    keys: &'static [&'static str],
    seed: Seed,
    config: &'static str,
}

/// A scene with nothing seeded and no settings of its own.
const fn scene(
    name: &'static str,
    fixture: Fixture,
    files: &'static [&'static str],
    keys: &'static [&'static str],
) -> DocScene {
    DocScene { name, fixture, claude: "idle", files, keys, seed: Seed::Nothing, config: "" }
}

impl DocScene {
    const fn claude(self, claude: &'static str) -> DocScene {
        DocScene { claude, ..self }
    }
    const fn seed(self, seed: Seed) -> DocScene {
        DocScene { seed, ..self }
    }
}

const LIB: &[&str] = &["src/lib.rs"];
const MAIN: &[&str] = &["src/main.rs"];

/// The docs' own scenes (each picture's row names one, or one of `scenes.rs`'s).
const DOC_SCENES: &[DocScene] = &[
    // (the start screen: files opened lately in two projects)
    scene("welcome", Fixture::Demo, &[], &["@?Open a file", "@1"]).seed(Seed::Recent),
    // (…and in a folder with files open last time: `r` first)
    scene("continue", Fixture::Calc, &[], &["@?Continue where", "@1"]).seed(Seed::LastOpen),
    // (the gutter on a changed file, the cursor on its added function: `+a −r` on the status line)
    scene("gutter", Fixture::Calc, LIB, &["@?main", "@1", "]", "g", "]", "g", ";", "@1"]),
    // (disk sync in toasts: a save; a line added by another program — the clean buffer reloads; then a line typed
    // here and another added outside — the conflict, and the path bar's `▲ changed on disk`)
    scene(
        "toastsync",
        Fixture::Demo,
        MAIN,
        &[
            "@1",
            ":",
            "w",
            "\r",
            "@1",
            ":",
            "sh echo '// added by another program' >> src/main.rs",
            "\r",
            "@20?reloaded",
            "O",
            "// typed here, not saved",
            "\x1b",
            "@1",
            ":",
            "sh echo '// and another, while you typed' >> src/main.rs",
            "\r",
            "@20?changed on disk",
            "@1",
        ],
    ),
    // (`space i` on a line, answered whole: its review in place)
    scene(
        "askreview",
        Fixture::Demo,
        MAIN,
        &[
            "@1",
            "10",
            "G",
            "x",
            " ",
            "i",
            "@1",
            "keep the first strand",
            "\r",
            "@20?Change 1 of 1",
            "@10!claude answered",
            "@1",
        ],
    )
    .claude("askdone"),
    scene("palette", Fixture::Calc, LIB, &["@1", " ", "?", "@1", "chat", "@1"]),
    scene("whichkey", Fixture::Calc, LIB, &["@1", " ", "@2"]),
    scene("tutor", Fixture::Calc, LIB, &["@1", ":", "tutor", "\r", "@1"]),
    // (the next theme previewed live on the code)
    scene("themes", Fixture::Calc, LIB, &["@1", " ", "t", "@1", "\x0e", "@1"]),
    scene("search", Fixture::Calc, LIB, &["@1", "/", "i32", "\r", "n", "n", "@1"]),
    // (side by side, then the right one stacked: three files, the left focused)
    scene(
        "splits",
        Fixture::Calc,
        LIB,
        &[
            "@1",
            "\x17",
            "v",
            "@1",
            ":",
            "open src/stats.rs",
            "\r",
            "@1",
            "\x17",
            "s",
            "@1",
            ":",
            "open README.md",
            "\r",
            "@1",
            "\x17",
            "h",
            "@1",
        ],
    ),
    scene("cmdline", Fixture::Calc, LIB, &["@1", ":", "wr", "@1"]),
    scene("outline", Fixture::Calc, LIB, &["@1", " ", "o", "@1"]),
    scene("doccomments", Fixture::Calc, LIB, &["@1"]),
    // (insert mode in `add`'s doc: that block as written)
    scene("doccommentsopen", Fixture::Calc, LIB, &["@1", "7", "G", "i", "@1"]),
    scene("injections", Fixture::Calc, &["README.md"], &["@1"]),
    // (the chat beside the code: a question and Claude's answer — inline code, a list, a code block)
    scene(
        "chatanswer",
        Fixture::Demo,
        MAIN,
        &["@1", " ", "l", "@1", "why does insert clone the name?", "\r", "@20?Nothing else reads", "@2"],
    )
    .claude("answer"),
    // (`space f` with a few letters typed: the files that have them, across folders, the first previewed)
    scene("filepicker", Fixture::Calc, LIB, &["@1", " ", "f", "@?README", "@1", "stat", "@1"]),
    // (pointing from the chat, the git tab open and a card up: every element with an id labelled)
    scene(
        "point",
        Fixture::Trusted,
        MAIN,
        &["@?Trust this", "@1", " ", "g", "@?knot.rs", " ", "l", "@1", "\x0f", "@1"],
    ),
    // (`C-t` in the chat: the models the stand-in's `initialize` answer offers)
    scene("model", Fixture::Calc, LIB, &["@1", " ", "l", "@1", "\x14", "@1"]).claude("models"),
    // (`:chat-resume`: the project's earlier conversations, the newest previewed — waited on by its title, so a
    // seed `chats::file` no longer finds fails the shot rather than show an empty list)
    scene(
        "conversations",
        Fixture::Calc,
        LIB,
        &["@1", ":", "chat-resume", "\r", "@10?why does adds() fail", "@1"],
    )
    .seed(Seed::Chats),
    // (your code review: the first change in hand, a comment being written at it)
    scene(
        "codereview",
        Fixture::Calc,
        LIB,
        &[
            "@1",
            " ",
            "v",
            "v",
            "@?viewed",
            "@1",
            "]",
            "g",
            "@1",
            " ",
            "n",
            "@1",
            "Should min say which one it returns when they're equal, as max now does?",
            "\r",
            "@1",
        ],
    ),
    // (Claude asks to run outside the sandbox: its card, the keys back from the chat)
    scene(
        "runcard",
        Fixture::Demo,
        MAIN,
        &["@1", " ", "l", "@1", "run the tests", "\r", "@?Its reason", "\x1b", "@1"],
    )
    .claude("card"),
    // (a review whose changes are graded high: an `unsafe` line, a new build script)
    scene("risk", Fixture::Words, MAIN, &["@1", " ", "v", "v", "@?high", "@1"]),
    // (…and its end held while they're not looked at — asked once the review's toast has gone: caught fading, the
    // picture came out different at each take)
    scene(
        "riskend",
        Fixture::Words,
        MAIN,
        &["@1", " ", "v", "v", "@?high", "@20!3 files changed", "@1", " ", "v", "q", "@?End", "@1"],
    ),
];

/// The scene `name`: the docs' own, else `scenes.rs`'s on its demo crate (`trusted` with its settings).
fn scene_named(name: &str) -> Option<DocScene> {
    if let Some(s) = DOC_SCENES.iter().find(|s| s.name == name) {
        return Some(*s);
    }
    let s = SCENES.iter().find(|s| s.name == name)?;
    let fixture = if s.name == "trusted" { Fixture::Trusted } else { Fixture::Demo };
    Some(DocScene { claude: s.claude, ..scene(s.name, fixture, s.files, s.keys) })
}

/// The docs' own stand-in scripts, ahead of `scenes::claude`'s: `models` answers `initialize` with the models a
/// `claude` offers (and no message, as `idle`); `askdone` answers `space i` whole — the line it was given, keyed
/// by the entry API, and the answer's end; `answer` answers the chat's question in Markdown (inline code, a list,
/// a code block) on a model the conversation names, a line at a time, three `pause`s after each (as
/// `scenes::claude`'s turns pause). Whether it answered `line` (tarae's, one).
pub(super) fn answered(
    script: &str,
    line: &str,
    out: &mut impl Write,
    pause: std::time::Duration,
) -> io::Result<bool> {
    let say = |out: &mut dyn Write, v: serde_json::Value| -> io::Result<()> {
        writeln!(out, "{v}")?;
        out.flush()
    };
    match script {
        "models" | "answer" if line.contains("\"tarae-init\"") => {
            let model = |value: &str, name: &str, about: &str, resolved: &str| {
                json!({"value": value, "displayName": name, "description": about, "resolvedModel": resolved,
                    "supportedEffortLevels": ["low", "medium", "high"]})
            };
            let models = [
                model(
                    "default",
                    "Default (recommended)",
                    "Opus 5.5 · Best for everyday, complex tasks",
                    "claude-opus-5-5",
                ),
                model("opus", "Opus 5.5", "For complex work and everyday tasks", "claude-opus-5-5"),
                model("claude-fable-5-1", "Fable 5.1", "For your toughest challenges", "claude-fable-5-1"),
                model("sonnet", "Sonnet 5.5", "Efficient for routine tasks", "claude-sonnet-5-5"),
                model("haiku", "Haiku 5.5", "Fastest for quick answers", "claude-haiku-5-5"),
            ];
            let answer = json!({"models": models, "account": {"apiKeySource": "none"}});
            let response = json!({"subtype": "success", "request_id": "tarae-init", "response": answer});
            say(out, json!({"type": "control_response", "response": response}))?;
            Ok(true)
        }
        "askdone" if line.starts_with("{\"message\"") => {
            let pieces = [
                "<r i=\"1\">",
                "    threads",
                ".entry(first.name",
                ".clone())",
                ".or_insert(first);\n",
                "</r>",
            ];
            for t in pieces {
                let delta = json!({"type": "text_delta", "text": t});
                let event = json!({"type": "content_block_delta", "index": 0, "delta": delta});
                say(out, json!({"type": "stream_event", "event": event}))?;
            }
            // (the result's text is the answer tarae applies)
            say(out, json!({"type": "result", "result": pieces.concat()}))?;
            Ok(true)
        }
        "answer" if line.starts_with("{\"message\"") => {
            let answer = ANSWER;
            let (id, model) = ("conv-docs", "claude-opus-5-5");
            say(out, json!({"type": "system", "subtype": "init", "session_id": id, "model": model}))?;
            // (a first request's usage: the conversation written to the 5-minute cache)
            let usage = json!({"input_tokens": 3, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 9_800,
                "cache_creation": {"ephemeral_5m_input_tokens": 9_800}, "output_tokens": 96});
            let message = json!({"id": format!("{id}-1"), "model": model, "usage": usage});
            say(
                out,
                json!({"type": "stream_event", "event": {"type": "message_start", "message": message}}),
            )?;
            for t in answer.split_inclusive('\n') {
                let delta = json!({"type": "text_delta", "text": t});
                let event = json!({"type": "content_block_delta", "index": 0, "delta": delta});
                say(out, json!({"type": "stream_event", "event": event}))?;
                // (a model's pace: the turn's time on its last row reads as one — none in tests)
                std::thread::sleep(pause * 3);
            }
            let used = json!({model: {"inputTokens": 3, "cacheCreationInputTokens": 9_800, "outputTokens": 96,
                "contextWindow": 1_000_000}});
            say(
                out,
                json!({"type": "result", "result": answer, "session_id": id, "total_cost_usd": 0.0632,
                    "usage": usage, "modelUsage": used}),
            )?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// `answer`'s words, about the demo's `main.rs`.
const ANSWER: &str = "`insert` takes its key by value, so the name is cloned before `first` moves into the map.\n\n\
    Two things follow from keying by the name alone:\n\n\
    - a second strand with the same name **replaces** the first\n\
    - each strand costs one more `String`\n\n\
    To keep the first one instead:\n\n\
    ```rust\nthreads\n    .entry(first.name.clone())\n    .or_insert(first);\n```\n\n\
    Nothing else reads the map, so that line is the whole change.\n";

/// Every picture, by page in the order it shows them (`target/sb/docshots-plan.md` says what each is for).
const DOCS: &[DocShot] = &[
    // README.md (and features.md)
    row("hero-hanji.png", None, (140, 40)).light().at(2),
    row("hero-meok.png", None, (140, 40)).at(2),
    // docs/claude-integration.md
    row("m4-review.png", Some("askreview"), S),
    row("m4-chat.png", None, S),
    row("chat-model.png", Some("model"), S),
    row("chat-conversations.png", Some("conversations"), S),
    row("inbox.png", None, (120, 34)),
    row("inbox-hanji.png", None, (120, 34)).light(),
    row("chat-screen.png", None, (140, 40)),
    row("chat-point-hanji.png", Some("point"), (140, 34)).light(),
    row("chat-ids.png", None, (140, 34)),
    row("chat-explore.png", None, S),
    // (`chatmid` shows neither the wash nor the thinking these show: phase 3's `thinking` script)
    row("follow.png", None, S),
    row("follow-hanji.png", None, S).light(),
    row("follow-thinking.png", None, S),
    row("follow-show.png", None, (140, 40)),
    row("follow-ask.png", None, S),
    // (the turn's time on the chat's last answer row, right-aligned: tarae's clock, a tenth either way)
    row("m4-chat-hanji.png", Some("chatanswer"), S).light().varies(&[turn_time(24, (103, 111))]),
    row("chat-plan.png", None, S),
    // (its head quotes what was asked, as every review of Claude's change does — frictions34 43)
    row("chat-edit.png", Some("review"), S),
    row("chat-edit-why.png", None, S),
    row("chat-turn-review.png", None, (200, 36)),
    row("chat-review-at-end.png", None, S),
    row("chat-undo-turn.png", None, S),
    row("chat-tests.png", None, S),
    row("chat-run.png", Some("runcard"), S),
    row("notes.png", None, S),
    row("commit-draft.png", None, S),
    row("review.png", Some("codereview"), S),
    row("claude-reviews.png", None, S),
    row("review-risk.png", Some("risk"), S),
    row("review-end-held.png", Some("riskend"), S).light(),
    // docs/features.md
    row("ux-palette.png", Some("palette"), S),
    row("ux-which-key.png", Some("whichkey"), S),
    row("ux-tutor.png", Some("tutor"), S),
    row("ux-welcome.png", Some("welcome"), S),
    row("ux-welcome-continue.png", Some("continue"), S),
    row("ux-theme-picker.png", Some("themes"), S),
    row("ux-toasts-sync.png", Some("toastsync"), S),
    row("ux-doc-comments.png", Some("doccomments"), S),
    row("ux-doc-comments-open.png", Some("doccommentsopen"), S),
    row("ux-guides.png", None, S),
    row("ux-side-panel.png", Some("code"), S),
    row("ux-outline.png", Some("outline"), S),
    row("m2-file-picker.png", Some("filepicker"), S),
    row("ux-search.png", Some("search"), S),
    row("ux-splits.png", Some("splits"), S),
    row("ux-cmdline-complete.png", Some("cmdline"), S),
    row("ux-git.png", Some("gutter"), S),
    row("syntax-helm.png", None, S),
    row("grammar-offer.png", None, S),
    row("offer-stack.png", None, S),
    row("syntax-injections.png", Some("injections"), S),
    row("ux-error-lens.png", None, S),
    row("m3-completion.png", None, S),
    row("m3-signature.png", None, S),
    row("m3-inlay-hints.png", None, S),
    row("m3-code-action-preview.png", None, S),
    row("m3-hover.png", None, S),
    row("ux-symbols.png", None, S),
    row("ux-workspace-symbols.png", None, S),
    row("java-jdt-source.png", None, S).needs(&["jdtls"]),
    row("java-debug-offer.png", None, S).needs(&["jdtls"]),
    // docs/testing-and-debugging.md
    row("grammar-needed.png", None, S),
    row("test-results-rust.png", None, S),
    row("test-results-go.png", None, S).needs(&["go"]),
    row("test-results-python.png", None, S).needs(&["pytest"]),
    row("test-results-java.png", None, S).needs(&["java", "gradle"]),
    row("test-run.png", None, S),
    row("test-run-java.png", None, S).needs(&["java", "gradle"]),
    row("ux-debugger.png", None, S),
    row("ux-debugger-python.png", None, S),
    row("ux-debugger-go.png", None, S),
    row("ux-debugger-java.png", None, S).needs(&["jdtls"]),
    row("ux-debugger-conditions.png", None, S),
    row("attach-python.png", None, S),
];

pub(super) fn run(args: &[String]) -> Result<()> {
    let (mut out, mut names, mut keep, mut both) = (root().join("target/docshots"), Vec::new(), false, false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--list" => {
                DOCS.iter().for_each(|d| println!("{}", listed(d)));
                return Ok(());
            }
            "--out" => out = PathBuf::from(it.next().ok_or(USAGE)?),
            "--keep" => keep = true,
            "--both" => both = true,
            n if !n.starts_with('-') => names.push(n.to_string()),
            _ => return Err(USAGE.into()),
        }
    }
    let rows = chosen(&names)?;
    let docs = root().join("docs/screenshots");
    if keep {
        let pictures: Vec<&str> = rows.iter().map(|d| d.picture).collect();
        return kept(&pictures, &out, &docs);
    }
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let bin = shot::build(&root(), "shot-docs", &["bundled-grammars"])?;
    let me = std::env::current_exe().map_err(|e| format!("this xtask: {e}"))?;
    let (mut failed, mut skipped, mut none, mut unreadable) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let (mut results, mut same, mut twins_failed) = (Vec::new(), 0, Vec::new());
    for d in &rows {
        let Some(scene) = d.scene.and_then(scene_named) else {
            none.push(d.picture);
            continue;
        };
        let missing: Vec<&str> = d.needs.iter().copied().filter(|p| !on_path(p)).collect();
        if !missing.is_empty() {
            skipped.push(format!("{} (no {})", d.picture, missing.join(", ")));
            continue;
        }
        let r = take_row(d, both, &out, &docs, &mut |bg, file| one(&scene, d, bg, &bin, &me, file))?;
        match &r.compared {
            Some(c) => {
                same += usize::from(*c == Compared::Same);
                if matches!(c, Compared::Unreadable(_)) {
                    unreadable.push(d.picture);
                }
            }
            None => failed.push(d.picture),
        }
        if r.twin_failed {
            twins_failed.push(d.picture);
        }
        results.push((d.picture, r.verdict));
    }
    if !none.is_empty() {
        println!("{} with no scene yet: {}", none.len(), none.join(", "));
    }
    if !skipped.is_empty() {
        println!("{} skipped: {}", skipped.len(), skipped.join(", "));
    }
    let taken = results.len() - failed.len();
    println!("{same} of {taken} taken the same as the committed ones");
    // (last, after every picture it names: `--keep` refuses a picture written after it)
    write_manifest(&out, &results)?;
    let mut wrong = Vec::new();
    if !failed.is_empty() {
        wrong.push(format!("{} pictures failed: {}", failed.len(), failed.join(", ")));
    }
    if !unreadable.is_empty() {
        wrong.push(format!("{} couldn't be compared: {}", unreadable.len(), unreadable.join(", ")));
    }
    if !twins_failed.is_empty() {
        wrong.push(format!("{} failed in the other theme: {}", twins_failed.len(), twins_failed.join(", ")));
    }
    if wrong.is_empty() { Ok(()) } else { Err(wrong.join("; ")) }
}

/// What came of one row: its word for the manifest, how it compared (None: its shot failed), and whether its
/// other theme's picture (`--both`) failed.
struct Row {
    verdict: &'static str,
    compared: Option<Compared>,
    twin_failed: bool,
}

/// Row `d` taken into `out` by `take(bg, file)` and compared with its committed picture in `docs` (passing over
/// its `varies`) — and with `both`, taken again on the other theme as its twin, to look at: it has no word of its
/// own, so the manifest never names it and `--keep` never copies it. Each said as it comes.
fn take_row(
    d: &DocShot,
    both: bool,
    out: &Path,
    docs: &Path,
    take: &mut dyn FnMut(Bg, &Path) -> Result<()>,
) -> Result<Row> {
    let file = ready(out, d.picture)?;
    let (verdict, compared) = match take(d.bg, &file) {
        Ok(()) => {
            let c = compared_but(out, docs, d.picture, &|x, y, w, h| d.varying(x, y, w, h));
            println!("{}: {}", d.picture, c.said());
            (verdict(&c), Some(c))
        }
        Err(e) => {
            eprintln!("{}: {e}", file.display());
            ("failed", None)
        }
    };
    let mut twin_failed = false;
    if both {
        let other = ready(out, &twin(d.picture, d.bg))?;
        match take(d.bg.other(), &other) {
            Ok(()) => println!("{}: taken in {} too, to look at", d.picture, d.bg.other().theme()),
            Err(e) => {
                eprintln!("{}: {e}", other.display());
                twin_failed = true;
            }
        }
    }
    Ok(Row { verdict, compared, twin_failed })
}

/// The run's `MANIFEST` in `out`: each row it took and what came of it (`take_row`'s verdicts).
fn write_manifest(out: &Path, results: &[(&str, &str)]) -> Result<()> {
    let manifest: String = results.iter().map(|(p, r)| format!("{p}\t{r}\n")).collect();
    write(&out.join(MANIFEST), &manifest)
}

/// The file in `out` where the last `--docs` run there said what came of each row it took: `<picture>\t<result>`
/// a line (`same`, `differs`, `new`, `unreadable`, `failed`) — written last.
const MANIFEST: &str = "manifest.tsv";

/// A comparison as the manifest says it.
fn verdict(c: &Compared) -> &'static str {
    match c {
        Compared::Same => "same",
        Compared::Differs(_) => "differs",
        Compared::New => "new",
        Compared::Unreadable(_) => "unreadable",
    }
}

/// Where `picture` is taken in `out`, what an earlier run left there gone first — a shot that fails now leaves
/// nothing a `--keep` could take for it.
fn ready(out: &Path, picture: &str) -> Result<PathBuf> {
    let file = out.join(picture);
    match std::fs::remove_file(&file) {
        Ok(()) => Ok(file),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(file),
        Err(e) => Err(format!("{}: {e}", file.display())),
    }
}

/// A row as `--list` prints it.
fn listed(d: &DocShot) -> String {
    let (c, r) = d.size;
    let needs = if d.needs.is_empty() { String::new() } else { format!("  needs {}", d.needs.join(", ")) };
    let scene = d.scene.unwrap_or("(no scene yet)");
    let varies: String =
        d.varies.iter().map(|v| format!("  varies row {} cols {}..{}", v.row, v.cols.0, v.cols.1)).collect();
    format!("{:<28} {scene:<16} {c}×{r} ×{} {}{needs}{varies}", d.picture, d.scale, d.bg.word())
}

/// The rows `names` names (with or without `.png`), in their order — every one when none is named.
fn chosen(names: &[String]) -> Result<Vec<&'static DocShot>> {
    if names.is_empty() {
        return Ok(DOCS.iter().collect());
    }
    let find = |n: &String| {
        DOCS.iter()
            .find(|d| d.picture == n || d.picture.strip_suffix(".png") == Some(n.as_str()))
            .ok_or_else(|| format!("no docs picture `{n}` — cargo xtask scenes --docs --list"))
    };
    names.iter().map(find).collect()
}

/// `--keep`: each of `pictures` the last run in `out` took and found different from the one in `docs`, or new
/// (its `MANIFEST`), copied over it; every other is left, and said why. Refused with no manifest, or when a
/// picture it names was written after it (not what the run compared).
fn kept(pictures: &[&str], out: &Path, docs: &Path) -> Result<()> {
    let list = out.join(MANIFEST);
    let nothing = |e: std::io::Error| {
        format!("{}: {e} — no run's results to keep from: cargo xtask scenes --docs first", list.display())
    };
    let written = std::fs::metadata(&list).and_then(|m| m.modified()).map_err(nothing)?;
    let text = std::fs::read_to_string(&list).map_err(nothing)?;
    let results: Vec<(&str, &str)> = text.lines().filter_map(|l| l.split_once('\t')).collect();
    for (p, _) in &results {
        let shot = out.join(p);
        if let Ok(when) = std::fs::metadata(&shot).and_then(|m| m.modified())
            && when > written
        {
            return Err(format!(
                "{}: written after the run that compared it — cargo xtask scenes --docs again",
                shot.display()
            ));
        }
    }
    let (mut copied, mut left) = (0, Vec::new());
    for p in pictures {
        match results.iter().find(|(q, _)| q == p).map(|(_, r)| *r) {
            Some("differs" | "new") => {
                let (shot, there) = (out.join(p), docs.join(p));
                std::fs::copy(&shot, &there).map_err(|e| format!("{}: {e}", shot.display()))?;
                println!("kept {}", there.display());
                copied += 1;
            }
            Some(r) => left.push(format!("{p} ({r})")),
            None => left.push(format!("{p} (not taken)")),
        }
    }
    println!("{copied} kept");
    if !left.is_empty() {
        println!("{} left as they are: {}", left.len(), left.join(", "));
    }
    Ok(())
}

/// One picture of `d`'s on `bg`: `scene`'s project, config, state and homes in a folder of its name, removed
/// after — by its real path, as tarae names its folders (the state seeded says them as tarae will).
fn one(scene: &DocScene, d: &DocShot, bg: Bg, bin: &Path, me: &Path, out: &Path) -> Result<()> {
    let p = prepared(scene, bg, bin, me)?;
    let screen = shot::take_at(&p.setup, &p.keys, d.size, d.scale, &p.state, &p.homes, out)?;
    d.varies_hold(&screen)?;
    none_of_yours(&screen, std::env::var_os("HOME").as_deref())
}

/// A docs picture names nothing of yours: your home's path never shows on its screen (`yours`, the `HOME` the shot
/// is taken in — the toolchains' homes are still reached there, `homes`, and this xtask runs from there as the
/// stand-in claude), nor your name as a folder in a path cut from the left (`…/you/.cargo`).
fn none_of_yours(screen: &[Vec<String>], yours: Option<&std::ffi::OsStr>) -> Result<()> {
    let Some(home) = yours.and_then(|h| h.to_str()).filter(|h| h.len() > 1) else { return Ok(()) };
    let name = Path::new(home).file_name().and_then(|n| n.to_str()).map(|n| format!("/{n}/"));
    for (i, row) in screen.iter().enumerate() {
        let text = row.concat();
        if text.contains(home) || name.as_ref().is_some_and(|n| text.contains(n.as_str())) {
            return Err(format!(
                "row {i} shows your home ({home}): a docs picture names nothing of yours — {text:?}"
            ));
        }
    }
    Ok(())
}

/// A shot of `scene`'s, ready to take: its folder (removed on drop), tarae's setup, state and homes, the keys.
struct Prepared {
    _dir: Named,
    setup: Setup,
    state: PathBuf,
    homes: Vec<(&'static str, std::ffi::OsString)>,
    keys: Vec<String>,
}

fn prepared(scene: &DocScene, bg: Bg, bin: &Path, me: &Path) -> Result<Prepared> {
    let dir = Named::new(scene.name)?;
    let real = std::fs::canonicalize(&dir.0).map_err(|e| format!("{}: {e}", dir.0.display()))?;
    let mut homes = homes(&real, &|k| std::env::var_os(k))?;
    homes.push(("TZ", fixed_clock(now()).into()));
    let home = real.join("home");
    let cwd = home.join(scene.fixture.folder());
    scene.fixture.make(&cwd)?;
    let state = real.join("state");
    fixtures::seed(scene.seed, &state.join("tarae"), &home, &cwd, now())?;
    let config = real.join("xdg");
    write(&config.join("tarae/config.toml"), &format!("{}{}", config_toml(me, scene.claude), scene.config))?;
    let setup = Setup {
        bin: bin.to_path_buf(),
        files: scene.files.iter().map(|f| f.to_string()).collect(),
        cwd,
        config,
        background: Some(bg.word()),
        env: Vec::new(),
    };
    let keys: Vec<String> = scene.keys.iter().map(|k| k.to_string()).collect();
    Ok(Prepared { _dir: dir, setup, state, homes, keys })
}

/// The time a docs shot's clock reads as it starts: what tarae says in local time (a cache's `warm until 14:32`,
/// `:messages`) comes out the same at every retake — it reads the clock in the shot's own time zone.
const SHOT_CLOCK: u64 = 14 * 3600 + 27 * 60;

/// A POSIX `TZ` whose local time is `SHOT_CLOCK` at `now` (Unix seconds): an offset from UTC to the second, within
/// ±12 h (POSIX's sign is west of UTC, so a zone east of it reads `-`).
fn fixed_clock(now: u64) -> String {
    const DAY: i64 = 86_400;
    let east = (SHOT_CLOCK as i64 - (now as i64).rem_euclid(DAY)).rem_euclid(DAY);
    let east = if east > DAY / 2 { east - DAY } else { east };
    let (sign, a) = if east >= 0 { ('-', east) } else { ('+', -east) };
    format!("SHT{sign}{:02}:{:02}:{:02}", a / 3600, a / 60 % 60, a % 60)
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// A docs shot's homes in its folder `dir` (`var`: the environment it's taken in): `HOME`, `XDG_DATA_HOME`,
/// `XDG_CACHE_HOME` its own — and the toolchains' homes yours, named, so what runs cargo there still finds its
/// toolchain (rustup's and cargo's homes default to `~`).
fn homes(
    dir: &Path,
    var: &dyn Fn(&str) -> Option<std::ffi::OsString>,
) -> Result<Vec<(&'static str, std::ffi::OsString)>> {
    let yours = var("HOME").map(PathBuf::from).ok_or("no HOME to find the toolchains' homes in")?;
    let tool = |name: &str, default: &str| var(name).unwrap_or_else(|| yours.join(default).into_os_string());
    Ok(vec![
        ("CARGO_HOME", tool("CARGO_HOME", ".cargo")),
        ("RUSTUP_HOME", tool("RUSTUP_HOME", ".rustup")),
        ("HOME", dir.join("home").into()),
        ("XDG_DATA_HOME", dir.join("data").into()),
        ("XDG_CACHE_HOME", dir.join("cache").into()),
    ])
}

/// `<temp>/tarae-docs/<name>`, made new — never one that was there: a docs shot that didn't finish, or someone
/// else, left it, and it's said rather than cleared. Removed on drop, and only it.
struct Named(PathBuf);

impl Named {
    fn new(name: &str) -> Result<Named> {
        Named::under(&std::env::temp_dir().join("tarae-docs"), name)
    }

    /// `name` (one name, no `/`) made new, 0700, in `base` — which is made 0700 when it isn't there, and either
    /// way opened as itself (no link at its end) and judged on that descriptor: a folder of yours that no one else
    /// may use (`refused`), else someone could put a link where `name` goes. `name` is made through that
    /// descriptor (`mkdirat`); the path kept is `base`/`name` as made.
    fn under(base: &Path, name: &str) -> Result<Named> {
        let said = |e: std::io::Error| format!("{}: {e}", base.display());
        if name.is_empty() || name.contains('/') || name == "." || name == ".." {
            return Err(format!("`{name}`: not one folder's name"));
        }
        match std::fs::DirBuilder::new().mode(0o700).create(base) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(said(e)),
        }
        let c =
            CString::new(base.as_os_str().as_bytes()).map_err(|_| format!("{}: a NUL", base.display()))?;
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        // SAFETY: a NUL-terminated path; the descriptor is owned below.
        let fd = unsafe { libc::open(c.as_ptr(), flags) };
        if fd < 0 {
            let e = std::io::Error::last_os_error();
            return Err(match e.raw_os_error() {
                Some(libc::ELOOP | libc::ENOTDIR) => {
                    format!("{}: not a folder (a link, or a file)", base.display())
                }
                _ => said(e),
            });
        }
        // SAFETY: `fd` was just opened and is owned by nothing else.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: a zeroed stat is a valid buffer for fstat to fill.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: `fd` is open, `st` is ours.
        if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
            return Err(said(std::io::Error::last_os_error()));
        }
        // SAFETY: getuid has no preconditions and can't fail.
        let me = unsafe { libc::getuid() };
        if let Some(why) = refused(st.st_uid, st.st_mode, me) {
            return Err(format!("{}: {why}", base.display()));
        }
        let n = CString::new(name).map_err(|_| format!("`{name}`: a NUL"))?;
        let p = base.join(name);
        // SAFETY: `fd` is open, `n` NUL-terminated.
        if unsafe { libc::mkdirat(fd.as_raw_fd(), n.as_ptr(), 0o700) } != 0 {
            let e = std::io::Error::last_os_error();
            return Err(match e.kind() {
                std::io::ErrorKind::AlreadyExists => format!(
                    "{}: there already (a docs shot that didn't finish?) — look, remove it, run again",
                    p.display()
                ),
                _ => format!("{}: {e}", p.display()),
            });
        }
        Ok(Named(p))
    }
}

/// Why a folder owned by `uid` with `mode` can't hold docs shots for user `me`: not theirs, or others may use it.
/// (`mode`: `st_mode`, a `u16` on macOS and a `u32` on Linux)
fn refused(uid: u32, mode: impl Into<u32>, me: u32) -> Option<String> {
    let mode: u32 = mode.into();
    if uid != me {
        return Some(format!("not yours (its owner is {uid})"));
    }
    (mode & 0o077 != 0)
        .then(|| format!("others may use it (mode {:o}) — it must be yours alone (0700)", mode & 0o777))
}

impl Drop for Named {
    /// Only the folder it made, and what's in it — a link put in its place is removed itself, never followed.
    fn drop(&mut self) {
        let _ = match std::fs::symlink_metadata(&self.0) {
            Ok(m) if m.is_dir() => std::fs::remove_dir_all(&self.0),
            Ok(_) => std::fs::remove_file(&self.0),
            Err(_) => Ok(()),
        };
    }
}

/// Whether `program` is a file in one of `PATH`'s folders.
fn on_path(program: &str) -> bool {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path).any(|d| d.join(program).is_file())
}

/// The pictures `text` references — `screenshots/<name>.png` wherever a line has it (`](…)`, `src=`, `srcset=`):
/// each with its line number.
#[cfg(test)]
fn references(text: &str) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    for (i, line) in text.lines().enumerate() {
        for (at, _) in line.match_indices("screenshots/") {
            let rest = &line[at + "screenshots/".len()..];
            let name: String =
                rest.chars().take_while(|c| c.is_ascii_alphanumeric() || "-_.".contains(*c)).collect();
            if let Some(end) = name.find(".png") {
                found.push((i + 1, name[..end + ".png".len()].to_string()));
            }
        }
    }
    found
}

/// The pages that show pictures: the README and every Markdown file under `docs/`.
#[cfg(test)]
fn pages(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, into: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let p = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() => walk(&p, into),
                Ok(t) if t.is_file() && p.extension().is_some_and(|x| x == "md") => into.push(p),
                _ => {}
            }
        }
    }
    let mut all = vec![root.join("README.md")];
    walk(&root.join("docs"), &mut all);
    all
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::super::compared;
    use super::*;

    fn rows() -> BTreeSet<&'static str> {
        DOCS.iter().map(|d| d.picture).collect()
    }

    /// Each picture a page references (`page:line`), and each row nobody references.
    #[test]
    fn every_docs_picture_has_a_row() {
        let root = root();
        let (rows, mut seen, mut unlisted) = (rows(), BTreeSet::new(), Vec::new());
        for page in pages(&root) {
            let text = std::fs::read_to_string(&page).unwrap();
            let shown = page.strip_prefix(&root).unwrap_or(&page).display().to_string();
            for (line, name) in references(&text) {
                if !rows.contains(name.as_str()) {
                    unlisted.push(format!("{shown}:{line} {name}"));
                }
                seen.insert(name);
            }
        }
        assert!(unlisted.is_empty(), "pictures the docs show with no row in DOCS: {unlisted:?}");
        let nobodys: Vec<_> = rows.iter().filter(|r| !seen.contains(**r)).collect();
        assert!(nobodys.is_empty(), "rows in DOCS no page references: {nobodys:?}");
    }

    /// No picture in `docs/screenshots/` without a row, no row without its picture.
    #[test]
    fn every_picture_file_is_in_the_table() {
        let dir = root().join("docs/screenshots");
        let files: BTreeSet<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n.ends_with(".png"))
            .collect();
        let rows = rows();
        let orphans: Vec<_> = files.iter().filter(|f| !rows.contains(f.as_str())).collect();
        assert!(orphans.is_empty(), "pictures in docs/screenshots/ with no row in DOCS: {orphans:?}");
        let gone: Vec<_> = rows.iter().filter(|r| !files.contains(**r)).collect();
        assert!(gone.is_empty(), "rows in DOCS with no picture in docs/screenshots/: {gone:?}");
    }

    /// Each row's picture once, its scene the docs' own or one `scenes --list` names, a scale `shot` draws (1–4).
    #[test]
    fn every_docs_row_names_a_scene() {
        assert_eq!(rows().len(), DOCS.len(), "a picture in DOCS twice");
        for d in DOCS {
            if let Some(s) = d.scene {
                assert!(scene_named(s).is_some(), "{}: no scene `{s}`", d.picture);
            }
            assert!((1..=4).contains(&d.scale), "{}: scale {}", d.picture, d.scale);
        }
        assert!(chosen(&["chat-edit".into(), "follow.png".into()]).is_ok_and(|r| r.len() == 2));
        assert!(chosen(&["nope".into()]).unwrap_err().contains("no docs picture `nope`"));
    }

    /// A docs scene's name is its own: once among the docs' scenes, never one of `scenes.rs`'s (which the docs'
    /// would hide), each made a picture of; one of `scenes.rs`'s stands on its demo crate, `trusted` with its
    /// settings.
    #[test]
    fn docs_scenes_are_named_once_and_apart_from_the_scenes() {
        let mut seen = BTreeSet::new();
        for s in DOC_SCENES {
            assert!(seen.insert(s.name), "`{}` twice in DOC_SCENES", s.name);
            assert!(!SCENES.iter().any(|x| x.name == s.name), "`{}` is also a scenes.rs scene", s.name);
            assert!(DOCS.iter().any(|d| d.scene == Some(s.name)), "`{}`: no picture of it", s.name);
        }
        let card = scene_named("card").unwrap();
        assert_eq!((card.fixture, card.claude, card.seed), (Fixture::Demo, "card", Seed::Nothing));
        assert_eq!(scene_named("trusted").map(|s| s.fixture), Some(Fixture::Trusted));
        assert!(scene_named("nope").is_none());
    }

    /// A docs shot's `HOME`, data and cache homes are in its own folder; the toolchains' homes are yours — as set,
    /// else where they default in your home — and never in that folder.
    #[test]
    fn a_docs_shot_keeps_to_its_own_homes() {
        let dir = Path::new("/t/tarae-docs/gutter");
        let env = |set: &'static [(&'static str, &'static str)]| {
            move |k: &str| set.iter().find(|(n, _)| *n == k).map(|(_, v)| std::ffi::OsString::from(v))
        };
        let got = homes(dir, &env(&[("HOME", "/Users/me")])).unwrap();
        let at = |name: &str| got.iter().find(|(n, _)| *n == name).map(|(_, p)| PathBuf::from(p)).unwrap();
        assert_eq!(at("HOME"), dir.join("home"));
        assert_eq!(at("XDG_DATA_HOME"), dir.join("data"));
        assert_eq!(at("XDG_CACHE_HOME"), dir.join("cache"));
        assert_eq!(at("CARGO_HOME"), Path::new("/Users/me/.cargo"));
        assert_eq!(at("RUSTUP_HOME"), Path::new("/Users/me/.rustup"));
        let set = homes(dir, &env(&[("HOME", "/Users/me"), ("CARGO_HOME", "/opt/cargo")])).unwrap();
        assert!(set.contains(&("CARGO_HOME", "/opt/cargo".into())), "{set:?}");
        assert!(homes(dir, &env(&[])).is_err(), "no HOME, no toolchains found");
    }

    /// A docs shot's clock reads `SHOT_CLOCK` as it starts, whenever it's taken: its `TZ` puts local time there —
    /// checked with the C library's own reading of it (as tarae's `log::clock` reads the time).
    #[test]
    fn a_docs_shots_clock_reads_the_same_whenever_it_is_taken() {
        assert_eq!(fixed_clock(SHOT_CLOCK), "SHT-00:00:00");
        assert_eq!(fixed_clock(SHOT_CLOCK + 3600 + 61), "SHT+01:01:01");
        assert_eq!(fixed_clock(SHOT_CLOCK + 13 * 3600), "SHT-11:00:00", "within ±12 h");
        // (the zone read by `date` in a process of its own: this test binary's TZ stays as it is)
        for now in [1_760_000_000u64, 1_760_031_337, 1_760_077_777] {
            // (macOS's date takes the seconds after -r, GNU's after -d @)
            let at = if cfg!(target_os = "macos") {
                ["-r".into(), now.to_string()]
            } else {
                ["-d".into(), format!("@{now}")]
            };
            let out = std::process::Command::new("date")
                .args(at)
                .arg("+%H:%M:%S")
                .env("TZ", fixed_clock(now))
                .output()
                .unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "14:27:00", "at {now}");
        }
    }

    /// The docs' scripts answer what's theirs and leave the rest: `models` the `initialize` request with models
    /// (not a message), `askdone` the message with the whole rewritten line and the answer's end; another script
    /// nothing. Through `scenes::claude`, as tarae starts it.
    #[test]
    fn the_docs_scripts_answer_what_is_theirs() {
        let init =
            r#"{"request":{"subtype":"initialize"},"request_id":"tarae-init","type":"control_request"}"#;
        let msg = r#"{"message":{"content":"keep the first strand"},"type":"user"}"#;
        let said = |script: &str| {
            let mut out = Vec::new();
            let input = format!("{init}\n{msg}\n");
            super::super::claude(script, input.as_bytes(), &mut out, std::time::Duration::ZERO).unwrap();
            String::from_utf8(out).unwrap()
        };
        let models = said("models");
        assert_eq!(models.lines().count(), 1, "{models}");
        let v: serde_json::Value = serde_json::from_str(models.trim()).unwrap();
        assert_eq!(v["response"]["request_id"], "tarae-init");
        let offered = v["response"]["response"]["models"].as_array().unwrap();
        assert!(
            offered.len() > 2
                && offered.iter().all(|m| m["value"].is_string() && m["displayName"].is_string())
        );
        let ask = said("askdone");
        let text: String = ask
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter_map(|v| v["event"]["delta"]["text"].as_str().map(String::from))
            .collect();
        assert_eq!(text, "<r i=\"1\">    threads.entry(first.name.clone()).or_insert(first);\n</r>");
        let last: serde_json::Value = serde_json::from_str(ask.lines().last().unwrap()).unwrap();
        assert_eq!(
            (&last["type"], last["result"].as_str()),
            (&json!("result"), Some(text.as_str())),
            "{ask}"
        );
        assert!(!ask.contains("tarae-init"));
        let answer = said("answer");
        let lines: Vec<serde_json::Value> =
            answer.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        // (the models first, so the conversation's model is named as `claude` names it)
        assert_eq!(lines[0]["response"]["request_id"], "tarae-init");
        assert_eq!((&lines[1]["subtype"], &lines[1]["model"]), (&json!("init"), &json!("claude-opus-5-5")));
        let text: String = lines.iter().filter_map(|v| v["event"]["delta"]["text"].as_str()).collect();
        assert_eq!(text, ANSWER);
        assert!(text.contains("\n- ") && text.contains("```rust\n") && text.contains("`insert`"), "{text}");
        assert_eq!(lines.last().unwrap()["result"], json!(ANSWER));
        assert_eq!(said("idle"), "");
    }

    /// `--both`'s other theme goes beside the row's picture under a name no row has, never in the manifest.
    #[test]
    fn the_other_themes_picture_has_a_name_of_its_own() {
        assert_eq!(twin("chat-run.png", Bg::Dark), "chat-run.hanji.png");
        assert_eq!(twin("m4-chat-hanji.png", Bg::Light), "m4-chat-hanji.meok.png");
        assert!(DOCS.iter().all(|d| !rows().contains(twin(d.picture, d.bg).as_str())));
    }

    /// `--keep` goes by the last run's manifest alone: only what it took and found different or new is copied —
    /// never what it found the same, a row whose shot failed, or a file it didn't take (an older run's); with no
    /// manifest, or a picture written after it, nothing is.
    #[test]
    fn keep_copies_only_what_the_run_compared_as_changed() {
        let dir = super::super::Fresh::new("tarae-docshots-keep").unwrap();
        let (out, docs) = (dir.0.join("out"), dir.0.join("docs"));
        let put = |p: &Path, s: &str| write(p, s).unwrap();
        let read = |p: &Path| std::fs::read_to_string(p).unwrap();
        let all = ["same.png", "new.png", "changed.png", "failed.png", "stale.png"];
        assert!(kept(&all, &out, &docs).unwrap_err().contains("cargo xtask scenes --docs first"));
        for n in all {
            put(&docs.join(n), "committed");
            put(&out.join(n), "taken");
        }
        std::fs::remove_file(docs.join("new.png")).unwrap();
        put(&out.join(MANIFEST), "same.png\tsame\nnew.png\tnew\nchanged.png\tdiffers\nfailed.png\tfailed\n");
        kept(&all, &out, &docs).unwrap();
        assert_eq!(read(&docs.join("new.png")), "taken");
        assert_eq!(read(&docs.join("changed.png")), "taken");
        for n in ["same.png", "failed.png", "stale.png"] {
            assert_eq!(read(&docs.join(n)), "committed", "{n} kept, though the run didn't find it changed");
        }

        // (a picture the manifest names, written after it)
        put(&docs.join("changed.png"), "committed");
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(out.join("changed.png"))
            .unwrap()
            .set_modified(later)
            .unwrap();
        let after = kept(&all, &out, &docs).unwrap_err();
        assert!(after.contains("written after the run"), "{after}");
        assert_eq!(read(&docs.join("changed.png")), "committed");
    }

    /// A row's picture from an earlier run is gone before its shot: one that fails now leaves nothing to keep.
    #[test]
    fn a_rows_old_picture_goes_before_its_shot() {
        let dir = super::super::Fresh::new("tarae-docshots-ready").unwrap();
        write(&dir.0.join("a.png"), "an earlier run's").unwrap();
        let file = ready(&dir.0, "a.png").unwrap();
        assert_eq!(file, dir.0.join("a.png"));
        assert!(!file.exists());
        assert!(ready(&dir.0, "b.png").is_ok(), "none there is fine");
    }

    /// A docs shot's folder is made new in a folder of yours alone, through no link: one there already is
    /// refused and left as it is; a base others may read or write, a link or a file is refused, nothing made; the
    /// drop removes only the folder made (a link put in its place goes, its target stays), the base kept.
    #[test]
    fn a_docs_shots_folder_is_new_and_in_a_folder_of_yours_alone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = super::super::Fresh::new("tarae-docshots-named").unwrap();
        let base = dir.0.join("tarae-docs");
        let a = Named::under(&base, "code").unwrap();
        assert_eq!(a.0, base.join("code"), "the path as made");
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!((mode(&base), mode(&a.0)), (0o700, 0o700));
        write(&a.0.join("marker"), "mine").unwrap();
        write(&base.join("beside"), "not the shot's").unwrap();
        let again = Named::under(&base, "code").err().unwrap();
        assert!(again.contains("there already"), "{again}");
        assert!(a.0.join("marker").is_file(), "a folder there already is left as it is");
        let made = a.0.clone();
        drop(a);
        assert!(!made.exists() && base.join("beside").is_file());

        // (the made folder swapped for a link before the drop: the link goes, never what it leads to)
        let target = dir.0.join("target");
        write(&target.join("keep"), "theirs").unwrap();
        let b = Named::under(&base, "swap").unwrap();
        std::fs::remove_dir(&b.0).unwrap();
        std::os::unix::fs::symlink(&target, &b.0).unwrap();
        drop(b);
        assert!(target.join("keep").is_file() && std::fs::symlink_metadata(base.join("swap")).is_err());

        for (m, said) in [(0o777, "mode 777"), (0o720, "mode 720"), (0o704, "mode 704")] {
            std::fs::set_permissions(&base, std::fs::Permissions::from_mode(m)).unwrap();
            let open = Named::under(&base, "picker").err().unwrap();
            assert!(open.contains(said), "{open}");
            assert!(!base.join("picker").exists());
        }

        let link = dir.0.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let linked = Named::under(&link, "picker").err().unwrap();
        assert!(linked.contains("not a folder"), "{linked}");
        assert!(!target.join("picker").exists());
        let file = dir.0.join("file");
        write(&file, "").unwrap();
        assert!(Named::under(&file, "picker").err().unwrap().contains("not a folder"));
        assert!(Named::under(&target, "../x").err().unwrap().contains("not one folder's name"));
    }

    /// A base owned by another user is refused (`refused` is what `Named` asks of the base's descriptor — a
    /// folder of another user's can't be made here without root), as is one others may use; yours alone isn't.
    #[test]
    fn a_base_not_yours_or_open_to_others_is_refused() {
        assert!(refused(501, 0o40700u32, 502).is_some_and(|w| w.contains("not yours (its owner is 501)")));
        assert!(refused(501, 0o40750u32, 501).is_some_and(|w| w.contains("mode 750")));
        assert_eq!(refused(501, 0o40700u32, 501), None);
    }

    /// A row's `varies` cells are passed over when it's compared: a picture differing there alone is the same;
    /// one differing in a cell beside them, or on another row, isn't. (10×3 cells of 9×21 px, as `shot` draws
    /// them at 1×.)
    #[test]
    fn a_rows_varying_cells_are_left_out_of_its_comparison() {
        let dir = super::super::Fresh::new("tarae-docshots-varies").unwrap();
        const VARIES: &[Varies] = &[turn_time(1, (4, 6))];
        let d = row("x.png", None, (10, 3)).varies(VARIES);
        let (w, h, pad) = (10 * 9 + 2 * shot::PAD, 3 * 21 + 2 * shot::PAD, shot::PAD);
        let png = |p: &Path, dot: Option<(usize, usize)>| {
            let mut px = vec![0u8; w * h * 3];
            if let Some((x, y)) = dot {
                px[(y * w + x) * 3] = 255;
            }
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            let mut e = png::Encoder::new(std::fs::File::create(p).unwrap(), w as u32, h as u32);
            e.set_color(png::ColorType::Rgb);
            e.write_header().unwrap().write_image_data(&px).unwrap();
        };
        let (a, b) = (dir.0.join("a"), dir.0.join("b"));
        png(&b.join("x.png"), None);
        let cell = |row: usize, col: usize| (pad + col * 9 + 4, pad + row * 21 + 10);
        let said = |dot| {
            png(&a.join("x.png"), Some(dot));
            compared_but(&a, &b, "x.png", &|x, y, w, h| d.varying(x, y, w, h))
        };
        assert_eq!(said(cell(1, 4)), Compared::Same, "in a varying cell");
        assert_eq!(said(cell(1, 5)), Compared::Same, "in the other varying cell");
        assert!(matches!(said(cell(1, 6)), Compared::Differs(_)), "the cell after them");
        assert!(matches!(said(cell(1, 3)), Compared::Differs(_)), "the cell before them");
        assert!(matches!(said(cell(0, 4)), Compared::Differs(_)), "the row above");
        assert_eq!(
            compared(&a, &b, "x.png"),
            Compared::Differs("in x 54..=54, y 24..=24 (of 118×91)".into())
        );
    }

    /// A row's `varies` is checked against the screen just taken: a turn's time there passes, anything else —
    /// the layout moved under the mask — fails the shot, naming the cells and what they hold.
    #[test]
    fn a_varying_cells_text_is_checked_at_every_take() {
        const VARIES: &[Varies] = &[turn_time(1, (4, 9))];
        let d = row("x.png", None, (10, 2)).varies(VARIES);
        let screen = |text: &str| {
            let mut row: Vec<String> = vec![String::new(); 10];
            text.chars().enumerate().for_each(|(i, c)| row[4 + i] = c.to_string());
            vec![vec![String::new(); 10], row]
        };
        assert_eq!(d.varies_hold(&screen(" 2.0s")), Ok(()));
        assert_eq!(d.varies_hold(&screen("12.3s")), Ok(()));
        for moved in ["ctx 0", "", "2s", "2.05s", "a.0s"] {
            let e = d.varies_hold(&screen(moved)).unwrap_err();
            assert!(e.contains("row 1 cols 4..9") && e.contains("the layout moved"), "{moved:?}: {e}");
        }
    }

    /// A docs shot's screen that shows your home — whole, or your name as a folder in a path cut from the left —
    /// fails the shot, naming the row; one that doesn't passes, and so does a shot taken with no home known.
    #[test]
    fn a_docs_shot_names_nothing_of_yours() {
        let screen = |t: &str| vec![vec!["ok".to_string()], t.chars().map(String::from).collect::<Vec<_>>()];
        let yours = Some(std::ffi::OsStr::new("/Users/me"));
        assert_eq!(none_of_yours(&screen("~/tarae-calc/src  rust"), yours), Ok(()));
        for shown in ["cargo at /Users/me/.cargo/bin", "…rs/me/.rustup/toolchains"] {
            let e = none_of_yours(&screen(shown), yours).unwrap_err();
            assert!(e.starts_with("row 1 shows your home (/Users/me)"), "{shown:?}: {e}");
        }
        assert_eq!(none_of_yours(&screen("/Users/me/.cargo"), None), Ok(()), "no home known");
    }

    /// `--both`: a row is taken on its own theme, then its twin on the other beside it, to look at — the twin's
    /// failure is the run's to say, never the row's word; the manifest names only the row, so `--keep` copies it
    /// and never the twin. (A stand-in takes the pictures: one pixel, red on meok, blue on hanji.)
    #[test]
    fn both_takes_the_twin_on_the_other_theme_and_never_keeps_it() {
        let dir = super::super::Fresh::new("tarae-docshots-both").unwrap();
        let (out, docs) = (dir.0.join("out"), dir.0.join("docs"));
        let png = |p: &Path, px: [u8; 3]| {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            let mut e = png::Encoder::new(std::fs::File::create(p).unwrap(), 1, 1);
            e.set_color(png::ColorType::Rgb);
            e.write_header().unwrap().write_image_data(&px).unwrap();
        };
        png(&docs.join("p.png"), [0, 0, 0]);
        let d = row("p.png", None, (1, 1));
        let mut taken = Vec::new();
        let mut take = |bg: Bg, file: &Path| -> Result<()> {
            taken.push((bg, file.file_name().unwrap().to_string_lossy().into_owned()));
            png(file, if bg == Bg::Dark { [255, 0, 0] } else { [0, 0, 255] });
            Ok(())
        };
        let r = take_row(&d, true, &out, &docs, &mut take).unwrap();
        assert_eq!(taken, [(Bg::Dark, "p.png".into()), (Bg::Light, "p.hanji.png".into())]);
        assert_eq!((r.verdict, r.twin_failed), ("differs", false));
        write_manifest(&out, &[(d.picture, r.verdict)]).unwrap();
        assert_eq!(std::fs::read_to_string(out.join(MANIFEST)).unwrap(), "p.png\tdiffers\n");
        kept(&["p.png", "p.hanji.png"], &out, &docs).unwrap();
        assert!(docs.join("p.png").is_file() && !docs.join("p.hanji.png").exists(), "the twin never kept");

        // (the twin's shot failing: the row's word stands, the failure said apart)
        let mut failing = |bg: Bg, file: &Path| -> Result<()> {
            if bg == Bg::Light {
                return Err("no hanji today".into());
            }
            png(file, [255, 0, 0]);
            Ok(())
        };
        let r = take_row(&d, true, &out, &docs, &mut failing).unwrap();
        assert_eq!((r.verdict, r.twin_failed), ("same", true), "the row as kept above, the twin failed");
        let r = take_row(&d, false, &out, &docs, &mut failing).unwrap();
        assert!(!r.twin_failed && !out.join("p.hanji.png").exists(), "no --both: no twin, the old one gone");
    }

    /// A docs shot is put together from its scene: its project made in the shot's home, its state seeded under
    /// the state home's `tarae/`, its settings after the scenes' config, the theme's ground — and the homes it runs
    /// with are its folder's, the clock's zone among them. (Nothing is run: `prepared` only lays it out.)
    #[test]
    fn a_docs_shot_is_laid_out_from_its_scene() {
        let name: &'static str = Box::leak(format!("prepared-test-{}", std::process::id()).into_boxed_str());
        let s = DocScene {
            config: "\n[review]\nrisky = [\"knot.rs\"]\n",
            ..scene(name, Fixture::Calc, LIB, &["@1"])
        }
        .seed(Seed::Chats);
        let p = prepared(&s, Bg::Light, Path::new("/x/tarae"), Path::new("/x/xtask")).unwrap();
        let real = p.setup.cwd.parent().and_then(Path::parent).unwrap().to_path_buf();
        assert!(real.ends_with(name), "{}", real.display());
        assert_eq!(p.setup.cwd, real.join("home/tarae-calc"));
        assert!(
            p.setup.cwd.join(".git").is_dir() && p.setup.cwd.join("src/lib.rs").is_file(),
            "the project made"
        );
        assert_eq!(
            (p.setup.background, p.setup.files.as_slice()),
            (Some("light"), &["src/lib.rs".to_string()][..])
        );
        assert_eq!(p.state, real.join("state"));
        let chats = std::fs::read_dir(p.state.join("tarae/chats")).unwrap().count();
        assert_eq!(chats, 1, "the seed under the state home's tarae/");
        let config = std::fs::read_to_string(p.setup.config.join("tarae/config.toml")).unwrap();
        assert!(config.contains("lsp = false") && config.ends_with(s.config), "{config}");
        let home = |k: &str| p.homes.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone());
        assert_eq!(home("HOME"), Some(real.join("home").into()));
        assert_eq!(home("XDG_CACHE_HOME"), Some(real.join("cache").into()));
        assert!(home("TZ").is_some_and(|z| z.to_string_lossy().starts_with("SHT")), "the clock's zone");
        assert_eq!(p.keys, ["@1"]);
        drop(p);
        assert!(!real.exists(), "its folder goes with it");
    }

    /// Each picture is compared on its own: the same, different (in pixels or size), new (no committed one), or
    /// unreadable — said with the file that couldn't be read, and never an error that stops the run.
    #[test]
    fn a_picture_compares_on_its_own_and_an_unreadable_one_is_named() {
        let dir = super::super::Fresh::new("tarae-docshots-compare").unwrap();
        let (a, b) = (dir.0.join("a"), dir.0.join("b"));
        let png = |p: &Path, w: u32, px: &[u8]| {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            let mut e = png::Encoder::new(std::fs::File::create(p).unwrap(), w, 1);
            e.set_color(png::ColorType::Rgb);
            e.write_header().unwrap().write_image_data(px).unwrap();
        };
        png(&a.join("same.png"), 1, &[1, 2, 3]);
        png(&b.join("same.png"), 1, &[1, 2, 3]);
        png(&a.join("pixel.png"), 2, &[1, 2, 3, 4, 5, 6]);
        png(&b.join("pixel.png"), 2, &[1, 2, 3, 9, 9, 9]);
        png(&a.join("size.png"), 1, &[1, 2, 3]);
        png(&b.join("size.png"), 2, &[1, 2, 3, 1, 2, 3]);
        png(&a.join("new.png"), 1, &[1, 2, 3]);
        write(&a.join("bad.png"), "not a picture").unwrap();
        png(&b.join("bad.png"), 1, &[1, 2, 3]);
        assert_eq!(compared(&a, &b, "same.png"), Compared::Same);
        assert_eq!(compared(&a, &b, "pixel.png"), Compared::Differs("in x 1..=1, y 0..=0 (of 2×1)".into()));
        assert!(
            matches!(compared(&a, &b, "size.png"), Compared::Differs(s) if s.contains("1×1 against 2×1"))
        );
        assert_eq!(compared(&a, &b, "new.png"), Compared::New);
        match compared(&a, &b, "bad.png") {
            Compared::Unreadable(why) => assert!(why.contains("bad.png"), "{why}"),
            other => panic!("an unreadable picture compared as {other:?}"),
        }
    }

    /// A reference is read wherever its line has it, ending at `.png` whatever follows.
    #[test]
    fn a_reference_is_read_in_links_images_and_html() {
        let text = "![a](screenshots/a-1.png) and [b](screenshots/b.png).\n<source srcset=\"docs/screenshots/c_2.png\">\nscreenshots/ alone, screenshots/d.svg";
        let got: Vec<(usize, String)> = references(text);
        let want = [(1, "a-1.png"), (1, "b.png"), (2, "c_2.png")].map(|(l, n)| (l, n.to_string()));
        assert_eq!(got, want);
    }
}
