//! Native clock and timezone adapters.

use crate::application::{ClockError, ElapsedClock, WallClock};
use crate::domain::{ElapsedInstant, UtcTimestamp};

#[cfg(any(target_os = "linux", test))]
const NANOS_PER_SECOND: u128 = 1_000_000_000;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct NativeClock;

impl WallClock for NativeClock {
    fn now_utc(&self) -> Result<UtcTimestamp, ClockError> {
        Ok(UtcTimestamp::from_jiff(jiff::Timestamp::now()))
    }
}

impl ElapsedClock for NativeClock {
    fn now_elapsed(&self) -> Result<ElapsedInstant, ClockError> {
        platform_elapsed()
    }

    fn boot_identity(&self) -> Result<String, ClockError> {
        platform_boot_identity()
    }
}

#[cfg(any(target_os = "linux", test))]
fn elapsed_from_parts(seconds: i64, nanoseconds: i64) -> Result<ElapsedInstant, ClockError> {
    if seconds < 0 || !(0..1_000_000_000).contains(&nanoseconds) {
        return Err(ClockError::OutOfRange);
    }
    let seconds = u128::try_from(seconds).map_err(|_| ClockError::OutOfRange)?;
    let nanoseconds = u128::try_from(nanoseconds).map_err(|_| ClockError::OutOfRange)?;
    seconds
        .checked_mul(NANOS_PER_SECOND)
        .and_then(|value| value.checked_add(nanoseconds))
        .map(ElapsedInstant::from_nanos)
        .ok_or(ClockError::OutOfRange)
}

fn ticks_to_nanos(ticks: u128, numerator: u32, denominator: u32) -> Result<u128, ClockError> {
    if denominator == 0 {
        return Err(ClockError::Unavailable);
    }
    ticks
        .checked_mul(u128::from(numerator))
        .and_then(|value| value.checked_div(u128::from(denominator)))
        .ok_or(ClockError::OutOfRange)
}

#[cfg(target_os = "linux")]
fn platform_elapsed() -> Result<ElapsedInstant, ClockError> {
    let now = rustix::time::clock_gettime(rustix::time::ClockId::Boottime);
    elapsed_from_parts(now.tv_sec, now.tv_nsec)
}

