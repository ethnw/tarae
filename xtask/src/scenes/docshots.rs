//! `cargo xtask scenes --docs [picture …]` — the pictures the docs show (`docs/screenshots/`), each retaken from the
//! scene its row in `DOCS` names, at its size, theme and scale, into `target/docshots/` (`--out DIR`), then
//! compared with the committed ones pixel for pixel (`scenes --compare`; one that can't be read is said and the
//! run goes on). A row's old picture in `out` goes before its shot, and the run writes each row's result last, in
//! `<out>/manifest.tsv`. `--keep` copies only what that manifest says was taken and found different (or new) —
//! once you've looked at them; it takes nothing itself, and refuses with no manifest or with a picture written
//! after it. `--list` prints the table. A row with no scene yet is named apart; one whose `needs` aren't on
//! `PATH` is skipped and named.
//!
//! A docs shot differs from a scenes one three ways: the `shot-docs` profile's tarae (the shot profile with every
//! bundled grammar), a data home of its own (grammars, packs, sessions' checkouts — empty: nothing downloaded on
//! this machine shows), and a folder of a fixed name, `<temp>/tarae-docs/<scene>`, so the paths an output panel
//! prints stay put between retakes — there already, the shot fails rather than clear it.
//!
//! A new picture in the docs = a row here (the tests below: every picture a page references has a row and every
//! row's picture is referenced, every PNG in `docs/screenshots/` has a row, every row's scene exists).

use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use super::{Compared, SCENES, Scene, compared, config_toml, demo_crate, write};
use crate::shot::{self, Setup};
use crate::{Result, root};

const USAGE: &str =
    "usage: cargo xtask scenes --docs [--out DIR] [picture …] | --docs --keep [picture …] | --docs --list";

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
}

/// One picture the docs show: its file under `docs/screenshots/`, the scene that makes it (None: none yet), its
/// size in cells, theme and scale, and the programs it runs for real (skipped where they aren't on `PATH`).
#[derive(Debug)]
struct DocShot {
    picture: &'static str,
    scene: Option<&'static str>,
    size: (u16, u16),
    bg: Bg,
    scale: usize,
    needs: &'static [&'static str],
}

/// `cargo xtask shot`'s default size, most pictures'.
const S: (u16, u16) = (112, 30);

const fn row(picture: &'static str, scene: Option<&'static str>, size: (u16, u16)) -> DocShot {
    DocShot { picture, scene, size, bg: Bg::Dark, scale: 1, needs: &[] }
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
}

