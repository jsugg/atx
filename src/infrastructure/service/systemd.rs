//! Per-user systemd service adapter.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use crate::application::{ServiceAvailability, ServiceManager, ServiceManagerError, ServiceStatus};

const UNIT_NAME: &str = "atx.service";

pub(crate) struct SystemdUserService<R = NativeCommandRunner> {
    executable: PathBuf,
    state_directory: PathBuf,
    runtime_directory: PathBuf,
    unit_path: PathBuf,
    uid: u32,
    runner: R,
}

impl SystemdUserService {
    pub(crate) fn new(
        executable: PathBuf,
        state_directory: PathBuf,
        runtime_directory: PathBuf,
        config_home: &Path,
        uid: u32,
    ) -> Self {
        Self {
            executable,
            state_directory,
            runtime_directory,
            unit_path: config_home.join("systemd/user").join(UNIT_NAME),
            uid,
            runner: NativeCommandRunner,
        }
    }
}

impl<R: CommandRunner> SystemdUserService<R> {
    #[cfg(test)]
    fn with_runner(
        executable: PathBuf,
        state_directory: PathBuf,
        runtime_directory: PathBuf,
        unit_path: PathBuf,
        uid: u32,
        runner: R,
    ) -> Self {
        Self {
            executable,
            state_directory,
            runtime_directory,
            unit_path,
            uid,
            runner,
        }
    }

    fn expected_unit(&self) -> Result<String, ServiceManagerError> {
        render_unit(
            &self.executable,
            &self.state_directory,
            &self.runtime_directory,
        )
    }

    fn installed(&self) -> Result<bool, ServiceManagerError> {
        match fs::read_to_string(&self.unit_path) {
            Ok(contents) if contents == self.expected_unit()? => Ok(true),
            Ok(_) => Err(ServiceManagerError::new(format!(
                "refusing foreign systemd unit {}",
                self.unit_path.display()
            ))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(ServiceManagerError::new(format!(
                "cannot read {}: {error}",
                self.unit_path.display()
            ))),
        }
    }

    fn systemctl(&self, args: &[&str]) -> Result<Output, ServiceManagerError> {
        self.runner
            .run("systemctl", args)
            .map_err(ServiceManagerError::new)
    }

    fn manager_available(&self) -> bool {
        self.systemctl(&["--user", "show-environment"])
            .is_ok_and(|output| output.status.success())
    }

    fn lingering_enabled(&self) -> bool {
        let uid = self.uid.to_string();
        self.runner
            .run(
                "loginctl",
                &["show-user", &uid, "--property=Linger", "--value"],
            )
            .is_ok_and(|output| output.status.success() && output.stdout == b"yes\n")
    }
}

impl<R: CommandRunner> ServiceManager for SystemdUserService<R> {
    fn status(&self) -> Result<ServiceStatus, ServiceManagerError> {
        let available = self.manager_available();
        let installed = self.installed()?;
        let running = available
            && installed
            && self
                .systemctl(&["--user", "is-active", "--quiet", UNIT_NAME])
                .is_ok_and(|output| output.status.success());
        let lingering = available && self.lingering_enabled();
        let guarantee = if lingering {
            "restarts after crashes and starts without an interactive login"
        } else {
            "restarts after crashes while this user's systemd manager is running"
        };
        Ok(ServiceStatus {
            manager: "systemd-user".to_owned(),
            availability: if available {
                ServiceAvailability::Available
            } else {
                ServiceAvailability::Unavailable
            },
            installed,
            running,
            files: vec![self.unit_path.clone()],
            guarantee: guarantee.to_owned(),
            detail: if !available {
                "the systemd user manager is unavailable".to_owned()
            } else if running {
                if lingering {
                    "user service is enabled, running, and lingering is enabled".to_owned()
                } else {
                    "user service is enabled and running; lingering is disabled".to_owned()
                }
            } else if installed {
                "user service is installed but not running".to_owned()
            } else {
                "user service is not installed".to_owned()
            },
        })
    }

    fn install(&mut self) -> Result<(), ServiceManagerError> {
        if !self.manager_available() {
            return Err(ServiceManagerError::new(
                "the systemd user manager is unavailable",
            ));
        }
        if self.installed()? {
            return Ok(());
        }
        let parent = self
            .unit_path
            .parent()
            .ok_or_else(|| ServiceManagerError::new("systemd unit path has no parent"))?;
        fs::create_dir_all(parent).map_err(|error| {
            ServiceManagerError::new(format!("cannot create {}: {error}", parent.display()))
        })?;
        let temporary = self.unit_path.with_extension("service.tmp");
        let write_result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)
                .map_err(|error| ServiceManagerError::new(error.to_string()))?;
            file.write_all(self.expected_unit()?.as_bytes())
                .map_err(|error| ServiceManagerError::new(error.to_string()))?;
            file.sync_all()
                .map_err(|error| ServiceManagerError::new(error.to_string()))?;
            fs::rename(&temporary, &self.unit_path)
                .map_err(|error| ServiceManagerError::new(error.to_string()))?;
            Ok::<(), ServiceManagerError>(())
        })();
        if let Err(error) = write_result {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        fs::set_permissions(&self.unit_path, fs::Permissions::from_mode(0o600))
            .map_err(|error| ServiceManagerError::new(error.to_string()))?;
        require_success(
            "systemctl --user daemon-reload",
            &self.systemctl(&["--user", "daemon-reload"])?,
        )?;
        require_success(
            "systemctl --user enable",
            &self.systemctl(&["--user", "enable", "--now", UNIT_NAME])?,
        )
    }

    fn uninstall(&mut self) -> Result<(), ServiceManagerError> {
        if !self.installed()? {
            return Ok(());
        }
        require_success(
            "systemctl --user disable",
            &self.systemctl(&["--user", "disable", "--now", UNIT_NAME])?,
        )?;
        fs::remove_file(&self.unit_path).map_err(|error| {
            ServiceManagerError::new(format!(
                "cannot remove {}: {error}",
                self.unit_path.display()
            ))
        })?;
        require_success(
            "systemctl --user daemon-reload",
            &self.systemctl(&["--user", "daemon-reload"])?,
        )
    }
}

