use super::*;
use crate::conn::Password;

/// The project at `/p`: what `:db` finds when `.tarae/db.toml` says `config`, a git-ignored `.tarae/db.local.toml`
/// says `local`, the compose files say theirs and the environment has `vars` — every action the plugin answered
/// with, in order.
fn drive(s: &mut State, config: Option<&str>, local: Option<&str>, compose: &[(&str, &str)], vars: &[(&str, &str)]) -> Vec<Action> {
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

// ── No secret reaches any action ──

/// Passwords made of what breaks quoting and URLs, from a fixed seed.
fn fake_secrets(n: usize) -> Vec<String> {
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
