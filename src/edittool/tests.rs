use super::*;

/// A project in the test's own temp folder with `files` (path, text).
fn project(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = crate::scratch::Scratch::new(&format!("edittool-{name}")).into_path();
    for (p, t) in files {
        let f = dir.join(p);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, t).unwrap();
    }
    dir
}

fn known(dir: &Path) -> Known {
    Known {
        root: dir.to_path_buf(),
        open: HashMap::new(),
        dirty: Vec::new(),
        seen: HashMap::new(),
        keys: crate::reads::Keys::none_opened(dir),
    }
}

fn run(k: &Known, edits: Value) -> Result<Resolved, String> {
    resolve(k, &items_of(&json!({ "edits": edits }))?)
}

fn change<'r>(r: &'r Resolved, rel: &str) -> &'r FileChange {
    r.files.iter().find(|f| f.rel == rel).unwrap_or_else(|| panic!("{rel} in {:?}", r.files))
}

/// Items go in order — a later one sees what an earlier one left in the same file — across files, as one
/// change set; one item failing fails them all (nothing comes back to land), and the error names it.
#[test]
fn replacements_go_in_order_across_files_or_not_at_all() {
    let dir = project("order", &[("a.rs", "fn one() {}\nfn two() {}\n"), ("b.rs", "let x = one();\n")]);
    let k = known(&dir);
    let r = run(
        &k,
        json!([
            {"file_path": "a.rs", "old_string": "fn one()", "new_string": "fn uno()"},
            {"file_path": "a.rs", "old_string": "fn uno() {}", "new_string": "fn uno() { }"},
            {"file_path": "b.rs", "old_string": "one()", "new_string": "uno()"},
        ]),
    )
    .unwrap();
    assert_eq!(change(&r, "a.rs").after.to_string(), "fn uno() { }\nfn two() {}\n");
    assert_eq!((change(&r, "a.rs").places, change(&r, "a.rs").op.clone()), (2, Op::Text));
    assert_eq!(change(&r, "b.rs").after.to_string(), "let x = uno();\n");
    assert!(r.told.is_empty() && r.stale.is_empty());
    let e = run(
        &k,
        json!([
            {"file_path": "a.rs", "old_string": "fn one()", "new_string": "fn uno()"},
            {"file_path": "b.rs", "old_string": "nowhere", "new_string": "x"},
        ]),
    )
    .unwrap_err();
    assert!(e.starts_with("nothing changed — 1 of 2 items failed"), "{e}");
    assert!(e.contains("edits[1] (b.rs): old_string isn't in the file"), "{e}");
    assert!(!e.contains("edits[0]"), "only the one that failed: {e}");
    // A malformed item is answered at once
    let bad = items_of(&json!({"edits": [{"file_path": "a.rs", "old_string": "x"}]})).unwrap_err();
    assert!(bad.contains("both strings"), "{bad}");
    let both = items_of(&json!({"edits": [{"file_path": "a.rs", "delete": true, "move_to": "b.rs"}]}));
    assert!(both.unwrap_err().contains("not more"));
    std::fs::remove_dir_all(&dir).ok();
}

/// An old_string found more than once is refused, naming the lines, unless replace_all (then every one).
#[test]
fn a_match_is_one_unless_replace_all() {
    let dir = project("unique", &[("a.rs", "x = 1;\ny = 2;\nx = 1;\n")]);
    let k = known(&dir);
    let one = json!([{"file_path": "a.rs", "old_string": "x = 1;", "new_string": "x = 3;"}]);
    let e = run(&k, one).unwrap_err();
    assert!(e.contains("old_string matches 2 times (lines 1, 3)"), "{e}");
    let all =
        json!([{"file_path": "a.rs", "old_string": "x = 1;", "new_string": "x = 3;", "replace_all": true}]);
    let r = run(&k, all).unwrap();
    assert_eq!(change(&r, "a.rs").after.to_string(), "x = 3;\ny = 2;\nx = 3;\n");
    assert_eq!(change(&r, "a.rs").places, 2);
    std::fs::remove_dir_all(&dir).ok();
}

/// A miss says what Claude can act on: the lines most like old_string, numbered, and — when new_string is in
/// the file already — that the edit may have been made; nothing alike is said too.
#[test]
fn a_miss_shows_the_closest_lines_and_new_string_already_there() {
    let text = "use std::io;\n\nfn parse(input: &str) -> u32 {\n    input.trim().parse().unwrap()\n}\n";
    let dir = project("miss", &[("a.rs", text)]);
    let k = known(&dir);
    let typo = "fn parse(imput: &str) -> u32 {\n    imput.trim().parse().unwrap()";
    let e = run(&k, json!([{"file_path": "a.rs", "old_string": typo, "new_string": "x"}])).unwrap_err();
    assert!(e.contains("the closest lines, from line 3:"), "{e}");
    assert!(
        e.contains("\n  3: fn parse(input: &str) -> u32 {\n  4:     input.trim().parse().unwrap()"),
        "{e}"
    );
    let made = json!([{"file_path": "a.rs", "old_string": "use std::fs;", "new_string": "use std::io;"}]);
    let e = run(&k, made).unwrap_err();
    assert!(e.contains("new_string is already there (line 1): was this edit made already?"), "{e}");
    let none = json!([{"file_path": "a.rs", "old_string": "#[derive(Zzz)]", "new_string": "q"}]);
    assert!(run(&k, none).unwrap_err().contains("nor anything like it"));
    std::fs::remove_dir_all(&dir).ok();
}

