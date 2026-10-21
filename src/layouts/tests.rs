use super::*;
use crate::config::Config;

fn waits_for(path: &Path, what: impl Fn(&Map<String, Value>) -> bool) -> Map<String, Value> {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let kept = read(Some(path));
        if what(&kept) || std::time::Instant::now() > end {
            return kept;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// A write replaces only its own checkout's key — another tarae's, written since this one read the file, stays —
/// and an editor started after another (a test's, a simulation's run again) has its writes taken as the newer.
#[test]
fn a_write_keeps_the_other_checkouts_layouts_and_a_later_editors_wins() {
    let dir = crate::scratch::Scratch::new("layouts-merge");
    let root = dir.to_path_buf();
    let a = root.join("a.rs");
    std::fs::write(&a, "fn a() {}\n").unwrap();
    let file = root.join("layouts.json");
    let key = root.to_str().unwrap().to_string();
    let editor = || {
        let mut ed = Editor::new(Config::default());
        ed.session.root = root.clone();
        ed.layouts_path = Some(file.clone());
        ed.layouts = read(Some(&file));
        ed.open(&a).unwrap();
        ed
    };
    let mut first = editor();
    for _ in 0..3 {
        first.split_view(crate::split::Dir::Vertical);
        first.layout_keep();
    }
    waits_for(&file, |k| k.get(&key).is_some_and(|l| l["views"].as_array().is_some_and(|v| v.len() == 4)));
    // (another checkout's tarae writes its own since)
    let mut all = read(Some(&file));
    all.insert("/elsewhere".into(), json!({ "views": [] }));
    std::fs::write(&file, Value::Object(all).to_string()).unwrap();
    // (a later editor's first write — its own count starts at one — lands, and keeps the other's)
    let mut later = editor();
    later.layout_keep();
    let kept = waits_for(&file, |k| {
        k.get(&key).is_some_and(|l| l["views"].as_array().is_some_and(|v| v.len() == 1))
    });
    assert_eq!(kept[&key]["views"].as_array().map(Vec::len), Some(1), "the later editor's layout: {kept:?}");
    assert!(kept.contains_key("/elsewhere"), "the other checkout's stays");
}

/// §5.1: a checkout's panes are written as they change (a worker's write — not while only the start screen's scratch
/// is open), and come back with its files on the next start; a pane whose file is gone goes with it.
#[test]
fn the_layout_is_kept_as_it_changes_and_comes_back_with_the_files() {
    let dir = crate::scratch::Scratch::new("layouts");
    let root = dir.to_path_buf();
    let (a, b) = (root.join("a.rs"), root.join("b.rs"));
    std::fs::write(&a, "fn a() {}\n").unwrap();
    std::fs::write(&b, "fn b() {}\n").unwrap();
    let file = root.join("layouts.json");
    let started = |files: &[&Path]| {
        let mut ed = Editor::new(Config::default());
        ed.session.root = root.clone();
        ed.layouts_path = Some(file.clone());
        ed.layouts = read(Some(&file));
        let files: Vec<&str> = files.iter().map(|f| f.to_str().unwrap()).collect();
        let folders =
            json!({ root.to_str().unwrap(): { "files": files, "current": files.len().saturating_sub(1) } });
        ed.restore = crate::restore::State::from_json(json!({ "folders": folders }));
        ed
    };
    let mut ed = started(&[]);
    ed.layout_keep();
    assert!(ed.layouts.is_empty(), "the start screen's scratch: nothing kept");
    ed.open(&a).unwrap();
    ed.split_view(crate::split::Dir::Vertical);
    ed.open(&b).unwrap();
    ed.sync_focus_view();
    ed.layout_keep();
    let seq = ed.layouts_seq;
    ed.layout_keep();
    assert_eq!(ed.layouts_seq, seq, "unchanged: not written again");
    let key = root.to_str().unwrap().to_string();
    let kept = waits_for(&file, |k| {
        k.get(&key).is_some_and(|l| l["views"].as_array().is_some_and(|v| v.len() == 2))
    });
    assert!(kept[&key]["split"].to_string().contains("\"v\""), "side by side: {}", kept[&key]);
    // Started again, the files reopened: the two panes, the one last focused on b
    let mut ed = started(&[&a, &b]);
    assert_eq!(ed.restore_files(), 2);
    assert_eq!(ed.session.views.len(), 2, "both panes back");
    assert_eq!(ed.doc().path.as_deref(), Some(b.as_path()));
    let shown: Vec<_> = ed
        .session
        .views
        .iter()
        .filter_map(|v| ed.docs.iter().find(|d| d.id == v.doc)?.path.clone())
        .collect();
    assert!(shown.contains(&a) && shown.contains(&b), "{shown:?}");
    assert!(ed.session.split.leaves().iter().all(|id| ed.session.shows(*id).is_some()), "every leaf a pane");
    // A file gone: its pane with it, the other taking the room
    std::fs::remove_file(&b).unwrap();
    let mut ed = started(&[&a, &b]);
    assert_eq!(ed.restore_files(), 1);
    assert_eq!(ed.session.views.len(), 1);
    assert_eq!(ed.doc().path.as_deref(), Some(a.as_path()));
}
