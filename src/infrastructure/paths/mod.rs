//! Secure filesystem paths.

use std::fs::{self, File};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use rustix::fs::{FileType, Mode, OFlags, fstat, openat};
use rustix::process::geteuid;
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Platform {
    #[cfg(any(target_os = "linux", test))]
    Linux,
    #[cfg(any(target_os = "macos", test))]
    MacOs,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PathEnvironment {
    pub(crate) home: Option<PathBuf>,
    pub(crate) xdg_state_home: Option<PathBuf>,
    pub(crate) xdg_runtime_dir: Option<PathBuf>,
    pub(crate) temporary_directory: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PlatformPaths {
    state_dir: PathBuf,
    runtime_dir: PathBuf,
    runtime_uses_fallback: bool,
}

impl PlatformPaths {
    pub(crate) fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    pub(crate) fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }

    #[cfg(test)]
    pub(crate) const fn runtime_uses_fallback(&self) -> bool {
        self.runtime_uses_fallback
    }
}

pub(crate) fn resolve_paths(
    platform: Platform,
    environment: &PathEnvironment,
    state_override: Option<&Path>,
    uid: u32,
) -> Result<PlatformPaths, PathError> {
    if let Some(state_dir) = state_override {
        require_absolute(state_dir)?;
        return Ok(PlatformPaths {
            state_dir: state_dir.to_owned(),
            runtime_dir: state_dir.join("runtime"),
            runtime_uses_fallback: true,
        });
    }

    let state_dir = match platform {
        #[cfg(any(target_os = "linux", test))]
        Platform::Linux => environment
            .xdg_state_home
            .as_ref()
            .map(|root| root.join("atx"))
            .or_else(|| {
                environment
                    .home
                    .as_ref()
                    .map(|home| home.join(".local/state/atx"))
            })
            .ok_or(PathError::MissingHome)?,
        #[cfg(any(target_os = "macos", test))]
        Platform::MacOs => environment
            .home
            .as_ref()
            .map(|home| home.join("Library/Application Support/atx"))
            .ok_or(PathError::MissingHome)?,
    };
    require_absolute(&state_dir)?;

    let (runtime_dir, runtime_uses_fallback) = match platform {
        #[cfg(any(target_os = "linux", test))]
        Platform::Linux => match &environment.xdg_runtime_dir {
            Some(root) => (root.join("atx"), false),
            None => (fallback_runtime(environment, uid)?, true),
        },
        #[cfg(any(target_os = "macos", test))]
        Platform::MacOs => (fallback_runtime(environment, uid)?, true),
    };
    require_absolute(&runtime_dir)?;

    Ok(PlatformPaths {
        state_dir,
        runtime_dir,
        runtime_uses_fallback,
    })
}

fn fallback_runtime(environment: &PathEnvironment, uid: u32) -> Result<PathBuf, PathError> {
    environment
        .temporary_directory
        .as_ref()
        .map(|root| root.join(format!("atx-{uid}")))
        .ok_or(PathError::MissingRuntimeRoot)
}

fn require_absolute(path: &Path) -> Result<(), PathError> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(PathError::RelativePath)
    }
}

pub(crate) fn ensure_private_dir(path: &Path) -> Result<(), PathError> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    validate_private_dir_for_uid(path, geteuid().as_raw())
}

pub(crate) fn validate_private_dir_for_uid(
    path: &Path,
    expected_uid: u32,
) -> Result<(), PathError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(PathError::Insecure("path is not a real directory"));
    }
    if metadata.uid() != expected_uid {
        return Err(PathError::Insecure("directory has the wrong owner"));
    }
    if metadata.permissions().mode() & 0o777 != 0o700 {
        return Err(PathError::Insecure("directory mode must be 0700"));
    }
    Ok(())
}

pub(crate) fn create_new_private_file(directory: &File, name: &str) -> Result<File, PathError> {
    validate_leaf_name(name)?;
    let file = openat(
        directory,
        name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )?;
    Ok(File::from(file))
}

