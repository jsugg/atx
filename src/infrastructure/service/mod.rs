//! Durable service adapters.

mod macos;
mod systemd;

#[allow(unused_imports)]
pub(crate) use macos::LaunchdService;
#[allow(unused_imports)]
pub(crate) use systemd::SystemdUserService;
