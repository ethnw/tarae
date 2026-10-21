//! Themes — TOML. `"scope" = "color"` or `{ fg, bg, bold, italic, underline, dim, reversed, crossed }`
//! (also `modifiers = [...]`·`underline = { color, style = "curl" }`), color = `#rrggbb`·`#rgb`·ANSI name·
//! `[palette]` name (palette entries may point at each other — `accent = "coral"`), `inherits = "meok"`
//! layers over another theme. Bad color names and unknown attributes are not silently dropped — reported
//! via `Theme::warnings`.
//! tarae-only UI keys: `ui.accent` (the single accent color), `ui.tint` (base for blending light colors —
//! transparent variants), `ui.panel` (the docked panels' ground under `ui.edge = "lines"`), `ui.virtual.strong`
//! (the dark wash: hint keys), `ui.edge` (a word, not a style — `Edge`: `"lines"` or `"ground"`).
//! Four built-ins: `meok` (먹, dark) · `hanji` (한지, light) · `meok-transparent` · `hanji-transparent`
//! (don't paint the background, the terminal's shows through). `"default"` = meok/hanji to match the terminal
//! (`set_light` — term.rs asks at startup).
//! Otherwise only `~/.config/tarae/themes/*.toml` (Helix folders are not read — user decision 2026-09-27).
//! Scope names are looked up by trimming from the end (`keyword.control.import` → `keyword.control` →
//! `keyword`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crossterm::style::Color;
use toml::Value;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub dim: bool,
    pub reversed: bool,
    pub crossed: bool,
    /// Underline color (diagnostics), curly underline.
    pub ul_color: Option<Color>,
    pub curly: bool,
}

impl Style {
    #[cfg(test)]
    pub const fn fg(c: Color) -> Self {
        Self {
            fg: Some(c),
            bg: None,
            bold: false,
            italic: false,
            underline: false,
            dim: false,
            reversed: false,
            crossed: false,
            ul_color: None,
            curly: false,
        }
    }

