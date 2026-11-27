//! Owned raw PTY fixtures for unit and CLI integration checks.

#![allow(clippy::expect_used)]

use std::ffi::CStr;
use std::fs::File;
use std::io::{self, Read};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub(crate) struct Pty {
    master: File,
    slave: File,
    path: PathBuf,
}

impl Pty {
    pub(crate) fn open() -> io::Result<Self> {
        let (mut master, mut slave) = (-1, -1);
        // SAFETY: output pointers are valid; optional name/settings pointers are null.
        if unsafe {
            libc::openpty(
                &raw mut master,
                &raw mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        } == -1
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful openpty transferred two distinct, valid descriptors.
        let (master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
        for file in [&master, &slave] {
            // SAFETY: file owns a valid descriptor and F_SETFD accepts this flag.
            if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
                return Err(io::Error::last_os_error());
            }
        }
        // SAFETY: master owns the descriptor; these fcntl commands use no pointers.
        let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
        // SAFETY: the descriptor and combined file-status flags are valid.
        if flags == -1
            || unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }
                == -1
        {
            return Err(io::Error::last_os_error());
        }
        let mut name = [0_u8; 128];
        // SAFETY: the descriptor is valid and name has the stated writable size.
        let code =
            unsafe { libc::ttyname_r(slave.as_raw_fd(), name.as_mut_ptr().cast(), name.len()) };
        if code != 0 {
            return Err(io::Error::from_raw_os_error(code));
        }
        let name = CStr::from_bytes_until_nul(&name).map_err(io::Error::other)?;
        let path = PathBuf::from(name.to_str().map_err(io::Error::other)?);
        let mut settings = MaybeUninit::<libc::termios>::uninit();
        // SAFETY: tcgetattr receives a valid descriptor and writable termios buffer.
        if unsafe { libc::tcgetattr(slave.as_raw_fd(), settings.as_mut_ptr()) } == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful tcgetattr initialized every field used by termios APIs.
        let mut settings = unsafe { settings.assume_init() };
        // SAFETY: cfmakeraw receives an initialized, exclusively borrowed termios.
        unsafe { libc::cfmakeraw(&raw mut settings) };
        // SAFETY: only this fixture's slave is changed, using initialized settings.
        if unsafe { libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &raw const settings) } == -1 {
            return Err(io::Error::last_os_error());
        }
        slave.set_permissions(std::fs::Permissions::from_mode(0o620))?;
        Ok(Self {
            master,
            slave,
            path,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Duplicate the owned slave without reopening or acquiring a terminal.
    pub(crate) fn writer(&self) -> io::Result<File> {
        self.slave.try_clone()
    }

    pub(crate) fn set_mode(&self, mode: u32) -> io::Result<()> {
        self.slave
            .set_permissions(std::fs::Permissions::from_mode(mode))
    }

    pub(crate) fn read_available(&self) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        match (&self.master).take(65_536).read_to_end(&mut bytes) {
            Ok(_) => Ok(bytes),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(bytes),
            Err(error) => Err(error),
        }
    }
}
