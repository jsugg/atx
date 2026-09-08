//! Color policy against real child stdout and stderr terminals.

// Cleanup failures in the shared test guards must fail this test, never pass silently.
#![allow(clippy::expect_used, clippy::panic)]

use std::fs::{self, File};
use std::io::IsTerminal;
use std::io::Read;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

#[path = "../src/run_monitor/pty_fixture.rs"]
mod pty_fixture;
mod support;

use pty_fixture::Pty;
use support::{tempdir, wait_until};

fn bounded_file(path: &Path) -> Vec<u8> {
    let mut bytes = Vec::new();
    File::open(path)
        .expect("captured stream")
        .take(65_537)
        .read_to_end(&mut bytes)
        .expect("stream read");
    assert!(bytes.len() <= 65_536, "stream exceeds color-test bound");
    bytes
}

fn drain_terminal(pty: Option<&Pty>, bytes: &mut Vec<u8>) -> Result<(), String> {
    if let Some(pty) = pty {
        let chunk = pty.read_available().map_err(|error| error.to_string())?;
        if bytes.len() + chunk.len() > 65_536 {
            return Err("terminal output exceeds color-test bound".to_owned());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(())
}

struct ColorCase<'a> {
    stdout_tty: bool,
    stderr_tty: bool,
    flag: Option<&'a str>,
    no_color: Option<&'a str>,
    term: &'a str,
    command: ColorCommand,
    config_color: Option<&'a str>,
    colored: bool,
}

#[derive(Clone, Copy)]
enum ColorCommand {
    DryRun,
    ParserError,
    Help,
    Version,
    JsonDryRun,
    QuietDryRun,
    JsonParserError,
}

#[test]
fn owned_pty_writer_remains_a_terminal_with_owner_only_permissions() {
    let pty = Pty::open().expect("owned terminal");
    assert!(
        fs::metadata(pty.path())
            .expect("terminal metadata")
            .file_type()
            .is_char_device()
    );
    pty.set_mode(0o600).expect("owner-only terminal");
    assert_eq!(
        fs::metadata(pty.path())
            .expect("terminal mode")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(pty.writer().expect("cloned terminal writer").is_terminal());
}

fn color_command(case: &ColorCase<'_>, root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_atx"));
    command
        .env_clear()
        .env("HOME", root)
        .env("PATH", "/usr/bin:/bin")
        .env("TERM", case.term);
    if let Some(value) = case.no_color {
        command.env("NO_COLOR", value);
    }
    if let Some(color) = case.config_color {
        command.env("ATX_COLOR", color);
    }
    if matches!(
        case.command,
        ColorCommand::JsonDryRun | ColorCommand::JsonParserError
    ) {
        command.arg("--json");
    }
    if matches!(case.command, ColorCommand::QuietDryRun) {
        command.arg("--quiet");
    }
    command.arg("--state-dir").arg(root.join("state"));
    if let Some(flag) = case.flag {
        command.args(["--color", flag]);
    }
    match case.command {
        ColorCommand::ParserError | ColorCommand::JsonParserError => {
            command.arg("--unknown-color-fixture");
        }
        ColorCommand::Help => {
            command.arg("--help");
        }
        ColorCommand::Version => {
            command.arg("--version");
        }
        _ => {
            command.args(["--dry-run", "1s", "--", "/bin/true"]);
        }
    }
    command
}

fn check_case(case: &ColorCase<'_>) {
    let parser_error = matches!(
        case.command,
        ColorCommand::ParserError | ColorCommand::JsonParserError
    );
    let json = matches!(
        case.command,
        ColorCommand::JsonDryRun | ColorCommand::JsonParserError
    );
    let quiet = matches!(case.command, ColorCommand::QuietDryRun);
    let root = tempdir().expect("root");
    let stdout = root.path().join("stdout");
    let stderr = root.path().join("stderr");
    let out_pty = case.stdout_tty.then(|| Pty::open().expect("stdout PTY"));
    let err_pty = case.stderr_tty.then(|| Pty::open().expect("stderr PTY"));
    let mut command = color_command(case, root.path());
    let out = out_pty.as_ref().map_or_else(
        || File::create(&stdout).expect("stdout file"),
        |pty| pty.writer().expect("stdout slave"),
    );
    let err = err_pty.as_ref().map_or_else(
        || File::create(&stderr).expect("stderr file"),
        |pty| pty.writer().expect("stderr slave"),
    );
    command.stdout(Stdio::from(out)).stderr(Stdio::from(err));
    let mut child = root.spawn_owned(&mut command).expect("color child");
    let mut status = None;
    let (mut out, mut err) = (Vec::new(), Vec::new());
    assert!(
        wait_until(Duration::from_secs(5), || {
            drain_terminal(out_pty.as_ref(), &mut out)?;
            drain_terminal(err_pty.as_ref(), &mut err)?;
            status = child.try_wait().map_err(|error| error.to_string())?;
            Ok(status.is_some())
        })
        .expect("bounded child wait"),
        "color child did not exit"
    );
    assert_eq!(
        status.expect("exit").code(),
        Some(if parser_error { 2 } else { 0 })
    );
    drain_terminal(out_pty.as_ref(), &mut out).expect("final stdout drain");
    drain_terminal(err_pty.as_ref(), &mut err).expect("final stderr drain");
    if out_pty.is_none() {
        out = bounded_file(&stdout);
    }
    if err_pty.is_none() {
        err = bounded_file(&stderr);
    }
    let (output, other, expected) = match case.command {
        ColorCommand::ParserError | ColorCommand::JsonParserError => (&err, &out, "error:"),
        ColorCommand::Help => (&out, &err, "Usage:"),
        ColorCommand::Version => (&out, &err, concat!("atx ", env!("CARGO_PKG_VERSION"))),
        _ => (&out, &err, "Dry run"),
    };
    if quiet {
        assert!(out.is_empty() && err.is_empty(), "quiet output leaked");
    } else if json {
        let value: serde_json::Value = serde_json::from_slice(output).expect("plain JSON stream");
        if parser_error {
            assert_eq!(value["error"]["code"], "INVALID_ARGUMENT");
        } else {
            assert!(value.get("data").is_some(), "{value}");
        }
    } else {
        assert!(
            String::from_utf8_lossy(output).contains(expected),
            "missing real output: {output:?}"
        );
    }
    assert!(other.is_empty(), "wrong stream received output: {other:?}");
    assert_eq!(
        output.windows(2).any(|bytes| bytes == b"\x1b["),
        case.colored,
        "stdoutTTY={} stderrTTY={} flag={:?} NO_COLOR={:?} TERM={} parser={}",
        case.stdout_tty,
        case.stderr_tty,
        case.flag,
        case.no_color,
        case.term,
        parser_error
    );
}

#[test]
fn automatic_color_obeys_environment_and_each_output_stream() {
    for command in [
        ColorCommand::DryRun,
        ColorCommand::ParserError,
        ColorCommand::Help,
    ] {
        let parser_error = matches!(command, ColorCommand::ParserError);
        for stdout_tty in [false, true] {
            for stderr_tty in [false, true] {
                for (no_color, term, disabled) in [
                    (None, "xterm-256color", false),
                    (Some("1"), "xterm-256color", true),
                    (Some(""), "xterm-256color", true),
                    (None, "dumb", true),
                ] {
                    check_case(&ColorCase {
                        stdout_tty,
                        stderr_tty,
                        flag: None,
                        no_color,
                        term,
                        command,
                        config_color: None,
                        colored: !disabled && if parser_error { stderr_tty } else { stdout_tty },
                    });
                }
            }
        }
    }
}

#[test]
fn explicit_color_modes_override_environment_without_changing_streams() {
    for command in [
        ColorCommand::DryRun,
        ColorCommand::ParserError,
        ColorCommand::Help,
    ] {
        for tty in [false, true] {
            for flag in ["always", "never", "auto"] {
                check_case(&ColorCase {
                    stdout_tty: tty,
                    stderr_tty: tty,
                    flag: Some(flag),
                    no_color: Some("1"),
                    term: "dumb",
                    command,
                    config_color: None,
                    colored: flag == "always" || (flag == "auto" && tty),
                });
            }
        }
    }
}

#[test]
fn version_stays_on_stdout_without_adding_styles() {
    for stdout_tty in [false, true] {
        for stderr_tty in [false, true] {
            check_case(&ColorCase {
                stdout_tty,
                stderr_tty,
                flag: Some("always"),
                no_color: Some("1"),
                term: "dumb",
                command: ColorCommand::Version,
                config_color: None,
                colored: false,
            });
        }
    }
}

#[test]
fn config_defaults_json_and_quiet_keep_the_same_policy() {
    for (configured, flag, disabled, colored) in [
        ("always", None, false, true),
        ("always", None, true, false),
        ("never", Some("always"), true, true),
        ("always", Some("never"), false, false),
    ] {
        check_case(&ColorCase {
            stdout_tty: true,
            stderr_tty: false,
            flag,
            no_color: disabled.then_some("1"),
            term: "xterm-256color",
            command: ColorCommand::DryRun,
            config_color: Some(configured),
            colored,
        });
    }
    for command in [
        ColorCommand::JsonDryRun,
        ColorCommand::QuietDryRun,
        ColorCommand::JsonParserError,
    ] {
        check_case(&ColorCase {
            stdout_tty: true,
            stderr_tty: true,
            flag: Some("always"),
            no_color: None,
            term: "xterm-256color",
            command,
            config_color: Some("always"),
            colored: false,
        });
    }
}
