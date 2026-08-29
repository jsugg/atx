use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
#[cfg(target_os = "macos")]
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::FileTypeExt;
#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
use rustix::process::kill_process;
use rustix::process::{Pid, Signal};
#[cfg(target_os = "linux")]
use rustix::process::{PidfdFlags, pidfd_open, pidfd_send_signal};
use serde::Deserialize;
use tempfile::TempDir;

const CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);
const CENSUS_TIMEOUT: Duration = Duration::from_secs(5);
const TERM_GRACE: Duration = Duration::from_millis(250);
const CLEANUP_POLL: Duration = Duration::from_millis(100);
const STABLE_EMPTY_SCANS: u8 = 2;
#[cfg(target_os = "macos")]
const ARGV_INSPECTION_ATTEMPTS: usize = 3;

/// Temporary test root that stops every ATX process rooted beneath it.
pub(crate) struct TestRoot {
    inner: TempDir,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProcessRow {
    pid: i32,
    parent_pid: i32,
    process_group_id: i32,
    user_id: u32,
    state: String,
    command_name: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub(crate) struct ProcessIdentity {
    boot_identity: String,
    pid: u32,
    start_token: u64,
    process_group_id: i32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Inspection<T> {
    Present(T),
    Absent,
    Transient,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IdentityMatch {
    Same,
    Absent,
    Different,
    WrongUser,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CensusObservation {
    Empty,
    NonEmpty,
    Unknown,
}

#[derive(Default)]
pub(crate) struct StableEmpty {
    consecutive: u8,
}

#[derive(Clone, Debug)]
struct OwnedProcess {
    row: ProcessRow,
    identity: ProcessIdentity,
}

struct ProcessCensus {
    processes: Vec<OwnedProcess>,
    complete: bool,
}

#[derive(Default)]
struct FixtureOwnership {
    identities: BTreeMap<i32, ProcessIdentity>,
}

impl ProcessRow {
    #[allow(dead_code)]
    pub(crate) fn for_test(pid: i32, parent_pid: i32, process_group_id: i32, user_id: u32) -> Self {
        Self {
            pid,
            parent_pid,
            process_group_id,
            user_id,
            state: "S".to_owned(),
            command_name: "test".to_owned(),
        }
    }
}

impl ProcessIdentity {
    #[allow(dead_code)]
    pub(crate) fn for_test(
        boot_identity: &str,
        pid: u32,
        start_token: u64,
        process_group_id: i32,
    ) -> Self {
        Self {
            boot_identity: boot_identity.to_owned(),
            pid,
            start_token,
            process_group_id,
        }
    }
}

impl StableEmpty {
    pub(crate) fn observe(&mut self, observation: CensusObservation) -> bool {
        self.consecutive = match observation {
            CensusObservation::Empty => self.consecutive.saturating_add(1),
            CensusObservation::NonEmpty | CensusObservation::Unknown => 0,
        };
        self.consecutive >= STABLE_EMPTY_SCANS
    }
}

pub(crate) fn classify_identity(
    expected: &ProcessIdentity,
    expected_user: u32,
    current: Inspection<ProcessIdentity>,
    current_user: u32,
) -> IdentityMatch {
    match current {
        Inspection::Absent => IdentityMatch::Absent,
        Inspection::Transient => IdentityMatch::Unknown,
        Inspection::Present(_) if current_user != expected_user => IdentityMatch::WrongUser,
        Inspection::Present(current) if current == *expected => IdentityMatch::Same,
        Inspection::Present(_) => IdentityMatch::Different,
    }
}

pub(crate) fn expand_owned_descendants(
    rows: &[ProcessRow],
    mut owned: BTreeSet<i32>,
    effective_uid: u32,
) -> BTreeSet<i32> {
    loop {
        let before = owned.len();
        for process in rows {
            if process.user_id == effective_uid && owned.contains(&process.parent_pid) {
                owned.insert(process.pid);
            }
        }
        if owned.len() == before {
            return owned;
        }
    }
}

#[allow(dead_code)]
pub(crate) fn retry_inspection<T>(
    attempts: usize,
    mut inspect: impl FnMut() -> Result<Inspection<T>, String>,
) -> Result<Inspection<T>, String> {
    if attempts == 0 {
        return Ok(Inspection::Transient);
    }
    for _ in 1..attempts {
        match inspect()? {
            Inspection::Transient => std::thread::yield_now(),
            result => return Ok(result),
        }
    }
    inspect()
}

/// Create an ATX-aware temporary root.
pub(crate) fn tempdir() -> io::Result<TestRoot> {
    tempfile::tempdir().map(|inner| TestRoot { inner })
}

/// Create an ATX-aware temporary root beneath `parent`.
#[allow(dead_code)]
pub(crate) fn tempdir_in(parent: &Path) -> io::Result<TestRoot> {
    tempfile::tempdir_in(parent).map(|inner| TestRoot { inner })
}

/// Abruptly stop the session supervisor owned by `state` for crash tests.
#[allow(dead_code)]
pub(crate) fn kill_session_supervisor(state: &Path) -> Result<(), String> {
    let runtime = state.join("runtime");
    let lock = runtime.join("supervisor.lock");
    let Some(identity) = read_identity_if_present(&lock)? else {
        return Ok(());
    };
    if !supervisor_matches(
        &identity,
        &runtime,
        Some(state),
        state.parent().unwrap_or(state),
    )? {
        return Ok(());
    }
    if !identity_is_unchanged(&lock, &identity)? {
        return Ok(());
    }
    signal_process(&identity, Signal::KILL)?;
    if wait_until(CLEANUP_TIMEOUT, || process_identity_is_terminal(&identity))? {
        Ok(())
    } else {
        Err(format!(
            "supervisor PID {} did not exit after SIGKILL",
            identity.pid
        ))
    }
}

/// Return whether `pid` is still present, including zombie processes.
#[allow(dead_code)]
pub(crate) fn process_exists(pid: i32) -> Result<bool, String> {
    Ok(process_table()?.iter().any(|process| process.pid == pid))
}

/// Return whether `pid` has exited or reached an unreaped terminal state.
#[allow(dead_code)]
pub(crate) fn process_is_terminal(pid: i32) -> Result<bool, String> {
    Ok(process_table()?
        .into_iter()
        .find(|process| process.pid == pid)
        .is_none_or(|process| process.state.starts_with('Z')))
}

/// Count processes whose current identity and argv establish ownership by `root`.
#[allow(dead_code)]
pub(crate) fn fixture_process_count(root: &Path) -> Result<usize, String> {
    let mut ownership = FixtureOwnership::default();
    Ok(stable_fixture_census(root, &mut ownership)?.processes.len())
}

/// Return process IDs whose current identity and argv establish ownership by `root`.
#[allow(dead_code)]
pub(crate) fn fixture_process_ids(root: &Path) -> Result<Vec<i32>, String> {
    let mut ownership = FixtureOwnership::default();
    Ok(stable_fixture_census(root, &mut ownership)?
        .processes
        .into_iter()
        .map(|process| process.row.pid)
        .collect())
}

/// Return exact fixture monitor PIDs, excluding descendants and the supervisor.
#[allow(dead_code)]
pub(crate) fn fixture_monitor_process_ids(root: &Path) -> Result<Vec<i32>, String> {
    let processes = process_table()?;
    let mut result = Vec::new();
    for process in processes {
        if process.user_id != rustix::process::geteuid().as_raw() {
            continue;
        }
        #[cfg(target_os = "macos")]
        if !OsStr::new(env!("CARGO_BIN_EXE_atx"))
            .as_bytes()
            .starts_with(process.command_name.trim_matches(['(', ')']).as_bytes())
            || !process_executable_is_atx(process.pid)?
        {
            continue;
        }
        #[cfg(target_os = "linux")]
        if Path::new(process.command_name.trim_matches(['(', ')'])).file_name()
            != Some(OsStr::new("atx"))
        {
            continue;
        }
        if let Inspection::Present(command) = process_arguments(process.pid)?
            && is_fixture_command(&command, root, "__monitor")
        {
            result.push(process.pid);
        }
    }
    Ok(result)
}

impl TestRoot {
    pub(crate) fn path(&self) -> &Path {
        self.inner.path()
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        if let Err(error) = cleanup_root(self.path()) {
            if std::thread::panicking() {
                eprintln!("ATX test fixture cleanup failed during unwind: {error}");
            } else {
                panic!("ATX test fixture cleanup failed: {error}");
            }
        }
    }
}

fn cleanup_root(root: &Path) -> Result<(), String> {
    let mut ownership = FixtureOwnership::default();
    let mut runtime_directories = Vec::new();
    discover_runtime_directories(root, &mut runtime_directories)
        .map_err(|error| format!("scan {}: {error}", root.display()))?;
    let _ = fixture_processes(root, &mut ownership)?;
    for runtime in &runtime_directories {
        stop_supervisor(root, runtime)?;
    }
    stop_remaining_processes(root, &mut ownership)
}

fn discover_runtime_directories(
    directory: &Path,
    runtime_directories: &mut Vec<PathBuf>,
) -> io::Result<()> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let mut has_runtime_artifact = false;
    let mut child_directories = Vec::new();
    for entry in entries {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let name = entry.file_name();
        if file_type.is_file() || file_type.is_socket() {
            has_runtime_artifact |= name == "supervisor.lock" || name == "supervisor.sock";
        } else if file_type.is_dir() {
            child_directories.push(entry.path());
        }
    }
    if has_runtime_artifact {
        runtime_directories.push(directory.to_owned());
    }
    for child in child_directories {
        discover_runtime_directories(&child, runtime_directories)?;
    }
    Ok(())
}

fn stop_supervisor(root: &Path, runtime: &Path) -> Result<(), String> {
    let lock = runtime.join("supervisor.lock");
    let Some(identity) = read_identity_if_present(&lock)? else {
        return Ok(());
    };
    if !supervisor_matches(&identity, runtime, None, root)? {
        return Ok(());
    }
    if !identity_is_unchanged(&lock, &identity)? {
        return Ok(());
    }
    signal_process(&identity, Signal::TERM)?;
    if wait_until(TERM_GRACE, || {
        Ok(process_identity_is_terminal(&identity)? && !lock.exists())
    })? {
        return Ok(());
    }

    if !supervisor_matches(&identity, runtime, None, root)? {
        return Ok(());
    }
    if !identity_is_unchanged(&lock, &identity)? {
        return Ok(());
    }
    signal_process(&identity, Signal::KILL)?;
    if wait_until(CLEANUP_TIMEOUT, || process_identity_is_terminal(&identity))? {
        Ok(())
    } else {
        Err(format!("supervisor PID {} did not exit", identity.pid))
    }
}

fn stop_remaining_processes(root: &Path, ownership: &mut FixtureOwnership) -> Result<(), String> {
    let mut processes = Vec::new();
    if stop_processes_with_signal(root, ownership, Signal::TERM, TERM_GRACE, &mut processes)? {
        return Ok(());
    }
    if stop_processes_with_signal(
        root,
        ownership,
        Signal::KILL,
        CLEANUP_TIMEOUT,
        &mut processes,
    )? {
        Ok(())
    } else {
        Err(format!(
            "fixture processes did not exit: {}",
            describe_processes(&processes)
        ))
    }
}

fn stop_processes_with_signal(
    root: &Path,
    ownership: &mut FixtureOwnership,
    signal: Signal,
    timeout: Duration,
    last_processes: &mut Vec<OwnedProcess>,
) -> Result<bool, String> {
    let mut stable_empty = StableEmpty::default();
    wait_until(timeout, || {
        let census = fixture_processes(root, ownership)?;
        *last_processes = census.processes;
        for process in &*last_processes {
            signal_process(&process.identity, signal)?;
        }
        let observation = if !census.complete {
            CensusObservation::Unknown
        } else if last_processes.is_empty() {
            CensusObservation::Empty
        } else {
            CensusObservation::NonEmpty
        };
        Ok(stable_empty.observe(observation))
    })
}

fn stable_fixture_census(
    root: &Path,
    ownership: &mut FixtureOwnership,
) -> Result<ProcessCensus, String> {
    let mut stable_empty = StableEmpty::default();
    let deadline = Instant::now() + CENSUS_TIMEOUT;
    loop {
        let census = fixture_processes(root, ownership)?;
        if !census.processes.is_empty() {
            return Ok(census);
        }
        let observation = if census.complete {
            CensusObservation::Empty
        } else {
            CensusObservation::Unknown
        };
        if stable_empty.observe(observation) {
            return Ok(census);
        }
        if Instant::now() >= deadline {
            return Err("fixture process census did not produce a stable empty result".to_owned());
        }
        std::thread::sleep(CLEANUP_POLL);
    }
}

fn fixture_processes(
    root: &Path,
    ownership: &mut FixtureOwnership,
) -> Result<ProcessCensus, String> {
    let processes = process_table()?;
    let effective_uid = rustix::process::geteuid().as_raw();
    let current_boot = boot_identity()?;
    let mut owned = BTreeSet::new();
    let mut complete = true;
    for process in &processes {
        match is_fixture_atx_process(process, root)? {
            Inspection::Present(true) => {
                match inspect_process_identity(process.pid, &current_boot)? {
                    Inspection::Present(identity) if process.user_id == effective_uid => {
                        ownership.identities.insert(process.pid, identity);
                        owned.insert(process.pid);
                    }
                    Inspection::Transient => complete = false,
                    Inspection::Present(_) | Inspection::Absent => {}
                }
            }
            Inspection::Transient => complete = false,
            Inspection::Present(false) | Inspection::Absent => {}
        }
    }

    for process in &processes {
        let Some(expected) = ownership.identities.get(&process.pid) else {
            continue;
        };
        match classify_live_identity(expected, process.user_id)? {
            IdentityMatch::Same => {
                owned.insert(process.pid);
            }
            IdentityMatch::Unknown => complete = false,
            IdentityMatch::Absent | IdentityMatch::Different | IdentityMatch::WrongUser => {}
        }
    }

    let expanded = expand_owned_descendants(&processes, owned.clone(), effective_uid);
    let new_descendants: Vec<i32> = expanded.difference(&owned).copied().collect();
    for pid in new_descendants {
        match inspect_process_identity(pid, &current_boot)? {
            Inspection::Present(identity) => {
                ownership.identities.insert(pid, identity);
                owned.insert(pid);
            }
            Inspection::Transient => complete = false,
            Inspection::Absent => {}
        }
    }

    let result = processes
        .into_iter()
        .filter(|process| owned.contains(&process.pid))
        .filter_map(|row| {
            ownership
                .identities
                .get(&row.pid)
                .cloned()
                .map(|identity| OwnedProcess { row, identity })
        })
        .collect();
    Ok(ProcessCensus {
        processes: result,
        complete,
    })
}

fn is_fixture_atx_process(process: &ProcessRow, root: &Path) -> Result<Inspection<bool>, String> {
    if process.user_id != rustix::process::geteuid().as_raw() || process.state.starts_with('Z') {
        return Ok(Inspection::Present(false));
    }
    #[cfg(target_os = "linux")]
    if Path::new(process.command_name.trim_matches(['(', ')'])).file_name()
        != Some(OsStr::new("atx"))
    {
        return Ok(Inspection::Present(false));
    }
    #[cfg(target_os = "macos")]
    if !OsStr::new(env!("CARGO_BIN_EXE_atx"))
        .as_bytes()
        .starts_with(process.command_name.trim_matches(['(', ')']).as_bytes())
        || !process_executable_is_atx(process.pid)?
    {
        return Ok(Inspection::Present(false));
    }
    let command = match process_arguments(process.pid)? {
        Inspection::Present(command) => command,
        Inspection::Absent => return Ok(Inspection::Absent),
        Inspection::Transient => return Ok(Inspection::Transient),
    };
    Ok(Inspection::Present(
        is_fixture_command(&command, root, "__supervisor")
            || is_fixture_command(&command, root, "__monitor"),
    ))
}

#[cfg(target_os = "macos")]
fn process_executable_is_atx(pid: i32) -> Result<bool, String> {
    let capacity = usize::try_from(libc::PROC_PIDPATHINFO_MAXSIZE)
        .map_err(|error| format!("invalid proc_pidpath buffer size: {error}"))?;
    let mut bytes = vec![0_u8; capacity];
    // SAFETY: `bytes` owns the writable buffer and its size is passed unchanged.
    let written = unsafe {
        libc::proc_pidpath(
            pid,
            bytes.as_mut_ptr().cast(),
            u32::try_from(bytes.len()).expect("proc_pidpath buffer fits u32"),
        )
    };
    if written <= 0 {
        return match io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH | libc::ENOENT | libc::EINVAL | libc::EPERM | libc::EACCES) | None => {
                Ok(false)
            }
            Some(error) => Err(format!(
                "inspect executable for PID {pid}: OS error {error}"
            )),
        };
    }
    let written = usize::try_from(written)
        .map_err(|error| format!("inspect executable for PID {pid}: {error}"))?;
    let path = bytes
        .get(..written)
        .ok_or_else(|| format!("inspect executable for PID {pid}: oversized result"))?;
    let path = path.strip_suffix(&[0]).unwrap_or(path);
    Ok(OsStr::from_bytes(path) == OsStr::new(env!("CARGO_BIN_EXE_atx")))
}

fn is_fixture_command(command: &[OsString], root: &Path, expected_role: &str) -> bool {
    command
        .first()
        .is_some_and(|executable| executable == OsStr::new(env!("CARGO_BIN_EXE_atx")))
        && command
            .get(1)
            .is_some_and(|role| role == OsStr::new(expected_role))
        && command
            .iter()
            .skip(2)
            .map(Path::new)
            .any(|argument| argument.starts_with(root))
}

fn supervisor_matches(
    identity: &ProcessIdentity,
    runtime: &Path,
    expected_state: Option<&Path>,
    root: &Path,
) -> Result<bool, String> {
    let pid = i32::try_from(identity.pid).map_err(|_| format!("invalid PID {}", identity.pid))?;
    let Some(process) = process_table()?
        .into_iter()
        .find(|process| process.pid == pid)
    else {
        return Ok(false);
    };
    if process.state.starts_with('Z') {
        return Ok(false);
    }
    if process.user_id != rustix::process::geteuid().as_raw()
        || classify_live_identity(identity, process.user_id)? != IdentityMatch::Same
    {
        return Ok(false);
    }
    let command = match process_arguments(process.pid)? {
        Inspection::Present(command) => command,
        Inspection::Absent => return Ok(false),
        Inspection::Transient => {
            return Err(format!(
                "argv inspection for supervisor PID {pid} stayed transient"
            ));
        }
    };
    let state = command_value(&command, "--state-dir");
    Ok(state.is_some_and(|state| {
        state.starts_with(root) && expected_state.is_none_or(|expected| state == expected)
    }) && command_has_pair(&command, "--runtime-dir", runtime)
        && command
            .first()
            .is_some_and(|value| value == OsStr::new(env!("CARGO_BIN_EXE_atx")))
        && command
            .get(1)
            .is_some_and(|value| value == OsStr::new("__supervisor")))
}

fn command_value<'a>(command: &'a [OsString], option: &str) -> Option<&'a Path> {
    command
        .windows(2)
        .find(|pair| pair[0] == OsStr::new(option))
        .map(|pair| Path::new(&pair[1]))
}

