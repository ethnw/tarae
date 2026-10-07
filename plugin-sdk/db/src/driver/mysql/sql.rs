//! A statement read as MySQL reads it, only as far as the plugin's own refusals need (§12.2): strings, quoted names
//! and comments set aside — but `/*! … */` (and MariaDB's `/*M! … */`) read as code, because MySQL runs what's in
//! them. It only ever adds a refusal; whether something writes is the server's to say (§4.2).
//!
//! Neither reading of a session's `sql_mode` is trusted: every statement is read both ways strings may be read, and
//! a refusal from either stands.

/// Why the plugin doesn't send a statement read-only would let through — `None`: it's the server's to judge.
pub fn refusal(sql: &str) -> Option<&'static str> {
    if writes_a_server_file(sql) {
        return Some("INTO OUTFILE / DUMPFILE writes a file on the server — read-only doesn't stop it, so it isn't sent");
    }
    if sets_sql_mode(sql) {
        return Some("it changes how the server reads statements (sql_mode) — taken as a write, not sent");
    }
    if runs_sql_unread(sql) {
        return Some("it runs SQL the plugin can't read (prepared text, a procedure) — taken as a write, not sent");
    }
    None
}

/// Whether it's a `SET` that touches `sql_mode` — plain, `SESSION`/`GLOBAL`/`PERSIST`/`LOCAL`, `@@sql_mode`,
/// `@@session.sql_mode`, among other assignments, or MariaDB's `SET STATEMENT sql_mode=… FOR`. A mode like
/// `NO_BACKSLASH_ESCAPES` or `ANSI_QUOTES` changes where strings end, so what the plugin read wouldn't be what runs.
/// (A user variable named `@sql_mode` is refused too: one `@` or two isn't told apart — a refusal too many.)
pub fn sets_sql_mode(sql: &str) -> bool {
    [true, false].into_iter().any(|escapes| {
        let words = words(sql, escapes);
        words.first().is_some_and(|w| w.eq_ignore_ascii_case("set")) && words.iter().any(|w| w.eq_ignore_ascii_case("sql_mode"))
    })
}

/// Whether it runs SQL held where the plugin can't read it: `PREPARE … FROM '…'` / `EXECUTE` (the statement is a
/// string, or a variable), `CALL` (a procedure's body is the server's).
fn runs_sql_unread(sql: &str) -> bool {
    [true, false].into_iter().any(|escapes| {
        words(sql, escapes).first().is_some_and(|w| ["prepare", "execute", "call"].iter().any(|k| w.eq_ignore_ascii_case(k)))
    })
}

/// Whether it writes a file on the server's disk — `INTO OUTFILE` or `INTO DUMPFILE`, which a read-only
/// transaction doesn't stop (it guards tables, not the server's files).
///
/// Read both ways a session may read strings: with `\` escaping in them, and without — any session can set
/// `sql_mode = NO_BACKSLASH_ESCAPES` in a statement read-only allows, and then `'a\' into outfile …` is a string
/// that ends early. Either reading finding it refuses it (the reading without escapes also covers `ANSI_QUOTES`:
/// `"…"` is then a name, which has none). A string that only holds those words with a `\` before its quote is refused
/// too — a refusal too many, never one too few.
pub fn writes_a_server_file(sql: &str) -> bool {
    [true, false].into_iter().any(|escapes| {
        let words = words(sql, escapes);
        words.windows(2).any(|w| w[0].eq_ignore_ascii_case("into") && ["outfile", "dumpfile"].iter().any(|f| w[1].eq_ignore_ascii_case(f)))
    })
}

