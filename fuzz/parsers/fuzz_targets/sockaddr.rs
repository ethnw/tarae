#![no_main]
//! A `struct sockaddr` as a call names it — `tarae_fuzz_parsers::sockaddr`.
libfuzzer_sys::fuzz_target!(|b: &[u8]| tarae_fuzz_parsers::sockaddr(b));
