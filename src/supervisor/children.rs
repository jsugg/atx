//! Ownership and nonblocking reaping of this supervisor's run monitors.

use std::collections::BTreeMap;
use std::io;
use std::process::{Child, Command};
use std::sync::Mutex;

#[derive(Default)]
pub(super) struct MonitorChildren {
    children: Mutex<BTreeMap<u32, Child>>,
}

impl MonitorChildren {
    /// Register a spawned monitor before its exit notification can be handled.
    pub(super) fn spawn(&self, command: &mut Command) -> io::Result<()> {
        let mut children = self
            .children
            .lock()
            .map_err(|_| io::Error::other("monitor child registry poisoned"))?;
        // Hold the lock across spawn so even an immediate exit cannot precede registration.
        let child = command.spawn()?;
        children.insert(child.id(), child);
        Ok(())
    }

    /// Reap every finished monitor without waiting for still-running commands.
    pub(super) fn reap(&self) -> io::Result<()> {
        let mut children = self
            .children
            .lock()
            .map_err(|_| io::Error::other("monitor child registry poisoned"))?;
        reap_ready(&mut children, next_exited_child, |child| {
            child.try_wait().map(|status| status.is_some())
        })
    }
}

/// Preserve ownership on wait failure and still reap the rest of the batch.
fn reap_ready<T>(
    children: &mut BTreeMap<u32, T>,
    mut ready: impl FnMut() -> io::Result<Option<u32>>,
    mut finished: impl FnMut(&mut T) -> io::Result<bool>,
) -> io::Result<()> {
    while !children.is_empty() {
        let Some(pid) = ready()? else { break };
        let Some(child) = children.get_mut(&pid) else {
            // An unrelated child's owner must consume its status, not this registry.
            return reap_finished(children, finished);
        };
        match finished(child) {
            Ok(true) => {
                children.remove(&pid);
            }
            Ok(false) => return Err(io::Error::other("waitable monitor is still running")),
            Err(error) => {
                let _ = reap_finished(children, finished);
                return Err(error);
            }
        }
    }
    Ok(())
}

