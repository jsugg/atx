//! ATX schedules commands for later without keeping a terminal open.
//!
//! One-shot relative (`30s`) and calendar (`15:00`, `2026-09-01 09:00`)
//! scheduling, fixed-rate recurrence (`--every 5m`), direct argv or
//! explicit `--shell` execution, durable `SQLite` state with crash recovery,
//! and optional launchd/systemd service ownership. Session-mode jobs
//! survive the submitter's terminal closing; durable jobs survive logout.
//!
//! This is a command-line tool: install it and read the guide at
//! <https://github.com/jsugg/atx/blob/main/docs/cli.md> (also rendered by
//! `atx --help` after long help). The library modules below are private
//! implementation details.

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
