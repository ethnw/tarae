//! `cargo xtask scenes [--out DIR] [scene …]` — tarae's main surfaces as shots, each in both themes (meok and
//! hanji) at 80×24 and 160×45: `<DIR>/<scene>-<dark|light>-<80|160>.png` (default `target/scenes`; with
//! `--transparent`, on the transparent themes, `…-transparent.png`); every scene
//! when none is named, `--list` names them. For a change to the screen: the same scenes before and after, and
//! `--compare BEFORE AFTER` says which shots differ, pixel for pixel, and where.
//!
//! Each shot gets a demo crate of its own — a git repository in a fresh temp folder: one commit, one file changed
//! since, one new — so what one shot saves never shows in the next. Its config is empty but for `editor.lsp =
//! false` (when a server answers would change the picture from run to run) and `llm.command` = this xtask as a
//! stand-in claude, `scenes --claude <script>`: stream-json both ways, a scripted turn, no network, no account.
//! The shot profile's tarae is built once. Each shot runs tarae on a pty (where terminals may be opened).

use std::io::{self, BufRead, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

use crate::shot::{self, Setup};
use crate::{Result, root};

// (`scenes --docs`: the docs' pictures, each from a scene here)
mod docshots;

const USAGE: &str = "usage: cargo xtask scenes [--out DIR] [--size COLSxROWS] [--transparent] [--bin TARAE] [scene …] | scenes --list | scenes --compare A B";

/// One surface: the stand-in claude's script, the files tarae opens, the keys sent (as `cargo xtask shot`
/// takes them: `@1` waits a second, `@?text` until `text` is on screen, `@20?text` at most 20 s).
#[derive(Debug)]
struct Scene {
    name: &'static str,
    claude: &'static str,
    files: &'static [&'static str],
    keys: &'static [&'static str],
}

const MAIN: &[&str] = &["src/main.rs"];

/// The finished turn (`chatdone`'s), in its view mode a line selected (`enter quote` said) — then `enter`: the
/// chip over the input.
const QUOTE_VIEW: &[&str] = &[
    "@1",
    " ",
    "l",
    "@1",
    "why does insert clone the name?",
    "\r",
    "@?Change 1 of 1",
    "@1",
    "y",
    "@20?Nothing else",
    "@1",
    "\x1b",
    "@?normal · chat",
    "x",
    "x",
    // (no wait on `enter quote`: a build from before quoting takes this one too)
    "@1",
];
/// Claude writing src/main.rs in the checkout tarae started in, you in another checkout of the repository with the
/// same file at the lines it writes (`presence` scenes).
macro_rules! presence_keys {
    ($($more:expr),* $(,)?) => {
        &[
            "@1",
            " ",
            "l",
            "@1",
            "why does insert clone the name?",
            "\r",
            "@20?knots\", t.k",
            "\x1b",
            // (another checkout beside it, opened as a session — the shot's own folders, not tarae's worktrees')
            ":",
            "sh git worktree add -q ../fix",
            "\r",
            "@3",
            ":",
            "session-open ../fix",
            "\r",
            "@3",
            ":",
            "e src/main.rs",
            "\r",
            "@1",
            "/",
            "threads.insert",
            "\r",
            "z",
            "z",
            "@2",
            $($more),*
        ]
    };
}
const PRESENCE: &[&str] = presence_keys!();
const PRESENCE_LIST: &[&str] = presence_keys!(" ", "W", "@2");
const PRESENCE_FILES: &[&str] = presence_keys!(" ", "e", "@2");
const PRESENCE_FOLLOW: &[&str] = presence_keys!(" ", "F", "@2");
// (a third checkout, idle, then back in fix with its chat open: the strip over it, each session's name in its colour,
// main's `working` and its file after it, fix in front in bold)
const PRESENCE_STRIP: &[&str] = presence_keys!(
    ":",
    "sh git worktree add -q ../docs",
    "\r",
    "@3",
    ":",
    "session-open ../docs",
    "\r",
    "@3",
    " ",
    "W",
    "@1",
    "2",
    "@2",
    " ",
    "l",
    "@2"
);
/// The main checkout's Claude asking for a run outside the sandbox (its card), you in another checkout: the card waits
/// behind with main (`presencewaits` scenes).
macro_rules! waits_keys {
    ($($more:expr),* $(,)?) => {
        &[
            "@1",
            ":",
            "sh git worktree add -q ../fix",
            "\r",
            "@3",
            ":",
            "session-open ../fix",
            "\r",
            "@3",
            // (back in main: its Claude asked, its card up)
            " ",
            "W",
            "@1",
            "1",
            "@2",
            " ",
            "l",
            "@1",
            "run the tests",
            "\r",
            "@?Its reason",
            "@1",
            "\x1b",
            // (into fix: main's card waits behind)
            " ",
            "W",
            "@1",
            "2",
            "@2",
            $($more),*
        ]
    };
}
// (fix's chat open: the strip over it — main's `▲ waits` in warning, each name in its colour)
const PRESENCE_WAITS: &[&str] = waits_keys!(" ", "l", "@2");
// (…the list: main's `▲ waits for you` in warning, each name in its colour)
const PRESENCE_WAITS_LIST: &[&str] = waits_keys!(" ", "W", "@2");
// (your cursor at the top, then this checkout's own Claude asked too: two Claudes writing one file — fix's caret and rows
// in its colour at full strength, followed, main's mark and tag in its own, dim; the view on fix's writing, your
// cursor left on line 1)
const PRESENCE_TWO: &[&str] = presence_keys!(
    "g",
    "g",
    " ",
    "l",
    "@1",
    "why does insert clone the name?",
    "\r",
    "@20?knots\", t.k",
    "\x1b",
    "\x1b",
    "@2"
);
/// The line `threads.insert` is on changed and saved in the checkout tarae started in, then another checkout opened as
/// a session and the same line changed its own way and saved there: their changes would conflict (`overlap` scenes,
/// collab C2 — said once the trees are still, a few seconds after the save).
macro_rules! overlap_keys {
    ($($more:expr),* $(,)?) => {
        &[
            "@1",
            "/",
            "threads.insert",
            "\r",
            "A",
            " // main's way",
            "\x1b",
            ":",
            "w",
            "\r",
            "@1",
            ":",
            "sh git worktree add -q ../fix",
            "\r",
            "@3",
            ":",
            "session-open ../fix",
            "\r",
            "@3",
            ":",
            "e src/main.rs",
            "\r",
            "@1",
            "/",
            "threads.insert",
            "\r",
            "A",
            " // fix's way",
            "\x1b",
            ":",
            "w",
            "\r",
            "@12?fix ≈ main",
            "@1",
            $($more),*
        ]
    };
}
// (the toast, once)
const OVERLAP: &[&str] = overlap_keys!();
// (…the strip over fix's chat: `fix ≈ main`, `main ≈ fix`, each name in its colour)
const OVERLAP_STRIP: &[&str] = overlap_keys!(" ", "l", "@2");
// (…the list: `≈ main` on fix's row, `≈ fix` on main's)
const OVERLAP_LIST: &[&str] = overlap_keys!(" ", "W", "@2");
// (…the file tree: `≈m` on src/main.rs — main's initial in its colour — and the row in hand's words at its foot)
const OVERLAP_FILES: &[&str] = overlap_keys!(" ", "e", "@?main.rs", "@1");
// (collab C2b: …and fix's `println!` line changed too, unsaved, then `:merged` — the conflict side by side where it's
// wide, stacked at 80; fix's change in its colour, `fix unsaved` in the path bar)
const MERGED: &[&str] = overlap_keys!(
    "/",
    "println",
    "\r",
    "A",
    " // and fix's",
    "\x1b",
    ":",
    "merged",
    "\r",
    "@10?conflict 1 of 1",
    "@1"
);
/// Three checkouts changing `src/main.rs` far apart — main's `threads.insert` line saved, docs's `println!` saved, fix's
/// `mod thread` unsaved — then `:merged` in fix: a clean merge, each change in its session's colour, its prefix in the
/// sign column (`mergedclean`, collab C2b); the cursor on fix's change, `‹fix›` at its end.
const MERGED_CLEAN: &[&str] = &[
    "@1",
    "/",
    "threads.insert",
    "\r",
    "A",
    " // main's way",
    "\x1b",
    ":",
    "w",
    "\r",
    "@1",
    ":",
    "sh git worktree add -q ../fix && git worktree add -q ../docs",
    "\r",
    "@3",
    ":",
    "session-open ../docs",
    "\r",
    "@3",
    ":",
    "e src/main.rs",
    "\r",
    "@1",
    "/",
    "println",
    "\r",
    "A",
    " // docs' way",
    "\x1b",
    ":",
    "w",
    "\r",
    "@1",
    ":",
    "session-open ../fix",
    "\r",
    "@3",
    ":",
    "e src/main.rs",
    "\r",
    "@1",
    "/",
    "mod thread",
    "\r",
    "A",
    " // fix's way",
    "\x1b",
    ":",
    "merged",
    "\r",
    "@10?docs' way",
    "/",
    "fix's way",
    "\r",
    ";",
    "@1",
];

const QUOTE_CHIP: &[&str] = &[
    "@1",
    " ",
    "l",
    "@1",
    "why does insert clone the name?",
    "\r",
    "@?Change 1 of 1",
    "@1",
    "y",
    "@20?Nothing else",
    "@1",
    "\x1b",
    "@?normal · chat",
    "x",
    "x",
    "@?enter quote",
    "\r",
    "@?goes with it",
    "@1",
];

/// The finished turn, in its view mode the paragraph that wraps found and selected with `x` (a paragraph is a line).
const TRANSCRIPT_SELECT: &[&str] = &[
    "@1",
    " ",
    "l",
    "@1",
    "why does insert clone the name?",
    "\r",
    "@?Change 1 of 1",
    "@1",
    "y",
    "@20?Nothing else",
    "@1",
    "\x1b",
    "@?normal · chat",
    "/",
    "cloned because",
    "\r",
    "x",
    "@1",
];
/// The same, a search for a phrase long enough to cross a wrap at both widths.
const TRANSCRIPT_SEARCH: &[&str] = &[
    "@1",
    " ",
    "l",
    "@1",
    "why does insert clone the name?",
    "\r",
    "@?Change 1 of 1",
    "@1",
    "y",
    "@20?Nothing else",
    "@1",
    "\x1b",
    "@?normal · chat",
    "/",
    "moves into the map in the same call. Keying by the name",
    "\r",
    "@1",
];
/// `QUOTE_CHIP`'s chip, then a question of two lines typed under it (`A-ret` between them).
const INPUT_LINES: &[&str] = &[
    "@1",
    " ",
    "l",
    "@1",
    "why does insert clone the name?",
    "\r",
    "@?Change 1 of 1",
    "@1",
    "y",
    "@20?Nothing else",
    "@1",
    "\x1b",
    "@?normal · chat",
    "x",
    "x",
    "@?enter quote",
    "\r",
    "@?goes with it",
    "and if a name repeats?",
    "\x1b\r",
    "keep the first strand",
    "@1",
];

/// The finished turn (`chatdone`'s), then the keys to the code: one `esc` (before the chat's view mode), two.
const CURSOR_TURN_ESC1: &[&str] = &[
    "@1",
    " ",
    "l",
    "@1",
    "why does insert clone the name?",
    "\r",
    "@?Change 1 of 1",
    "@1",
    "y",
    "@20?Nothing else",
    "@1",
    "\x1b",
    "@1",
];
const CURSOR_TURN_ESC2: &[&str] = &[
    "@1",
    " ",
    "l",
    "@1",
    "why does insert clone the name?",
    "\r",
    "@?Change 1 of 1",
    "@1",
    "y",
    "@20?Nothing else",
    "@1",
    "\x1b",
    "@?normal · chat",
    "\x1b",
    "@1",
];