/// Exact first; trailing whitespace, then curly quotes and dashes, only when the stricter tier finds nothing —
/// and a loose match is told (the user's card and review show it — `Landing.told`). A loose tier that
/// finds two is refused like an exact one.
#[test]
fn loose_tiers_only_after_exact_and_they_are_told() {
    let text = "let a = 1;   \nlet b = “q” – r;\nlet a = 1;\n";
    let dir = project("tiers", &[("a.rs", text)]);
    let k = known(&dir);
    // Exact: the third line only (the first has trailing blanks)
    let exact = json!([{"file_path": "a.rs", "old_string": "let a = 1;\n", "new_string": "let a = 2;\n"}]);
    let r = run(&k, exact).unwrap();
    assert_eq!(change(&r, "a.rs").after.to_string(), "let a = 1;   \nlet b = “q” – r;\nlet a = 2;\n");
    assert!(r.told.is_empty());
    // Trailing blanks: only the first has `1;` then `\nlet b`
    let trailing =
        json!([{"file_path": "a.rs", "old_string": "let a = 1;\nlet b", "new_string": "let a = 9;\nlet b"}]);
    let r = run(&k, trailing).unwrap();
    assert_eq!(change(&r, "a.rs").after.to_string(), "let a = 9;\nlet b = “q” – r;\nlet a = 1;\n");
    assert_eq!(r.told, ["edits[0] (a.rs) matched only with trailing whitespace ignored — check it"]);
    // Quotes and dashes: what Claude typed plain
    let quotes = json!([{"file_path": "a.rs", "old_string": "\"q\" - r", "new_string": "\"s\""}]);
    let r = run(&k, quotes).unwrap();
    assert_eq!(change(&r, "a.rs").after.to_string(), "let a = 1;   \nlet b = \"s\";\nlet a = 1;\n");
    assert!(r.told[0].contains("curly quotes and dashes"), "{:?}", r.told);
    // Two at a loose tier: refused
    let two = json!([{"file_path": "a.rs", "old_string": "let a = 1; \n", "new_string": "x"}]);
    assert!(run(&k, two).unwrap_err().contains("matches 2 times"));
    std::fs::remove_dir_all(&dir).ok();
}

/// An empty old_string makes a file where there's none (its folder too, when written); not over one that has
/// text; a non-empty old_string in no file says so.
#[test]
fn a_new_file_is_an_empty_old_string_where_none_is() {
    let dir = project("create", &[("a.rs", "x\n")]);
    let k = known(&dir);
    let r = run(&k, json!([{"file_path": "sub/new.rs", "old_string": "", "new_string": "pub fn n() {}\n"}]))
        .unwrap();
    let f = change(&r, "sub/new.rs");
    assert_eq!(
        (f.op.clone(), f.after.to_string(), f.before.to_string()),
        (Op::Create, "pub fn n() {}\n".into(), "".into())
    );
    let over = json!([{"file_path": "a.rs", "old_string": "", "new_string": "y\n"}]);
    assert!(run(&k, over).unwrap_err().contains("is there and isn't empty"));
    let missing = json!([{"file_path": "gone.rs", "old_string": "x", "new_string": "y"}]);
    assert!(run(&k, missing).unwrap_err().contains("gone.rs doesn't exist — an empty old_string makes it"));
    std::fs::remove_dir_all(&dir).ok();
}

/// frictions38: `whole: true` rewrites a file's text as one change of it — a text change, never a delete and a
/// create; where there's none it makes it; it takes new_string alone, and the same text is said.
#[test]
fn a_whole_rewrite_is_one_text_change_never_a_delete() {
    let dir = project("whole", &[("msg.txt", "Old subject\n\nold body\n"), ("Cargo.toml", "[package]\n")]);
    let k = known(&dir);
    let r = run(&k, json!([{"file_path": "msg.txt", "whole": true, "new_string": "New subject\n"}])).unwrap();
    let f = change(&r, "msg.txt");
    assert_eq!(
        (f.op.clone(), f.before.to_string(), f.after.to_string(), f.places),
        (Op::Text, "Old subject\n\nold body\n".into(), "New subject\n".into(), 1)
    );
    // (an empty old_string is the same as none; a later item sees the text it left)
    let then = json!([
        {"file_path": "msg.txt", "whole": true, "old_string": "", "new_string": "a\nb\n"},
        {"file_path": "msg.txt", "old_string": "b", "new_string": "c"},
    ]);
    assert_eq!(change(&run(&k, then).unwrap(), "msg.txt").after.to_string(), "a\nc\n");
    let new = run(&k, json!([{"file_path": "plan.md", "whole": true, "new_string": "# plan\n"}])).unwrap();
    assert_eq!(change(&new, "plan.md").op, Op::Create);
    // (a protected name: a text change as a replace's is, its review warning — not refused as a delete is)
    let p =
        run(&k, json!([{"file_path": "Cargo.toml", "whole": true, "new_string": "[workspace]\n"}])).unwrap();
    assert!(change(&p, "Cargo.toml").protected && change(&p, "Cargo.toml").op == Op::Text);
    let with_old = json!([{"file_path": "msg.txt", "whole": true, "old_string": "Old", "new_string": "x"}]);
    assert!(run(&k, with_old).unwrap_err().contains("whole: true takes new_string alone"));
    let same = json!([{"file_path": "msg.txt", "whole": true, "new_string": "Old subject\n\nold body\n"}]);
    assert!(run(&k, same).unwrap_err().contains("new_string is msg.txt's text already"));
    // (the empty old_string's error points at it)
    let over = json!([{"file_path": "msg.txt", "old_string": "", "new_string": "y\n"}]);
    assert!(run(&k, over).unwrap_err().contains("or whole: true for its whole new text"));
    std::fs::remove_dir_all(&dir).ok();
}

