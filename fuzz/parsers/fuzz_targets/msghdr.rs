#![no_main]
//! A run's memory holding a `struct msghdr` and what it points to — `tarae_fuzz_parsers::msghdr`.
libfuzzer_sys::fuzz_target!(|b: &[u8]| tarae_fuzz_parsers::msghdr(b));