const SCENES: &[Scene] = &[
    Scene { name: "start", claude: "idle", files: &[], keys: &["@?Open a file", "@1"] },
    Scene { name: "code", claude: "idle", files: MAIN, keys: &["@1", " ", "e", "@?thread.rs", "@1"] },
    Scene { name: "picker", claude: "idle", files: MAIN, keys: &["@1", " ", "f", "@?README", "@1"] },
    // (an error, then a save: two toasts in one card)
    Scene {
        name: "toast",
        claude: "idle",
        files: MAIN,
        keys: &["@1", ":", "frobnicate", "\r", "@1", ":", "w", "\r", "@1"],
    },
    Scene { name: "git", claude: "idle", files: MAIN, keys: &["@1", " ", "g", "@?knot.rs", "@1"] },
    // (the run panels' output with the keys: a task's, two lines of it selected — its settings trusted first)
    Scene {
        name: "output-task",
        claude: "idle",
        files: MAIN,
        keys: &[
            "@?Trust this",
            "@1",
            "y",
            "@1",
            ":",
            "task check",
            "\r",
            "@20?2 warnings",
            "@1",
            "\x17",
            "j",
            "@1",
            "k",
            "k",
            "k",
            "x",
            "x",
            "@1",
        ],
    },
    // (a run still printing, held still under the keys: `k` — what came since is said in its head)
    Scene {
        name: "output-live",
        claude: "idle",
        files: MAIN,
        keys: &[
            "@?Trust this",
            "@1",
            "y",
            "@1",
            ":",
            "task watch",
            "\r",
            "@20?knot 12 tied",
            "\x17",
            "j",
            "@1",
            "k",
            "k",
            "@2",
        ],
    },
    // (a preflight's, its first step's heading at the top: `gg`)
    Scene {
        name: "output-preflight",
        claude: "idle",
        files: MAIN,
        keys: &[
            "@?Trust this",
            "@1",
            "y",
            "@1",
            ":",
            "preflight",
            "\r",
            "@30?test result",
            "@1",
            "\x17",
            "j",
            "@1",
            "g",
            "g",
            "@1",
        ],
    },
    // (the tests' failure, searched: `/left`, `n` — a real `cargo test` of the demo's)
    Scene {
        name: "output-tests",
        claude: "idle",
        files: MAIN,
        keys: &["@1", " ", "x", "f", "@90?right: 8", "@1", "\x17", "j", "@1", "/", "left", "\r", "n", "@1"],
    },
    // (panes P3: the tests group under the code while the code has the keys — where the two meet)
    Scene {
        name: "panes-tests-code",
        claude: "idle",
        files: MAIN,
        keys: &["@1", " ", "x", "f", "@90?right: 8", "@1"],
    },
    // (two tabs in the slot — the task's, the tests' in front — between the side panel and the chat: the joins on the
    // meeting line. The task first: a task started gives a finished test run's panel over)
    Scene {
        name: "panes-task-tabs",
        claude: "idle",
        files: MAIN,
        keys: &[
            "@?Trust this",
            "@1",
            "y",
            "@1",
            ":",
            "task check",
            "\r",
            "@20?2 warnings",
            "@1",
            " ",
            "x",
            "f",
            "@90?right: 8",
            "@1",
            " ",
            "e",
            "@1",
            "\x1b",
            " ",
            "l",
            "@1",
            "\x1b",
            "@1",
        ],
    },
    // (panes P3 step 3: a task's places beside its output — the second selected, its line in sight)
    Scene {
        name: "panes-task-places",
        claude: "idle",
        files: MAIN,
        keys: &[
            "@?Trust this",
            "@1",
            "y",
            "@1",
            ":",
            "task check",
            "\r",
            "@20?2 warnings",
            "@1",
            "\x17",
            "j",
            "@1",
            "\x1b[Z",
            "@1",
            "j",
            "@1",
        ],
    },
    // (…and a preflight's steps beside its output — the failed one selected, its heading at the top)
    Scene {
        name: "panes-preflight-steps",
        claude: "idle",
        files: MAIN,
        keys: &[
            "@?Trust this",
            "@1",
            "y",
            "@1",
            ":",
            "preflight",
            "\r",
            "@30?test result",
            "@1",
            "\x17",
            "j",
            "@1",
            "\x1b[Z",
            "@1",
            "j",
            "j",
            "@1",
        ],
    },
    // (panes P3 step 3b: each group with its list's keys, then its detail's — which member's name and stretch of the
    // line are lit)
    Scene {
        name: "group-tests-list",
        claude: "idle",
        files: MAIN,
        keys: &["@1", " ", "x", "f", "@90?right: 8", "@1", "\x17", "j", "@1", "\x1b[Z", "@1"],
    },
    Scene {
        name: "group-tests-detail",
        claude: "idle",
        files: MAIN,
        keys: &["@1", " ", "x", "f", "@90?right: 8", "@1", "\x17", "j", "@1", "\x1b[Z", "@1", "\t", "@1"],
    },
    Scene {
        name: "group-task-detail",
        claude: "idle",
        files: MAIN,
        keys: &[
            "@?Trust this",
            "@1",
            "y",
            "@1",
            ":",
            "task check",
            "\r",
            "@20?2 warnings",
            "@1",
            "\x17",
            "j",
            "@1",
            "\x1b[Z",
            "@1",
            "j",
            "@1",
            "\t",
            "@1",
        ],
    },
    Scene {
        name: "group-preflight-detail",
        claude: "idle",
        files: MAIN,
        keys: &[
            "@?Trust this",
            "@1",
            "y",
            "@1",
            ":",
            "preflight",
            "\r",
            "@30?test result",
            "@1",
            "\x17",
            "j",
            "@1",
            "\x1b[Z",
            "@1",
            "j",
            "j",
            "@1",
            "\t",
            "@1",
        ],
    },
    // (…and with the side panel up: at 80 too narrow for both members — stacked, the list's keys; the tests' detail's)
    Scene {
        name: "group-task-stacked",
        claude: "idle",
        files: MAIN,
        keys: &[
            "@?Trust this",
            "@1",
            "y",
            "@1",
            ":",
            "task check",
            "\r",
            "@20?2 warnings",
            "@1",
            " ",
            "e",
            "@1",
            "\x1b",
            "@1",
            "\x17",
            "j",
            "@1",
        ],
    },
    Scene {
        name: "group-tests-stacked",
        claude: "idle",
        files: MAIN,
        keys: &[
            "@1",
            " ",
            "x",
            "f",
            "@90?right: 8",
            "@1",
            " ",
            "e",
            "@1",
            "\x1b",
            "@1",
            "\x17",
            "j",
            "@1",
            "\t",
            "@1",
        ],
    },
    // (panes polish: a preflight under way, its test step printing — the side panel up, so at 80 its group is
    // stacked, at 160 side by side; the steps' list with the keys: the running row's mark one cell, its time whole)
    Scene {
        name: "polish-preflight-list",
        claude: "idle",
        files: MAIN,
        keys: &[
            "@?Trust this",
            "@1",
            "y",
            "@1",
            ":",
            "preflight",
            "\r",
            "@30?tests::knot_5 ",
            "@1",
            " ",
            "e",
            "@1",
            "\x1b",
            "@1",
            "\x17",
            "j",
            "@4",
        ],
    },
    // (…its output in front: stacked at 80, as tall as the output)
    Scene {
        name: "polish-preflight-output",
        claude: "idle",
        files: MAIN,
        keys: &[
            "@?Trust this",
            "@1",
            "y",
            "@1",
            ":",
            "preflight",
            "\r",
            "@30?tests::knot_5 ",
            "@1",
            " ",
            "e",
            "@1",
            "\x1b",
            "@1",
            "\x17",
            "j",
            "@1",
            "\t",
            "@3",
        ],
    },
    // (…a task under way: the wave once, by its name; the time it has run in the details)
    Scene {
        name: "polish-task-running",
        claude: "idle",
        files: MAIN,
        keys: &["@?Trust this", "@1", "y", "@1", ":", "task watch", "\r", "@20?knot 12 tied", "@1"],
    },
    // (a floating frame over the tests group — which-key's top line beside the slot's)
    Scene {
        name: "panes-float",
        claude: "idle",
        files: MAIN,
        keys: &["@1", " ", "x", "f", "@90?right: 8", "@1", " ", "@1"],
    },
    // (Claude's answers first, each its own key — a read `a` this chat, a host through `also` `s` this session, a
    // read `A` always — then the list, the hand on its last row (`G`: at 24 rows the defaults fill it), the `a`. The
    // code zoomed first (`space w z`): at 80 the chat beside it would cut the cards and fold the list's stamps. The
    // host's card needs the sandbox: run outside one)
    Scene {
        name: "granted",
        claude: "grants",
        files: MAIN,
        keys: &[
            "@1",
            " ",
            "l",
            "@1",
            "read the notes the strands follow",
            "\r",
            "@1",
            "\x1b",
            "@1",
            " ",
            "w",
            "z",
            "@20?Claude wants to read",
            "@1",
            "\x11",
            "@1",
            "a",
            "@20?Open the sandbox a little",
            "@1",
            "\x11",
            "@1",
            "s",
            "@20?Claude wants to read",
            "@1",
            "\x11",
            "@1",
            "A",
            "@3",
            ":",
            "granted",
            "\r",
            "@1",
            "G",
            "@1",
        ],
    },
    // (the demo repository's settings trusted on their card — its `.tarae.toml` and a git command of its own — then
    // the list)
    Scene {
        name: "trusted",
        claude: "idle",
        files: MAIN,
        keys: &["@?Trust this", "@1", "y", "@1", ":", "granted", "\r", "@1"],
    },
    // (the answer stops mid code block: the follow card, then the panel itself — `C-f` leaves follow)
    Scene {
        name: "chatmid",
        claude: "mid",
        files: MAIN,
        keys: &["@1", " ", "l", "@1", "why does insert clone the name?", "\r", "@?entry(first", "@1"],
    },
    // (the panel, its answer stopping in a list after a quote — a line in a script without spaces at its head)
    Scene {
        name: "chatpanel",
        claude: "midlist",
        files: MAIN,
        keys: &[
            "@1",
            " ",
            "l",
            "@1",
            "why does insert clone the name?",
            "\r",
            "@?keeps the first",
            "\x06",
            "@1",
        ],
    },
    // (the panel mid-turn, a stop in it: the answer under way after its read — `C-f` leaves follow, the panel back)
    Scene {
        name: "chatthread",
        claude: "mid",
        files: MAIN,
        keys: &["@1", " ", "l", "@1", "why does insert clone the name?", "\r", "@?entry(first", "\x06", "@1"],
    },
    // (the follow card beside the panel: mid-turn, the panel brought back — `esc`, `space l`)
    Scene {
        name: "cardpanel",
        claude: "mid",
        files: MAIN,
        keys: &[
            "@1",
            " ",
            "l",
            "@1",
            "why does insert clone the name?",
            "\r",
            "@?entry(first",
            "\x1b",
            " ",
            "l",
            "@1",
        ],
    },
    // (the follow card replaying the turn once it's done: `[r` in the code, its newest stop)
    Scene {
        name: "cardreplay",
        claude: "full",
        files: MAIN,
        keys: &[
            "@1",
            " ",
            "l",
            "@1",
            "why does insert clone the name?",
            "\r",
            "@?Change 1 of 1",
            "@1",
            "y",
            "@20?Nothing else",
            "@1",
            "\x1b",
            "[",
            "r",
            "@?stop by stop",
            "@1",
        ],
    },
    // (`space i` on a line: its card while the answer is written)
    Scene {
        name: "ask",
        claude: "ask",
        files: MAIN,
        keys: &["@1", "9", "G", "x", " ", "i", "@1", "keep the first strand", "\r", "@?claude writing", "@1"],
    },
    // (Claude's edit: its review; accepted and the turn's end; then `space v t` on that turn)
    Scene {
        name: "review",
        claude: "full",
        files: MAIN,
        keys: &["@1", " ", "l", "@1", "why does insert clone the name?", "\r", "@?Change 1 of 1", "@1"],
    },
    // (글타래 P7: Claude's edit of three changes as it's written — two whole, the third's new line mid-way, its head
    // wet; nothing applied, no review yet)
    Scene {
        name: "writing",
        claude: "writemid",
        files: MAIN,
        keys: &["@1", " ", "l", "@1", "why does insert clone the name?", "\r", "@20?knots\", t.k", "@1"],
    },
    // (the same with the keys back in the code: the cursor on the line being rewritten, Claude's caret at the end of
    // what it writes — two places, one cursor)
    Scene {
        name: "writingcode",
        claude: "writemid",
        files: MAIN,
        keys: &[
            "@1",
            " ",
            "l",
            "@1",
            "why does insert clone the name?",
            "\r",
            "@20?knots\", t.k",
            "\x1b",
            "@1",
        ],
    },
    // (Claude's edit of three changes: the first accepted, the second refused, the third in hand)
    Scene {
        name: "decided",
        claude: "three",
        files: MAIN,
        keys: &[
            "@1",
            " ",
            "l",
            "@1",
            "why does insert clone the name?",
            "\r",
            "@?Change 1 of 3",
            "@1",
            "y",
            "@1",
            "n",
            "@1",
        ],
    },
    Scene {
        name: "chatdone",
        claude: "full",
        files: MAIN,
        keys: &[
            "@1",
            " ",
            "l",
            "@1",
            "why does insert clone the name?",
            "\r",
            "@?Change 1 of 1",
            "@1",
            "y",
            "@20?Nothing else",
            "@1",
        ],
    },
    // (scrolling alike, uxaudit D7: the finished turn, `C-u` from the empty input — half the rows in sight)
    Scene {
        name: "scroll-chat",
        claude: "full",
        files: MAIN,
        keys: &[
            "@1",
            " ",
            "l",
            "@1",
            "why does insert clone the name?",
            "\r",
            "@?Change 1 of 1",
            "@1",
            "y",
            "@20?Nothing else",
            "@1",
            "\x15",
            "@1",
        ],
    },
    // (Z2: the terminal's past by `S-pageup` while the shell has the keys — every terminal emulator's key)
    Scene {
        name: "scroll-term",
        claude: "idle",
        files: MAIN,
        keys: &["@1", " ", "T", "@3", "seq 1 300", "\r", "@?298", "@1", "\x1b[5;2~", "@1"],
    },
    // (D7: the wheel over `:procs` scrolls it, the selection stays — filling the screen, `C-w z`, so the wheel's
    // row is the panel's at 80×24 and 160×45 alike; a shell first, a row more)
    Scene {
        name: "scroll-procs",
        claude: "idle",
        files: MAIN,
        keys: &[
            "@1",
            " ",
            "T",
            "@3",
            "\x1c",
            "@1",
            ":procs",
            "\r",
            "@2",
            "\x17",
            "z",
            "@1",
            "\x1b[<65;20;8M",
            "@1",
        ],
    },
    // (panes P3 step 4: `:procs` as a group — the table with the keys, a shell's row selected; at 80 stacked)
    Scene {
        name: "group-procs-list",
        claude: "idle",
        files: MAIN,
        keys: &["@1", " ", "T", "@3", "\x1c", "@1", "\x1b", "@1", ":procs", "\r", "@2", "G", "@1"],
    },
    // (…and its detail with the keys: what that row is — its command line whole, its parents up to tarae)
    Scene {
        name: "group-procs-detail",
        claude: "idle",
        files: MAIN,
        keys: &[
            "@1", " ", "T", "@3", "\x1c", "@1", "\x1b", "@1", ":procs", "\r", "@2", "G", "@1", "\t", "@1",
        ],
    },
    // (panes P3 step 5: the git tab with a changed file picked — `thread.rs`, its changes inline in the code while
    // the tab has the keys)
    Scene {
        name: "group-git-file",
        claude: "idle",
        files: MAIN,
        keys: &["@1", " ", "g", "@?thread.rs", "@1", "j", "@1", "k", "@2"],
    },
    // (the review's rail the same: `space v v`, the rail given the keys, `j` onto the next file — shown as the
    // preview, the rail keeping the keys)
    Scene {
        name: "group-rail-preview",
        claude: "idle",
        files: MAIN,
        keys: &["@1", " ", "v", "v", "@?2 files", "@2", " ", "e", "@1", "j", "@2"],
    },
    // (…and `enter` on its row: a buffer of its own — upright in the tabs, its hunks under it)
    Scene {
        name: "group-git-kept",
        claude: "idle",
        files: MAIN,
        keys: &["@1", " ", "g", "@?thread.rs", "@1", "j", "@1", "k", "@2", "\r", "@2"],
    },
    // (…and `esc`: the code has the keys, the index's gutter back)
    Scene {
        name: "group-git-code",
        claude: "idle",
        files: MAIN,
        keys: &["@1", " ", "g", "@?thread.rs", "@1", "j", "@1", "k", "@2", "\x1b", "@1"],
    },
    // (H1, Helix's picker keys: the file picker's hints, then `C-v` — the file opened beside, in a split — and `C-t`,
    // the preview put away)
    Scene {
        name: "picker-hints",
        claude: "idle",
        files: MAIN,
        keys: &["@1", " ", "f", "@?README", "READ", "@1"],
    },
    Scene {
        name: "picker-split",
        claude: "idle",
        files: MAIN,
        keys: &["@1", " ", "f", "@?README", "READ", "@1", "\x16", "@1"],
    },
    Scene {
        name: "picker-nopreview",
        claude: "idle",
        files: MAIN,
        keys: &["@1", " ", "f", "@?README", "READ", "@1", "\x14", "@1"],
    },
    // (the chat's view mode on the finished turn: `esc` from the input, up a few rows, a word and a line selected)
    Scene {
        name: "chatview",
        claude: "full",
        files: MAIN,
        keys: &[
            "@1",
            " ",
            "l",
            "@1",
            "why does insert clone the name?",
            "\r",
            "@?Change 1 of 1",
            "@1",
            "y",
            "@20?Nothing else",
            "@1",
            "\x1b",
            "@?normal · chat",
            "k",
            "k",
            "x",
            "w",
            "@1",
        ],
    },
    // (one cursor on screen, where the keys go: the finished turn, then the keys in the code — `esc` before view
    // mode, `esc esc` with it (`-1`/`-2`) —, in the chat's view mode, in the terminal's program from the code)
    // (view mode for a first-timer: the space menu's way in, a key it has no use for said with both ways on,
    // `y`'s copy said)
    Scene { name: "view-space", claude: "idle", files: MAIN, keys: &["@1", " ", "@?Close chat", "@1"] },
    // (an empty conversation: nothing to view — its input's hint row says `esc code`)
    Scene { name: "view-empty", claude: "idle", files: MAIN, keys: &["@1", " ", "l", "@?Ask about", "@1"] },
    Scene {
        name: "view-said",
        claude: "full",
        files: MAIN,
        keys: &[
            "@1",
            " ",
            "l",
            "@1",
            "why does insert clone the name?",
            "\r",
            "@?Change 1 of 1",
            "@1",
            "y",
            "@20?Nothing else",
            "@1",
            "\x1b",
            "@?normal · chat",
            "d",
            "@?read-only",
            "@1",
        ],
    },
    Scene {
        name: "view-copy",
        claude: "full",
        files: MAIN,
        keys: &[
            "@1",
            " ",
            "l",
            "@1",
            "why does insert clone the name?",
            "\r",
            "@?Change 1 of 1",
            "@1",
            "y",
            "@20?Nothing else",
            "@1",
            "\x1b",
            "@?normal · chat",
            "x",
            "y",
            "@?copied",
            "@1",
        ],
    },
    // (view P2, quoting: a selection on the conversation and its `enter quote` hint; `enter`'s chip over the input;
    // `space >` on the code's lines; the message sent with it, its chip a link; the terminal's rows quoted)
    Scene { name: "quote-view", claude: "full", files: MAIN, keys: QUOTE_VIEW },
    Scene { name: "quote-chip", claude: "full", files: MAIN, keys: QUOTE_CHIP },
    // (panes P2c: the transcript a document — `x` on a wrapped paragraph, a search for a phrase across a wrap, the
    // input a buffer of two lines under a quote's chip)
    Scene { name: "transcript-select", claude: "full", files: MAIN, keys: TRANSCRIPT_SELECT },
    Scene { name: "transcript-search", claude: "full", files: MAIN, keys: TRANSCRIPT_SEARCH },
    Scene { name: "input-lines", claude: "full", files: MAIN, keys: INPUT_LINES },
    Scene {
        name: "quote-code",
        claude: "idle",
        files: MAIN,
        keys: &["@1", "j", "j", "x", "x", " ", ">", "@?goes with it", "@1"],
    },
    Scene {
        name: "quote-sent",
        claude: "idle",
        files: MAIN,
        // (`C-f`: not following — the chat stays whole, the message's row in sight)
        keys: &[
            "@1",
            "j",
            "j",
            "x",
            "x",
            " ",
            ">",
            "@?goes with it",
            "what do these do?",
            "\r",
            "@?what do these",
            "\x06",
            "@?L3",
            "@1",
        ],
    },
    // (the terminal's view, rows selected — HEAD's build takes it too — then `enter`'s chip)
    Scene {
        name: "quote-termview",
        claude: "idle",
        files: MAIN,
        keys: &[
            "@1", " ", "T", "@3", "seq 1 40", "\r", "@?40", "@1", "\x1c", "@?view", "k", "k", "x", "x", "@1",
        ],
    },
    Scene {
        name: "quote-term",
        claude: "idle",
        files: MAIN,
        keys: &[
            "@1",
            " ",
            "T",
            "@3",
            "seq 1 40",
            "\r",
            "@?40",
            "@1",
            "\x1c",
            "@?view",
            "k",
            "k",
            "x",
            "x",
            "@?enter quote",
            "\r",
            "@?goes with it",
            "@1",
        ],
    },
    Scene { name: "cursor-code-1", claude: "full", files: MAIN, keys: CURSOR_TURN_ESC1 },
    Scene { name: "cursor-code-2", claude: "full", files: MAIN, keys: CURSOR_TURN_ESC2 },
    Scene {
        name: "cursor-view",
        claude: "full",
        files: MAIN,
        keys: &[
            "@1",
            " ",
            "l",
            "@1",
            "why does insert clone the name?",
            "\r",
            "@?Change 1 of 1",
            "@1",
            "y",
            "@20?Nothing else",
            "@1",
            "\x1b",
            "@?normal · chat",
            "@1",
        ],
    },
    Scene {
        name: "cursor-term-1",
        claude: "full",
        files: MAIN,
        keys: &[
            "@1",
            " ",
            "l",
            "@1",
            "why does insert clone the name?",
            "\r",
            "@?Change 1 of 1",
            "@1",
            "y",
            "@20?Nothing else",
            "@1",
            "\x1b",
            "@1",
            " ",
            "T",
            "@3",
        ],
    },
    Scene {
        name: "cursor-term-2",
        claude: "full",
        files: MAIN,
        keys: &[
            "@1",
            " ",
            "l",
            "@1",
            "why does insert clone the name?",
            "\r",
            "@?Change 1 of 1",
            "@1",
            "y",
            "@20?Nothing else",
            "@1",
            "\x1b",
            "@?normal · chat",
            "\x1b",
            "@1",
            " ",
            "T",
            "@3",
        ],
    },
    Scene {
        name: "turn",
        claude: "full",
        files: MAIN,
        keys: &[
            "@1",
            " ",
            "l",
            "@1",
            "why does insert clone the name?",
            "\r",
            "@?Change 1 of 1",
            "@1",
            "y",
            "@20?Nothing else",
            "@1",
            "\x1b",
            " ",
            "v",
            "t",
            "@2",
        ],
    },
    // (the same turn of three changes, decided — accepted, refused, accepted — then `space v t` on it, its file
    // marked viewed: kept)
    Scene {
        name: "decidedturn",
        claude: "three",
        files: MAIN,
        keys: &[
            "@1",
            " ",
            "l",
            "@1",
            "why does insert clone the name?",
            "\r",
            "@?Change 1 of 3",
            "@1",
            "y",
            "@1",
            "n",
            "@1",
            "y",
            "@20?Nothing else",
            "@1",
            "\x1b",
            " ",
            "v",
            "t",
            "@2",
            " ",
            "v",
            "m",
            "@1",
        ],
    },
    // (keep-warm, `keepwarm.rs`: a big conversation on the 1h cache marked to be kept warm — the chat's last row says
    // the ping ahead; then one asked for now — the row says what it kept and cost, its toast too)
    Scene {
        name: "warmnext",
        claude: "warm",
        files: MAIN,
        keys: &[
            "@1",
            " ",
            "l",
            "@1",
            "how is the map filled?",
            "\r",
            "@?ready for your review",
            "@1",
            "\x1b",
            "@1",
            ":",
            "keep-warm",
            "\r",
            "@?will be kept warm",
            "@1",
        ],
    },
    Scene {
        name: "warmkept",
        claude: "warm",
        files: MAIN,
        keys: &[
            "@1",
            " ",
            "l",
            "@1",
            "how is the map filled?",
            "\r",
            "@?ready for your review",
            "@1",
            "\x1b",
            "@1",
            ":",
            "keep-warm",
            "\r",
            "@?will be kept warm",
            ":",
            "keep-warm now",
            "\r",
            "@?cache kept warm: read",
            "@1",
        ],
    },
    // (a run outside the sandbox: its card)
    Scene {
        name: "card",
        claude: "card",
        files: MAIN,
        keys: &["@1", " ", "l", "@1", "run the tests", "\r", "@?Its reason", "@1"],
    },
    // (panes P2b: the default layout — the side panel, the code, the chat — the keys back on the code by `C-w h`)
    Scene {
        name: "panes-default",
        claude: "idle",
        files: MAIN,
        keys: &["@1", " ", "e", "@1", " ", "l", "@1", "\x17", "h", "@1"],
    },
    // (two code panes side by side: the divider lit beside the one with the keys)
    Scene {
        name: "panes-split",
        claude: "idle",
        files: MAIN,
        keys: &["@1", "\x17", "v", "@1", "8", "j", "@1"],
    },
    // (the tests group: a failing test written and run, `C-w j` to the group, `C-w h` to its list — its title lit)
    Scene {
        name: "panes-tests",
        claude: "idle",
        files: &["src/thread.rs"],
        keys: &[
            "@1",
            "G",
            "o",
            "\r#[test]\rfn ties() {\rassert!(false, \"a knot slipped\");",
            "\x1b",
            ":w\r",
            "@1",
            " ",
            "x",
            "f",
            "@60?FAILURE",
            "@1",
            "\x17",
            "j",
            "@1",
            "\x17",
            "h",
            "@1",
        ],
    },
    // (a card is a pane that asks: `C-q` from the chat gives it the keys — it floats with them, the chat's again after)
    Scene {
        name: "panes-card",
        claude: "card",
        files: MAIN,
        keys: &["@1", " ", "l", "@1", "run the tests", "\r", "@?Its reason", "@1", "\x11", "@1"],
    },
    // (the same run asked again after a yes, the program it runs rebuilt since — git ignores it: the card's "Same as")
    Scene {
        name: "samebinary",
        claude: "samebinary",
        files: MAIN,
        keys: &[
            "@1",
            " ",
            "l",
            "@1",
            "run the probe",
            "\r",
            "@?Its reason",
            "@1",
            "\x11",
            "@1",
            "y",
            "@20?Same as card",
            "@1",
        ],
    },
    // (tarae's search of a folder outside the project: its read card, the pattern named)
    Scene {
        name: "searchcard",
        claude: "searchcard",
        files: MAIN,
        keys: &["@1", " ", "l", "@1", "find the card record", "\r", "@?goes to Claude", "@1"],
    },
    // (the board under way: an angle done, two reading, one waiting its turn — then, done, a confirmed finding's
    // card: the angles that found it, its skeptic's trace)
    Scene {
        name: "board",
        claude: "boardmid",
        files: MAIN,
        keys: &["@1", " ", "v", "C", "@?Start a board", "@1", "y", "@?2 found", "@1"],
    },
    // (the card a board asks on first: the session, what boards cost, the angles — frictions32)
    Scene {
        name: "boardcard",
        claude: "boardmid",
        files: MAIN,
        keys: &["@1", " ", "v", "C", "@?Start a board", "@1"],
    },
    // (the same on a narrow screen, the rail given the keys: the four angles folded into rows of marks —
    // `--size 74x30` is where it folds)
    Scene {
        name: "boardnarrow",
        claude: "boardmid",
        files: MAIN,
        keys: &[
            "@1",
            " ",
            "v",
            "C",
            "@?Start a board",
            "@1",
            "y",
            "@?the board reads",
            "@3",
            " ",
            "e",
            "@?skeptics",
            "@1",
        ],
    },
    Scene {
        name: "boardnote",
        claude: "boarddone",
        files: MAIN,
        keys: &["@1", " ", "v", "C", "@?Start a board", "@1", "y", "@30?the board is done", "@2"],
    },
    // (bgreview: the reviewer in the background, no review open — the status line says it: reading, then what it
    // found and the key that reads it; the board's head; then a review opened after a file changed, its rail's
    // `read … at` row)
    Scene {
        name: "bgreviewing",
        claude: "reviewing",
        files: MAIN,
        keys: &["@1", " ", "v", "c", "@?3 reads", "@3"],
    },
    Scene {
        name: "bgreviewdone",
        claude: "reviewdone",
        files: MAIN,
        keys: &["@1", " ", "v", "c", "@30?1 finding · space v r", "@3"],
    },
    Scene {
        name: "bgboard",
        claude: "boardmid",
        files: MAIN,
        keys: &["@1", " ", "v", "C", "@?Start a board", "@1", "y", "@?¶ board", "@3"],
    },
    // (two sessions, each its reviewer reading: the one in front says it on the status line, its tab without its
    // `¶` — the user's decision 2026-10-20 — the one behind on its tab)
    Scene {
        name: "bgsessions",
        claude: "reviewing",
        files: MAIN,
        keys: &[
            "@1",
            " ",
            "v",
            "c",
            "@?3 reads",
            // (another checkout beside it, opened as a session — the shot's own folders, not tarae's worktrees')
            ":",
            "sh git worktree add -q ../fix",
            "\r",
            "@3",
            ":",
            "session-open ../fix",
            "\r",
            "@3",
            ":",
            "e notes.txt",
            "\r",
            "i",
            "a note",
            "\x1b",
            ":",
            "w",
            "\r",
            "@1",
            " ",
            "v",
            "c",
            "@?3 reads",
            " ",
            "W",
            "@1",
            "1",
            "@1",
            " ",
            "l",
            "@?¶ reviewing",
            "@2",
        ],
    },
    // (collab C1: the main checkout's Claude writing src/main.rs while you read the same file in another checkout
    // — its lines marked there, its name tag at the first)
    Scene { name: "presence", claude: "writemid", files: MAIN, keys: PRESENCE },
    // (…the list says where each session is)
    Scene { name: "presencelist", claude: "writemid", files: MAIN, keys: PRESENCE_LIST },
    // (…the file tree marks the file another session is in)
    Scene { name: "presencefiles", claude: "writemid", files: MAIN, keys: PRESENCE_FILES },
    // (…`space F`: following the next session at work takes you into its checkout, `● follow <name>`)
    Scene { name: "presencefollow", claude: "writemid", files: MAIN, keys: PRESENCE_FOLLOW },
    // (…the sessions' strip over a chat: every session's name in its colour, what it does after it)
    Scene { name: "presencestrip", claude: "writemid", files: MAIN, keys: PRESENCE_STRIP },
    // (…a session waiting on you behind — its card parked: the strip and the list say it in warning)
    Scene { name: "presencewaits", claude: "card", files: MAIN, keys: PRESENCE_WAITS },
    Scene { name: "presencewaitslist", claude: "card", files: MAIN, keys: PRESENCE_WAITS_LIST },
    // (…two Claudes writing one file in two checkouts, each its colour; follow holds the view, your cursor elsewhere)
    Scene { name: "presencetwo", claude: "writemid", files: MAIN, keys: PRESENCE_TWO },
    // (collab C2: the same line changed and saved two ways in two checkouts — the toast, the strip, the list, the tree)
    Scene { name: "overlap", claude: "writemid", files: MAIN, keys: OVERLAP },
    Scene { name: "overlapstrip", claude: "writemid", files: MAIN, keys: OVERLAP_STRIP },
    Scene { name: "overlaplist", claude: "writemid", files: MAIN, keys: OVERLAP_LIST },
    Scene { name: "overlapfiles", claude: "writemid", files: MAIN, keys: OVERLAP_FILES },
    // (collab C2b: the merged view — a conflict side by side and stacked, and a clean merge of three in their colours)
    Scene { name: "merged", claude: "writemid", files: MAIN, keys: MERGED },
    Scene { name: "mergedclean", claude: "writemid", files: MAIN, keys: MERGED_CLEAN },
    // (the docs' `claude-reviews.png`: the reviewer read in the background — three findings on thread.rs's lines,
    // one's card open under it, its report beside it in a split (`space v r` — read, the status line's
    // `¶ 3 findings` goes, as decided))
    Scene {
        name: "bgreviewnote",
        claude: "reviewfull",
        files: MAIN,
        keys: &[
            "@1",
            " ",
            "v",
            "c",
            "@30?3 findings · space v r",
            ":",
            "e src/thread.rs",
            "\r",
            "@?Ties a knot",
            ":",
            "vsplit",
            "\r",
            "@1",
            " ",
            "v",
            "r",
            "@?Verdict",
            "\x17",
            "h",
            "@1",
            ":",
            "15",
            "\r",
            "@8",
        ],
    },
    // (the same without the report: the findings on their lines, one's card, the status line's `¶ 3 findings ▲ ·
    // space v r` — the docs' picture's other candidate)
    Scene {
        name: "bgreviewfindings",
        claude: "reviewfull",
        files: MAIN,
        keys: &[
            "@1",
            " ",
            "v",
            "c",
            "@30?3 findings · space v r",
            ":",
            "e src/thread.rs",
            "\r",
            "@?Ties a knot",
            ":",
            "15",
            "\r",
            "@8",
        ],
    },
    Scene {
        name: "bgreviewrail",
        claude: "reviewdone",
        files: MAIN,
        keys: &[
            "@1",
            " ",
            "v",
            "c",
            "@30?1 finding · space v r",
            "@1",
            "g",
            "g",
            "O",
            "/",
            "/",
            " ",
            "x",
            "\x1b",
            ":",
            "w",
            "\r",
            "@1",
            " ",
            "v",
            "v",
            "@?changed since",
            "@1",
        ],
    },
    // (shuttle keys, `shuttle.rs`: a program started from the terminal panel says hello — its card; `y`, and its
    // step lit on the status line's chip; then the person's key, and the chip says it's paused)
    Scene { name: "shuttlecard", claude: "idle", files: MAIN, keys: SHUTTLE_HELLO },
    Scene {
        name: "shuttle",
        claude: "idle",
        files: MAIN,
        keys: &[
            "@1",
            " ",
            "T",
            "@?terminal",
            SHUTTLE_RUN,
            "\r",
            "\x1c",
            "@?asks to type",
            "@1",
            "y",
            "@?k → pane",
        ],
    },
    // (…and the same once its step went out: the chip as it was, nothing beside it moved)
    Scene {
        name: "shuttleunlit",
        claude: "idle",
        files: MAIN,
        keys: &[
            "@1",
            " ",
            "T",
            "@?terminal",
            SHUTTLE_RUN,
            "\r",
            "\x1c",
            "@?asks to type",
            "@1",
            "y",
            "@?k → pane",
            "@!k → pane",
        ],
    },
    Scene {
        name: "shuttlepaused",
        claude: "idle",
        files: MAIN,
        keys: &[
            "@1",
            " ",
            "T",
            "@?terminal",
            SHUTTLE_RUN,
            "\r",
            "\x1c",
            "@?asks to type",
            "@1",
            "y",
            "@?j → pane",
            "@3",
            "k",
            "@?paused",
            "@1",
        ],
    },
];