#[cfg(test)]
pub(crate) fn open_private_file(directory: &File, name: &str) -> Result<File, PathError> {
    validate_leaf_name(name)?;
    let file = openat(
        directory,
        name,
        OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let metadata = fstat(&file)?;
    if metadata.st_uid != geteuid().as_raw()
        || metadata.st_mode & 0o777 != 0o600
        || !FileType::from_raw_mode(metadata.st_mode).is_file()
    {
        return Err(PathError::Insecure(
            "file must be regular, owner-only, and owned by this user",
        ));
    }
    Ok(File::from(file))
}

/// Create-or-open an append log inside a validated private directory.
///
/// Like the run-log helpers, this never follows symlinks and rejects any
/// existing entry that is not a regular owner-only 0600 file, so a planted
/// `supervisor.log` cannot redirect supervisor diagnostics.
pub(crate) fn open_or_create_private_append_log(
    directory: &File,
    name: &str,
) -> Result<File, PathError> {
    validate_leaf_name(name)?;
    // NOENT means no entry exists yet, so the create path races nothing; a
    // FIFO or symlink at the leaf fails validation below instead of blocking
    // on open (O_NONBLOCK would mask that distinction).
    let file = match openat(
        directory,
        name,
        OFlags::WRONLY | OFlags::APPEND | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    ) {
        Ok(file) => file,
        Err(rustix::io::Errno::NOENT) => return create_new_private_file(directory, name),
        Err(errno) => return Err(errno.into()),
    };
    let metadata = fstat(&file)?;
    if metadata.st_uid != geteuid().as_raw()
        || metadata.st_mode & 0o777 != 0o600
        || !FileType::from_raw_mode(metadata.st_mode).is_file()
    {
        return Err(PathError::Insecure(
            "log must be a regular, owner-only file owned by this user",
        ));
    }
    Ok(File::from(file))
}

fn validate_leaf_name(name: &str) -> Result<(), PathError> {
    let mut components = Path::new(name).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(PathError::InvalidLeafName);
    }
    Ok(())
}

