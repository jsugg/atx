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
