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

/// getrandom 0.3 and 0.4 ask a function of this name (built with `--cfg getrandom_backend="custom"`) — one for
/// both. A plugin mustn't define it too.
#[cfg(feature = "getrandom04")]
#[unsafe(no_mangle)]
unsafe extern "Rust" fn __getrandom_v03_custom(dest: *mut u8, len: usize) -> Result<(), getrandom04::Error> {
    // SAFETY: getrandom hands `len` writable bytes at `dest` — maybe uninitialized (it fills `MaybeUninit`s), so
    // they're zeroed before a `&mut [u8]` is made of them.
    let buf = unsafe {
        std::ptr::write_bytes(dest, 0, len);
        std::slice::from_raw_parts_mut(dest, len)
    };
    fill(buf).map_err(|_| getrandom04::Error::UNSUPPORTED)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    /// The generator is the process's: these tests take turns with it.
    static TURN: Mutex<()> = Mutex::new(());

    /// As before any call: nothing to draw from.
    fn reset() {
        *super::STATE.lock().unwrap() = None;
    }

    /// The generator's next `n` bytes after a fresh `set(millis, entropy)`.
    fn generators(entropy: &[u8], n: usize) -> Vec<u8> {
        reset();
        super::set(1, entropy);
        let mut b = vec![0; n];
        super::fill(&mut b).unwrap();
        b
    }

    /// getrandom 0.3/0.4's hook (what rand 0.9+ and postgres-protocol's SCRAM draw through) gives the generator's
    /// own bytes — fed by each call's entropy, not some other source — two draws differ, and before the first
    /// `set` it's an error, not zeros.
    #[cfg(feature = "getrandom04")]
    #[test]
    fn getrandom_04s_hook_draws_from_the_generator() {
        let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
        let hook = |n: usize| {
            let mut b = vec![0xAA; n];
            // SAFETY: `b` holds `n` bytes.
            unsafe { super::__getrandom_v03_custom(b.as_mut_ptr(), n) }.map(|()| b)
        };
        reset();
        assert!(hook(16).is_err(), "nothing to draw from before the first call");
        let want = generators(&[3; 32], 64);
        reset();
        super::set(1, &[3; 32]);
        let (a, b) = (hook(32).unwrap(), hook(32).unwrap());
        assert_ne!(a, b);
        assert_eq!([a, b].concat(), want, "the generator's bytes, in order");
    }

    /// The same through getrandom 0.4 itself, as a plugin's build has it (`--cfg getrandom_backend="custom"`,
    /// round4.sh runs it so): its `fill` reaches ours.
    #[cfg(all(feature = "getrandom04", getrandom_backend = "custom"))]
    #[test]
    fn getrandom_04_itself_reaches_the_generator() {
        let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
        reset();
        assert!(getrandom04::fill(&mut [0u8; 8]).is_err(), "before the first call");
        let want = generators(&[5; 32], 32);
        reset();
        super::set(1, &[5; 32]);
        let mut got = [0u8; 32];
        getrandom04::fill(&mut got).unwrap();
        assert_eq!(got.to_vec(), want);
    }

    #[test]
    fn stirred_by_each_call() {
        let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
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
