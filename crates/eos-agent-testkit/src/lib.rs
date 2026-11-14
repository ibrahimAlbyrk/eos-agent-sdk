//! Memory measurement for tests and benches.
//!
//! - [`CountingAlloc`] counts every allocation, so a test can assert that a
//!   repeated operation allocates the same amount each time (no growth).
//! - [`footprint_bytes`] reads the process's physical footprint, the number
//!   Activity Monitor shows, for soak tests.
//! - [`slope`] fits a line through samples to tell steady growth from noise.
//! - [`cpu_time`] and [`peak_footprint_bytes`] for benchmarks;
//!   [`raise_fd_limit`] for runs with thousands of open connections.

// A global allocator and the macOS rusage call cannot be written without it.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

static ALLOCS: AtomicU64 = AtomicU64::new(0);
static FREES: AtomicU64 = AtomicU64::new(0);
static LIVE: AtomicU64 = AtomicU64::new(0);
static PEAK: AtomicU64 = AtomicU64::new(0);

/// Install with `#[global_allocator] static A: CountingAlloc = CountingAlloc;`.
#[derive(Debug)]
pub struct CountingAlloc;

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            grew(layout.size() as u64);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        FREES.fetch_add(1, Relaxed);
        LIVE.fetch_sub(layout.size() as u64, Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        if !new.is_null() {
            FREES.fetch_add(1, Relaxed);
            LIVE.fetch_sub(layout.size() as u64, Relaxed);
            grew(new_size as u64);
        }
        new
    }
}

fn grew(size: u64) {
    ALLOCS.fetch_add(1, Relaxed);
    let live = LIVE.fetch_add(size, Relaxed) + size;
    PEAK.fetch_max(live, Relaxed);
}

/// Allocation counters at one moment. Subtract two snapshots to measure a span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AllocStats {
    pub allocs: u64,
    pub frees: u64,
    pub live_bytes: u64,
    pub peak_bytes: u64,
}

impl AllocStats {
    /// Only meaningful when [`CountingAlloc`] is the global allocator.
    pub fn now() -> Self {
        Self {
            allocs: ALLOCS.load(Relaxed),
            frees: FREES.load(Relaxed),
            live_bytes: LIVE.load(Relaxed),
            peak_bytes: PEAK.load(Relaxed),
        }
    }

    /// Allocations made and bytes still held since `earlier`.
    pub fn since(&self, earlier: &Self) -> (u64, i64) {
        (self.allocs - earlier.allocs, self.live_bytes as i64 - earlier.live_bytes as i64)
    }
}

/// This process's physical memory footprint in bytes (macOS `phys_footprint`).
pub fn footprint_bytes() -> u64 {
    let mut info = std::mem::MaybeUninit::<libc::rusage_info_v2>::zeroed();
    // SAFETY: `info` is a valid, writable rusage_info_v2 and the flavor matches it.
    let rc = unsafe {
        libc::proc_pid_rusage(libc::getpid(), libc::RUSAGE_INFO_V2, info.as_mut_ptr().cast::<libc::rusage_info_t>())
    };
    if rc != 0 {
        return 0;
    }
    // SAFETY: the call succeeded, so the kernel filled the struct.
    unsafe { info.assume_init() }.ri_phys_footprint
}

/// The highest physical footprint this process has had.
pub fn peak_footprint_bytes() -> u64 {
    let mut info = std::mem::MaybeUninit::<libc::rusage_info_v4>::zeroed();
    // SAFETY: `info` is a valid, writable rusage_info_v4 and the flavor matches it.
    let rc = unsafe {
        libc::proc_pid_rusage(libc::getpid(), libc::RUSAGE_INFO_V4, info.as_mut_ptr().cast::<libc::rusage_info_t>())
    };
    if rc != 0 {
        return 0;
    }
    // SAFETY: the call succeeded, so the kernel filled the struct.
    unsafe { info.assume_init() }.ri_lifetime_max_phys_footprint
}

/// User plus system CPU time this process has used so far.
pub fn cpu_time() -> std::time::Duration {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: `usage` is a valid, writable rusage.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return std::time::Duration::ZERO;
    }
    // SAFETY: the call succeeded, so the kernel filled the struct.
    let usage = unsafe { usage.assume_init() };
    let micros = |t: libc::timeval| t.tv_sec as u64 * 1_000_000 + t.tv_usec as u64;
    std::time::Duration::from_micros(micros(usage.ru_utime) + micros(usage.ru_stime))
}

/// Raises this process's open-file limit to its hard limit (capped by the
/// kernel's per-process maximum). Returns the new soft limit.
pub fn raise_fd_limit() -> u64 {
    let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: `limit` is a valid, writable rlimit.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return 0;
    }
    // macOS refuses RLIM_INFINITY for open files; OPEN_MAX-sized limits come from sysctl.
    let max_per_proc = sysctl_u64("kern.maxfilesperproc").unwrap_or(10_240);
    limit.rlim_cur = limit.rlim_max.min(max_per_proc);
    // SAFETY: `limit` holds values the kernel accepts for this resource.
    unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) };
    limit.rlim_cur
}

fn sysctl_u64(name: &str) -> Option<u64> {
    let name = std::ffi::CString::new(name).ok()?;
    let mut value: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>();
    // SAFETY: `value` and `size` describe a writable c_int, the size sysctl expects for this name.
    let rc = unsafe { libc::sysctlbyname(name.as_ptr(), (&raw mut value).cast(), &mut size, std::ptr::null_mut(), 0) };
    (rc == 0).then_some(value as u64)
}

/// Least-squares slope of `samples` per step. Near zero means no steady growth.
pub fn slope(samples: &[u64]) -> f64 {
    let n = samples.len() as f64;
    if samples.len() < 2 {
        return 0.0;
    }
    let mean_x = (n - 1.0) / 2.0;
    let mean_y = samples.iter().map(|&y| y as f64).sum::<f64>() / n;
    let (mut num, mut den) = (0.0, 0.0);
    for (i, &y) in samples.iter().enumerate() {
        let dx = i as f64 - mean_x;
        num += dx * (y as f64 - mean_y);
        den += dx * dx;
    }
    num / den
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slope_tells_growth_from_flat() {
        assert!(slope(&[10, 10, 10, 10]).abs() < f64::EPSILON);
        assert!((slope(&[0, 2, 4, 6]) - 2.0).abs() < 1e-9);
    }

    #[test]
    fn footprint_is_readable() {
        assert!(footprint_bytes() > 0);
        assert!(peak_footprint_bytes() >= footprint_bytes() / 2);
        assert!(raise_fd_limit() >= 256);
    }
}
