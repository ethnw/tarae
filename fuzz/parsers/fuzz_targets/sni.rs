#![no_main]
//! A tunnel's first bytes, a ClientHello in TLS records — `tarae_fuzz_parsers::sni`.
libfuzzer_sys::fuzz_target!(|b: &[u8]| tarae_fuzz_parsers::sni(b));
