//! `cargo xtask scenes [--out DIR] [scene …]` — tarae's main surfaces as shots, each in both themes (meok and
//! hanji) at 80×24 and 160×45: `<DIR>/<scene>-<dark|light>-<80|160>.png` (default `target/scenes`); every scene
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

const USAGE: &str = "usage: cargo xtask scenes [--out DIR] [scene …] | scenes --list | scenes --compare A B";

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
    Scene { name: "granted", claude: "idle", files: MAIN, keys: &["@1", ":", "granted", "\r", "@1"] },
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
    // (a run outside the sandbox: its card)
    Scene {
        name: "card",
        claude: "card",
        files: MAIN,
        keys: &["@1", " ", "l", "@1", "run the tests", "\r", "@?Its reason", "@1"],
    },
];

/// `scenes --compare A B`: each shot in `A` against `B`'s of its name, pixel for pixel — `same`, or the box the
/// differing pixels lie in, to look at there (a turn's time or a temp path moves from run to run).
fn compare(a: &Path, b: &Path) -> Result<()> {
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
    let mut names: Vec<String> = std::fs::read_dir(a)
        .map_err(|e| format!("{}: {e}", a.display()))?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.ends_with(".png"))
        .collect();
    names.sort();
    let mut differ = 0;
    for n in &names {
        let (x, y) = (read(&a.join(n))?, read(&b.join(n))?);
        if (x.0, x.1) != (y.0, y.1) {
            differ += 1;
            println!("{n}: {}×{} against {}×{}", x.0, x.1, y.0, y.1);
            continue;
        }
        let (w, px) = (x.0 as usize, x.2.len() / (x.0 * x.1) as usize);
        let mut boxed: Option<(usize, usize, usize, usize)> = None;
        for (i, (p, q)) in x.2.chunks(px).zip(y.2.chunks(px)).enumerate() {
            if p != q {
                let (cx, cy) = (i % w, i / w);
                let (x0, y0, x1, y1) = boxed.unwrap_or((cx, cy, cx, cy));
                boxed = Some((x0.min(cx), y0.min(cy), x1.max(cx), y1.max(cy)));
            }
        }
        match boxed {
            None => println!("{n}: same"),
            Some((x0, y0, x1, y1)) => {
                differ += 1;
                println!("{n}: differs in x {x0}..={x1}, y {y0}..={y1} (of {}×{})", x.0, x.1);
            }
        }
    }
    println!("{} of {} the same", names.len() - differ, names.len());
    Ok(())
}

const SIZES: [(u16, u16); 2] = [(80, 24), (160, 45)];
const BACKGROUNDS: [&str; 2] = ["dark", "light"];

