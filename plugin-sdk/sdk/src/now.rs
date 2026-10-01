//! The time and randomness tarae hands every call: kept here for whatever asks later — `millis()`, and
//! `getrandom` (a ChaCha20 generator, stirred with each call's fresh bytes).

use std::sync::Mutex;

use rand_chacha::ChaCha20Rng;
use rand_core::{RngCore, SeedableRng};

static STATE: Mutex<Option<(u64, ChaCha20Rng)>> = Mutex::new(None);

/// Hands over a call's `now`: its time (ms since 1970) and its 32 random bytes.
pub fn set(millis: u64, entropy: &[u8]) {
    let mut s = STATE.lock().unwrap_or_else(|e| e.into_inner());
    // Stirred, not replaced: what came before still counts
    let mut seed = [0u8; 32];
    if let Some((_, rng)) = s.as_mut() {
        rng.fill_bytes(&mut seed);
    }
    for (i, b) in entropy.iter().enumerate() {
        seed[i % 32] ^= b;
    }
    *s = Some((millis, ChaCha20Rng::from_seed(seed)));
}

/// The time of the latest call, in ms since 1970 (0 before any).
pub fn millis() -> u64 {
    STATE.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map_or(0, |(m, _)| *m)
}

/// Random bytes (an error before the first `set`: there's nothing to draw from yet).
pub fn fill(buf: &mut [u8]) -> Result<(), getrandom::Error> {
    let mut s = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let (_, rng) = s.as_mut().ok_or(getrandom::Error::UNSUPPORTED)?;
    rng.fill_bytes(buf);
    Ok(())
}

// Where the target has no random source (wasm32-unknown-unknown), `getrandom` asks this
getrandom::register_custom_getrandom!(fill);

#[cfg(test)]
mod tests {
    #[test]
    fn stirred_by_each_call() {
        super::set(42, &[1; 32]);
        assert_eq!(super::millis(), 42);
        let (mut a, mut b) = ([0u8; 16], [0u8; 16]);
        super::fill(&mut a).unwrap();
        super::fill(&mut b).unwrap();
        assert_ne!(a, b);
        super::set(43, &[1; 32]);
        let mut c = [0u8; 16];
        super::fill(&mut c).unwrap();
        assert_ne!(a, c, "the same bytes again don't start it over");
    }
}
