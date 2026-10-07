use super::*;
use crate::conn::Password;

/// The project at `/p`: what `:db` finds when `.tarae/db.toml` says `config`, a git-ignored `.tarae/db.local.toml`
/// says `local`, the compose files say theirs and the environment has `vars` — every action the plugin answered
/// with, in order.
fn drive(s: &mut State, config: Option<&str>, local: Option<&str>, compose: &[(&str, &str)], vars: &[(&str, &str)]) -> Vec<Action> {
    // (tarae hands every call fresh entropy; here the state is given its random bytes, else the connection in use
    // would wait for some — `live::tests::no_randomness_no_sign_in`)
    s.seed.get_or_insert([1; 32]);
    let mut out = s.run("db", Some("/p/src/main.rs".into()));
    let config_found = config.map(|_| vec!["/p/.tarae/db.toml".to_string()]).unwrap_or_default();
    let compose_found = compose.iter().map(|(p, _)| p.to_string()).collect();
    out.extend(s.on_events(vec![
        HostEvent::Found((FIND_CONFIG, config_found)),
        HostEvent::Found((FIND_COMPOSE, compose_found)),
        HostEvent::Found((FIND_ROOT, vec!["/p/Cargo.toml".into(), "/p/README.md".into()])),
        // (git-ignored: `find` leaves it out)
        HostEvent::Found((FIND_LOCAL, Vec::new())),
    ]));
    let mut came: Vec<HostEvent> = Vec::new();
    if let Some(text) = config {
        came.push(HostEvent::File((CONFIG, text.as_bytes().to_vec())));
    }
    came.push(match local {
        Some(text) => HostEvent::File((LOCAL, text.as_bytes().to_vec())),
        None => HostEvent::Failed((LOCAL, "/p/.tarae/db.local.toml: No such file or directory (os error 2)".into())),
    });
    for (i, (_, text)) in compose.iter().enumerate() {
        came.push(HostEvent::File((COMPOSE + i as u64, text.as_bytes().to_vec())));
    }
    for (i, name) in driver::env_names().iter().enumerate() {
        let value = vars.iter().find(|(n, _)| n == name).map(|(_, v)| v.to_string());
        came.push(HostEvent::Env((ENV + i as u64, value)));
    }
    out.extend(s.on_events(came));
    out
}

fn badge(out: &[Action]) -> Option<(String, Tone)> {
    out.iter().rev().find_map(|a| match a {
        Action::Badge(b) => b.clone(),
        _ => None,
    })
}

fn side_rows(s: &State) -> Vec<String> {
    s.side.as_ref().map(|side| side.rows.iter().map(|r| format!("{}{}", "  ".repeat(r.depth as usize), r.text)).collect()).unwrap_or_default()
}

const CONFIG_TEXT: &str ="[[connection]]\nname = \"dev\"\nurl = \"postgres://app@localhost:5433/shop\"\npassword-env = \"PGPASSWORD\"\npaths = [\"migrations/**\"]\n\n[[connection]]\nname = \"prod\"\nurl-env = \"DATABASE_URL\"\n";
const COMPOSE_FILE: &str = "services:\n  db:\n    image: postgres:17\n    environment: {POSTGRES_PASSWORD: tarae-fake-pg}\n    ports: [\"5433:5432\"]\n";

/// What `:db` finds is put together in one list — the config's first (they're named on purpose), then compose
/// services, then the environment — each name once, a variable a config names (`url-env`) once too (as the
/// config's), and with several and none chosen, it asks.
#[test]
fn discovery_puts_config_compose_and_env_together() {
    let mut s = State::default();
    let out = drive(&mut s, Some(CONFIG_TEXT), None, &[("/p/compose.yaml", COMPOSE_FILE)], &[("DATABASE_URL", "postgres://ro@db.prod.internal/shop"), ("REDIS_URL", "redis://localhost:6379")]);
    let names: Vec<&str> = s.connections.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["dev", "prod", "db", "redis_url"]);
    let prod = &s.connections[1];
    assert_eq!((prod.shown(), prod.production()), ("postgres://ro@db.prod.internal:5432/shop".into(), true), "url-env read");
    assert_eq!(badge(&out), Some(("db · 4 found — :db to choose".into(), Tone::Info)));
    let picked = s.run("db", None);
    let Some(Action::Pick(p)) = picked.first() else { panic!("{picked:?}") };
    assert_eq!(p.items.len(), 4);
    assert!(p.items[1].1.starts_with("prod · production  "), "said before what a long line loses: {:?}", p.items[1]);
    let out = s.on_events(vec![HostEvent::Answer((PICK, Some("db".into())))]);
    assert_eq!(badge(&out), Some(("db db".into(), Tone::Ok)));
    let side = s.side.as_ref().unwrap();
    let at = side.rows.iter().position(|r| r.id == "conn:db").unwrap();
    assert_eq!(side.rows[at].detail, "pg · in use");
    assert_eq!(side.rows[at + 1].text, "postgres@localhost:5433/postgres");
    assert_eq!(side.rows[at + 2].text, "password known");
    let prod = side.rows.iter().find(|r| r.id == "conn:prod").unwrap();
    assert_eq!(prod.detail, "pg · prod", "production said on its row, short");
}

