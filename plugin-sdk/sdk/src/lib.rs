//! Helpers for tarae plugins, beside the bindings wit-bindgen makes from `wit/tarae.wit`.
//!
//! A plugin has no clock and no random source of its own: every call from tarae brings `now` (the time, and
//! 32 random bytes). Hand it over first thing, and the rest works — `getrandom` (so rustls, RustCrypto, …)
//! and the time TLS checks certificates against:
//!
//! ```ignore
//! fn run(name: String, cx: Context) -> Vec<Action> {
//!     tarae_plugin::now::set(cx.now.millis, &cx.now.entropy);
//!     // …
//! }
//! ```
//!
//! - [`now`]: the time, and the randomness it keeps stirred.
//! - [`http`]: HTTP/1.1 requests written out, responses read as their bytes come (chunked too), and a
//!   streamed body split into lines (a Kubernetes watch).
//! - [`tls`] (feature `tls`): a TLS client over bytes — rustls with RustCrypto, no sockets: what it wants
//!   sent goes out with tarae's `send`, what comes back (`data`) goes in.

pub mod http;
pub mod now;
#[cfg(feature = "tls")]
pub mod tls;
