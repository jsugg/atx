#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../../src/domain/execution.rs"]
#[allow(dead_code)]
mod execution;

use execution::ExecutionSpec;

// SQLite stores the execution spec as JSON; decoding attacker-controlled or
// corrupt rows must never panic and never accept shell-interpretable input
// in direct mode.
fuzz_target!(|input: &str| {
    if let Ok(execution) = ExecutionSpec::from_persistence_json(input) {
        assert_eq!(execution.mode(), execution::ExecutionMode::Direct);
        assert!(!execution.argv().is_empty());
    }
});
