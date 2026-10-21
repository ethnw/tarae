#![no_main]
//! A resolver's response to tarae's own question — `tarae_fuzz_parsers::dns_response`.
libfuzzer_sys::fuzz_target!(|b: &[u8]| tarae_fuzz_parsers::dns_response(b));
