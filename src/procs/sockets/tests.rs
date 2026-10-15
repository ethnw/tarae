use super::*;

/// What a tree is connected to outside itself is what's said: a unix peer by its path, a TCP one by its
/// address — and a connection between two of the tree's own processes (a socketpair, a loopback pipe between a
/// test runner and its child) isn't, or every busy run would look connected elsewhere.
#[test]
fn only_peers_outside_the_tree_are_named() {
    let tcp = |l: &str, p: &str| Sock::Tcp { local: l.parse().unwrap(), peer: p.parse().unwrap() };
    let socks = [
        // (a client to a daemon's socket, twice — said once)
        Sock::Unix { id: 1, peer: 90, path: Some("/var/run/engine.sock".into()) },
        Sock::Unix { id: 2, peer: 91, path: Some("/var/run/engine.sock".into()) },
        // (a socketpair between two of its processes)
        Sock::Unix { id: 3, peer: 4, path: None },
        Sock::Unix { id: 4, peer: 3, path: None },
        // (one end of a connection to another program, unnamed)
        Sock::Unix { id: 5, peer: 92, path: None },
        // (a remote engine; and a loopback connection between two of its own)
        tcp("10.0.0.2:50000", "10.0.0.5:2376"),
        tcp("127.0.0.1:41000", "127.0.0.1:41001"),
        tcp("127.0.0.1:41001", "127.0.0.1:41000"),
    ];
    assert_eq!(outside(&socks), ["/var/run/engine.sock", "another program's socket", "10.0.0.5:2376"]);
    assert!(outside(&socks[2..4]).is_empty(), "its own pair: nothing");
}

/// The reader finds this process's own connections with their peers — a unix socket's by the listener's path,
/// a TCP one by its port — so a run's tree is read for real, the hand-declared `socket_fdinfo` included (a
/// layout off by a field reads nonsense here). The listeners are the test's own: a socket in its temp folder,
/// a port-0 one on 127.0.0.1.
#[test]
fn a_processs_connections_are_read_with_their_peers() {
    let dir = crate::scratch::Scratch::new("ps");
    // (a datagram client with an address of its own, `c.sock`, connected to `d.sock`: the peer's path read
    // as its own address would say `c.sock` — a stream pair's two ends can't tell those apart)
    let path = dir.join("d.sock");
    let _daemon = std::os::unix::net::UnixDatagram::bind(&path).unwrap();
    let client = std::os::unix::net::UnixDatagram::bind(dir.join("c.sock")).unwrap();
    client.connect(&path).unwrap();
    let pair = std::os::unix::net::UnixListener::bind(dir.join("p.sock")).unwrap();
    let _a = std::os::unix::net::UnixStream::connect(dir.join("p.sock")).unwrap();
    let _b = pair.accept().unwrap();
    let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let at = tcp.local_addr().unwrap();
    let out = std::net::TcpStream::connect(at).unwrap();
    let _in = tcp.accept().unwrap();
    let socks = of(std::process::id());
    let mine = Sock::Tcp { local: out.local_addr().unwrap(), peer: at };
    assert!(socks.contains(&mine), "the TCP connection, both ends right: {socks:?}");
    if cfg!(target_os = "macos") {
        let named = |p: &str| {
            socks.iter().filter(|s| matches!(s, Sock::Unix { path: Some(x), .. } if x.ends_with(p))).count()
        };
        assert_eq!((named("d.sock"), named("c.sock")), (1, 0), "the client by its peer's path: {socks:?}");
        // (the daemon's end isn't connected — a peer outside what was read: named)
        assert!(outside(&socks).iter().any(|s| s.ends_with("d.sock")), "{:?}", outside(&socks));
        // (the stream pair: each end's peer is the other, both here — the tree's own, not outside)
        let pairs = socks.iter().filter(|s| {
            matches!(s, Sock::Unix { id, peer, .. }
                if socks.iter().any(|o| matches!(o, Sock::Unix { id: i, peer: p, .. } if i == peer && p == id)))
        });
        assert!(pairs.count() >= 2, "both ends of the pair, each the other's peer: {socks:?}");
        assert!(!outside(&socks).iter().any(|s| s.ends_with("p.sock")), "both ends here: not outside");
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// `/proc/net/tcp`'s addresses: one 32-bit word per four bytes, printed in the kernel's byte order.
#[test]
fn a_proc_net_tcp_row_reads_as_its_addresses() {
    let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   \
                 0: 0100007F:A028 0500000A:0948 01 00000000:00000000 00:00000000 00000000  501        0 4242 1\n   \
                 1: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  501        0 4243 1\n";
    // (the machines tarae builds for are little-endian, as this table was printed on one)
    let want =
        Sock::Tcp { local: "127.0.0.1:41000".parse().unwrap(), peer: "10.0.0.5:2376".parse().unwrap() };
    assert_eq!(tcp_table(table, &[4242, 4243]), [want], "the established one, not the listener");
}
