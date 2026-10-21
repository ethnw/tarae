//! tarae's readers of what a sandboxed run sends, built apart for cargo-fuzz (`--cfg fuzzing`): their modules
//! included from tarae's tree as they are (`#[path]` — tarae is a binary crate, no library to depend on), laid out
//! as tarae lays them out where one reaches another (`proxy`). Each function here is a target's whole body: the
//! property function the reader's in-tree loop calls too (its module's `check`), on libFuzzer's input. A property
//! broken panics — libFuzzer's crash.

use std::sync::OnceLock;

#[allow(dead_code)]
#[path = "../../../src/sandbox/supervisor/wire.rs"]
mod wire;

#[allow(dead_code)]
mod proxy;

use proxy::{dns, parse};

/// A `struct sockaddr` as a call names it (`wire::parse`).
pub fn sockaddr(b: &[u8]) {
    wire::check::address(b);
}

/// An rtnetlink batch a run sends (`wire::netlink_reads`, against the kernel's reading).
pub fn netlink(b: &[u8]) {
    let _ = wire::check::netlink(b);
}

/// A run's memory holding a `struct msghdr` and what it points to (`wire::read_msg`, against the kernel's
/// reading) — laid out as `Fake::from_input` reads it.
pub fn msghdr(b: &[u8]) {
    if let Some((at, datagram, m)) = wire::check::Fake::from_input(b) {
        wire::check::message(&m, at, datagram);
    }
}

/// A request's head through the proxy (`parse::head`).
pub fn head(b: &[u8]) {
    parse::check::head(b);
}

/// A request to its server, as the session's network hands it over (`parse::origin`).
pub fn origin(b: &[u8]) {
    parse::check::origin(b);
}

/// A tunnel's first bytes: TLS records holding a ClientHello (`parse::server_name`).
pub fn sni(b: &[u8]) {
    parse::check::sni(b);
}

/// A chunked body, given a few bytes at a time (`parse::Body`) — the first byte says how many.
pub fn body(b: &[u8]) {
    parse::check::body_input(b);
}

/// A run's DNS question (`dns::question`).
pub fn dns_question(b: &[u8]) {
    dns::check::question(b);
}

/// A resolver's response to tarae's own question (`dns::response`).
pub fn dns_response(b: &[u8]) {
    static ASKED: OnceLock<Vec<u8>> = OnceLock::new();
    dns::check::response(b, ASKED.get_or_init(dns::check::asked));
}