/// frictions38: under review at the end, a file rewritten whole goes in unseen as any text change — no card (a
/// delete first would have been one), the answer saying it written.
#[test]
fn under_review_at_the_end_a_whole_rewrite_asks_no_card() {
    let dir = project("whole-at-end", &[("msg.txt", "first draft\n")]);
    let mut ed = chat_for(&dir, &dir.join("stdin.log"));
    ed.session.chat.as_mut().unwrap().edits.at_end = Some(dir.clone());
    let args = json!({"edits": [{"file_path": "msg.txt", "whole": true, "new_string": "second draft\n"}]});
    call(&mut ed, Call::test(43), &args).unwrap();
    pump(&mut ed, "the answer", |_| answered(43).is_some());
    assert!(change_card(&ed).is_none(), "no card");
    assert_eq!(std::fs::read_to_string(dir.join("msg.txt")).unwrap(), "second draft\n");
    let a = answered(43).unwrap().unwrap();
    assert!(a.contains("written: msg.txt (1)"), "{a}");
    std::fs::remove_dir_all(&dir).ok();
}

/// A move or delete only of the project's own plain files — never a name Claude's runs can't write (a build
/// script, at either end of a move, through a link too), a file named like a key, anything outside the project,
/// a link, or a buffer with the user's unsaved edits; a move never over a file. (Nothing is written here: the
/// change set comes back or doesn't.)
#[test]
fn moves_and_deletes_only_of_the_projects_own_plain_files() {
    let dir = project(
        "paths",
        &[("a.rs", "a\n"), ("b.rs", "b\n"), ("build.rs", "fn main() {}\n"), (".env", "K=tarae-fake-token\n")],
    );
    std::os::unix::fs::symlink("a.rs", dir.join("link.rs")).unwrap();
    std::os::unix::fs::symlink("build.rs", dir.join("x.rs")).unwrap();
    let k = known(&dir);
    let refused = |edits: Value, why: &str| {
        let e = run(&k, edits.clone()).expect_err(&format!("{edits} refused"));
        assert!(e.contains(why), "{edits}: {e}");
    };
    refused(json!([{"file_path": "build.rs", "delete": true}]), "build script");
    refused(json!([{"file_path": "build.rs", "move_to": "src/build2.rs"}]), "build script");
    refused(json!([{"file_path": "a.rs", "move_to": "build.rs"}]), "build script");
    refused(json!([{"file_path": "x.rs", "delete": true}]), "build script");
    refused(json!([{"file_path": ".env", "delete": true}]), "named like a key");
    refused(json!([{"file_path": "a.rs", "move_to": ".env.local"}]), "named like a key");
    // (a file of the test's own beside the project — there, so only being outside refuses it)
    let outside = dir.with_file_name(format!("tarae-edittool-outside-{}.rs", std::process::id()));
    std::fs::write(&outside, "x\n").unwrap();
    refused(json!([{"file_path": outside, "delete": true}]), "outside the project");
    std::fs::remove_file(&outside).ok();
    refused(json!([{"file_path": "a.rs", "move_to": "../a.rs"}]), "outside the project");
    refused(json!([{"file_path": "link.rs", "delete": true}]), "is a link");
    refused(json!([{"file_path": "a.rs", "move_to": "b.rs"}]), "b.rs is there already");
    refused(json!([{"file_path": "nope.rs", "delete": true}]), "nope.rs doesn't exist");
    let mut dirty = known(&dir);
    dirty.dirty = vec![dir.join("a.rs")];
    let e = run(&dirty, json!([{"file_path": "a.rs", "delete": true}])).unwrap_err();
    assert!(e.contains("unsaved edits"), "{e}");
    // The project's own plain files: a move (folders made when written) and a delete
    let r =
        run(&k, json!([{"file_path": "a.rs", "move_to": "sub/c.rs"}, {"file_path": "b.rs", "delete": true}]))
            .unwrap();
    let to = dir.join("sub/c.rs");
    assert_eq!(change(&r, "a.rs").op, Op::Move { to, to_rel: "sub/c.rs".into() });
    assert_eq!(change(&r, "b.rs").op, Op::Delete);
    assert!(r.files.iter().all(|f| !f.protected));
    std::fs::remove_dir_all(&dir).ok();
}

/// A file moved in a call is edited at its new place (one move, its text after edited); its old place is
/// gone for the rest of the call.
#[test]
fn a_moved_file_is_edited_at_its_new_place() {
    let dir = project("moved", &[("a.rs", "fn a() {}\n")]);
    let k = known(&dir);
    let r = run(
        &k,
        json!([
            {"file_path": "a.rs", "move_to": "b.rs"},
            {"file_path": "b.rs", "old_string": "fn a()", "new_string": "fn b()"},
        ]),
    )
    .unwrap();
    assert_eq!(r.files.len(), 1, "{:?}", r.files);
    let f = change(&r, "a.rs");
    assert_eq!((f.before.to_string(), f.after.to_string()), ("fn a() {}\n".into(), "fn b() {}\n".into()));
    let e = run(
        &k,
        json!([
            {"file_path": "a.rs", "move_to": "b.rs"},
            {"file_path": "a.rs", "old_string": "fn a()", "new_string": "fn b()"},
        ]),
    )
    .unwrap_err();
    assert!(e.contains("a.rs was moved earlier in this call"), "{e}");
    std::fs::remove_dir_all(&dir).ok();
}