fn command_has_pair(command: &[OsString], option: &str, value: &Path) -> bool {
    command_value(command, option).is_some_and(|actual| actual == value)
}

fn read_identity_if_present(lock: &Path) -> Result<Option<ProcessIdentity>, String> {
    match fs::read(lock) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| format!("parse {}: {error}", lock.display())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("read {}: {error}", lock.display())),
    }
}

fn identity_is_unchanged(lock: &Path, expected: &ProcessIdentity) -> Result<bool, String> {
    match read_identity_if_present(lock)? {
        Some(actual) if actual == *expected => Ok(true),
        Some(_) => Err(format!("{} changed before signalling", lock.display())),
        None => Ok(false),
    }
}

fn classify_live_identity(
    expected: &ProcessIdentity,
    process_table_user: u32,
) -> Result<IdentityMatch, String> {
    let current_boot = boot_identity()?;
    if current_boot != expected.boot_identity {
        return Ok(IdentityMatch::Different);
    }
    let current = inspect_process_identity(
        i32::try_from(expected.pid).map_err(|_| format!("invalid PID {}", expected.pid))?,
        &current_boot,
    )?;
    Ok(classify_identity(
        expected,
        rustix::process::geteuid().as_raw(),
        current,
        process_table_user,
    ))
}

#[cfg(target_os = "linux")]
fn boot_identity() -> Result<String, String> {
    let identity = fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map_err(|error| format!("read boot identity: {error}"))?;
    let identity = identity.trim();
    if identity.is_empty() {
        return Err("boot identity is empty".to_owned());
    }
    Ok(identity.to_owned())
}

