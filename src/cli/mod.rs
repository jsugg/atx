//! Command-line parsing, dispatch, and rendering.

mod args;
mod dispatch;
mod exit;
mod human;
mod json;
mod view;

use std::ffi::OsString;
use std::process::ExitCode;

pub(crate) fn run(args: impl IntoIterator<Item = OsString>) -> ExitCode {
    dispatch::run(args)
}
