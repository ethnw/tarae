//! Panes (panes P2b, `target/sb/panes-plan.md` §2.1–2.1d): everything the screen shows is a pane. The docked ones
//! are the leaves of their session's split tree (`Session.split`, `split.rs`: ids and shares only) — the code's
//! panes, the side panel, the review's rail, the bottom slot, the chat — and each shows one surface (`Shows`);
//! term.rs lays the tree out every frame and draws each surface in its leaf's rect. The floating ones — a card,
//! a picker, which-key, a prompt — come and go by their keys over the docked ones (`floating`); while one has the
//! keys, as `Editor::keys_to()` says, `Session.at` is `At::Float` with its pane id and `Session.under` the docked
//! pane they go back to (`float_sync`).
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
    /// The tests' linked group (§2.1b, `Group`): the list of results — its selection drives the detail.
    TestList,
    /// …and the detail: the failure picked, else the run's output (its view mode, `view.rs`).
    TestDetail,
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
            Shows::TestList => "tests:list",
            Shows::TestDetail => "tests:detail",
        }
    }

    /// A surface of the tree's (`SURFACES`, the code) by its name — a group's members aren't kept by name: the
    /// group comes with its host.
    pub fn of_name(s: &str) -> Option<Self> {
        [Shows::Code, Shows::Side, Shows::Rail, Shows::Slot, Shows::Chat].into_iter().find(|k| k.name() == s)
    }
}

/// A linked group (§2.1b): a list and its detail, two panes laid out side by side in their host's rect — the slot's,
/// for the tests — under one header and one `q`, a line between them; the list's selection drives the detail. Each
/// is a pane of its own: hopped to (`C-w h/l`, `tab`), its own keys and selection. They aren't the tree's leaves
/// (the slot shows one panel at a time); term.rs lays them out in the slot's leaf while it shows the tests with
/// results to list (`Screen.leaves` has them there, the slot's leaf not).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Group {
    pub list: PaneId,
    pub detail: PaneId,
}

impl Group {
    /// The member pane `id` is, if it's one.
    pub fn member(&self, id: PaneId) -> Option<Shows> {
        match id {
            _ if id == self.list => Some(Shows::TestList),
            _ if id == self.detail => Some(Shows::TestDetail),
            _ => None,
        }
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

/// The ids a new session takes: its first code pane's, one for each of `SURFACES`, then its tests group's two.
pub const IDS: u64 = 1 + SURFACES.len() as u64 + 2;

/// The tests group of a session whose first code pane is `code` — the two ids after its surfaces'.
pub fn tests_group(code: PaneId) -> Group {
    let list = code + 1 + SURFACES.len() as u64;
    Group { list, detail: list + 1 }
}

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
    /// What it shows, in the state (`card`, `picker`, `prompt`, `which-key`).
    pub fn kind(self) -> &'static str {
        match self {
            Floating::Card(_) => "card",
            Floating::Picker => "picker",
            Floating::Prompt => "prompt",
            Floating::WhichKey => "which-key",
        }
    }

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
        let output = crate::rundoc::keyed(self).is_some_and(|(_, o)| !o.keys.pending.is_empty());
        if (!self.pending.is_empty() || output || viewing.is_some_and(|v| !v.pending().is_empty()))
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

    /// The floating pane the keys go to now, as `keys_to()` routes them: a card's (held, or taking its answers),
    /// the picker, a prompt — else which-key while a key sequence is under way.
    pub fn float_front(&self) -> Option<Floating> {
        use crate::uistate::Region;
        match self.keys_region() {
            Region::Card(n) => Some(Floating::Card(n)),
            Region::Picker => Some(Floating::Picker),
            Region::Prompt => Some(Floating::Prompt),
            _ => self.floating().into_iter().find(|f| *f == Floating::WhichKey),
        }
    }

    /// Every key and event (`focus_sync`): `Session.at` is the floating pane with the keys while one has them —
    /// risen over the docked pane (`Session.under`), its id the one it rose with — and the docked pane's again once
    /// it goes (answered, closed, `esc`). A docked pane given the keys while a floating one is up goes under it.
    pub fn float_sync(&mut self) {
        use crate::session::At;
        let front = self.float_front();
        let mut up = self.floating();
        up.extend(front);
        // (a card keeps its pane while it's asked, drawn or not — a bar, stepped aside, behind another)
        let asked = |f: &Floating| matches!(f, Floating::Card(n) if self.offers.iter().any(|o| o.id == *n));
        let kept: Vec<_> =
            self.session.floats.iter().filter(|(f, _)| up.contains(f) || asked(f)).copied().collect();
        self.session.floats = kept;
        let Some(f) = front else {
            if let At::Float(_) = self.session.at {
                self.session.at = self.session.under;
            }
            return;
        };
        let id = match self.session.floats.iter().find(|x| x.0 == f) {
            Some(x) => x.1,
            None => {
                let id = self.next_pane;
                self.next_pane += 1;
                self.session.floats.push((f, id));
                id
            }
        };
        let s = &mut self.session;
        if !matches!(s.at, At::Float(_)) {
            s.under = s.at;
        }
        s.at = At::Float(id);
    }

    /// The floating pane `id` (`At::Float`'s), while it's up.
    #[cfg(test)]
    pub fn float_of(&self, id: crate::split::PaneId) -> Option<Floating> {
        self.session.floats.iter().find(|x| x.1 == id).map(|x| x.0)
    }
}

#[cfg(test)]
mod tests;