fn render_unit(
    executable: &Path,
    state_directory: &Path,
    runtime_directory: &Path,
) -> Result<String, ServiceManagerError> {
    let arguments = [
        executable,
        Path::new("__supervisor"),
        Path::new("--state-dir"),
        state_directory,
        Path::new("--runtime-dir"),
        runtime_directory,
        Path::new("--service-managed"),
    ]
    .into_iter()
    .map(systemd_quote)
    .collect::<Result<Vec<_>, _>>()?
    .join(" ");
    Ok(format!(
        "[Unit]\nDescription=ATX per-user command scheduler\n\n\
         [Service]\nType=simple\nExecStart={arguments}\nRestart=on-failure\nRestartSec=2s\n\
         KillMode=process\nNoNewPrivileges=true\nPrivateTmp=true\n\n\
         [Install]\nWantedBy=default.target\n"
    ))
}

fn systemd_quote(value: &Path) -> Result<String, ServiceManagerError> {
    let value = value
        .to_str()
        .ok_or_else(|| ServiceManagerError::new("service paths must be valid UTF-8"))?;
    if value.contains(['\n', '\r', '\0']) {
        return Err(ServiceManagerError::new(
            "service arguments cannot contain control characters",
        ));
    }
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "$$")
        .replace('%', "%%");
    Ok(format!("\"{escaped}\""))
}

