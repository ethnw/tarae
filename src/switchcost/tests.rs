//! What a switch costs — the table read as the tools pack says it, and the cost of a conversation's cache by its
//! state. No claude here: a `Usage` fed the request a stream would say.

use super::*;
use crate::usage::{Counts, Seen, Ttl};

/// A conversation whose last request carried `tokens` on `model`, its cache written `ago_ms` before `now`
/// (an hour-long write).
pub(crate) fn conversation(model: &str, tokens: u64, now: u64, ago_ms: u64) -> Usage {
    let mut u = Usage::default();
    u.resumed("conv-1");
    u.requesting(now - ago_ms);
    let counts = Counts { input: Some(10), write: Some(tokens - 10), read: Some(0), output: Some(5) };
    u.seen(Seen {
        id: Some("msg-1".into()),
        model: model.into(),
        counts,
        ttl: Some(Ttl::Hour1),
        at_ms: now - ago_ms,
    });
    u
}

const NOW: u64 = 1_800_000_000_000;

/// A warm cache switched onto the key costs its whole context at the hour-long write (an API chat's cache lives
/// an hour): 150k tokens of Opus 5.5 at $8/MTok is $1.20 — never the 5-minute write's $0.75. On the plan it's
/// tokens, toward its limits; a model the table lacks, tokens too. Cold, or nothing cached: no extra cost.
#[test]
fn a_warm_cache_onto_the_key_costs_its_context_at_the_hour_long_write() {
    let prices = &crate::sandboxpack::get().price;
    let warm = conversation("claude-opus-5-5", 150_000, NOW, 60_000);
    let until = NOW - 60_000 + Ttl::Hour1.ms();
    let c = cost(&warm, NOW, Billing::Api, "claude-opus-5-5", prices);
    assert_eq!(c, Cost::Warm { tokens: 150_000, dollars: Some(1.2), until_ms: until });
    assert_eq!(c.said(Billing::Api), "rewrites ≈ $1.20 of cached context (150k tokens)");
    let plan = cost(&warm, NOW, Billing::Subscription, "claude-opus-5-5", prices);
    assert_eq!(
        plan,
        Cost::Warm { tokens: 150_000, dollars: None, until_ms: until },
        "the plan: never dollars"
    );
    assert_eq!(
        plan.said(Billing::Subscription),
        "rewrites ≈ 150k tokens of cached context, toward your plan's limits"
    );
    let unknown = cost(&warm, NOW, Billing::Api, "claude-someday-9", prices);
    assert!(unknown.said(Billing::Api).contains("150k tokens of cached context (no price for this model)"));
    // Cold since its TTL ran out: staying writes it again too
    let cold = conversation("claude-opus-5-5", 150_000, NOW, 2 * Ttl::Hour1.ms());
    let gone = NOW - 2 * Ttl::Hour1.ms() + Ttl::Hour1.ms();
    assert_eq!(cost(&cold, NOW, Billing::Api, "claude-opus-5-5", prices), Cost::Cold { since_ms: gone });
    assert!(
        Cost::Cold { since_ms: gone }
            .said(Billing::Api)
            .starts_with("no extra cost — the cache went cold at ")
    );
    assert_eq!(cost(&Usage::default(), NOW, Billing::Api, "claude-opus-5-5", prices), Cost::Nothing);
}

/// A model's row is found by its id whole — a date or a `[1m]` suffix aside — never by a prefix: `claude-opus-5`
/// isn't `claude-opus-5-5`'s row (that would price Opus 5.5 as Opus 5). Haiku 5.5's two tiers by the context.
#[test]
fn a_price_is_a_models_whole_id_never_a_prefix() {
    let prices = &crate::sandboxpack::get().price;
    let write = |id: &str, tokens| price_of(prices, id, tokens).map(|p| p.write_1h);
    assert_eq!(write("claude-opus-5-5", 1), Some(8.0));
    assert_eq!(write("claude-opus-5", 1), Some(10.0));
    assert_eq!(write("claude-opus-5-5[1m]", 1), Some(8.0), "the long-context suffix");
    assert_eq!(write("claude-haiku-4-5-20251001", 1), Some(2.0), "a dated id");
    assert_eq!(write("claude-opus-5-55", 1), None);
    assert_eq!(write("claude-haiku-5-5", 100_000), Some(0.2), "up to 100k: the lower tier");
    assert_eq!(write("claude-haiku-5-5", 100_001), Some(1.0), "over it: the higher");
    assert_eq!(write("opus", 1), None, "an alias isn't priced — tokens only");
}

/// The tools pack's table reads clean, every row with both writes, and what effort does to the cache as the
/// sources say: Fable 5.1 keeps it (Claude Code says so), Opus 5.5 unchecked (L7), Haiku 4.5 rewrites. A row
/// that's wrong is refused, saying which.
#[test]
fn the_price_table_reads_clean_and_a_wrong_row_is_refused() {
    let t = crate::sandboxpack::get();
    assert!(!t.problems.iter().any(|p| p.contains("price")), "{:?}", t.problems);
    let effort = |id: &str| price_of(&t.price, id, 1).map(|p| p.effort);
    assert_eq!(effort("claude-fable-5-1"), Some(Effort::Keeps));
    assert_eq!(effort("claude-opus-5-5"), Some(Effort::Unknown));
    assert_eq!(effort("claude-haiku-4-5"), Some(Effort::Rewrites));
    let rows = |s: &str| read(&s.parse::<toml::Table>().unwrap()["price"]);
    assert!(rows("[[price]]\nmodel = \"m\"\nwrite-5m = 1\n").unwrap_err().contains("no `write-1h`"));
    assert!(
        rows("[[price]]\nmodel = \"m\"\nwrite-5m = 1\nwrite-1h = -2\n")
            .unwrap_err()
            .contains("isn't a price")
    );
    assert!(
        rows("[[price]]\nmodel = \"m\"\nwrite-5m = 1\nwrite-1h = 2\ncolour = 1\n")
            .unwrap_err()
            .contains("no key `colour`")
    );
    assert!(rows("[[price]]\nmodel = \"m\"\nwrite-5m = 1\nwrite-1h = 2\neffort = \"maybe\"\n").is_err());
    assert_eq!(rows("[[price]]\nmodel = \"m\"\nwrite-5m = 1\nwrite-1h = 2.5\n").unwrap()[0].write_1h, 2.5);
}