/// A connection's `paths` make it the file's: the migrations get `dev` whatever was chosen — and the status line
/// says production in the error colour when that's where the file in front would go.
#[test]
fn the_file_in_front_gets_the_connection_whose_paths_cover_it() {
    let mut s = State::default();
    let config = format!("{CONFIG_TEXT}paths = [\"ops/**\"]\n");
    drive(&mut s, Some(&config), None, &[], &[("DATABASE_URL", "postgres://ro@db.prod.internal/shop")]);
    s.chosen = Some("prod".into());
    let shown = |s: &mut State, path: &str| {
        s.front = Some(path.into());
        let out = s.show();
        badge(&out).or_else(|| s.badge.clone())
    };
    assert_eq!(shown(&mut s, "/p/migrations/0042_orders.sql"), Some(("db dev".into(), Tone::Ok)));
    assert_eq!(shown(&mut s, "/p/ops/fix.sql"), Some(("db prod · production".into(), Tone::Error)));
    assert_eq!(shown(&mut s, "/p/src/main.rs"), Some(("db prod · production".into(), Tone::Error)), "else the one chosen");
}

/// What can't be read is in the tab, said where it is and why — never a silent "nothing found".
#[test]
fn what_cant_be_read_is_said_in_the_tab() {
    let mut s = State::default();
    s.run("db", None);
    s.on_events(vec![
        HostEvent::Found((FIND_CONFIG, vec!["/p/.tarae/db.toml".into()])),
        HostEvent::Failed((FIND_COMPOSE, "it may not read the project".into())),
        HostEvent::Failed((FIND_ROOT, "it may not read the project".into())),
        HostEvent::Failed((FIND_LOCAL, "it may not read the project".into())),
    ]);
    let mut came = vec![
        HostEvent::File((CONFIG, b"[[connection]]\nname = \"a\"\nhost = 5\n".to_vec())),
        HostEvent::Failed((LOCAL, "/p/.tarae/db.local.toml: No such file or directory (os error 2)".into())),
    ];
    came.extend((0..driver::env_names().len()).map(|i| HostEvent::Env((ENV + i as u64, None))));
    let out = s.on_events(came);
    assert_eq!(
        side_rows(&s),
        ["the project's files", "  it may not read the project", ".tarae/db.toml", "  line 3: not what .tarae/db.toml takes there", "no databases found", "  a .tarae/db.toml, a compose service or $DATABASE_URL names one"]
    );
    assert_eq!(badge(&out), Some(("db · none found".into(), Tone::Warning)));
}

/// Your own `.tarae/db.local.toml` is git-ignored, so `find` never lists it: it's read by its path, in the folder
/// the project's top files are in — with no `.tarae/db.toml` too — and a connection of yours replaces the project's
/// of the same name, where it stood.
#[test]
fn a_personal_config_is_read_by_its_path_and_wins_by_name() {
    let local = "[[connection]]\nname = \"dev\"\nurl = \"postgres://me@localhost:5434/shop\"\nask = true\n\n[[connection]]\nname = \"mine\"\nkind = \"redis\"\nport = 6380\n";
    let mut s = State::default();
    let out = drive(&mut s, None, Some(local), &[], &[]);
    assert!(out.contains(&Action::ReadFile((LOCAL, "/p/.tarae/db.local.toml".into()))), "{out:?}");
    assert_eq!(s.connections.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["dev", "mine"]);
    let mut s = State::default();
    drive(&mut s, Some(CONFIG_TEXT), Some(local), &[], &[]);
    let shown: Vec<String> = s.connections.iter().map(|c| format!("{} {} {}", c.name, c.shown(), c.origin.said())).collect();
    assert_eq!(shown, ["dev postgres://me@localhost:5434/shop .tarae/db.local.toml", "prod postgres://localhost:5432 .tarae/db.toml", "mine redis://localhost:6380 .tarae/db.local.toml"]);
}

