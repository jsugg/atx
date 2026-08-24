//! Human (non-JSON) presentation coverage.
//!
//! The rest of the e2e suites speak `--json`; this one drives the real
//! binary in its default human mode across management, doctor, service,
//! and completion commands so the renderers and their error paths stay
//! honest alongside the machine envelope.

#![allow(clippy::expect_used, clippy::panic)]

use std::process::{Command, Output};
use std::time::Duration;

use serde_json::Value;
use tempfile::tempdir;

fn atx() -> Command {
    Command::new(env!("CARGO_BIN_EXE_atx"))
}

/// Client pointed at the not-yet-existing state subdirectory, so ATX
/// creates it with its required 0700 mode.
fn tools(root: &std::path::Path) -> Command {
    let mut command = atx();
    command.arg("--state-dir").arg(root.join("state"));
    command
}

fn json_output(command: &mut Command) -> (Value, Output) {
    let output = command.output().expect("run atx");
    let value = serde_json::from_slice(&output.stdout).expect("JSON envelope");
    (value, output)
}

fn wait_for(predicate: impl Fn() -> bool, what: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !predicate() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn lifecycle_commands_render_human_envelopes() {
    let root = tempdir().expect("root");
    let marker = root.path().join("marker");
    let script = format!("'/usr/bin/touch' '{}'", marker.display());
    let submitted = tools(root.path())
        .args(["1s", "--", "/bin/sh", "-c", script.as_str()])
        .output()
        .expect("submit");
    assert!(submitted.status.success(), "{submitted:?}");
    let stdout = String::from_utf8_lossy(&submitted.stdout);
    assert!(stdout.starts_with("Scheduled "), "{stdout}");
    let job_id = stdout.split_whitespace().nth(1).expect("job id").to_owned();

    // Poll through the JSON envelope so assertions below stay about the
    // human renderers rather than about timing.
    wait_for(|| marker.exists(), "the job to execute");
    wait_for(
        || {
            let (value, _) = json_output(tools(root.path()).args(["--json", "show", &job_id]));
            value["data"]["state"] == "succeeded"
        },
        "the job to reach succeeded",
    );

    let listed = tools(root.path()).args(["list"]).output().expect("list");
    let listing = String::from_utf8_lossy(&listed.stdout);
    assert!(
        listing.starts_with("JOB\tSTATE\tDUE\tREMAINING\tNAME"),
        "{listing}"
    );
    assert!(listing.contains(&job_id), "{listing}");

    let colored = tools(root.path())
        .args(["--color", "always", "list"])
        .output()
        .expect("colored list");
    assert!(
        String::from_utf8_lossy(&colored.stdout).contains('\x1b'),
        "explicit color mode must emit escapes"
    );

    let shown = tools(root.path())
        .args(["show", &job_id])
        .output()
        .expect("show");
    let detail = String::from_utf8_lossy(&shown.stdout);
    assert!(detail.starts_with(&format!("Job: {job_id}\n")), "{detail}");
    assert!(detail.contains("State: Succeeded"), "{detail}");

    let (history, _) = json_output(tools(root.path()).args(["--json", "history"]));
    let run_id = history["data"][0]["run_id"]
        .as_str()
        .expect("run id")
        .to_owned();
    let runs = tools(root.path())
        .args(["history"])
        .output()
        .expect("history");
    let run_lines = String::from_utf8_lossy(&runs.stdout);
    assert!(run_lines.starts_with("RUN\tJOB\tSTATE"), "{run_lines}");
    assert!(run_lines.contains(&run_id), "{run_lines}");

    let processes = tools(root.path()).args(["ps"]).output().expect("ps");
    let process_lines = String::from_utf8_lossy(&processes.stdout);
    assert!(process_lines.starts_with("JOB\tRUN\tROLE\tPID\tPGID\tSTATE"));

    let printed = tools(root.path())
        .args(["output", &run_id])
        .output()
        .expect("output");
    let printed_text = String::from_utf8_lossy(&printed.stdout);
    assert!(
        printed_text.starts_with(&format!("Run: {run_id}\n")),
        "{printed_text}"
    );
    assert!(printed_text.contains("--- stdout ---"), "{printed_text}");

    let removed = tools(root.path())
        .args(["rm", "--keep-history", &job_id])
        .output()
        .expect("rm");
    let removal = String::from_utf8_lossy(&removed.stdout);
    assert!(
        removal.starts_with(&format!("Job: {job_id}\n")),
        "{removal}"
    );
}

#[test]
fn doctor_service_version_and_completions_render() {
    let root = tempdir().expect("root");

    let doctor = tools(root.path()).arg("doctor").output().expect("doctor");
    assert!(
        doctor.status.success(),
        "{}",
        String::from_utf8_lossy(&doctor.stderr)
    );
    let report = String::from_utf8_lossy(&doctor.stdout);
    assert!(report.starts_with("ATX is "), "{report}");
    assert!(report.contains("[ok]"), "{report}");
    assert!(report.contains("tzdb:"), "{report}");

    let service = tools(root.path())
        .args(["service", "status"])
        .output()
        .expect("service status");
    let status = String::from_utf8_lossy(&service.stdout);
    assert!(status.starts_with("Manager: "), "{status}");
    assert!(status.contains("Guarantee: "), "{status}");

    let version = tools(root.path()).arg("version").output().expect("version");
    assert!(version.status.success());
    assert!(!version.stdout.is_empty());

    let completions = tools(root.path())
        .args(["completions", "--shell", "bash"])
        .output()
        .expect("completions");
    assert!(completions.status.success());
    assert!(!completions.stdout.is_empty());
}

#[test]
fn human_errors_report_exit_codes_on_stderr() {
    let root = tempdir().expect("root");

    // No database yet: management commands fail with not_found (3).
    let missing = tools(root.path())
        .arg("list")
        .output()
        .expect("list without state");
    assert_eq!(missing.status.code(), Some(3));
    let message = String::from_utf8_lossy(&missing.stderr);
    assert!(message.contains("no state database"), "{message}");

    // With a database present, an unknown prefix is still not_found.
    let submitted = tools(root.path())
        .args(["1h", "--", "true"])
        .output()
        .expect("submit");
    assert!(submitted.status.success(), "{submitted:?}");
    let unknown = tools(root.path())
        .args(["show", "nosuch"])
        .output()
        .expect("unknown job");
    assert_eq!(unknown.status.code(), Some(3));

    // Usage errors exit 2 with a human explanation (needs an open store,
    // which happens before argument validation).
    let usage = tools(root.path())
        .args(["list", "--state", "bogus"])
        .output()
        .expect("usage");
    assert_eq!(usage.status.code(), Some(2));
    assert!(!usage.stderr.is_empty());
}