#[cfg(target_os = "macos")]
fn boot_identity() -> Result<String, String> {
    let mut boot_time = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    let mut size = std::mem::size_of::<libc::timeval>();
    // SAFETY: the output pointer and size describe a live `timeval`.
    let status = unsafe {
        libc::sysctlbyname(
            c"kern.boottime".as_ptr(),
            (&raw mut boot_time).cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0
        || size != std::mem::size_of::<libc::timeval>()
        || boot_time.tv_sec <= 0
        || boot_time.tv_usec < 0
    {
        return Err("read boot identity: kern.boottime unavailable".to_owned());
    }
    Ok(format!("macos:{}:{}", boot_time.tv_sec, boot_time.tv_usec))
}

#[cfg(target_os = "linux")]
fn inspect_process_identity(pid: i32, boot: &str) -> Result<Inspection<ProcessIdentity>, String> {
    let path = PathBuf::from(format!("/proc/{pid}/stat"));
    let stat = match fs::read_to_string(&path) {
        Ok(stat) => stat,
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.raw_os_error() == Some(libc::ESRCH) =>
        {
            return Ok(Inspection::Absent);
        }
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            return Ok(Inspection::Transient);
        }
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    parse_linux_identity(&stat, boot).map(Inspection::Present)
}

#[cfg(target_os = "linux")]
fn parse_linux_identity(stat: &str, boot: &str) -> Result<ProcessIdentity, String> {
    let opening = stat
        .find('(')
        .ok_or_else(|| "process stat has no command opening".to_owned())?;
    let closing = stat
        .rfind(')')
        .ok_or_else(|| "process stat has no command closing".to_owned())?;
    if opening == 0 || closing <= opening {
        return Err("process stat has malformed command".to_owned());
    }
    let pid = stat[..opening]
        .trim()
        .parse::<u32>()
        .map_err(|error| format!("process stat has invalid PID: {error}"))?;
    let mut fields = stat[closing + 1..].split_whitespace();
    let _state = fields
        .next()
        .ok_or_else(|| "process stat has no state".to_owned())?;
    let _parent_pid = fields
        .next()
        .ok_or_else(|| "process stat has no parent PID".to_owned())?;
    let process_group_id = fields
        .next()
        .ok_or_else(|| "process stat has no process group".to_owned())?
        .parse::<i32>()
        .map_err(|error| format!("process stat has invalid process group: {error}"))?;
    let start_token = fields
        .nth(16)
        .ok_or_else(|| "process stat has no start token".to_owned())?
        .parse::<u64>()
        .map_err(|error| format!("process stat has invalid start token: {error}"))?;
    Ok(ProcessIdentity {
        boot_identity: boot.to_owned(),
        pid,
        start_token,
        process_group_id,
    })
}

#[cfg(target_os = "macos")]
fn inspect_process_identity(pid: i32, boot: &str) -> Result<Inspection<ProcessIdentity>, String> {
    retry_inspection(ARGV_INSPECTION_ATTEMPTS, || {
        inspect_process_identity_once(pid, boot)
    })
}

#[cfg(target_os = "macos")]
fn inspect_process_identity_once(
    pid: i32,
    boot: &str,
) -> Result<Inspection<ProcessIdentity>, String> {
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    let size_i32 = i32::try_from(size).map_err(|error| format!("proc_bsdinfo size: {error}"))?;
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    // SAFETY: `info` points to writable storage of exactly the size passed.
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
            Some(libc::ESRCH) | None => Ok(Inspection::Absent),
            Some(libc::EPERM | libc::EACCES | libc::EINVAL) => Ok(Inspection::Transient),
            Some(error) => Err(format!("inspect PID {pid}: OS error {error}")),
        };
    }
    if written != size_i32 {
        return Ok(Inspection::Transient);
    }
    // SAFETY: libproc reported that it initialized the complete structure.
    let info = unsafe { info.assume_init() };
    let start_token = info
        .pbi_start_tvsec
        .checked_mul(1_000_000)
        .and_then(|value| value.checked_add(info.pbi_start_tvusec))
        .ok_or_else(|| format!("inspect PID {pid}: invalid start token"))?;
    Ok(Inspection::Present(ProcessIdentity {
        boot_identity: boot.to_owned(),
        pid: info.pbi_pid,
        start_token,
        process_group_id: i32::try_from(info.pbi_pgid)
            .map_err(|error| format!("inspect PID {pid}: invalid process group: {error}"))?,
    }))
}

