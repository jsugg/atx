#![no_main]

use libfuzzer_sys::fuzz_target;

// Minimal stand-in for the crate-root domain module the frame codec imports;
// the codec only needs validated identifiers and revisions.
#[path = "domain_shim.rs"]
#[allow(dead_code)]
pub(crate) mod domain;

#[path = "../../src/supervisor/frame.rs"]
#[allow(dead_code)]
mod frame;

use std::io::Cursor;

// Control-channel frames come over a socket from any local process; decoding
// arbitrary bytes must never panic and only well-formed messages parse.
fuzz_target!(|input: &[u8]| {
    let mut reader = Cursor::new(input.to_vec());
    let _ = frame::read_frame(&mut reader);
});