/// What the user sees is what's matched: an open buffer's text (unsaved edits and all), not the disk's. A file
/// changed since Claude last saw it through tarae is still changed where the text matches, and Claude is told
/// it has other changes; one as Claude saw it, nothing.
#[test]
fn the_buffer_is_matched_and_a_file_changed_since_seen_is_said() {
    let dir = project("seen", &[("a.rs", "on disk\n")]);
    let mut k = known(&dir);
    k.open.insert(dir.join("a.rs"), Rope::from_str("in the buffer\n"));
    let edit = json!([{"file_path": "a.rs", "old_string": "buffer", "new_string": "editor"}]);
    let r = run(&k, edit.clone()).unwrap();
    assert_eq!(change(&r, "a.rs").after.to_string(), "in the editor\n");
    assert!(r.stale.is_empty(), "never seen: no claim");
    k.seen.insert(dir.join("a.rs"), crate::disk::stamp(&dir.join("a.rs")).unwrap());
    assert!(run(&k, edit.clone()).unwrap().stale.is_empty(), "as Claude saw it");
    k.seen.insert(dir.join("a.rs"), (std::time::UNIX_EPOCH, 1));
    let r = run(&k, edit).unwrap();
    assert_eq!(
        r.stale,
        [
            "(a.rs has changed since you last read it — your edit matched what's there now; read it again before more edits)"
        ]
    );
    std::fs::remove_dir_all(&dir).ok();
}

// ── Landing: the card, review at the end, undo ─────────────────────────────

/// An editor on `dir` with a chat whose stand-in claude keeps what it's sent in `log`.
fn chat_for(dir: &Path, log: &Path) -> Editor {
    let mut ed = Editor::new(crate::config::Config::default());
    ed.session.root = dir.to_path_buf();
    ed.config.llm.command = "sh".into();
    ed.config.llm.args = vec!["-c".into(), format!("cat > '{}'", log.display())];
    crate::chat::open(&mut ed);
    ed
}

/// Events until `done` (the change is worked out and written on workers).
fn pump(ed: &mut Editor, what: &str, done: impl Fn(&Editor) -> bool) {
    let end = std::time::Instant::now() + crate::test_results::PATIENCE;
    while !done(ed) {
        assert!(std::time::Instant::now() < end, "{what}");
        if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(20)) {
            ed.handle_event(ev);
        }
    }
}

/// `llm.edit-tools`: by default the chat's claude has `Read` alone of Claude Code's tools and tarae's `edit`
/// listed (it makes every change); `:set llm.edit-tools native` leaves the running process as it started — its
/// `tools/list` the same — and the next chat's has Edit and Write and no `edit`. (A stand-in claude writes its
/// arguments in the test's own folder.)
#[test]
fn the_edit_tools_take_effect_with_the_next_chat() {
    let dir = project("setting", &[]);
    let mut ed = Editor::new(crate::config::Config::default());
    ed.session.root = dir.clone();
    ed.config.llm.command = "sh".into();
    ed.config.llm.args = vec!["-c".into(), "printf '%s\\n' \"$0\" \"$@\" > argv; cat >> stdin".into()];
    let tools_arg = |dir: &Path| {
        let argv = std::fs::read_to_string(dir.join("argv")).unwrap_or_default();
        let mut it = argv.lines();
        it.find(|l| *l == "--tools").and_then(|_| it.next()).map(String::from)
    };
    let listed = |ed: &mut Editor| -> Vec<String> {
        let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18"}});
        crate::tools::mcp(ed, 0, "r1", &init);
        let list =
            crate::tools::mcp(ed, 0, "r2", &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
        let tools = list.unwrap()["result"]["tools"].as_array().cloned().unwrap_or_default();
        tools.iter().filter_map(|t| t["name"].as_str().map(String::from)).collect()
    };
    crate::chat::open(&mut ed);
    pump(&mut ed, "the chat's claude", |_| tools_arg(&dir).is_some());
    assert_eq!(tools_arg(&dir).as_deref(), Some(""), "no tool of Claude Code's: no Edit or Write, no Read");
    assert!(listed(&mut ed).iter().any(|t| t == "edit"));
    crate::typed::execute(&mut ed, "set llm.edit-tools native").unwrap();
    assert_eq!(ed.config.llm.edit_tools, Arm::Native);
    assert!(listed(&mut ed).iter().any(|t| t == "edit"), "the running process keeps what it started with");
    std::fs::remove_file(dir.join("argv")).unwrap();
    crate::chat::reset(&mut ed);
    pump(&mut ed, "the next chat's claude", |_| tools_arg(&dir).is_some());
    assert_eq!(tools_arg(&dir).as_deref(), Some("Edit,Write"));
    assert!(!listed(&mut ed).iter().any(|t| t == "edit"), "native: no edit");
    std::fs::remove_dir_all(&dir).ok();
}

/// The change set's card, if up: its id, whose (its chat), and whether it has a move or delete.
fn change_card(ed: &Editor) -> Option<(u64, crate::refactor::Whose, bool)> {
    ed.offers.iter().find_map(|o| match o.what {
        crate::offer::What::ChangeSet { whose, ops } => Some((o.id, whose, ops)),
        _ => None,
    })
}

/// What call `id` was answered (`tools::ANSWERED`).
fn answered(id: u64) -> Option<Result<String, String>> {
    crate::tools::ANSWERED.with(|a| a.borrow().iter().find(|(i, _)| *i == json!(id)).map(|(_, r)| r.clone()))
}

/// Under review at the end a text change goes in as Claude makes it, but a delete in the same call doesn't:
/// it waits on the card (`r` refused — no buffer shows a delete), and only `y` deletes. (The files are the
/// test's own, in its temp folder.)
#[test]
fn under_review_at_the_end_a_delete_still_waits_on_the_card() {
    let dir = project("at-end", &[("a.rs", "one\n"), ("b.rs", "gone soon\n")]);
    let mut ed = chat_for(&dir, &dir.join("stdin.log"));
    ed.session.chat.as_mut().unwrap().edits.at_end = Some(dir.clone());
    let args = json!({"edits": [
        {"file_path": "a.rs", "old_string": "one", "new_string": "two"},
        {"file_path": "b.rs", "delete": true},
    ]});
    call(&mut ed, Call::test(41), &args).unwrap();
    pump(&mut ed, "the card", |ed| change_card(ed).is_some());
    let (id, generation, ops) = change_card(&ed).unwrap();
    assert!(ops, "a delete among them");
    assert_eq!(std::fs::read_to_string(dir.join("a.rs")).unwrap(), "two\n", "the text change went in unseen");
    assert!(dir.join("b.rs").exists(), "the delete waits on the card");
    crate::refactor::one_by_one(&mut ed, id, generation);
    assert!(change_card(&ed).is_some(), "r refused: the card is still up");
    assert!(dir.join("b.rs").exists());
    crate::refactor::accept_all(&mut ed, id, generation);
    pump(&mut ed, "the answer", |_| answered(41).is_some());
    assert!(!dir.join("b.rs").exists(), "y deleted it");
    let a = answered(41).unwrap().unwrap();
    assert!(a.contains("written: a.rs (1)") && a.contains("accepted by the user: b.rs"), "{a}");
    std::fs::remove_dir_all(&dir).ok();
}

/// frictions34 43: the review of an `edit` is titled with what the user asked, as the native Edit's is — not
/// `edit a.rs` (that stays the answer's and a card's); with nothing asked yet, the change itself.
#[test]
fn the_edit_tools_review_quotes_what_you_asked() {
    let dir = project("asked", &[("a.rs", "fn hi() {}\n"), ("b.rs", "fn bye() {}\n")]);
    let mut ed = chat_for(&dir, &dir.join("stdin.log"));
    let title = |ed: &Editor| ed.session.review.as_ref().map(|r| r.instruction.clone());
    let c = ed.session.chat.as_mut().unwrap();
    c.msgs.push(crate::chat::Msg::new(crate::chat::Role::User, "tidy the greeting\nand keep it short", ""));
    let args = json!({"edits": [{"file_path": "a.rs", "old_string": "fn hi()", "new_string": "fn hello()"}]});
    call(&mut ed, Call::test(43), &args).unwrap();
    pump(&mut ed, "the review", |ed| ed.session.review.is_some());
    assert_eq!(title(&ed).as_deref(), Some("tidy the greeting"));
    // (the answer still names the change, not what was asked: `n`, and Claude hears `edit a.rs`)
    let end = std::time::Instant::now() + crate::test_results::PATIENCE;
    while answered(43).is_none() {
        assert!(std::time::Instant::now() < end, "the answer");
        crate::llm::review_key(&mut ed, crate::key::Key::plain(crate::key::Code::Char('n')));
        if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(20)) {
            ed.handle_event(ev);
        }
    }
    let a = answered(43).unwrap().unwrap();
    assert!(a.contains("edit a.rs") && !a.contains("tidy the greeting"), "{a}");
    // (a chat where nothing was asked yet)
    let mut ed = chat_for(&dir, &dir.join("stdin2.log"));
    let args = json!({"edits": [{"file_path": "b.rs", "old_string": "fn bye()", "new_string": "fn ciao()"}]});
    call(&mut ed, Call::test(44), &args).unwrap();
    pump(&mut ed, "the other review", |ed| ed.session.review.is_some());
    assert_eq!(title(&ed).as_deref(), Some("edit b.rs"));
    std::fs::remove_dir_all(&dir).ok();
}

