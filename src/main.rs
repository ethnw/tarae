//! tarae (타래) — a selection-first modal editor carrying on helix's spirit and keymap.

// (std's `sort_by` family is `sorting::by`'s in the binary — clippy.toml; tests sort as they like: what's only
// built with them isn't in it, and the binary's own build still checks the rest)
#![cfg_attr(test, allow(clippy::disallowed_methods, reason = "tests: not in the release binary"))]

mod allow;
mod attach;
mod basetree;
mod beneath;
mod billing;
mod blocked;
mod builds;
mod calls;
mod cards;
mod cardwarn;
mod chat;
mod chats;
mod checkpoint;
mod chrome;
mod claudehome;
mod claudetrust;
mod claudeupdate;
mod clipboard;
mod cmdline;
mod codereview;
mod commands;
mod comment;
mod compaction;
mod completion;
mod config;
mod conflict;
mod containers;
mod dap;
mod debugpack;
mod diffview;
mod disk;
mod doccomment;
mod document;
mod dsyms;
mod editdiff;
mod editor;
mod edits;
mod edittool;
mod ended;
mod event;
mod features;
mod fetch;
mod filetree;
mod filters;
mod follow;
mod fswatch;
mod git;
mod gitmenu;
mod gitpanel;
mod gitrepo;
mod gittool;
mod gittrust;
mod grammar;
mod graphemes;
mod grep;
mod hooks;
mod hosts;
mod inbox;
mod indent;
mod install;
mod java;
mod jumplist;
mod key;
mod keymap;
mod labels;
#[cfg(target_os = "linux")]
mod landlock;
mod languages;
mod limits;
mod llm;
mod log;
mod lsp;
mod lsp_editor;
mod markdown;
mod mcp;
mod models;
mod movement;
mod nav;
mod nofile;
mod notes;
mod offer;
mod outlines;
mod packs;
mod pairs;
mod paths;
mod picker;
mod plan;
mod plugin;
mod preflight;
mod procs;
mod pty;
mod queued;
mod quickfix;
mod readonly;
mod reads;
mod readtool;
mod reaper;
mod rebase;
mod recent;
mod recovery;
mod refactor;
mod repeat;
mod replace;
mod replay;
mod restart;
mod restore;
mod reviewer;
mod risk;
mod runhomes;
mod runtime;
mod sandbox;
mod sandboxpack;
mod saver;
mod screen;
mod search;
#[cfg(target_os = "linux")]
mod seccomp;
mod secretnames;
mod seeing;
mod selection;
mod session;
mod settings;
mod sha256;
mod share;
mod shell;
mod sidebar;
mod signature;
mod sinceyes;
mod sizes;
mod snapshot;
mod sorting;
mod spawn;
mod split;
mod structure;
mod stuck;
mod switchcost;
mod syntax;
mod task;
mod term;
mod terminal;
mod test_results;
mod testcode;
mod testing;
mod textobject;
mod theme;
mod tokens;
mod tools;
mod trace;
mod transaction;
mod trust;
mod typed;
mod uisocket;
mod uistate;
mod undofile;
mod usage;
mod viewalign;
mod virtualdoc;
mod waits;
mod wasm;
mod worktree;
mod wrap;

use std::path::PathBuf;

use anyhow::Result;

const USAGE: &str = "\
tarae — a selection-first modal editor in the spirit of Helix

usage: tarae [FILE]...
       tarae FOLDER                          work in FOLDER (its config, its last files)
       tarae --diff OLD NEW                  NEW, with its changes from OLD shown in place
                                             (git difftool -x 'tarae --diff')
       tarae grammar install [LANG]...       fetch and build syntax grammars (no LANG = all;
                                             needs git and a C compiler) → ~/.local/share/tarae
       tarae grammar list                    which grammars are installed
       tarae plugin new NAME                 start a plugin (Rust → WebAssembly) from the template
       tarae plugin dev [DIR]                build it on every save; a running tarae reloads it
       tarae plugin list                     installed plugins and their commands
                                             (~/.config/tarae/plugins)
       tarae state [--json] [--pid PID]      a running tarae's UI state: where the keys go, its
                   [--after-input N]         cards, sessions and toasts (state --help)