#[cfg(target_os = "linux")]
fn process_arguments(pid: i32) -> Result<Inspection<Vec<OsString>>, String> {
    let path = PathBuf::from(format!("/proc/{pid}/cmdline"));
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
            ) =>
        {
            return Ok(if error.kind() == io::ErrorKind::NotFound {
                Inspection::Absent
            } else {
                Inspection::Transient
            });
        }
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    if bytes.is_empty() {
        return Ok(Inspection::Transient);
    }
    Ok(Inspection::Present(parse_nul_arguments(&bytes)))
}

#[cfg(target_os = "linux")]
#[allow(dead_code)]
pub(crate) fn parse_nul_arguments(bytes: &[u8]) -> Vec<OsString> {
    let mut arguments: Vec<OsString> = bytes
        .split(|byte| *byte == 0)
        .map(|argument| OsString::from_vec(argument.to_vec()))
        .collect();
    if bytes.last() == Some(&0) {
        arguments.pop();
    }
    arguments
}

#[cfg(target_os = "macos")]
fn process_arguments(pid: i32) -> Result<Inspection<Vec<OsString>>, String> {
    retry_inspection(ARGV_INSPECTION_ATTEMPTS, || process_arguments_once(pid))
}

#[cfg(target_os = "macos")]
fn process_arguments_once(pid: i32) -> Result<Inspection<Vec<OsString>>, String> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size = 0_usize;
    // SAFETY: both sysctl calls receive valid stack/Vec pointers and lengths.
    let sizing_result = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            u32::try_from(mib.len()).expect("MIB length fits u32"),
            std::ptr::null_mut(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if sizing_result == -1 {
        return sysctl_process_error(pid);
    }
    if size == 0 {
        return Ok(Inspection::Transient);
    }
    let mut bytes = vec![0_u8; size];
    // SAFETY: `bytes` owns `size` writable bytes and the MIB points to this PID.
    let read_result = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            u32::try_from(mib.len()).expect("MIB length fits u32"),
            bytes.as_mut_ptr().cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if read_result == -1 {
        return sysctl_process_error(pid);
    }
    bytes.truncate(size);
    if bytes.len() < std::mem::size_of::<i32>() {
        return Ok(Inspection::Transient);
    }
    let mut argc_bytes = [0_u8; std::mem::size_of::<i32>()];
    argc_bytes.copy_from_slice(&bytes[..std::mem::size_of::<i32>()]);
    let argc = i32::from_ne_bytes(argc_bytes);
    if argc <= 0 {
        return Ok(Inspection::Transient);
    }

    let mut cursor = std::mem::size_of::<i32>();
    cursor = next_nul(&bytes, cursor)
        .ok_or_else(|| format!("KERN_PROCARGS2 for PID {pid} has no executable terminator"))?
        + 1;
    while bytes.get(cursor) == Some(&0) {
        cursor += 1;
    }
    let mut arguments = Vec::with_capacity(usize::try_from(argc).unwrap_or_default());
    for _ in 0..argc {
        let end = next_nul(&bytes, cursor)
            .ok_or_else(|| format!("KERN_PROCARGS2 for PID {pid} has truncated argv"))?;
        arguments.push(OsString::from_vec(bytes[cursor..end].to_vec()));
        cursor = end + 1;
    }
    Ok(Inspection::Present(arguments))
}

