//! Operating-system and persistence adapters.

// Adapters are built before CLI/application wiring.
#[allow(dead_code)]
pub(crate) mod config;
#[allow(dead_code)]
pub(crate) mod paths;
#[allow(dead_code)]
pub(crate) mod process;
#[allow(dead_code)]
pub(crate) mod runtime;
#[allow(dead_code)]
pub(crate) mod service;
#[allow(dead_code)]
pub(crate) mod sqlite;
#[allow(dead_code)]
pub(crate) mod time;
