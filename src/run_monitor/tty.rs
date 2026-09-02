//! Bounded terminal echo after a run finishes.

use std::fs::{File, OpenOptions};
use std::io::{self, IsTerminal, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::Path;

const ECHO_LIMIT: u16 = 8 * 1024;

struct LogTail {
    bytes: Vec<u8>,
    truncated: bool,
}

fn tty_attributes_allowed(char_device: bool, owner: u32, mode: u32, effective_uid: u32) -> bool {
    char_device && owner == effective_uid && mode & 0o002 == 0
}

fn open_tty(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NOCTTY | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !tty_attributes_allowed(
        metadata.file_type().is_char_device(),
        metadata.uid(),
        metadata.mode(),
        rustix::process::geteuid().as_raw(),
    ) || !file.is_terminal()
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "saved terminal is not an owned, non-world-writable TTY",
        ));
    }
    Ok(file)
}

fn read_tail(path: &Path) -> io::Result<LogTail> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NOCTTY | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "captured log is not a regular file",
        ));
    }
    file.seek(SeekFrom::Start(
        metadata.len().saturating_sub(u64::from(ECHO_LIMIT)),
    ))?;
    let mut bytes = Vec::with_capacity(usize::from(ECHO_LIMIT));
    file.take(u64::from(ECHO_LIMIT)).read_to_end(&mut bytes)?;
    Ok(LogTail {
        bytes,
        truncated: metadata.len() > u64::from(ECHO_LIMIT),
    })
}

fn sanitize_tail(bytes: &[u8]) -> String {
    let limit = usize::from(ECHO_LIMIT);
    let bytes = &bytes[bytes.len().saturating_sub(limit)..];
    let mut text = String::with_capacity(bytes.len());
    for character in String::from_utf8_lossy(bytes).chars() {
        if !character.is_control() || matches!(character, '\n' | '\t') {
            text.push(character);
        }
    }
    let mut start = text.len().saturating_sub(limit);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text.drain(..start);
    text
}

/// Echo bounded sanitized tails to a validated terminal without waiting for its reader.
pub(super) fn echo_tty(
    tty: &Path,
    stdout_log: &Path,
    stderr_log: &Path,
    captured_truncated: bool,
) -> io::Result<()> {
    let mut terminal = open_tty(tty)?;
    let stdout = read_tail(stdout_log)?;
    let stderr = read_tail(stderr_log);
    if captured_truncated {
        writeln!(terminal, "[atx: output truncated at capture cap]")?;
    }
    if stdout.truncated || stderr.as_ref().is_ok_and(|tail| tail.truncated) {
        writeln!(
            terminal,
            "[atx: terminal echo limited to last 8 KiB per stream]"
        )?;
    }
    writeln!(terminal)?;
    terminal.write_all(sanitize_tail(&stdout.bytes).as_bytes())?;
    terminal.write_all(sanitize_tail(&stderr?.bytes).as_bytes())?;
    terminal.flush()
}

