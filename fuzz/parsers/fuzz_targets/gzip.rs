#![no_main]
//! A gzip body, as the fetch unpacks it — `tarae_fuzz_parsers::gzip`.
libfuzzer_sys::fuzz_target!(|b: &[u8]| tarae_fuzz_parsers::gzip(b));
