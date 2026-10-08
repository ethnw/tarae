//! The bytes nft 1.1.3 and iproute2 6.15 sent the kernel, and what it answered — Debian 12's 6.1, each case in a
//! user and network namespace of its own — kept to what the tests read (`fixtures/netlink.txt`, made by the scripts in
//! `fixtures/capture/`): per case its command, the last datagram sent (`>`), the kernel's answers (`<`), each as hex,
//! a continuation line indented.

const TEXT: &str = include_str!("../fixtures/netlink.txt");

/// A case: what it sent last, and the datagrams the kernel answered.
struct Case {
    name: String,
    sent: Option<Vec<u8>>,
    received: Vec<Vec<u8>>,
}

fn hex(h: &str) -> Vec<u8> {
    (0..h.len()).step_by(2).map(|i| u8::from_str_radix(&h[i..i + 2], 16).expect("hex")).collect()
}

fn cases() -> Vec<Case> {
    let mut out: Vec<Case> = Vec::new();
    // (the datagram being read: whether it was sent, its bytes)
    let mut open: Option<(bool, Vec<u8>)> = None;
    let close = |c: Option<&mut Case>, open: &mut Option<(bool, Vec<u8>)>| {
        if let (Some(c), Some((sent, bytes))) = (c, open.take()) {
            match sent {
                true => c.sent = Some(bytes),
                false => c.received.push(bytes),
            }
        }
    };
    for line in TEXT.lines() {
        if let Some(rest) = line.strip_prefix("  ") {
            if let Some((_, bytes)) = &mut open {
                bytes.extend(hex(rest));
            }
            continue;
        }
        close(out.last_mut(), &mut open);
        if let Some(name) = line.strip_prefix("=== ") {
            out.push(Case { name: name.to_string(), sent: None, received: Vec::new() });
        } else if let Some(h) = line.strip_prefix("> ") {
            open = Some((true, hex(h)));
        } else if let Some(h) = line.strip_prefix("< ") {
            open = Some((false, hex(h)));
        }
    }
    close(out.last_mut(), &mut open);
    out
}

fn case(name: &str) -> Case {
    cases().into_iter().find(|c| c.name == name).unwrap_or_else(|| panic!("no captured case {name}"))
}

/// What case `name` sent last — nft's batch, or ip's change.
pub fn last_sent(name: &str) -> Vec<u8> {
    case(name).sent.unwrap_or_else(|| panic!("case {name} keeps no send"))
}

/// What the kernel answered in case `name`, datagram by datagram.
pub fn received(name: &str) -> Vec<Vec<u8>> {
    case(name).received
}

/// Every datagram the kernel answered in every case kept.
pub fn every_reply() -> Vec<Vec<u8>> {
    cases().into_iter().flat_map(|c| c.received).collect()
}