/// A personal config git doesn't ignore would be committed with the project: `find` lists only what git doesn't
/// ignore, so its being found says so.
#[test]
fn a_personal_config_git_doesnt_ignore_is_said_so() {
    let mut s = State::default();
    s.run("db", None);
    let mut came = vec![
        HostEvent::Found((FIND_CONFIG, Vec::new())),
        HostEvent::Found((FIND_COMPOSE, Vec::new())),
        HostEvent::Found((FIND_ROOT, vec!["/p/Cargo.toml".into()])),
        HostEvent::Found((FIND_LOCAL, vec!["/p/.tarae/db.local.toml".into()])),
        HostEvent::File((LOCAL, b"[[connection]]\nname = \"mine\"\nkind = \"redis\"\n".to_vec())),
    ];
    came.extend((0..driver::env_names().len()).map(|i| HostEvent::Env((ENV + i as u64, None))));
    s.on_events(came);
    let rows = side_rows(&s);
    assert!(rows.windows(2).any(|w| w == [".tarae/db.local.toml", "  isn't git-ignored — it would be committed with the project"]), "{rows:#?}");
}

/// libpq reads `~/.pgpass` only when no one else may (0600); the plugin isn't told a file's mode yet (API 0.3),
/// so a connection whose password is there says the mode wasn't checked — not silence that reads as "fine".
#[test]
fn a_password_file_says_its_mode_wasnt_checked() {
    let mut s = State::default();
    drive(&mut s, Some("[[connection]]\nname = \"dev\"\nkind = \"postgres\"\npassword-file = \"~/.pgpass\"\n"), None, &[], &[]);
    let rows = side_rows(&s);
    assert!(rows.windows(2).any(|w| w == ["  password: ~/.pgpass", "  its mode can't be checked yet"]), "{rows:#?}");
}

/// Encryption without the server's certificate checked comes only from `tls = "require-unverified"` — and the tab
/// says so under it in the warning tone, every time; `require` alone is verified and says nothing.
#[test]
fn unverified_tls_is_said_in_the_tab() {
    let mut s = State::default();
    let config = "[[connection]]\nname = \"a\"\nkind = \"postgres\"\nhost = \"db.internal\"\ntls = \"require-unverified\"\n\n\
                  [[connection]]\nname = \"b\"\nkind = \"postgres\"\nhost = \"db.internal\"\ntls = \"require\"\n";
    drive(&mut s, Some(config), None, &[], &[]);
    let side = s.side.as_ref().unwrap();
    let notes: Vec<(&str, Tone)> = side.rows.iter().filter(|r| r.note).map(|r| (r.text.as_str(), r.tone)).collect();
    assert_eq!(notes, [("TLS not verified", Tone::Warning)], "once, under `a`");
    let at = side.rows.iter().position(|r| r.text == "TLS not verified").unwrap();
    let owner = side.rows[..at].iter().rev().find(|r| r.depth == 0).unwrap();
    assert_eq!(owner.text, "a");
}

// ── The connection in use, live ──

/// The test server's connection (target/sb/db/testdb — fake), its password from `$PGPASSWORD`.
const LIVE_CONFIG: &str = "[[connection]]\nname = \"dev\"\nurl = \"postgres://app@127.0.0.1:55432/shop\"\npassword-env = \"PGPASSWORD\"\n";
/// What the captured exchange's nonce was made of (`driver/postgres/wire/tests.rs`: 18 sevens).
const SEED: [u8; 32] = [7; 32];

/// `:db` with `config`'s connections, `PGPASSWORD` set: (the state, what it answered).
fn live_drive(config: &str, vars: &[(&str, &str)]) -> (State, Vec<Action>) {
    let mut s = State { seed: Some(SEED), ..State::default() };
    let out = drive(&mut s, Some(config), None, &[], vars);
    (s, out)
}

fn captured(name: &str) -> (Vec<u8>, Vec<u8>) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/driver/postgres/captured");
    let read = |side: &str| std::fs::read(dir.join(format!("{name}.{side}.raw"))).unwrap();
    (read("client"), read("server"))
}

/// The connections opened: their ids, hosts, ports.
fn connects(out: &[Action]) -> Vec<(u64, String, u64)> {
    out.iter().filter_map(|a| if let Action::Connect(c) = a { Some((c.id, c.host.clone(), c.port)) } else { None }).collect()
}

fn sent_on(out: &[Action], id: u64) -> Vec<u8> {
    out.iter().filter_map(|a| if let Action::Send((i, b)) = a { (*i == id).then(|| b.clone()) } else { None }).flatten().collect()
}

/// A PostgreSQL message as the server frames one.
fn pg_msg(tag: u8, body: &[u8]) -> Vec<u8> {
    [&[tag][..], &((body.len() + 4) as u32).to_be_bytes(), body].concat()
}

