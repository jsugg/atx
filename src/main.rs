#![forbid(unsafe_op_in_unsafe_fn)]

mod application;
mod cli;
mod domain;
mod infrastructure;
mod run_monitor;
mod supervisor;

use std::process::ExitCode;

fn main() -> ExitCode {
    cli::run(std::env::args_os())
}