#[derive(Debug, Error)]
pub(crate) enum PathError {
    #[error("home directory is unavailable")]
    MissingHome,
    #[error("secure runtime root is unavailable")]
    MissingRuntimeRoot,
    #[error("path must be absolute")]
    RelativePath,
    #[error("path must be one normal relative component")]
    InvalidLeafName,
    #[error("insecure filesystem object: {0}")]
    Insecure(&'static str),
    #[error("filesystem operation failed: {0}")]
    Io(#[from] io::Error),
    #[error("system call failed: {0}")]
    System(#[from] rustix::io::Errno),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::ffi::CString;
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::sync::Arc;
    use std::thread;

    use tempfile::tempdir;

    use super::{
        PathEnvironment, Platform, create_new_private_file, ensure_private_dir,
        open_or_create_private_append_log, open_private_file, resolve_paths,
        validate_private_dir_for_uid,
    };

    #[test]
    fn platform_path_tables_match_conventions() {
        let environment = PathEnvironment {
            home: Some("/home/juan".into()),
            xdg_state_home: Some("/state".into()),
            xdg_runtime_dir: Some("/run/user/1000".into()),
            temporary_directory: Some("/tmp".into()),
        };
        let linux = resolve_paths(Platform::Linux, &environment, None, 1000).expect("linux paths");
        assert_eq!(linux.state_dir(), std::path::Path::new("/state/atx"));
        assert_eq!(
            linux.runtime_dir(),
            std::path::Path::new("/run/user/1000/atx")
        );
        assert!(!linux.runtime_uses_fallback());

        let mac = resolve_paths(Platform::MacOs, &environment, None, 1000).expect("mac paths");
        assert_eq!(
            mac.state_dir(),
            std::path::Path::new("/home/juan/Library/Application Support/atx")
        );
        assert_eq!(mac.runtime_dir(), std::path::Path::new("/tmp/atx-1000"));
        assert!(mac.runtime_uses_fallback());
    }

    #[test]
    fn override_works_without_home_but_missing_roots_fail() {
        let empty = PathEnvironment::default();
        assert!(resolve_paths(Platform::Linux, &empty, None, 1000).is_err());
        let paths = resolve_paths(
            Platform::Linux,
            &empty,
            Some(std::path::Path::new("/isolated")),
            1000,
        )
        .expect("isolated override");
        assert_eq!(paths.state_dir(), std::path::Path::new("/isolated"));
        assert_eq!(
            paths.runtime_dir(),
            std::path::Path::new("/isolated/runtime")
        );
    }

    #[test]
    fn private_directory_rejects_symlinks_modes_and_owners() {
        let root = tempdir().expect("temp root");
        let real = root.path().join("real");
        fs::create_dir(&real).expect("real directory");
        fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).expect("private mode");
        let link = root.path().join("link");
        symlink(&real, &link).expect("symlink");
        assert!(ensure_private_dir(&link).is_err());

        let open = root.path().join("open");
        fs::create_dir(&open).expect("open directory");
        fs::set_permissions(&open, fs::Permissions::from_mode(0o755)).expect("open mode");
        assert!(ensure_private_dir(&open).is_err());

        let uid = fs::metadata(&real).expect("metadata").uid();
        assert!(validate_private_dir_for_uid(&real, uid + 1).is_err());
    }

    #[test]
    fn concurrent_private_directory_creation_is_idempotent() {
        let root = tempdir().expect("temp root");
        let target = Arc::new(root.path().join("state"));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let target = Arc::clone(&target);
                thread::spawn(move || ensure_private_dir(&target))
            })
            .collect();
        for handle in threads {
            handle
                .join()
                .expect("thread completed")
                .expect("secure dir");
        }
        assert_eq!(
            fs::metadata(target.as_ref())
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[test]
    fn exclusive_relative_creation_does_not_follow_symlinks() {
        let root = tempdir().expect("temp root");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).expect("private root");
        ensure_private_dir(root.path()).expect("secure root");
        let directory = fs::File::open(root.path()).expect("directory handle");
        let victim = root.path().join("victim");
        fs::write(&victim, b"safe").expect("victim");
        symlink(&victim, root.path().join("state.db")).expect("malicious link");

        assert!(create_new_private_file(&directory, "state.db").is_err());
        assert_eq!(fs::read(victim).expect("victim unchanged"), b"safe");
        assert!(create_new_private_file(&directory, "../escape").is_err());
    }

    #[test]
    fn platform_paths_accessors_report_values() {
        let environment = PathEnvironment {
            home: Some("/home/juan".into()),
            xdg_state_home: Some("/state".into()),
            xdg_runtime_dir: Some("/run/user/1000".into()),
            temporary_directory: Some("/tmp".into()),
        };
        let paths = resolve_paths(Platform::Linux, &environment, None, 1000).expect("paths");
        assert_eq!(paths.state_dir(), std::path::Path::new("/state/atx"));
        assert_eq!(
            paths.runtime_dir(),
            std::path::Path::new("/run/user/1000/atx")
        );
        assert!(!paths.runtime_uses_fallback());
    }

    #[test]
    fn missing_home_and_runtime_root_fail_cleanly() {
        let empty = PathEnvironment::default();
        // Linux without an xdg state home or home yields MissingHome.
        assert!(resolve_paths(Platform::Linux, &empty, None, 1000).is_err());
        // macOS without a home yields MissingHome.
        assert!(resolve_paths(Platform::MacOs, &empty, None, 1000).is_err());
        // No temporary directory means the runtime fallback root is missing.
        let no_tmp = PathEnvironment {
            home: Some("/home/juan".into()),
            xdg_state_home: Some("/state".into()),
            xdg_runtime_dir: None,
            temporary_directory: None,
        };
        assert!(resolve_paths(Platform::Linux, &no_tmp, None, 1000).is_err());
    }

    #[test]
    fn relative_override_is_rejected() {
        let environment = PathEnvironment {
            home: Some("/home/juan".into()),
            xdg_state_home: Some("/state".into()),
            xdg_runtime_dir: Some("/run/user/1000".into()),
            temporary_directory: Some("/tmp".into()),
        };
        assert!(
            resolve_paths(
                Platform::Linux,
                &environment,
                Some(std::path::Path::new("relative/state")),
                1000,
            )
            .is_err()
        );
    }

    #[test]
    fn relative_leaf_names_are_rejected() {
        let root = tempdir().expect("temp root");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).expect("private root");
        ensure_private_dir(root.path()).expect("secure root");
        let directory = fs::File::open(root.path()).expect("directory handle");
        assert!(create_new_private_file(&directory, "").is_err());
        assert!(create_new_private_file(&directory, "/absolute").is_err());
        assert!(create_new_private_file(&directory, "dir/file").is_err());
        assert!(create_new_private_file(&directory, "file.db").is_ok());
    }

