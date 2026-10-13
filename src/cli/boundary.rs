//! Process-entry panic containment without exposing dispatch internals.

use std::io::{self, Write};
use std::panic::{self, UnwindSafe};
use std::process::ExitCode;

/// Run once at process entry; return internal failure instead of resuming after an unwind.
pub(super) fn run(dispatch: impl FnOnce() -> ExitCode + UnwindSafe) -> ExitCode {
    let entry_thread = std::thread::current().id();
    let previous_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        if std::thread::current().id() != entry_thread {
            previous_hook(info);
        }
    }));
    match panic::catch_unwind(dispatch) {
        Ok(code) => code,
        Err(payload) => {
            // The process is exiting; a payload destructor must not trigger another panic.
            std::mem::forget(payload);
            let _ = writeln!(io::stderr().lock(), "atx: unexpected internal failure");
            super::exit::internal()
        }
    }
}
