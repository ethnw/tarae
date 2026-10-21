#![no_main]
//! A request to its server — `tarae_fuzz_parsers::origin`.
libfuzzer_sys::fuzz_target!(|b: &[u8]| tarae_fuzz_parsers::origin(b));