/// Call `n` of `tool` with `args`, as `tools::mcp` makes it: its row claimed by its input as it comes.
fn asked(ed: &mut Editor, n: u64, tool: &str, args: &Value) -> Call {
    crate::chat::claim_for(ed, tool, args, &json!(n));
    Call::test(n)
}

/// Every review that comes up answered `key` until `done` — pressed again while one's still up: a review takes no
/// key before it's settled (as `the_edit_tools_review_quotes_what_you_asked` answers it). After a key, what the
/// main loop does after every event: the next file's review up (`refactor::next`) — no other event may come.
fn answer_reviews(ed: &mut Editor, key: char, done: impl Fn(&Editor) -> bool) {
    let end = std::time::Instant::now() + crate::test_results::PATIENCE;
    while !done(ed) {
        assert!(std::time::Instant::now() < end, "the reviews answered `{key}`");
        if ed.session.review.as_ref().is_some_and(|r| r.ready) {
            crate::llm::review_key(ed, crate::key::Key::plain(crate::key::Code::Char(key)));
            crate::refactor::next(ed);
        }
        if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(20)) {
            ed.handle_event(ev);
        }
    }
}

/// 글타래 P6: Claude's `edit` answered on its card stamps its call's knot with the card's key — `y` accepts them
/// all, `n` none; one in its in-buffer review, with the review's key.
#[test]
fn an_edit_answered_is_stamped_on_its_knot_with_the_key() {
    let args = json!({"edits": [
        {"file_path": "a.rs", "old_string": "one", "new_string": "uno"},
        {"file_path": "b.rs", "old_string": "two", "new_string": "dos"},
    ]});
    let one = json!({"edits": [{"file_path": "a.rs", "old_string": "one", "new_string": "uno"}]});
    let stamps = |ed: &Editor| -> String {
        let c = ed.session.chat.as_ref().unwrap();
        c.msgs.iter().find(|m| m.call == "t1").unwrap().stamps.iter().map(|s| s.0).collect()
    };
    for (key, n, args) in [("y", 45, &args), ("n", 46, &args), ("review n", 47, &one)] {
        let dir = project(&format!("stamped-{n}"), &[("a.rs", "one\n"), ("b.rs", "two\n")]);
        let mut ed = chat_for(&dir, &dir.join("stdin.log"));
        // (its call, as claude's stream made it: the call waiting, its row)
        let c = ed.session.chat.as_mut().unwrap();
        c.calls.push(crate::chat::Called {
            id: "t1".into(),
            name: "mcp__tarae__edit".into(),
            input: args.clone(),
        });
        let look = crate::chat::Look {
            verb: "edit",
            what: "a.rs".into(),
            pattern: false,
            range: String::new(),
            link: None,
        };
        c.msgs.push(crate::chat::Msg::tool(look, "t1"));
        let asked = asked(&mut ed, n, "mcp__tarae__edit", args);
        call(&mut ed, asked, args).unwrap();
        match key {
            "review n" => {
                pump(&mut ed, "the review", |ed| ed.session.review.is_some());
                assert_eq!(stamps(&ed), "", "undecided: none");
                answer_reviews(&mut ed, 'n', |ed| ed.session.review.is_none());
            }
            _ => {
                pump(&mut ed, "the card", |ed| change_card(ed).is_some() || answered(n).is_some());
                assert!(change_card(&ed).is_some(), "the card ({key}): {:?}", answered(n));
                assert_eq!(stamps(&ed), "", "undecided: none");
                let (id, whose, _) = change_card(&ed).unwrap();
                match key {
                    "y" => crate::refactor::accept_all(&mut ed, id, whose),
                    _ => crate::refactor::decline_all(&mut ed, whose),
                }
            }
        }
        assert_eq!(stamps(&ed), key.trim_start_matches("review "), "{key}");
        // (its files written on a worker before the folder goes)
        pump(&mut ed, "the answer", |_| answered(n).is_some());
        std::fs::remove_dir_all(&dir).ok();
    }
}

