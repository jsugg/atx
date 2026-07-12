//! Framed supervisor wake protocol.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::ProcessIdentitySnapshot;
use crate::infrastructure::paths::ensure_private_dir;
use crate::infrastructure::process::{IdentityStatus, NativeProcessInspector};

const MAX_FRAME_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum IpcMessage {
    Wake { protocol: u16 },
    Ack { protocol: u16 },
    Shutdown { protocol: u16 },
}

pub(crate) fn write_frame(writer: &mut impl Write, message: &IpcMessage) -> Result<(), IpcError> {
    let body = serde_json::to_vec(message)?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(IpcError::FrameTooLarge);
    }
    let length = u32::try_from(body.len()).map_err(|_| IpcError::FrameTooLarge)?;
    writer.write_all(&length.to_be_bytes())?;
    writer.write_all(&body)?;
    writer.flush()?;
    Ok(())
}

pub(crate) fn read_frame(reader: &mut impl Read) -> Result<IpcMessage, IpcError> {
    let mut length = [0_u8; 4];
    reader.read_exact(&mut length)?;
    let length =
        usize::try_from(u32::from_be_bytes(length)).map_err(|_| IpcError::FrameTooLarge)?;
    if length == 0 || length > MAX_FRAME_BYTES {
        return Err(IpcError::FrameTooLarge);
    }
    let mut body = vec![0_u8; length];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(IpcError::from)
}

pub(crate) struct RuntimeGuard {
    listener: UnixListener,
    lock_path: PathBuf,
    socket_path: PathBuf,
    lock_inode: u64,
    socket_inode: u64,
}

impl RuntimeGuard {
    pub(crate) fn acquire(
        runtime_directory: &Path,
        identity: &ProcessIdentitySnapshot,
        inspector: &NativeProcessInspector,
    ) -> Result<Self, IpcError> {
        ensure_private_dir(runtime_directory)
            .map_err(|error| IpcError::Runtime(error.to_string()))?;
        let lock_path = runtime_directory.join("supervisor.lock");
        acquire_lock(&lock_path, identity, inspector)?;
        let lock_inode = fs::symlink_metadata(&lock_path)?.ino();
        let socket_path = runtime_directory.join("supervisor.sock");
        if fs::symlink_metadata(&socket_path).is_ok() {
            let metadata = fs::symlink_metadata(&socket_path)?;
            if metadata.file_type().is_symlink() || !metadata.file_type().is_socket() {
                let _ = fs::remove_file(&lock_path);
                return Err(IpcError::SocketSubstitution);
            }
            if UnixStream::connect(&socket_path).is_ok() {
                let _ = fs::remove_file(&lock_path);
                return Err(IpcError::AlreadyRunning);
            }
            fs::remove_file(&socket_path)?;
        }
        let listener = match UnixListener::bind(&socket_path) {
            Ok(listener) => listener,
            Err(error) => {
                let _ = fs::remove_file(&lock_path);
                return Err(error.into());
            }
        };
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
        let socket_inode = fs::symlink_metadata(&socket_path)?.ino();
        Ok(Self {
            listener,
            lock_path,
            socket_path,
            lock_inode,
            socket_inode,
        })
    }

    pub(crate) const fn listener(&self) -> &UnixListener {
        &self.listener
    }
}

impl Drop for RuntimeGuard {
    fn drop(&mut self) {
        remove_if_inode_matches(&self.socket_path, self.socket_inode);
        remove_if_inode_matches(&self.lock_path, self.lock_inode);
    }
}