/// The shuttle scenes' program, run in the terminal panel: `shuttle.sh` beside the demo (`one`).
const SHUTTLE_RUN: &str = "sh ../shuttle.sh &";

const SHUTTLE_HELLO: &[&str] =
    &["@1", " ", "T", "@?terminal", SHUTTLE_RUN, "\r", "\x1c", "@?asks to type", "@1"];

/// It waits until the state says the keys are back in a pane (`C-\` left the terminal), and sends `j` there — its
/// hello brings the card first; then, once no toast is up (the connect toast gone), `k`: the step lit alone, for
/// the lit and the unlit shots. Its grant goes to the terminal's shell (`--for $PPID`), which lives on.
fn shuttle_sh(bin: &Path) -> String {
    let bin = bin.to_string_lossy().replace('\'', "'\\''");
    format!(
        "to=\nwhile [ -z \"$to\" ]; do\n  sleep 0.2\n  to=$('{bin}' state | sed -n 's/^keys → \\(pane#[0-9]*\\).*/\\1/p')\ndone\n\
         '{bin}' keys --for $PPID --name claude-code --expect-keys \"$to\" j >/dev/null\n\
         until '{bin}' state --json | grep -q '\"toasts\":\\[\\]'; do sleep 0.2; done\n\
         exec '{bin}' keys --for $PPID --name claude-code --expect-keys \"$to\" k >/dev/null\n"
    )
}

