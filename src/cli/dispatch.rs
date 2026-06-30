//! Top-level command dispatch.

use std::ffi::OsString;
use std::process::ExitCode;

pub(crate) fn run(args: impl IntoIterator<Item = OsString>) -> ExitCode {
    let mut args = args.into_iter();
    let _program = args.next();

    match args.next().as_deref() {
        Some(value) if value == "version" || value == "--version" || value == "-V" => {
            println!("atx {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("Usage: atx version");
            ExitCode::from(2)
        }
    }
}
