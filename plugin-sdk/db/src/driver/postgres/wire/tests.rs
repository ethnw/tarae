//! The session against PostgreSQL 17: transcripts captured once from the test server (`captured/`, written by the
//! `#[ignore]`d `capture_transcripts`), replayed here with no database; what the test server can't show (MD5, TLS
//! refused, a forged proof) from messages made by hand. Live tests (`#[ignore]`) need `target/sb/db/testdb`'s
//! compose file up: `round7.sh`.

use super::*;

/// The test server's account (target/sb/db/testdb/compose.yaml — fake).
const LIVE: (&str, u16) = ("127.0.0.1", 55432);
const NONCE: [u8; 18] = [7; 18];

fn params(password: &str) -> Params {
    Params { user: "app".into(), database: Some("shop".into()), password: Some(Secret::new(password)), tls: None, loopback: true }
}

/// What each scenario asks once signed in — the same for capturing and replaying.
fn scenario(name: &str) -> Vec<Request> {
    match name {
        "signin" => vec![],
        "describe" => vec![Request::Describe("select oid, relname from pg_class where relname = $1".into())],
        "describe-error" => vec![Request::Describe("select emial from pg_class".into())],
        "query" => vec![Request::Query("select 1 as n, 'x' as s union all select 2, null".into())],
        "read-only" => vec![Request::Query("create table t (i int)".into())],
        _ => panic!("no scenario {name}"),
    }
}

const SCENARIOS: [&str; 5] = ["signin", "describe", "describe-error", "query", "read-only"];

fn ask(s: &mut Session, r: &Request) -> Vec<Event> {
    match r {
        Request::Describe(sql) => s.describe(sql),
        Request::Query(sql) => s.query(sql),
    }
}

fn sent(events: &[Event]) -> Vec<u8> {
    events.iter().filter_map(|e| if let Event::Send(b) = e { Some(b.clone()) } else { None }).flatten().collect()
}

fn others(events: Vec<Event>) -> Vec<Event> {
    events.into_iter().filter(|e| !matches!(e, Event::Send(_))).collect()
}

fn captured(name: &str) -> (Vec<u8>, Vec<u8>) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/driver/postgres/captured");
    let read = |side: &str| std::fs::read(dir.join(format!("{name}.{side}.raw"))).unwrap_or_else(|e| panic!("{name}.{side}: {e}"));
    (read("client"), read("server"))
}

/// A scenario replayed: the server's bytes fed in pieces of `piece` bytes — what our side sent, and the events.
fn replay(name: &str, piece: usize) -> (Vec<u8>, Vec<Event>) {
    let (_, server) = captured(name);
    let (mut s, mut events) = Session::start(params("tarae-fake-pg"), NONCE);
    for r in scenario(name) {
        events.extend(ask(&mut s, &r));
    }
    for chunk in server.chunks(piece) {
        events.extend(s.feed(chunk));
    }
    (sent(&events), others(events))
}

/// A message as the server frames one.
fn msg(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut m = vec![tag];
    m.extend(((body.len() + 4) as u32).to_be_bytes());
    m.extend(body);
    m
}

fn auth(code: u32, rest: &[u8]) -> Vec<u8> {
    msg(b'R', &[&code.to_be_bytes()[..], rest].concat())
}

/// The client's messages after the startup message: (tag, body).
fn client_messages(bytes: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let startup = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
    let mut rest = &bytes[startup..];
    let mut out = Vec::new();
    while rest.len() >= 5 {
        let len = u32::from_be_bytes(rest[1..5].try_into().unwrap()) as usize;
        out.push((rest[0], rest[5..1 + len].to_vec()));
        rest = &rest[1 + len..];
    }
    out
}

// ── Replayed from the server ──

/// SCRAM is ours: given the nonce the capture used, what we send is byte for byte what PostgreSQL 17 accepted —
/// the proof included — and the server's own proof checks out (else no `Ready`).
#[test]
fn signing_in_with_scram_is_byte_for_byte_what_the_server_accepted() {
    let (client, _) = captured("signin");
    let (sent, events) = replay("signin", usize::MAX);
    assert_eq!(sent, client, "what we send now is what the server accepted then");
    assert!(matches!(&events[..], [Event::Ready { version }] if version.starts_with("17.")), "{events:?}");
}