#[cfg(target_os = "macos")]
fn next_nul(bytes: &[u8], start: usize) -> Option<usize> {
    bytes
        .get(start..)?
        .iter()
        .position(|byte| *byte == 0)
        .map(|offset| start + offset)
}

#[cfg(target_os = "macos")]
fn sysctl_process_error(pid: i32) -> Result<Inspection<Vec<OsString>>, String> {
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(Inspection::Absent),
        Some(libc::EINVAL | libc::EPERM) => Ok(Inspection::Transient),
        _ => Err(format!("read argv for PID {pid}: {error}")),
    }
}

fn process_table() -> Result<Vec<ProcessRow>, String> {
    let output = Command::new("/bin/ps")
        .args(["-axww", "-o", "pid=,ppid=,pgid=,uid=,state=,comm="])
        .output()
        .map_err(|error| format!("read process table: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "read process table: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let stdout = String::from_utf8(output.stdout)
        .map_err(|error| format!("process table is not UTF-8: {error}"))?;
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(parse_process_row)
        .collect()
}

fn parse_process_row(line: &str) -> Result<ProcessRow, String> {
    let mut fields = line.split_whitespace();
    let pid = parse_process_number(fields.next(), "PID", line)?;
    let parent_pid = parse_process_number(fields.next(), "parent PID", line)?;
    let process_group_id = parse_process_number(fields.next(), "process group", line)?;
    let user_id = parse_process_number(fields.next(), "user ID", line)?;
    let user_id = u32::try_from(user_id)
        .map_err(|error| format!("process row has invalid user ID: {error}: {line:?}"))?;
    let state = fields
        .next()
        .ok_or_else(|| format!("process row has no state: {line:?}"))?
        .to_owned();
    let command_name = fields.collect::<Vec<_>>().join(" ");
    Ok(ProcessRow {
        pid,
        parent_pid,
        process_group_id,
        user_id,
        state,
        command_name,
    })
}

fn parse_process_number(value: Option<&str>, name: &str, line: &str) -> Result<i32, String> {
    value
        .ok_or_else(|| format!("process row has no {name}: {line:?}"))?
        .parse()
        .map_err(|error| format!("process row has invalid {name}: {error}: {line:?}"))
}

#[cfg(target_os = "linux")]
fn signal_process(expected: &ProcessIdentity, signal: Signal) -> Result<(), String> {
    let raw_pid =
        i32::try_from(expected.pid).map_err(|_| format!("invalid PID {}", expected.pid))?;
    let pid = Pid::from_raw(raw_pid).ok_or_else(|| format!("invalid PID {raw_pid}"))?;
    let pidfd = match pidfd_open(pid, PidfdFlags::empty()) {
        Ok(pidfd) => pidfd,
        Err(rustix::io::Errno::SRCH) => return Ok(()),
        Err(error) => return Err(format!("open pidfd for PID {raw_pid}: {error}")),
    };
    let metadata = match fs::metadata(format!("/proc/{raw_pid}")) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("read UID for PID {raw_pid}: {error}")),
    };
    let current_boot = boot_identity()?;
    let current = inspect_process_identity(raw_pid, &current_boot)?;
    if classify_identity(
        expected,
        rustix::process::geteuid().as_raw(),
        current,
        metadata.uid(),
    ) != IdentityMatch::Same
    {
        return Ok(());
    }
    match pidfd_send_signal(pidfd, signal) {
        Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
        Err(error) => Err(format!("send {signal:?} to PID {raw_pid}: {error}")),
    }
}

