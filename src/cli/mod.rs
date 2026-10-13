//! Command-line parsing, dispatch, and rendering.

mod args;
mod boundary;
mod dispatch;
mod exit;
mod human;
mod json;
mod view;

use std::ffi::OsString;
use std::process::ExitCode;

/// Run once at process entry, returning internal failure if dispatch unwinds.
pub(crate) fn run(args: impl IntoIterator<Item = OsString>) -> ExitCode {
    // Dispatch is not resumed after a panic, so partially changed state is never reused.
    boundary::run(std::panic::AssertUnwindSafe(|| dispatch::run(args)))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic)]

    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::process::{Command, ExitCode, Output};

    struct PanickingArgs {
        prefix: std::vec::IntoIter<OsString>,
        hostile_payload: bool,
        cleanup_path: Option<PathBuf>,
    }

    struct HostilePayload;

    impl Drop for HostilePayload {
        fn drop(&mut self) {
            panic!("private payload destructor detail");
        }
    }

    impl Drop for PanickingArgs {
        fn drop(&mut self) {
            if let Some(path) = &self.cleanup_path {
                let _ = std::fs::write(path, b"unwound");
            }
        }
    }

    impl Iterator for PanickingArgs {
        type Item = OsString;

        fn next(&mut self) -> Option<Self::Item> {
            if let Some(argument) = self.prefix.next() {
                return Some(argument);
            }
            if self.hostile_payload {
                std::panic::panic_any(HostilePayload);
            }
            panic!("private dispatch panic detail");
        }
    }

    fn probe_command(mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().expect("test executable"));
        command
            .args(["--exact", "cli::tests::panic_probe", "--nocapture"])
            .env("ATX_TEST_CLI_PANIC_PROBE", mode)
            .env_remove("ATX_TEST_CLI_PANIC_CLEANUP")
            .env("RUST_BACKTRACE", "full");
        command
    }

    fn assert_internal_failure(output: &Output) {
        assert_eq!(output.status.code(), Some(70));
        assert_eq!(output.stderr, b"atx: unexpected internal failure\n");
    }

    #[test]
    fn panic_probe() {
        let Ok(mode) = std::env::var("ATX_TEST_CLI_PANIC_PROBE") else {
            return;
        };
        let code = match mode.as_str() {
            "panic" | "payload" | "formatted_panic" => super::run(PanickingArgs {
                prefix: if mode == "formatted_panic" {
                    vec!["atx", "--json", "--verbose"]
                } else {
                    vec!["atx"]
                }
                .into_iter()
                .map(OsString::from)
                .collect::<Vec<_>>()
                .into_iter(),
                hostile_payload: mode == "payload",
                cleanup_path: std::env::var_os("ATX_TEST_CLI_PANIC_CLEANUP").map(PathBuf::from),
            }),
            "help" => super::run(["atx", "--help"].map(OsString::from)),
            "version" => super::run(["atx", "--version"].map(OsString::from)),
            "usage" => super::run(["atx", "--definitely-not-a-flag"].map(OsString::from)),
            "json_usage" => {
                super::run(["atx", "--json", "--definitely-not-a-flag"].map(OsString::from))
            }
            "typed_internal" => super::boundary::run(super::exit::internal),
            "worker_hook" => {
                std::panic::set_hook(Box::new(|_| eprintln!("previous worker hook")));
                super::boundary::run(|| {
                    assert!(
                        std::thread::spawn(|| panic!("worker detail"))
                            .join()
                            .is_err()
                    );
                    super::exit::usage()
                })
            }
            _ => panic!("unknown probe mode"),
        };
        let status = if code == ExitCode::SUCCESS {
            0
        } else if code == super::exit::usage() {
            2
        } else if code == super::exit::internal() {
            70
        } else {
            panic!("unexpected probe exit code");
        };
        std::process::exit(status);
    }

    #[test]
    fn dispatch_panic_exits_70_without_payload_paths_or_backtrace() {
        assert_internal_failure(&probe_command("panic").output().expect("panic probe"));
    }

    #[test]
    fn panic_unwinds_dispatch_resources_before_returning() {
        let root = tempfile::tempdir().expect("temporary directory");
        let marker = root.path().join("cleanup");
        let output = probe_command("panic")
            .env("ATX_TEST_CLI_PANIC_CLEANUP", &marker)
            .output()
            .expect("cleanup probe");
        assert_internal_failure(&output);
        assert_eq!(std::fs::read(marker).expect("cleanup marker"), b"unwound");
    }

    #[test]
    fn hostile_panic_payload_cannot_replace_the_internal_exit() {
        assert_internal_failure(&probe_command("payload").output().expect("payload probe"));
    }

    #[test]
    fn json_and_verbose_requests_do_not_expose_panic_details() {
        assert_internal_failure(
            &probe_command("formatted_panic")
                .output()
                .expect("formatted panic probe"),
        );
    }

    #[test]
    fn ordinary_help_version_and_typed_errors_keep_their_output_and_exit() {
        let help = probe_command("help").output().expect("help probe");
        assert_eq!(help.status.code(), Some(0));
        assert!(String::from_utf8_lossy(&help.stdout).contains("Usage:"));
        assert!(help.stderr.is_empty());
        let version = probe_command("version").output().expect("version probe");
        assert_eq!(version.status.code(), Some(0));
        assert!(String::from_utf8_lossy(&version.stdout).contains(env!("CARGO_PKG_VERSION")));
        assert!(version.stderr.is_empty());
        let usage = probe_command("usage").output().expect("usage probe");
        assert_eq!(usage.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&usage.stderr).contains("unexpected argument"));
        let json = probe_command("json_usage")
            .output()
            .expect("JSON usage probe");
        assert_eq!(json.status.code(), Some(2));
        let error: serde_json::Value = serde_json::from_slice(&json.stderr).expect("error JSON");
        assert_eq!(error["error"]["code"], "INVALID_ARGUMENT");
        let internal = probe_command("typed_internal")
            .output()
            .expect("typed internal probe");
        assert_eq!(internal.status.code(), Some(70));
        assert!(internal.stderr.is_empty());
    }

    #[test]
    fn panic_hook_keeps_other_threads_previous_diagnostics() {
        let output = probe_command("worker_hook")
            .output()
            .expect("worker hook probe");
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(output.stderr, b"previous worker hook\n");
    }
}