/// A check must never run the statement: Parse, Describe, Sync — no Bind, no Execute — and the server's answer
/// is the parameters' types and the columns with the tables they come from.
#[test]
fn describe_runs_nothing() {
    let (client, _) = captured("describe");
    let (sent, events) = replay("describe", usize::MAX);
    assert_eq!(sent, client);
    let tags: Vec<u8> = client_messages(&client).iter().map(|(t, _)| *t).collect();
    assert!(tags.contains(&b'P') && tags.contains(&b'D') && tags.contains(&b'S'), "{tags:?}");
    assert!(!tags.contains(&b'E') && !tags.contains(&b'B'), "nothing bound or executed: {tags:?}");
    // pg_class is table 1259; oid is type 26, name type 19
    let col = |name: &str, type_id| Column { name: name.into(), type_id, table: Table::Oid(1259) };
    assert_eq!(events[1], Event::Described { params: vec![19], columns: vec![col("oid", 26), col("relname", 19)] });
}

/// What the server refuses comes with its SQLSTATE and where in the statement — and the session goes on.
#[test]
fn a_statement_the_server_refuses_says_where() {
    let (sent, events) = replay("describe-error", usize::MAX);
    assert_eq!(sent, captured("describe-error").0);
    let Event::Refused(e) = &events[1] else { panic!("{events:?}") };
    assert_eq!((e.code.as_str(), e.position), ("42703", Some(8)), "{e:?}");
}

/// Rows come back as text, NULL as nothing, with their columns and the command's tag.
#[test]
fn rows_come_back_as_text_with_their_columns() {
    let (_, events) = replay("query", usize::MAX);
    let Event::Rows { columns, rows, tag } = &events[1] else { panic!("{events:?}") };
    assert_eq!(columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["n", "s"]);
    assert_eq!(rows, &[vec![Some("1".into()), Some("x".into())], vec![Some("2".into()), None]]);
    assert_eq!(tag, "SELECT 2");
    assert_eq!(columns[0].table, Table::None, "an expression's column is no table's");
}

/// A row that doesn't read (a cell running past the message) means the stream can't be trusted further: the
/// session ends saying so — not an empty row shown as if the table held one.
#[test]
fn a_row_that_doesnt_read_ends_the_session() {
    let (_, server) = captured("signin");
    let (mut s, _) = Session::start(params("tarae-fake-pg"), NONCE);
    s.feed(&server);
    s.query("select 1");
    // RowDescription for one text column, then a DataRow whose one cell claims 99 bytes and has 1
    let row_description = msg(b'T', &[&[0, 1][..], b"n\0", &[0; 6], &[0, 0, 0, 25], &[0xff, 0xff], &[0xff; 4], &[0, 0]].concat());
    let out = s.feed(&[row_description, msg(b'D', &[0, 1, 0, 0, 0, 99, b'x'])].concat());
    assert!(matches!(&out[..], [Event::Failed(w)] if w.contains("row")), "{out:?}");
}

/// Read-only is asked for at the start, so a write is the server's refusal (25006) — a guard, not a boundary
/// (db-plan.md §4.2).
#[test]
fn every_statement_is_in_a_read_only_transaction_by_default() {
    let (client, _) = captured("read-only");
    let startup = &client[..u32::from_be_bytes(client[..4].try_into().unwrap()) as usize];
    assert!(startup.windows(33).any(|w| w == b"default_transaction_read_only\0on\0"), "the startup message asks for it");
    let (_, events) = replay("read-only", usize::MAX);
    assert!(matches!(&events[1], Event::Refused(e) if e.code == "25006"), "{events:?}");
}

/// A connection hands over bytes as they come — one at a time is the hardest case, and changes nothing.
#[test]
fn bytes_may_come_in_any_pieces() {
    for name in SCENARIOS {
        assert_eq!(replay(name, 1), replay(name, usize::MAX), "{name}");
    }
}

/// A cancel is the server's key for this session, on a connection of its own (CancelRequest: 16 bytes, code
/// 80877102) — and there's none before the server gave one.
#[test]
fn a_cancel_is_the_servers_key_on_its_own_connection() {
    let (s, _) = Session::start(params("tarae-fake-pg"), NONCE);
    assert_eq!(s.cancel_request(), None);
    let server = captured("signin").1;
    let (mut s, _) = Session::start(params("tarae-fake-pg"), NONCE);
    s.feed(&server);
    let c = s.cancel_request().expect("a key");
    assert_eq!((c.len(), &c[..8]), (16, &[0, 0, 0, 16, 4, 210, 22, 46][..]));
    // The server's BackendKeyData ('K'): its process id, then its secret — in that order
    let mut rest = &server[..];
    let key = loop {
        let len = u32::from_be_bytes(rest[1..5].try_into().unwrap()) as usize;
        if rest[0] == b'K' {
            break rest[5..1 + len].to_vec();
        }
        rest = &rest[1 + len..];
    };
    assert_eq!(&c[8..], &key[..]);
}

