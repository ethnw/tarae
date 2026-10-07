//! The session against MySQL 8.4: transcripts captured once from the test server (`captured/`, written by the
//! `#[ignore]`d `capture_transcripts`), replayed here with no database; what the test server can't show (a server
//! without TLS, a 16 MB row, a request for our files, a plugin we don't speak) from packets made by hand. Live tests
//! (`#[ignore]`) need `target/sb/db/testdb`'s compose file up: `round11.sh`.

use super::*;
use crate::secret::Secret;

/// The test server (target/sb/db/testdb/compose.yaml — fake accounts).
const LIVE: (&str, u16) = ("127.0.0.1", 53306);
const SEED: [u8; 32] = [7; 32];
const APP: (&str, &str) = ("app", "tarae-fake-my");

fn params((user, password): (&str, &str)) -> Params {
    Params { user: user.into(), database: Some("shop".into()), password: Some(Secret::new(password)), tls: None, loopback: true }
}

enum Ask {
    Query(&'static str),
    Describe(&'static str),
}

/// What each scenario signs in as and asks — the same for capturing and replaying.
fn scenario(name: &str) -> ((&'static str, &'static str), Vec<Ask>) {
    use Ask::*;
    let insert = "insert into shop.users values (3, 'c@example.com', null)";
    match name {
        // (fast: the account's hash cached by an earlier sign-in; full: after FLUSH PRIVILEGES emptied the cache)
        "signin-fast" | "signin-full-rsa" => (APP, vec![]),
        "signin-native" => (("native", "tarae-fake-native"), vec![]),
        "query" => (APP, vec![Query("select id, email, note from shop.users order by id"), Query("select repeat('x', 70000) as long_text")]),
        "prepare" => (APP, vec![Describe("select id, email from shop.users where id = ?")]),
        "prepare-error" => (APP, vec![Describe("select emial from shop.users")]),
        "read-only" => (APP, vec![Query(insert), Query("create table shop.t (i int)"), Query("create temporary table tt (i int)")]),
        "lift" => (APP, vec![Query("set session transaction read write"), Query(insert)]),
        "one-statement" => (APP, vec![Query("select 1; select 2")]),
        _ => panic!("no scenario {name}"),
    }
}

const SCENARIOS: [&str; 9] =
    ["signin-fast", "signin-full-rsa", "signin-native", "query", "prepare", "prepare-error", "read-only", "lift", "one-statement"];

fn ask(s: &mut Session, a: &Ask) -> Vec<Event> {
    match a {
        Ask::Query(sql) => s.query(sql),
        Ask::Describe(sql) => s.describe(sql),
    }
}

fn sent(events: &[Event]) -> Vec<u8> {
    events.iter().filter_map(|e| if let Event::Send(b) = e { Some(b.clone()) } else { None }).flatten().collect()
}

fn others(events: Vec<Event>) -> Vec<Event> {
    events.into_iter().filter(|e| !matches!(e, Event::Send(_))).collect()
}

fn captured(name: &str) -> (Vec<u8>, Vec<u8>) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/driver/mysql/captured");
    let read = |side: &str| std::fs::read(dir.join(format!("{name}.{side}.raw"))).unwrap_or_else(|e| panic!("{name}.{side}: {e}"));
    (read("client"), read("server"))
}

/// A scenario replayed: the server's bytes fed in pieces of `piece` bytes — what our side sent, and the events.
fn replay(name: &str, piece: usize) -> (Vec<u8>, Vec<Event>) {
    let (account, asks) = scenario(name);
    let (_, server) = captured(name);
    let (mut s, mut events) = Session::start(params(account), SEED);
    for a in &asks {
        events.extend(ask(&mut s, a));
    }
    for chunk in server.chunks(piece) {
        events.extend(s.feed(chunk));
    }
    (sent(&events), others(events))
}

/// Packets as they go on the wire: (sequence, payload).
fn packets(bytes: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let (mut out, mut rest) = (Vec::new(), bytes);
    while rest.len() >= 4 {
        let len = rest[0] as usize | (rest[1] as usize) << 8 | (rest[2] as usize) << 16;
        out.push((rest[3], rest[4..4 + len].to_vec()));
        rest = &rest[4 + len..];
    }
    out
}

/// The client's commands (after signing in): their texts, `COM_QUERY`'s as `Q:…`.
fn commands(client: &[u8]) -> Vec<String> {
    packets(client)
        .iter()
        .filter(|(seq, _)| *seq == 0)
        .map(|(_, p)| match p[0] {
            COM_QUERY => format!("Q:{}", String::from_utf8_lossy(&p[1..])),
            c => format!("{c:#04x}"),
        })
        .collect()
}

/// A packet as the server frames one.
fn pkt(seq: u8, payload: &[u8]) -> Vec<u8> {
    let mut p = (payload.len() as u32).to_le_bytes()[..3].to_vec();
    p.push(seq);
    p.extend(payload);
    p
}

/// A session signed in from the fast transcript, ready for statements.
fn signed_in() -> Session {
    let (mut s, _) = Session::start(params(APP), SEED);
    let out = s.feed(&captured("signin-fast").1);
    assert!(matches!(out.last(), Some(Event::Ready { .. })), "{out:?}");
    s
}

const OK: &[u8] = &[0, 0, 0, 2, 0, 0, 0];

// ── Replayed from the server ──

/// caching_sha2's fast authentication is ours: what we send is byte for byte what MySQL 8.4 accepted (else no
/// `Ready`) — the scramble's hash order included (sqlx had it backwards once, #4244).
#[test]
fn caching_sha2_fast_auth_is_byte_for_byte_what_the_server_accepted() {
    let (client, _) = captured("signin-fast");
    let (sent, events) = replay("signin-fast", usize::MAX);
    assert_eq!(sent, client);
    assert!(matches!(&events[..], [Event::Ready { version }] if version.starts_with("8.4")), "{events:?}");
}

/// The full authentication never puts the password on the wire in the clear: to this machine without TLS it asks
/// for the server's key (`0x02`) and sends the password RSA-encrypted under it — replayed byte for byte, so this is
/// what the server took. (Over TLS it goes inside the encryption: `live_tls_verifies_the_servers_certificate`.)
#[test]
fn caching_sha2_full_auth_needs_tls_or_rsa() {
    let (client, _) = captured("signin-full-rsa");
    let (sent, events) = replay("signin-full-rsa", usize::MAX);
    assert_eq!(sent, client);
    assert!(matches!(&events[..], [Event::Ready { .. }]), "{events:?}");
    let ps = packets(&client);
    assert!(ps.iter().any(|(_, p)| p == &[0x02]), "the key asked for");
    assert!(!client.windows(APP.1.len()).any(|w| w == APP.1.as_bytes()), "never the password itself");
}

/// Off this machine without TLS the key isn't asked for — whoever is in the way would hand over theirs — and
/// nothing of the password is sent: the full authentication is refused, said why.
#[test]
fn the_servers_key_is_never_asked_off_this_machine() {
    let greeting = packets(&captured("signin-fast").1).remove(0).1;
    let (mut s, _) = Session::start(Params { loopback: false, ..params(APP) }, SEED);
    s.feed(&pkt(0, &greeting));
    let out = s.feed(&pkt(2, &[0x01, 0x04]));
    assert!(matches!(&out[..], [Event::Failed(w)] if w.contains("needs TLS")), "{out:?}");
}

/// mysql_native_password, which 8.4 has off unless asked: SHA1(pw) XOR SHA1(nonce + SHA1(SHA1(pw))) — what the
/// server accepted from the `native` account (it switches us to the plugin).
#[test]
fn mysql_native_password_is_what_the_server_accepted() {
    let (client, _) = captured("signin-native");
    let (sent, events) = replay("signin-native", usize::MAX);
    assert_eq!(sent, client);
    assert!(matches!(&events[..], [Event::Ready { .. }]), "{events:?}");
}

/// A check must never run the statement: prepared (0x16), its definitions read, closed (0x19) — never executed
/// (0x17); the columns name the table they come from, as the server says it.
#[test]
fn describe_runs_nothing() {
    let (client, _) = captured("prepare");
    let (sent, events) = replay("prepare", usize::MAX);
    assert_eq!(sent, client);
    let cmds = commands(&client);
    assert!(cmds.contains(&"0x16".to_string()) && cmds.contains(&"0x19".to_string()), "{cmds:?}");
    assert!(!cmds.contains(&"0x17".to_string()), "never executed: {cmds:?}");
    let Event::Described { params, columns } = &events[1] else { panic!("{events:?}") };
    assert_eq!(params.len(), 1);
    let users = Table::Named { schema: "shop".into(), name: "users".into() };
    assert_eq!(columns.iter().map(|c| (c.name.as_str(), &c.table)).collect::<Vec<_>>(), [("id", &users), ("email", &users)]);
}

/// What the server refuses comes with its number and SQLSTATE — and the session goes on.
#[test]
fn a_statement_the_server_refuses_says_why() {
    let (sent, events) = replay("prepare-error", usize::MAX);
    assert_eq!(sent, captured("prepare-error").0);
    let Event::Refused(e) = &events[1] else { panic!("{events:?}") };
    assert_eq!((e.code.as_str(), e.sqlstate.as_deref()), ("1054", Some("42S22")), "{e:?}");
}

/// Rows come back as text, NULL as nothing (not ""), with their columns' tables; a row past one TCP read whole.
#[test]
fn rows_come_back_as_text_with_their_columns() {
    let (_, events) = replay("query", usize::MAX);
    let Event::Rows { columns, rows, tag } = &events[1] else { panic!("{events:?}") };
    assert_eq!(columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["id", "email", "note"]);
    assert_eq!(rows[0], [Some("1".into()), Some("a@example.com".into()), None]);
    assert_eq!(rows[1][2].as_deref(), Some("second — é"));
    assert_eq!(tag, "2 rows");
    let Event::Rows { rows, columns, .. } = &events[2] else { panic!("{events:?}") };
    assert_eq!((rows[0][0].as_ref().map(String::len), &columns[0].table), (Some(70_000), &Table::None));
}

/// Read-only is set for the session and asked again for every statement, so a write is the server's refusal
/// (25006, as PostgreSQL's) — DDL too (VERIFIED `my-set-transaction` l.399), a temporary table's included (MEASURED,
/// 8.4: only DML on one made before is let through). A guard, not a boundary (§4.2).
#[test]
fn every_statement_is_in_a_read_only_transaction_by_default() {
    let (client, _) = captured("read-only");
    let (sent, events) = replay("read-only", usize::MAX);
    assert_eq!(sent, client, "what we send now is what the server refused then");
    let cmds = commands(&client);
    assert_eq!(cmds[0], "Q:SET SESSION TRANSACTION READ ONLY");
    assert_eq!(cmds[1..4], ["Q:START TRANSACTION READ ONLY", "Q:insert into shop.users values (3, 'c@example.com', null)", "Q:ROLLBACK"]);
    for (i, what) in [(1, "insert"), (2, "create table"), (3, "create temporary table")] {
        assert!(matches!(&events[i], Event::Refused(e) if e.sqlstate.as_deref() == Some("25006")), "{what}: {events:?}");
    }
}

/// A statement can lift the session's read-only (`SET SESSION TRANSACTION READ WRITE`) — not the transaction each
/// statement is started in: the next write is still refused.
#[test]
fn a_statement_cant_lift_read_only_for_the_next() {
    let (client, _) = captured("lift");
    let (sent, events) = replay("lift", usize::MAX);
    assert_eq!(sent, client, "what we send now is what the server refused then");
    let cmds = commands(&client);
    let insert = "Q:insert into shop.users values (3, 'c@example.com', null)";
    let at = cmds.iter().position(|c| c == insert).unwrap();
    assert_eq!(cmds[at - 1], "Q:START TRANSACTION READ ONLY", "the next statement's own transaction: {cmds:?}");
    assert!(matches!(&events[2], Event::Refused(e) if e.sqlstate.as_deref() == Some("25006")), "{events:?}");
}

/// One statement per query: the plugin never asks for several (nor for files of ours, nor several results), so
/// `a; b` is the server's syntax error, not two statements run.
#[test]
fn one_statement_per_query() {
    let (sent, events) = replay("one-statement", usize::MAX);
    assert_eq!(sent, captured("one-statement").0);
    let response = &packets(&sent)[0].1;
    let flags = u32::from_le_bytes(response[..4].try_into().unwrap());
    for (never, name) in [(128, "LOCAL_FILES"), (1 << 16, "MULTI_STATEMENTS"), (1 << 17, "MULTI_RESULTS")] {
        assert_eq!(flags & never, 0, "{name} asked for");
    }
    assert!(matches!(&events[1], Event::Refused(e) if e.code == "1064"), "{events:?}");
}

/// A connection hands over bytes as they come — one at a time is the hardest case, and changes nothing.
#[test]
fn bytes_may_come_in_any_pieces() {
    for name in SCENARIOS {
        assert_eq!(replay(name, 1), replay(name, usize::MAX), "{name}");
    }
}

/// A cancel is `KILL QUERY` with this session's connection id — the greeting's — sent on a second session, alone
/// (not in a transaction of ours).
#[test]
fn a_cancel_is_kill_query_of_this_connections_id() {
    let greeting = packets(&captured("signin-fast").1).remove(0).1;
    let at = greeting.iter().position(|&b| b == 0).unwrap() + 1;
    let id = u32::from_le_bytes(greeting[at..at + 4].try_into().unwrap());
    let s = signed_in();
    assert_eq!(s.connection_id(), id);
    let mut other = signed_in();
    let out = other.kill_query(id);
    assert_eq!(packets(&sent(&out)), [(0, [&[COM_QUERY][..], format!("KILL QUERY {id}").as_bytes()].concat())]);
}

// ── Made by hand: what the test server doesn't do ──

/// A row of 16 MB or more comes in packets of 16 MB − 1 and the rest: joined, not cut.
#[test]
fn a_packet_of_16_mb_continues_in_the_next() {
    let mut s = signed_in();
    s.query("select long_text");
    // (its length takes 8 bytes: the row's first byte is 0xFE — which, this long, isn't the end of the rows)
    let cell = vec![b'x'; CONTINUED + 10];
    let mut row = vec![0xfe];
    row.extend((cell.len() as u64).to_le_bytes());
    row.extend(&cell);
    let def = [&b"\x03def\x00\x00\x00\x09long_text\x00\x0c"[..], &[45, 0, 0, 0, 0, 1, 252, 0, 0, 0, 0, 0]].concat();
    let mut answer = pkt(1, OK);
    answer.extend(pkt(1, &[1]));
    answer.extend(pkt(2, &def));
    answer.extend(pkt(3, &row[..CONTINUED]));
    answer.extend(pkt(4, &row[CONTINUED..]));
    answer.extend(pkt(5, &[0xfe, 0, 0, 2, 0, 0, 0]));
    answer.extend(pkt(1, OK));
    let events = others(s.feed(&answer));
    let Some(Event::Rows { rows, .. }) = events.first() else { panic!("{events:?}") };
    assert_eq!(rows[0][0].as_ref().map(String::len), Some(cell.len()));
}

/// A server that asks for a file of ours (LOAD DATA LOCAL's request) gets nothing — the plugin never offered to
/// send files — and the session ends.
#[test]
fn a_servers_request_for_a_local_file_is_refused() {
    let mut s = signed_in();
    s.query("select 1");
    // (the guard's OK: the statement goes; its answer is the request)
    let out = s.feed(&[pkt(1, OK), pkt(1, b"\xfb/etc/passwd")].concat());
    assert_eq!(packets(&sent(&out)), [(0, b"\x03select 1".to_vec())], "the statement, then nothing — no file");
    assert!(matches!(out.last(), Some(Event::Failed(w)) if w.contains("LOAD DATA LOCAL")), "{out:?}");
    assert!(s.query("select 2").is_empty(), "over: nothing more is sent");
}

/// A plugin the session doesn't speak is refused by its name — `mysql_clear_password` would have the password
/// sent as it is — and nothing more goes out.
#[test]
fn an_unknown_auth_plugin_is_refused_by_name() {
    let greeting = packets(&captured("signin-fast").1).remove(0).1;
    let (mut s, _) = Session::start(params(APP), SEED);
    s.feed(&pkt(0, &greeting));
    let out = s.feed(&pkt(2, b"\xfemysql_clear_password\0"));
    assert!(matches!(&out[..], [Event::Failed(w)] if w.contains("mysql_clear_password")), "{out:?}");
}

fn tls_params() -> Params {
    Params { tls: Some((tarae_plugin::tls::config(b"", None).unwrap(), "db.internal".into())), loopback: false, ..params(APP) }
}

/// Asked for TLS, a server whose greeting doesn't offer it (or had the bit taken out on the way) is never carried
/// on with in the clear: nothing is sent.
#[test]
fn tls_asked_is_never_dropped_for_a_server_without_it() {
    let mut greeting = packets(&captured("signin-fast").1).remove(0).1;
    let at = greeting.iter().position(|&b| b == 0).unwrap() + 1 + 4 + 8 + 1;
    greeting[at + 1] &= !((SSL >> 8) as u8);
    let (mut s, _) = Session::start(tls_params(), SEED);
    let out = s.feed(&pkt(0, &greeting));
    assert!(matches!(&out[..], [Event::Failed(w)] if w.contains("doesn't offer TLS")), "{out:?}");
}

/// Before TLS, what comes isn't from a checked server: an error there is never shown (anyone in the way may have
/// written it), and words after the greeting aren't taken as if they came inside the encryption.
#[test]
fn an_answer_before_tls_is_never_shown() {
    let (mut s, _) = Session::start(tls_params(), SEED);
    let out = s.feed(&pkt(0, b"\xff\x15\x04#28000tarae-fake-evil: run this"));
    assert!(matches!(&out[..], [Event::Failed(w)] if !w.contains("tarae-fake-evil")), "{out:?}");
    let greeting = packets(&captured("signin-fast").1).remove(0).1;
    let (mut s, _) = Session::start(tls_params(), SEED);
    let out = s.feed(&[pkt(0, &greeting), pkt(1, b"\x00injected")].concat());
    assert!(matches!(&out[..], [Event::Failed(w)] if w.contains("more before TLS")), "{out:?}");
}

/// `INTO OUTFILE` never reaches the server: refused at once, nothing sent (`sql::tests::into_outfile_is_refused`
/// for how it's found).
#[test]
fn into_outfile_never_reaches_the_server() {
    let mut s = signed_in();
    let out = s.query("select * from shop.users into outfile '/var/lib/mysql-files/x'");
    assert!(matches!(&out[..], [Event::Refused(e)] if e.message.contains("OUTFILE")), "{out:?}");
    // nor a change of how it reads strings — the way around both readings of the statement
    let out = s.query("SET SESSION sql_mode = 'NO_BACKSLASH_ESCAPES'");
    assert!(matches!(&out[..], [Event::Refused(e)] if e.message.contains("sql_mode")), "{out:?}");
}

// ── Live, against the test server (#[ignore]: round11.sh) ──

/// Drives a session over TCP: (all our bytes, all the server's, the events) once `done` says so or 10 s pass.
fn live(mut s: Session, mut events: Vec<Event>, done: impl Fn(&Session, &[Event]) -> bool) -> (Session, Vec<u8>, Vec<u8>, Vec<Event>) {
    use std::io::{Read, Write};
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
    tarae_plugin::now::set(ms, &[0x5a; 32]);
    let mut tcp = std::net::TcpStream::connect(LIVE).expect("the test server (round9.sh)");
    tcp.set_read_timeout(Some(std::time::Duration::from_millis(200))).unwrap();
    let (mut client, mut server) = (Vec::new(), Vec::new());
    let (start, mut flushed) = (std::time::Instant::now(), 0);
    loop {
        for e in &events[flushed..] {
            if let Event::Send(b) = e {
                tcp.write_all(b).unwrap();
                client.extend(b);
            }
        }
        flushed = events.len();
        if done(&s, &events) || start.elapsed().as_secs() > 10 || events.iter().any(|e| matches!(e, Event::Failed(_))) {
            let events = others(events);
            return (s, client, server, events);
        }
        let mut buf = [0u8; 65536];
        match tcp.read(&mut buf) {
            Ok(0) => return (s, client, server, others(events)),
            Ok(n) => {
                server.extend(&buf[..n]);
                events.extend(s.feed(&buf[..n]));
            }
            Err(_) => {}
        }
    }
}

/// Signs in and runs `asks`, until each has its answer.
fn live_run(account: (&str, &str), tls: Option<(std::sync::Arc<tarae_plugin::tls::Config>, String)>, asks: &[Ask]) -> (Session, Vec<u8>, Vec<u8>, Vec<Event>) {
    let (mut s, mut events) = Session::start(Params { tls, ..params(account) }, SEED);
    for a in asks {
        events.extend(ask(&mut s, a));
    }
    let want = 1 + asks.len();
    live(s, events, move |s, ev| s.idle() && ev.iter().filter(|e| !matches!(e, Event::Send(_))).count() >= want)
}

/// As root (fake), a statement run as it is — the tests' own setup (emptying the sign-in cache, a row). The
/// session's read-only (set at sign-in) is lifted for it first: it holds for statements run as they are too.
fn as_root(sql: &str) {
    let (mut s, mut events) = Session::start(params(("root", "tarae-fake-my-root")), SEED);
    events.extend(s.raw("SET SESSION TRANSACTION READ WRITE"));
    events.extend(s.raw(sql));
    let (_, _, _, events) = live(s, events, |s, ev| s.idle() && ev.iter().filter(|e| !matches!(e, Event::Send(_))).count() >= 3);
    assert!(matches!(&events[..], [Event::Ready { .. }, Event::Rows { .. }, Event::Rows { .. }]), "{sql}: {events:?}");
}

/// Writes `captured/` from the test server — run by hand when the session's bytes change (`round11.sh capture`).
#[test]
#[ignore]
fn capture_transcripts() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/driver/mysql/captured");
    std::fs::create_dir_all(&dir).unwrap();
    // (the row with non-ASCII text written over our utf8mb4 session — the image's init client wrote it as latin1)
    as_root("UPDATE shop.users SET note = 'second — é' WHERE id = 2");
    for name in SCENARIOS {
        match name {
            "signin-full-rsa" => as_root("FLUSH PRIVILEGES"),
            // (an earlier sign-in caches the account's hash: the next one is fast)
            _ => drop(live_run(scenario(name).0, None, &[])),
        }
        let (account, asks) = scenario(name);
        let (_, client, server, events) = live_run(account, None, &asks);
        assert!(!events.iter().any(|e| matches!(e, Event::Failed(_))), "{name}: {events:?}");
        assert_eq!(events.len(), 1 + asks.len(), "{name}: {events:?}");
        std::fs::write(dir.join(format!("{name}.client.raw")), client).unwrap();
        std::fs::write(dir.join(format!("{name}.server.raw")), server).unwrap();
    }
}