    /// Overrides only what `other` sets (modifiers are merged).
    pub fn patch(self, other: Style) -> Style {
        Style {
            fg: other.fg.or(self.fg),
            bg: other.bg.or(self.bg),
            bold: self.bold || other.bold,
            italic: self.italic || other.italic,
            underline: self.underline || other.underline,
            dim: self.dim || other.dim,
            reversed: self.reversed || other.reversed,
            crossed: self.crossed || other.crossed,
            ul_color: other.ul_color.or(self.ul_color),
            curly: self.curly || other.curly,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Theme {
    pub name: String,
    /// This theme as read, apart from every other read (a copy keeps it): what was drawn in it is kept by it
    /// (the chat's laid-out messages) — a theme file read again under the same name is another.
    pub id: u64,
    styles: HashMap<String, Style>,
    /// Where surfaces meet (`ui.edge`).
    pub edge: Edge,
    /// Problems hit while reading (bad color names·unknown attributes) — shown as config warnings.
    pub warnings: Vec<String>,
    captures: CaptureStyles,
}

/// How docked surfaces meet and floating ones stand apart (`ui.edge`, design pass 2 D2/D3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Edge {
    /// One thin line where surfaces meet (`│`, `─`, joins), the docked panels on the ground (`ui.panel`); what
    /// takes keys floats in a frame with its name.
    #[default]
    Lines,
    /// The half-cell ground between surfaces of their own colour; cards with half-block edges.
    Ground,
}

/// `Theme::capture_styles`' table — rebuilt when languages bring new capture names; a copy starts empty.
#[derive(Debug, Default)]
struct CaptureStyles(std::sync::Mutex<std::sync::Arc<Vec<Option<Style>>>>);

impl Clone for CaptureStyles {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl Theme {
    /// Exactly that name, otherwise trimming dot by dot.
    pub fn try_get(&self, scope: &str) -> Option<Style> {
        let mut s = scope;
        loop {
            if let Some(st) = self.styles.get(s) {
                return Some(*st);
            }
            s = &s[..s.rfind('.')?];
        }
    }

    pub fn get(&self, scope: &str) -> Style {
        self.try_get(scope).unwrap_or_default()
    }

    /// Tests: `scope` drawn as `st` from now on (a theme unlike the built-ins, without a file).
    #[cfg(test)]
    pub fn set(&mut self, scope: &str, st: Style) {
        self.styles.insert(scope.to_string(), st);
    }

    /// Global capture number → style (`syntax::highlights` spans carry the number). Every frame and code
    /// block wants it, so it's built once — and again only when a language brings new capture names.
    pub fn capture_styles(&self) -> std::sync::Arc<Vec<Option<Style>>> {
        let mut table = self.captures.0.lock().unwrap_or_else(|e| e.into_inner());
        if table.len() != crate::syntax::capture_count() {
            *table = std::sync::Arc::new(crate::syntax::capture_styles(|n| self.try_get(n)));
        }
        table.clone()
    }

    /// Built-in default theme — meok/hanji to match the terminal (also fills UI keys other themes lack).
    pub fn builtin() -> Theme {
        let name = if is_light() { "hanji" } else { "meok" };
        let table = builtin_table(name).expect("builtin theme");
        from_table(name, &table)
    }
}

impl Default for Theme {
    fn default() -> Self {
        Theme::builtin()
    }
}

/// Built-in theme sources (name, TOML) — transparent variants are made from these (`builtin_table`).
const SOURCES: [(&str, &str); 2] =
    [("meok", include_str!("themes/meok.toml")), ("hanji", include_str!("themes/hanji.toml"))];

/// Built-in themes (name, human-readable name).
pub const BUILTIN: [(&str, &str); 4] = [
    ("meok", "먹"),
    ("hanji", "한지"),
    ("meok-transparent", "먹 · 투명"),
    ("hanji-transparent", "한지 · 투명"),
];

static LIGHT: AtomicBool = AtomicBool::new(false);

/// Whether the terminal background is light — decides whether `"default"` is meok or hanji.
pub fn set_light(light: bool) {
    LIGHT.store(light, Ordering::Relaxed);
}

pub fn is_light() -> bool {
    LIGHT.load(Ordering::Relaxed)
}

fn builtin_table(name: &str) -> Option<toml::Table> {
    let name = if name == "default" { if is_light() { "hanji" } else { "meok" } } else { name };
    let (base, transparent) = match name.strip_suffix("-transparent") {
        Some(b) => (b, true),
        None => (name, false),
    };
    let src = SOURCES.iter().find(|(n, _)| *n == base)?.1;
    let mut table: toml::Table = toml::from_str(src).expect("builtin theme parses");
    if transparent {
        // Don't paint the background (the terminal's shows through). The blend base is the original
        // background — `ui.tint` (tarae-only key)
        if let Some(bg) =
            table.get_mut("ui.background").and_then(Value::as_table_mut).and_then(|t| t.remove("bg"))
        {
            let mut tint = toml::Table::new();
            tint.insert("bg".into(), bg);
            table.insert("ui.tint".into(), Value::Table(tint));
        }
        // (the docked panels' ground is the background: not painted either)
        if let Some(panel) = table.get_mut("ui.panel").and_then(Value::as_table_mut) {
            panel.remove("bg");
        }
    }
    Some(table)
}

pub fn theme_dirs() -> Vec<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    config.map(|c| c.join("tarae/themes")).into_iter().collect()
}

/// Selectable theme names: the four built-ins first, then `~/.config/tarae/themes` (sorted, deduped).
pub fn available() -> Vec<String> {
    let mut names: Vec<String> = BUILTIN.iter().map(|(n, _)| n.to_string()).collect();
    let mut found: Vec<String> = theme_dirs()
        .into_iter()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flatten()
        .filter_map(|e| {
            let p = e.ok()?.path();
            (p.extension()? == "toml").then(|| p.file_stem()?.to_str().map(str::to_string)).flatten()
        })
        .filter(|n| !names.contains(n))
        .collect();
    crate::sorting::sort(&mut found);
    found.dedup();
    names.extend(found);
    names
}

/// Loads a theme by name (reads files — call from a worker thread).
pub fn load(name: &str) -> Result<Theme, String> {
    if name == "default" {
        return Ok(Theme::builtin());
    }
    let table = load_table(&theme_dirs(), name, 0)?;
    Ok(from_table(name, &table))
}

/// Table with child laid over parent, following `inherits` upward (palette merged key by key).
fn load_table(dirs: &[PathBuf], name: &str, depth: usize) -> Result<toml::Table, String> {
    if depth > 8 {
        return Err(format!("theme '{name}': inherits too deep"));
    }
    // A same-named theme in the user folder wins (customizing a built-in)
    let user = dirs.iter().map(|d| d.join(format!("{name}.toml"))).find(|p| p.is_file());
    if user.is_none()
        && let Some(t) = builtin_table(name)
    {
        return Ok(t);
    }
    let path = user.ok_or_else(|| format!("theme '{name}' not found"))?;
    let src = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut table: toml::Table =
        toml::from_str(&src).map_err(|e| format!("{}: {}", path.display(), e.message()))?;
    let Some(parent) = table.remove("inherits").and_then(|v| v.as_str().map(str::to_string)) else {
        return Ok(table);
    };
    // `meok.toml` with `inherits = "meok"` = customizing the built-in one (not itself again)
    let mut base = match builtin_table(&parent).filter(|_| parent == name) {
        Some(t) => t,
        None => load_table(dirs, &parent, depth + 1)?,
    };
    for (k, v) in table {
        match (k.as_str(), v, base.get_mut("palette")) {
            ("palette", Value::Table(p), Some(Value::Table(bp))) => bp.extend(p),
            (_, v, _) => {
                base.insert(k, v);
            }
        }
    }
    Ok(base)
}

fn from_table(name: &str, table: &toml::Table) -> Theme {
    let mut warnings = Vec::new();
    let palette = parse_palette(table.get("palette").and_then(Value::as_table), &mut warnings);
    let mut styles = HashMap::new();
    let edge = match table.get("ui.edge") {
        None => Edge::default(),
        Some(Value::String(s)) if s == "lines" => Edge::Lines,
        Some(Value::String(s)) if s == "ground" => Edge::Ground,
        Some(v) => {
            warnings.push(format!("theme '{name}': \"ui.edge\": {v} — \"lines\" or \"ground\""));
            Edge::default()
        }
    };
    for (k, v) in table.iter().filter(|(k, _)| !matches!(k.as_str(), "palette" | "ui.edge")) {
        let mut warn = |what: String| warnings.push(format!("theme '{name}': \"{k}\": {what}"));
        if let Some(st) = parse_style(v, &palette, &mut warn) {
            styles.insert(k.clone(), st);
        }
    }
    static READ: AtomicU64 = AtomicU64::new(1);
    let id = READ.fetch_add(1, Ordering::Relaxed);
    Theme { name: name.to_string(), id, styles, edge, warnings, captures: CaptureStyles::default() }
}

/// Palette — values may be other palette names (`accent = "coral"`, any depth, warns on cycles).
fn parse_palette(table: Option<&toml::Table>, warnings: &mut Vec<String>) -> HashMap<String, Color> {
    let Some(table) = table else { return HashMap::new() };
    let mut out = HashMap::new();
    for (k, v) in table {
        let mut at = v.as_str();
        let mut seen = vec![k.as_str()];
        let color = loop {
            let Some(s) = at else { break None };
            if let Some(c) = parse_color(s, &HashMap::new()) {
                break Some(c);
            }
            if seen.contains(&s) || !table.contains_key(s) {
                break None;
            }
            seen.push(s);
            at = table.get(s).and_then(Value::as_str);
        };
        match color {
            Some(c) => {
                out.insert(k.clone(), c);
            }
            None => warnings.push(format!("theme palette: {k} = {v} is not a color")),
        }
    }
    out
}

fn parse_style(v: &Value, palette: &HashMap<String, Color>, warn: &mut impl FnMut(String)) -> Option<Style> {
    let color = |s: &str, warn: &mut dyn FnMut(String)| {
        let c = parse_color(s, palette);
        if c.is_none() {
            warn(format!("unknown color '{s}'"));
        }
        c
    };
    match v {
        Value::String(s) => Some(Style { fg: color(s, warn), ..Style::default() }),
        Value::Table(t) => {
            let mut st = Style::default();
            for (k, v) in t {
                match (k.as_str(), v) {
                    ("fg", Value::String(s)) => st.fg = color(s, warn),
                    ("bg", Value::String(s)) => st.bg = color(s, warn),
                    ("modifiers", Value::Array(ms)) => {
                        for m in ms {
                            match m.as_str() {
                                Some(m) if set_modifier(&mut st, m, true) => {}
                                _ => warn(format!("unknown modifier {m}")),
                            }
                        }
                    }
                    ("underline", Value::Table(u)) => {
                        st.underline = true;
                        for (uk, uv) in u {
                            match (uk.as_str(), uv.as_str()) {
                                ("color", Some(s)) => st.ul_color = color(s, warn),
                                ("style", Some("curl")) => st.curly = true,
                                ("style", Some("line")) => {}
                                _ => warn(format!(
                                    "underline.{uk} = {uv} (color = \"…\", style = \"line\" | \"curl\")"
                                )),
                            }
                        }
                    }
                    (m, Value::Boolean(on)) if set_modifier(&mut st, m, *on) => {}
                    _ => warn(format!(
                        "{k} = {v} — expected fg, bg, bold, italic, underline, dim, reversed, crossed"
                    )),
                }
            }
            Some(st)
        }
        _ => {
            warn(format!("{v} — expected \"color\" or {{ fg = …, … }}"));
            None
        }
    }
}

/// Toggles one modifier — true if the name is known. (`underlined`·`crossed_out` are old names)
fn set_modifier(st: &mut Style, name: &str, on: bool) -> bool {
    let slot = match name {
        "bold" => &mut st.bold,
        "italic" => &mut st.italic,
        "underline" | "underlined" => &mut st.underline,
        "dim" => &mut st.dim,
        "reversed" => &mut st.reversed,
        "crossed" | "crossed_out" | "strikethrough" => &mut st.crossed,
        _ => return false,
    };
    *slot = on;
    true
}

fn parse_color(s: &str, palette: &HashMap<String, Color>) -> Option<Color> {
    if let Some(c) = palette.get(s) {
        return Some(*c);
    }
    if let Some(hex) = s.strip_prefix('#') {
        let v = u32::from_str_radix(hex, 16).ok()?;
        return match hex.len() {
            6 => Some(Color::Rgb { r: (v >> 16) as u8, g: (v >> 8) as u8, b: v as u8 }),
            3 => Some(Color::Rgb {
                r: ((v >> 8) & 0xf) as u8 * 17,
                g: ((v >> 4) & 0xf) as u8 * 17,
                b: (v & 0xf) as u8 * 17,
            }),
            _ => None,
        };
    }
    Some(match s {
        "black" => Color::Black,
        "red" => Color::DarkRed,
        "green" => Color::DarkGreen,
        "yellow" => Color::DarkYellow,
        "blue" => Color::DarkBlue,
        "magenta" => Color::DarkMagenta,
        "cyan" => Color::DarkCyan,
        "gray" => Color::DarkGrey,
        "light-red" => Color::Red,
        "light-green" => Color::Green,
        "light-yellow" => Color::Yellow,
        "light-blue" => Color::Blue,
        "light-magenta" => Color::Magenta,
        "light-cyan" => Color::Cyan,
        "light-gray" => Color::Grey,
        "white" => Color::White,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_fallback_and_patch() {
        let t: toml::Table = toml::from_str(
            r##"
            "keyword" = "red"
            "keyword.control" = { fg = "#00ff00", modifiers = ["bold"] }
            "ui.selection" = { bg = "sel" }
            [palette]
            sel = "#112233"
            "##,
        )
        .unwrap();
        let th = from_table("t", &t);
        assert_eq!(th.get("keyword.control.import").fg, Some(Color::Rgb { r: 0, g: 255, b: 0 }));
        assert!(th.get("keyword.control.import").bold);
        assert_eq!(th.get("keyword.function").fg, Some(Color::DarkRed));
        assert_eq!(th.get("ui.selection").bg, Some(Color::Rgb { r: 0x11, g: 0x22, b: 0x33 }));
        assert_eq!(th.try_get("nothing"), None);
        let s = Style::fg(Color::Red).patch(Style { bg: Some(Color::Blue), bold: true, ..Style::default() });
        assert_eq!((s.fg, s.bg, s.bold), (Some(Color::Red), Some(Color::Blue), true));
    }

    #[test]
    fn builtin_theme_has_ui_and_syntax() {
        let t = Theme::builtin();
        assert!(t.try_get("ui.selection").is_some());
        assert!(t.try_get("keyword").is_some());
        assert!(t.try_get("comment").is_some());
    }

    /// Four built-ins — transparent ones don't paint the background, keeping it as blend base (`ui.tint`).
    #[test]
    fn builtin_four_and_transparent_variants() {
        let names: Vec<&str> = BUILTIN.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, ["meok", "hanji", "meok-transparent", "hanji-transparent"]);
        let solid = load("meok").unwrap();
        let clear = load("meok-transparent").unwrap();
        let bg = solid.try_get("ui.background").and_then(|s| s.bg);
        assert!(bg.is_some());
        assert_eq!(clear.try_get("ui.background").and_then(|s| s.bg), None, "does not paint the background");
        assert_eq!(clear.try_get("ui.tint").and_then(|s| s.bg), bg, "blend base is the original background");
        assert_eq!(clear.try_get("keyword"), solid.try_get("keyword"), "the rest is the same");
        assert!(load("dracula").is_err(), "Helix themes are not read");
    }

    /// A colour in CIELAB (D65), for distances between colours (CIE76 ΔE).
    fn lab(c: Option<Color>) -> [f64; 3] {
        let Some(Color::Rgb { r, g, b }) = c else {
            panic!("a built-in theme's colour is a true colour: {c:?}")
        };
        let lin = |v: u8| {
            let v = v as f64 / 255.0;
            if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
        };
        let (r, g, b) = (lin(r), lin(g), lin(b));
        let x = (0.4124 * r + 0.3576 * g + 0.1805 * b) / 0.95047;
        let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        let z = (0.0193 * r + 0.1192 * g + 0.9505 * b) / 1.08883;
        let f = |t: f64| if t > 0.008856 { t.cbrt() } else { 7.787 * t + 16.0 / 116.0 };
        [116.0 * f(y) - 16.0, 500.0 * (f(x) - f(y)), 200.0 * (f(y) - f(z))]
    }

    fn delta_e(a: Option<Color>, b: Option<Color>) -> f64 {
        let (a, b) = (lab(a), lab(b));
        ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
    }

    /// The code's comments are never drawn as what tarae adds to the code (`ui.virtual`: inlay hints, ghost text,
    /// end-of-line diagnostics) — in every built-in theme a comment stands apart from the virtual text by a step
    /// one sees at a glance (ΔE ≥ 12; a hair's difference, ΔE 5, read as the same grey).
    #[test]
    fn comments_are_never_drawn_as_virtual_text() {
        for name in ["meok", "hanji"] {
            let t = load(name).unwrap();
            let de = delta_e(t.get("comment").fg, t.get("ui.virtual").fg);
            assert!(de >= 12.0, "{name}: a comment is ΔE {de:.1} from virtual text (floor 12)");
        }
    }

    /// Every built-in theme's text clears its floor (WCAG 2 contrast) on each ground it's read on, read from the
    /// themes themselves: body text and the code's colours 4.5 on the ground and the cards (4.0 under the primary
    /// selection and on a card's selected row — hanji's paper has no room for 4.5 there without going muddy),
    /// comments 4 (2.5 under a selection), punctuation and dim 3, faint 2; a mode's word and the cursor 4.5 on
    /// their own colour.
    #[test]
    fn builtin_themes_clear_their_contrast_floors() {
        use crate::chrome::contrast;
        let rgb = |c: Option<Color>| match c {
            Some(Color::Rgb { r, g, b }) => [r, g, b],
            other => panic!("a built-in theme's colour is a true colour: {other:?}"),
        };
        let mut low = Vec::new();
        for name in ["meok", "hanji"] {
            let t = load(name).unwrap();
            let (fg, bg) = (|k: &str| rgb(t.get(k).fg), |k: &str| rgb(t.get(k).bg));
            let grounds = [
                ("ground", bg("ui.background")),
                ("card", bg("ui.popup")),
                ("selected row", bg("ui.menu.selected")),
                ("selection", bg("ui.selection.primary")),
                ("cursor line", bg("ui.cursorline.primary")),
            ];
            let on = |g: &str| grounds.iter().find(|(n, _)| *n == g).unwrap().1;
            let mut check = |what: &str, fg: [u8; 3], ground: &str, floor: f64| {
                let r = contrast(fg, on(ground));
                if r < floor {
                    low.push(format!("{name}: {what} on the {ground} {r:.2} (floor {floor})"));
                }
            };
            let code = ["variable", "keyword", "function", "type", "constant", "string", "function.macro"];
            for k in code.into_iter().chain([
                "attribute",
                "tag",
                "ui.text",
                "ui.accent",
                "error",
                "warning",
                "info",
            ]) {
                check(k, fg(k), "ground", 4.5);
                check(k, fg(k), "cursor line", 4.5);
                check(k, fg(k), "selection", 4.0);
            }
            for k in ["ui.text", "ui.accent", "error", "warning", "info", "diff.plus", "diff.minus"] {
                check(k, fg(k), "card", 4.5);
                check(k, fg(k), "selected row", 4.0);
            }
            check("comment", fg("comment"), "ground", 4.0);
            check("comment", fg("comment"), "cursor line", 4.0);
            check("comment", fg("comment"), "selection", 2.5);
            check("punctuation", fg("punctuation"), "ground", 3.0);
            for g in ["ground", "card", "selected row"] {
                check("dim", fg("ui.virtual"), g, 3.0);
                check("faint", fg("ui.linenr"), g, 2.0);
            }
            for k in
                ["ui.statusline.normal", "ui.statusline.insert", "ui.statusline.select", "ui.cursor.primary"]
            {
                let r = contrast(fg(k), bg(k));
                if r < 4.5 {
                    low.push(format!("{name}: {k}'s text on its colour {r:.2} (floor 4.5)"));
                }
            }
        }
        assert!(low.is_empty(), "under their floors:\n{}", low.join("\n"));
    }

    /// A warning never reads as code: its colour stands apart (CIE76 ΔE in CIELAB, ≥ 20) from every colour the code
    /// and the accent use. On ink a light signal amber has the room; on paper the code's ochres fill the amber band at
    /// their lightness, so hanji's warning is the amber's paper form — a deep burnt amber, apart by lightness (a grid
    /// search, 2026-10-21: the only warm colour ≥ 20 from all of them at 4.5:1).
    #[test]
    fn warnings_stand_apart_from_the_codes_colours() {
        let code =
            ["variable", "keyword", "function", "type", "constant", "string", "attribute", "function.macro"];
        for (name, floor) in [("meok", 20.0), ("hanji", 20.0)] {
            let t = load(name).unwrap();
            for k in code.into_iter().chain(["ui.accent", "error"]) {
                let de = delta_e(t.get("warning").fg, t.get(k).fg);
                assert!(de >= floor, "{name}: warning is ΔE {de:.1} from {k} (floor {floor})");
            }
        }
    }

    /// meok and hanji have one set of keys, their palettes apart — change one, change the other.
    #[test]
    fn builtin_themes_have_the_same_keys() {
        let keys = |src: &str| {
            let t: toml::Table = toml::from_str(src).unwrap();
            let mut k: Vec<String> = t.keys().filter(|k| *k != "palette").cloned().collect();
            k.sort();
            k
        };
        assert_eq!(keys(SOURCES[1].1), keys(SOURCES[0].1), "hanji's keys aren't meok's");
    }

    #[test]
    fn builtin_themes_parse_without_warnings() {
        for (n, _) in BUILTIN {
            let t = load(n).unwrap();
            assert!(t.warnings.is_empty(), "{n}: {:?}", t.warnings);
        }
    }

    /// A user `meok.toml` inheriting `meok` layers over the built-in (no self-recursion).
    #[test]
    fn user_theme_can_inherit_the_builtin_of_the_same_name() {
        let dir = std::env::temp_dir().join(format!("tarae-theme-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("meok.toml"), "inherits = \"meok\"\n\"keyword\" = \"#010203\"\n").unwrap();
        let table = load_table(std::slice::from_ref(&dir), "meok", 0).unwrap();
        let th = from_table("meok", &table);
        assert_eq!(th.get("keyword").fg, Some(Color::Rgb { r: 1, g: 2, b: 3 }));
        assert!(th.try_get("ui.selection").is_some(), "the rest comes from the built-in");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Palette aliases · flat modifiers · bad names warn.
    #[test]
    fn palette_aliases_flat_modifiers_and_warnings() {
        let t: toml::Table = toml::from_str(
            r##"
            "keyword" = { fg = "accent", bold = true, italic = true }
            "comment" = { fg = "grey", modifiers = ["italic", "blink"] }
            "string" = "corall"
            "markup" = { fg = "coral", colour = "red", underline = { color = "accent", style = "curl" } }
            [palette]
            coral = "#ef8a5a"
            accent = "coral"
            loop_a = "loop_b"
            loop_b = "loop_a"
            "##,
        )
        .unwrap();
        let th = from_table("t", &t);
        let coral = Some(Color::Rgb { r: 0xef, g: 0x8a, b: 0x5a });
        let kw = th.get("keyword");
        assert_eq!((kw.fg, kw.bold, kw.italic), (coral, true, true));
        let mk = th.get("markup");
        assert_eq!((mk.ul_color, mk.curly, mk.underline), (coral, true, true));
        assert!(th.get("comment").italic);
        let w = th.warnings.join("\n");
        for want in [
            "unknown color 'grey'",
            "unknown modifier \"blink\"",
            "unknown color 'corall'",
            "colour",
            "loop_a",
        ] {
            assert!(w.contains(want), "{want} missing:\n{w}");
        }
        assert_eq!(th.warnings.len(), 6, "{w}");
    }
}
