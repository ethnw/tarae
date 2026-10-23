//! The web reader (web-plan; the user's 2026-10-23: the core's, as it's Claude's tool — always there): a page's
//! bytes → its readable text as markdown, its links and anchors, its title, how it read — `read`, a pure function
//! (`reader.rs`; the converter `convert.rs`, code blocks' languages `lang.rs`, addresses `url.rs`, charsets
//! `charset.rs`). tarae fetches (its network, the runs' gate — the next piece) and makes the document.
//!
//! A page's bytes are anyone's (`media.rs`'s rule): parsed only by memory-safe code (html5ever and encoding_rs — no
//! parser of tarae's own; the tree they're read into `tree.rs`, the part read `pick.rs`), on a worker, never the
//! main thread, within limits — its
//! first `PAGE_MAX` bytes, `DEPTH_MAX` levels of nesting, `TEXT_MAX` of text, each said when hit — and never a
//! panic (release aborts on one): no index guessed, every slice at a place found. `check` holds that as a
//! property, its loop in every `cargo test`, and `fuzz/parsers`' `web` target.
//!
//! `:web <path> [url] [main|article|page]` reads a saved page into a buffer (`open.rs`) — until tarae fetches.

mod charset;
#[cfg(any(test, fuzzing))]
pub mod check;
mod convert;
mod lang;
#[cfg(not(fuzzing))]
pub mod open;
mod pick;
mod reader;
mod tree;
mod url;

pub use reader::{PAGE_MAX, Page, Prefer, Read, read};

#[cfg(test)]
mod tests;
