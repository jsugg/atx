//! Detached session supervisor.

use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use thiserror::Error;

use super::heap::DeadlineHeap;
use super::ipc::{IpcMessage, RuntimeGuard, read_frame, write_frame};
use super::loop_driver::{SupervisorEvent, reconcile_wall_schedule, run_loop};
use super::recovery::rebuild_deadline_heap;
use crate::application::{ElapsedClock, WallClock, reconcile_startup};
use crate::domain::{ElapsedInstant, JobState, RunOutcome, TransitionActor};
use crate::infrastructure::paths::ensure_private_dir;
use crate::infrastructure::process::NativeProcessInspector;
use crate::infrastructure::sqlite::{Database, JobStore, RetentionPolicy, StartupStore};
use crate::infrastructure::time::NativeClock;

const DATABASE_TIMEOUT: Duration = Duration::from_secs(2);
const CLIENT_TIMEOUT: Duration = Duration::from_secs(2);
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// The signals a service manager uses to stop the unit.
fn stop_signal_set() -> libc::sigset_t {
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    let set_ptr = std::ptr::addr_of_mut!(set);
    // SAFETY: all calls take only stack pointers and no pointer preconditions.
    unsafe {
        libc::sigemptyset(set_ptr);
        libc::sigaddset(set_ptr, libc::SIGTERM);
        libc::sigaddset(set_ptr, libc::SIGINT);
    }
    set
}

/// Startup recovery: open the store, apply operator-configured retention,
/// and reconcile interrupted work into a deadline heap.
fn recover_startup_state(
    store: &mut JobStore,
    state_directory: &Path,
    clock: NativeClock,
    inspector: &NativeProcessInspector,
    boot_identity: &str,
) -> Result<DeadlineHeap, DaemonError> {
    // Operator-configured retention reaches the supervisor only through the
    // shared config boundary; hardcoding defaults here discards settings.
    let config = crate::infrastructure::config::load_process_config(state_directory)
        .map_err(|error| DaemonError::Recovery(error.to_string()))?;
    let retention = RetentionPolicy::new(config.history_days(), config.terminal_job_days())
        .map_err(|error| DaemonError::Recovery(error.to_string()))?;
    let wall_now = clock.now_utc()?;
    let elapsed_now = clock.now_elapsed()?;
    let plan = {
        let mut startup = StartupStore::new(store, retention);
        reconcile_startup(
            &mut startup,
            inspector,
            wall_now,
            elapsed_now,
            boot_identity,
        )
        .map_err(|error| DaemonError::Recovery(error.to_string()))?
    };
    Ok(rebuild_deadline_heap(&plan))
}

