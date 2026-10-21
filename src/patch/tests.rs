use super::*;

const GIT: &str = "\
From 1234 Mon Sep 17 00:00:00 2001
Subject: [PATCH] a change

---
 src/a.rs | 2 +-

diff --git a/src/a.rs b/src/a.rs
index 3b18e51..a4c1f2d 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,3 +1,3 @@
 one
-two
+TWO
 three
diff --git a/new.rs b/new.rs
new file mode 100755
index 0000000..e69de29
--- /dev/null
+++ b/new.rs
@@ -0,0 +1,2 @@
+fn x() {}
+fn y() {}
\\ No newline at end of file
diff --git a/gone.rs b/gone.rs
deleted file mode 100644
index 1111111..0000000
--- a/gone.rs
+++ /dev/null
@@ -1 +0,0 @@
-bye
diff --git a/old name.rs b/new name.rs
similarity index 100%
rename from old name.rs
rename to new name.rs
";

#[test]
fn a_git_diff_reads_its_files_hunks_and_headers() {
    let p = parse(GIT).unwrap();
    let names: Vec<(Option<&str>, Option<&str>)> =
        p.files.iter().map(|f| (f.old.as_deref(), f.new.as_deref())).collect();
    assert_eq!(
        names,
        [
            (Some("src/a.rs"), Some("src/a.rs")),
            (None, Some("new.rs")),
            (Some("gone.rs"), None),
            (Some("old name.rs"), Some("new name.rs")),
        ]
    );
    let a = &p.files[0];
    assert_eq!(a.base.as_deref(), Some("3b18e51"));
    assert_eq!(a.hunks[0].start, 0);
    assert_eq!(a.hunks[0].old, ["one\n", "two\n", "three\n"]);
    assert_eq!(a.hunks[0].new, ["one\n", "TWO\n", "three\n"]);
    // A new file: no base, its mode said, its last line without its line end
    let n = &p.files[1];
    assert_eq!((n.base.as_deref(), n.mode.as_deref()), (None, Some("100755")));
    assert_eq!(n.hunks[0].new, ["fn x() {}\n", "fn y() {}"]);
    assert_eq!(p.files[2].hunks[0].old, ["bye\n"]);
    assert!(p.files[3].hunks.is_empty());
}

#[test]
fn a_plain_diff_keeps_its_names_and_drops_their_timestamps() {
    let text = "--- x.txt.orig\t2026-10-19 10:00:00\n+++ x.txt\t2026-10-19 10:01:00\n@@ -2 +2 @@\n-b\n+B\n";
    // (one file named twice, as `diff -u x.txt.orig x.txt` does: the new name, not a move)
    let p = parse(text).unwrap();
    assert_eq!(p.files[0].old.as_deref(), Some("x.txt"));
    assert_eq!(p.files[0].new.as_deref(), Some("x.txt"));
    assert_eq!(p.files[0].hunks[0].start, 1);
    // `diff -ru old/ new/`: what both share past their first folder
    let p = parse("--- old/src/x.txt\n+++ new/src/x.txt\n@@ -1 +1 @@\n-a\n+b\n").unwrap();
    assert_eq!(
        (p.files[0].old.as_deref(), p.files[0].new.as_deref()),
        (Some("src/x.txt"), Some("src/x.txt"))
    );
    // git's rename is a move still
    assert_eq!(parse(GIT).unwrap().files[3].old.as_deref(), Some("old name.rs"));
    // Quoted, as git writes a name with a tab or past ASCII in it
    let text = "diff --git \"a/t\\tab\\303\\251\" \"b/t\\tab\\303\\251\"\n--- \"a/t\\tab\\303\\251\"\n+++ \"b/t\\tab\\303\\251\"\n@@ -1 +1 @@\n-a\n+b\n";
    assert_eq!(parse(text).unwrap().files[0].new.as_deref(), Some("t\tabé"));
}