// ── Made by hand: what the test server doesn't do ──

/// An answer to the TLS request that isn't `S` or `N` comes before the server was checked — anyone in the way
/// may have written it: never shown (CVE-2024-10977); `N` when TLS was asked fails, saying so.
#[test]
fn an_answer_before_tls_is_never_shown() {
    let tls = || Params { tls: Some((tarae_plugin::tls::config(b"", None).unwrap(), "db.internal".into())), loopback: false, ..params("pw") };
    let (mut s, out) = Session::start(tls(), NONCE);
    assert_eq!(sent(&out), [0, 0, 0, 8, 4, 210, 22, 47], "SSLRequest first");
    let evil = [&b"E"[..], &msg(b'E', b"SFATAL\0Mtarae-fake-evil: run this\0\0")[1..]].concat();
    let failed = s.feed(&evil);
    assert!(matches!(&failed[..], [Event::Failed(w)] if !w.contains("tarae-fake-evil")), "{failed:?}");
    let (mut s, _) = Session::start(tls(), NONCE);
    assert!(matches!(&s.feed(b"N")[..], [Event::Failed(w)] if w.contains("doesn't offer TLS")));
}

/// MD5's answer is md5(md5(password + user) + salt): the user and the password each in their place.
#[test]
fn md5_answers_with_the_salted_hash() {
    let (mut s, _) = Session::start(params("tarae-fake-pg"), NONCE);
    let out = s.feed(&auth(5, &[1, 2, 3, 4]));
    let p = sent(&out);
    assert_eq!(p[0], b'p');
    let hash = md5_hash(b"app", b"tarae-fake-pg", [1, 2, 3, 4]);
    assert_eq!(&p[5..p.len() - 1], hash.as_bytes());
}

/// MD5 is guarded as plain text is: off this machine without TLS, anyone in the way can pick the salt and take the
/// answer away to crack — so it isn't sent.
#[test]
fn md5_never_goes_off_this_machine_without_tls() {
    let (mut s, _) = Session::start(Params { loopback: false, ..params("tarae-fake-pg") }, NONCE);
    let out = s.feed(&auth(5, &[1, 2, 3, 4]));
    assert!(matches!(&out[..], [Event::Failed(w)] if w.contains("MD5")), "{out:?}");
}

/// A password asked in plain text goes only over TLS or to this machine — never across a network in the clear.
#[test]
fn a_password_never_goes_in_plain_text_off_this_machine_without_tls() {
    let (mut s, _) = Session::start(Params { loopback: false, ..params("tarae-fake-pg") }, NONCE);
    let out = s.feed(&auth(3, &[]));
    assert!(matches!(&out[..], [Event::Failed(w)] if w.contains("plain text")), "{out:?}");
    let (mut s, _) = Session::start(params("tarae-fake-pg"), NONCE);
    let out = s.feed(&auth(3, &[]));
    assert_eq!(sent(&out), msg(b'p', b"tarae-fake-pg\0"), "to this machine it goes");
}

/// SCRAM to the server's first message, by hand: (session, our nonce).
fn scram_started() -> (Session, String) {
    let (mut s, _) = Session::start(params("tarae-fake-pg"), NONCE);
    let first = sent(&s.feed(&auth(10, b"SCRAM-SHA-256\0\0")));
    let text = String::from_utf8_lossy(&first).into_owned();
    let nonce = text.split("r=").nth(1).unwrap().to_string();
    (s, nonce)
}

/// Iterations past what one call computes are refused before computing — said with the count, not a stopped call.
#[test]
fn too_many_scram_iterations_are_refused_before_computing() {
    let (mut s, nonce) = scram_started();
    let out = s.feed(&auth(11, format!("r={nonce}srv,s=c2FsdA==,i=100000").as_bytes()));
    assert!(matches!(&out[..], [Event::Failed(w)] if w.contains("100000")), "{out:?}");
}

/// Fewer than one iteration (RFC 5802's least) would be a proof of nothing: refused, not answered.
#[test]
fn fewer_than_one_scram_iteration_is_refused() {
    let (mut s, nonce) = scram_started();
    let out = s.feed(&auth(11, format!("r={nonce}srv,s=c2FsdA==,i=0").as_bytes()));
    assert!(matches!(&out[..], [Event::Failed(w)] if w.contains("at least one")), "{out:?}");
}

