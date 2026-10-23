# fuzz/parsers — coverage-guided fuzzing of what a sandboxed run sends

libFuzzer targets (cargo-fuzz) for tarae's readers of bytes a run controls: the supervisor's `struct sockaddr`,
netlink batches and `struct msghdr`, the egress proxy's request heads, ClientHellos and chunked bodies
(`target/sb/v2-fuzz-plan.md` §1, §3 "A + C").

The readers' modules are included from tarae's tree as they are (`src/lib.rs`, `#[path]`), and each target is one
call to the **property function** its in-tree loop calls too — the module's `check` (`wire/check.rs`,
`proxy/parse/check.rs`), built for tests and under cargo-fuzz's `--cfg fuzzing`, never in
tarae's own build. A broken property panics, which is a crash here and a failure there. The loops run in every
`cargo test`; this crate is optional and nothing of tarae's reads it.

It has its own workspace, lockfile and toolchain (`rust-toolchain.toml`: a dated nightly, which cargo-fuzz needs).
tarae and the plugin fuzzer in `../` keep the pinned stable.

| target | reader | property (`check::`) | input |
|---|---|---|---|
| `sockaddr` | `wire::parse` | `address`: read as the kernel reads it | the address's bytes |
| `netlink` | `wire::netlink_reads` | `netlink`: taken ⇒ the kernel's model finds no change | a batch |
| `msghdr` | `wire::read_msg` | `message`: the kernel's model's memory, errno, data, descriptors | a run's memory (below) |
| `head` | `parse::head` | `head` | what a client sends the proxy |
| `sni` | `parse::server_name` | `sni` | a tunnel's first bytes |
| `body` | `parse::Body` (chunked) | `body` | first byte: bytes given at a time (1 + 2n), then the body |
| `web` | `web::read` (a page's bytes) | `web::check::page_input`: no panic, the limits, links and anchors in the text | first byte: which part read, the type given; then the page |
| `response` | `parse::response`, `Body::data` (the fetch's) | `response_input`: a head ends at its blank line, a body only where its status has one; the data as its framing says (`dechunked` reads a chunked one apart) | first byte: bytes given at a time (1 + 2n), then the response |
| `gzip` | `web::gzip::gunzip` (the fetch's) | `web::check::gzip_input`: no panic, never past its limit and one, a whole one ends with its CRC | first byte: the limit (64n), then the body |

`msghdr`'s input is a run's memory (`Fake::from_input`): a byte of flags (1: a datagram), where the header is
(8 bytes), then pieces, each an address (8 bytes), a length (2 bytes) and its bytes. Numbers are little-endian.

Not here: `linux.rs`'s audit reader (`audit_value` can't be included alone), the keeper's channel (Linux-only, it
needs the tier types), and `said_in_output`/`said_in_result` (they need a `Policy`). Their loops
are in-tree.

## Running

Once, with the network: the nightly in `rust-toolchain.toml`, cargo-fuzz, and a C++ compiler for libFuzzer.

```sh
rustup toolchain install nightly-2026-10-14 --profile minimal
cargo install cargo-fuzz
```

The first build makes `Cargo.lock` (check it in). Run from the repository's root, with this folder named:

```sh
t=sni  # any target above; `cargo +nightly-2026-10-14 fuzz list --fuzz-dir fuzz/parsers` lists them
mkdir -p fuzz/parsers/corpus/$t
cargo +nightly-2026-10-14 fuzz run --fuzz-dir fuzz/parsers $t fuzz/parsers/corpus/$t fuzz/parsers/seeds/$t \
  -- -max_total_time=600
```

The first folder is the corpus libFuzzer grows (git-ignored, kept between runs). The second holds the seeds,
which are read and never written. Builds are optimized with debug assertions and overflow checks on, as in
tarae's test profile, so an overflow the release build would wrap is a crash here too. `-max_len` matters
for targets whose limits are past libFuzzer's default of 4096 bytes: `head` 12288 (past `HEAD_MAX`),
`sni` 40000 (past `HELLO_MAX` in records), `msghdr` 20000 (a full iovec array), `web` 65536 (a page's pieces: the
parser is fed 8 KB at a time).

`web` reads what anyone's page holds, not what a sandboxed run sends; it's here as the same kind of reader — its
module (`src/web/`, and `media.rs` for sniffing) included as the others are, its crates (html5ever and
encoding_rs) in this folder's `Cargo.toml` at tarae's versions. `response` and `gzip` read what a server sends the
fetch — `gzip` with the inflate tarae has (miniz_oxide) and png's CRC (crc32fast), here too; `response` with
`-max_len` 73728 (past `RESPONSE_HEAD_MAX`, 64 KB: a response's head gets its own, larger limit).

`hour.sh` does all of it on Linux: it builds, then runs each target for an hour (`./hour.sh [seconds]`, `JOBS`
at a time, one per core by default). It leaves `logs/<target>.log` and prints each target's crashes and last
coverage line (`cov:`, `ft:`). The coverage line, compared over the hour, says whether a target plateaued.

## Checking without the nightly

`sh fuzz/parsers/check.sh` builds the targets' library — every reader's module under `--cfg fuzzing` — on tarae's
pinned stable, offline, with libfuzzer-sys and the targets left out (a copy in a temp folder). It's the check that a
change to a reader still compiles as the fuzzer includes it; running the targets needs the nightly above.

## A crash

libFuzzer stops at the first one and leaves its input in `artifacts/<target>/crash-…` (also `oom-…`,
`timeout-…`). To run it again and to minimize it:

```sh
cargo +nightly-2026-10-14 fuzz run  --fuzz-dir fuzz/parsers $t fuzz/parsers/artifacts/$t/crash-…
cargo +nightly-2026-10-14 fuzz tmin --fuzz-dir fuzz/parsers $t fuzz/parsers/artifacts/$t/crash-…
```

The minimized input becomes a hex literal in a plain `#[test]` beside the reader, named for what it shows,
and it's written up in the plan's report form (`v2-fuzz-plan.md` §4).

## Seeds

`seeds/<target>/` is small and checked in: the inputs the in-tree loops start from (captured client heads and
ClientHellos, glibc's question, laid-out messages and batches). The loops' own builders write them:

```sh
cargo test writes_fuzz_seeds -- --ignored
```

Run that again when a builder changes.
