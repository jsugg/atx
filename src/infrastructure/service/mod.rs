//! Durable service adapters.

mod macos;
mod systemd;

use std::path::{Path, PathBuf};

use crate::application::{ServiceManager, ServiceManagerError, ServiceStatus};

#[allow(unused_imports)]
pub(crate) use macos::LaunchdService;
#[allow(unused_imports)]
pub(crate) use systemd::SystemdUserService;

pub(crate) enum NativeServiceManager {
    #[cfg(target_os = "macos")]
    Launchd(LaunchdService),
    #[cfg(target_os = "linux")]
    Systemd(SystemdUserService),
}

impl NativeServiceManager {
    pub(crate) fn detect(
        executable: PathBuf,
        state_directory: PathBuf,
        runtime_directory: PathBuf,
        home: &Path,
        uid: u32,
    ) -> Self {
        #[cfg(target_os = "macos")]
        {
            Self::Launchd(LaunchdService::new(
                executable,
                state_directory,
                runtime_directory,
                home,
                uid,
            ))
        }
        #[cfg(target_os = "linux")]
        {
            let config_home = std::env::var_os("XDG_CONFIG_HOME")
                .map_or_else(|| home.join(".config"), PathBuf::from);
            Self::Systemd(SystemdUserService::new(
                executable,
                state_directory,
                runtime_directory,
                &config_home,
                uid,
            ))
        }
    }
}

impl ServiceManager for NativeServiceManager {
    fn status(&self) -> Result<ServiceStatus, ServiceManagerError> {
        match self {
            #[cfg(target_os = "macos")]
            Self::Launchd(manager) => manager.status(),
            #[cfg(target_os = "linux")]
            Self::Systemd(manager) => manager.status(),
        }
    }

    fn install(&mut self) -> Result<(), ServiceManagerError> {
        match self {
            #[cfg(target_os = "macos")]
            Self::Launchd(manager) => manager.install(),
            #[cfg(target_os = "linux")]
            Self::Systemd(manager) => manager.install(),
        }
    }

    fn uninstall(&mut self) -> Result<(), ServiceManagerError> {
        match self {
            #[cfg(target_os = "macos")]
            Self::Launchd(manager) => manager.uninstall(),
            #[cfg(target_os = "linux")]
            Self::Systemd(manager) => manager.uninstall(),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use crate::application::ServiceManager;

    use super::NativeServiceManager;

    #[cfg(target_os = "macos")]
    #[test]
    fn detect_builds_a_launchd_manager_with_agent_path() {
        let manager = NativeServiceManager::detect(
            "/bin/atx".into(),
            "/state".into(),
            "/runtime".into(),
            std::path::Path::new("/home/juan"),
            501,
        );
        match manager {
            NativeServiceManager::Launchd(service) => {
                let status = service.status();
                // On a host with launchctl present, the manager reports itself
                // available; the probe exercising the real binary is what we
                // assert, and it must not error.
                assert!(status.is_ok());
                assert_eq!(
                    status.expect("launchd status").availability,
                    crate::application::ServiceAvailability::Available
                );
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn detect_builds_a_systemd_manager_honoring_xdg_config_home() {
        // SAFETY: single-threaded test binary; no other thread reads the env.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", "/custom/config") };
        let manager = NativeServiceManager::detect(
            "/bin/atx".into(),
            "/state".into(),
            "/runtime".into(),
            std::path::Path::new("/home/juan"),
            1000,
        );
        match manager {
            NativeServiceManager::Systemd(service) => {
                let status = service.status();
                assert!(status.is_ok());
                assert_eq!(
                    status.expect("launchd status").availability,
                    crate::application::ServiceAvailability::Unavailable
                );
            }
            _ => panic!("expected a systemd manager on Linux"),
        }
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn detect_uses_home_fallback_for_config_when_unset() {
        // SAFETY: single-threaded test binary; no other thread reads the env.
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
        let manager = NativeServiceManager::detect(
            "/bin/atx".into(),
            "/state".into(),
            "/runtime".into(),
            std::path::Path::new("/home/juan"),
            1000,
        );
        assert!(matches!(manager, NativeServiceManager::Systemd(_)));
    }
}