    #[test]
    fn open_private_file_validates_ownership_mode_and_type() {
        let root = tempdir().expect("temp root");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).expect("private root");
        ensure_private_dir(root.path()).expect("secure root");
        let directory = fs::File::open(root.path()).expect("directory handle");
        create_new_private_file(&directory, "data.db").expect("create");

        // Opening the regular, owner-only file succeeds.
        let file = open_private_file(&directory, "data.db").expect("open");
        drop(file);

        // A symlink to a private file is rejected by NOFOLLOW.
        symlink(root.path().join("data.db"), root.path().join("link.db")).expect("symlink");
        assert!(open_private_file(&directory, "link.db").is_err());

        // A wrong-mode file is rejected.
        let loose = root.path().join("loose.db");
        fs::write(&loose, b"x").expect("write");
        fs::set_permissions(&loose, fs::Permissions::from_mode(0o644)).expect("loose mode");
        assert!(open_private_file(&directory, "loose.db").is_err());

        // A directory in place of the leaf is rejected.
        fs::create_dir(root.path().join("dir.db")).expect("dir");
        assert!(open_private_file(&directory, "dir.db").is_err());

        // A FIFO opens fine but is not a regular file.
        let fifo = CString::new(root.path().join("pipe.db").as_os_str().as_encoded_bytes())
            .expect("fifo path");
        // SAFETY: `fifo` points at a valid C string naming a fresh path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        fs::set_permissions(
            root.path().join("pipe.db"),
            fs::Permissions::from_mode(0o600),
        )
        .expect("fifo mode");
        assert!(open_private_file(&directory, "pipe.db").is_err());
    }

    #[test]
    fn private_append_log_rejects_substituted_entries() {
        let root = tempdir().expect("temp root");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).expect("private root");
        ensure_private_dir(root.path()).expect("secure root");
        let directory = fs::File::open(root.path()).expect("directory handle");

        // Fresh creation yields a regular owner-only file.
        let log = open_or_create_private_append_log(&directory, "supervisor.log")
            .expect("create fresh log");
        drop(log);

        // Reopening an existing valid log succeeds.
        open_or_create_private_append_log(&directory, "supervisor.log").expect("reopen log");

        // A symlink planted over the log path is rejected by NOFOLLOW.
        symlink(root.path().join("victim"), root.path().join("planted.log")).expect("symlink");
        assert!(open_or_create_private_append_log(&directory, "planted.log").is_err());

        // A loose-mode existing log is rejected instead of appended to.
        fs::write(root.path().join("loose.log"), b"x").expect("write");
        fs::set_permissions(
            root.path().join("loose.log"),
            fs::Permissions::from_mode(0o644),
        )
        .expect("loose mode");
        assert!(open_or_create_private_append_log(&directory, "loose.log").is_err());

        // A directory in place of the log is rejected.
        fs::create_dir(root.path().join("dir.log")).expect("dir");
        assert!(open_or_create_private_append_log(&directory, "dir.log").is_err());

        // A FIFO opens but is not a regular file, so it must be rejected.
        let fifo = CString::new(root.path().join("pipe.log").as_os_str().as_encoded_bytes())
            .expect("fifo path");
        // SAFETY: `fifo` points at a valid C string naming a fresh path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        fs::set_permissions(
            root.path().join("pipe.log"),
            fs::Permissions::from_mode(0o600),
        )
        .expect("fifo mode");
        assert!(open_or_create_private_append_log(&directory, "pipe.log").is_err());
    }

    #[test]
    fn ensure_private_dir_surfaces_unrelated_creation_errors() {
        let root = tempdir().expect("temp root");
        // The parent of the target does not exist, so creation fails for a
        // reason other than AlreadyExists and must not be swallowed.
        let nested = root.path().join("missing").join("leaf");
        let error = ensure_private_dir(&nested).expect_err("missing parent");
        assert!(
            error.to_string().contains("filesystem operation failed"),
            "{error}"
        );
    }
}