/// The keys of a review go only on the call it's for: a review of your own change (a patch) stamps none of
/// Claude's calls waiting, even one naming that file; and in a batch with a rename waiting first, an edit's keys go
/// on the edit's row, not the rename's.
#[test]
fn a_reviews_keys_go_only_on_the_call_its_for() {
    use crate::chat::{Called, Look, Msg};
    let waiting = |ed: &mut Editor, id: &str, name: &str, input: Value| {
        let c = ed.session.chat.as_mut().unwrap();
        c.calls.push(Called { id: id.into(), name: name.into(), input });
        let look =
            Look { verb: "edit", what: "a.rs".into(), pattern: false, range: String::new(), link: None };
        c.msgs.push(Msg::tool(look, id));
    };
    let stamps = |ed: &Editor, id: &str| -> String {
        let c = ed.session.chat.as_ref().unwrap();
        c.msgs.iter().find(|m| m.call == id).unwrap().stamps.iter().map(|s| s.0).collect()
    };
    let edit = json!({"edits": [{"file_path": "a.rs", "old_string": "one", "new_string": "uno"}]});
    // Your own patch, answered `y`, with Claude's edit of the same file waiting
    let dir = project("own-review", &[("a.rs", "one\n")]);
    let mut ed = chat_for(&dir, &dir.join("stdin.log"));
    waiting(&mut ed, "t1", "mcp__tarae__edit", edit.clone());
    let file = crate::refactor::FileChange {
        path: dir.join("a.rs"),
        rel: "a.rs".into(),
        before: ropey::Rope::from_str("one\n"),
        after: ropey::Rope::from_str("uno\n"),
        places: 1,
        op: Op::Text,
        protected: false,
    };
    crate::refactor::land_yours(
        &mut ed,
        "p".into(),
        "apply p".into(),
        vec![file],
        String::new(),
        String::new(),
    );
    // (its card first: `r`, one by one — into its review)
    pump(&mut ed, "your card", |ed| change_card(ed).is_some() || ed.session.review.is_some());
    if let Some((id, whose, _)) = change_card(&ed) {
        crate::refactor::one_by_one(&mut ed, id, whose);
    }
    pump(&mut ed, "your review", |ed| ed.session.review.as_ref().is_some_and(|r| r.ready));
    answer_reviews(&mut ed, 'y', |ed| ed.session.review.is_none());
    assert_eq!(stamps(&ed, "t1"), "", "your answer is no key of Claude's call");
    std::fs::remove_dir_all(&dir).ok();
    // A batch: a rename waiting first, then the edit — the edit's `n` goes on the edit's row
    let dir = project("mixed-batch", &[("a.rs", "one\n")]);
    let mut ed = chat_for(&dir, &dir.join("stdin.log"));
    waiting(&mut ed, "r1", "mcp__tarae__rename", json!({"path": "b.rs", "symbol": "x", "new_name": "y"}));
    waiting(&mut ed, "t1", "mcp__tarae__edit", edit.clone());
    let asked48 = asked(&mut ed, 48, "mcp__tarae__edit", &edit);
    call(&mut ed, asked48, &edit).unwrap();
    answer_reviews(&mut ed, 'n', |_| answered(48).is_some());
    assert_eq!((stamps(&ed, "r1"), stamps(&ed, "t1")), (String::new(), "n".into()));
    std::fs::remove_dir_all(&dir).ok();
    // An edit no waiting call was made with (its input another): no stamp — not the rename's either
    let dir = project("unnamed", &[("a.rs", "one\n")]);
    let mut ed = chat_for(&dir, &dir.join("stdin.log"));
    waiting(&mut ed, "r1", "mcp__tarae__rename", json!({"path": "b.rs", "symbol": "x", "new_name": "y"}));
    let elsewhere = json!({"edits": [{"file_path": "other.rs", "old_string": "one", "new_string": "uno"}]});
    waiting(&mut ed, "t1", "mcp__tarae__edit", elsewhere);
    let asked49 = asked(&mut ed, 49, "mcp__tarae__edit", &edit);
    call(&mut ed, asked49, &edit).unwrap();
    answer_reviews(&mut ed, 'n', |_| answered(49).is_some());
    assert_eq!((stamps(&ed, "r1"), stamps(&ed, "t1")), (String::new(), String::new()));
    std::fs::remove_dir_all(&dir).ok();
}

