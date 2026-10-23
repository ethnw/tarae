#![no_main]
//! A web page's bytes through the reader — `tarae_fuzz_parsers::web`.
libfuzzer_sys::fuzz_target!(|b: &[u8]| tarae_fuzz_parsers::web(b));