/// `scenes --compare A B`: each shot in `A` against `B`'s of its name, pixel for pixel — `same`, or the box the
/// differing pixels lie in, to look at there (a turn's time or a temp path moves from run to run).
fn compare(a: &Path, b: &Path) -> Result<()> {
    let mut names: Vec<String> = std::fs::read_dir(a)
        .map_err(|e| format!("{}: {e}", a.display()))?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.ends_with(".png"))
        .collect();
    names.sort();
    let (mut same, mut unreadable) = (0, 0);
    for n in &names {
        let c = compared(a, b, n);
        same += usize::from(c == Compared::Same);
        unreadable += usize::from(matches!(c, Compared::Unreadable(_)));
        println!("{n}: {}", c.said());
    }
    println!("{same} of {} the same", names.len());
    match unreadable {
        0 => Ok(()),
        n => Err(format!("{n} couldn't be read as pictures")),
    }
}

/// How a shot compares with its twin of the same name.
#[derive(Debug, PartialEq)]
enum Compared {
    Same,
    /// What differs: the box the differing pixels lie in, or the two sizes.
    Differs(String),
    /// No twin.
    New,
    /// One of the two couldn't be read as a PNG: which, and why.
    Unreadable(String),
}

impl Compared {
    fn said(&self) -> String {
        match self {
            Compared::Same => "same".into(),
            Compared::Differs(how) => format!("differs {how}"),
            Compared::New => "new — none to compare it with".into(),
            Compared::Unreadable(why) => format!("can't be compared: {why}"),
        }
    }
}