pub fn run(args: &[String]) -> Result<()> {
    if args.first().is_some_and(|a| a == "--claude") {
        let script = args.get(1).ok_or(USAGE)?;
        let stdin = io::stdin();
        let pause = Duration::from_millis(40);
        return claude(script, stdin.lock(), &mut io::stdout().lock(), pause)
            .map_err(|e| format!("the stand-in claude: {e}"));
    }
    if args.first().is_some_and(|a| a == "--compare") {
        let [_, a, b] = args else { return Err(USAGE.into()) };
        return compare(Path::new(a), Path::new(b));
    }
    let (mut out, mut names) = (root().join("target/scenes"), Vec::new());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--list" => {
                SCENES.iter().for_each(|s| println!("{}", s.name));
                return Ok(());
            }
            "--out" => out = PathBuf::from(it.next().ok_or(USAGE)?),
            n if !n.starts_with('-') => names.push(n.to_string()),
            _ => return Err(USAGE.into()),
        }
    }
    let scenes = chosen(&names)?;
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let bin = shot::build_shot(&root())?;
    let me = std::env::current_exe().map_err(|e| format!("this xtask: {e}"))?;
    let mut failed = Vec::new();
    for scene in scenes {
        for bg in BACKGROUNDS {
            for size in SIZES {
                let file = out.join(format!("{}-{bg}-{}.png", scene.name, size.0));
                if let Err(e) = one(scene, bg, size, &bin, &me, &file) {
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

/// One shot of `scene`: its demo crate, config and tarae's state in a fresh folder, removed after.
fn one(scene: &Scene, bg: &'static str, size: (u16, u16), bin: &Path, me: &Path, out: &Path) -> Result<()> {
    let dir = Fresh::new("tarae-scene")?;
    let demo = dir.0.join("tarae-demo");
    demo_crate(&demo)?;
    let config = dir.0.join("xdg");
    write(&config.join("tarae/config.toml"), &config_toml(me, scene.claude))?;
    let setup = Setup {
        bin: bin.to_path_buf(),
        files: scene.files.iter().map(|f| f.to_string()).collect(),
        cwd: demo,
        config,
        background: Some(bg),
    };
    let keys: Vec<String> = scene.keys.iter().map(|k| k.to_string()).collect();
    shot::take(&setup, &keys, size, &dir.0.join("state"), out)
}

/// The shot's config: language servers off, this xtask as claude running `script`.
fn config_toml(me: &Path, script: &str) -> String {
    let quoted = |s: &str| toml::Value::String(s.to_string()).to_string();
    format!(
        "[editor]\nlsp = false\n\n[llm]\ncommand = {}\nargs = [\"scenes\", \"--claude\", {}]\n",
        quoted(&me.to_string_lossy()),
        quoted(script)
    )
}

/// A folder made new for one shot (0700; the next number when the name is taken — never one that was there),
/// by its real path; removed on drop, and only it.
struct Fresh(PathBuf);

impl Fresh {
    fn new(name: &str) -> Result<Fresh> {
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

/// The crate's files as committed, then as changed since (`thread.rs` changed, `knot.rs` new).
fn demo_files(dir: &Path, changed: bool) -> Result<()> {
    if !changed {
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
fn demo_crate(dir: &Path) -> Result<()> {
    demo_files(dir, false)?;
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
    demo_files(dir, true)
}

// ── The stand-in claude ──────────────────────────────────────────────────────

/// Reads tarae's lines from `input` and answers each user message (the line that starts `{"message"` — tarae's
/// JSON has its keys sorted) with `script`'s turn on `out`, `pause` after each piece of text: `full` explains,
/// reads and edits (its calls wait for tarae's answers); `mid` stops in the middle of its code block and waits
/// for tarae to go; `midlist` stops in a list after a quote, on a line in Japanese; `ask` answers `space i` and
/// stops mid-line; `card` asks to run outside the sandbox; `idle` never answers.
fn claude(script: &str, input: impl BufRead, out: &mut impl Write, pause: Duration) -> io::Result<()> {
    let mut lines = input.lines();
    while let Some(line) = lines.next() {
        if !line?.starts_with("{\"message\"") {
            continue;
        }
        let mut turn = Turn { out: &mut *out, lines: &mut lines, pause };
        match script {
            "full" => turn.explain(true)?,
            "mid" => turn.explain(false)?,
            "midlist" => turn.explain_listed()?,
            "ask" => turn.rewrite()?,
            "card" => turn.ask_outside()?,
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

    fn explain(&mut self, whole: bool) -> io::Result<()> {
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
        let edit = json!({"file_path": "src/main.rs", "old_string": "    threads.insert(first.name.clone(), first);\n",
            "new_string": "    threads.entry(first.name.clone()).or_insert(first);\n"});
        self.call(2, "edit", json!({"edits": [edit]}))?;
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
        demo_files(&a.0, false).unwrap();
        demo_files(&a.0, true).unwrap();
        let thread = std::fs::read_to_string(a.0.join("src/thread.rs")).unwrap();
        assert!(thread.contains("at the end of the strand.") && a.0.join("src/knot.rs").is_file());
        let path = a.0.clone();
        drop(a);
        assert!(!path.exists());
    }
}