/// A wrong password is the server's refusal, said in its words (1045) — no `Ready`.
#[test]
#[ignore]
fn live_a_wrong_password_is_the_servers_refusal() {
    let (_, _, _, events) = live_run(("app", "tarae-fake-wrong"), None, &[]);
    assert!(matches!(&events[..], [Event::Failed(w)] if w.contains("(1045)")), "{events:?}");
}

/// `KILL QUERY` from a second session stops a long statement in the first: it comes back long before it would.
#[test]
#[ignore]
fn live_kill_query_stops_a_running_statement() {
    let id = std::sync::Arc::new(std::sync::Mutex::new(None::<u32>));
    let seen = id.clone();
    let killer = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let id = seen.lock().unwrap().expect("signed in by now");
        let (mut k, mut events) = Session::start(params(APP), SEED);
        events.extend(k.kill_query(id));
        live(k, events, |k, ev| k.idle() && ev.iter().filter(|e| !matches!(e, Event::Send(_))).count() >= 2).3
    });
    let start = std::time::Instant::now();
    let (mut s, mut events) = Session::start(params(APP), SEED);
    events.extend(s.query("select sleep(8)"));
    let (_, _, _, events) = live(s, events, move |s, ev| {
        if s.connection_id() != 0 {
            id.lock().unwrap().get_or_insert(s.connection_id());
        }
        ev.iter().any(|e| matches!(e, Event::Rows { .. } | Event::Refused(_)))
    });
    let killed = killer.join().unwrap();
    assert!(start.elapsed().as_secs() < 5, "stopped early: {events:?}");
    assert!(matches!(&killed[..], [Event::Ready { .. }, Event::Rows { .. }]), "{killed:?}");
    // MEASURED (8.4): a stopped SLEEP isn't an error — it answers 1 (the plugin's run must say "stopped" itself)
    assert!(matches!(&events[1], Event::Rows { rows, .. } if rows == &[vec![Some("1".to_string())]]), "{events:?}");
}

