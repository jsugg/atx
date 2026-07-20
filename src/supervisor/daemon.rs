//! Detached session supervisor.

use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use thiserror::Error;

use super::ipc::{IpcMessage, RuntimeGuard, read_frame, write_frame};
use super::loop_driver::{SupervisorEvent, reconcile_wall_schedule, run_loop};
use super::recovery::rebuild_deadline_heap;
use crate::application::{ElapsedClock, WallClock, reconcile_startup};
use crate::domain::{JobState, RunState, TransitionActor};
use crate::infrastructure::process::{NativeProcessInspector, NativeProcessRunner};
use crate::infrastructure::sqlite::{Database, JobStore, RetentionPolicy, StartupStore};
use crate::infrastructure::time::NativeClock;
use crate::run_monitor::RunMonitor;

const DATABASE_TIMEOUT: Duration = Duration::from_secs(2);
const CLIENT_TIMEOUT: Duration = Duration::from_secs(2);
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_MAX_LOG_BYTES: usize = 10 * 1024 * 1024;

pub(crate) fn run_session_supervisor(
    state_directory: &Path,
    runtime_directory: &Path,
    service_managed: bool,
) -> Result<(), DaemonError> {
    let database_path = state_directory.join("atx.db");
    let clock = NativeClock;
    let boot_identity = clock.boot_identity()?;
    let inspector = NativeProcessInspector::new(boot_identity.clone());
    let identity = inspector
        .inspect(std::process::id())?
        .ok_or(DaemonError::MissingIdentity)?;
    let runtime = RuntimeGuard::acquire(runtime_directory, &identity, &inspector)?;
    let mut store = JobStore::new(Database::open(&database_path, DATABASE_TIMEOUT)?);
    let retention =
        RetentionPolicy::new(30, 30).map_err(|error| DaemonError::Recovery(error.to_string()))?;
    let wall_now = clock.now_utc()?;
    let elapsed_now = clock.now_elapsed()?;
    let plan = {
        let mut startup = StartupStore::new(&mut store, retention);
        reconcile_startup(
            &mut startup,
            &inspector,
            wall_now,
            elapsed_now,
            &boot_identity,
        )
        .map_err(|error| DaemonError::Recovery(error.to_string()))?
    };
    let mut heap = rebuild_deadline_heap(&plan);
    let (sender, receiver) = mpsc::channel();
    let listener = runtime.listener().try_clone()?;
    let ipc_database = database_path.clone();
    let event_sender = sender.clone();
    let ipc_thread = std::thread::spawn(move || {
        serve_ipc(&listener, &ipc_database, &event_sender);
    });

    let execution_database = database_path;
    let execution_state = state_directory.to_owned();
    run_loop(
        &receiver,
        &mut heap,
        (!service_managed).then_some(IDLE_TIMEOUT),
        || match clock.now_elapsed() {
            Ok(now) => now,
            Err(error) => {
                eprintln!("atx supervisor: elapsed clock failed: {error}");
                elapsed_now
            }
        },
        |jobs| match execute_due_jobs(&execution_database, &execution_state, &jobs) {
            Ok(deadlines) => {
                for (job_id, deadline) in deadlines {
                    if sender
                        .send(SupervisorEvent::Schedule { job_id, deadline })
                        .is_err()
                    {
                        break;
                    }
                }
            }
            Err(error) => eprintln!("atx supervisor: {error}"),
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
                let loaded = load_deadline(database_path, job_id, revision);
                if let Ok(deadline) = loaded {
                    if sender
                        .send(SupervisorEvent::Schedule { job_id, deadline })
                        .is_err()
                    {
                        break;
                    }
                    let _ = write_frame(
                        &mut stream,
                        &IpcMessage::Ack {
                            protocol: 1,
                            job_id,
                            revision,
                        },
                    );
                }
            }
            Ok(IpcMessage::Shutdown { .. }) => break,
            Ok(IpcMessage::Wake { .. } | IpcMessage::Ack { .. }) | Err(_) => {}
        }
    }
}

fn load_deadline(
    database_path: &Path,
    job_id: crate::domain::JobId,
    revision: crate::domain::Revision,
) -> Result<crate::domain::ElapsedInstant, DaemonError> {
    let store = JobStore::new(Database::open(database_path, DATABASE_TIMEOUT)?);
    let job = store.load(job_id)?.ok_or(DaemonError::MissingJob)?;
    if job.revision() != revision || job.state().is_terminal() {
        return Err(DaemonError::RevisionChanged);
    }
    let clock = NativeClock;
    reconcile_wall_schedule(clock.now_utc()?, clock.now_elapsed()?, job.next_due_utc())
        .map_err(|error| DaemonError::Recovery(error.to_string()))
}

fn execute_due_jobs(
    database_path: &Path,
    state_directory: &Path,
    jobs: &[crate::domain::JobId],
) -> Result<Vec<(crate::domain::JobId, crate::domain::ElapsedInstant)>, DaemonError> {
    let clock = NativeClock;
    let boot_identity = clock.boot_identity()?;
    let mut recurring_deadlines = Vec::new();
    for &job_id in jobs {
        let mut store = JobStore::new(Database::open(database_path, DATABASE_TIMEOUT)?);
        let Some(mut job) = store.load(job_id)? else {
            continue;
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
            continue;
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
        let inspector = NativeProcessInspector::new(boot_identity.clone());
        let runner = NativeProcessRunner::new(inspector.clone());
        let completed = RunMonitor::new(
            &mut store,
            runner,
            inspector,
            clock,
            state_directory,
            DEFAULT_MAX_LOG_BYTES,
        )
        .execute(&run, job.execution());
        let (target, reason) = match completed {
            Ok(run) => match run.state() {
                RunState::Succeeded => (JobState::Succeeded, "command exited successfully"),
                RunState::Cancelled => (JobState::Cancelled, "command was cancelled"),
                RunState::Failed => (JobState::Failed, "command failed"),
                RunState::Interrupted => (JobState::Interrupted, "command outcome is unknown"),
                _ => (
                    JobState::Interrupted,
                    "run monitor returned a nonterminal state",
                ),
            },
            Err(_) => (JobState::Interrupted, "run monitor failed"),
        };
        if matches!(
            job.schedule(),
            crate::domain::Schedule::RecurringInterval { .. }
        ) && target != JobState::Cancelled
        {
            let now = clock.now_utc()?;
            let recurring = store.advance_recurring_job(job.id(), job.revision(), now)?;
            let deadline =
                reconcile_wall_schedule(now, clock.now_elapsed()?, recurring.next_due_utc())
                    .map_err(|error| DaemonError::Recovery(error.to_string()))?;
            recurring_deadlines.push((recurring.id(), deadline));
            continue;
        }
        store.transition_job(
            job.id(),
            job.revision(),
            target,
            false,
            TransitionActor::Monitor,
            reason,
            clock.now_utc()?,
        )?;
    }
    Ok(recurring_deadlines)
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
    #[error("submitted job revision changed before acknowledgement")]
    RevisionChanged,
    #[error("IPC thread panicked")]
    IpcThreadPanicked,
}
