#![no_main]
//! An rtnetlink batch a run sends — `tarae_fuzz_parsers::netlink`.
libfuzzer_sys::fuzz_target!(|b: &[u8]| tarae_fuzz_parsers::netlink(b));