fn acquire_lock(
    path: &Path,
    identity: &ProcessIdentitySnapshot,
    inspector: &NativeProcessInspector,
) -> Result<(), IpcError> {
    for _ in 0..2 {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
        {
            Ok(mut file) => {
                serde_json::to_writer(&mut file, identity)?;
                file.sync_all()?;
                return Ok(());
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let metadata = fs::symlink_metadata(path)?;
                if metadata.file_type().is_symlink()
                    || !metadata.is_file()
                    || metadata.permissions().mode() & 0o777 != 0o600
                {
                    return Err(IpcError::LockSubstitution);
                }
                let bytes = fs::read(path)?;
                let owner: ProcessIdentitySnapshot =
                    serde_json::from_slice(&bytes).map_err(|_| IpcError::MalformedLock)?;
                if inspector
                    .classify(&owner)
                    .map_err(|error| IpcError::Runtime(error.to_string()))?
                    == IdentityStatus::Alive
                {
                    return Err(IpcError::AlreadyRunning);
                }
                fs::remove_file(path)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Err(IpcError::AlreadyRunning)
}

fn remove_if_inode_matches(path: &Path, inode: u64) {
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.ino() == inode) {
        let _ = fs::remove_file(path);
    }
}

#[derive(Debug, Error)]
pub(crate) enum IpcError {
    #[error("another supervisor is running")]
    AlreadyRunning,
    #[error("runtime lock was substituted")]
    LockSubstitution,
    #[error("runtime socket was substituted")]
    SocketSubstitution,
    #[error("runtime lock is malformed")]
    MalformedLock,
    #[error("IPC frame is empty or too large")]
    FrameTooLarge,
    #[error("runtime setup failed: {0}")]
    Runtime(String),
    #[error("IPC I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("IPC JSON failed: {0}")]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};

    use tempfile::tempdir;

    use super::{IpcMessage, RuntimeGuard, read_frame, write_frame};
    use crate::application::ElapsedClock;
    use crate::infrastructure::process::NativeProcessInspector;
    use crate::infrastructure::time::NativeClock;

    fn inspector() -> NativeProcessInspector {
        let clock = NativeClock;
        NativeProcessInspector::new(clock.boot_identity().expect("boot identity"))
    }

    #[test]
    fn framing_rejects_malformed_and_oversize_messages() {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &IpcMessage::Wake { protocol: 1 }).expect("write");
        assert_eq!(
            read_frame(&mut bytes.as_slice()).expect("read"),
            IpcMessage::Wake { protocol: 1 }
        );
        assert!(read_frame(&mut [0, 1, 0, 1].as_slice()).is_err());
        assert!(read_frame(&mut [0, 0, 0, 2, b'{', b'}'].as_slice()).is_err());
    }

    #[test]
    fn runtime_is_single_instance_and_cleans_up() {
        let root = tempdir().expect("root");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).expect("mode");
        let inspector = inspector();
        let identity = inspector
            .inspect(std::process::id())
            .expect("inspect")
            .expect("identity");
        let guard = RuntimeGuard::acquire(root.path(), &identity, &inspector).expect("first");
        assert!(RuntimeGuard::acquire(root.path(), &identity, &inspector).is_err());
        assert_eq!(
            guard
                .listener()
                .local_addr()
                .expect("address")
                .as_pathname(),
            Some(root.path().join("supervisor.sock").as_path())
        );
        drop(guard);
        assert!(!root.path().join("supervisor.lock").exists());
        assert!(!root.path().join("supervisor.sock").exists());
    }

    #[test]
    fn stale_lock_is_replaced_but_socket_symlink_is_rejected() {
        let root = tempdir().expect("root");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).expect("mode");
        let inspector = inspector();
        let mut stale = inspector
            .inspect(std::process::id())
            .expect("inspect")
            .expect("identity");
        stale.start_token += 1;
        fs::write(
            root.path().join("supervisor.lock"),
            serde_json::to_vec(&stale).expect("serialize"),
        )
        .expect("lock");
        fs::set_permissions(
            root.path().join("supervisor.lock"),
            fs::Permissions::from_mode(0o600),
        )
        .expect("lock mode");
        symlink("/tmp", root.path().join("supervisor.sock")).expect("symlink");
        let identity = inspector
            .inspect(std::process::id())
            .expect("inspect")
            .expect("identity");
        assert!(RuntimeGuard::acquire(root.path(), &identity, &inspector).is_err());
    }
}
