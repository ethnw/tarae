//! What the plugin sends Redis as a read (§12.3). Redis has no read-only mode (VERIFIED `redis-readonly`: READONLY is
//! for cluster replicas) and its `@read` category isn't "writes nothing" (VERIFIED `redis-cmd-pfcount`: PFCOUNT is
//! `@read` yet "technically a write command") — so a list of the plugin's own, each a read by what it does, narrowed
//! per server by `COMMAND INFO`'s flags (`wire.rs`). The real wall is an ACL user (`+@read -@dangerous`).

/// Reads, by name (a container command's subcommand after `|`, as `COMMAND INFO` names it).
pub const READS: &[&str] = &[
    // strings, keys
    "get", "mget", "getrange", "strlen", "exists", "type", "ttl", "pttl", "expiretime", "pexpiretime", "dbsize",
    "getbit", "bitcount", "bitpos", "bitfield_ro", "lcs", "object|encoding", "object|freq", "object|idletime",
    "object|refcount", "memory|usage",
    // hashes, lists, sets, sorted sets, streams, geo
    "hget", "hmget", "hgetall", "hkeys", "hvals", "hlen", "hexists", "hstrlen", "hrandfield", "hscan",
    "lrange", "llen", "lindex", "lpos",
    "smembers", "sismember", "smismember", "scard", "srandmember", "sscan", "sinter", "sunion", "sdiff", "sintercard",
    "zrange", "zrangebyscore", "zrangebylex", "zrevrange", "zrevrangebyscore", "zrevrangebylex", "zscore", "zmscore",
    "zcard", "zcount", "zlexcount", "zrank", "zrevrank", "zrandmember", "zscan", "zinter", "zunion", "zdiff",
    "zintercard",
    "xrange", "xrevrange", "xlen", "xpending", "xinfo|stream", "xinfo|groups", "xinfo|consumers",
    "geopos", "geodist", "geohash", "geosearch", "georadius_ro", "georadiusbymember_ro",
    "sort_ro", "scan",
];

/// Why a command isn't sent — `None`: it's a read.
pub fn refusal(words: &[&str]) -> Option<String> {
    let name = name(words);
    Some(match name.split('|').next().unwrap_or("") {
        "" => "no command".into(),
        // KEYS walks every key at once, blocking the server (VERIFIED `redis-cmd-keys`: `@dangerous`)
        "keys" => "KEYS blocks the server while it walks every key — SCAN instead".into(),
        // A script runs whatever it says; *_RO ones are kept from writing by the server, but run code all the same
        "eval" | "evalsha" | "eval_ro" | "evalsha_ro" | "fcall" | "fcall_ro" | "script" | "function" => {
            "a script may write whatever it says — not run as a read".into()
        }
        // The connection's database is the connection's (its URL's), not a statement's to change
        "select" | "swapdb" => "the database is the connection's — another connection for another one".into(),
        "subscribe" | "psubscribe" | "ssubscribe" | "monitor" => "it would take the connection over".into(),
        _ if READS.contains(&name.as_str()) => return None,
        _ => format!("{} isn't a read the plugin runs", name.to_ascii_uppercase()),
    })
}

/// A command's name as `READS` and `COMMAND INFO` say it: lowercase, `container|sub` for the container commands.
pub fn name(words: &[&str]) -> String {
    let first = words.first().map(|w| w.to_ascii_lowercase()).unwrap_or_default();
    match (first.as_str(), words.get(1)) {
        ("object" | "memory" | "xinfo", Some(sub)) => format!("{first}|{}", sub.to_ascii_lowercase()),
        _ => first,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A script runs whatever it says, so none counts as a read — EVAL and its kin are refused before anything is
    /// sent, whatever case they're written in; a read is let through.
    #[test]
    fn eval_counts_as_a_write() {
        for script in ["EVAL", "eval", "EvalSha", "EVAL_RO", "EVALSHA_RO", "FCALL", "FCALL_RO", "SCRIPT", "FUNCTION"] {
            let why = refusal(&[script, "return 1", "0"]);
            assert!(why.as_deref().is_some_and(|w| w.contains("script")), "{script}: {why:?}");
        }
        assert_eq!(refusal(&["GET", "k"]), None);
        assert_eq!(refusal(&["OBJECT", "ENCODING", "k"]), None, "a container's read subcommand");
        assert!(refusal(&["OBJECT", "HELP"]).is_some(), "not every subcommand");
        // (@read, but it writes its cache into the key — VERIFIED `redis-cmd-pfcount`)
        assert!(refusal(&["PFCOUNT", "hll"]).is_some());
    }
}
