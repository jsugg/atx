//! Durable-service lifecycle contracts.

use std::path::PathBuf;

use serde::Serialize;
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ServiceAvailability {
    Available,
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ServiceStatus {
    pub(crate) manager: String,
    pub(crate) availability: ServiceAvailability,
    pub(crate) installed: bool,
    pub(crate) running: bool,
    pub(crate) files: Vec<PathBuf>,
    pub(crate) guarantee: String,
    pub(crate) detail: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ServiceChange {
    pub(crate) changed: bool,
    pub(crate) status: ServiceStatus,
}

pub(crate) trait ServiceManager {
    fn status(&self) -> Result<ServiceStatus, ServiceManagerError>;
    fn install(&mut self) -> Result<(), ServiceManagerError>;
    fn uninstall(&mut self) -> Result<(), ServiceManagerError>;
}

pub(crate) fn install_service(
    manager: &mut impl ServiceManager,
) -> Result<ServiceChange, ServiceLifecycleError> {
    let before = manager.status()?;
    require_available(&before)?;
    if before.installed && before.running {
        return Ok(ServiceChange {
            changed: false,
            status: before,
        });
    }

    if let Err(install_error) = manager.install() {
        return match manager.uninstall() {
            Ok(()) => Err(ServiceLifecycleError::Install(install_error)),
            Err(rollback_error) => Err(ServiceLifecycleError::InstallRollback {
                install: install_error.to_string(),
                rollback: rollback_error.to_string(),
            }),
        };
    }
    let status = manager.status()?;
    if !status.installed || !status.running {
        let detail = status.detail.clone();
        return match manager.uninstall() {
            Ok(()) => Err(ServiceLifecycleError::IncompleteInstall(detail)),
            Err(rollback_error) => Err(ServiceLifecycleError::InstallRollback {
                install: detail,
                rollback: rollback_error.to_string(),
            }),
        };
    }
    Ok(ServiceChange {
        changed: true,
        status,
    })
}

pub(crate) fn uninstall_service(
    manager: &mut impl ServiceManager,
) -> Result<ServiceChange, ServiceLifecycleError> {
    let before = manager.status()?;
    require_available(&before)?;
    if !before.installed {
        return Ok(ServiceChange {
            changed: false,
            status: before,
        });
    }
    manager.uninstall()?;
    let status = manager.status()?;
    if status.installed || status.running {
        return Err(ServiceLifecycleError::IncompleteUninstall(status.detail));
    }
    Ok(ServiceChange {
        changed: true,
        status,
    })
}

fn require_available(status: &ServiceStatus) -> Result<(), ServiceLifecycleError> {
    if status.availability == ServiceAvailability::Available {
        Ok(())
    } else {
        Err(ServiceLifecycleError::Unavailable(status.detail.clone()))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
#[error("{message}")]
pub(crate) struct ServiceManagerError {
    message: String,
}

impl ServiceManagerError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub(crate) enum ServiceLifecycleError {
    #[error(transparent)]
    Manager(#[from] ServiceManagerError),
    #[error("durable service is unavailable: {0}")]
    Unavailable(String),
    #[error("service installation failed and was rolled back: {0}")]
    Install(ServiceManagerError),
    #[error("service installation remained incomplete and was rolled back: {0}")]
    IncompleteInstall(String),
    #[error("service installation failed ({install}); rollback also failed ({rollback})")]
    InstallRollback { install: String, rollback: String },
    #[error("service uninstall remained incomplete: {0}")]
    IncompleteUninstall(String),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{
        ServiceAvailability, ServiceLifecycleError, ServiceManager, ServiceManagerError,
        ServiceStatus, install_service, uninstall_service,
    };

    #[test]
    fn unavailable_manager_never_attempts_install() {
        let mut manager = FakeManager::unavailable();
        assert!(matches!(
            install_service(&mut manager),
            Err(ServiceLifecycleError::Unavailable(_))
        ));
        assert_eq!(manager.installs, 0);
    }

    #[test]
    fn partial_install_is_rolled_back() {
        let mut manager = FakeManager::available();
        manager.fail_install = true;
        assert!(matches!(
            install_service(&mut manager),
            Err(ServiceLifecycleError::Install(_))
        ));
        assert_eq!(manager.uninstalls, 1);
        assert!(!manager.installed);
    }

    #[test]
    fn lifecycle_is_idempotent() {
        let mut manager = FakeManager::available();
        assert!(install_service(&mut manager).expect("install").changed);
        assert!(
            !install_service(&mut manager)
                .expect("repeat install")
                .changed
        );
        assert!(uninstall_service(&mut manager).expect("uninstall").changed);
        assert!(
            !uninstall_service(&mut manager)
                .expect("repeat uninstall")
                .changed
        );
    }

    #[allow(clippy::struct_excessive_bools)]
    struct FakeManager {
        available: bool,
        installed: bool,
        running: bool,
        fail_install: bool,
        installs: usize,
        uninstalls: usize,
    }

    impl FakeManager {
        const fn available() -> Self {
            Self {
                available: true,
                installed: false,
                running: false,
                fail_install: false,
                installs: 0,
                uninstalls: 0,
            }
        }

        const fn unavailable() -> Self {
            Self {
                available: false,
                ..Self::available()
            }
        }
    }

    impl ServiceManager for FakeManager {
        fn status(&self) -> Result<ServiceStatus, ServiceManagerError> {
            Ok(ServiceStatus {
                manager: "fake".to_owned(),
                availability: if self.available {
                    ServiceAvailability::Available
                } else {
                    ServiceAvailability::Unavailable
                },
                installed: self.installed,
                running: self.running,
                files: Vec::new(),
                guarantee: "test".to_owned(),
                detail: "test fixture".to_owned(),
            })
        }

        fn install(&mut self) -> Result<(), ServiceManagerError> {
            self.installs += 1;
            self.installed = true;
            if self.fail_install {
                return Err(ServiceManagerError::new("injected failure"));
            }
            self.running = true;
            Ok(())
        }

        fn uninstall(&mut self) -> Result<(), ServiceManagerError> {
            self.uninstalls += 1;
            self.installed = false;
            self.running = false;
            Ok(())
        }
    }
}
