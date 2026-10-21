//! `editor.motion` — how much moves on screen: `full` (default) · `calm` · `none`. What moves on a clock (the
//! waiting wave, a toast's fade) moves only at `full`; at `calm` the wave holds still, its time counted by the
//! second, and a toast comes and goes whole. Wet ink (번짐 — the head of what's being written, a function of
//! where the stream is, never of time) stays at `calm` and goes at `none`; the rows of an edit being written and
//! a decision's stamp stay at all three. macOS's Reduce Motion (read once at start, on a worker) makes `full`
//! behave as `calm`; an explicit `none` stays `none`.

use crate::editor::Editor;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    Full,
    Calm,
    None,
}

pub const VALUES: &[&str] = &["full", "calm", "none"];

impl Editor {
    /// How much moves now: the setting, `full` as `calm` under the system's Reduce Motion.
    pub fn motion(&self) -> Motion {
        match self.config.motion.as_str() {
            "none" => Motion::None,
            "calm" => Motion::Calm,
            _ if self.reduce_motion => Motion::Calm,
            _ => Motion::Full,
        }
    }

    /// Whether what moves on a clock moves (the wave, a toast's fade).
    pub fn moving(&self) -> bool {
        self.motion() == Motion::Full
    }

    /// Whether the head of what's being written is drawn wet (off in tests unless one turns it on — `wet_ink`).
    pub fn wet(&self) -> bool {
        self.wet_ink && self.motion() != Motion::None
    }
}

/// Whether macOS's Reduce Motion is on: `defaults read com.apple.universalaccess reduceMotion` says `1` (the
/// key it sets — measured on macOS 27, 2026-10-21: `0` with it off). A worker's: it runs `defaults`. Never in
/// tests; off macOS, no.
pub fn reduce_motion() -> bool {
    if cfg!(test) || !cfg!(target_os = "macos") {
        return false;
    }
    std::process::Command::new("/usr/bin/defaults")
        .args(["read", "com.apple.universalaccess", "reduceMotion"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "1")
}
