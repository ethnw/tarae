//! tarae's own record lookup against a stand-in nameserver on loopback — UDP, and TCP on the same port — that
//! answers what each test gives it. Nothing leaves this machine.

use super::*;
use crate::sandbox::proxy::dns::{CLASS_IN, NOERROR, Record, SERVFAIL, TYPE_SRV, TYPE_TXT};
use std::sync::atomic::{AtomicUsize, Ordering};

/// How a stand-in answers a question: its rcode, TC, and its records' data (each under the question's name).
#[derive(Clone)]
struct Says {
    rcode: u8,
    tc_over_udp: bool,
    records: Vec<(u16, Vec<u8>)>,
}

/// The response to `q` (a whole question as tarae sends it) that `says` says.
fn respond(q: &[u8], says: &Says, udp: bool) -> Vec<u8> {
    let tc = udp && says.tc_over_udp;
    let records = if tc { &[][..] } else { &says.records[..] };
    let mut m = q[..2].to_vec();
    m.extend([0x81 | if tc { 0x02 } else { 0 }, 0x80 | says.rcode, 0, 1, 0, records.len() as u8, 0, 0, 0, 0]);
    // (the question as sent: up to its OPT — tarae's carries one, 11 bytes — which the response leaves out)
    let end = q.len() - 11;
    m.extend_from_slice(&q[12..end]);
    for (kind, data) in records {
        m.extend([0xc0, 0x0c]);
        m.extend(kind.to_be_bytes());
        m.extend(CLASS_IN.to_be_bytes());
        m.extend(60u32.to_be_bytes());
        m.extend((data.len() as u16).to_be_bytes());
        m.extend_from_slice(data);
    }
    m
}

/// A stand-in nameserver on a loopback port of its own (UDP and TCP): what it says, and how many questions came over
/// each (UDP, TCP). `silent`: it answers nothing over UDP.
fn nameserver(says: Says, silent: bool) -> (SocketAddr, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let at = udp.local_addr().unwrap();
    let tcp = std::net::TcpListener::bind(at).unwrap();
    let (by_udp, by_tcp) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let (n, s) = (by_udp.clone(), says.clone());
    std::thread::spawn(move || {
        let mut b = [0u8; 4096];
        while let Ok((len, from)) = udp.recv_from(&mut b) {
            n.fetch_add(1, Ordering::SeqCst);
            if !silent {
                let _ = udp.send_to(&respond(&b[..len], &s, true), from);
            }
        }
    });
    let n = by_tcp.clone();
    std::thread::spawn(move || {
        for mut c in tcp.incoming().flatten() {
            n.fetch_add(1, Ordering::SeqCst);
            let mut len = [0u8; 2];
            if c.read_exact(&mut len).is_err() {
                continue;
            }
            let mut q = vec![0u8; usize::from(u16::from_be_bytes(len))];
            if c.read_exact(&mut q).is_ok() {
                let _ = c.write_all(&dns::framed(&respond(&q, &says, false)));
            }
        }
    });
    (at, by_udp, by_tcp)
}

fn srv_data() -> Vec<u8> {
    [&[0, 0, 0, 0, 0x69, 0x89][..], &[3], b"db1", &[4], b"test", &[0]].concat()
}

/// A name's records are asked of the nameserver and read back: over UDP; where its answer was cut short (TC), again
/// over TCP — the whole of them.
#[test]
fn records_are_asked_over_udp_then_tcp_when_cut_short() {
    let says = Says { rcode: NOERROR, tc_over_udp: false, records: vec![(TYPE_SRV, srv_data())] };
    let (at, udp, tcp) = nameserver(says, false);
    let want = Record::Srv { priority: 0, weight: 0, port: 27017, target: "db1.test".into() };
    let found = look_up(&[at], "_mongodb._tcp.cluster.test", TYPE_SRV, TRY);
    assert_eq!(found, Found::Records(vec![want], 60));
    assert_eq!((udp.load(Ordering::SeqCst), tcp.load(Ordering::SeqCst)), (1, 0), "UDP alone");
    let long = vec![vec![b'x'; 255]; 4].iter().flat_map(|s| [&[255u8][..], s].concat()).collect();
    let says = Says { rcode: NOERROR, tc_over_udp: true, records: vec![(TYPE_TXT, long)] };
    let (at, udp, tcp) = nameserver(says, false);
    let Found::Records(r, _) = look_up(&[at], "cluster.test", TYPE_TXT, TRY) else { panic!("records") };
    assert_eq!(r, [Record::Txt(vec![vec![b'x'; 255]; 4])], "whole, over TCP");
    assert_eq!((udp.load(Ordering::SeqCst), tcp.load(Ordering::SeqCst)), (1, 1));
}

/// A nameserver that fails, or answers nothing, is passed for the next (`resolv.conf`'s order), each asked again in
/// the next round; none answering is `Failed` — a run's question gets SERVFAIL, and asks again.
#[test]
fn a_failing_nameserver_is_passed_for_the_next() {
    let fails = Says { rcode: SERVFAIL, tc_over_udp: false, records: Vec::new() };
    let (failing, failed, _) = nameserver(fails.clone(), false);
    let (quiet, asked_quiet, _) = nameserver(fails.clone(), true);
    let good = Says { rcode: NOERROR, tc_over_udp: false, records: vec![(TYPE_SRV, srv_data())] };
    let (answering, _, _) = nameserver(good, false);
    let wait = Duration::from_millis(200);
    let found = look_up(&[failing, quiet, answering], "_x._tcp.y.test", TYPE_SRV, wait);
    assert!(matches!(found, Found::Records(..)), "{found:?}");
    assert_eq!((failed.load(Ordering::SeqCst), asked_quiet.load(Ordering::SeqCst)), (1, 1));
    let found = look_up(&[failing, quiet], "_x._tcp.y.test", TYPE_SRV, wait);
    assert_eq!(found, Found::Failed);
    assert_eq!(asked_quiet.load(Ordering::SeqCst), 1 + ROUNDS, "each round");
}

/// `resolv.conf`'s nameservers as glibc takes them: `nameserver` lines in order, at most three, port 53; a scoped
/// IPv6 one left out; none, this machine's.
#[test]
fn nameservers_are_resolv_confs() {
    let conf = "# c\nsearch corp.example\nnameserver 10.0.0.2\nnameserver fe80::1%eth0\nnameserver ::1\n\
                nameserver 10.0.0.3\nnameserver 10.0.0.4\noptions ndots:5\n";
    let got: Vec<String> = servers(conf).iter().map(|a| a.to_string()).collect();
    assert_eq!(got, ["10.0.0.2:53", "[::1]:53", "10.0.0.3:53"]);
    assert_eq!(servers("search x\n"), [SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 53)]);
}
