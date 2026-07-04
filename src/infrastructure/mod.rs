//! Operating-system and persistence adapters.

// Adapters are built before CLI/application wiring.
#[allow(dead_code)]
pub(crate) mod config;
pub(crate) mod paths;
pub(crate) mod process;
pub(crate) mod runtime;
pub(crate) mod service;
pub(crate) mod sqlite;
pub(crate) mod time;