/// Inspect an exited child without consuming a status owned by somebody else.
fn next_exited_child() -> io::Result<Option<u32>> {
    loop {
        // SAFETY: zero initializes the output, including si_pid on older Unix versions.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: the output pointer is valid and WNOHANG makes this nonblocking.
        let result = unsafe {
            libc::waitid(
                libc::P_ALL,
                0,
                &raw mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == 0 {
            // SAFETY: waitid initializes the SIGCHLD payload; otherwise it remains zeroed.
            let pid = unsafe { info.si_pid() };
            return if pid == 0 {
                Ok(None)
            } else {
                u32::try_from(pid).map(Some).map_err(io::Error::other)
            };
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ECHILD) {
            return Ok(None);
        }
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn reap_finished<T>(
    children: &mut BTreeMap<u32, T>,
    mut finished: impl FnMut(&mut T) -> io::Result<bool>,
) -> io::Result<()> {
    let mut first_error = None;
    children.retain(|_, child| match finished(child) {
        Ok(true) => false,
        Ok(false) => true,
        Err(error) => {
            first_error.get_or_insert(error);
            true
        }
    });
    first_error.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic)]

    use proptest::prelude::*;
    use std::collections::BTreeMap;
    use std::io;
    use std::process::Command;

    use super::{MonitorChildren, reap_finished, reap_ready};

    struct TestChildren(MonitorChildren);

    impl Drop for TestChildren {
        fn drop(&mut self) {
            let children = self
                .0
                .children
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut failure = None;
            for child in children.values_mut() {
                let stopped = child.kill().or_else(|error| {
                    child
                        .try_wait()
                        .and_then(|status| status.map_or(Err(error), |_| Ok(())))
                });
                let result = stopped.and_then(|()| child.wait().map(|_| ()));
                if let Err(error) = result {
                    failure.get_or_insert(error);
                }
            }
            if let Some(error) = failure {
                if std::thread::panicking() {
                    eprintln!("monitor unit-test cleanup failed: {error}");
                } else {
                    panic!("monitor unit-test cleanup failed: {error}");
                }
            }
        }
    }

    #[test]
    fn failed_wait_keeps_ownership_and_does_not_skip_finished_siblings() {
        let mut children: BTreeMap<_, _> = (1..=4).map(|id| (id, id)).collect();
        let error = reap_finished(&mut children, |child| match child {
            1 => Err(io::Error::other("injected wait failure")),
            2 => Ok(false),
            _ => Ok(true),
        })
        .expect_err("wait failure must be reported");
        assert_eq!(error.to_string(), "injected wait failure");
        assert_eq!(children.keys().copied().collect::<Vec<_>>(), [1, 2]);
        reap_finished(&mut children, |_| Ok(true)).expect("retry reaping");
        assert!(children.is_empty());
    }

    #[test]
    fn failed_spawn_never_registers_a_child() {
        let children = MonitorChildren::default();
        children
            .spawn(&mut Command::new("/nonexistent/atx-test-monitor"))
            .expect_err("spawn failure");
        assert!(children.children.lock().expect("registry").is_empty());
        children.reap().expect("empty reap");
    }

    #[test]
    fn completed_children_are_reaped_without_waiting_for_an_older_live_child() {
        let guard = TestChildren(MonitorChildren::default());
        let children = &guard.0;
        children
            .spawn(Command::new("/bin/sleep").arg("30"))
            .expect("long child");
        children
            .spawn(Command::new("/bin/sh").args(["-c", "exit 0"]))
            .expect("short child");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while children.children.lock().expect("registry").len() != 1 {
            children.reap().expect("reap completed child");
            assert!(
                std::time::Instant::now() < deadline,
                "short child was not reaped"
            );
            std::thread::yield_now();
        }
        let owned = children.children.lock().expect("registry");
        assert_eq!(owned.len(), 1);
    }

    #[test]
    fn poisoned_registry_fails_before_spawning() {
        let children = MonitorChildren::default();
        let _ = std::panic::catch_unwind(|| {
            let _guard = children.children.lock().expect("registry");
            panic!("inject registry failure");
        });
        assert_eq!(
            children
                .spawn(&mut Command::new("/bin/sh"))
                .expect_err("poisoned spawn")
                .to_string(),
            "monitor child registry poisoned"
        );
        assert!(children.reap().is_err());
    }

    #[test]
    fn child_notifications_only_probe_the_reported_monitors() {
        let mut children: BTreeMap<_, _> = (1..=10_000).map(|id| (id, id)).collect();
        let mut notifications = [Some(8), Some(12), None].into_iter();
        let mut probes = Vec::new();
        reap_ready(
            &mut children,
            || Ok(notifications.next().expect("notification")),
            |child| {
                probes.push(*child);
                Ok(true)
            },
        )
        .expect("reap reported children");
        assert_eq!(probes, [8, 12]);
        assert_eq!(children.len(), 9_998);
    }

    #[test]
    fn unrelated_exit_does_not_consume_its_status_or_block_owned_children() {
        let mut children = [(1, false), (2, true)].into_iter().collect();
        reap_ready(&mut children, || Ok(Some(99)), |finished| Ok(*finished)).expect("owned reap");
        assert_eq!(children, [(1, false)].into_iter().collect());
    }

    #[test]
    fn notification_failure_preserves_all_children() {
        let mut children: BTreeMap<_, _> = [(1, true)].into_iter().collect();
        let error = reap_ready(
            &mut children,
            || Err(io::Error::other("notification failed")),
            |done| Ok(*done),
        )
        .expect_err("notification failure");
        assert_eq!(error.to_string(), "notification failed");
        assert_eq!(children.len(), 1);
    }

    proptest! {
        #[test]
        fn reaping_preserves_live_and_failed_children(states in prop::collection::vec(0_u8..3, 1..200)) {
            let mut children: BTreeMap<_, _> = (0_u32..).zip(states.iter().copied()).collect();
            let mut notifications = (0_u32..).zip(&states)
                .filter_map(|(pid, state)| (*state != 0).then_some(pid));
            let result = reap_ready(&mut children, || Ok(notifications.next()), |state| match *state {
                0 => Ok(false),
                1 => Ok(true),
                _ => Err(io::Error::other("injected wait failure")),
            });
            let retained: BTreeMap<_, _> = (0_u32..).zip(states.iter().copied())
                .filter(|(_, state)| *state != 1).collect();
            prop_assert_eq!(children, retained);
            prop_assert_eq!(result.is_err(), states.contains(&2));
        }
    }
}