fn require_success(command: &str, output: &Output) -> Result<(), ServiceManagerError> {
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(ServiceManagerError::new(format!(
            "{command} failed: {}",
            stderr.trim()
        )))
    }
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

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::cell::RefCell;
    use std::fs;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Output;
    use std::rc::Rc;

    use tempfile::tempdir;

    use super::{CommandRunner, SystemdUserService, UNIT_NAME, render_unit};
    use crate::application::{ServiceManager, install_service};

    #[test]
    fn unit_quotes_specifiers_and_requests_restart() {
        let unit = render_unit(
            std::path::Path::new("/opt/100%/atx"),
            std::path::Path::new("/tmp/state one"),
            std::path::Path::new("/tmp/runtime"),
        )
        .expect("unit");
        assert!(unit.contains("\"/opt/100%%/atx\""));
        assert!(unit.contains("\"/tmp/state one\""));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("\"--service-managed\""));
    }

    #[test]
    fn isolated_install_runs_reload_enable_and_status() {
        let root = tempdir().expect("root");
        let unit = root.path().join(UNIT_NAME);
        let runner = FakeRunner::healthy();
        let calls = runner.calls.clone();
        let mut service = SystemdUserService::with_runner(
            "/bin/atx".into(),
            root.path().join("state"),
            root.path().join("runtime"),
            unit,
            1000,
            runner,
        );
        assert!(install_service(&mut service).expect("install").changed);
        let calls = calls.borrow().join("\n");
        assert!(calls.contains("systemctl --user daemon-reload"));
        assert!(calls.contains("systemctl --user enable --now atx.service"));
        assert!(calls.contains("systemctl --user is-active --quiet atx.service"));
    }

    #[test]
    fn status_reflects_running_and_lingering_states() {
        let root = tempdir().expect("root");
        let unit = root.path().join(UNIT_NAME);
        fs::write(
            &unit,
            render_unit(
                std::path::Path::new("/bin/atx"),
                &root.path().join("state"),
                &root.path().join("runtime"),
            )
            .expect("unit"),
        )
        .expect("write unit");

        let lingering = SystemdUserService::with_runner(
            "/bin/atx".into(),
            root.path().join("state"),
            root.path().join("runtime"),
            unit.clone(),
            1000,
            FakeRunner::healthy(),
        );
        let status = lingering.status().expect("status");
        assert!(status.running);
        assert_eq!(
            status.detail,
            "user service is enabled, running, and lingering is enabled"
        );
        assert_eq!(
            status.guarantee,
            "restarts after crashes and starts without an interactive login"
        );

        let plain = SystemdUserService::with_runner(
            "/bin/atx".into(),
            root.path().join("state"),
            root.path().join("runtime"),
            unit,
            1000,
            FakeRunner::no_linger(),
        );
        let status = plain.status().expect("status");
        assert!(status.running);
        assert_eq!(
            status.detail,
            "user service is enabled and running; lingering is disabled"
        );
        assert_eq!(
            status.guarantee,
            "restarts after crashes while this user's systemd manager is running"
        );
    }

    #[test]
    fn uninstall_disables_removes_unit_and_reloads() {
        let root = tempdir().expect("root");
        let unit = root.path().join(UNIT_NAME);
        fs::write(
            &unit,
            render_unit(
                std::path::Path::new("/bin/atx"),
                &root.path().join("state"),
                &root.path().join("runtime"),
            )
            .expect("unit"),
        )
        .expect("write unit");
        let runner = FakeRunner::healthy();
        let calls = runner.calls.clone();
        let mut service = SystemdUserService::with_runner(
            "/bin/atx".into(),
            root.path().join("state"),
            root.path().join("runtime"),
            unit.clone(),
            1000,
            runner,
        );
        service.uninstall().expect("uninstall");
        let calls = calls.borrow().join("\n");
        assert!(calls.contains("systemctl --user disable --now atx.service"));
        assert!(calls.contains("systemctl --user daemon-reload"));
        assert!(!unit.exists());
    }

    #[test]
    fn uninstall_without_install_is_a_noop() {
        let root = tempdir().expect("root");
        let runner = FakeRunner::healthy();
        let calls = runner.calls.clone();
        let mut service = SystemdUserService::with_runner(
            "/bin/atx".into(),
            root.path().join("state"),
            root.path().join("runtime"),
            root.path().join(UNIT_NAME),
            1000,
            runner,
        );
        service.uninstall().expect("uninstall");
        assert!(!calls.borrow().iter().any(|call| call.contains("disable")));
    }

    #[test]
    fn failed_enable_fails_the_install_without_leaving_temporary_files() {
        let root = tempdir().expect("root");
        let unit = root.path().join(UNIT_NAME);
        let mut service = SystemdUserService::with_runner(
            "/bin/atx".into(),
            root.path().join("state"),
            root.path().join("runtime"),
            unit,
            1000,
            FakeRunner::failing("enable"),
        );
        assert!(service.install().is_err());
        let leftovers: Vec<_> = fs::read_dir(root.path())
            .expect("directory")
            .filter_map(|entry| entry.expect("entry").file_name().into_string().ok())
            .collect();
        let temporary_left = leftovers.iter().any(|name| {
            std::path::Path::new(name)
                .extension()
                .is_some_and(|extension| extension == "tmp")
        });
        assert!(!temporary_left);
    }

    #[test]
    fn foreign_unit_blocks_install() {
        let root = tempdir().expect("root");
        let unit = root.path().join(UNIT_NAME);
        fs::write(&unit, "foreign").expect("foreign unit");
        let mut service = SystemdUserService::with_runner(
            "/bin/atx".into(),
            root.path().join("state"),
            root.path().join("runtime"),
            unit,
            1000,
            FakeRunner::healthy(),
        );
        assert!(service.install().is_err());
    }

    #[test]
    fn unavailable_manager_and_foreign_unit_are_rejected() {
        let root = tempdir().expect("root");
        let unit = root.path().join(UNIT_NAME);
        let unavailable = SystemdUserService::with_runner(
            "/bin/atx".into(),
            root.path().join("state"),
            root.path().join("runtime"),
            unit.clone(),
            1000,
            FakeRunner::unavailable(),
        );
        assert_eq!(
            unavailable.status().expect("status").availability,
            crate::application::ServiceAvailability::Unavailable
        );

        fs::write(&unit, "foreign").expect("foreign unit");
        assert!(unavailable.status().is_err());
    }

    struct FakeRunner {
        calls: Rc<RefCell<Vec<String>>>,
        available: bool,
        linger: bool,
        failing_command: Option<&'static str>,
    }

    impl FakeRunner {
        fn healthy() -> Self {
            Self {
                calls: Rc::new(RefCell::new(Vec::new())),
                available: true,
                linger: true,
                failing_command: None,
            }
        }

        fn unavailable() -> Self {
            Self {
                available: false,
                ..Self::healthy()
            }
        }

        fn no_linger() -> Self {
            Self {
                linger: false,
                ..Self::healthy()
            }
        }

        fn failing(command: &'static str) -> Self {
            Self {
                failing_command: Some(command),
                ..Self::healthy()
            }
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, program: &str, args: &[&str]) -> Result<Output, String> {
            self.calls
                .borrow_mut()
                .push(format!("{program} {}", args.join(" ")));
            let success = self.available
                && !self
                    .failing_command
                    .is_some_and(|needle| args.contains(&needle));
            let stdout = if program == "loginctl" && success && self.linger {
                b"yes\n".to_vec()
            } else {
                Vec::new()
            };
            Ok(Output {
                status: std::process::ExitStatus::from_raw(i32::from(!success) << 8),
                stdout,
                stderr: Vec::new(),
            })
        }
    }
}
