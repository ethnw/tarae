//! A password, kept apart from everything shown: no `Display`, a `Debug` that says nothing of it, and one way
//! to reach the text — for the wire, nowhere else.

use std::fmt;

#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Secret {
        Secret(s.into())
    }

    /// The text itself — only to send it to the database it's for.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("‹secret›")
    }
}
