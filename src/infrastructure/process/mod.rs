//! Native process adapter.

mod cancel;
mod spawn;

#[allow(unused_imports)]
pub(crate) use cancel::NativeGroupCanceller;
#[allow(unused_imports)]
pub(crate) use cancel::{CancellationResult, cancel_validated_group};
#[allow(unused_imports)]
pub(crate) use spawn::{NativeProcessRunner, SpawnedChild};

use std::io;

use thiserror::Error;

use crate::application::{
    IdentityInspectionError, IdentityInspector, IdentityStatus as RecoveryIdentityStatus,
};
use crate::domain::ProcessIdentitySnapshot;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IdentityStatus {
    Alive,
    Dead,
    Reused,
}

#[derive(Clone, Debug)]
pub(crate) struct NativeProcessInspector {
    boot_identity: String,
}

impl NativeProcessInspector {
    pub(crate) fn new(boot_identity: String) -> Self {
        Self { boot_identity }
    }

    pub(crate) fn inspect(
        &self,
        pid: u32,
    ) -> Result<Option<ProcessIdentitySnapshot>, ProcessError> {
        if pid == 0 {
            return Err(ProcessError::InvalidIdentity);
        }
        inspect_platform(pid, &self.boot_identity)
    }

    pub(crate) fn classify(
        &self,
        expected: &ProcessIdentitySnapshot,
    ) -> Result<IdentityStatus, ProcessError> {
        validate_snapshot(expected)?;
        let current = self.inspect(expected.pid)?;
        Ok(classify_identity(expected, current.as_ref()))
    }
}

impl IdentityInspector for NativeProcessInspector {
    fn classify(
        &self,
        identity: &ProcessIdentitySnapshot,
    ) -> Result<RecoveryIdentityStatus, IdentityInspectionError> {
        NativeProcessInspector::classify(self, identity)
            .map(|status| match status {
                IdentityStatus::Alive => RecoveryIdentityStatus::Alive,
                IdentityStatus::Dead => RecoveryIdentityStatus::Dead,
                IdentityStatus::Reused => RecoveryIdentityStatus::Changed,
            })
            .map_err(|error| IdentityInspectionError(error.to_string()))
    }
}

fn classify_identity(
    expected: &ProcessIdentitySnapshot,
    current: Option<&ProcessIdentitySnapshot>,
) -> IdentityStatus {
    match current {
        None => IdentityStatus::Dead,
        Some(current) if current == expected => IdentityStatus::Alive,
        Some(_) => IdentityStatus::Reused,
    }
}

