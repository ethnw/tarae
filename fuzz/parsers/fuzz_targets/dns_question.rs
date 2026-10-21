#![no_main]
//! A run's DNS question — `tarae_fuzz_parsers::dns_question`.
libfuzzer_sys::fuzz_target!(|b: &[u8]| tarae_fuzz_parsers::dns_question(b));