#[cfg(test)]
pub(crate) mod test_support {
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
                || unsafe {
                    libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK)
                } == -1
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
            if unsafe { libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &raw const settings) }
                == -1
            {
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
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::test_support::Pty;
    use super::{ECHO_LIMIT, echo_tty, read_tail, sanitize_tail, tty_attributes_allowed};
    use proptest::prelude::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn tty_permissions_allow_owner_group_write_but_reject_unsafe_metadata() {
        assert!(tty_attributes_allowed(true, 501, 0o620, 501));
        assert!(tty_attributes_allowed(true, 501, 0o600, 501));
        assert!(!tty_attributes_allowed(false, 501, 0o620, 501));
        assert!(!tty_attributes_allowed(true, 502, 0o620, 501));
        assert!(!tty_attributes_allowed(true, 501, 0o622, 501));
    }

    #[test]
    fn sanitizer_removes_controls_without_damaging_utf8() {
        let input = "\0\u{1b}[31mhello\u{7f}\u{80}\u{9b} été😀\t\n";
        assert_eq!(sanitize_tail(input.as_bytes()), "[31mhello été😀\t\n");
    }

    #[test]
    fn tails_read_only_the_last_eight_kibibytes() {
        let root = tempdir().expect("root");
        let log = root.path().join("stdout.log");
        let mut bytes = vec![b'x'; 2 * usize::from(ECHO_LIMIT)];
        bytes.extend_from_slice(b"end");
        fs::write(&log, &bytes).expect("log");
        let tail = read_tail(&log).expect("tail");
        assert_eq!(tail.bytes, bytes[bytes.len() - usize::from(ECHO_LIMIT)..]);
        assert!(tail.truncated);
    }

    #[test]
    fn owned_pty_echoes_sanitized_streams_and_rejects_links_and_world_write() {
        let root = tempdir().expect("root");
        let pty = Pty::open().expect("pty");
        let stdout = root.path().join("stdout.log");
        let stderr = root.path().join("stderr.log");
        fs::write(&stdout, b"out\x1b[31m\t\n").expect("stdout");
        fs::write(&stderr, "err\u{9b}é".as_bytes()).expect("stderr");
        echo_tty(pty.path(), &stdout, &stderr, false).expect("echo");
        assert_eq!(
            pty.read_available().expect("output"),
            b"\nout[31m\t\nerr\xc3\xa9"
        );
        let link = root.path().join("tty-link");
        std::os::unix::fs::symlink(pty.path(), &link).expect("link");
        assert!(echo_tty(&link, &stdout, &stderr, false).is_err());
        assert!(pty.read_available().expect("unchanged pty").is_empty());
        pty.set_mode(0o622).expect("world writable");
        assert!(echo_tty(pty.path(), &stdout, &stderr, false).is_err());
        assert!(pty.read_available().expect("unchanged pty").is_empty());
        pty.set_mode(0o620).expect("restore fixture mode");
    }

    #[test]
    fn log_tails_reject_links_and_nonregular_inputs() {
        let root = tempdir().expect("root");
        let target = root.path().join("log");
        fs::write(&target, b"unchanged").expect("log");
        let link = root.path().join("log-link");
        std::os::unix::fs::symlink(&target, &link).expect("link");
        assert!(read_tail(&link).is_err());
        assert!(read_tail(root.path()).is_err());
        assert_eq!(fs::read(&target).expect("unchanged log"), b"unchanged");
    }

    #[test]
    fn missing_terminal_and_unreadable_stdout_do_not_write() {
        let root = tempdir().expect("root");
        let pty = Pty::open().expect("pty");
        let missing = root.path().join("missing");
        assert!(echo_tty(&missing, &missing, &missing, false).is_err());
        assert!(echo_tty(pty.path(), &missing, &missing, false).is_err());
        assert!(pty.read_available().expect("unchanged terminal").is_empty());
    }

    #[test]
    fn tail_truncation_is_reported_even_when_controls_remove_the_payload() {
        let root = tempdir().expect("root");
        let pty = Pty::open().expect("pty");
        let stdout = root.path().join("stdout.log");
        let stderr = root.path().join("stderr.log");
        fs::write(&stdout, vec![0_u8; 2 * usize::from(ECHO_LIMIT)]).expect("stdout");
        fs::write(&stderr, b"end").expect("stderr");
        echo_tty(pty.path(), &stdout, &stderr, false).expect("tail echo");
        let echoed = String::from_utf8(pty.read_available().expect("echoed bytes")).expect("UTF-8");
        assert!(echoed.contains("last 8 KiB per stream"), "{echoed}");
        assert!(echoed.ends_with("\n\nend"), "{echoed}");
        assert!(!echoed.contains('\0'));
    }

    proptest! {
        #[test]
        fn arbitrary_bytes_produce_bounded_text_without_controls(bytes in prop::collection::vec(any::<u8>(), 0..32768)) {
            let text = sanitize_tail(&bytes);
            prop_assert!(text.len() <= usize::from(ECHO_LIMIT));
            prop_assert!(text.chars().all(|character| !character.is_control() || matches!(character, '\n' | '\t')));
        }
    }
}