/// tarae's `edit` sent as claude sends it (`tools/call` through `tools::mcp`): its call's row is claimed as the
/// request comes — by its tool's full name and the request's id — and takes its review's key.
#[test]
fn a_tool_call_claims_its_row_as_it_comes() {
    let dir = project("through-mcp", &[("a.rs", "one\n")]);
    let mut ed = chat_for(&dir, &dir.join("stdin.log"));
    let edit = json!({"edits": [{"file_path": "a.rs", "old_string": "one", "new_string": "uno"}]});
    let c = ed.session.chat.as_mut().unwrap();
    c.calls.push(crate::chat::Called {
        id: "t1".into(),
        name: "mcp__tarae__edit".into(),
        input: edit.clone(),
    });
    let look = crate::chat::Look {
        verb: "edit",
        what: "a.rs".into(),
        pattern: false,
        range: String::new(),
        link: None,
    };
    c.msgs.push(crate::chat::Msg::tool(look, "t1"));
    let generation = c.generation();
    let msg = json!({"jsonrpc": "2.0", "id": 61, "method": "tools/call",
        "params": {"name": "edit", "arguments": edit}});
    assert!(crate::tools::mcp(&mut ed, generation, "r61", &msg).is_none(), "answered after its review");
    answer_reviews(&mut ed, 'n', |_| answered(61).is_some());
    let c = ed.session.chat.as_ref().unwrap();
    let stamps: String = c.msgs.iter().find(|m| m.call == "t1").unwrap().stamps.iter().map(|s| s.0).collect();
    assert_eq!(stamps, "n");
    std::fs::remove_dir_all(&dir).ok();
}

/// A change set card's key goes on its own tool's call: a rename's `y` on the rename's row, an edit of the same
/// file waiting before it left alone. Its `y` is for the files it writes then — one held for your unsaved edits
/// takes its own review's keys — and it's the key that decided, kept when the write then finds the file changed.
#[test]
fn a_cards_key_goes_on_its_own_tools_call() {
    use crate::chat::{Called, Look, Msg};
    let waiting = |ed: &mut Editor, id: &str, name: &str, input: Value| {
        let c = ed.session.chat.as_mut().unwrap();
        c.calls.push(Called { id: id.into(), name: name.into(), input });
        let look =
            Look { verb: "edit", what: "a.rs".into(), pattern: false, range: String::new(), link: None };
        c.msgs.push(Msg::tool(look, id));
    };
    let stamps = |ed: &Editor, id: &str| -> String {
        let c = ed.session.chat.as_ref().unwrap();
        c.msgs.iter().find(|m| m.call == id).unwrap().stamps.iter().map(|s| s.0).collect()
    };
    let change = |dir: &Path, rel: &str| crate::refactor::FileChange {
        path: dir.join(rel),
        rel: rel.into(),
        before: ropey::Rope::from_str("one\n"),
        after: ropey::Rope::from_str("uno\n"),
        places: 1,
        op: Op::Text,
        protected: false,
    };
    let both = json!({"edits": [
        {"file_path": "a.rs", "old_string": "one", "new_string": "uno"},
        {"file_path": "b.rs", "old_string": "one", "new_string": "uno"},
    ]});
    // A rename's card, an edit of its first file waiting before it
    let dir = project("rename-card", &[("a.rs", "one\n"), ("b.rs", "one\n")]);
    let mut ed = chat_for(&dir, &dir.join("stdin.log"));
    waiting(&mut ed, "t1", "mcp__tarae__edit", both.clone());
    let rename = json!({"path": "a.rs", "symbol": "one", "new_name": "uno"});
    waiting(&mut ed, "r1", "mcp__tarae__rename", rename.clone());
    let files = vec![change(&dir, "a.rs"), change(&dir, "b.rs")];
    let asked50 = asked(&mut ed, 50, "mcp__tarae__rename", &rename);
    crate::refactor::land(&mut ed, asked50, "rename `one` → `uno`".into(), "place", files, String::new());
    pump(&mut ed, "the card", |ed| change_card(ed).is_some());
    let (id, whose, _) = change_card(&ed).unwrap();
    crate::refactor::accept_all(&mut ed, id, whose);
    assert_eq!((stamps(&ed, "r1"), stamps(&ed, "t1")), ("y".into(), String::new()));
    pump(&mut ed, "the rename's answer", |_| answered(50).is_some());
    std::fs::remove_dir_all(&dir).ok();
    // Two renames side by side, both waiting (their results after both), the one called second asked first and
    // landing first (parallel calls come in any order; its server answered sooner): each card's key on its own
    // call's row
    let dir =
        project("two-renames", &[("a.rs", "one\n"), ("b.rs", "one\n"), ("c.rs", "one\n"), ("d.rs", "one\n")]);
    let mut ed = chat_for(&dir, &dir.join("stdin.log"));
    let (first, second) = (
        json!({"path": "a.rs", "symbol": "one", "new_name": "uno"}),
        json!({"path": "c.rs", "symbol": "one", "new_name": "uno"}),
    );
    waiting(&mut ed, "r1", "mcp__tarae__rename", first.clone());
    waiting(&mut ed, "r2", "mcp__tarae__rename", second.clone());
    let asked54 = asked(&mut ed, 54, "mcp__tarae__rename", &second);
    let asked53 = asked(&mut ed, 53, "mcp__tarae__rename", &first);
    let files = vec![change(&dir, "c.rs"), change(&dir, "d.rs")];
    crate::refactor::land(&mut ed, asked54, "rename c".into(), "place", files, String::new());
    pump(&mut ed, "the second's card", |ed| change_card(ed).is_some());
    let (id, whose, _) = change_card(&ed).unwrap();
    crate::refactor::accept_all(&mut ed, id, whose);
    pump(&mut ed, "the second's answer", |_| answered(54).is_some());
    let files = vec![change(&dir, "a.rs"), change(&dir, "b.rs")];
    crate::refactor::land(&mut ed, asked53, "rename a".into(), "place", files, String::new());
    pump(&mut ed, "the first's card", |ed| change_card(ed).is_some());
    ed.offers.clear();
    crate::refactor::decline_all(&mut ed, whose);
    pump(&mut ed, "the first's answer", |_| answered(53).is_some());
    assert_eq!((stamps(&ed, "r1"), stamps(&ed, "r2")), ("n".into(), "y".into()));
    std::fs::remove_dir_all(&dir).ok();
    // Every file held for your unsaved edits: the card writes none — their reviews' keys alone
    let dir = project("held-card", &[("a.rs", "one\n"), ("b.rs", "one\n")]);
    let mut ed = chat_for(&dir, &dir.join("stdin.log"));
    for f in ["a.rs", "b.rs"] {
        ed.open(&dir.join(f)).unwrap();
        let typed = crate::transaction::Change { from: 3, to: 3, insert: "!".into() };
        ed.doc_mut().apply(&crate::transaction::Transaction::new(vec![typed]));
    }
    waiting(&mut ed, "t1", "mcp__tarae__edit", both.clone());
    let asked51 = asked(&mut ed, 51, "mcp__tarae__edit", &both);
    call(&mut ed, asked51, &both).unwrap();
    pump(&mut ed, "the card", |ed| change_card(ed).is_some());
    let (id, whose, _) = change_card(&ed).unwrap();
    crate::refactor::accept_all(&mut ed, id, whose);
    assert_eq!(stamps(&ed, "t1"), "", "nothing written yet: no key of the card's");
    answer_reviews(&mut ed, 'n', |_| answered(51).is_some());
    assert_eq!(stamps(&ed, "t1"), "n", "each file's own review");
    std::fs::remove_dir_all(&dir).ok();
    // Its `y`, the file changed on disk since: not written — the key that decided stays
    let dir = project("changed-card", &[("a.rs", "one\n"), ("b.rs", "one\n")]);
    let mut ed = chat_for(&dir, &dir.join("stdin.log"));
    waiting(&mut ed, "t1", "mcp__tarae__edit", both.clone());
    let asked52 = asked(&mut ed, 52, "mcp__tarae__edit", &both);
    call(&mut ed, asked52, &both).unwrap();
    pump(&mut ed, "the card", |ed| change_card(ed).is_some());
    std::fs::write(dir.join("a.rs"), "changed\n").unwrap();
    let (id, whose, _) = change_card(&ed).unwrap();
    crate::refactor::accept_all(&mut ed, id, whose);
    pump(&mut ed, "the changed file's answer", |_| answered(52).is_some());
    let a = answered(52).unwrap().unwrap();
    assert!(a.contains("changed since"), "{a}");
    assert_eq!(stamps(&ed, "t1"), "y");
    std::fs::remove_dir_all(&dir).ok();
}