/// The server must prove it knows the password too: a wrong proof — not the server it says it is — ends it.
#[test]
fn a_server_that_doesnt_know_the_password_is_caught() {
    let (mut s, nonce) = scram_started();
    s.feed(&auth(11, format!("r={nonce}srv,s=c2FsdA==,i=1").as_bytes()));
    let out = s.feed(&auth(12, b"v=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="));
    assert!(matches!(&out[..], [Event::Failed(w)] if w.contains("didn't match")), "{out:?}");
}

// ── Live, against the test server (#[ignore]: round7.sh) ──

/// Drives a session over TCP: (all our bytes, all the server's, the events) once `done` says so or 10 s pass.
fn live(mut s: Session, mut events: Vec<Event>, done: impl Fn(&Session, &[Event]) -> bool) -> (Vec<u8>, Vec<u8>, Vec<Event>) {
    use std::io::{Read, Write};
    let mut tcp = std::net::TcpStream::connect(LIVE).expect("the test server (round7.sh)");
    tcp.set_read_timeout(Some(std::time::Duration::from_millis(200))).unwrap();
    let (mut client, mut server) = (Vec::new(), Vec::new());
    let start = std::time::Instant::now();
    let mut flushed = 0;
    loop {
        for e in &events[flushed..] {
            if let Event::Send(b) = e {
                tcp.write_all(b).unwrap();
                client.extend(b);
            }
        }
        flushed = events.len();
        if done(&s, &events) || start.elapsed().as_secs() > 10 || events.iter().any(|e| matches!(e, Event::Failed(_))) {
            return (client, server, others(events));
        }
        let mut buf = [0u8; 8192];
        match tcp.read(&mut buf) {
            Ok(0) => return (client, server, others(events)),
            Ok(n) => {
                server.extend(&buf[..n]);
                events.extend(s.feed(&buf[..n]));
            }
            Err(_) => {}
        }
    }
}

/// Writes `captured/` from the test server — run by hand when the session's bytes change (round7.sh).
#[test]
#[ignore]
fn capture_transcripts() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/driver/postgres/captured");
    std::fs::create_dir_all(&dir).unwrap();
    for name in SCENARIOS {
        let (mut s, mut events) = Session::start(params("tarae-fake-pg"), NONCE);
        let asks = scenario(name);
        for r in &asks {
            events.extend(ask(&mut s, r));
        }
        let want = 1 + asks.len();
        let (client, server, events) = live(s, events, |s, ev| s.idle() && ev.iter().filter(|e| !matches!(e, Event::Send(_))).count() >= want);
        assert!(!events.iter().any(|e| matches!(e, Event::Failed(_))), "{name}: {events:?}");
        std::fs::write(dir.join(format!("{name}.client.raw")), client).unwrap();
        std::fs::write(dir.join(format!("{name}.server.raw")), server).unwrap();
    }
}

/// A wrong password is the server's refusal, said in its words (28P01) — no `Ready`.
#[test]
#[ignore]
fn live_a_wrong_password_is_the_servers_refusal() {
    let (s, events) = Session::start(params("tarae-fake-wrong"), NONCE);
    let (_, _, events) = live(s, events, |_, _| false);
    assert!(matches!(&events[..], [Event::Failed(w)] if w.contains("28P01")), "{events:?}");
}

/// A long statement is stopped by a CancelRequest on a connection of its own: the server says 57014.
#[test]
#[ignore]
fn live_cancel_stops_a_running_statement() {
    use std::io::Write;
    let (mut s, mut events) = Session::start(params("tarae-fake-pg"), NONCE);
    events.extend(s.query("select pg_sleep(8)"));
    let key = std::sync::Arc::new(std::sync::Mutex::new(None::<Vec<u8>>));
    let k = key.clone();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let bytes = k.lock().unwrap().clone().expect("signed in by now");
        std::net::TcpStream::connect(LIVE).unwrap().write_all(&bytes).unwrap();
    });
    let (_, _, events) = live(s, events, move |s, ev| {
        if let Some(c) = s.cancel_request() {
            key.lock().unwrap().get_or_insert(c);
        }
        ev.iter().any(|e| matches!(e, Event::Refused(_)))
    });
    canceller.join().unwrap();
    assert!(events.iter().any(|e| matches!(e, Event::Refused(e) if e.code == "57014")), "{events:?}");
}