/// The tab's rows under a connection: (text, tone).
fn rows_of(s: &State, name: &str) -> Vec<(String, Tone)> {
    let rows = &s.side.as_ref().unwrap().rows;
    let at = rows.iter().position(|r| r.id == format!("conn:{name}")).unwrap();
    rows[at + 1..].iter().take_while(|r| r.depth > 0).map(|r| (r.text.clone(), r.tone)).collect()
}

/// The connection in use is opened, and what goes on it is its session's own bytes — the whole SCRAM exchange
/// PostgreSQL 17 accepted, byte for byte — so the badge and the tab say the server's version once it's signed in.
#[test]
fn the_connection_in_use_signs_in_and_says_its_servers_version() {
    let (mut s, out) = live_drive(LIVE_CONFIG, &[("PGPASSWORD", "tarae-fake-pg")]);
    let id = live::CONNECT + 1;
    assert_eq!(connects(&out), [(id, "127.0.0.1".into(), 55432)]);
    assert_eq!(rows_of(&s, "dev").last(), Some(&("connecting…".to_string(), Tone::Info)));
    let (client, server) = captured("signin");
    let mut out = s.on_events(vec![HostEvent::Connected(id)]);
    out.extend(s.on_events(vec![HostEvent::Data((id, server))]));
    assert_eq!(sent_on(&out, id), client, "the session's bytes, all of them, on its connection");
    let (text, tone) = badge(&out).unwrap();
    assert!(text.starts_with("db dev · pg 17.") && tone == Tone::Ok, "{text}");
    assert!(rows_of(&s, "dev").iter().any(|(t, tone)| t.starts_with("connected · pg 17.") && *tone == Tone::Ok));
}

/// A server that turns it away (a wrong password: 28P01) is said with its words, and the connection isn't tried
/// again — knocking again and again would only lock the account.
#[test]
fn a_server_that_turns_it_away_isnt_asked_again() {
    let (mut s, _) = live_drive(LIVE_CONFIG, &[("PGPASSWORD", "tarae-fake-wrong")]);
    let id = live::CONNECT + 1;
    s.on_events(vec![HostEvent::Connected(id)]);
    let refused = pg_msg(b'E', b"SFATAL\0C28P01\0Mpassword authentication failed for user \"app\"\0\0");
    let out = s.on_events(vec![HostEvent::Data((id, refused))]);
    assert!(out.contains(&Action::Stop(id)), "{out:?}");
    assert!(!out.iter().any(|a| matches!(a, Action::Timer(_))), "not tried again: {out:?}");
    assert_eq!(
        rows_of(&s, "dev")[2..],
        [("not connected".into(), Tone::Warning), ("password authentication failed for user \"app\" (28P01)".into(), Tone::Warning)]
    );
    assert_eq!(badge(&out), Some(("db dev · not connected".into(), Tone::Warning)));
}

/// The plumbing is the same for every database the plugin speaks: a MySQL connection in use is opened, MySQL
/// speaks first (nothing sent on `connected`), and what goes out is its session's bytes — the sign-in MySQL 8.4
/// accepted, byte for byte — so the badge says its version.
#[test]
fn a_mysql_connection_signs_in_through_the_same_plumbing() {
    let config = "[[connection]]\nname = \"dev\"\nurl = \"mysql://app@127.0.0.1:53306/shop\"\npassword-env = \"PGPASSWORD\"\n";
    let (mut s, out) = live_drive(config, &[("PGPASSWORD", "tarae-fake-my")]);
    let id = live::CONNECT + 1;
    assert_eq!(connects(&out), [(id, "127.0.0.1".into(), 53306)]);
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/driver/mysql/captured");
    let (client, server) = (std::fs::read(dir.join("signin-fast.client.raw")).unwrap(), std::fs::read(dir.join("signin-fast.server.raw")).unwrap());
    let mut out = s.on_events(vec![HostEvent::Connected(id)]);
    assert!(sent_on(&out, id).is_empty(), "MySQL speaks first");
    out.extend(s.on_events(vec![HostEvent::Data((id, server))]));
    assert_eq!(sent_on(&out, id), client);
    assert_eq!(badge(&out).map(|b| b.0), Some("db dev · mysql 8.4.11".into()));
}

/// Redis through the same plumbing: the client speaks first (AUTH goes on `connected`), as the ACL user the URL
/// names — byte for byte what Redis took — and the badge says its version.
#[test]
fn a_redis_connection_signs_in_as_its_acl_user() {
    let config = "[[connection]]\nname = \"cache\"\nurl = \"redis://ro@127.0.0.1:56379\"\npassword-env = \"REDISCLI_AUTH\"\n";
    let (mut s, out) = live_drive(config, &[("REDISCLI_AUTH", "tarae-fake-redis-ro")]);
    let id = live::CONNECT + 1;
    assert_eq!(connects(&out), [(id, "127.0.0.1".into(), 56379)]);
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/driver/redis/captured");
    let (client, server) = (std::fs::read(dir.join("signin-ro.client.raw")).unwrap(), std::fs::read(dir.join("signin-ro.server.raw")).unwrap());
    let mut out = s.on_events(vec![HostEvent::Connected(id)]);
    out.extend(s.on_events(vec![HostEvent::Data((id, server))]));
    assert_eq!(sent_on(&out, id), client);
    assert_eq!(badge(&out), Some(("db cache · redis 7.4.11".into(), Tone::Ok)));
}

