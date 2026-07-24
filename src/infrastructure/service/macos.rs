//! Per-user launchd service adapter.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use crate::application::{ServiceAvailability, ServiceManager, ServiceManagerError, ServiceStatus};

const LABEL: &str = "io.github.jsugg.atx";

pub(crate) struct LaunchdService<R = NativeCommandRunner> {
    executable: PathBuf,
    state_directory: PathBuf,
    runtime_directory: PathBuf,
    agent_path: PathBuf,
    uid: u32,
    runner: R,
}

impl LaunchdService {
    pub(crate) fn new(
        executable: PathBuf,
        state_directory: PathBuf,
        runtime_directory: PathBuf,
        home: &Path,
        uid: u32,
    ) -> Self {
        Self {
            executable,
            state_directory,
            runtime_directory,
            agent_path: home
                .join("Library")
                .join("LaunchAgents")
                .join(format!("{LABEL}.plist")),
            uid,
            runner: NativeCommandRunner,
        }
    }
}

impl<R: CommandRunner> LaunchdService<R> {
    #[cfg(test)]
    fn with_runner(
        executable: PathBuf,
        state_directory: PathBuf,
        runtime_directory: PathBuf,
        agent_path: PathBuf,
        uid: u32,
        runner: R,
    ) -> Self {
        Self {
            executable,
            state_directory,
            runtime_directory,
            agent_path,
            uid,
            runner,
        }
    }

    fn expected_plist(&self) -> String {
        render_plist(
            &self.executable,
            &self.state_directory,
            &self.runtime_directory,
        )
    }

    fn domain(&self) -> String {
        format!("gui/{}", self.uid)
    }

    fn service_target(&self) -> String {
        format!("{}/{LABEL}", self.domain())
    }

    fn installed(&self) -> Result<bool, ServiceManagerError> {
        match fs::read_to_string(&self.agent_path) {
            Ok(contents) if contents == self.expected_plist() => Ok(true),
            Ok(_) => Err(ServiceManagerError::new(format!(
                "refusing foreign launchd file {}",
                self.agent_path.display()
            ))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(ServiceManagerError::new(format!(
                "cannot read {}: {error}",
                self.agent_path.display()
            ))),
        }
    }

    fn run(&mut self, args: &[&str]) -> Result<Output, ServiceManagerError> {
        self.runner
            .run("/bin/launchctl", args)
            .map_err(ServiceManagerError::new)
    }
}

impl<R: CommandRunner> ServiceManager for LaunchdService<R> {
    fn status(&self) -> Result<ServiceStatus, ServiceManagerError> {
        let available = Path::new("/bin/launchctl").is_file();
        let installed = self.installed()?;
        let target = self.service_target();
        let running = available
            && installed
            && self
                .runner
                .run("/bin/launchctl", &["print", &target])
                .is_ok_and(|output| output.status.success());
        Ok(ServiceStatus {
            manager: "launchd".to_owned(),
            availability: if available {
                ServiceAvailability::Available
            } else {
                ServiceAvailability::Unavailable
            },
            installed,
            running,
            files: vec![self.agent_path.clone()],
            guarantee: "restarts after crashes and when this user's launchd domain starts"
                .to_owned(),
            detail: if !available {
                "launchctl is unavailable".to_owned()
            } else if running {
                "launch agent is loaded and running".to_owned()
            } else if installed {
                "launch agent is installed but not running".to_owned()
            } else {
                "launch agent is not installed".to_owned()
            },
        })
    }

    fn install(&mut self) -> Result<(), ServiceManagerError> {
        if self.installed()? {
            return Ok(());
        }
        let parent = self
            .agent_path
            .parent()
            .ok_or_else(|| ServiceManagerError::new("launch agent path has no parent"))?;
        fs::create_dir_all(parent).map_err(|error| {
            ServiceManagerError::new(format!("cannot create {}: {error}", parent.display()))
        })?;
        let temporary = self.agent_path.with_extension("plist.tmp");
        let write_result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(self.expected_plist().as_bytes())?;
            file.sync_all()?;
            fs::rename(&temporary, &self.agent_path)?;
            Ok::<(), std::io::Error>(())
        })();
        if let Err(error) = write_result {
            let _ = fs::remove_file(&temporary);
            return Err(ServiceManagerError::new(format!(
                "cannot install {}: {error}",
                self.agent_path.display()
            )));
        }
        fs::set_permissions(&self.agent_path, fs::Permissions::from_mode(0o600))
            .map_err(|error| ServiceManagerError::new(error.to_string()))?;
        let domain = self.domain();
        let agent = self.agent_path.to_string_lossy().into_owned();
        let output = self.run(&["bootstrap", &domain, &agent])?;
        if !output.status.success() {
            return Err(command_error("launchctl bootstrap", &output));
        }
        let target = self.service_target();
        let output = self.run(&["kickstart", "-k", &target])?;
        if !output.status.success() {
            return Err(command_error("launchctl kickstart", &output));
        }
        Ok(())
    }

    fn uninstall(&mut self) -> Result<(), ServiceManagerError> {
        if !self.installed()? {
            return Ok(());
        }
        let target = self.service_target();
        let output = self.run(&["bootout", &target])?;
        if !output.status.success() && self.status()?.running {
            return Err(command_error("launchctl bootout", &output));
        }
        fs::remove_file(&self.agent_path).map_err(|error| {
            ServiceManagerError::new(format!(
                "cannot remove {}: {error}",
                self.agent_path.display()
            ))
        })
    }
}