/// Every picture, by page in the order it shows them (`target/sb/docshots-plan.md` says what each is for).
const DOCS: &[DocShot] = &[
    // README.md (and features.md)
    row("hero-hanji.png", None, (140, 40)).light().at(2),
    row("hero-meok.png", None, (140, 40)).at(2),
    // docs/claude-integration.md
    row("m4-review.png", None, S),
    row("m4-chat.png", None, S),
    row("chat-model.png", None, S),
    row("chat-conversations.png", None, S),
    row("inbox.png", None, (120, 34)),
    row("inbox-hanji.png", None, (120, 34)).light(),
    row("chat-screen.png", None, (140, 40)),
    row("chat-point-hanji.png", None, (140, 34)).light(),
    row("chat-ids.png", None, (140, 34)),
    row("chat-explore.png", None, S),
    // (`chatmid` shows neither the wash nor the thinking these show: phase 3's `thinking` script)
    row("follow.png", None, S),
    row("follow-hanji.png", None, S).light(),
    row("follow-thinking.png", None, S),
    row("follow-show.png", None, (140, 40)),
    row("follow-ask.png", None, S),
    row("m4-chat-hanji.png", None, S).light(),
    row("chat-plan.png", None, S),
    // (`review` would do, but its head quotes the edit tool's `what`, not what was asked — a piece of its own)
    row("chat-edit.png", None, S),
    row("chat-edit-why.png", None, S),
    row("chat-turn-review.png", None, (200, 36)),
    row("chat-review-at-end.png", None, S),
    row("chat-undo-turn.png", None, S),
    row("chat-tests.png", None, S),
    row("chat-run.png", None, S),
    row("notes.png", None, S),
    row("commit-draft.png", None, S),
    row("review.png", None, S),
    row("claude-reviews.png", None, S),
    row("review-risk.png", None, S),
    row("review-end-held.png", None, S),
    // docs/features.md
    row("ux-palette.png", None, S),
    row("ux-which-key.png", None, S),
    row("ux-tutor.png", None, S),
    // (`start`, once phase 2 seeds recent files: a fresh state has none)
    row("ux-welcome.png", None, S),
    row("ux-welcome-continue.png", None, S),
    row("ux-theme-picker.png", None, S),
    row("ux-toasts-sync.png", None, S),
    row("ux-doc-comments.png", None, S),
    row("ux-doc-comments-open.png", None, S),
    row("ux-guides.png", None, S),
    row("ux-side-panel.png", Some("code"), S),
    row("ux-outline.png", None, S),
    row("m2-file-picker.png", Some("picker"), S),
    row("ux-search.png", None, S),
    row("ux-splits.png", None, S),
    row("ux-cmdline-complete.png", None, S),
    row("ux-git.png", None, S),
    row("syntax-helm.png", None, S),
    row("grammar-offer.png", None, S),
    row("offer-stack.png", None, S),
    row("syntax-injections.png", None, S),
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
    let (mut out, mut names, mut keep) = (root().join("target/docshots"), Vec::new(), false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--list" => {
                DOCS.iter().for_each(|d| println!("{}", listed(d)));
                return Ok(());
            }
            "--out" => out = PathBuf::from(it.next().ok_or(USAGE)?),
            "--keep" => keep = true,
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
    let (mut results, mut same) = (Vec::new(), 0);
    for d in &rows {
        let Some(scene) = d.scene.and_then(|n| SCENES.iter().find(|s| s.name == n)) else {
            none.push(d.picture);
            continue;
        };
        let missing: Vec<&str> = d.needs.iter().copied().filter(|p| !on_path(p)).collect();
        if !missing.is_empty() {
            skipped.push(format!("{} (no {})", d.picture, missing.join(", ")));
            continue;
        }
        let file = ready(&out, d.picture)?;
        let result = match one(scene, d, &bin, &me, &file) {
            Ok(()) => {
                let c = compared(&out, &docs, d.picture);
                println!("{}: {}", d.picture, c.said());
                same += usize::from(c == Compared::Same);
                if matches!(c, Compared::Unreadable(_)) {
                    unreadable.push(d.picture);
                }
                verdict(&c)
            }
            Err(e) => {
                eprintln!("{}: {e}", file.display());
                failed.push(d.picture);
                "failed"
            }
        };
        results.push((d.picture, result));
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
    let manifest: String = results.iter().map(|(p, r)| format!("{p}\t{r}\n")).collect();
    write(&out.join(MANIFEST), &manifest)?;
    let mut wrong = Vec::new();
    if !failed.is_empty() {
        wrong.push(format!("{} pictures failed: {}", failed.len(), failed.join(", ")));
    }
    if !unreadable.is_empty() {
        wrong.push(format!("{} couldn't be compared: {}", unreadable.len(), unreadable.join(", ")));
    }
    if wrong.is_empty() { Ok(()) } else { Err(wrong.join("; ")) }
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
    format!("{:<28} {scene:<16} {c}×{r} ×{} {}{needs}", d.picture, d.scale, d.bg.word())
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

/// One picture: `scene`'s demo crate, config, state and data home in a folder of its name, removed after.
fn one(scene: &Scene, d: &DocShot, bin: &Path, me: &Path, out: &Path) -> Result<()> {
    let dir = Named::new(scene.name)?;
    let demo = dir.0.join("tarae-demo");
    demo_crate(&demo, scene.name == "trusted")?;
    let config = dir.0.join("xdg");
    write(&config.join("tarae/config.toml"), &config_toml(me, scene.claude))?;
    let setup = Setup {
        bin: bin.to_path_buf(),
        files: scene.files.iter().map(|f| f.to_string()).collect(),
        cwd: demo,
        config,
        background: Some(d.bg.word()),
        env: Vec::new(),
    };
    let keys: Vec<String> = scene.keys.iter().map(|k| k.to_string()).collect();
    shot::take_at(&setup, &keys, d.size, d.scale, &dir.0.join("state"), &dir.0.join("data"), out)
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
        if let Some(why) = refused(st.st_uid, u32::from(st.st_mode), me) {
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
fn refused(uid: u32, mode: u32, me: u32) -> Option<String> {
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

    /// Each row's picture once, its scene one `scenes --list` names, a scale `shot` draws (1–4).
    #[test]
    fn every_docs_row_names_a_scene() {
        assert_eq!(rows().len(), DOCS.len(), "a picture in DOCS twice");
        for d in DOCS {
            if let Some(s) = d.scene {
                assert!(SCENES.iter().any(|x| x.name == s), "{}: no scene `{s}`", d.picture);
            }
            assert!((1..=4).contains(&d.scale), "{}: scale {}", d.picture, d.scale);
        }
        assert!(chosen(&["chat-edit".into(), "follow.png".into()]).is_ok_and(|r| r.len() == 2));
        assert!(chosen(&["nope".into()]).unwrap_err().contains("no docs picture `nope`"));
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
        assert!(refused(501, 0o40700, 502).is_some_and(|w| w.contains("not yours (its owner is 501)")));
        assert!(refused(501, 0o40750, 501).is_some_and(|w| w.contains("mode 750")));
        assert_eq!(refused(501, 0o40700, 501), None);
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
