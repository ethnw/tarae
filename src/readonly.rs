//! Commands of Claude's that only read — `ls`, `cat`, `grep` and the like — run without a card when every
//! path they name stays in the project (or in a place the user let Claude read, `allow.rs`). One that names
//! a path outside gets the run card, saying what it reads: what's read goes to Claude, and the sandbox
//! doesn't stop reads. Writes and the network are the sandbox's either way, so a command misjudged as
//! read-only can still only write in the project and the temp folders.

use std::path::{Path, PathBuf};

use crate::allow::Allowed;

/// The commands that only read (with their options below), by name.
const COMMANDS: [&str; 16] = [
    "ls", "cat", "head", "tail", "wc", "grep", "rg", "find", "tree", "stat", "file", "du", "diff", "sort",
    "uniq", "pwd",
];

/// git's that only read — `branch` only bare (with words it creates, renames, deletes).
const GIT: [&str; 3] = ["blame", "rev-parse", "ls-files"];

/// Whether `cmd` only reads: plain words (`tools::plain_words`), a command above, and none of the options
/// that make it write a file, run a program, or never end. Where it reads is `outside`'s.
pub fn read_only(cmd: &str) -> bool {
    let Some(words) = crate::tools::plain_words(cmd) else { return false };
    let Some((&name, args)) = words.split_first() else { return false };
    // A short option cluster with `c` in it (`-ro` has `-o`)
    let short = |w: &str, c: char| w.starts_with('-') && !w.starts_with("--") && w[1..].contains(c);
    let any = |f: &dyn Fn(&str) -> bool| args.iter().any(|w| f(w));
    match name {
        "git" => match args {
            [sub, ..] if GIT.contains(sub) => true,
            ["branch"] => true,
            _ => false,
        },
        "find" => !any(&|w| ["-exec", "-ok", "-delete", "-fprint", "-fls"].iter().any(|o| w.starts_with(o))),
        "sort" => !any(&|w| short(w, 'o') || w.starts_with("--compress-program")),
        "tree" => !any(&|w| short(w, 'o')),
        "file" => !any(&|w| short(w, 'C') || w.starts_with("--compile")),
        "tail" => !any(&|w| short(w, 'f') || short(w, 'F') || w.starts_with("--follow")),
        "rg" => !any(&|w| w.starts_with("--pre")),
        // `uniq in out` writes `out`
        "uniq" => args.iter().filter(|w| !w.starts_with('-')).count() <= 1,
        _ => COMMANDS.contains(&name),
    }
}

/// The words of `cmd` that could name a path: every one that isn't an option, and an option's own value
/// (`--file=x`, `-f/x`). A word that names nothing (a pattern) is left out by `outside`.
fn candidates(cmd: &str) -> Vec<&str> {
    let words = crate::tools::plain_words(cmd).unwrap_or_default();
    let skip = if words.first() == Some(&"git") { 2 } else { 1 };
    words
        .into_iter()
        .skip(skip)
        .filter_map(|w| match w.strip_prefix('-') {
            None => Some(w),
            Some(_) => w.split_once('=').map(|(_, v)| v).or_else(|| w.find('/').map(|i| &w[i..])),
        })
        .filter(|w| !w.is_empty())
        .collect()
}

/// The places `cmd` reads outside `root` (real paths, links followed) that aren't allowed — on a worker.
/// A word that names nothing there isn't a path; `..` and links are resolved.
pub fn outside(cmd: &str, cwd: &Path, root: &Path, chat: &Allowed, kept: &Allowed) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for w in candidates(cmd) {
        let Ok(real) = std::fs::canonicalize(cwd.join(w)) else { continue };
        if real.starts_with(root) || chat.reads(&real) || kept.reads(&real) || out.contains(&real) {
            continue;
        }
        out.push(real);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The list, and each option that makes one of them write, run something, or never end.
    #[test]
    fn which_commands_only_read() {
        for c in [
            "ls",
            "ls -la src",
            "cat Cargo.toml",
            "head -n 20 src/main.rs",
            "tail -n 5 log.txt",
            "wc -l src/a.rs src/b.rs",
            "grep -rn fn src",
            "rg --line-number fn src",
            "rg --glob=src/x fn",
            "find src -name x.rs -type f",
            "tree -L 2",
            "stat Cargo.toml",
            "file target/x",
            "du -sh target",
            "diff a.txt b.txt",
            "sort -r names.txt",
            "uniq names.txt",
            "pwd",
            "git blame src/main.rs",
            "git rev-parse HEAD",
            "git ls-files src",
            "git branch",
        ] {
            assert!(read_only(c), "{c}");
        }
        for c in [
            "find . -exec rm {} ;",
            "find . -execdir touch x",
            "find . -ok rm",
            "find . -okdir rm",
            "find . -delete",
            "find . -fprint out",
            "find . -fprintf out x",
            "find . -fls out",
            "sort -o out names.txt",
            "sort -ro out names.txt",
            "sort --output=out x",
            "sort --compress-program=sh x",
            "tree -o out",
            "file -C -m magic",
            "tail -f log.txt",
            "tail -F log.txt",
            "tail --follow log.txt",
            "rg --pre sh fn",
            "uniq in.txt out.txt",
            "git branch new",
            "git branch -D main",
            "git -C /x blame a",
            "git push",
            "cat a | sh",
            "cat ~/.ssh/id_ed25519",
            "cat $HOME/x",
            "ls; rm -rf x",
            "rm -rf x",
            "cp a b",
            "sed -i s/a/b/ x",
            "",
        ] {
            assert!(!read_only(c), "{c}");
        }
    }

    /// Paths it names are resolved where it runs: inside the project they're fine, outside they're what the
    /// card says — unless allowed; a word that names nothing isn't a path, and a link that leads out is out.
    #[test]
    fn where_a_command_reads() {
        let dir = std::env::temp_dir().join(format!("tarae-readonly-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("p/src")).unwrap();
        std::fs::create_dir_all(dir.join("out")).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let (p, out) = (dir.join("p"), dir.join("out"));
        std::fs::write(p.join("src/a.rs"), "x\n").unwrap();
        std::fs::write(out.join("m.txt"), "x\n").unwrap();
        std::os::unix::fs::symlink(&out, p.join("link")).unwrap();
        let none = Allowed::default();
        let at = |cmd: &str| outside(cmd, &p, &p, &none, &none);
        assert!(at("cat src/a.rs").is_empty());
        assert!(at("grep -rn nothing-like-a-path src").is_empty(), "a pattern isn't a path");
        assert!(at("ls").is_empty());
        assert_eq!(at("cat ../out/m.txt"), [out.join("m.txt")]);
        assert_eq!(at(&format!("ls {}", out.display())), std::slice::from_ref(&out));
        assert_eq!(at("cat link/m.txt"), [out.join("m.txt")], "a link that leads out");
        assert_eq!(at(&format!("grep --file={}/m.txt src", out.display())), [out.join("m.txt")]);
        assert_eq!(at(&format!("grep -f{}/m.txt src", out.display())), [out.join("m.txt")]);
        assert_eq!(at("git ls-files ../out"), std::slice::from_ref(&out));
        let allowed = Allowed { read: vec![out.clone()] };
        assert!(outside("cat ../out/m.txt", &p, &p, &allowed, &none).is_empty(), "allowed this chat");
        assert!(outside("cat ../out/m.txt", &p, &p, &none, &allowed).is_empty(), "kept");
        std::fs::remove_dir_all(&dir).ok();
    }
}