/// The statement's words, in order — what's in strings, quoted names and comments isn't among them; `escapes`:
/// whether a `\` in a string escapes the next character (MySQL's default).
fn words(sql: &str, escapes: bool) -> Vec<&str> {
    let b = sql.as_bytes();
    let (mut out, mut i) = (Vec::new(), 0);
    // Inside an executable comment: its `*/` ends it, the rest is code
    let mut running = false;
    let to = |from: usize, end: &[u8]| b[from..].windows(end.len()).position(|w| w == end).map_or(b.len(), |p| from + p + end.len());
    while i < b.len() {
        match b[i] {
            q @ (b'\'' | b'"' | b'`') => {
                // To its closing quote: doubled, it's one inside; a backslash escapes in strings (not in names)
                let mut j = i + 1;
                while j < b.len() {
                    if (escapes && b[j] == b'\\' && q != b'`') || (b[j] == q && b.get(j + 1) == Some(&q)) {
                        j += 2;
                    } else if b[j] == q {
                        break;
                    } else {
                        j += 1;
                    }
                }
                i = j + 1;
            }
            b'#' => i = to(i, b"\n"),
            // `--` starts a comment only before a space or a control character (else it's two minuses)
            b'-' if b.get(i + 1) == Some(&b'-') && b.get(i + 2).is_none_or(|c| *c <= b' ') => i = to(i, b"\n"),
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let exec = match (b.get(i + 2), b.get(i + 3)) {
                    (Some(b'!'), _) => Some(i + 3),
                    (Some(b'M'), Some(b'!')) => Some(i + 4),
                    _ => None,
                };
                match exec {
                    Some(mut j) => {
                        // (a version number may lead it: the code runs on servers from that one on)
                        while b.get(j).is_some_and(u8::is_ascii_digit) {
                            j += 1;
                        }
                        running = true;
                        i = j;
                    }
                    None => i = to(i + 2, b"*/"),
                }
            }
            b'*' if running && b.get(i + 1) == Some(&b'/') => {
                running = false;
                i += 2;
            }
            c if c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80 => {
                let start = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$' || b[i] >= 0x80) {
                    i += 1;
                }
                out.push(&sql[start..i]);
            }
            _ => i += 1,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file written on the server's disk isn't stopped by read-only (`live_into_outfile_isnt_stopped_by_read_only`
    /// shows it): the plugin refuses it — also hidden in an executable comment MySQL runs, or split by a comment —
    /// and never mistakes the words inside a string, a quoted name or a comment for it.
    #[test]
    fn into_outfile_is_refused() {
        for writes in [
            "select * from t into outfile '/tmp/x'",
            "SELECT 1 INTO   DUMPFILE '/x'",
            "select 1 /*!50000 into outfile '/x' */",
            "select 1 /*!INTO OUTFILE '/x'*/",
            "/*M!100000 select 1 into outfile '/x' */",
            "select 1 into/**/outfile '/x'",
            "select 1 into -- a comment\n outfile '/x'",
            "select 'a''b' into outfile '/x'",
            // (without backslash escapes — a session may set that — this string ends at `\"` and the rest is code)
            "select \"a\\\" into outfile\" from t",
        ] {
            assert!(writes_a_server_file(writes), "{writes}");
        }
        for reads in [
            "select 'into outfile' from t",
            "select `into`, outfile from t",
            "select 1 -- into outfile",
            "select 1 # into outfile\n",
            "select 1 /* into outfile */",
            "select 1 /*+ into outfile */",
            "select 2--1 from t into_outfile",
            "select * from t into @x",
        ] {
            assert!(!writes_a_server_file(reads), "{reads}");
        }
    }

    /// A session may read strings without backslash escapes (`SET sql_mode = 'NO_BACKSLASH_ESCAPES'` — read-only lets
    /// it): then `'a\'` is a whole string and what follows is code. Read the default way only, the file write would
    /// hide inside one long string; both readings are taken, so it's refused whichever mode the session is in.
    #[test]
    fn into_outfile_is_refused_whatever_the_backslash_mode() {
        for hidden in [
            "select 'a\\' into outfile '/x' -- '",
            "select \"a\\\" into dumpfile '/x' -- \"",
            "select 'a\\\\' into outfile '/x'",
        ] {
            assert!(writes_a_server_file(hidden), "{hidden}");
        }
        assert!(!writes_a_server_file("select 'a\\\\' as into_outfile"), "no reading finds the words");
    }

    /// The other half of not trusting one reading: a statement that would change how the server reads strings —
    /// any `SET` touching `sql_mode`, in every form MySQL and MariaDB take — is a write, never sent; a look at the
    /// mode, a `SET` of something else, the word inside a string are let through.
    #[test]
    fn a_set_of_sql_mode_is_refused_as_a_write() {
        for sets in [
            "SET sql_mode = 'NO_BACKSLASH_ESCAPES'",
            "set session sql_mode=''",
            "SET LOCAL sql_mode=''",
            "SET @@sql_mode = 'ANSI_QUOTES'",
            "SET @@SESSION.sql_mode=''",
            "SET GLOBAL sql_mode=''",
            "SET @@global.SQL_MODE = ''",
            "SET PERSIST sql_mode=''",
            "set autocommit = 1, sql_mode = ''",
            "/*!40101 SET sql_mode='' */",
            "SET STATEMENT sql_mode='' FOR select 1",
        ] {
            assert!(sets_sql_mode(sets), "{sets}");
        }
        for not in ["select @@sql_mode", "SET @x = 'sql_mode'", "SET SESSION TRANSACTION READ WRITE", "select 'set sql_mode'"] {
            assert!(!sets_sql_mode(not), "{not}");
        }
    }

    /// SQL held in a string or a procedure can't be read, so none of the refusals above could see in it —
    /// `PREPARE s FROM 'select … into outfile …'` then `EXECUTE s` would write the file: refused, all three.
    #[test]
    fn sql_the_plugin_cant_read_is_refused() {
        for unread in ["PREPARE s FROM 'select 1 into outfile ''/x'''", "execute s", "EXECUTE s USING @a", "call p()", "/*!CALL p() */"] {
            assert!(refusal(unread).is_some_and(|w| w.contains("can't read")), "{unread}");
        }
        assert_eq!(refusal("select 'prepare', `call` from t"), None);
    }
}
