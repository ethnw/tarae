//! The stamp (도장, 글타래 P5 — `geultarae-plan.md` §2.4): a decision you made is stamped where it was made with
//! the key that made it — `▐y▌ accepted`, `▐n▌ rejected` — so what's left is what has none: the edit review's
//! change heads, the turn review's files, `:granted`'s answers. Stamped in the frame the key draws, where it stays;
//! one your key makes in sight lands lit and dries to its dry ink (motion §5 — `Editor::press`, `motion.rs`).
//! (A stamp, not a seal: the seal is the brand's mark, never drawn in the editor — one word for one thing.)
//!
//! The key, not a word: it's the letter the hints of what's still undecided name (`y accept · n reject`), in the
//! code's own font and bold — the plan's Hangul `가`/`부` (the user, 2026-10-19) fell back to another font, thin
//! and off the grid, in a terminal whose coding font has no Hangul (most), and said nothing to whoever doesn't
//! read Korean. Only a key known to have given it: `:granted`'s answers record theirs (`allow::Allowed.given`).
//!
//! How it's drawn is `term::stamp_spans`, the one way: filled where what it's stamped on has a colour (the letter
//! cut out of the ground), carved the other way — the letter and its frame in ink — where it hasn't (a
//! transparent theme). The accent is rationed (one mark a region at rest): a stamp is in the accent only on the
//! row in hand, where the accent's bar already is; every other stamp is dry — the same stamp in the dim ink.

use crate::llm::Decision;

/// What was decided, as the key that decided it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp(pub char);

impl Stamp {
    /// An edit review's change, once answered: `y`, `n`.
    pub fn of(d: Decision) -> Option<Stamp> {
        match d {
            Decision::Accept => Some(Stamp('y')),
            Decision::Reject => Some(Stamp('n')),
            Decision::Undecided => None,
        }
    }
}
