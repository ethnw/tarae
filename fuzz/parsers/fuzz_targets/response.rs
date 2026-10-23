#![no_main]
//! A server's response, its body a few bytes at a time — `tarae_fuzz_parsers::response`.
libfuzzer_sys::fuzz_target!(|b: &[u8]| tarae_fuzz_parsers::response(b));