pub(crate) fn run_session_supervisor(
    state_directory: &Path,
    runtime_directory: &Path,
    service_managed: bool,
) -> Result<(), DaemonError> {
    // Service managers may hand us directories that do not exist yet; create
    // them with the required private mode instead of failing to open the DB.
    ensure_private_dir(state_directory).map_err(std::io::Error::other)?;
    let database_path = state_directory.join("atx.db");
    let clock = NativeClock;
    let boot_identity = clock.boot_identity()?;
    let inspector = NativeProcessInspector::new(boot_identity.clone());
    let identity = inspector
        .inspect(std::process::id())?
        .ok_or(DaemonError::MissingIdentity)?;
    let runtime = RuntimeGuard::acquire(runtime_directory, &identity, &inspector)?;
    let mut store = JobStore::new(Database::open(&database_path, DATABASE_TIMEOUT)?);
    let mut heap = recover_startup_state(
        &mut store,
        state_directory,
        clock,
        &inspector,
        &boot_identity,
    )?;
    // launchd/systemd stop units with SIGTERM. Block the stop signals before
    // any helper thread inherits the mask, then consume them in a sigwait
    // thread so the loop unwinds cleanly: the IPC thread is joined and the
    // runtime socket removed, instead of dying mid-write on the default
    // disposition.
    let mask = stop_signal_set();
    let mask_ptr = std::ptr::addr_of!(mask);
    // SAFETY: takes only stack pointers and no pointer preconditions.
    unsafe {
        let _ = libc::pthread_sigmask(libc::SIG_SETMASK, mask_ptr.cast(), std::ptr::null_mut());
    }
    let (sender, receiver) = mpsc::channel();
    let signal_sender = sender.clone();
    std::thread::spawn(move || {
        let mut delivered: i32 = 0;
        let delivered_ptr = std::ptr::addr_of_mut!(delivered);
        let mut set = stop_signal_set();
        let set = std::ptr::addr_of_mut!(set);
        // SAFETY: sigwait takes only stack pointers; the stop signals are
        // blocked process-wide above, so no other thread can steal them.
        unsafe {
            libc::sigwait(set, delivered_ptr);
        }
        let _ = signal_sender.send(SupervisorEvent::Shutdown);
    });
    let listener = runtime.listener().try_clone()?;
    let ipc_database = database_path.clone();
    let event_sender = sender.clone();
    let ipc_thread = std::thread::spawn(move || {
        serve_ipc(&listener, &ipc_database, &event_sender);
    });

    let execution_database = database_path;
    let execution_state = state_directory.to_owned();
    let mut retries = DueFailureRetries::default();
    // Fallback clock reading if the elapsed clock fails mid-loop.
    let fallback_elapsed = clock.now_elapsed()?;
    run_loop(
        &receiver,
        &mut heap,
        (!service_managed).then_some(IDLE_TIMEOUT),
        || match clock.now_elapsed() {
            Ok(now) => now,
            Err(error) => {
                eprintln!("atx supervisor: elapsed clock failed: {error}");
                fallback_elapsed
            }
        },
        |jobs| {
            for &job_id in &jobs {
                retries.forget(job_id);
            }
            let failures = execute_due_jobs(
                &execution_database,
                &execution_state,
                runtime_directory,
                &jobs,
            );
            let now_elapsed = clock.now_elapsed().unwrap_or(fallback_elapsed);
            for (job_id, error) in failures {
                match retries.record_failure(job_id) {
                    Some(backoff) => {
                        eprintln!(
                            "atx supervisor: job {job_id} due execution failed \
                             ({backoff}); requeueing with backoff: {error}"
                        );
                        let backoff_nanos =
                            u64::from(backoff.attempt).saturating_mul(2_000_000_000);
                        let deadline = ElapsedInstant::from_nanos(
                            now_elapsed.as_nanos() + u128::from(backoff_nanos),
                        );
                        let _ = sender.send(SupervisorEvent::Schedule { job_id, deadline });
                    }
                    None => eprintln!(
                        "atx supervisor: job {job_id} exhausted its due-execution \
                         retries without executing; leaving it for recovery: {error}"
                    ),
                }
            }
        },
    );

    request_ipc_shutdown(runtime_directory);
    ipc_thread
        .join()
        .map_err(|_| DaemonError::IpcThreadPanicked)?;
    Ok(())
}

fn serve_ipc(
    listener: &std::os::unix::net::UnixListener,
    database_path: &Path,
    sender: &mpsc::Sender<SupervisorEvent>,
) {
    for connection in listener.incoming() {
        let Ok(mut stream) = connection else {
            break;
        };
        let _ = stream.set_read_timeout(Some(CLIENT_TIMEOUT));
        let _ = stream.set_write_timeout(Some(CLIENT_TIMEOUT));
        match read_frame(&mut stream) {
            Ok(IpcMessage::Wake {
                protocol: 1,
                job_id,
                revision,
            }) => {
                // Every Wake gets a reply so clients never read a bare EOF:
                // retry the load briefly to ride out transient database
                // contention, then nack with the reason.
                let reply = match load_deadline_with_retry(database_path, job_id) {
                    Ok(deadline) => {
                        if sender
                            .send(SupervisorEvent::Schedule { job_id, deadline })
                            .is_err()
                        {
                            break;
                        }
                        IpcMessage::Ack {
                            protocol: 1,
                            job_id,
                            revision,
                        }
                    }
                    Err(error) => IpcMessage::Nack {
                        protocol: 1,
                        reason: error.to_string(),
                    },
                };
                let _ = write_frame(&mut stream, &reply);
            }
            Ok(IpcMessage::Shutdown { .. }) => break,
            Ok(IpcMessage::Wake { .. } | IpcMessage::Ack { .. } | IpcMessage::Nack { .. })
            | Err(_) => {}
        }
    }
}