/// Shot `n` in `a` against its twin in `b`, pixel for pixel.
fn compared(a: &Path, b: &Path, n: &str) -> Compared {
    compared_but(a, b, n, &|_, _, _, _| false)
}

/// `compared`, passing over the pixels `left_out(x, y, w, h)` names in a `w`×`h` shot (a docs row's cells that may
/// differ between takes — `docshots.rs`'s `varies`).
fn compared_but(
    a: &Path,
    b: &Path,
    n: &str,
    left_out: &dyn Fn(usize, usize, usize, usize) -> bool,
) -> Compared {
    let read = |p: &Path| -> Result<(u32, u32, Vec<u8>)> {
        let file = std::fs::File::open(p).map_err(|e| format!("{}: {e}", p.display()))?;
        let mut r = png::Decoder::new(io::BufReader::new(file))
            .read_info()
            .map_err(|e| format!("{}: {e}", p.display()))?;
        let mut buf = vec![0; r.output_buffer_size().ok_or("too big a shot")?];
        let info = r.next_frame(&mut buf).map_err(|e| format!("{}: {e}", p.display()))?;
        buf.truncate(info.buffer_size());
        Ok((info.width, info.height, buf))
    };
    if !b.join(n).exists() {
        return Compared::New;
    }
    let (x, y) = match (read(&a.join(n)), read(&b.join(n))) {
        (Ok(x), Ok(y)) => (x, y),
        (Err(e), _) | (_, Err(e)) => return Compared::Unreadable(e),
    };
    if (x.0, x.1) != (y.0, y.1) {
        return Compared::Differs(format!("in size: {}×{} against {}×{}", x.0, x.1, y.0, y.1));
    }
    let (w, h, px) = (x.0 as usize, x.1 as usize, x.2.len() / (x.0 * x.1).max(1) as usize);
    let mut boxed: Option<(usize, usize, usize, usize)> = None;
    for (i, (p, q)) in x.2.chunks(px.max(1)).zip(y.2.chunks(px.max(1))).enumerate() {
        let (cx, cy) = (i % w, i / w);
        if p != q && !left_out(cx, cy, w, h) {
            let (x0, y0, x1, y1) = boxed.unwrap_or((cx, cy, cx, cy));
            boxed = Some((x0.min(cx), y0.min(cy), x1.max(cx), y1.max(cy)));
        }
    }
    match boxed {
        None => Compared::Same,
        Some((x0, y0, x1, y1)) => {
            Compared::Differs(format!("in x {x0}..={x1}, y {y0}..={y1} (of {}×{})", x.0, x.1))
        }
    }
}

const SIZES: [(u16, u16); 2] = [(80, 24), (160, 45)];
const BACKGROUNDS: [&str; 2] = ["dark", "light"];

pub fn run(args: &[String]) -> Result<()> {
    if args.first().is_some_and(|a| a == "--claude") {
        let script = args.get(1).ok_or(USAGE)?;
        // (a ping's fork of the conversation — `keepwarm.rs` — answers as one)
        let fork = args.iter().any(|a| a == "--fork-session");
        let script = if fork && script == "warm" { "warmfork" } else { script.as_str() };
        let stdin = io::stdin();
        let pause = Duration::from_millis(40);
        return claude(script, stdin.lock(), &mut io::stdout().lock(), pause)
            .map_err(|e| format!("the stand-in claude: {e}"));
    }
    if args.first().is_some_and(|a| a == "--compare") {
        let [_, a, b] = args else { return Err(USAGE.into()) };
        return compare(Path::new(a), Path::new(b));
    }
    if args.first().is_some_and(|a| a == "--docs") {
        return docshots::run(&args[1..]);
    }
    let (mut out, mut names) = (root().join("target/scenes"), Vec::new());
    let mut sizes = SIZES.to_vec();
    let mut transparent = false;
    let mut bin: Option<PathBuf> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--list" => {
                SCENES.iter().for_each(|s| println!("{}", s.name));
                return Ok(());
            }
            "--out" => out = PathBuf::from(it.next().ok_or(USAGE)?),
            // (one size instead of the two: a width where something folds)
            "--size" => {
                let s = it.next().ok_or(USAGE)?;
                let (c, r) = s.split_once('x').ok_or(USAGE)?;
                sizes = vec![(c.parse().map_err(|_| USAGE)?, r.parse().map_err(|_| USAGE)?)];
            }
            // (the transparent themes: what has no background of its own)
            "--transparent" => transparent = true,
            // (another build of tarae — main's, for the shots before a change: A/B)
            "--bin" => bin = Some(PathBuf::from(it.next().ok_or(USAGE)?)),
            n if !n.starts_with('-') => names.push(n.to_string()),
            _ => return Err(USAGE.into()),
        }
    }
    let scenes = chosen(&names)?;
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let bin = match bin {
        Some(b) if b.is_absolute() && b.is_file() => b,
        Some(b) => return Err(format!("--bin {}: an absolute path to a tarae", b.display())),
        None => shot::build_shot(&root())?,
    };
    let me = std::env::current_exe().map_err(|e| format!("this xtask: {e}"))?;
    let mut failed = Vec::new();
    for scene in scenes {
        for bg in BACKGROUNDS {
            for &size in &sizes {
                // (the transparent themes' apart: `--compare` matches shots by name)
                let clear = if transparent { "-transparent" } else { "" };
                let file = out.join(format!("{}-{bg}-{}{clear}.png", scene.name, size.0));
                if let Err(e) = one(scene, bg, size, transparent, &bin, &me, &file) {
                    eprintln!("{}: {e}", file.display());
                    failed.push(file.display().to_string());
                }
            }
        }
    }
    match failed.len() {
        0 => Ok(()),
        n => Err(format!("{n} shots failed: {}", failed.join(", "))),
    }
}

/// The scenes `names` names, in their order — every one when none is named.
fn chosen(names: &[String]) -> Result<Vec<&'static Scene>> {
    if names.is_empty() {
        return Ok(SCENES.iter().collect());
    }
    let find = |n: &String| {
        SCENES
            .iter()
            .find(|s| s.name == n)
            .ok_or_else(|| format!("no scene `{n}` — cargo xtask scenes --list"))
    };
    names.iter().map(find).collect()
}

/// One shot of `scene`: its demo crate, config and tarae's state in a fresh folder, removed after — on `bg`'s
/// transparent theme when `transparent`.
fn one(
    scene: &Scene,
    bg: &'static str,
    size: (u16, u16),
    transparent: bool,
    bin: &Path,
    me: &Path,
    out: &Path,
) -> Result<()> {
    let dir = Fresh::new("tarae-scene")?;
    let demo = dir.0.join("tarae-demo");
    demo_crate_with(&demo, scene.name == "trusted", &run_files(scene.name))?;
    let config = dir.0.join("xdg");
    let mut toml = config_toml(me, scene.claude);
    if transparent {
        let theme = if bg == "dark" { "meok-transparent" } else { "hanji-transparent" };
        toml = format!("theme = \"{theme}\"\n{toml}");
    }
    // (the shuttle scenes: a program may type, and the one that does)
    if scene.name.starts_with("shuttle") {
        toml.push_str("\n[ui]\nshuttle-keys = \"light\"\n");
        write(&dir.0.join("shuttle.sh"), &shuttle_sh(bin))?;
    }
    write(&config.join("tarae/config.toml"), &toml)?;
    let setup = Setup {
        bin: bin.to_path_buf(),
        files: scene.files.iter().map(|f| f.to_string()).collect(),
        cwd: demo,
        config,
        background: Some(bg),
        // (the shuttle scenes' terminal: a plain sh of the shot's own — a `$ ` prompt, no rc, no history, no user
        // or host name; never the shell of whoever takes the shots)
        // (a plain shell, its prompt `$ ` — nobody's own setup in the picture)
        env: match scene.name.starts_with("shuttle") || scene.name.starts_with("quote-term") {
            true => {
                vec![("SHELL", Some("/bin/sh")), ("PS1", Some("$ ")), ("HISTFILE", Some("")), ("ENV", None)]
            }
            false => Vec::new(),
        },
    };
    let keys: Vec<String> = scene.keys.iter().map(|k| k.to_string()).collect();
    shot::take(&setup, &keys, size, &dir.0.join("state"), out)
}

/// The shot's config: language servers off, this xtask as claude running `script` — and for the board's
/// scripts, the review settings that bring their angles (`knot.rs` high-risk, `thread.rs` the screen's).
fn config_toml(me: &Path, script: &str) -> String {
    let quoted = |s: &str| toml::Value::String(s.to_string()).to_string();
    let review = match script {
        "boardmid" => "\n[review]\nrisky = [\"knot.rs\"]\nscreen = [\"thread.rs\"]\nboard-at-once = 2\n",
        "boarddone" => "\n[review]\nrisky = [\"knot.rs\"]\n",
        _ => "",
    };
    format!(
        "[editor]\nlsp = false\n\n[llm]\ncommand = {}\nargs = [\"scenes\", \"--claude\", {}]\n{review}",
        quoted(&me.to_string_lossy()),
        quoted(script)
    )
}

/// A folder made new for one shot (0700; the next number when the name is taken — never one that was there),
/// by its real path; removed on drop, and only it.
pub(crate) struct Fresh(pub(crate) PathBuf);