#[test]
fn what_it_cant_apply_is_refused_by_name() {
    let bin = "diff --git a/i.png b/i.png\nindex 1..2 100644\nBinary files a/i.png and b/i.png differ\n";
    assert!(parse(bin).unwrap_err().starts_with("i.png: a binary change"), "{:?}", parse(bin));
    let lit = "diff --git a/i.png b/i.png\nindex 1..2 100644\nGIT binary patch\nliteral 3\n";
    assert!(parse(lit).unwrap_err().contains("binary"));
    let twice = "--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-b\n+c\n";
    assert!(parse(twice).unwrap_err().contains("x is in it twice (lines 1 and 6)"));
    assert!(parse("just words\n").unwrap_err().starts_with("no diff in it"));
    let short = "--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n a\n-b\n";
    assert!(parse(short).unwrap_err().contains("the hunk at line 3 ends early"));
    let sub = "diff --git a/m b/m\nindex 1..2 160000\n--- a/m\n+++ b/m\n@@ -1 +1 @@\n-Subproject commit 1\n+Subproject commit 2\n";
    assert!(parse(sub).is_ok_and(|p| p.files[0].hunks.len() == 1));
}

/// A hunk of `old` → `new` lines at `start` (0-based).
fn h(start: usize, old: &[&str], new: &[&str]) -> Hunk {
    let lines = |v: &[&str]| v.iter().map(|l| format!("{l}\n")).collect();
    Hunk { start, old: lines(old), new: lines(new), line: 1 }
}

#[test]
fn hunks_go_on_where_they_say_or_the_one_place_their_lines_are() {
    let text = "a\nb\nc\nd\n";
    let hs = [h(1, &["b"], &["B"]), h(3, &["d"], &["D", "E"])];
    assert_eq!(lay(text, &hs, false), Ok(("a\nB\nc\nD\nE\n".into(), vec![])));
    // Two lines added above: exactly is refused, moving finds them (and the next hunk moves with it)
    let moved = "x\ny\na\nb\nc\nd\n";
    assert_eq!(lay(moved, &hs, false), Err(vec![(0, Unlaid::Missing), (1, Unlaid::Missing)]));
    assert_eq!(lay(moved, &hs, true), Ok(("x\ny\na\nB\nc\nD\nE\n".into(), vec![(0, 2)])));
    // Its lines in two places, neither where it says: never picked
    let twice = "q\nb\nq\nb\n";
    assert_eq!(lay(twice, &[h(0, &["b"], &["B"])], true), Err(vec![(0, Unlaid::Several(vec![2, 4]))]));
    assert_eq!(lay("a\n", &[h(0, &["z"], &[])], true), Err(vec![(0, Unlaid::Missing)]));
    // No old lines to place it by: only on an empty file
    assert_eq!(lay("a\n", &[h(1, &[], &["b"])], true), Err(vec![(0, Unlaid::NoContext)]));
    assert_eq!(lay("", &[h(0, &[], &["b"])], true), Ok(("b\n".into(), vec![])));
    assert_eq!(lay("a\n", &[h(1, &[], &["b"])], false), Ok(("a\nb\n".into(), vec![])));
}

const L: [&str; 3] = ["this session", "base", "x.patch"];

#[test]
fn a_merge_takes_each_sides_changes_and_shows_where_both_differ() {
    let base = "1\n2\n3\n4\n5\n6\n7\n";
    // Apart: both kept
    let this = "0\n1\n2\n3\n4\n5\n6\n7\n";
    let theirs = "1\n2\n3\n4\n5\nSIX\n7\n";
    assert_eq!(merge3(base, this, theirs, L), ("0\n1\n2\n3\n4\n5\nSIX\n7\n".into(), 0));
    // One side only, and the same change on both
    assert_eq!(merge3(base, base, theirs, L), (theirs.into(), 0));
    assert_eq!(merge3(base, this, base, L), (this.into(), 0));
    let both = "1\n2\nTHREE\n4\n5\n6\n7\n";
    let this_too = "0\n1\n2\nTHREE\n4\n5\n6\n7\n";
    assert_eq!(merge3(base, this_too, both, L), (this_too.into(), 0));
    // The same line, two ways: a diff3 block
    let mine = "1\n2\nthree\n4\n5\n6\n7\n";
    let (text, n) = merge3(base, mine, both, L);
    assert_eq!(n, 1);
    assert_eq!(
        text,
        "1\n2\n<<<<<<< this session\nthree\n||||||| base\n3\n=======\nTHREE\n>>>>>>> x.patch\n4\n5\n6\n7\n"
    );
    assert_eq!(crate::conflict::find(&ropey::Rope::from_str(&text)).len(), 1);
    // Neighbouring lines are one place
    let next = "1\n2\n3\nfour\n5\n6\n7\n";
    assert_eq!(merge3(base, next, both, L).1, 1);
}