config: $XDG_CONFIG_HOME/tarae/config.toml (default ~/.config/tarae/config.toml)
        project overrides: .tarae.toml (searched upward from the current directory)
        inside tarae: :set <path> [value], :set! (also saves), :toggle, :config-show";

fn main() -> Result<()> {
    // The shim (`tarae --nofile N <path> <argv0> args…`, `nofile`): a child of tarae's given back the descriptor limit
    // tarae started with, then exec'd — before anything of the editor reads its environment or config
    nofile::shim_main();
    // A session's keeper (Linux's sandbox): nothing of the editor runs in it
    #[cfg(target_os = "linux")]
    if std::env::args_os().nth(1).is_some_and(|a| a == sandbox::keeper::FLAG) {
        sandbox::keeper::main();
    }
    // (what tarae was handed open goes no further than tarae)
    spawn::close_on_exec();
    restart::remember_exe();
    let mut files = Vec::new();
    let mut diff_against: Option<PathBuf> = None;
    // `:restart`: the state the last tarae left (restart.rs)
    let (mut restarted, mut old_children) = (None, Vec::new());
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--restart" => {
                old_children = restart::reap();
                restarted = args.next().and_then(|f| restart::read(std::path::Path::new(&f)));
            }
            "--diff" => match (args.next(), args.next()) {
                (Some(old), Some(new)) => {
                    diff_against = Some(PathBuf::from(old));
                    files.push(PathBuf::from(new));
                }
                _ => {
                    eprintln!("usage: tarae --diff OLD NEW");
                    std::process::exit(2);
                }
            },
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            "grammar" if files.is_empty() => {
                let rest: Vec<String> = std::env::args().skip(2).collect();
                return grammar::cli(&rest);
            }
            "plugin" if files.is_empty() => {
                let rest: Vec<String> = std::env::args().skip(2).collect();
                return plugin::cli(&rest);
            }
            "state" if files.is_empty() => {
                let rest: Vec<String> = std::env::args().skip(2).collect();
                return uisocket::cli(&rest);
            }
            "-V" | "--version" => {
                println!("tarae {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            _ => files.push(PathBuf::from(arg)),
        }
    }

    // A folder (`tarae .`, `tarae ~/src/app`): tarae runs there, as if started in it — its project config and
    // the files last open there (the files given beside it, read from where you were)
    let mut folder = None;
    files = files
        .into_iter()
        .filter_map(|f| match f.is_dir() {
            true => {
                folder.get_or_insert(f);
                None
            }
            false => Some(std::path::absolute(&f).unwrap_or(f)),
        })
        .collect();
    if let Some(dir) = &folder
        && let Err(e) = std::env::set_current_dir(dir)
    {
        eprintln!("tarae: {}: {e}", dir.display());
        std::process::exit(2);
    }

    // (the editor's alone, before any thread starts: the soft limit on open descriptors up to the hard one, and
    // tarae's binary pinned for the shim — said once the log is. A subcommand that ends at once — `state`, `grammar`,
    // `plugin`, `--version` — raises nothing and pins nothing: it would leave a pin no prune of its sees)
    let raised = nofile::raise();
    // (before any thread starts — SIGTERM and SIGHUP quit as `:q!` does, ending what tarae started)
    term::catch_signals();
    // (Linux: what Claude's runs leave running stays tarae's — served, reaped, ended with its session)
    reaper::at_start();
    // The UI state's socket — now, while no thread runs (its umask, and for a long path its folder, are the
    // process's for a moment); served once the editor is there
    let ui = uisocket::bind();
    // (threads may start now: the last tarae's children still running, waited for as they end — #136)
    restart::wait_for(old_children);
    if let Some(said) = raised {
        log::line("tarae", "info", &said);
    }
    // Theme "default" (meok/hanji) follows the terminal background — before reading config (config reads it).
    // A restart knows it already (and the query's answer could mix with keys typed meanwhile).
    let light = restarted.as_ref().and_then(|s| s["light"].as_bool());
    if let Some(light) = light.or_else(term::detect_light_background) {
        theme::set_light(light);
    }
    // (the one look at where tarae runs — from here on the editor's root is what everything goes by)
    let root = editor::started_in();
    // The language table — the packs, a runtime folder's read from disk here, once: files opened, fences drawn
    // and servers started from now on read memory (the config checks its servers against it)
    let packs = languages::get();
    let (config, warnings) = config::load(&root, &config::Sources::own());
    let mut editor = editor::Editor::with_root(config, root);
    // (where answers about repositories' own git commands are kept — `gitrepo` reads them as it resolves)
    gitrepo::set_trust_store(editor.trust_path.clone());
    // (and where a resolve that finds them unanswered puts the card up — any repository, any git call)
    gitrepo::set_asker(std::path::Path::new("/"), editor.events.shared_sender());
    // (the egress proxy asks the user about hosts through the main loop — before any run can start one)
    hosts::listen(editor.events.shared_sender());
    // Started without a file: recent files on the start screen (existing ones only; reads one small file)
    editor.recent = recent::load();
    editor.restore = restore::State::load();
    for f in &files {
        if let Err(e) = editor.open(f) {
            editor.set_error(format!("{e:#}"));
        }
    }
    if let Some(state) = &restarted {
        restart::restore(&mut editor, state);
    } else if files.is_empty() {
        // Started without files: the start screen (offering this folder's last files — `r`), or those
        // files right away if `editor.restore-files` is on
        editor.find_restorable();
        if editor.config.restore_files {
            editor.restore_files();
        }
    }
    // This repository's sessions from last time — a restart brought them already
    if !editor.session_store.restored {
        editor.sessions_restore();
    }
    // Claude's notes from last time in this folder (the open files get them right away), and its turns, for
    // `space u` and `space v t` (a restart's, or ones a crash cut short) — the checkout tarae started in;
    // the other sessions' came back with them
    editor.session_kept_back(0);
    // Unsaved edits a tarae that's gone kept (a crash, a kill) — a card each (`recovery.rs`)
    editor.recovery_scan();
    // Kept run homes no run needs any more — unused, replaced, their project gone (`runhomes.rs`; a worker's)
    let days = editor.config.llm.sandbox_kept_days;
    editor.events.jobs().background(move || runhomes::prune_now(days));
    // Run folders a tarae that's gone left in tarae's cache (`sandbox::prune_runs`; a worker's)
    editor.events.jobs().background(sandbox::prune_runs);
    // (and the pins of tarae's binary such a tarae left — macOS's shim, `nofile::pin`)
    editor.events.jobs().background(nofile::prune_pins);
    if !files.is_empty() {
        editor.current = 0;
    }
    if let Some(old) = diff_against {
        let old = std::fs::canonicalize(&old).unwrap_or(old);
        if let Err(e) = editor.diff_against(&old.to_string_lossy()) {
            editor.set_error(e);
        }
    }
    if !warnings.is_empty() {
        editor.set_error(format!("config: {}", warnings.join("; ")));
    }
    if !packs.said.is_empty() {
        editor.set_warning(format!("packs: {}", packs.said.join("; ")));
    }
    editor.disk_watch();
    match ui {
        Ok(listener) => uisocket::serve(listener, editor.events.shared_sender()),
        Err(e) => log::line("ui", "info", &format!("no UI state socket: {e}")),
    }
    let result = term::run(&mut editor);
    // What Claude's runs left running ends with tarae — a `:restart` too (their supervisors don't survive the
    // exec: a daemon kept would have every connect refused)
    sandbox::end_leftovers(None);
    // (`:restart`'s new binary binds its own at the same path; a failed one ends here too)
    uisocket::remove();
    editor.save_restore();
    // The last saves' undo history is still being written on workers — exiting or exec'ing cuts it off
    editor.undo_flush();
    // Copies of unsaved buffers: removed by a quit; kept, written to the last key, for `:restart`'s exec (and
    // its failure), an error, or a signal (`Recovery.keep`)
    let restarting = editor.restart.as_ref().is_some_and(|r| r.ready().is_some());
    editor.recovery_end(result.is_err() || restarting);
    // (a `:restart`'s new tarae pins its own binary)
    nofile::unpin();
    if result.is_ok()
        && let Some(bin) = editor.restart.as_ref().and_then(|r| r.ready()).map(PathBuf::from)
    {
        // Returns only if the new tarae couldn't take over
        eprintln!(
            "tarae: {} — run tarae again; r on its start screen reopens your files",
            restart::exec(editor, &bin)
        );
        std::process::exit(1);
    }
    result
}
