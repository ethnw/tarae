#![no_main]
//! A request's head through the proxy — `tarae_fuzz_parsers::head`.
libfuzzer_sys::fuzz_target!(|b: &[u8]| tarae_fuzz_parsers::head(b));