/// One that drops, or can't be reached, is tried again after a pause that doubles — a server restarting is met
/// when it's back, a server that's gone isn't hammered — and a sign-in that works starts the pauses over.
#[test]
fn a_dropped_connection_is_tried_again_after_a_pause_that_grows() {
    let (mut s, _) = live_drive(LIVE_CONFIG, &[("PGPASSWORD", "tarae-fake-pg")]);
    let id = live::CONNECT + 1;
    s.on_events(vec![HostEvent::Connected(id)]);
    s.on_events(vec![HostEvent::Data((id, captured("signin").1))]);
    let out = s.on_events(vec![HostEvent::Closed((id, String::new()))]);
    assert!(out.contains(&Action::Timer((live::RETRY, 2_000))), "{out:?}");
    assert_eq!(rows_of(&s, "dev")[2].0, "not connected — trying again");
    let out = s.on_events(vec![HostEvent::Tick(live::RETRY)]);
    assert_eq!(connects(&out), [(id + 1, "127.0.0.1".into(), 55432)], "the next attempt, its own id");
    let out = s.on_events(vec![HostEvent::Failed((id + 1, "127.0.0.1:55432: Connection refused".into()))]);
    assert!(out.contains(&Action::Timer((live::RETRY, 4_000))), "longer: {out:?}");
    // A late word from the first attempt changes nothing
    assert!(s.on_events(vec![HostEvent::Data((id, vec![b'Z', 0, 0, 0, 5, b'I']))]).iter().all(|a| !matches!(a, Action::Send(_))));
    s.on_events(vec![HostEvent::Tick(live::RETRY)]);
    s.on_events(vec![HostEvent::Connected(id + 2)]);
    s.on_events(vec![HostEvent::Data((id + 2, captured("signin").1))]);
    let out = s.on_events(vec![HostEvent::Closed((id + 2, String::new()))]);
    assert!(out.contains(&Action::Timer((live::RETRY, 2_000))), "signed in: the pauses start over — {out:?}");
}

/// The password comes from where the connection says, at the moment of connecting: a variable (unset: said), a
/// `~/.pgpass` line (read then, as libpq does), or asked — which waits for API 0.3's masked prompt, said so.
/// Without one at hand, nothing is opened.
#[test]
fn the_password_comes_from_where_the_connection_says() {
    let (s, out) = live_drive(LIVE_CONFIG, &[]);
    assert!(connects(&out).is_empty());
    assert_eq!(rows_of(&s, "dev")[2..], [("not connected".into(), Tone::Info), ("$PGPASSWORD isn't set".into(), Tone::Info)]);
    let asked = LIVE_CONFIG.replace("password-env = \"PGPASSWORD\"", "ask = true");
    let (s, out) = live_drive(&asked, &[]);
    assert!(connects(&out).is_empty());
    assert!(rows_of(&s, "dev").iter().any(|(t, _)| t.contains("masked prompt comes with plugin API 0.3")));
    let file = LIVE_CONFIG.replace("password-env = \"PGPASSWORD\"", "password-file = \"~/.pgpass\"");
    let (mut s, out) = live_drive(&file, &[]);
    assert!(out.contains(&Action::ReadFile((live::PASSWORD_FILE, "~/.pgpass".into()))), "{out:?}");
    let out = s.on_events(vec![HostEvent::File((live::PASSWORD_FILE, b"127.0.0.1:55432:shop:app:tarae-fake-pg\n".to_vec()))]);
    assert_eq!(connects(&out).len(), 1, "its line found: connected");
    let (mut s, _) = live_drive(&file, &[]);
    let out = s.on_events(vec![HostEvent::File((live::PASSWORD_FILE, b"other:5432:*:*:tarae-fake-pg\n".to_vec()))]);
    assert!(connects(&out).is_empty());
    assert!(rows_of(&s, "dev").iter().any(|(t, _)| t == "no line of ~/.pgpass is for it"));
    // A connection naming no database is looked up as libpq does: its database is the user's name
    let (mut s, _) = live_drive(&file.replace("55432/shop", "55432"), &[]);
    let out = s.on_events(vec![HostEvent::File((live::PASSWORD_FILE, b"127.0.0.1:55432:app:app:tarae-fake-pg\n".to_vec()))]);
    assert_eq!(connects(&out).len(), 1, "the line psql would use");
}

