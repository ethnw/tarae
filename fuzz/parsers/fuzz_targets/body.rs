#![no_main]
//! A chunked body, a few bytes at a time — `tarae_fuzz_parsers::body`.
libfuzzer_sys::fuzz_target!(|b: &[u8]| tarae_fuzz_parsers::body(b));
