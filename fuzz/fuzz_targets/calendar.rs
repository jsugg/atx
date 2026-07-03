#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../../src/domain/calendar_syntax.rs"]
#[allow(dead_code)]
mod calendar_syntax;

fuzz_target!(|input: &str| {
    let _ = calendar_syntax::parse_calendar(input);
});