/// The live connection is the one in use: choosing another closes it and opens that; `:db-off` closes it — no
/// connection is left open to a database nobody's using.
#[test]
fn the_live_connection_follows_the_one_in_use() {
    let two = format!("{LIVE_CONFIG}\n[[connection]]\nname = \"other\"\nurl = \"postgres://app@localhost:55433/shop\"\npassword-env = \"PGPASSWORD\"\n");
    let (mut s, out) = live_drive(&two, &[("PGPASSWORD", "tarae-fake-pg")]);
    assert!(connects(&out).is_empty(), "two and none chosen: none opened");
    let out = s.on_events(vec![HostEvent::Answer((PICK, Some("dev".into())))]);
    assert_eq!(connects(&out), [(live::CONNECT + 1, "127.0.0.1".into(), 55432)]);
    let out = s.on_events(vec![HostEvent::Answer((PICK, Some("other".into())))]);
    assert!(out.contains(&Action::Stop(live::CONNECT + 1)), "{out:?}");
    assert_eq!(connects(&out), [(live::CONNECT + 1, "localhost".into(), 55433)], "a new connection, its own attempt count");
    let out = s.run("db-off", None);
    assert!(out.contains(&Action::Stop(live::CONNECT + 1)) && out.contains(&Action::Stop(live::RETRY)), "{out:?}");
}

/// The password goes on the wire to the server it's for — and nowhere else: whatever it holds, no other action
/// carries it, also when the server asks for it in plain text and then turns it away.
#[test]
fn the_password_goes_only_to_the_wire() {
    for secret in fake_secrets(40) {
        let (mut s, mut out) = live_drive(LIVE_CONFIG, &[("PGPASSWORD", &secret)]);
        let id = live::CONNECT + 1;
        out.extend(s.on_events(vec![HostEvent::Connected(id)]));
        out.extend(s.on_events(vec![HostEvent::Data((id, pg_msg(b'R', &[0, 0, 0, 3])))]));
        assert!(sent_on(&out, id).windows(secret.len()).any(|w| w == secret.as_bytes()), "it was sent — else this proves nothing");
        let refused = pg_msg(b'E', b"SFATAL\0C28P01\0Mpassword authentication failed for user \"app\"\0\0");
        out.extend(s.on_events(vec![HostEvent::Data((id, refused))]));
        out.extend(s.run("db", None));
        for a in out.iter().filter(|a| !matches!(a, Action::Send((i, _)) if *i == id)) {
            assert!(!format!("{a:?}").contains(&secret), "{secret:?} reached {a:?}");
        }
    }
}

/// TLS as the connection says, before a byte of the account goes out: `require-unverified` and verify (public CAs,
/// or a `ca-file` read first) open with PostgreSQL's TLS request; off on this machine, with the startup message. A
/// CA file that holds no certificate is said, not taken as "trust nothing" and failed later in a riddle.
#[test]
fn tls_is_asked_as_the_connection_says() {
    let ssl_request: &[u8] = &[0, 0, 0, 8, 4, 210, 22, 47];
    let first_sent = |config: &str| {
        let (mut s, out) = live_drive(config, &[("PGPASSWORD", "tarae-fake-pg")]);
        let id = connects(&out).first().map(|c| c.0).expect("connected");
        sent_on(&s.on_events(vec![HostEvent::Connected(id)]), id)
    };
    let tls = |mode: &str| LIVE_CONFIG.replace("password-env", &format!("tls = \"{mode}\"\npassword-env"));
    assert_eq!(first_sent(&tls("require-unverified")), ssl_request);
    assert_eq!(first_sent(&tls("verify")), ssl_request, "verified against the public CAs");
    assert_ne!(first_sent(LIVE_CONFIG), ssl_request, "off on this machine");
    let with_ca = format!("{}ca-file = \"certs/ca.pem\"\n", tls("verify"));
    let (mut s, out) = live_drive(&with_ca, &[("PGPASSWORD", "tarae-fake-pg")]);
    assert!(out.contains(&Action::ReadFile((live::CA_FILE, "/p/certs/ca.pem".into()))) && connects(&out).is_empty(), "{out:?}");
    s.on_events(vec![HostEvent::File((live::CA_FILE, b"not a certificate\n".to_vec()))]);
    assert!(rows_of(&s, "dev").iter().any(|(t, _)| t.starts_with("its ca-file: ")), "{:?}", rows_of(&s, "dev"));
}