fn validate_snapshot(identity: &ProcessIdentitySnapshot) -> Result<(), ProcessError> {
    if identity.boot_identity.is_empty()
        || identity.boot_identity.contains('\0')
        || identity.pid == 0
        || identity.start_token == 0
        || identity.process_group_id <= 0
    {
        return Err(ProcessError::InvalidIdentity);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn inspect_platform(
    pid: u32,
    boot_identity: &str,
) -> Result<Option<ProcessIdentitySnapshot>, ProcessError> {
    let path = format!("/proc/{pid}/stat");
    match std::fs::read_to_string(path) {
        Ok(stat) => parse_linux_stat(&stat, boot_identity).map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(map_inspection_error(error)),
    }
}

fn map_inspection_error(error: io::Error) -> ProcessError {
    if error.kind() == io::ErrorKind::PermissionDenied {
        ProcessError::PermissionDenied
    } else {
        ProcessError::Io(error)
    }
}

#[cfg(target_os = "linux")]
fn parse_linux_stat(
    stat: &str,
    boot_identity: &str,
) -> Result<ProcessIdentitySnapshot, ProcessError> {
    let opening = stat.find('(').ok_or(ProcessError::Malformed)?;
    let closing = stat.rfind(')').ok_or(ProcessError::Malformed)?;
    if opening == 0 || closing <= opening {
        return Err(ProcessError::Malformed);
    }
    let pid = stat[..opening]
        .trim()
        .parse::<u32>()
        .map_err(|_| ProcessError::Malformed)?;
    let fields: Vec<&str> = stat[closing + 1..].split_whitespace().collect();
    if fields.len() < 20 {
        return Err(ProcessError::Malformed);
    }
    let process_group_id = fields[2]
        .parse::<i32>()
        .map_err(|_| ProcessError::Malformed)?;
    let start_token = fields[19]
        .parse::<u64>()
        .map_err(|_| ProcessError::Malformed)?;
    let snapshot = ProcessIdentitySnapshot {
        boot_identity: boot_identity.to_owned(),
        pid,
        start_token,
        process_group_id,
    };
    validate_snapshot(&snapshot)?;
    Ok(snapshot)
}

#[cfg(target_os = "macos")]
fn inspect_platform(
    pid: u32,
    boot_identity: &str,
) -> Result<Option<ProcessIdentitySnapshot>, ProcessError> {
    let pid = i32::try_from(pid).map_err(|_| ProcessError::InvalidIdentity)?;
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    let size_i32 = i32::try_from(size).map_err(|_| ProcessError::Unsupported)?;
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    // SAFETY: `info` points to writable storage of exactly the size passed to libproc.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size_i32,
        )
    };
    if written == 0 {
        return match io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH) | None => Ok(None),
            Some(libc::EPERM | libc::EACCES) => Err(ProcessError::PermissionDenied),
            _ => Err(ProcessError::Unavailable),
        };
    }
    if written != size_i32 {
        return Err(ProcessError::Malformed);
    }
    // SAFETY: libproc reported that it initialized the complete structure.
    let info = unsafe { info.assume_init() };
    let start_token = info
        .pbi_start_tvsec
        .checked_mul(1_000_000)
        .and_then(|value| value.checked_add(info.pbi_start_tvusec))
        .ok_or(ProcessError::Malformed)?;
    let process_group_id = i32::try_from(info.pbi_pgid).map_err(|_| ProcessError::Malformed)?;
    let snapshot = ProcessIdentitySnapshot {
        boot_identity: boot_identity.to_owned(),
        pid: info.pbi_pid,
        start_token,
        process_group_id,
    };
    validate_snapshot(&snapshot)?;
    Ok(Some(snapshot))
}

#[derive(Debug, Error)]
pub(crate) enum ProcessError {
    #[error("process identity must include boot, PID, start token, and process group")]
    InvalidIdentity,
    #[error("process information is malformed")]
    Malformed,
    #[error("permission denied while inspecting process")]
    PermissionDenied,
    #[error("process inspection is unsupported on this platform")]
    Unsupported,
    #[error("process inspection is unavailable")]
    Unavailable,
    #[error("process inspection failed: {0}")]
    Io(io::Error),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use crate::application::{ElapsedClock, IdentityInspector};
    use crate::domain::ProcessIdentitySnapshot;
    use crate::infrastructure::time::NativeClock;

    #[cfg(target_os = "linux")]
    use super::parse_linux_stat;
    use super::{
        IdentityStatus, NativeProcessInspector, ProcessError, RecoveryIdentityStatus,
        classify_identity, map_inspection_error, validate_snapshot,
    };

    #[test]
    fn current_process_has_stable_full_identity() {
        let clock = NativeClock;
        let inspector = NativeProcessInspector::new(clock.boot_identity().expect("boot identity"));
        let pid = std::process::id();
        let identity = inspector.inspect(pid).expect("inspect").expect("alive");
        assert_eq!(identity.pid, pid);
        assert!(identity.start_token > 0);
        assert!(identity.process_group_id > 0);
        assert_eq!(
            inspector.classify(&identity).expect("classify"),
            IdentityStatus::Alive
        );
    }

    #[test]
    fn identity_classification_detects_dead_and_reused_processes() {
        let expected = ProcessIdentitySnapshot {
            boot_identity: "boot-a".to_owned(),
            pid: 42,
            start_token: 100,
            process_group_id: 42,
        };
        assert_eq!(classify_identity(&expected, None), IdentityStatus::Dead);
        let mut changed = expected.clone();
        changed.start_token += 1;
        assert_eq!(
            classify_identity(&expected, Some(&changed)),
            IdentityStatus::Reused
        );
        assert_eq!(
            classify_identity(&expected, Some(&expected)),
            IdentityStatus::Alive
        );
    }