impl Fresh {
    pub(crate) fn new(name: &str) -> Result<Fresh> {
        let base = std::env::temp_dir();
        for n in 0..1000 {
            let p = base.join(format!("{name}-{}-{n}", std::process::id()));
            match std::fs::DirBuilder::new().mode(0o700).create(&p) {
                Ok(()) => {
                    let real = std::fs::canonicalize(&p).map_err(|e| format!("{}: {e}", p.display()))?;
                    return Ok(Fresh(real));
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(format!("{}: {e}", p.display())),
            }
        }
        Err(format!("no fresh folder in {}", base.display()))
    }
}

impl Drop for Fresh {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}

// ── The demo crate ───────────────────────────────────────────────────────────

const CARGO_TOML: &str =
    "[package]\nname = \"tarae-demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n";

const MAIN_RS: &str = r#"use std::collections::HashMap;

mod thread;
use thread::Thread;

/// 실타래 — 이어진 실 한 가닥.
fn main() {
    let mut threads: HashMap<String, Thread> = HashMap::new();
    let first = Thread { name: "타래".into(), knots: vec![1, 2, 3] };
    threads.insert(first.name.clone(), first);
    for (name, t) in &threads {
        println!("{name}: {} knots, {} long", t.knots.len(), t.length());
    }
}
"#;

const THREAD_RS: &str = r#"/// One strand of a skein: a name and the knots tied along it.
pub struct Thread {
    pub name: String,
    pub knots: Vec<u32>,
}

impl Thread {
    /// The strand's length: the sum of the gaps between its knots.
    pub fn length(&self) -> u32 {
        self.knots.iter().sum()
    }

    /// Ties a knot at the end.
    pub fn tie(&mut self, at: u32) {
        self.knots.push(at);
    }
}
"#;

/// The crate's files as committed, then as changed since (`thread.rs` changed, `knot.rs` new) — with settings
/// that wait for trust when `trusted` (a build of its own for `:restart`).
fn demo_files(dir: &Path, changed: bool, trusted: bool) -> Result<()> {
    if !changed {
        if trusted {
            write(&dir.join(".tarae.toml"), "[editor]\nrestart-build = \"cargo build\"\n")?;
        }
        write(&dir.join("Cargo.toml"), CARGO_TOML)?;
        write(&dir.join(".gitignore"), "target/\nCargo.lock\n")?;
        write(&dir.join("README.md"), "# tarae-demo\n\nA skein of threads.\n")?;
        write(&dir.join("src/main.rs"), MAIN_RS)?;
        return write(&dir.join("src/thread.rs"), THREAD_RS);
    }
    let thread = THREAD_RS.replace("Ties a knot at the end.", "Ties a knot at the end of the strand.");
    write(&dir.join("src/thread.rs"), &thread)?;
    write(&dir.join("src/knot.rs"), "pub fn untie() {}\n")
}

/// The demo crate at `dir`: committed in a repository of its own, then changed. git runs with none of your
/// settings (no hooks, no signing) and finds no repository above `dir`.
fn demo_crate(dir: &Path, trusted: bool) -> Result<()> {
    demo_crate_with(dir, trusted, &[])
}

/// `demo_crate` with `extra` files committed beside its own (over them where named alike).
fn demo_crate_with(dir: &Path, trusted: bool, extra: &[(&str, String)]) -> Result<()> {
    demo_files(dir, false, trusted)?;
    for (path, text) in extra {
        write(&dir.join(path), text)?;
    }
    let git = |args: &[&str]| -> Result<()> {
        let st = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=demo", "-c", "user.email=demo@example.com", "-c", "commit.gpgsign=false"])
            .args(["-c", "core.hooksPath=/dev/null", "-c", "init.defaultBranch=main"])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CEILING_DIRECTORIES", dir.parent().unwrap_or(dir))
            .stdout(Stdio::null())
            .status()
            .map_err(|e| format!("git: {e}"))?;
        if st.success() { Ok(()) } else { Err(format!("git {} failed in {}", args.join(" "), dir.display())) }
    };
    git(&["init", "-q"])?;
    git(&["add", "-A"])?;
    git(&["commit", "-q", "-m", "A skein of threads"])?;
    // (a command of the repository's own, held until trusted — ssh's, which only a push, pull or fetch would run:
    // the demo has no remote)
    if trusted {
        git(&["config", "core.sshCommand", "ssh -o BatchMode=yes"])?;
    }
    demo_files(dir, true, trusted)
}

/// A failing test and one that passes, below `main` (`cargo test` on the bin): the tests panel's failure.
const MAIN_TESTS: &str = r#"
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn length_sums_the_knots() {
        let t = Thread { name: "실".into(), knots: vec![1, 2, 3] };
        assert_eq!(t.length(), 6);
    }

    #[test]
    fn a_tied_knot_lengthens_it() {
        let mut t = Thread { name: "실".into(), knots: vec![1, 2] };
        t.tie(4);
        println!("knots after the tie: {:?}", t.knots);
        assert_eq!(t.length(), 8, "the knot at 4 adds its gap");
    }
}
"#;

/// A check as a compiler prints it — warnings and their places on stderr, then a passing end.
const CHECK_SH: &str = r#"echo '    Checking tarae-demo v0.1.0'
echo 'warning: method `tie` is never used' >&2
echo '  --> src/thread.rs:15:12' >&2
echo '   |' >&2
echo '15 |     pub fn tie(&mut self, at: u32) {' >&2
echo '   |            ^^^' >&2
echo '   = note: `#[warn(dead_code)]` on by default' >&2
echo 'warning: unused variable: `first`' >&2
echo '  --> src/main.rs:8:9' >&2
echo "warning: \`tarae-demo\` (bin \"tarae-demo\") generated 2 warnings" >&2
echo '    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.42s'
"#;

/// The run scenes' own files: a task (`output-task`), a preflight of three steps (`output-preflight`) — both in
/// a `.tarae.toml` trusted on its card — or a failing test (`output-tests`, its build in the demo's own folder,
/// never cargo's shared one).
fn run_files(scene: &str) -> Vec<(&'static str, String)> {
    match scene {
        "output-task" | "panes-task-places" | "group-task-detail" | "group-task-stacked" => vec![
            (".tarae.toml", "[task.check]\ncmd = \"sh check.sh\"\n".into()),
            ("check.sh", CHECK_SH.into()),
        ],
        // (a run still printing: a line every 0.2 s for a minute)
        "output-live" | "polish-task-running" => vec![
            (".tarae.toml", "[task.watch]\ncmd = \"sh watch.sh\"\n".into()),
            (
                "watch.sh",
                "i=0\nwhile [ $i -lt 300 ]; do echo \"knot $i tied\"; i=$((i+1)); sleep 0.2; done\n".into(),
            ),
        ],
        "output-preflight" | "panes-preflight-steps" | "group-preflight-detail" => vec![
            (
                ".tarae.toml",
                "[preflight]\nsteps = [\n  { name = \"fmt\", cmd = \"sh fmt.sh\" },\n  { name = \"clippy\", cmd = \"sh check.sh\" },\n  { name = \"test\", cmd = \"sh test.sh\" },\n]\n"
                    .into(),
            ),
            ("fmt.sh", "echo 'Diff in src/thread.rs:8:'\necho '-    /// Ties a knot.'\necho '+    /// Ties a knot at the end.'\nexit 0\n".into()),
            ("check.sh", CHECK_SH.into()),
            (
                "test.sh",
                "echo 'running 2 tests'\necho 'test tests::length_sums_the_knots ... ok'\necho 'test tests::a_tied_knot_lengthens_it ... FAILED'\necho\necho \"thread 'tests::a_tied_knot_lengthens_it' panicked at src/main.rs:30:9:\"\necho 'assertion `left == right` failed: the knot at 4 adds its gap'\necho '  left: 7'\necho ' right: 8'\necho\necho 'test result: FAILED. 1 passed; 1 failed; 0 ignored'\nexit 101\n"
                    .into(),
            ),
        ],
        // (a preflight whose test step prints a line every 0.1 s for a minute)
        "polish-preflight-list" | "polish-preflight-output" => vec![
            (
                ".tarae.toml",
                "[preflight]\nsteps = [\n  { name = \"fmt\", cmd = \"sh fmt.sh\" },\n  { name = \"clippy\", cmd = \"sh check.sh\" },\n  { name = \"test\", cmd = \"sh test.sh\" },\n]\n"
                    .into(),
            ),
            ("fmt.sh", "exit 0\n".into()),
            ("check.sh", CHECK_SH.into()),
            (
                "test.sh",
                "echo 'running 600 tests'\ni=0\nwhile [ $i -lt 600 ]; do echo \"test tests::knot_$i ... ok\"; i=$((i+1)); sleep 0.1; done\n"
                    .into(),
            ),
        ],
        "output-tests" | "panes-tests-code" | "panes-float" | "group-tests-list" | "group-tests-detail"
        | "group-tests-stacked" => vec![
            ("src/main.rs", format!("{MAIN_RS}{MAIN_TESTS}")),
            (".cargo/config.toml", "[build]\nbuild-dir = \"target/build\"\n".into()),
        ],
        "panes-task-tabs" => vec![
            ("src/main.rs", format!("{MAIN_RS}{MAIN_TESTS}")),
            (".cargo/config.toml", "[build]\nbuild-dir = \"target/build\"\n".into()),
            (".tarae.toml", "[task.check]\ncmd = \"sh check.sh\"\n".into()),
            ("check.sh", CHECK_SH.into()),
        ],
        _ => Vec::new(),
    }
}

// ── The stand-in claude ──────────────────────────────────────────────────────

/// Reads tarae's lines from `input` and answers each user message (the line that starts `{"message"` — tarae's
/// JSON has its keys sorted) with `script`'s turn on `out`, `pause` after each piece of text: `full` explains,
/// reads and edits (its calls wait for tarae's answers), `three` the same with three changes; `mid` stops in the
/// middle of its code block and waits for tarae to go; `midlist` stops in a list after a quote, on a line in
/// Japanese; `ask` answers `space i` and stops mid-line; `card` asks to run outside the sandbox; `samebinary` asks
/// it twice, a program rebuilt between; `searchcard` searches a folder in home with tarae's `search`; `grants` asks
/// for what `:granted` then lists; `warm` answers on a big conversation's 1h cache, and
/// `warmfork` as a ping's fork of it; `boardmid` / `boarddone` are a board's members (each by what its message
/// asks — `board`); `idle` never answers.
fn claude(script: &str, input: impl BufRead, out: &mut impl Write, pause: Duration) -> io::Result<()> {
    let mut lines = input.lines();
    while let Some(line) = lines.next() {
        let line = line?;
        // (the docs' own scripts first)
        if docshots::answered(script, &line, out, pause)? {
            continue;
        }
        // (a ping's fork sends its message once `initialize`'s answer names the credential: answered for it)
        if script == "warmfork" && line.contains("\"tarae-init\"") {
            let account = json!({"models": [], "account": {"apiKeySource": "none"}});
            let response = json!({"subtype": "success", "request_id": "tarae-init", "response": account});
            writeln!(out, "{}", json!({"type": "control_response", "response": response}))?;
            out.flush()?;
            continue;
        }
        if !line.starts_with("{\"message\"") {
            continue;
        }
        let mut turn = Turn { out: &mut *out, lines: &mut lines, pause };
        match script {
            "full" => turn.explain(true, false)?,
            "three" => turn.explain(true, true)?,
            // (its three-item edit being written: the third item's new text held mid-line)
            "writemid" => turn.explain_held(true, true, Some("nots.len());\\n\"}"))?,
            "mid" => turn.explain(false, false)?,
            "midlist" => turn.explain_listed()?,
            "ask" => turn.rewrite()?,
            "card" => turn.ask_outside()?,
            "samebinary" => turn.same_binary()?,
            "searchcard" => turn.search_outside()?,
            "grants" => turn.grants()?,
            "warm" => turn.warm(false)?,
            "warmfork" => turn.warm(true)?,
            "boardmid" => turn.board(&line, false)?,
            "boarddone" => turn.board(&line, true)?,
            "reviewing" => turn.reviewer(false)?,
            "reviewdone" => turn.reviewer(true)?,
            "reviewfull" => turn.reviewer_full()?,
            _ => {}
        }
    }
    Ok(())
}

