//! tarae (타래) — a selection-first modal editor carrying on helix's spirit and keymap.

mod agent;
mod attach;
mod chat;
mod chats;
mod checkpoint;
mod clipboard;
mod cmdline;
mod codereview;
mod commands;
mod comment;
mod completion;
mod config;
mod conflict;
mod dap;
mod diffview;
mod disk;
mod doccomment;
mod document;
mod editdiff;
mod editor;
mod edits;
mod event;
mod filetree;
mod follow;
mod fswatch;
mod git;
mod gitmenu;
mod gitpanel;
mod grammar;
mod graphemes;
mod indent;
mod install;
mod java;
mod jumplist;
mod key;
mod keymap;
mod labels;
mod llm;
mod log;
mod lsp;
mod lsp_editor;
mod markdown;
mod models;
mod movement;
mod notes;
mod offer;
mod packs;
mod pairs;
mod picker;
mod plan;
mod plugin;
mod quickfix;
mod recent;
mod repeat;
mod replace;
mod restart;
mod runtime;
mod sandbox;
mod saver;
mod screen;
mod search;
mod selection;
mod session;
mod settings;
mod shell;
mod sidebar;
mod signature;
mod split;
mod structure;
mod syntax;
mod task;
mod term;
mod terminal;
mod test_results;
mod testing;
mod textobject;
mod theme;
mod tools;
mod transaction;
mod trust;
mod typed;
mod undofile;
mod viewalign;
mod wasm;
mod wrap;
mod ws;

use std::path::PathBuf;

use anyhow::Result;

const USAGE: &str = "\
tarae — a selection-first modal editor in the spirit of Helix

usage: tarae [FILE]...
       tarae FOLDER                          work in FOLDER (its config and session)
       tarae --diff OLD NEW                  NEW, with its changes from OLD shown in place
                                             (git difftool -x 'tarae --diff')
       tarae grammar install [LANG]...       fetch and build syntax grammars (no LANG = all;
                                             needs git and a C compiler) → ~/.local/share/tarae
       tarae grammar list                    which grammars are installed
       tarae plugin new NAME                 start a plugin (Rust → WebAssembly) from the template
       tarae plugin dev [DIR]                build it on every save; a running tarae reloads it
       tarae plugin list                     installed plugins and their commands
                                             (~/.config/tarae/plugins)

config: $XDG_CONFIG_HOME/tarae/config.toml (default ~/.config/tarae/config.toml)
        project overrides: .tarae.toml (searched upward from the current directory)
        inside tarae: :set <path> [value], :set! (also saves), :toggle, :config-show";

fn main() -> Result<()> {
    restart::remember_exe();
    let mut files = Vec::new();
    let mut diff_against: Option<PathBuf> = None;
    // `:restart`: the state the last tarae left (restart.rs)
    let mut restarted = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--restart" => {
                restart::reap();
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
            "-V" | "--version" => {
                println!("tarae {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            _ => files.push(PathBuf::from(arg)),
        }
    }

    // A folder (`tarae .`, `tarae ~/src/app`): tarae runs there, as if started in it — its project config and
    // session (the files given beside it, read from where you were)
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

    // Theme "default" (meok/hanji) follows the terminal background — before reading config (config reads it).
    // A restart knows it already (and the query's answer could mix with keys typed meanwhile).
    let light = restarted.as_ref().and_then(|s| s["light"].as_bool());
    if let Some(light) = light.or_else(term::detect_light_background) {
        theme::set_light(light);
    }
    let (config, warnings) = config::load();
    let mut editor = editor::Editor::new(config);
    // Started without a file: recent files on the start screen (existing ones only; reads one small file)
    editor.recent = recent::load();
    editor.session = session::State::load();
    for f in &files {
        if let Err(e) = editor.open(f) {
            editor.set_error(format!("{e:#}"));
        }
    }
    if let Some(state) = &restarted {
        restart::restore(&mut editor, state);
    } else if files.is_empty() {
        // Started without files: the start screen (offering this folder's last session — `r`), or that
        // session right away if `editor.restore-session` is on
        editor.find_last_session();
        if editor.config.restore_session {
            editor.restore_session();
        }
    }
    // Claude's notes from last time in this folder (the open files get them right away)
    notes::restore(&mut editor);
    // Claude's turns here, for `space u` and `space v t` (a restart's, or ones a crash cut short)
    checkpoint::restore(&mut editor);
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
    editor.disk_watch();
    editor.agent_start();
    let result = term::run(&mut editor);
    editor.save_session();
    // The last saves' undo history is still being written on workers — exiting or exec'ing cuts it off
    editor.undo_flush();
    editor.agent = None; // remove the lock file
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