/// Load the deadline, retrying transient database failures so a busy store
/// under load does not turn into an immediate nack.
fn load_deadline_with_retry(
    database_path: &Path,
    job_id: crate::domain::JobId,
) -> Result<crate::domain::ElapsedInstant, DaemonError> {
    const LOAD_ATTEMPTS: usize = 3;
    let mut last_error = DaemonError::MissingJob;
    for attempt in 0..LOAD_ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(50));
        }
        match load_deadline(database_path, job_id) {
            Ok(deadline) => return Ok(deadline),
            // A missing or terminal job is definitive; only retry
            // storage-level faults.
            Err(error @ (DaemonError::MissingJob | DaemonError::RevisionChanged)) => {
                return Err(error);
            }
            Err(error) => last_error = error,
        }
    }
    Err(last_error)
}

/// The wake carries the submitter's revision, but the deadline is always read
/// from the freshly-loaded job: racing the supervisor's own Scheduled→Waiting
/// transition must not nack a valid submission.
fn load_deadline(
    database_path: &Path,
    job_id: crate::domain::JobId,
) -> Result<crate::domain::ElapsedInstant, DaemonError> {
    let store = JobStore::new(Database::open(database_path, DATABASE_TIMEOUT)?);
    let job = store.load(job_id)?.ok_or(DaemonError::MissingJob)?;
    if job.state().is_terminal() {
        return Err(DaemonError::RevisionChanged);
    }
    let clock = NativeClock;
    reconcile_wall_schedule(clock.now_utc()?, clock.now_elapsed()?, job.next_due_utc())
        .map_err(|error| DaemonError::Recovery(error.to_string()))
}

/// Bounded requeue policy for due-batch failures (see `execute_due_jobs`):
/// each failing job is rescheduled with linear backoff up to
/// [`DueFailureRetries::MAX_ATTEMPTS`] times; a job that keeps failing stays
/// nonterminal in `SQLite` and is left to startup reconciliation or the
/// operator.
#[derive(Default)]
struct DueFailureRetries {
    attempts: std::collections::HashMap<crate::domain::JobId, u32>,
}

impl DueFailureRetries {
    const MAX_ATTEMPTS: u32 = 3;

    /// Record one failure for the job; returns the retry attempt with its
    /// linear backoff, or `None` once retries are exhausted.
    fn record_failure(&mut self, job_id: crate::domain::JobId) -> Option<Backoff> {
        let attempt = self
            .attempts
            .entry(job_id)
            .and_modify(|a| *a += 1)
            .or_insert(1);
        if *attempt > Self::MAX_ATTEMPTS {
            return None;
        }
        Some(Backoff {
            attempt: *attempt,
            seconds: u64::from(*attempt) * 2,
        })
    }

    fn forget(&mut self, job_id: crate::domain::JobId) {
        self.attempts.remove(&job_id);
    }
}

struct Backoff {
    attempt: u32,
    seconds: u64,
}

impl std::fmt::Display for Backoff {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "attempt {}/{}, backoff {}s",
            self.attempt,
            DueFailureRetries::MAX_ATTEMPTS,
            self.seconds
        )
    }
}

/// Execute every due job, containing failures per job.
///
/// The loop driver surrenders the whole due batch before this runs, so a
/// single broken job must never abort its batch-mates: each job is driven to
/// completion independently and any failure is reported to the caller for
/// bounded requeueing instead of short-circuiting the loop.
fn execute_due_jobs(
    database_path: &Path,
    state_directory: &Path,
    runtime_directory: &Path,
    jobs: &[crate::domain::JobId],
) -> Vec<(crate::domain::JobId, DaemonError)> {
    let mut failures = Vec::new();
    for &job_id in jobs {
        if let Err(error) =
            execute_single_due_job(database_path, state_directory, runtime_directory, job_id)
        {
            failures.push((job_id, error));
        }
    }
    failures
}