/// Encryption with nobody checked at the other end is said on the status line too, in the warning tone — on every
/// buffer, not only in the tab.
#[test]
fn unverified_tls_is_said_on_the_badge_too() {
    let config = LIVE_CONFIG.replace("password-env", "tls = \"require-unverified\"\npassword-env");
    let (_, out) = live_drive(&config, &[("PGPASSWORD", "tarae-fake-pg")]);
    assert_eq!(badge(&out), Some(("db dev · TLS not verified".into(), Tone::Warning)));
}

// ── Live, against the test server (#[ignore]: round11.sh) ──

/// The test databases' fake CA, and another that signed nothing (target/sb/db/testdb/certs.sh).
fn test_ca(name: &str) -> String {
    // (up from the crate — round5.sh's broken copies live elsewhere in the checkout)
    let found = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .map(|a| a.join(format!("target/sb/db/testdb/certs/{name}.pem")))
        .find(|p| p.exists());
    found.expect("the test certificates (testdb/certs.sh)").to_string_lossy().into_owned()
}

/// Does what the plugin asks over real TCP and files, until `done` or 10 s: the actions, all of them.
fn over_tcp(s: &mut State, out: Vec<Action>, done: impl Fn(&State) -> bool) -> Vec<Action> {
    use std::io::{Read, Write};
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
    tarae_plugin::now::set(ms, &[0x5a; 32]);
    let (mut all, mut todo) = (Vec::new(), out);
    let mut socks: BTreeMap<u64, std::net::TcpStream> = BTreeMap::new();
    let start = std::time::Instant::now();
    while !done(s) && start.elapsed().as_secs() < 10 {
        let mut came = Vec::new();
        for a in std::mem::take(&mut todo) {
            match &a {
                Action::Connect(c) => match std::net::TcpStream::connect((c.host.as_str(), c.port as u16)) {
                    Ok(t) => {
                        t.set_read_timeout(Some(std::time::Duration::from_millis(100))).unwrap();
                        socks.insert(c.id, t);
                        came.push(HostEvent::Connected(c.id));
                    }
                    Err(e) => came.push(HostEvent::Failed((c.id, e.to_string()))),
                },
                Action::Send((id, b)) => socks.get_mut(id).unwrap().write_all(b).unwrap(),
                Action::Stop(id) => drop(socks.remove(id)),
                Action::ReadFile((id, path)) => came.push(match std::fs::read(path) {
                    Ok(b) => HostEvent::File((*id, b)),
                    Err(e) => HostEvent::Failed((*id, e.to_string())),
                }),
                _ => {}
            }
            all.push(a);
        }
        for (id, t) in &mut socks {
            let mut buf = [0u8; 16384];
            match t.read(&mut buf) {
                Ok(0) => came.push(HostEvent::Closed((*id, String::new()))),
                Ok(n) => came.push(HostEvent::Data((*id, buf[..n].to_vec()))),
                Err(_) => {}
            }
        }
        if !came.is_empty() {
            todo = s.on_events(came);
        }
    }
    all.extend(todo);
    all
}

fn signed_in_or_down(s: &State) -> bool {
    matches!(s.live.as_ref().map(|l| &l.status), Some(Status::Ready { .. } | Status::Down { .. } | Status::Cannot(_)))
}

/// The plugin, as tarae drives it, signs in to PostgreSQL 17 — in the clear on this machine, and over TLS
/// verified against the server's CA; a certificate another CA signed is refused (and not tried again: it won't
/// change by waiting), and only `require-unverified` takes it, said so.
#[test]
#[ignore]
fn live_the_plugin_signs_in_plain_and_over_verified_tls() {
    let tls = |mode: &str, ca: Option<&str>| {
        let ca = ca.map(|c| format!("ca-file = \"{}\"\n", test_ca(c))).unwrap_or_default();
        LIVE_CONFIG.replace("password-env", &format!("tls = \"{mode}\"\n{ca}password-env"))
    };
    for (config, ok) in [
        (LIVE_CONFIG.to_string(), true),
        (tls("verify", Some("ca")), true),
        (tls("verify", Some("other-ca")), false),
        (tls("require-unverified", None), true),
    ] {
        let mut s = State::default();
        let out = drive(&mut s, Some(&config), None, &[], &[("PGPASSWORD", "tarae-fake-pg")]);
        over_tcp(&mut s, out, signed_in_or_down);
        let status = s.live.as_ref().map(|l| l.status.clone());
        match ok {
            true => assert!(matches!(&status, Some(Status::Ready { version }) if version.starts_with("17.")), "{config}: {status:?}"),
            false => assert!(matches!(&status, Some(Status::Down { why, again: false }) if why.contains("UnknownIssuer")), "{config}: {status:?}"),
        }
    }
}

// ── No secret reaches any action ──

