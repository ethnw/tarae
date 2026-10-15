//! `cargo xtask glyphs [chars]` — whether each font CLAUDE.md's glyph rule names (JetBrains Mono, Hack, Menlo,
//! SF Mono) maps each character, read from the cmap of the font files installed on this machine (fontdue builds its
//! map from every cmap subtable, through ttf-parser). Every face of every file of a family is read: a family has a
//! character when all of them map it. A family with no file here is "unknown", never "yes".

use std::path::{Path, PathBuf};

use crate::Result;

/// What's asked when no characters are given: box junctions, dashed lines, square corners (the diagram plan's P0).
const ASKED: &str = "├┤┬┴┼┄┆╌╎┌┐└┘";

/// The families, and how their files' names start.
const FAMILIES: [(&str, &[&str]); 4] = [
    ("JetBrains Mono", &["JetBrainsMono"]),
    ("Hack", &["Hack-"]),
    ("Menlo", &["Menlo."]),
    ("SF Mono", &["SFNSMono", "SF-Mono-", "SFMono-"]),
];

fn dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    vec![
        home.join("Library/Fonts"),
        "/Library/Fonts".into(),
        "/System/Library/Fonts".into(),
        // (SF Mono ships inside Terminal.app)
        "/System/Applications/Utilities/Terminal.app/Contents/Resources/Fonts".into(),
        home.join(".local/share/fonts"),
        home.join(".fonts"),
        "/usr/share/fonts".into(),
        "/usr/local/share/fonts".into(),
    ]
}

/// One face read: its file, its index in the file, its name, and the characters asked that it maps.
struct Face {
    file: PathBuf,
    index: u32,
    name: String,
    has: Vec<bool>,
}

pub fn run(args: &[String]) -> Result<()> {
    let asked: Vec<char> = match args.is_empty() {
        true => ASKED.chars().collect(),
        false => args.iter().flat_map(|a| a.chars()).collect(),
    };
    let mut files = Vec::new();
    let mut unread = Vec::new();
    for d in dirs() {
        walk(&d, 3, &mut files, &mut unread);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let shown = |p: &Path| {
        let s = p.display().to_string();
        match s.strip_prefix(&home) {
            Some(rest) if !home.is_empty() => format!("~{rest}"),
            _ => s,
        }
    };
    let mut families: Vec<(&str, Vec<Face>)> = Vec::new();
    let mut failed = Vec::new();
    for (family, starts) in FAMILIES {
        let mut faces = Vec::new();
        for file in files.iter().filter(|f| {
            let name = f.file_name().and_then(|n| n.to_str()).unwrap_or("");
            starts.iter().any(|s| name.starts_with(s))
        }) {
            let bytes = match std::fs::read(file) {
                Ok(b) => b,
                Err(e) => {
                    failed.push(format!("{}: {e}", shown(file)));
                    continue;
                }
            };
            for index in 0..faces_in(&bytes) {
                let settings = fontdue::FontSettings { collection_index: index, ..Default::default() };
                match fontdue::Font::from_bytes(bytes.as_slice(), settings) {
                    Ok(font) => faces.push(Face {
                        file: file.clone(),
                        index,
                        name: font.name().unwrap_or("?").to_string(),
                        has: asked.iter().map(|&c| font.has_glyph(c)).collect(),
                    }),
                    Err(e) => failed.push(format!("{} face {index}: {e}", shown(file))),
                }
            }
        }
        families.push((family, faces));
    }

    println!("Faces read (cmap):");
    for (family, faces) in &families {
        if faces.is_empty() {
            println!("  {family:<16}none installed here");
        }
        for f in faces {
            let lacks: String = asked.iter().zip(&f.has).filter(|(_, h)| !**h).map(|(c, _)| *c).collect();
            let lacks = if lacks.is_empty() { String::new() } else { format!("  — lacks {lacks}") };
            println!("  {family:<16}{} #{} — {}{lacks}", shown(&f.file), f.index, f.name);
        }
    }
    for d in &unread {
        println!("  not readable: {d}");
    }
    for f in &failed {
        println!("  not parsed: {f}");
    }
    println!();
    print!("{:<8}", "");
    for (family, _) in &families {
        print!("{family:<16}");
    }
    println!();
    for (i, c) in asked.iter().enumerate() {
        print!("{c}  {:<5}", format!("{:04X}", *c as u32));
        for (_, faces) in &families {
            let n = faces.iter().filter(|f| f.has[i]).count();
            let verdict = match (faces.len(), n) {
                (0, _) => "unknown".to_string(),
                (all, n) if n == all => "yes".to_string(),
                (_, 0) => "no".to_string(),
                (all, n) => format!("{n}/{all} faces"),
            };
            print!("{verdict:<16}");
        }
        println!();
    }
    Ok(())
}

/// The faces in a font file: a collection (`ttcf`) says how many; anything else is one.
fn faces_in(bytes: &[u8]) -> u32 {
    match bytes.get(..12) {
        Some(h) if &h[..4] == b"ttcf" => u32::from_be_bytes([h[8], h[9], h[10], h[11]]),
        _ => 1,
    }
}

/// Font files below `dir` (`depth` folders down at most); a folder there but not readable goes to `unread`.
fn walk(dir: &Path, depth: u8, out: &mut Vec<PathBuf>, unread: &mut Vec<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => return unread.push(format!("{}: {e}", dir.display())),
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir() {
            if depth > 0 {
                walk(&path, depth - 1, out, unread);
            }
        } else if path
            .extension()
            .and_then(|x| x.to_str())
            .is_some_and(|x| ["ttf", "otf", "ttc"].contains(&x.to_ascii_lowercase().as_str()))
        {
            out.push(path);
        }
    }
}