fn execute_single_due_job(
    database_path: &Path,
    state_directory: &Path,
    runtime_directory: &Path,
    job_id: crate::domain::JobId,
) -> Result<(), DaemonError> {
    let clock = NativeClock;
    let mut store = JobStore::new(Database::open(database_path, DATABASE_TIMEOUT)?);
    let Some(mut job) = store.load(job_id)? else {
        return Ok(());
    };
    let now = clock.now_utc()?;
    if job.state() == JobState::Scheduled {
        job = store.transition_job(
            job.id(),
            job.revision(),
            JobState::Waiting,
            false,
            TransitionActor::Supervisor,
            "supervisor loaded deadline",
            now,
        )?;
    }
    if job.state() != JobState::Waiting {
        return Ok(());
    }
    job = store.transition_job(
        job.id(),
        job.revision(),
        JobState::Starting,
        false,
        TransitionActor::Supervisor,
        "deadline became due",
        now,
    )?;
    let run = store.claim_run(job.id(), job.next_due_utc(), now)?;
    job = store.transition_job(
        job.id(),
        job.revision(),
        JobState::Running,
        false,
        TransitionActor::Supervisor,
        "run monitor claimed command",
        now,
    )?;
    if let Err(error) = spawn_run_monitor(state_directory, runtime_directory, job.id(), run.id()) {
        let finished = clock.now_utc()?;
        store.record_run_terminal(
            run.id(),
            run.claim_token(),
            finished,
            RunOutcome::Failure(format!("monitor spawn failed: {error}")),
        )?;
        store.transition_job(
            job.id(),
            job.revision(),
            JobState::Failed,
            false,
            TransitionActor::Supervisor,
            "run monitor could not start",
            finished,
        )?;
    }
    Ok(())
}

fn spawn_run_monitor(
    state_directory: &Path,
    runtime_directory: &Path,
    job_id: crate::domain::JobId,
    run_id: crate::domain::RunId,
) -> Result<(), std::io::Error> {
    use crate::infrastructure::paths::open_or_create_private_append_log;

    // Same secure open as the session-supervisor spawn: no symlink following
    // and strict owner/mode/type validation on any existing log.
    ensure_private_dir(state_directory).map_err(std::io::Error::other)?;
    let state_handle = std::fs::File::open(state_directory)?;
    let log = open_or_create_private_append_log(&state_handle, "supervisor.log")
        .map_err(std::io::Error::other)?;
    let stderr = log.try_clone()?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("__monitor")
        .arg("--state-dir")
        .arg(state_directory)
        .arg("--runtime-dir")
        .arg(runtime_directory)
        .arg("--job")
        .arg(job_id.to_string())
        .arg("--run")
        .arg(run_id.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr));
    // SAFETY: `setsid` has no pointer preconditions and runs in the child just
    // before exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    command.spawn().map(|_| ())
}

fn request_ipc_shutdown(runtime_directory: &Path) {
    if let Ok(mut stream) = UnixStream::connect(runtime_directory.join("supervisor.sock")) {
        let _ = write_frame(&mut stream, &IpcMessage::Shutdown { protocol: 1 });
    }
}

