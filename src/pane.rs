//! Panes (panes P2b, `target/sb/panes-plan.md` §2.1–2.1d): everything the screen shows is a pane. The docked ones
//! are the leaves of their session's split tree (`Session.split`, `split.rs`: ids and shares only) — the code's
//! panes, the side panel, the review's rail, the bottom slot, the chat — and each shows one surface (`Shows`);
//! term.rs lays the tree out every frame and draws each surface in its leaf's rect. The floating ones — a card,
//! a picker, which-key, a prompt — come and go by their keys over the docked ones (`floating`); while one is up
//! it has the keys, as `Editor::keys_to()` says, and `Session.at` names the docked pane they go back to.
//!
//! The default tree is today's screen, cell for cell (§2.5): the side panel or the rail on the left, the code over
//! the slot in the middle, the chat on the right — each surface's own size (`Share::Auto`, `sizes.rs`'s rules),
//! the code the rest. A session's tree always holds one code pane (§2.3).

use crate::split::{Dir, PaneId, Share, Split};

/// What a pane shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Shows {
    /// A file: its `View` in `Session.views` under the pane's id (document, scroll, jumps).
    Code,
    /// The side panel — files, outline, the list, git, a plugin's tab (`sidebar.rs`).
    Side,
    /// The code review's rail (`codereview.rs`) — in the side panel's place while a review is up.
    Rail,
    /// The bottom slot: one panel at a time, its tabs (`sizes::Slot`).
    Slot,
    /// The session's chat.
    Chat,
}

impl Shows {
    /// Its name in the state (`panes: [{shows: "chat"}]`) and the restart's file.
    pub fn name(self) -> &'static str {
        match self {
            Shows::Code => "code",
            Shows::Side => "side",
            Shows::Rail => "rail",
            Shows::Slot => "slot",
            Shows::Chat => "chat",
        }
    }

    pub fn of_name(s: &str) -> Option<Self> {
        [Shows::Code, Shows::Side, Shows::Rail, Shows::Slot, Shows::Chat].into_iter().find(|k| k.name() == s)
    }
}

/// A surface's pane: its id in the tree and what it shows. (A code pane's is its `View`, the same id.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pane {
    pub id: PaneId,
    pub shows: Shows,
}

/// The surfaces a session has a pane for from its start, in the ids they're given after its first code pane's.
pub const SURFACES: [Shows; 4] = [Shows::Side, Shows::Rail, Shows::Slot, Shows::Chat];

/// The ids a new session takes: its first code pane's and one for each of `SURFACES`.
pub const IDS: u64 = 1 + SURFACES.len() as u64;

/// A new session's panes and tree, its first code pane `code`, the surfaces' the ids after it: today's screen.
///
/// ```text
/// │ side · rail │ code │ chat │
/// │             │ slot │      │
/// ```
pub fn default_layout(code: PaneId) -> (Vec<Pane>, Split) {
    let panes: Vec<Pane> =
        SURFACES.iter().enumerate().map(|(i, &shows)| Pane { id: code + 1 + i as u64, shows }).collect();
    let id = |s: Shows| panes.iter().find(|p| p.shows == s).map(|p| p.id).expect("a surface's pane");
    let column = Split::Node(
        Dir::Horizontal,
        vec![(Share::Even, Split::Leaf(code)), (Share::Auto, Split::Leaf(id(Shows::Slot)))],
    );
    let tree = Split::Node(
        Dir::Vertical,
        vec![
            (Share::Auto, Split::Leaf(id(Shows::Side))),
            (Share::Auto, Split::Leaf(id(Shows::Rail))),
            (Share::Even, column),
            (Share::Auto, Split::Leaf(id(Shows::Chat))),
        ],
    );
    (panes, tree)
}

/// A floating pane — up while its key or its asker keeps it (§2.1c): over the docked ones, in front of them for the
/// keys (`keys_to()`'s chain); `q`/`esc` and its own answers make it go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Floating {
    /// A card — a pane that asks (`offer.rs`), by its id.
    Card(u64),
    Picker,
    Prompt,
    WhichKey,
}

impl Floating {
    /// Its id in the state, as the element ids are (`card#12`, `picker`, `prompt`, `which-key`).
    pub fn id(self) -> String {
        match self {
            Floating::Card(n) => format!("card#{n}"),
            Floating::Picker => "picker".into(),
            Floating::Prompt => "prompt".into(),
            Floating::WhichKey => "which-key".into(),
        }
    }
}

impl crate::editor::Editor {
    /// The floating panes up now, the one in front last (which-key only with no picker — as term.rs draws it).
    pub fn floating(&self) -> Vec<Floating> {
        let mut out = Vec::new();
        if let Some(o) = self.offers.first().filter(|_| self.screen.offer_card.is_some()) {
            out.push(Floating::Card(o.id));
        }
        let viewing = crate::view::has_keys(self).and(self.session.view.as_ref());
        if (!self.pending.is_empty() || viewing.is_some_and(|v| !v.pending().is_empty()))
            && self.picker.is_none()
        {
            out.push(Floating::WhichKey);
        }
        if self.picker.is_some() {
            out.push(Floating::Picker);
        }
        if self.prompt.is_some() {
            out.push(Floating::Prompt);
        }
        out
    }
}

#[cfg(test)]
mod tests;