/// The test databases' fake CA, and one that signed nothing (target/sb/db/testdb/certs.sh).
fn test_ca(name: &str) -> std::sync::Arc<tarae_plugin::tls::Config> {
    let found = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .map(|a| a.join(format!("target/sb/db/testdb/certs/{name}.pem")))
        .find(|p| p.exists())
        .expect("the test certificates (testdb/certs.sh)");
    tarae_plugin::tls::config(&std::fs::read(found).unwrap(), None).unwrap()
}

/// Over TLS verified against the server's CA it signs in — the full authentication too (the cache emptied first:
/// the password goes inside the encryption, no RSA); a certificate another CA signed is refused.
#[test]
#[ignore]
fn live_tls_verifies_the_servers_certificate() {
    as_root("FLUSH PRIVILEGES");
    let (_, client, _, events) = live_run(APP, Some((test_ca("ca"), "127.0.0.1".into())), &[Ask::Query("select 1")]);
    assert!(matches!(&events[..], [Event::Ready { .. }, Event::Rows { .. }]), "{events:?}");
    assert!(!client.windows(APP.1.len()).any(|w| w == APP.1.as_bytes()), "never in the clear on the wire");
    let (_, _, _, events) = live_run(APP, Some((test_ca("other-ca"), "127.0.0.1".into())), &[]);
    assert!(matches!(&events[..], [Event::Failed(w)] if w.contains("UnknownIssuer")), "{events:?}");
}

/// Why `INTO OUTFILE` is refused by the plugin: the server's read-only transaction doesn't stop it — as root, into
/// the image's own `secure_file_priv` folder (inside the container, gone with it), it writes.
#[test]
#[ignore]
fn live_into_outfile_isnt_stopped_by_read_only() {
    let (mut s, mut events) = Session::start(params(("root", "tarae-fake-my-root")), SEED);
    let file = format!("/var/lib/mysql-files/tarae-test-{}.txt", std::process::id());
    events.extend(s.query_unrefused(&format!("select 1 into outfile '{file}'")));
    let (_, _, _, events) = live(s, events, |s, ev| s.idle() && ev.iter().filter(|e| !matches!(e, Event::Send(_))).count() >= 2);
    assert!(matches!(&events[1], Event::Rows { tag, .. } if tag == "1 rows affected"), "{events:?}");
}