#[derive(Debug, Error)]
pub(crate) enum DaemonError {
    #[error(transparent)]
    Clock(#[from] crate::application::ClockError),
    #[error(transparent)]
    Process(#[from] crate::infrastructure::process::ProcessError),
    #[error(transparent)]
    Store(#[from] crate::infrastructure::sqlite::StoreError),
    #[error(transparent)]
    Ipc(#[from] super::ipc::IpcError),
    #[error("supervisor I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("startup recovery failed: {0}")]
    Recovery(String),
    #[error("supervisor process identity disappeared")]
    MissingIdentity,
    #[error("submitted job was not found")]
    MissingJob,
    #[error("submitted job already reached a terminal state")]
    RevisionChanged,
    #[error("IPC thread panicked")]
    IpcThreadPanicked,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic)]

    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::mpsc;
    use std::time::Duration;

    use tempfile::tempdir;

    use super::{
        DaemonError, execute_due_jobs, load_deadline, load_deadline_with_retry,
        request_ipc_shutdown, serve_ipc, stop_signal_set,
    };
    use crate::domain::{
        DurationSeconds, Environment, ExecutionMode, ExecutionSpec, Job, JobId, JobState,
        MissedPolicy, Revision, RuntimeTier, Schedule, TransitionActor, UtcTimestamp,
    };
    use crate::infrastructure::sqlite::{Database, JobStore};
    use crate::supervisor::ipc::{IpcMessage, read_frame, write_frame};
    use crate::supervisor::loop_driver::SupervisorEvent;

    fn store_in(root: &std::path::Path) -> JobStore {
        let database =
            Database::open(&root.join("atx.db"), Duration::from_secs(2)).expect("open database");
        JobStore::new(database)
    }

    fn scheduled_job(now_sec: i64, due_sec: i64) -> Job {
        let now = UtcTimestamp::from_second(now_sec).expect("valid now");
        let due = UtcTimestamp::from_second(due_sec).expect("valid due");
        let schedule =
            Schedule::one_shot_relative(DurationSeconds::new(30).expect("duration"), due);
        let execution = ExecutionSpec::new(
            ExecutionMode::Direct,
            vec!["/bin/true".to_owned()],
            "/tmp".to_owned(),
            Environment::from_pairs([("ATX_TEST", "value")]).expect("environment"),
        )
        .expect("execution");
        Job::new(
            now,
            schedule,
            MissedPolicy::Hold,
            RuntimeTier::Session,
            execution,
            501,
        )
        .expect("job")
    }

    #[test]
    fn stop_signal_set_contains_term_and_int() {
        let set = stop_signal_set();
        let set_ptr = std::ptr::addr_of!(set);
        // SAFETY: sigismember takes only a stack pointer; `set` is a local.
        unsafe {
            assert_eq!(libc::sigismember(set_ptr, libc::SIGTERM), 1);
            assert_eq!(libc::sigismember(set_ptr, libc::SIGINT), 1);
        }
    }

    #[test]
    fn load_deadline_returns_deadline_for_live_job() {
        let root = tempdir().expect("root");
        let mut store = store_in(root.path());
        let job = scheduled_job(1000, 1030);
        store.create(&job).expect("create");
        let db_path = root.path().join("atx.db");
        load_deadline(&db_path, job.id()).expect("deadline");
    }

    #[test]
    fn load_deadline_reports_missing_job() {
        let root = tempdir().expect("root");
        let _store = store_in(root.path());
        let db_path = root.path().join("atx.db");
        let error = load_deadline(&db_path, JobId::new()).expect_err("missing job");
        assert!(matches!(error, DaemonError::MissingJob));
    }

    #[test]
    fn load_deadline_accepts_job_after_revision_advanced() {
        // The supervisor's own Scheduled→Waiting transition may race the wake;
        // the deadline must still load from the freshly-read job.
        let root = tempdir().expect("root");
        let mut store = store_in(root.path());
        let job = scheduled_job(1000, 1030);
        store.create(&job).expect("create");
        let advanced = store
            .transition_job(
                job.id(),
                job.revision(),
                JobState::Waiting,
                false,
                TransitionActor::Supervisor,
                "supervisor loaded deadline",
                UtcTimestamp::from_second(1001).expect("now"),
            )
            .expect("transition");
        assert_ne!(advanced.revision(), job.revision());
        let db_path = root.path().join("atx.db");
        // The deadline maps to the live elapsed clock; assert only that it
        // loaded rather than nacked.
        let deadline = load_deadline(&db_path, job.id()).expect("deadline after revision advance");
        assert!(deadline.as_nanos() > 0);
    }

    #[test]
    fn load_deadline_rejects_terminal_job() {
        let root = tempdir().expect("root");
        let mut store = store_in(root.path());
        let job = scheduled_job(1000, 1030);
        store.create(&job).expect("create");
        let now = UtcTimestamp::from_second(1001).expect("now");
        store
            .transition_job(
                job.id(),
                job.revision(),
                JobState::Missed,
                false,
                TransitionActor::Supervisor,
                "terminal",
                now,
            )
            .expect("mark terminal");
        let db_path = root.path().join("atx.db");
        let error = load_deadline(&db_path, job.id()).expect_err("terminal job");
        assert!(matches!(error, DaemonError::RevisionChanged));
    }

    #[test]
    fn storage_faults_exhaust_the_wake_load_retries() {
        let root = tempdir().expect("root");
        let _store = store_in(root.path());
        let db_path = root.path().join("atx.db");
        // A directory at the database path makes every open fail with a
        // storage-level fault; only such faults may exhaust the retries.
        fs::remove_file(&db_path).expect("drop fresh database");
        fs::create_dir(&db_path).expect("block the database path");

        // A storage-level fault retries to exhaustion; only the definitive
        // outcomes (missing job, changed revision) return immediately.
        let error =
            load_deadline_with_retry(&db_path, JobId::new()).expect_err("corrupt store fails");
        assert!(!matches!(
            error,
            DaemonError::MissingJob | DaemonError::RevisionChanged
        ));
    }

    #[test]
    fn serve_ipc_acknowledges_wake_and_loads_deadline() {
        let root = tempdir().expect("root");
        let mut store = store_in(root.path());
        let job = scheduled_job(1000, 1030);
        store.create(&job).expect("create");
        let socket = root.path().join("ipc.sock");
        let listener = UnixListener::bind(&socket).expect("listener");
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).expect("mode");
        let db_path = root.path().join("atx.db");
        let (sender, receiver) = mpsc::channel();
        let thread_sender = sender.clone();
        let db_clone = db_path.clone();
        let server = std::thread::spawn(move || {
            serve_ipc(&listener, &db_clone, &thread_sender);
        });

        let mut client = UnixStream::connect(&socket).expect("connect");
        write_frame(
            &mut client,
            &IpcMessage::Wake {
                protocol: 1,
                job_id: job.id(),
                revision: job.revision(),
            },
        )
        .expect("write wake");
        match receiver.recv().expect("event") {
            SupervisorEvent::Schedule { job_id, .. } => assert_eq!(job_id, job.id()),
            SupervisorEvent::Shutdown => panic!("unexpected shutdown event"),
        }
        let ack = read_frame(&mut client).expect("ack");
        assert_eq!(
            ack,
            IpcMessage::Ack {
                protocol: 1,
                job_id: job.id(),
                revision: job.revision(),
            }
        );
        drop(client);

        let mut shutdown = UnixStream::connect(&socket).expect("connect shutdown");
        write_frame(&mut shutdown, &IpcMessage::Shutdown { protocol: 1 }).expect("write shutdown");
        server.join().expect("join server");
    }

    #[test]
    fn serve_ipc_ignores_unexpected_frames_and_stops_on_shutdown() {
        let root = tempdir().expect("root");
        let mut store = store_in(root.path());
        let job = scheduled_job(1000, 1030);
        store.create(&job).expect("create");
        let socket = root.path().join("ipc.sock");
        let listener = UnixListener::bind(&socket).expect("listener");
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).expect("mode");
        let db_path = root.path().join("atx.db");
        let (sender, receiver) = mpsc::channel();
        let thread_sender = sender.clone();
        let db_clone = db_path.clone();
        let server = std::thread::spawn(move || {
            serve_ipc(&listener, &db_clone, &thread_sender);
        });

        let mut wrong_protocol = UnixStream::connect(&socket).expect("connect 1");
        write_frame(
            &mut wrong_protocol,
            &IpcMessage::Wake {
                protocol: 2,
                job_id: job.id(),
                revision: job.revision(),
            },
        )
        .expect("write wake proto 2");
        drop(wrong_protocol);

        let mut ack_frame = UnixStream::connect(&socket).expect("connect 2");
        write_frame(
            &mut ack_frame,
            &IpcMessage::Ack {
                protocol: 1,
                job_id: job.id(),
                revision: job.revision(),
            },
        )
        .expect("write ack");
        drop(ack_frame);

        let mut broken = UnixStream::connect(&socket).expect("connect 3");
        broken
            .write_all(&[0, 0, 0, 2, b'{', b'}'])
            .expect("write malformed");
        drop(broken);

        let mut shutdown = UnixStream::connect(&socket).expect("connect 4");
        write_frame(&mut shutdown, &IpcMessage::Shutdown { protocol: 1 }).expect("write shutdown");
        drop(shutdown);

        server.join().expect("join server");
        assert!(
            receiver.try_recv().is_err(),
            "no events should have been emitted"
        );
    }