#[cfg(target_os = "linux")]
fn platform_boot_identity() -> Result<String, ClockError> {
    let identity = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map_err(|_| ClockError::Unavailable)?;
    let identity = identity.trim();
    if identity.len() != 36
        || !identity
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
    {
        return Err(ClockError::Unavailable);
    }
    Ok(identity.to_owned())
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct MachTimebaseInfo {
    numerator: u32,
    denominator: u32,
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn mach_continuous_time() -> u64;
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
}

#[cfg(target_os = "macos")]
fn platform_elapsed() -> Result<ElapsedInstant, ClockError> {
    let mut info = MachTimebaseInfo {
        numerator: 0,
        denominator: 0,
    };
    // SAFETY: `info` is writable for the full C call and has the expected ABI.
    let status = unsafe { mach_timebase_info(&raw mut info) };
    // SAFETY: `mach_continuous_time` has no arguments or memory preconditions.
    let ticks = unsafe { mach_continuous_time() };
    elapsed_from_mach(status, ticks, &info)
}

/// Converts a mach timebase reading, rejecting a failed `mach_timebase_info`.
#[cfg(target_os = "macos")]
fn elapsed_from_mach(
    status: i32,
    ticks: u64,
    info: &MachTimebaseInfo,
) -> Result<ElapsedInstant, ClockError> {
    if status != 0 {
        return Err(ClockError::Unavailable);
    }
    ticks_to_nanos(u128::from(ticks), info.numerator, info.denominator)
        .map(ElapsedInstant::from_nanos)
}

#[cfg(target_os = "macos")]
fn platform_boot_identity() -> Result<String, ClockError> {
    let mut boot_time = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    let mut size = std::mem::size_of::<libc::timeval>();
    // SAFETY: The output pointer and size describe a live `timeval`; no input is supplied.
    let status = unsafe {
        libc::sysctlbyname(
            c"kern.boottime".as_ptr(),
            (&raw mut boot_time).cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    boot_identity_from_timeval(status, size, boot_time)
}

/// Validates the `kern.boottime` answer before trusting it as a boot identity.
#[cfg(target_os = "macos")]
fn boot_identity_from_timeval(
    status: i32,
    size: usize,
    boot_time: libc::timeval,
) -> Result<String, ClockError> {
    if status != 0
        || size != std::mem::size_of::<libc::timeval>()
        || boot_time.tv_sec <= 0
        || boot_time.tv_usec < 0
    {
        return Err(ClockError::Unavailable);
    }
    Ok(format!("macos:{}:{}", boot_time.tv_sec, boot_time.tv_usec))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use crate::application::{ElapsedClock, WallClock};

    use super::{NativeClock, elapsed_from_parts, ticks_to_nanos};

    #[test]
    fn native_clocks_are_ordered_and_boot_scoped() {
        let clock = NativeClock;
        let first_wall = clock.now_utc().expect("wall clock");
        let first_elapsed = clock.now_elapsed().expect("elapsed clock");
        let second_wall = clock.now_utc().expect("wall clock");
        let second_elapsed = clock.now_elapsed().expect("elapsed clock");

        assert!(second_wall >= first_wall);
        assert!(second_elapsed >= first_elapsed);
        assert!(!clock.boot_identity().expect("boot identity").is_empty());
    }

    #[test]
    fn conversions_reject_invalid_and_overflowing_values() {
        assert_eq!(
            elapsed_from_parts(2, 3).expect("parts").as_nanos(),
            2_000_000_003
        );
        assert!(elapsed_from_parts(-1, 0).is_err());
        assert!(elapsed_from_parts(0, 1_000_000_000).is_err());
        assert_eq!(ticks_to_nanos(3, 2, 1).expect("ticks"), 6);
        assert!(ticks_to_nanos(1, 1, 0).is_err());
        assert!(ticks_to_nanos(u128::MAX, u32::MAX, 1).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mach_readings_reject_a_failed_timebase_call() {
        use super::{MachTimebaseInfo, boot_identity_from_timeval, elapsed_from_mach};

        let good = MachTimebaseInfo {
            numerator: 1,
            denominator: 1,
        };
        assert_eq!(
            elapsed_from_mach(0, 7, &good)
                .expect("valid reading")
                .as_nanos(),
            7
        );
        assert!(matches!(
            elapsed_from_mach(-1, 7, &good),
            Err(crate::application::ClockError::Unavailable)
        ));

        let boot_time = libc::timeval {
            tv_sec: 1_700_000_000,
            tv_usec: 42,
        };
        let identity =
            boot_identity_from_timeval(0, std::mem::size_of::<libc::timeval>(), boot_time)
                .expect("valid boot time");
        assert_eq!(identity, "macos:1700000000:42");
        // Every rejection arm must be reachable: syscall failure, truncated
        // output buffer, non-positive seconds, and negative microseconds.
        for (status, size, boot_time) in [
            (-1, std::mem::size_of::<libc::timeval>(), boot_time),
            (0, std::mem::size_of::<libc::timeval>() - 1, boot_time),
            (
                0,
                std::mem::size_of::<libc::timeval>(),
                libc::timeval {
                    tv_sec: 0,
                    tv_usec: 0,
                },
            ),
            (
                0,
                std::mem::size_of::<libc::timeval>(),
                libc::timeval {
                    tv_sec: 10,
                    tv_usec: -1,
                },
            ),
        ] {
            assert!(matches!(
                boot_identity_from_timeval(status, size, boot_time),
                Err(crate::application::ClockError::Unavailable)
            ));
        }
    }
}
