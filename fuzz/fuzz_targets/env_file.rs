#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../../src/domain/execution.rs"]
#[allow(dead_code)]
mod execution;

use execution::Environment;

// Environment files are user-supplied KEY=VALUE text; the validation layer
// behind the file parser must never panic on arbitrary keys or values.
fuzz_target!(|input: &str| {
    for line in input.lines() {
        if let Some((key, value)) = line.split_once('=') {
            let _ = Environment::from_pairs([(key, value)]);
        }
    }
});