    #[test]
    fn serve_ipc_breaks_when_event_sink_is_gone() {
        let root = tempdir().expect("root");
        let mut store = store_in(root.path());
        let job = scheduled_job(1000, 1030);
        store.create(&job).expect("create");
        let socket = root.path().join("ipc.sock");
        let listener = UnixListener::bind(&socket).expect("listener");
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).expect("mode");
        let db_path = root.path().join("atx.db");
        let (sender, receiver) = mpsc::channel();
        drop(receiver);
        let db_clone = db_path.clone();
        let server = std::thread::spawn(move || {
            serve_ipc(&listener, &db_clone, &sender);
        });

        let mut client = UnixStream::connect(&socket).expect("connect");
        write_frame(
            &mut client,
            &IpcMessage::Wake {
                protocol: 1,
                job_id: job.id(),
                revision: job.revision(),
            },
        )
        .expect("write wake");
        server.join().expect("join server");
    }

    #[test]
    fn serve_ipc_answers_unknown_job_with_a_nack() {
        let root = tempdir().expect("root");
        // Schema must exist so the load fails on the missing row, not setup.
        let _store = store_in(root.path());
        let socket = root.path().join("ipc.sock");
        let listener = UnixListener::bind(&socket).expect("listener");
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).expect("mode");
        // No store rows at all: the load fails and the client must receive an
        // explicit nack instead of a silent connection close (EOF).
        let db_clone = root.path().join("atx.db");
        let (sender, _receiver) = mpsc::channel();
        let server = std::thread::spawn(move || {
            serve_ipc(&listener, &db_clone, &sender);
        });

        let mut client = UnixStream::connect(&socket).expect("connect");
        write_frame(
            &mut client,
            &IpcMessage::Wake {
                protocol: 1,
                job_id: JobId::new(),
                revision: Revision::new(1).expect("revision"),
            },
        )
        .expect("write wake");
        let nack = read_frame(&mut client).expect("nack reply, not EOF");
        assert!(
            matches!(&nack, IpcMessage::Nack { protocol: 1, .. }),
            "expected a nack, got: {nack:?}"
        );
        drop(client);
        // The server only exits on a Shutdown frame; send it on a fresh
        // connection like real clients do.
        let mut shutdown = UnixStream::connect(&socket).expect("connect shutdown");
        write_frame(&mut shutdown, &IpcMessage::Shutdown { protocol: 1 }).expect("shutdown");
        server.join().expect("join server");
    }

    #[test]
    fn serve_ipc_stops_when_the_listener_fails() {
        use std::os::unix::io::{FromRawFd, IntoRawFd};

        let root = tempdir().expect("root");
        let _store = store_in(root.path());
        // A connected stream wrapped as a listener is never in listening
        // state: accept fails immediately (EINVAL), which must end the IPC
        // thread instead of spinning on a broken runtime socket.
        let (stream, _peer) = UnixStream::pair().expect("socket pair");
        let bogus_listener = unsafe { UnixListener::from_raw_fd(stream.into_raw_fd()) };
        let (sender, receiver) = mpsc::channel();
        drop(receiver);
        let db_clone = root.path().join("atx.db");
        let server = std::thread::spawn(move || serve_ipc(&bogus_listener, &db_clone, &sender));
        server.join().expect("server must stop when accept fails");
    }

    #[test]
    fn request_ipc_shutdown_writes_shutdown_frame_to_runtime_socket() {
        let root = tempdir().expect("root");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).expect("mode");
        let socket = root.path().join("supervisor.sock");
        let listener = UnixListener::bind(&socket).expect("listener");
        request_ipc_shutdown(root.path());
        let (mut stream, _) = listener.accept().expect("accept");
        let message = read_frame(&mut stream).expect("read frame");
        assert_eq!(message, IpcMessage::Shutdown { protocol: 1 });
    }

    #[test]
    fn request_ipc_shutdown_is_a_noop_without_socket() {
        let root = tempdir().expect("root");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).expect("mode");
        request_ipc_shutdown(root.path());
    }

    #[test]
    fn execute_due_jobs_skips_missing_and_terminal_jobs() {
        let root = tempdir().expect("root");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).expect("mode");
        let runtime = root.path().join("runtime");
        fs::create_dir_all(&runtime).expect("runtime dir");
        let mut store = store_in(root.path());
        let job = scheduled_job(1000, 1030);
        store.create(&job).expect("create");
        let now = UtcTimestamp::from_second(1001).expect("now");
        let terminal = store
            .transition_job(
                job.id(),
                job.revision(),
                JobState::Missed,
                false,
                TransitionActor::Supervisor,
                "terminal",
                now,
            )
            .expect("mark terminal");
        let db_path = root.path().join("atx.db");
        let failures = execute_due_jobs(
            &db_path,
            root.path(),
            &runtime,
            &[JobId::new(), terminal.id()],
        );
        assert!(failures.is_empty(), "{failures:?}");
    }

    #[test]
    fn execute_due_jobs_handles_empty_schedule() {
        let root = tempdir().expect("root");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).expect("mode");
        let runtime = root.path().join("runtime");
        fs::create_dir_all(&runtime).expect("runtime dir");
        let _store = store_in(root.path());
        let db_path = root.path().join("atx.db");
        let failures = execute_due_jobs(&db_path, root.path(), &runtime, &[]);
        assert!(failures.is_empty());
    }

    #[test]
    fn a_failing_run_monitor_spawn_records_a_failed_terminal_run() {
        let root = tempdir().expect("root");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).expect("mode");
        let runtime = root.path().join("runtime");
        fs::create_dir_all(&runtime).expect("runtime dir");
        let mut store = store_in(root.path());
        let job = scheduled_job(1000, 1030);
        store.create(&job).expect("create");
        // The monitor spawns `current_exe __monitor`; a state directory whose
        // supervisor.log path is a directory makes the spawn's log open fail,
        // exercising the in-loop failure recording without touching main.rs.
        fs::create_dir(root.path().join("supervisor.log")).expect("block the log path");

        let now = UtcTimestamp::from_second(1001).expect("now");
        let waiting = store
            .transition_job(
                job.id(),
                job.revision(),
                JobState::Waiting,
                false,
                TransitionActor::Supervisor,
                "supervisor loaded deadline",
                now,
            )
            .expect("mark waiting");

        // The failure arm runs inside run_loop's due-callback; drive it
        // directly with one due job.
        let db_path = root.path().join("atx.db");
        let failures = execute_due_jobs(&db_path, root.path(), &runtime, &[job.id()]);
        assert!(
            failures.is_empty(),
            "spawn failures are contained: {failures:?}"
        );

        let mut statement = store
            .database()
            .connection()
            .prepare("SELECT state, COALESCE(failure, '') FROM runs WHERE job_id = ?1")
            .expect("prepare run query");
        let rows: Vec<(String, String)> = statement
            .query_map([waiting.id().to_string()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .expect("query runs")
            .map(|row| row.expect("row"))
            .collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "failed");
        assert!(!rows[0].1.is_empty(), "failure evidence must be persisted");
        assert_eq!(
            store.load(job.id()).expect("reload").expect("job").state(),
            JobState::Failed
        );
    }

    /// Regression: one broken due job must not drop its batch-mates. The loop
    /// driver surrenders the whole due batch before execution, so an early
    /// `?` used to discard every later job until the next supervisor restart.
    #[test]
    fn a_failing_due_job_does_not_drop_the_rest_of_its_batch() {
        let root = tempdir().expect("root");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).expect("mode");
        let runtime = root.path().join("runtime");
        fs::create_dir_all(&runtime).expect("runtime dir");
        // Block the monitor spawn log path so the valid jobs fail
        // deterministically at spawn (recorded terminal runs) instead of
        // launching real monitors from the test process.
        fs::create_dir(root.path().join("supervisor.log")).expect("block the log path");

        let mut store = store_in(root.path());
        let corrupt = scheduled_job(1000, 1030);
        let healthy_a = scheduled_job(1000, 1030);
        let healthy_b = scheduled_job(1000, 1030);
        for job in [&corrupt, &healthy_a, &healthy_b] {
            store.create(job).expect("create job");
        }
        // Corrupt the first job past deserialization: valid JSON (the
        // schema enforces json_valid) with an unknown mode so decode fails.
        store
            .database()
            .connection()
            .execute(
                "UPDATE jobs SET execution_json = '{\"mode\": \"nonexistent\"}' WHERE id = ?1",
                [corrupt.id().to_string()],
            )
            .expect("corrupt first job");

        let db_path = root.path().join("atx.db");
        let mut failures = execute_due_jobs(
            &db_path,
            root.path(),
            &runtime,
            &[corrupt.id(), healthy_a.id(), healthy_b.id()],
        );
        assert_eq!(
            failures.len(),
            1,
            "only the corrupt job fails: {failures:?}"
        );
        assert_eq!(failures.remove(0).0, corrupt.id());

        for job in [&healthy_a, &healthy_b] {
            let reloaded = store
                .load(job.id())
                .expect("load")
                .unwrap_or_else(|| panic!("job {} vanished", job.id()));
            assert_eq!(reloaded.state(), JobState::Failed, "{}", job.id());
            let mut statement = store
                .database()
                .connection()
                .prepare("SELECT COUNT(*) FROM runs WHERE job_id = ?1")
                .expect("prepare run count");
            let runs: i64 = statement
                .query_row([job.id().to_string()], |row| row.get(0))
                .expect("count runs");
            assert_eq!(runs, 1, "job {} must execute exactly once", job.id());
        }
    }
}
