//! `:` typed commands. Names and aliases match helix.

use std::path::{Path, PathBuf};

use crate::editor::Editor;
use crate::{config, settings};

pub fn execute(editor: &mut Editor, line: &str) -> Result<(), String> {
    let line = line.trim();
    // (a shuttle's key never runs a line that changes what tarae may do — however it came: typed, a key bound to
    // it, the palette; `shuttle.rs`)
    if let Some(why) = crate::shuttle::typed_refused(editor, line) {
        return Err(why);
    }
    let (cmd, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let rest = rest.trim();
    match cmd {
        "" => Ok(()),
        "w" | "write" if rest.is_empty() && editor.format_then_save(false) => Ok(()),
        "w" | "write" => write(editor, rest, false),
        "w!" | "write!" => write(editor, rest, true),
        "reload" => editor.reload_current(),
        "redraw" => {
            editor.redraw = true;
            Ok(())
        }
        "vs" | "vsplit" | "hs" | "sp" | "hsplit" | "split" => {
            let dir = if cmd.starts_with('v') {
                crate::split::Dir::Vertical
            } else {
                crate::split::Dir::Horizontal
            };
            editor.split_view(dir);
            if !rest.is_empty() {
                editor.open(std::path::Path::new(rest)).map_err(|e| format!("{e:#}"))?;
            }
            Ok(())
        }
        "only" => {
            editor.only_view();
            Ok(())
        }
        "close" => {
            if !editor.close_view() {
                return Err("only one window".into());
            }
            Ok(())
        }
        "tutor" => {
            editor.open_tutor();
            Ok(())
        }
        // Attach the debugger to a running program — a name from the config, or host:port (attach.rs)
        "attach" => editor.attach_command(rest),
        // Debugger watch expressions: `:watch expr` adds, `:unwatch [expr]` removes (all if no expr)
        "watch" if !rest.is_empty() => {
            editor.add_watch(rest);
            Ok(())
        }
        "watch" => Err("usage: :watch <expression>".into()),
        "unwatch" => {
            editor.remove_watch(rest);
            editor.note(if rest.is_empty() {
                "Cleared watches".into()
            } else {
                format!("Stopped watching {rest}")
            });
            Ok(())
        }
        "theme" if rest.is_empty() => {
            editor.run_command_by_name("theme_picker");
            Ok(())
        }
        // `:theme name` = until quit only (to save: `:set! theme name` or the picker)
        "theme" => set(editor, &format!("theme {rest}"), false),
        // A commit message: `:q` cancels (closes it), `:wq`/`:x` commits — not the whole editor
        "q" | "quit" | "q!" | "quit!" if editor.in_commit_message() => {
            let then_end = editor.session.committing.take().is_some_and(|c| c.then_end);
            editor.close_current(true)?;
            editor.set_status(match then_end {
                true => "commit canceled — the session stays, its changes staged",
                false => "commit canceled",
            });
            Ok(())
        }
        "wq" | "x" | "write-quit" if editor.in_commit_message() => write(editor, rest, false),
        // A rebase's commit message (`m` on its card): `:q` leaves it as it was, `:wq` goes on with it
        "q" | "quit" | "q!" | "quit!" if editor.in_rebase_message() => editor.rebase_message_left(),
        "wq" | "x" | "write-quit" if editor.in_rebase_message() => write(editor, rest, false),
        // With several panes :q closes only the current pane (as in Helix), :qa closes all
        "q" | "quit" if editor.close_view() => Ok(()),
        "q" | "quit" | "qa" | "quit-all" => quit(editor, false),
        "q!" | "quit!" | "qa!" | "quit-all!" => quit(editor, true),
        // Become the tarae at `editor.restart-binary`, as you were (restart.rs) — built first when
        // `editor.restart-build` is set, unless `!`
        "restart" => crate::restart::start(editor, true),
        "restart!" => crate::restart::start(editor, false),
        // The project's checks, one after another in the task panel (preflight.rs): the quick set, or all of
        // them before publishing
        "preflight" => editor.preflight_start(crate::preflight::Set::Quick),
        "preflight-full" => editor.preflight_start(crate::preflight::Set::Full),
        "preflight-output" => editor.preflight_output(rest),
        "wa" | "write-all" => write_all(editor, false),
        "wqa" | "xa" | "write-quit-all" => write_all(editor, true),
        "wq" | "x" | "write-quit" if rest.is_empty() && editor.format_then_save(true) => Ok(()),
        "wq" | "x" | "write-quit" => {
            write(editor, rest, false)?;
            quit(editor, false)
        }
        "o" | "open" | "e" | "edit" => {
            let paths = shell_words(rest);
            if paths.is_empty() {
                return Err("usage: :open <path>...".into());
            }
            for p in paths {
                let path = typed_path(editor, &p);
                match at_line(&path, |p| p.exists()) {
                    Some((path, line, col)) => editor.open_at(&path, line, col),
                    None => editor.open(&path),
                }
                .map_err(|e| format!("{e:#}"))?;
            }
            Ok(())
        }
        "n" | "new" => {
            editor.new_scratch();
            Ok(())
        }
        "bc" | "bclose" | "buffer-close" => editor.close_current(false),
        "bc!" | "bclose!" | "buffer-close!" => editor.close_current(true),
        "bn" | "bnext" | "buffer-next" => {
            editor.cycle_buffer(1);
            Ok(())
        }
        "bp" | "bprev" | "buffer-previous" => {
            editor.cycle_buffer(-1);
            Ok(())
        }
        "sh" | "run-shell-command" => shell(editor, rest),
        "diff" => editor.diff_against(rest.trim()),
        "diff-off" => editor.diff_off(),
        "session-open" if !rest.trim().is_empty() => {
            let id = editor.session_open(Path::new(rest.trim()))?;
            let all = editor.sessions_in_order();
            let at = all.iter().position(|l| l.id == id).map_or(all.len(), |i| i + 1);
            let msg =
                format!("{} · session {at} of {} — space W lists them", editor.session.label(), all.len());
            editor.set_status(msg);
            Ok(())
        }
        "session-open" => Err("usage: :session-open <folder> (a worktree or another checkout)".into()),
        "session-switch" => {
            let sessions = editor.sessions_listed();
            match rest.trim().parse() {
                Ok(id) if editor.session_switch(id) => Ok(()),
                _ => {
                    let list: Vec<String> = sessions
                        .iter()
                        .map(|(id, name, front)| format!("{id} {name}{}", if *front { " ●" } else { "" }))
                        .collect();
                    Err(format!("usage: :session-switch <n> — {}", list.join(" · ")))
                }
            }
        }
        // Making, ending, bringing in a session — a branch in a checkout of its own (worktree.rs)
        "session-new" => {
            match rest.split_whitespace().collect::<Vec<_>>()[..] {
                [name] => editor.session_new(name, None),
                [name, base] => editor.session_new(name, Some(base)),
                [] => editor.session_prompt(),
                _ => return Err("usage: :session-new <name> [<base>]".into()),
            }
            Ok(())
        }
        "session-end" => editor.session_end(rest),
        "session-bring" => editor.session_bring(rest),
        "session-apply" => editor.session_apply(rest),
        "session-billing" => editor.session_billing(rest),
        // A session's rebase stopped at a conflict (rebase.rs)
        "rebase-continue" => editor.rebase_by_hand(rest, true),
        "rebase-abort" => editor.rebase_by_hand(rest, false),
        "git-switch" if !rest.is_empty() => {
            editor.git_switch(rest);
            Ok(())
        }
        "git-switch" => {
            editor.git_branch_picker();
            Ok(())
        }
        "git-branch-new" => {
            match rest.split_whitespace().collect::<Vec<_>>()[..] {
                [name] => editor.git_branch_new(name, None),
                [name, from] => editor.git_branch_new(name, Some(from)),
                _ => return Err("usage: :git-branch-new <name> [<from>]".into()),
            }
            Ok(())
        }
        "git-branch-rename" => {
            match rest.split_whitespace().collect::<Vec<_>>()[..] {
                [new] => editor.git_branch_rename(None, new),
                [old, new] => editor.git_branch_rename(Some(old), new),
                _ => return Err("usage: :git-branch-rename [<branch>] <new name>".into()),
            }
            Ok(())
        }
        "git-branch-delete" => match rest.split_whitespace().collect::<Vec<_>>()[..] {
            [name] => {
                editor.git_branch_delete(name, false);
                Ok(())
            }
            _ => Err("usage: :git-branch-delete <branch>".into()),
        },
        "git-amend" => {
            editor.git_amend();
            Ok(())
        }
        "task" | "make" => match rest.trim() {
            "" => {
                editor.task_picker();
                Ok(())
            }
            name => editor.task_named(name),
        },
        "pipe" | "|" | "pipe-to" | "insert-output" | "append-output" => {
            use crate::shell::Pipe;
            let kind = match cmd {
                "pipe-to" => Pipe::To,
                "insert-output" => Pipe::Insert,
                "append-output" => Pipe::Append,
                _ => Pipe::Replace,
            };
            editor.shell_pipe(kind, rest);
            Ok(())
        }
        "set" => set(editor, rest, false),
        "set!" => set(editor, rest, true),
        "toggle" => toggle(editor, rest),
        "config-show" => {
            let text = settings::show(&editor.config);
            editor.new_scratch();
            editor.doc_mut().text = ropey::Rope::from_str(&text);
            Ok(())
        }
        "config-open" => {
            let path = config::config_path().ok_or("no config directory")?;
            editor.open(&path).map_err(|e| format!("{e:#}"))
        }
        "project-trust" => editor.project_trust_ask(),
        "project-untrust" => editor.project_untrust(),
        "claude-settings" => editor.claude_settings_ask(),
        "git-trust" => editor.git_trust_ask(),
        "claude-update" => crate::claudeupdate::update(editor),
        "claude-restart" => {
            crate::chat::restart_all(editor);
            Ok(())
        }
        "granted" | "sandbox-allowed" => {
            editor.granted();
            Ok(())
        }
        "sandbox-disallow" => editor.sandbox_disallow(rest),
        "chat-screen" => crate::chat::attach_screen(editor),
        "chat-point" => {
            crate::uistate::point::start(editor);
            Ok(())
        }
        "sandbox-blocked" => {
            editor.sandbox_blocked(crate::blocked::Which::Either);
            Ok(())
        }
        "sandbox-env" => {
            // (on a worker: `Here` reads the temp folder's real path)
            let llm = editor.config.llm.clone();
            editor.events.jobs().spawn(move || {
                let page = crate::sandbox::run_env_shown(&crate::sandbox::Here::now(), &llm).page();
                move |ed: &mut Editor| {
                    ed.new_scratch();
                    ed.doc_mut().text = ropey::Rope::from_str(&page);
                }
            });
            Ok(())
        }
        "config-reload" => {
            let (root, from) = (editor.started_root().to_path_buf(), editor.config_sources());
            editor.events.jobs().spawn(move || {
                let (c, w) = config::load(&root, &from);
                move |ed: &mut Editor| ed.reload_config(c, w)
            });
            Ok(())
        }
        "messages" => {
            editor.messages_picker();
            Ok(())
        }
        "inbox" => {
            editor.inbox_open();
            Ok(())
        }
        "usage" => {
            crate::usage::open(editor);
            Ok(())
        }
        "features" => {
            editor.features_open();
            Ok(())
        }
        "shuttle-end" => {
            crate::shuttle::end(editor);
            Ok(())
        }
        "features-reset" => {
            editor.features_reset();
            Ok(())
        }
        "log-open" => {
            let path = crate::log::path().ok_or("no state directory for the log")?;
            if !path.exists() {
                return Err("no log yet".into());
            }
            editor.open(&path).map_err(|e| format!("{e:#}"))
        }
        "lsp-restart" => editor.lsp_restart(),
        "procs" => {
            editor.procs_toggle();
            Ok(())
        }
        "plugin-permissions" => editor.plugin_permissions_list(),
        "plugin-revoke" => editor.plugin_revoke(rest),
        "plugin-approve" => editor.plugin_approve(rest),
        "lsp-stop" => {
            let name = editor.lsp_stop()?;
            editor.set_status(format!("{name} stopped"));
            Ok(())
        }
        "format" | "fmt" => {
            let extra = serde_json::json!({ "options": editor.format_options(editor.doc().id) });
            editor.lsp_request(crate::lsp_editor::Kind::Format, "textDocument/formatting", extra);
            Ok(())
        }
        "grammar-install" => editor.grammar_install(rest),
        "lang" | "set-language" => {
            let spec = crate::syntax::spec(rest).ok_or_else(|| format!("unknown language '{rest}'"))?;
            let id = editor.doc().id;
            editor.load_language(id, spec);
            Ok(())
        }
        "ask" => crate::llm::ask(editor, rest),
        "ask-cancel" => {
            crate::llm::cancel(editor);
            Ok(())
        }
        "chat" => {
            crate::chat::open(editor);
            if !rest.is_empty()
                && let Some(c) = &mut editor.session.chat
            {
                c.input = rest.to_string();
                c.cursor = rest.len();
                crate::chat::send(editor);
            }
            Ok(())
        }
        "chat-close" => {
            crate::chat::close(editor);
            Ok(())
        }
        "chat-new" => {
            crate::chat::reset(editor);
            Ok(())
        }
        "review" => {
            let branch = rest.split_whitespace().next().map(str::to_string);
            crate::codereview::start(editor, branch);
            Ok(())
        }
        "review-claude" => {
            let branch = rest.split_whitespace().next().map(str::to_string);
            crate::codereview::ask_claude(editor, branch);
            Ok(())
        }
        "review-board" => {
            let branch = rest.split_whitespace().next().map(str::to_string);
            crate::codereview::ask_board(editor, branch);
            Ok(())
        }
        "review-send" => {
            crate::codereview::send(editor);
            Ok(())
        }
        "review-close" => {
            crate::codereview::close_asked(editor);
            Ok(())
        }
        "review-hunk-seen" => {
            crate::codereview::hunk_seen(editor);
            Ok(())
        }
        "review-file-seen" => {
            crate::codereview::file_seen(editor);
            Ok(())
        }
        "review-verdict" => {
            crate::reviewer::verdict(editor);
            Ok(())
        }
        "reviewer-stop" => {
            crate::reviewer::stop(editor);
            Ok(())
        }
        "review-turn" => {
            crate::codereview::review_turn(editor);
            Ok(())
        }
        "soft-wrap" => {
            // (on · off · nothing: flip — this buffer's, the setting stays)
            let arg = rest.trim();
            let want = ["off", "on"].iter().position(|w| *w == arg).map(|i| i == 1);
            if want.is_none() && !arg.is_empty() {
                return Err(format!("soft-wrap: on or off, not {arg}"));
            }
            editor.soft_wrap_toggle(want);
            Ok(())
        }
        "review-at-end" => {
            // (on · off · nothing: flip)
            let arg = rest.trim();
            let want = ["off", "on"].iter().position(|w| *w == arg).map(|i| i == 1);
            if want.is_none() && !arg.is_empty() {
                return Err(format!("review-at-end: on or off, not {arg}"));
            }
            crate::edits::at_end_toggle(editor, want);
            Ok(())
        }
        "keep-warm" => {
            // (on · off · nothing: flip)
            let arg = rest.trim();
            if arg == "now" {
                return crate::keepwarm::now(editor);
            }
            let want = ["off", "on"].iter().position(|w| *w == arg).map(|i| i == 1);
            if want.is_none() && !arg.is_empty() {
                return Err(format!("keep-warm: on, off or now, not {arg}"));
            }
            crate::keepwarm::mark(editor, want);
            Ok(())
        }
        "note-close" => crate::notes::close_here(editor),
        "chat-resume" => {
            match rest.split_whitespace().next() {
                Some(id) => crate::chat::resume(editor, id),
                None => crate::chat::past_picker(editor),
            }
            Ok(())
        }
        "model" => match rest.split_whitespace().next() {
            Some(m) => crate::models::set(editor, m),
            None => {
                crate::models::picker(editor);
                Ok(())
            }
        },
        "effort" => match rest.split_whitespace().next() {
            Some(l) => crate::models::set_effort(editor, l),
            None => {
                crate::models::effort_picker(editor);
                Ok(())
            }
        },
        "commit" => editor.commit_from_review(rest),
        "commit-draft" => {
            editor.commit_draft();
            Ok(())
        }
        "undo-turn" => crate::checkpoint::step(editor, true),
        "redo-turn" => crate::checkpoint::step(editor, false),
        // (`:notes-clear reviewer`: only the notes the reviewer began — #197)
        // (your unsent drafts among them: a card asks first)
        "notes-clear" => {
            let by = match rest.split_whitespace().next() {
                Some(word) => {
                    Some(crate::notes::by_word(word).ok_or("notes-clear takes reviewer, author or you")?)
                }
                None => None,
            };
            crate::notes::close_by(editor, by);
            Ok(())
        }
        n if n.parse::<usize>().is_ok() => {
            editor.goto_line(n.parse::<usize>().unwrap().saturating_sub(1));
            Ok(())
        }
        // Commands plugins registered (`:name args`, `":name"` in a keymap)
        name if editor.plugin_command(name).is_some() => editor.plugin_run(name, rest),
        _ => Err(format!("no such command: '{cmd}'")),
    }
}

/// `:set path` = show current value and description, `:set path value` = until quit only,
/// `:set! path value` = also write it to the file.
fn set(editor: &mut Editor, rest: &str, persist: bool) -> Result<(), String> {
    let (path, raw) = rest.split_once(char::is_whitespace).map_or((rest, ""), |(p, v)| (p, v.trim()));
    if path.is_empty() {
        return Err("usage: :set <path> [value]   (:config-show lists all)".into());
    }
    let setting = settings::find(path).ok_or_else(|| format!("unknown setting '{path}' (:config-show)"))?;
    if raw.is_empty() {
        let cur = (setting.get)(&editor.config);
        editor.set_status(format!("{path} = {cur}   — {} ({})", setting.doc, setting.kind.describe()));
        return Ok(());
    }
    let v = settings::parse_value(raw);
    set_value(editor, path, v, persist)
}

fn set_value(editor: &mut Editor, path: &str, v: toml::Value, persist: bool) -> Result<(), String> {
    settings::apply(&mut editor.config, path, &v)?;
    editor.ensure_theme();
    editor.overrides.retain(|(p, _)| p != path);
    if persist {
        let file = config::persist(path, &v)?;
        editor.set_status(format!("{path} = {v}  (saved to {})", file.display()));
    } else {
        editor.set_status(format!("{path} = {v}  (until you quit — :set! to save)"));
        editor.overrides.push((path.to_string(), v));
    }
    Ok(())
}

/// Bools are flipped; choice settings advance to the next value.
fn toggle(editor: &mut Editor, rest: &str) -> Result<(), String> {
    let path = rest.trim();
    let setting = settings::find(path).ok_or_else(|| format!("unknown setting '{path}'"))?;
    let cur = (setting.get)(&editor.config);
    let next = match (&setting.kind, &cur) {
        (settings::Kind::Bool, toml::Value::Boolean(b)) => toml::Value::Boolean(!b),
        (settings::Kind::Enum(opts), toml::Value::String(s)) => {
            let i = opts.iter().position(|o| o == s).unwrap_or(0);
            toml::Value::from(opts[(i + 1) % opts.len()])
        }
        _ => return Err(format!("{path} is not a toggle ({})", setting.kind.describe())),
    };
    set_value(editor, path, next, false)
}

fn write(editor: &mut Editor, path: &str, force: bool) -> Result<(), String> {
    let id = editor.doc().id;
    let target = match shell_words(path).into_iter().next() {
        Some(p) => {
            let p = std::path::absolute(typed_path(editor, &p)).map_err(|e| e.to_string())?;
            let p = std::fs::canonicalize(&p).unwrap_or(p); // same form as `open`
            (editor.doc().path.as_ref() != Some(&p)).then_some(p)
        }
        None => None,
    };
    match target {
        // Save as: the new file's own stamp counts (not the old file's), and the name changes only once
        // it's written
        Some(new) => {
            // (written here: a save of the old name still under way finishes first)
            editor.flush_saves();
            let doc = editor.doc_mut();
            let (old, disk, conflict) = (doc.path.replace(new.clone()), doc.disk.take(), doc.disk_conflict);
            doc.disk_conflict = false;
            let saved = doc.save_as(force);
            doc.path = old;
            if let Err(e) = saved {
                (doc.disk, doc.disk_conflict) = (disk, conflict);
                return Err(format!("{e:#}"));
            }
            // Servers forget the old name; language, server and git base follow the new one
            editor.lsp_did_close(id);
            editor.doc_mut().path = Some(new);
            editor.attach_syntax(id);
            editor.attach_lsp(id);
            editor.git_load_base(id);
        }
        None => return save_doc(editor, id, force),
    }
    saved(editor, id);
    Ok(())
}

/// `:wa` — every modified buffer with a file (not ones still loading); errors are collected.
/// `:wa` / `:wqa` — every modified file, each formatted first like `:w` (those save when their server
/// answers; `:wqa` quits after the last).
fn write_all(editor: &mut Editor, and_quit: bool) -> Result<(), String> {
    let ids: Vec<_> = editor
        .docs
        .iter()
        .filter(|d| d.is_modified() && d.path.is_some() && !d.loading && d.virtual_uri.is_none())
        .map(|d| d.id)
        .collect();
    let (mut failed, mut formatting) = (Vec::new(), 0);
    for &id in &ids {
        if editor.format_then_save_doc(id, and_quit) {
            formatting += 1;
        } else if let Err(e) = save_doc(editor, id, false) {
            failed.push(e);
        }
    }
    if !failed.is_empty() {
        return Err(failed.join(" · "));
    }
    // ("Saved N files" comes when they're written)
    if ids.is_empty() {
        editor.set_success("nothing to save");
    }
    if and_quit && formatting == 0 {
        return quit(editor, false);
    }
    Ok(())
}

/// Save document `id` to its own path (also when it isn't the current one — format-on-save finishes later).
/// Written in the background: "Saved" and what follows a save come when it's on disk (`saver.rs`).
pub(crate) fn save_doc(editor: &mut Editor, id: crate::document::DocId, force: bool) -> Result<(), String> {
    editor.save_in_background(id, force, crate::saver::Then::Command)
}

fn saved(editor: &mut Editor, id: crate::document::DocId) {
    let Some(doc) = editor.docs.iter().find(|d| d.id == id) else { return };
    let msg = format!("Saved {} · {} lines", doc.display_name(&editor.session.root), doc.text.len_lines());
    editor.set_success(msg);
    editor.lsp_did_save(id);
    editor.undo_persist(id);
    editor.git_saved(id);
    editor.rebase_saved(id);
    editor.plugin_event(crate::plugin::PluginEvent::Saved, id);
}

pub(crate) fn quit(editor: &mut Editor, force: bool) -> Result<(), String> {
    // Saves still being written finish first (`:wq` saved a moment ago) — a disk that doesn't answer keeps
    // tarae open unless it's `:q!`
    if !editor.flush_saves() && !force {
        return Err("still saving — the disk is slow; try again, or :q! to quit anyway".into());
    }
    if !force {
        let dirty: Vec<String> = editor
            .docs
            .iter()
            .filter(|d| d.is_modified() && !d.throwaway)
            .map(|d| d.display_name(&editor.session.root))
            .collect();
        if !dirty.is_empty() {
            return Err(format!("unsaved changes: {} (use :q! to discard)", dirty.join(", ")));
        }
    }
    editor.should_quit = true;
    Ok(())
}

/// Runs a shell command **in the background** and puts the first output line in the status line
/// (helix freezes the editor here).
/// Used by keymaps to open zellij/tmux popups.
fn shell(editor: &mut Editor, cmd: &str) -> Result<(), String> {
    if cmd.is_empty() {
        return Err("usage: :sh <command>".into());
    }
    let (cmd, dir) = (cmd.to_string(), editor.session.root.clone());
    editor.events.jobs().spawn(move || {
        let result = run_shell(&cmd, &dir);
        move |ed: &mut Editor| match result {
            Ok(Some(line)) => ed.set_status(line),
            Ok(None) => {}
            Err(e) => ed.set_error(e),
        }
    });
    Ok(())
}

/// `:sh`'s command, run by your shell in `dir` (the editor's root).
fn run_shell(cmd: &str, dir: &std::path::Path) -> Result<Option<String>, String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".into());
    // (the descriptor limit tarae started with — `nofile`)
    let out =
        crate::nofile::command(shell, Some(dir)).arg("-c").arg(cmd).output().map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(if out.status.success() { &out.stdout } else { &out.stderr });
    let first = text.lines().find(|l| !l.trim().is_empty()).map(str::to_string);
    if out.status.success() {
        Ok(first)
    } else {
        Err(first.unwrap_or_else(|| format!("command failed: {}", out.status)))
    }
}

