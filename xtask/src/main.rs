//! Repository maintenance tasks. Run with `cargo xtask <task>`.

mod sbom;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let result = match args.as_slice() {
        [task] if task == "dist-man" => dist_man().map(|path| path.display().to_string()),
        [task, path, target] if task == "check-sbom" => match target.to_str() {
            Some(target) => sbom::check(Path::new(path), target)
                .map(|count| format!("SBOM: {count} locked runtime components verified")),
            None => Err("release target must be UTF-8".into()),
        },
        _ => {
            eprintln!("usage: cargo xtask dist-man | check-sbom FILE TARGET");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("xtask: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Render `atx.1` (plus per-subcommand pages) from clap metadata into `dist/man/`.
fn dist_man() -> Result<PathBuf, String> {
    let root = workspace_root()?;
    let out_dir = root.join("dist").join("man");
    std::fs::create_dir_all(&out_dir)
        .map_err(|error| format!("create {}: {error}", out_dir.display()))?;

    // The atx crate is bin-only, so xtask cannot link its clap tree directly.
    // Instead it rebuilds the binary with the `man` feature, which unlocks a
    // hidden `__man <OUT_DIR>` export rendered by clap_mangen. This keeps
    // shipped binaries free of any roff-generation code.
    let build = std::process::Command::new("cargo")
        .args([
            "build",
            "--locked",
            "--bin",
            "atx",
            "--features",
            "man",
            "--message-format=json-render-diagnostics",
        ])
        .current_dir(&root)
        .output()
        .map_err(|error| format!("cargo build: {error}"))?;
    std::io::stderr()
        .write_all(&build.stderr)
        .map_err(|error| format!("write build diagnostics: {error}"))?;
    if !build.status.success() {
        return Err(format!(
            "cargo build --features man failed: {}",
            build.status
        ));
    }

    let atx = artifact_executable(&build.stdout)?;
    let output = std::process::Command::new(&atx)
        .arg("__man")
        .arg(&out_dir)
        .output()
        .map_err(|error| format!("run {}: {error}", atx.display()))?;
    if !output.status.success() {
        return Err(format!(
            "atx __man failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(out_dir.join("atx.1"))
}

fn workspace_root() -> Result<PathBuf, String> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(std::path::Path::to_path_buf)
        .ok_or_else(|| "xtask has no parent directory".to_owned())
}

/// Find the exact executable Cargo built, independent of target-directory settings.
fn artifact_executable(output: &[u8]) -> Result<PathBuf, String> {
    let text = std::str::from_utf8(output)
        .map_err(|error| format!("Cargo output is not UTF-8: {error}"))?;
    let mut executable = None;
    for line in text.lines().filter(|line| !line.is_empty()) {
        let message: serde_json::Value = serde_json::from_str(line)
            .map_err(|error| format!("invalid Cargo build message: {error}"))?;
        let reason = message
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .ok_or("Cargo build message has no reason")?;
        if reason != "compiler-artifact" {
            continue;
        }
        let target = message
            .get("target")
            .and_then(serde_json::Value::as_object)
            .ok_or("Cargo artifact has no target")?;
        let name = target
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or("Cargo artifact has no target name")?;
        if name != "atx" {
            continue;
        }
        let kinds = target
            .get("kind")
            .and_then(serde_json::Value::as_array)
            .ok_or("Cargo artifact has no target kinds")?;
        if kinds.iter().any(|kind| !kind.is_string()) {
            return Err("Cargo target kinds must be strings".into());
        }
        if !kinds.iter().any(|kind| kind.as_str() == Some("bin")) {
            continue;
        }
        let value = message
            .get("executable")
            .and_then(serde_json::Value::as_str)
            .ok_or("Cargo did not report the atx executable")?;
        let path = Path::new(value);
        if !path.is_absolute() || value.contains('\0') {
            return Err("Cargo reported an invalid executable path".into());
        }
        if executable.replace(path.to_path_buf()).is_some() {
            return Err("Cargo reported multiple atx executables".into());
        }
    }
    executable.ok_or_else(|| "Cargo did not build an atx executable".into())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use proptest::prelude::*;
    use std::path::PathBuf;

    #[test]
    fn cargo_reports_the_custom_binary_path() {
        let messages = br#"{"reason":"compiler-artifact","target":{"name":"dependency","kind":["lib"]},"executable":null}
{"reason":"compiler-artifact","target":{"name":"atx","kind":["bin"]},"executable":"/tmp/custom/debug/atx"}
{"reason":"build-finished","success":true}"#;
        assert_eq!(
            super::artifact_executable(messages).expect("binary path"),
            PathBuf::from("/tmp/custom/debug/atx")
        );
    }

    #[test]
    fn cargo_output_requires_one_valid_atx_binary() {
        for messages in [
            b"not JSON".as_slice(),
            br#"{"reason":"build-finished","success":true}"#,
            br#"{"reason":"compiler-artifact","target":{"name":"atx","kind":["bin"]},"executable":null}"#,
            br#"{"reason":"compiler-artifact","target":{"name":"atx","kind":["bin"]},"executable":"relative/atx"}"#,
            br#"{"reason":"compiler-artifact","target":{"name":"atx","kind":["bin"]},"executable":"/tmp/atx\u0000"}"#,
            br#"{"reason":"compiler-artifact","target":{"name":"atx","kind":["bin"]},"executable":"/tmp/a"}
{"reason":"compiler-artifact","target":{"name":"atx","kind":["bin"]},"executable":"/tmp/b"}"#,
        ] {
            assert!(super::artifact_executable(messages).is_err());
        }
    }

    proptest! {
        #[test]
        fn arbitrary_cargo_output_never_panics(output in prop::collection::vec(any::<u8>(), 0..4096)) {
            if let Ok(path) = super::artifact_executable(&output) {
                prop_assert!(path.is_absolute());
                prop_assert!(!path.as_os_str().as_encoded_bytes().contains(&0));
            }
        }
    }
}
