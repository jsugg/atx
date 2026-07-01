//! Platform-independent scheduling domain.

// Domain contracts are built before their application services are wired.
#[allow(dead_code)]
mod error;
mod execution;
#[allow(dead_code)]
mod id;
mod job;
#[allow(dead_code)]
mod primitives;
mod run;
mod schedule;
mod state;
mod transition;
