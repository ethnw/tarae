//! What the client sends, held against what `busctl` sent the user's bus for the same call; what it reads, against
//! what the bus and systemd answered — M11's capture on Ubuntu 26.10 (systemd 261, dbus-broker), kept as hex in
//! `fixtures.txt` (`fixtures_from_the_capture` makes it again). Bytes only: every test here runs on every system.

use super::*;

/// The messages kept from M11's capture (`target/sb/profile6h/m11-ssh.pcap`): their names, and their place in it.
const KEPT: [(&str, usize); 11] = [
    ("hello", 0),
    ("hello-return", 1),
    ("name-owner-changed", 2),
    ("name-acquired", 3),
    ("call", 4),
    ("job-new", 12),
    ("call-return", 13),
    ("scope-changed", 18),
    ("job-removed", 22),
    ("unit-exists", 33),
    ("invalid-args", 43),
];

/// `fixtures.txt`'s message `name`, as the bus carried it.
fn fixture(name: &str) -> Vec<u8> {
    let text = include_str!("fixtures.txt");
    let at = text.find(&format!("=== {name}\n")).unwrap_or_else(|| panic!("no fixture {name}"));
    let hex: String = text[at..]
        .lines()
        .skip(1)
        .take_while(|l| !l.starts_with("==="))
        .filter(|l| !l.starts_with('#'))
        .flat_map(|l| l.chars().filter(char::is_ascii_hexdigit))
        .collect();
    (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect()
}

/// Every fixture, by name.
fn fixtures() -> Vec<(&'static str, Vec<u8>)> {
    KEPT.iter().map(|(name, _)| (*name, fixture(name))).collect()
}

/// The scope's call is the one `busctl` made for the same unit and pid, byte for byte — its body (where `Delegate`'s
/// boolean, `PIDs`' array and the empty aux array's padding live) and its header, field after field in sd-bus's
/// order — but for two things the bus and busctl add: the `sender` field the bus writes in, and busctl's flag
/// `ALLOW_INTERACTIVE_AUTHORIZATION` (4: the user's manager never asks; tarae sends 0). The captured message also
/// reads back to its own bytes, so `encode` is held to busctl's bytes, not only to itself.
#[test]
fn the_scope_request_is_what_busctl_sent() {
    let raw = fixture("call");
    let theirs = decode(&raw).unwrap();
    assert_eq!(encode(&theirs), raw, "read and written back: busctl's bytes");
    // (busctl's stand-in keeper: pid 432829)
    let ours = scope_call(2, "tarae-m11-432793-0.scope", 432829);
    assert_eq!(ours.body, theirs.body, "the body, byte for byte");
    let without_sender = Message {
        flags: 0,
        fields: theirs.fields.iter().filter(|(c, _)| *c != SENDER).cloned().collect(),
        ..theirs.clone()
    };
    assert_eq!(theirs.flags, 4, "busctl's interactive authorization");
    assert_eq!(encode(&ours), encode(&without_sender), "the header, field after field");
    // (and the start of every conversation: Hello, as busctl sent it)
    let hello = decode(&fixture("hello")).unwrap();
    let ours =
        Message::call(1, "org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "Hello");
    let fields: Vec<_> = hello.fields.into_iter().filter(|(c, _)| *c != SENDER).collect();
    assert_eq!(ours.fields, fields);
}

/// The call's answer is told among what else comes, by its reply serial: the return's first value is the job,
/// systemd's refusal is its error's name and words; Hello's return, the bus's signals and systemd's own are no
/// answer to it.
#[test]
fn a_reply_says_the_job_or_the_managers_words() {
    let read = |name: &str| decode(&fixture(name)).unwrap();
    assert_eq!(answer(&read("call-return"), 2), Some(Ok("/org/freedesktop/systemd1/job/80".into())));
    assert_eq!(
        answer(&read("unit-exists"), 2),
        Some(Err(
            "org.freedesktop.systemd1.UnitExists: Unit tarae-m11-432793-0.scope was already loaded or has a \
                  fragment file."
                .into()
        ))
    );
    assert_eq!(
        answer(&read("invalid-args"), 2),
        Some(Err("org.freedesktop.DBus.Error.InvalidArgs: Process 1 is a manager process, refusing.".into()))
    );
    for other in
        ["hello-return", "name-acquired", "name-owner-changed", "job-new", "job-removed", "scope-changed"]
    {
        assert_eq!(answer(&read(other), 2), None, "{other}");
    }
    assert_eq!(answer(&read("hello-return"), 1), Some(Ok(":1.4".into())), "Hello's own");
}

/// A small fixed-seed generator: the loop is the same every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// Whatever the bus sends, the reader answers a message or a refusal — never a panic, never a read past a length —
/// and a message it answers is those bytes exactly (it writes back to them). Each captured message mutated: bytes
/// flipped, cut, extended, a 4-byte field (a length, a count) set past the limits; `length` first, as the client reads.
#[test]
fn the_bus_reply_parser_never_panics() {
    let seeds: Vec<Vec<u8>> = fixtures().into_iter().map(|(_, b)| b).collect();
    for b in &seeds {
        assert_eq!(encode(&decode(b).unwrap()), *b, "each seed reads back to itself");
    }
    let mut rng = Rng(0x7a7a_d0b5_5eed_0001);
    let (mut messages, mut refusals) = (0, 0);
    for _ in 0..200_000 {
        let mut b = seeds[rng.below(seeds.len())].clone();
        for _ in 0..1 + rng.below(3) {
            match rng.below(5) {
                0 if !b.is_empty() => {
                    let at = rng.below(b.len());
                    b[at] ^= 1 << rng.below(8);
                }
                1 => b.truncate(rng.below(b.len() + 1)),
                2 => b.extend((0..1 + rng.below(16)).map(|_| rng.next() as u8)),
                3 if b.len() >= 4 => {
                    let at = rng.below(b.len() - 3);
                    let big = [u32::MAX, MAX as u32, 1 << 31, 7, 0][rng.below(5)];
                    b[at..at + 4].copy_from_slice(&big.to_le_bytes());
                }
                _ => {}
            }
        }
        // (as the client does: the length from the first 16 bytes, then the message)
        if b.len() >= 16 && length(&b[..16]).is_err() {
            refusals += 1;
            continue;
        }
        match decode(&b) {
            Ok(m) => {
                messages += 1;
                if !m.big {
                    assert_eq!(encode(&m), b, "a message is its bytes exactly");
                }
                let _ = (m.first_str(), answer(&m, 2));
            }
            Err(_) => refusals += 1,
        }
    }
    assert!(messages > 1000 && refusals > 10_000, "both kinds of answer: {messages} / {refusals}");
    assert!(decode(&[]).is_err() && length(&[b'l'; 3]).is_err());
}

/// `fixtures.txt` from the capture (`CAPTURE`, else M11's ssh run's): the messages `KEPT` names, each a line of its
/// sender, member and signature, then its bytes as hex — what the other tests read. Not a check: run by hand after a
/// new capture (`cargo test … fixtures_from_the_capture -- --ignored`).
#[test]
#[ignore = "makes fixtures.txt from a capture"]
fn fixtures_from_the_capture() {
    let path = std::env::var("CAPTURE").unwrap_or_else(|_| "target/sb/profile6h/m11-ssh.pcap".into());
    let packets = pcapng_packets(&std::fs::read(&path).unwrap());
    let mut out = String::from(
        "# The user's bus as `busctl --user call … StartTransientUnit` used it and the bus and systemd answered (M11,\n\
         # 2026-10-08: Ubuntu 26.10, systemd 261, dbus-broker; `busctl --user capture`), kept by\n\
         # dbus/tests.rs's fixtures_from_the_capture: each message whole, hex, 64 bytes a line.\n",
    );
    for (name, i) in KEPT {
        let p = &packets[i];
        let m = decode(p).unwrap();
        out += &format!(
            "\n=== {name}\n# {} → {}: {} ({})\n",
            m.str(SENDER).unwrap_or("?"),
            m.str(DESTINATION).unwrap_or("all"),
            m.str(MEMBER).or(m.str(ERROR_NAME)).unwrap_or("reply"),
            m.str(SIGNATURE).unwrap_or("")
        );
        for chunk in p.chunks(64) {
            out += &chunk.iter().map(|b| format!("{b:02x}")).collect::<String>();
            out += "\n";
        }
    }
    std::fs::write("src/sandbox/dbus/fixtures.txt", out).unwrap();
}