#[cfg(target_os = "macos")]
fn signal_process(expected: &ProcessIdentity, signal: Signal) -> Result<(), String> {
    let raw_pid =
        i32::try_from(expected.pid).map_err(|_| format!("invalid PID {}", expected.pid))?;
    let Some(process) = process_table()?
        .into_iter()
        .find(|process| process.pid == raw_pid)
    else {
        return Ok(());
    };
    if classify_live_identity(expected, process.user_id)? != IdentityMatch::Same {
        return Ok(());
    }
    let pid = Pid::from_raw(raw_pid).ok_or_else(|| format!("invalid PID {raw_pid}"))?;
    match kill_process(pid, signal) {
        Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
        Err(error) => Err(format!("send {signal:?} to PID {raw_pid}: {error}")),
    }
}

fn process_identity_is_terminal(expected: &ProcessIdentity) -> Result<bool, String> {
    let pid = i32::try_from(expected.pid).map_err(|_| format!("invalid PID {}", expected.pid))?;
    let current_boot = boot_identity()?;
    if current_boot != expected.boot_identity {
        return Ok(true);
    }
    match inspect_process_identity(pid, &current_boot)? {
        Inspection::Present(current) if current == *expected => process_is_terminal(pid),
        Inspection::Present(_) | Inspection::Absent => Ok(true),
        Inspection::Transient => Ok(false),
    }
}

fn describe_processes(processes: &[OwnedProcess]) -> String {
    processes
        .iter()
        .map(|process| {
            let process = &process.row;
            format!(
                "pid={} ppid={} pgid={} uid={} state={} command_name={:?}",
                process.pid,
                process.parent_pid,
                process.process_group_id,
                process.user_id,
                process.state,
                process.command_name
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn wait_until(
    timeout: Duration,
    mut predicate: impl FnMut() -> Result<bool, String>,
) -> Result<bool, String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate()? {
            return Ok(true);
        }
        std::thread::sleep(CLEANUP_POLL);
    }
    predicate()
}