/// A path typed after `:open` / `:w`: home spelled out, then from the root.
pub(crate) fn typed_path(editor: &Editor, p: &str) -> PathBuf {
    editor.path_of(&expand_home(p, crate::allow::home().as_deref()))
}

/// A typed path with home spelled out: `~`, `~/…`, `$HOME`, `${HOME}` at its start (`home`: the user's
/// home folder; without one, the path as typed).
fn expand_home(p: &str, home: Option<&Path>) -> PathBuf {
    let rest = ["~", "$HOME", "${HOME}"].iter().find_map(|h| {
        let rest = p.strip_prefix(h)?;
        (rest.is_empty() || rest.starts_with('/')).then(|| rest.trim_start_matches('/'))
    });
    match (rest, home) {
        (Some(""), Some(home)) => home.to_path_buf(),
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(p),
    }
}

/// `file:12` / `file:12:5` (1-based line and column, as compilers and greps print them) when `file`
/// exists and the whole doesn't: the file and the 0-based line and column.
fn at_line(path: &Path, exists: impl Fn(&Path) -> bool) -> Option<(PathBuf, usize, usize)> {
    if exists(path) {
        return None;
    }
    let s = path.to_str()?;
    let num = |s: &str| s.parse::<usize>().ok().filter(|&n| n > 0);
    let (head, last) = s.rsplit_once(':')?;
    let last = num(last)?;
    let (file, line, col) = match head.rsplit_once(':').and_then(|(f, l)| Some((f, num(l)?))) {
        Some((file, line)) if exists(Path::new(file)) => (file, line, last),
        _ => (head, last, 1),
    };
    exists(Path::new(file)).then(|| (PathBuf::from(file), line - 1, col - 1))
}

