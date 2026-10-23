#![no_main]
//! A web page's bytes through the reader, then every request its own language server takes at every byte —
//! `tarae_fuzz_parsers::facts`.
libfuzzer_sys::fuzz_target!(|b: &[u8]| tarae_fuzz_parsers::facts(b));
