//! Application services and infrastructure ports.

mod cancel;
mod clock;
mod doctor;
mod history;
mod list;
mod reconcile;
mod service;
mod submit;

pub(crate) use cancel::{
    CancelRunResult, CancellationStore, CancellationStoreError, GroupCancellation,
    ProcessCancellationError, ProcessGroupCanceller, cancel_claimed_run,
};
pub(crate) use clock::{ClockError, ElapsedClock, WallClock};
#[cfg(test)]
pub(crate) use doctor::DiagnosticCheck;
pub(crate) use doctor::{DiagnosticStatus, DoctorReport, DoctorReportBuilder};
#[cfg(test)]
pub(crate) use history::RunStream;
pub(crate) use history::{
    RunOutput, RunOutputError, RunOutputStore, RunOutputStoreError, read_run_output,
};
pub(crate) use list::{
    ManagementError, ManagementStore, ManagementStoreError, list_jobs, list_runs, remove_job,
    rerun_job, resolve_job,
};
#[cfg(test)]
pub(crate) use reconcile::RecoveredDeadline;
pub(crate) use reconcile::{
    CommandFate, IdentityInspectionError, IdentityInspector, IdentityStatus, RecoveryAction,
    RecoveryPlan, RecoveryRecord, RecoveryStore, RecoveryStoreError, reconcile_startup,
};
pub(crate) use service::{
    ServiceAvailability, ServiceChange, ServiceManager, ServiceManagerError, ServiceStatus,
    install_service, uninstall_service,
};
pub(crate) use submit::{
    SubmissionOutcome, SubmissionStore, SubmissionStoreError, SupervisorAckError,
    SupervisorAcknowledger, submit_job,
};