/// A single delete comes up on the card even under review at the end, and `n` leaves the file.
#[test]
fn a_lone_delete_is_never_written_unseen() {
    let dir = project("lone", &[("b.rs", "kept\n")]);
    let mut ed = chat_for(&dir, &dir.join("stdin.log"));
    ed.session.chat.as_mut().unwrap().edits.at_end = Some(dir.clone());
    call(&mut ed, Call::test(42), &json!({"edits": [{"file_path": "b.rs", "delete": true}]})).unwrap();
    pump(&mut ed, "the card", |ed| change_card(ed).is_some());
    let (_, generation, _) = change_card(&ed).unwrap();
    ed.offers.clear();
    crate::refactor::decline_all(&mut ed, generation);
    pump(&mut ed, "the answer", |_| answered(42).is_some());
    assert_eq!(std::fs::read_to_string(dir.join("b.rs")).unwrap(), "kept\n");
    assert!(answered(42).unwrap().unwrap().contains("declined by the user, left as they were: b.rs"));
    std::fs::remove_dir_all(&dir).ok();
}

/// `space u` takes a turn's move and delete back: the deleted file is written again as it was, the moved one
/// is back at its place and gone from the new one (its new folder's file removed); `space U` does them again.
#[test]
fn undo_brings_a_deleted_file_back_and_a_moved_one_home() {
    let dir = project("undo", &[("a.rs", "fn a() {}\n"), ("b.rs", "bye\n")]);
    let mut ed = chat_for(&dir, &dir.join("stdin.log"));
    let args = json!({"edits": [
        {"file_path": "a.rs", "move_to": "sub/c.rs"},
        {"file_path": "sub/c.rs", "old_string": "fn a()", "new_string": "fn c()"},
        {"file_path": "b.rs", "delete": true},
    ]});
    call(&mut ed, Call::test(43), &args).unwrap();
    pump(&mut ed, "the card", |ed| change_card(ed).is_some());
    let (id, generation, _) = change_card(&ed).unwrap();
    crate::refactor::accept_all(&mut ed, id, generation);
    pump(&mut ed, "the answer", |_| answered(43).is_some());
    let read = |p: &str| std::fs::read_to_string(dir.join(p)).ok();
    assert_eq!((read("a.rs"), read("b.rs"), read("sub/c.rs")), (None, None, Some("fn c() {}\n".into())));
    assert!(crate::checkpoint::turn_over(&mut ed, "move and delete").is_some());
    crate::checkpoint::step(&mut ed, true).unwrap();
    assert_eq!(
        (read("a.rs"), read("b.rs"), read("sub/c.rs")),
        (Some("fn a() {}\n".into()), Some("bye\n".into()), None),
        "undone"
    );
    crate::checkpoint::step(&mut ed, false).unwrap();
    assert_eq!(
        (read("a.rs"), read("b.rs"), read("sub/c.rs")),
        (None, None, Some("fn c() {}\n".into())),
        "redone"
    );
    std::fs::remove_dir_all(&dir).ok();
}