fn render_plist(executable: &Path, state_directory: &Path, runtime_directory: &Path) -> String {
    let arguments = [
        executable.to_string_lossy().into_owned(),
        "__supervisor".to_owned(),
        "--state-dir".to_owned(),
        state_directory.to_string_lossy().into_owned(),
        "--runtime-dir".to_owned(),
        runtime_directory.to_string_lossy().into_owned(),
        "--service-managed".to_owned(),
    ]
    .into_iter()
    .map(|argument| format!("      <string>{}</string>", xml_escape(&argument)))
    .collect::<Vec<_>>()
    .join("\n");
    let log = xml_escape(&state_directory.join("supervisor.log").to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
  <dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
{arguments}
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ProcessType</key>
    <string>Background</string>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
  </dict>
</plist>
"#
    )
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn command_error(command: &str, output: &Output) -> ServiceManagerError {
    let stderr = String::from_utf8_lossy(&output.stderr);
    ServiceManagerError::new(format!("{command} failed: {}", stderr.trim()))
}

pub(crate) trait CommandRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<Output, String>;
}

pub(crate) struct NativeCommandRunner;

impl CommandRunner for NativeCommandRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<Output, String> {
        Command::new(program)
            .args(args)
            .output()
            .map_err(|error| error.to_string())
    }
}

// NOTE: status() probes /bin/launchctl on the host, so these tests only hold
// where launchd tooling exists; the adapter itself compiles everywhere.
#[cfg(all(test, target_os = "macos"))]
mod tests {
    #![allow(clippy::expect_used)]

    use std::cell::RefCell;
    use std::os::unix::process::ExitStatusExt;

    use tempfile::tempdir;

    use super::{CommandRunner, LaunchdService, render_plist};
    use crate::application::{ServiceManager, install_service};

    #[test]
    fn plist_escapes_paths_and_contains_durable_supervisor_contract() {
        let plist = render_plist(
            std::path::Path::new("/Applications/A&T/atx"),
            std::path::Path::new("/tmp/state <one>"),
            std::path::Path::new("/tmp/runtime"),
        );
        assert!(plist.contains("/Applications/A&amp;T/atx"));
        assert!(plist.contains("/tmp/state &lt;one&gt;"));
        assert!(plist.contains("<string>--service-managed</string>"));
        assert!(plist.contains("<key>KeepAlive</key>"));
    }

    #[test]
    fn install_uses_expected_launchctl_lifecycle() {
        let root = tempdir().expect("root");
        let agent = root.path().join("agent.plist");
        let runner = FakeRunner::default();
        let calls = runner.calls.clone();
        let mut service = LaunchdService::with_runner(
            "/bin/atx".into(),
            root.path().join("state"),
            root.path().join("runtime"),
            agent.clone(),
            501,
            runner,
        );
        let change = install_service(&mut service).expect("install");
        assert!(change.changed);
        assert_eq!(
            calls.borrow().as_slice(),
            [
                &format!("bootstrap gui/501 {}", agent.display()),
                "kickstart -k gui/501/io.github.jsugg.atx",
                "print gui/501/io.github.jsugg.atx",
            ]
        );
    }

    #[test]
    fn foreign_agent_file_is_rejected() {
        let root = tempdir().expect("root");
        let agent = root.path().join("agent.plist");
        std::fs::write(&agent, "not ours").expect("foreign file");
        let service = LaunchdService::with_runner(
            "/bin/atx".into(),
            root.path().join("state"),
            root.path().join("runtime"),
            agent,
            501,
            FakeRunner::default(),
        );
        assert!(service.status().is_err());
    }

    #[derive(Clone, Default)]
    struct FakeRunner {
        calls: std::rc::Rc<RefCell<Vec<String>>>,
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, _program: &str, args: &[&str]) -> Result<std::process::Output, String> {
            self.calls.borrow_mut().push(args.join(" "));
            Ok(std::process::Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }
}
