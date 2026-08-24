//! Detached session supervisor.

use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use thiserror::Error;

use super::ipc::{IpcMessage, RuntimeGuard, read_frame, write_frame};
use super::loop_driver::{SupervisorEvent, reconcile_wall_schedule, run_loop};
use super::recovery::rebuild_deadline_heap;
use crate::application::{ElapsedClock, WallClock, reconcile_startup};
use crate::domain::{JobState, RunOutcome, TransitionActor};
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
        |jobs| {
            if let Err(error) = execute_due_jobs(
                &execution_database,
                &execution_state,
                runtime_directory,
                &jobs,
            ) {
                eprintln!("atx supervisor: {error}");
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
    runtime_directory: &Path,
    jobs: &[crate::domain::JobId],
) -> Result<(), DaemonError> {
    let clock = NativeClock;
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
        if let Err(error) =
            spawn_run_monitor(state_directory, runtime_directory, job.id(), run.id())
        {
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
    }
    Ok(())
}

fn spawn_run_monitor(
    state_directory: &Path,
    runtime_directory: &Path,
    job_id: crate::domain::JobId,
    run_id: crate::domain::RunId,
) -> Result<(), std::io::Error> {
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(state_directory.join("supervisor.log"))?;
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
    #[error("submitted job revision changed before acknowledgement")]
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
        DaemonError, execute_due_jobs, load_deadline, request_ipc_shutdown, serve_ipc,
        stop_signal_set,
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
        load_deadline(&db_path, job.id(), job.revision()).expect("deadline");
    }

    #[test]
    fn load_deadline_reports_missing_job() {
        let root = tempdir().expect("root");
        let _store = store_in(root.path());
        let db_path = root.path().join("atx.db");
        let error = load_deadline(&db_path, JobId::new(), Revision::new(1).expect("revision"))
            .expect_err("missing job");
        assert!(matches!(error, DaemonError::MissingJob));
    }

    #[test]
    fn load_deadline_reports_wrong_revision() {
        let root = tempdir().expect("root");
        let mut store = store_in(root.path());
        let job = scheduled_job(1000, 1030);
        store.create(&job).expect("create");
        let db_path = root.path().join("atx.db");
        let error = load_deadline(&db_path, job.id(), Revision::new(99).expect("revision"))
            .expect_err("revision changed");
        assert!(matches!(error, DaemonError::RevisionChanged));
    }

    #[test]
    fn load_deadline_rejects_terminal_job() {
        let root = tempdir().expect("root");
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
        let error =
            load_deadline(&db_path, job.id(), terminal.revision()).expect_err("terminal job");
        assert!(matches!(error, DaemonError::RevisionChanged));
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
            _ => panic!("unexpected event variant"),
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
        let result = execute_due_jobs(
            &db_path,
            root.path(),
            &runtime,
            &[JobId::new(), terminal.id()],
        );
        assert!(result.is_ok());
    }

    #[test]
    fn execute_due_jobs_handles_empty_schedule() {
        let root = tempdir().expect("root");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).expect("mode");
        let runtime = root.path().join("runtime");
        fs::create_dir_all(&runtime).expect("runtime dir");
        let _store = store_in(root.path());
        let db_path = root.path().join("atx.db");
        let result = execute_due_jobs(&db_path, root.path(), &runtime, &[]);
        assert!(result.is_ok());
    }
}
