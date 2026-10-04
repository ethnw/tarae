//! Choosing Claude's model and effort — from the list claude itself gives (its answer to `initialize`,
//! measured: value · displayName · description · resolvedModel · supportedEffortLevels).
//!
//! - `C-t` in the chat · `:model` · the palette: the models, the one in use marked; `:effort`: the levels
//!   the model takes. A choice is saved (`llm.model` · `llm.effort` — `space i` uses them too) and applied to
//!   the conversation at once: `set_model` and `apply_flag_settings {effortLevel}` control requests
//!   (measured: the next answer comes from the new model; `low`/`max` changed the thinking 47 → 174 tokens).
//!   Back to the model's own effort: `effortLevel: null` (measured: thinking back at the default's).
//! - The chat's title says which (`Opus 5.5 · high`) while it's idle — the model that answered (`init`, then
//!   each answer's `message.model`); when that isn't what's asked for, both (`claude-sonnet-5-5 · asked opus`).

use serde_json::json;

use crate::editor::Editor;
use crate::llm::ModelInfo;

/// Before claude has said which it offers (the chat not open yet): the aliases every claude knows.
fn fallback() -> Vec<ModelInfo> {
    [
        ("default", "Default", "The CLI's recommended model"),
        ("opus", "Opus", "For complex work"),
        ("sonnet", "Sonnet", "Efficient for routine tasks"),
        ("haiku", "Haiku", "Fastest for quick answers"),
    ]
    .into_iter()
    .map(|(v, n, a)| ModelInfo {
        value: v.into(),
        name: n.into(),
        about: a.into(),
        resolved: String::new(),
        efforts: Vec::new(),
    })
    .collect()
}

fn offered(ed: &Editor) -> Vec<ModelInfo> {
    match ed.session.chat.as_ref().map(|c| &c.models).filter(|m| !m.is_empty()) {
        Some(m) => m.clone(),
        None => fallback(),
    }
}

/// The value chosen (`default` when none is).
fn chosen(ed: &Editor) -> String {
    ed.config.llm.model.clone().unwrap_or_else(|| "default".into())
}

/// The model in use as the title says it: its name (for `default`, the model it stands for), and the effort
/// when the model takes one — `Opus 5.5 · high`. When what answered isn't what's asked for (a chat started
/// before `llm.model` changed), both: `claude-sonnet-5-5 · asked opus`. None before anything's known.
pub fn label(ed: &Editor) -> Option<String> {
    let c = ed.session.chat.as_ref()?;
    let models = &c.models;
    let value = chosen(ed);
    let m = models.iter().find(|m| m.value == value);
    if !c.model.is_empty() && !asks_for(m, &value, &c.model) {
        return Some(format!("{} · asked {value}", c.model));
    }
    let name = match m {
        Some(m) if m.value == "default" => models
            .iter()
            .find(|o| o.value != "default" && o.resolved == m.resolved)
            .map_or(m.name.clone(), |o| o.name.clone()),
        Some(m) => m.name.clone(),
        None if ed.config.llm.model.is_some() => value.clone(),
        None => return Some(c.model.clone()).filter(|m| !m.is_empty()),
    };
    let effort = &ed.config.llm.effort;
    let takes = m.is_some_and(|m| m.efforts.iter().any(|e| e == effort));
    Some(if takes { format!("{name} · {effort}") } else { name })
}

/// Whether the model `answered` is what `value` asks for: the model it resolves to (an alias, `default`) —
/// ids compared without a `[1m]`-style suffix, a dated one counting as the same — else, before claude has
/// said what it offers, the value itself or a family alias in the id (`opus`).
fn asks_for(m: Option<&ModelInfo>, value: &str, answered: &str) -> bool {
    let bare = |s: &str| s.split('[').next().unwrap_or(s).to_string();
    let a = bare(answered);
    match m.map(|m| bare(&m.resolved)).filter(|r| !r.is_empty()) {
        Some(r) => a.starts_with(&r) || r.starts_with(&a),
        None => value == "default" || a.contains(value),
    }
}