/// Whitespace splitting + backslash escapes + quotes. Accepts paths the yazi picker passes via `printf %q`.
pub fn shell_words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') | (None, '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
                in_word = true;
            }
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                in_word = true;
            }
            (None, c) if c.is_whitespace() => {
                if in_word {
                    out.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            (None, c) => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_words_handles_escapes_and_quotes() {
        assert_eq!(shell_words(r"a b\ c 'd e' "), vec!["a", "b c", "d e"]);
        assert_eq!(shell_words(r#""x \"y\"""#), vec![r#"x "y""#]);
        assert!(shell_words("   ").is_empty());
    }

    /// #53: `~` and `$HOME` at a path's start are the home folder — not a folder named `~` in the project.
    #[test]
    fn home_is_expanded_at_a_paths_start() {
        let home = Path::new("/h/me");
        for (typed, want) in [
            ("~/.config/tarae/config.toml", "/h/me/.config/tarae/config.toml"),
            ("~", "/h/me"),
            ("$HOME/a", "/h/me/a"),
            ("${HOME}/a", "/h/me/a"),
            ("~other/a", "~other/a"),
            ("$HOMEY/a", "$HOMEY/a"),
            ("src/~/a", "src/~/a"),
        ] {
            assert_eq!(expand_home(typed, Some(home)), PathBuf::from(want), "{typed}");
        }
        assert_eq!(expand_home("~/a", None), PathBuf::from("~/a"), "no home: as typed");
    }

    /// #52: `file:line[:col]` opens the file there, when the file exists and the whole doesn't.
    #[test]
    fn a_line_after_an_existing_file_is_where_it_opens() {
        let files = ["/p/src/typed.rs", "/p/odd:7", "/p/both", "/p/both:2"];
        let exists = |p: &Path| files.iter().any(|f| Path::new(f) == p);
        let at = |s: &str| at_line(Path::new(s), exists);
        assert_eq!(at("/p/src/typed.rs:302"), Some(("/p/src/typed.rs".into(), 301, 0)));
        assert_eq!(at("/p/src/typed.rs:302:5"), Some(("/p/src/typed.rs".into(), 301, 4)));
        assert_eq!(at("/p/src/typed.rs"), None, "no suffix");
        assert_eq!(at("/p/odd:7"), None, "a file whose name ends so is that file");
        assert_eq!(at("/p/both:2"), None, "… even when the name without it is a file too");
        assert_eq!(at("/p/odd:7:3"), Some(("/p/odd:7".into(), 2, 0)));
        assert_eq!(at("/p/new.rs:3"), None, "a new file: its name as typed");
        assert_eq!(at("/p/src/typed.rs:0"), None, "lines count from 1");
        assert_eq!(at("/p/src/typed.rs:x"), None);
    }

    /// `:open` with both, end to end: the file itself, at its line — no buffer named `typed.rs:3`.
    #[test]
    fn open_takes_a_line_and_home() {
        let scratch = crate::scratch::Scratch::new("open-line");
        let dir = scratch.to_path_buf();
        std::fs::write(dir.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let mut ed = Editor::new(config::Config::default());
        ed.session.root = dir.clone();
        execute(&mut ed, "open a.txt:3:2").unwrap();
        assert_eq!(ed.doc().path.as_deref(), Some(dir.join("a.txt").as_path()));
        let head = ed.doc().selection().primary().head;
        assert_eq!(head, "one\ntwo\nt".len(), "line 3, column 2");
        assert!(ed.docs.iter().all(|d| d.path.as_deref() != Some(dir.join("a.txt:3:2").as_path())));
        // (`:open` and `:w` read a path the same way: home first — only computed here, nothing written)
        if let Some(home) = crate::allow::home() {
            assert_eq!(typed_path(&ed, "~/x.toml"), home.join("x.toml"));
        }
        assert_eq!(typed_path(&ed, "b/x.toml"), dir.join("b/x.toml"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
