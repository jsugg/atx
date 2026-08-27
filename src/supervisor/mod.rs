//! Single-instance scheduling supervisor.

mod daemon;
mod frame;
mod heap;
mod ipc;
mod loop_driver;
mod recovery;

pub(crate) use daemon::run_session_supervisor;
pub(crate) use ipc::SocketAcknowledger;