    #[test]
    fn partial_identity_is_rejected() {
        let partial = ProcessIdentitySnapshot {
            boot_identity: String::new(),
            pid: 42,
            start_token: 0,
            process_group_id: 0,
        };
        assert!(validate_snapshot(&partial).is_err());
        assert!(matches!(
            map_inspection_error(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
            super::ProcessError::PermissionDenied
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_stat_parser_handles_spaces_and_parentheses() {
        let stat = "123 (odd ) name) S 1 77 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 4242";
        let parsed = parse_linux_stat(stat, "boot-a").expect("parse");
        assert_eq!(parsed.pid, 123);
        assert_eq!(parsed.process_group_id, 77);
        assert_eq!(parsed.start_token, 4242);
        assert!(parse_linux_stat("broken", "boot-a").is_err());
    }

    #[test]
    fn inspect_rejects_pid_zero() {
        let inspector = NativeProcessInspector::new("boot-a".to_owned());
        assert!(inspector.inspect(0).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_stat_parser_rejects_malformed_input() {
        // no parentheses at all
        assert!(parse_linux_stat("not-a-stat-line", "boot-a").is_err());
        // empty before/after the comm field, or comm field missing close
        assert!(parse_linux_stat("(orphan", "boot-a").is_err());
        // too few fields after comm to reach pgid/start_time
        assert!(parse_linux_stat("1 (init) S 0", "boot-a").is_err());
        // pid is not numeric
        assert!(
            parse_linux_stat(
                "abc (x) S 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0",
                "boot-a"
            )
            .is_err()
        );
        // pgid is not numeric
        assert!(
            parse_linux_stat(
                "1 (x) S 0 notanint 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0",
                "boot-a"
            )
            .is_err()
        );
        // start_time is not numeric
        assert!(
            parse_linux_stat(
                "1 (x) S 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 notanint",
                "boot-a"
            )
            .is_err()
        );
        // validated-away because pgid<=0
        assert!(
            parse_linux_stat("1 (x) S 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0", "boot-a").is_err()
        );
    }

    #[test]
    fn validate_snapshot_rejects_each_constraint() {
        let base = ProcessIdentitySnapshot {
            boot_identity: "boot-a".to_owned(),
            pid: 42,
            start_token: 100,
            process_group_id: 42,
        };
        let mut variant = base.clone();
        variant.boot_identity = String::new();
        assert!(validate_snapshot(&variant).is_err());

        variant = base.clone();
        variant.boot_identity = "has\0nul".to_owned();
        assert!(validate_snapshot(&variant).is_err());

        variant = base.clone();
        variant.pid = 0;
        assert!(validate_snapshot(&variant).is_err());

        variant = base.clone();
        variant.start_token = 0;
        assert!(validate_snapshot(&variant).is_err());

        variant = base.clone();
        variant.process_group_id = 0;
        assert!(validate_snapshot(&variant).is_err());

        assert!(validate_snapshot(&base).is_ok());
    }

    #[test]
    fn map_inspection_error_maps_permission_denied_and_io() {
        assert!(matches!(
            map_inspection_error(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
            ProcessError::PermissionDenied
        ));
        assert!(matches!(
            map_inspection_error(std::io::Error::other("boom")),
            ProcessError::Io(_)
        ));
    }

    #[test]
    fn identity_inspector_classify_maps_native_status() {
        let clock = NativeClock;
        let inspector = NativeProcessInspector::new(clock.boot_identity().expect("boot identity"));
        let pid = std::process::id();
        let identity = inspector.inspect(pid).expect("inspect").expect("alive");
        assert!(matches!(
            IdentityInspector::classify(&inspector, &identity),
            Ok(RecoveryIdentityStatus::Alive)
        ));

        // An invalid snapshot (pid 0) fails validation before any inspection.
        let invalid = ProcessIdentitySnapshot {
            boot_identity: String::new(),
            pid: 0,
            start_token: 0,
            process_group_id: 0,
        };
        assert!(IdentityInspector::classify(&inspector, &invalid).is_err());
    }
}