/// Passwords made of what breaks quoting and URLs, from a fixed seed.
pub(crate) fn fake_secrets(n: usize) -> Vec<String> {
    let chars: Vec<char> = "abcXYZ019:@/%?#&=$\"'\\ !~-_.+é".chars().collect();
    let mut x: u64 = 0x2545_f491_4f6c_dd1d;
    (0..n)
        .map(|_| {
            let mut s = String::from("tf");
            for _ in 0..12 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                s.push(chars[(x % chars.len() as u64) as usize]);
            }
            s
        })
        .collect()
}

fn toml_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn yaml_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// The one leak that matters: a password, wherever it came from — a compose file, a variable, a URL, a config
/// that wrongly holds one, a file that doesn't parse — never reaches anything the plugin answers with (tab,
/// status line, messages, the picker), through every command and key.
#[test]
fn no_secret_reaches_any_action() {
    for secret in fake_secrets(40) {
        let enc = url::encode(&secret);
        // (compose reads `$` as interpolation: `$$` is one)
        let (t, y) = (toml_str(&secret), yaml_str(&secret.replace('$', "$$")));
        let config = format!(
            "[[connection]]\nname = \"dev\"\nurl-env = \"DATABASE_URL\"\n\n\
             [[connection]]\nname = \"wrong\"\nkind = \"postgres\"\npassword = {t}\n\n\
             [[connection]]\nname = \"wrong-url\"\nurl = \"postgres://u:{enc}@h/db\"\n"
        );
        // and passwords around `${` that's never closed, or a `$` in the middle — compose's interpolation misread
        let bad = |v: String| yaml_str(&v);
        let (b1, b2, b3) = (bad(format!("${{{secret}")), bad(format!("{secret}${{{secret}")), bad(format!("x${secret}")));
        let compose = format!(
            "services:\n  db:\n    image: postgres\n    environment:\n      POSTGRES_PASSWORD: {y}\n    ports: [\"5433:5432\"]\n  \
             cache:\n    image: redis\n    command: ['redis-server', '--requirepass', {y}]\n    ports: [\"6379:6379\"]\n  \
             bad1:\n    image: postgres\n    environment:\n      POSTGRES_PASSWORD: {b1}\n  \
             bad2:\n    image: mysql\n    environment:\n      MYSQL_ROOT_PASSWORD: {b2}\n  \
             bad3:\n    image: redis\n    command: ['redis-server', '--requirepass', {b3}]\n"
        );
        let url = format!("postgres://app:{enc}@db.internal:5432/shop");
        let redis = format!("redis://:{enc}@localhost:6379/0");
        let vars = [("DATABASE_URL", url.as_str()), ("REDIS_URL", redis.as_str()), ("PGPASSWORD", secret.as_str()), ("REDISCLI_AUTH", secret.as_str())];
        let mut s = State::default();
        let local = format!("[[connection]]\nname = \"mine\"\nkind = \"redis\"\npassword = {t}\n");
        let mut out = drive(&mut s, Some(&config), Some(&local), &[("/p/compose.yaml", &compose)], &vars);
        // (it did take the passwords in — else this test would prove nothing)
        let given = s.connections.iter().filter(|c| matches!(&c.password, Password::Given(p) if p.expose() == secret)).count();
        assert!(given >= 4, "{secret:?}: {given} connections hold it");
        out.extend(s.run("db", None));
        for c in s.connections.clone() {
            out.extend(s.on_events(vec![HostEvent::Answer((PICK, Some(c.name.clone())))]));
            out.extend(s.on_events(vec![HostEvent::Key(("u".into(), None, Some(format!("conn:{}", c.name))))]));
        }
        for front in ["/p/compose.yaml", "/p/src/a.rs"] {
            s.front = Some(front.into());
            out.extend(s.show());
        }
        out.extend(s.run("db-off", None));
        // A config and a compose file that don't parse, with the password in them
        let mut broken = State::default();
        let bad_config = format!("[[connection]]\nname = \"x\"\nport = {t}\n");
        let bad_compose = format!("services:\n  db:\n    image: postgres\n    environment: [POSTGRES_PASSWORD={y}\n");
        out.extend(drive(&mut broken, Some(&bad_config), Some(&bad_config), &[("/p/compose.yaml", &bad_compose)], &vars));
        for a in &out {
            let said = format!("{a:?}");
            for form in [&secret, &enc, &t, &y] {
                assert!(!said.contains(form.as_str()), "{secret:?} reached an action: {said}");
            }
            // Not a piece of it either (8 characters in a row)
            let chars: Vec<char> = secret.chars().collect();
            for w in chars.windows(8) {
                let piece: String = w.iter().collect();
                assert!(!said.contains(&piece), "a piece of {secret:?} reached an action: {said}");
            }
        }
    }
}
