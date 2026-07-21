//! Framed supervisor wake protocol.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use thiserror::Error;

use super::frame::{self, FrameError, PROTOCOL_VERSION};

pub(crate) use super::frame::IpcMessage;
use crate::application::{SupervisorAckError, SupervisorAcknowledger};
use crate::domain::{JobId, ProcessIdentitySnapshot, Revision};
use crate::infrastructure::paths::ensure_private_dir;
use crate::infrastructure::process::{IdentityStatus, NativeProcessInspector};

pub(crate) struct SocketAcknowledger {
    socket_path: PathBuf,
    timeout: std::time::Duration,
}

impl SocketAcknowledger {
    pub(crate) fn new(socket_path: PathBuf, timeout: std::time::Duration) -> Self {
        Self {
            socket_path,
            timeout,
        }
    }

    fn send(&self, job_id: JobId, revision: Revision) -> Result<(), IpcError> {
        let metadata = fs::symlink_metadata(&self.socket_path)?;
        if metadata.file_type().is_symlink()
            || !metadata.file_type().is_socket()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.permissions().mode() & 0o777 != 0o600
        {
            return Err(IpcError::SocketSubstitution);
        }
        let mut stream = UnixStream::connect(&self.socket_path)?;
        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;
        write_frame(
            &mut stream,
            &IpcMessage::Wake {
                protocol: PROTOCOL_VERSION,
                job_id,
                revision,
            },
        )?;
        match read_frame(&mut stream)? {
            IpcMessage::Ack {
                protocol: PROTOCOL_VERSION,
                job_id: acknowledged_job,
                revision: acknowledged_revision,
            } if acknowledged_job == job_id && acknowledged_revision == revision => Ok(()),
            _ => Err(IpcError::InvalidAcknowledgement),
        }
    }
}

impl SupervisorAcknowledger for SocketAcknowledger {
    fn acknowledge(&self, job_id: JobId, revision: Revision) -> Result<(), SupervisorAckError> {
        self.send(job_id, revision)
            .map_err(|error| SupervisorAckError(error.to_string()))
    }
}

pub(crate) fn write_frame(writer: &mut impl Write, message: &IpcMessage) -> Result<(), IpcError> {
    frame::write_frame(writer, message).map_err(IpcError::from)
}

pub(crate) fn read_frame(reader: &mut impl Read) -> Result<IpcMessage, IpcError> {
    frame::read_frame(reader).map_err(IpcError::from)
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
    #[error("supervisor returned the wrong acknowledgement")]
    InvalidAcknowledgement,
    #[error("runtime setup failed: {0}")]
    Runtime(String),
    #[error("IPC I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("IPC JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Frame(#[from] FrameError),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::os::unix::net::UnixListener;
    use std::time::Duration;

    use tempfile::tempdir;

    use super::{IpcMessage, RuntimeGuard, SocketAcknowledger, read_frame, write_frame};
    use crate::application::{ElapsedClock, SupervisorAcknowledger};
    use crate::domain::{JobId, Revision};
    use crate::infrastructure::process::NativeProcessInspector;
    use crate::infrastructure::time::NativeClock;

    fn inspector() -> NativeProcessInspector {
        let clock = NativeClock;
        NativeProcessInspector::new(clock.boot_identity().expect("boot identity"))
    }

    #[test]
    fn framing_rejects_malformed_and_oversize_messages() {
        let mut bytes = Vec::new();
        let job_id = JobId::new();
        let revision = Revision::new(1).expect("revision");
        write_frame(
            &mut bytes,
            &IpcMessage::Wake {
                protocol: 1,
                job_id,
                revision,
            },
        )
        .expect("write");
        assert_eq!(
            read_frame(&mut bytes.as_slice()).expect("read"),
            IpcMessage::Wake {
                protocol: 1,
                job_id,
                revision,
            }
        );
        assert!(read_frame(&mut [0, 1, 0, 1].as_slice()).is_err());
        assert!(read_frame(&mut [0, 0, 0, 2, b'{', b'}'].as_slice()).is_err());
    }

    #[test]
    fn wake_requires_an_exact_revision_ack() {
        let root = tempdir().expect("root");
        let socket = root.path().join("ack.sock");
        let listener = UnixListener::bind(&socket).expect("listener");
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).expect("permissions");
        let job_id = JobId::new();
        let revision = Revision::new(2).expect("revision");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            assert_eq!(
                read_frame(&mut stream).expect("wake"),
                IpcMessage::Wake {
                    protocol: 1,
                    job_id,
                    revision,
                }
            );
            write_frame(
                &mut stream,
                &IpcMessage::Ack {
                    protocol: 1,
                    job_id,
                    revision,
                },
            )
            .expect("ack");
        });
        let client = SocketAcknowledger::new(socket, Duration::from_secs(1));
        client.acknowledge(job_id, revision).expect("acknowledged");
        server.join().expect("server");
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
