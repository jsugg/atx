#![no_main]

use std::str::FromStr;

use libfuzzer_sys::fuzz_target;

#[path = "../../src/domain/duration.rs"]
#[allow(dead_code)]
mod duration;

fuzz_target!(|input: &str| {
    let _ = duration::DurationSeconds::from_str(input);
});