struct Turn<'a, W, L> {
    out: &'a mut W,
    lines: &'a mut L,
    pause: Duration,
}

impl<W: Write, L: Iterator<Item = io::Result<String>>> Turn<'_, W, L> {
    fn line(&mut self, v: Value) -> io::Result<()> {
        writeln!(self.out, "{v}")?;
        self.out.flush()
    }

    fn text(&mut self, t: &str) -> io::Result<()> {
        let delta = json!({"type": "text_delta", "text": t});
        self.line(json!({"type": "stream_event", "event": {"type": "content_block_delta", "index": 0, "delta": delta}}))?;
        std::thread::sleep(self.pause);
        Ok(())
    }

    /// `s` a word at a time, as a model streams it.
    fn words(&mut self, s: &str) -> io::Result<()> {
        s.split(' ').try_for_each(|w| self.text(&format!("{w} ")))
    }

    /// A call of tarae's tool `name` (the turn's `n`th): its row in the chat, the request, then tarae's answer
    /// waited for.
    fn call(&mut self, n: u64, name: &str, args: Value) -> io::Result<()> {
        let tool = json!({"type": "tool_use", "id": format!("t{n}"), "name": format!("mcp__tarae__{name}"), "input": args});
        self.line(json!({"type": "assistant", "message": {"id": format!("m{n}"), "content": [tool]}}))?;
        let msg = json!({"jsonrpc": "2.0", "id": n, "method": "tools/call", "params": {"name": name, "arguments": args}});
        let request = json!({"subtype": "mcp_message", "server_name": "tarae", "message": msg});
        self.line(json!({"type": "control_request", "request_id": format!("c{n}"), "request": request}))?;
        let id = format!("\"c{n}\"");
        for l in self.lines.by_ref() {
            if l?.contains(&id) {
                break;
            }
        }
        Ok(())
    }

    /// A call's input as the model writes it, before the call (`input_json_delta`, tarae draws it as it comes —
    /// `writing.rs`): `json` (its keys in the order written) a few characters a piece; with `hold`, the pieces
    /// stop at the first `hold` in it — and wait there until tarae goes (a shot of it being written).
    fn input(&mut self, n: u64, name: &str, json: &str, hold: Option<&str>) -> io::Result<()> {
        let block = json!({"type": "tool_use", "id": format!("t{n}"), "name": format!("mcp__tarae__{name}"), "input": {}});
        self.line(json!({"type": "stream_event", "event": {"type": "content_block_start", "index": 1, "content_block": block}}))?;
        let end = hold.and_then(|h| json.find(h)).unwrap_or(json.len());
        let chars: Vec<char> = json[..end].chars().collect();
        for piece in chars.chunks(12) {
            let delta = json!({"type": "input_json_delta", "partial_json": piece.iter().collect::<String>()});
            self.line(json!({"type": "stream_event", "event": {"type": "content_block_delta", "index": 1, "delta": delta}}))?;
            std::thread::sleep(self.pause / 2);
        }
        if hold.is_some() {
            // (an empty piece after a pause, as the stream sends: tarae's reader sends what it holds at a line)
            std::thread::sleep(Duration::from_millis(60));
            let delta = json!({"type": "input_json_delta", "partial_json": ""});
            self.line(json!({"type": "stream_event", "event": {"type": "content_block_delta", "index": 1, "delta": delta}}))?;
            return self.lines.by_ref().try_for_each(|l| l.map(drop));
        }
        self.line(json!({"type": "stream_event", "event": {"type": "content_block_stop", "index": 1}}))
    }

    /// (`three`: the edit changes the doc comment and the print too — three changes in one review)
    fn explain(&mut self, whole: bool, three: bool) -> io::Result<()> {
        self.explain_held(whole, three, None)
    }

    /// `explain`, its edit's input held at `hold` while it's written (`writemid`).
    fn explain_held(&mut self, whole: bool, three: bool, hold: Option<&str>) -> io::Result<()> {
        self.words("Let me look at how the map is filled first.")?;
        self.text("\n\n")?;
        self.call(1, "read", json!({"files": ["src/main.rs:7-14"]}))?;
        self.words(
            "The name is cloned because **insert** takes the key by value, and `first` moves into the map",
        )?;
        self.words(
            "in the same call. Keying by the name alone means a second strand with the same name replaces",
        )?;
        self.words("the first:")?;
        self.text("\n\n```rust\nlet mut threads: HashMap<String, Thread> = HashMap::new();\n")?;
        if !whole {
            // (mid-answer: the shot is taken while it waits — until tarae goes)
            self.text("threads.entry(first.name.clone())")?;
            return self.lines.by_ref().try_for_each(|l| l.map(drop));
        }
        self.text("threads.entry(first.name.clone()).or_insert(first);\n```\n\n")?;
        self.words("I will switch it to the entry API so a repeat keeps the first strand.")?;
        self.text("\n")?;
        // (each item's keys in the order the model writes them: what it replaces, then what it writes)
        let item = |old: &str, new: &str| {
            let s = |t: &str| Value::from(t).to_string();
            format!(r#"{{"file_path": "src/main.rs", "old_string": {}, "new_string": {}}}"#, s(old), s(new))
        };
        let edit = item(
            "    threads.insert(first.name.clone(), first);\n",
            "    threads.entry(first.name.clone()).or_insert(first);\n",
        );
        let items = if three {
            let doc =
                item("/// 실타래 — 이어진 실 한 가닥.\n", "/// 실타래 — 같은 이름은 첫 가닥으로 남는다.\n");
            let print = item(
                "        println!(\"{name}: {} knots, {} long\", t.knots.len(), t.length());\n",
                "        println!(\"{name}: {} knots\", t.knots.len());\n",
            );
            [doc, edit, print].join(", ")
        } else {
            edit
        };
        let input = format!(r#"{{"edits": [{items}]}}"#);
        self.input(2, "edit", &input, hold)?;
        self.call(2, "edit", serde_json::from_str(&input).map_err(io::Error::other)?)?;
        self.words("Done — **main.rs** now keeps the first strand when a name repeats.")?;
        self.words("Nothing else reads the map, so no other change is needed.")?;
        self.line(json!({"type": "result", "result": "done"}))
    }

    /// `explain`'s start, then a quote and a list, stopping in its last item — and waiting until tarae goes.
    fn explain_listed(&mut self) -> io::Result<()> {
        self.words("The name is cloned because **insert** takes the key by value.")?;
        self.text("\n\n> ")?;
        self.words("같은 이름은 한 가닥으로 남는다.")?;
        self.text("\n\n- ")?;
        self.words("`insert` replaces the value")?;
        self.text("\n- ")?;
        self.words("`entry` keeps the first:")?;
        // (characters the shots' fonts all have)
        for c in ["同じ", "名前は", "一本", "だ"] {
            self.text(c)?;
        }
        self.lines.by_ref().try_for_each(|l| l.map(drop))
    }

    /// `space i`'s answer (`<r i="1">` …): the line rewritten, stopping mid-line — and holding there a minute (a
    /// shot's time): tarae closes this process's input once it has sent the message, so there's nothing to wait
    /// on, and an exit would end the answer.
    fn rewrite(&mut self) -> io::Result<()> {
        self.text("<r i=\"1\">")?;
        for t in ["    threads", ".entry(first.name", ".clone())", ".or_insert("] {
            self.text(t)?;
        }
        std::thread::sleep(Duration::from_secs(60));
        Ok(())
    }

    /// A turn of a big conversation (`warm`): its init, a request that wrote 181k tokens to the 1h cache, a short
    /// answer, its result — or (`fork`) a ping's fork of it: the request read them, its total its own (`modelUsage`
    /// counting its request alone, as a fork's does while the chat's claude runs).
    fn warm(&mut self, fork: bool) -> io::Result<()> {
        let id = if fork { "fork-demo" } else { "conv-demo" };
        self.line(
            json!({"type": "system", "subtype": "init", "session_id": id, "model": "claude-opus-5-5"}),
        )?;
        self.line(json!({"type": "system", "subtype": "status", "status": "requesting"}))?;
        let (read, write, output) = if fork { (181_200, 410, 4) } else { (0, 181_200, 120) };
        let usage = json!({"input_tokens": 3, "cache_read_input_tokens": read, "cache_creation_input_tokens": write,
            "cache_creation": {"ephemeral_1h_input_tokens": write}, "output_tokens": output});
        let message = json!({"id": format!("{id}-1"), "model": "claude-opus-5-5", "usage": usage});
        self.line(json!({"type": "stream_event", "event": {"type": "message_start", "message": message}}))?;
        match fork {
            true => self.text("ok")?,
            false => self.words(
                "The map is filled in one pass over the strands; the change is ready for your review.",
            )?,
        }
        let model = json!({"claude-opus-5-5": {"inputTokens": 3, "cacheReadInputTokens": read,
            "cacheCreationInputTokens": write, "outputTokens": output, "contextWindow": 1_000_000}});
        let cost = if fork { 0.0911 } else { 1.8125 };
        self.line(
            json!({"type": "result", "result": "ok", "session_id": id, "total_cost_usd": cost, "usage": usage,
            "modelUsage": model}),
        )
    }

    /// Waits for tarae to go — a member still reading when the shot is taken.
    fn reads_on(&mut self) -> io::Result<()> {
        self.lines.by_ref().try_for_each(|l| l.map(drop))
    }

    /// The one pass (`space v c`, in the background — bgreview): three reads, then (`done`) a finding on
    /// knot.rs and its verdict; else it reads on.
    fn reviewer(&mut self, done: bool) -> io::Result<()> {
        for (n, f) in ["src/knot.rs", "src/main.rs", "src/thread.rs"].iter().enumerate() {
            let read = json!({"type": "tool_use", "id": format!("r{n}"), "name": "mcp__tarae__read", "input": {"files": [f]}});
            self.line(json!({"type": "assistant", "message": {"id": format!("m{n}"), "content": [read]}}))?;
        }
        if !done {
            return self.reads_on();
        }
        let text = "`untie` does nothing: a caller that unties a knot keeps it.";
        self.call(1, "note", json!({"path": "src/knot.rs", "line": 1, "text": text}))?;
        self.line(json!({"type": "result", "result": "One finding: untie keeps the knot."}))
    }

    /// The one pass as the docs show it (`claude-reviews.png`): its reads, three findings on `thread.rs`'s
    /// change, and a verdict that weighs them.
    fn reviewer_full(&mut self) -> io::Result<()> {
        for (n, f) in ["src/thread.rs", "src/main.rs", "src/knot.rs"].iter().enumerate() {
            let read = json!({"type": "tool_use", "id": format!("r{n}"), "name": "mcp__tarae__read", "input": {"files": [f]}});
            self.line(json!({"type": "assistant", "message": {"id": format!("m{n}"), "content": [read]}}))?;
        }
        let note = |line: u64, text: &str| json!({"path": "src/thread.rs", "line": line, "text": text});
        let findings = [
            (
                10,
                "`length` sums `u32` gaps: a long strand overflows — a panic in debug builds, a wrong length in release.",
            ),
            (
                14,
                "The doc says the knot goes at the strand's end, but `at` is a gap: `tie(5)` makes the strand 5 longer.",
            ),
            (
                15,
                "No test ties a knot: nothing holds the doc's claim, or catches `tie(0)` adding a knot with no length.",
            ),
        ];
        for (n, (line, text)) in findings.iter().enumerate() {
            self.call(n as u64 + 1, "note", note(*line, text))?;
        }
        let verdict = "Three findings, all in thread.rs — none blocks, the first matters most.\n\n\
            - `length` can overflow on a long strand (a panic in debug builds).\n\
            - `tie`'s doc and its argument disagree: `at` is a gap, not a place.\n\
            - No test covers `tie`.\n\n\
            knot.rs's new `untie` is empty and unused; main.rs only reads the strand.";
        // (how long its turn took, as claude says it — the report's "Done in": fixed, so retakes compare the same)
        self.line(json!({"type": "result", "result": verdict, "duration_ms": 112_400}))
    }

    /// A board's member, by what its message (`msg`) asks: an angle notes its claims on the demo's changes
    /// (`done`: every angle ends; else security reads on, three reads made, and the others after correctness
    /// read on too); a skeptic confirms `knot.rs`'s claims (the second the same failure) and, on `thread.rs`,
    /// refutes the first and confirms the second.
    fn board(&mut self, msg: &str, done: bool) -> io::Result<()> {
        let note = |path: &str, line: u64, text: &str| json!({"path": path, "line": line, "text": text});
        if msg.contains("(angle: correctness)") {
            self.call(
                1,
                "note",
                note("src/knot.rs", 1, "`untie` does nothing: a caller that unties a knot keeps it."),
            )?;
            let doc =
                "The doc says the knot goes at the strand's end, but `at` is a gap: the length grows by it.";
            self.call(2, "note", note("src/thread.rs", 13, doc))?;
            return self
                .line(json!({"type": "result", "result": "Two findings: untie, and the doc of tie."}));
        }
        if msg.contains("(angle: security)") {
            if !done {
                for (n, f) in ["src/knot.rs", "src/main.rs", "src/thread.rs"].iter().enumerate() {
                    let read = json!({"type": "tool_use", "id": format!("r{n}"), "name": "mcp__tarae__read", "input": {"files": [f]}});
                    self.line(
                        json!({"type": "assistant", "message": {"id": format!("m{n}"), "content": [read]}}),
                    )?;
                }
                return self.reads_on();
            }
            let text = "`untie` is public and empty: any caller believes the knot is gone, and nothing says it isn't.";
            self.call(1, "note", note("src/knot.rs", 1, text))?;
            return self.line(json!({"type": "result", "result": "One finding."}));
        }
        if msg.contains("(angle: tests)") && done {
            self.call(
                1,
                "note",
                note("src/thread.rs", 14, "No test ties a knot: nothing holds the doc's claim."),
            )?;
            return self.line(json!({"type": "result", "result": "One finding."}));
        }
        // (a skeptic's claims come after the diff every member is sent: its group is read from there)
        if let Some((_, claims)) = msg.split_once("(role: skeptic)") {
            let judge = |claim: u64, verdict: &str, trace: &str| json!({"claim": claim, "verdict": verdict, "trace": trace});
            if claims.contains("knot.rs") {
                let trace = "knot.rs:1 — untie's body is empty; nothing in main.rs or thread.rs takes a knot out, so a call leaves it";
                self.call(1, "judge", judge(1, "confirmed", trace))?;
                let same =
                    json!({"claim": 2, "verdict": "confirmed", "trace": "the same empty body", "same_as": 1});
                self.call(2, "judge", same)?;
            } else {
                let held = "thread.rs:15 — push puts the knot last: the doc holds";
                self.call(1, "judge", judge(1, "refuted", held))?;
                self.call(
                    2,
                    "judge",
                    judge(2, "confirmed", "thread.rs:14 — no #[test] in the crate calls tie"),
                )?;
            }
            return self.line(json!({"type": "result", "result": "Judged."}));
        }
        self.reads_on()
    }

    /// A permission prompt of claude's for its own tool `tool` (the turn's `n`th): its row in the chat, the ask
    /// (`can_use_tool`), then tarae's answer waited for — as `call` does for tarae's tools.
    fn permission(&mut self, n: u64, tool: &str, input: Value) -> io::Result<()> {
        let used = json!({"type": "tool_use", "id": format!("p{n}"), "name": tool, "input": input});
        self.line(json!({"type": "assistant", "message": {"id": format!("q{n}"), "content": [used]}}))?;
        let request = json!({"subtype": "can_use_tool", "tool_name": tool, "input": input});
        self.line(json!({"type": "control_request", "request_id": format!("r{n}"), "request": request}))?;
        let id = format!("\"r{n}\"");
        for l in self.lines.by_ref() {
            if l?.contains(&id) {
                break;
            }
        }
        Ok(())
    }

    /// What `:granted` lists, asked for one at a time: a run first (the sandbox looked at — `also` waits for that),
    /// a read outside the project, a host through `also`, another read. Places under home that aren't there
    /// (nothing is read: the answer is all), a host that resolves nowhere, `ls` for the runs (one that runs
    /// without a card).
    fn grants(&mut self) -> io::Result<()> {
        let home = std::env::var("HOME").unwrap_or_default();
        self.words("Let me look at the notes the strands follow, outside the project.")?;
        self.text("\n")?;
        self.call(1, "run", json!({"command": "ls"}))?;
        self.permission(2, "Read", json!({"file_path": format!("{home}/notes/strands.md")}))?;
        let also = json!({"command": "ls", "also": {"network": "skein.example.invalid"},
            "reason": "the knots' index is fetched from there"});
        self.call(3, "run", also)?;
        self.permission(4, "Read", json!({"file_path": format!("{home}/Sites/skein/knots.css")}))?;
        self.words("Read them: nothing in the project needs to change.")?;
        self.line(json!({"type": "result", "result": "done"}))
    }

    fn ask_outside(&mut self) -> io::Result<()> {
        self.words(
            "The tests need the network for the registry, so I will ask to run them outside the sandbox.",
        )?;
        self.text("\n")?;
        let run = json!({"command": "cargo test --quiet", "outside_sandbox": true,
            "reason": "cargo needs to fetch the registry index"});
        self.call(1, "run", run)?;
        self.line(json!({"type": "result", "result": "done"}))
    }

    /// A program that isn't text (a NUL in it), under the demo's ignored `target/`: run outside the sandbox,
    /// rebuilt, run again — the second card is the first's "Same as".
    fn same_binary(&mut self) -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let probe = Path::new("target/probe");
        let build = |n: &str| -> io::Result<()> {
            std::fs::create_dir_all("target")?;
            // (a NUL: what tarae takes for a program's bytes — after the lines sh runs)
            let mut bytes = format!("#!/bin/sh\necho probe {n}\nexit 0\n# build ").into_bytes();
            bytes.extend(b"\x00\xff");
            std::fs::write(probe, bytes)?;
            std::fs::set_permissions(probe, std::fs::Permissions::from_mode(0o755))
        };
        build("one")?;
        self.words("The probe needs a terminal of its own, so I will run it outside the sandbox.")?;
        self.text("\n")?;
        let run = json!({"command": "./target/probe", "outside_sandbox": true,
            "reason": "the probe opens a terminal of its own"});
        self.call(1, "run", run.clone())?;
        build("two, with a drain loop")?;
        self.words("Rebuilt with a drain loop — once more.")?;
        self.text("\n")?;
        self.call(2, "run", run)?;
        self.line(json!({"type": "result", "result": "done"}))
    }

    /// tarae's `search` of a folder in home, outside the project: its read card, a pattern too long for it.
    fn search_outside(&mut self) -> io::Result<()> {
        let home = std::env::var("HOME").unwrap_or_default();
        self.words("The card record is outside the project; I will search it for the terminal grants.")?;
        self.text("\n")?;
        let pattern = r#""kind":"(grant|run|read)".*"(terminals|pty|tty)".*"answer":"(y|a|s|A)".*"session":"frictions3[0-9]""#;
        let search = json!({"pattern": pattern, "path": format!("{home}/Library/Logs")});
        self.call(1, "search", search)?;
        self.line(json!({"type": "result", "result": "done"}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the stand-in wrote, a JSON value a line.
    fn said(script: &str, input: &str) -> Vec<Value> {
        let mut out = Vec::new();
        claude(script, input.as_bytes(), &mut out, Duration::ZERO).unwrap();
        String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    const ASKED: &str = "{\"request\":{\"subtype\":\"initialize\"},\"type\":\"control_request\"}\n\
                         {\"message\":{\"content\":\"why?\",\"role\":\"user\"},\"type\":\"user\"}\n\
                         {\"response\":{\"request_id\":\"c1\"},\"type\":\"control_response\"}\n\
                         {\"response\":{\"request_id\":\"c2\"},\"type\":\"control_response\"}\n";

    /// `full`: text streamed, a `read` and an `edit` of tarae's tools — each waited on — and the turn's end; the
    /// edit's old text is the demo's own line, so tarae's review finds it.
    #[test]
    fn the_stand_in_streams_a_turn_and_its_calls() {
        let lines = said("full", ASKED);
        let calls: Vec<&str> =
            lines.iter().filter_map(|l| l["request"]["message"]["params"]["name"].as_str()).collect();
        assert_eq!(calls, ["read", "edit"]);
        let rows: Vec<&str> =
            lines.iter().filter_map(|l| l["message"]["content"][0]["name"].as_str()).collect();
        assert_eq!(rows, ["mcp__tarae__read", "mcp__tarae__edit"], "each call's row in the chat");
        assert_eq!(lines.last().unwrap()["type"], "result");
        let text: String = lines.iter().filter_map(|l| l["event"]["delta"]["text"].as_str()).collect();
        assert!(text.contains("or_insert(first);") && text.contains("Nothing else reads the map"), "{text}");
        let edit =
            lines.iter().find_map(|l| l["request"]["message"]["params"]["arguments"]["edits"][0].as_object());
        let old = edit.unwrap()["old_string"].as_str().unwrap();
        assert!(MAIN_RS.contains(old), "{old:?}");
    }

    /// `mid` ends in its code block with no result (the shot is taken then); `idle` says nothing; a turn waits
    /// for a user message — tarae's other lines aren't one.
    #[test]
    fn mid_stops_in_its_code_block_and_idle_says_nothing() {
        let mid = said("mid", ASKED);
        assert!(mid.iter().all(|l| l["type"] != "result"));
        let text: String = mid.iter().filter_map(|l| l["event"]["delta"]["text"].as_str()).collect();
        assert!(text.ends_with("threads.entry(first.name.clone())"), "{text}");
        assert!(said("idle", ASKED).is_empty());
        assert!(said("full", "{\"request\":{},\"type\":\"control_request\"}\n").is_empty());
    }

    /// The config names this xtask as claude with its script, and turns language servers off.
    #[test]
    fn a_scenes_config_runs_the_stand_in_and_no_server() {
        let t: toml::Table = config_toml(Path::new("/x/\"q\"/xtask"), "card").parse().unwrap();
        assert_eq!(t["editor"]["lsp"].as_bool(), Some(false));
        assert_eq!(t["llm"]["command"].as_str(), Some("/x/\"q\"/xtask"));
        let args: Vec<&str> =
            t["llm"]["args"].as_array().unwrap().iter().filter_map(|a| a.as_str()).collect();
        assert_eq!(args, ["scenes", "--claude", "card"]);
    }

    /// Every scene has a name of its own, found by it; an unknown one is said.
    #[test]
    fn scenes_are_named_once_and_found_by_name() {
        let mut names: Vec<&str> = SCENES.iter().map(|s| s.name).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), SCENES.len());
        let got = chosen(&["card".into(), "start".into()]).unwrap();
        assert_eq!(got.iter().map(|s| s.name).collect::<Vec<_>>(), ["card", "start"]);
        assert_eq!(chosen(&[]).unwrap().len(), SCENES.len());
        assert!(chosen(&["nope".into()]).unwrap_err().contains("no scene `nope`"));
    }

    /// The demo's files as committed and as changed; a fresh folder is a new one each time, gone on drop.
    #[test]
    fn the_demo_is_written_in_a_fresh_folder_removed_after() {
        let (a, b) = (Fresh::new("tarae-scenes-test").unwrap(), Fresh::new("tarae-scenes-test").unwrap());
        assert_ne!(a.0, b.0);
        demo_files(&a.0, false, false).unwrap();
        demo_files(&a.0, true, false).unwrap();
        let thread = std::fs::read_to_string(a.0.join("src/thread.rs")).unwrap();
        assert!(thread.contains("at the end of the strand.") && a.0.join("src/knot.rs").is_file());
        let path = a.0.clone();
        drop(a);
        assert!(!path.exists());
    }
}
