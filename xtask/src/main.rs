//! Repository maintenance tasks. Run with `cargo xtask <task>`.

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        Some("dist-man") => match dist_man() {
            Ok(path) => {
                println!("{}", path.display());
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("xtask dist-man: {error}");
                ExitCode::FAILURE
            }
        },
        _ => {
            eprintln!("usage: cargo xtask dist-man");
            ExitCode::from(2)
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
    let status = std::process::Command::new("cargo")
        .args(["build", "--bin", "atx", "--features", "man"])
        .current_dir(&root)
        .status()
        .map_err(|error| format!("cargo build: {error}"))?;
    if !status.success() {
        return Err("cargo build --features man failed".to_owned());
    }

    let atx = root.join("target").join("debug").join("atx");
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
    let manifest = std::env::var("CARGO_MANIFEST_DIR")
        .map_err(|_| "CARGO_MANIFEST_DIR not set; run via `cargo xtask`".to_owned())?;
    std::path::Path::new(&manifest)
        .parent()
        .map(std::path::Path::to_path_buf)
        .ok_or_else(|| "xtask has no parent directory".to_owned())
}