/// `C-t` · `:model` — the models, the one in use marked.
pub fn picker(ed: &mut Editor) {
    use crate::picker::{Action, Item};
    let now = chosen(ed);
    let models = offered(ed);
    let at = models.iter().position(|m| m.value == now).unwrap_or(0);
    let items: Vec<Item> = models
        .into_iter()
        .map(|m| Item {
            label: m.name.clone(),
            hint: m.about.clone(),
            glyph: Some(if m.value == now { ("●", "ui.accent") } else { ("◦", "ui.virtual") }),
            action: Action::Typed(format!("model {}", m.value)),
        })
        .collect();
    // A small card over the code, on the one in use (nothing to preview)
    let mut p = crate::picker::Picker::new("model", items, false).without_preview();
    p.compact = true;
    p.selected = at;
    ed.open_picker(p, None);
}

/// `:effort` — the levels the model in use takes, and its own default.
pub fn effort_picker(ed: &mut Editor) {
    use crate::picker::{Action, Item};
    let value = chosen(ed);
    let levels = match offered(ed).into_iter().find(|m| m.value == value) {
        Some(m) if m.efforts.is_empty() && !m.resolved.is_empty() => {
            return ed.set_status(format!("{} has no effort levels", m.name));
        }
        Some(m) if !m.efforts.is_empty() => m.efforts,
        _ => ["low", "medium", "high", "xhigh", "max"].map(String::from).to_vec(),
    };
    let now = ed.config.llm.effort.clone();
    let about = |l: &str| match l {
        "low" => "Quick — thinks little",
        "medium" => "Balanced",
        "high" => "Thorough",
        "xhigh" => "More thorough",
        "max" => "As hard as it can",
        _ => "",
    };
    let mut items: Vec<Item> = vec![Item {
        label: "default".into(),
        hint: "The model's own".into(),
        glyph: Some(if now.is_empty() { ("●", "ui.accent") } else { ("◦", "ui.virtual") }),
        action: Action::Typed("effort default".into()),
    }];
    items.extend(levels.iter().map(|l| Item {
        label: l.clone(),
        hint: about(l).into(),
        glyph: Some(if *l == now { ("●", "ui.accent") } else { ("◦", "ui.virtual") }),
        action: Action::Typed(format!("effort {l}")),
    }));
    let mut p = crate::picker::Picker::new("effort", items, false).without_preview();
    p.compact = true;
    p.selected = if now.is_empty() { 0 } else { levels.iter().position(|l| *l == now).map_or(0, |i| i + 1) };
    ed.open_picker(p, None);
}

/// A setting changed and saved (not from tests — they'd write the user's config).
fn save(ed: &mut Editor, path: &str, v: &str) -> Result<(), String> {
    let v = toml::Value::from(v);
    crate::settings::apply(&mut ed.config, path, &v)?;
    ed.overrides.retain(|(p, _)| p != path);
    if !cfg!(test) {
        crate::config::persist(path, &v)?;
    }
    Ok(())
}

/// `:model <value>` — saved, and the conversation goes on with it.
pub fn set(ed: &mut Editor, value: &str) -> Result<(), String> {
    let known = offered(ed);
    let m = known.iter().find(|m| m.value == value || m.name.eq_ignore_ascii_case(value));
    let value = m.map_or(value.to_string(), |m| m.value.clone());
    save(ed, "llm.model", if value == "default" { "" } else { &value })?;
    crate::chat::request(ed, json!({ "subtype": "set_model", "model": value }));
    // (what answers is known again from the next answer — until then the title says what was asked for)
    if let Some(c) = ed.session.chat.as_mut() {
        c.model.clear();
    }
    // An effort the new model doesn't take goes (it'd be refused)
    let effort = ed.config.llm.effort.clone();
    if m.is_some_and(|m| !m.resolved.is_empty() && !effort.is_empty() && !m.efforts.contains(&effort)) {
        save(ed, "llm.effort", "")?;
    }
    let name = label(ed).or_else(|| m.map(|m| m.name.clone())).unwrap_or(value);
    ed.set_success(format!("Claude: {name}"));
    Ok(())
}

/// `:effort <level>` — saved, and applied to the conversation.
pub fn set_effort(ed: &mut Editor, level: &str) -> Result<(), String> {
    let level = if level == "default" { "" } else { level };
    save(ed, "llm.effort", level)?;
    // (null = the model's own — measured: thinking back at the untouched default's)
    let effort = if level.is_empty() { serde_json::Value::Null } else { json!(level) };
    crate::chat::request(
        ed,
        json!({ "subtype": "apply_flag_settings", "settings": { "effortLevel": effort } }),
    );
    let what = if level.is_empty() { "the model's default".to_string() } else { level.to_string() };
    ed.set_success(format!("Effort: {what}"));
    Ok(())
}
