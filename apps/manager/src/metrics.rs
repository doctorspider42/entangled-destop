//! Host and per-VM process metrics: CPU, RSS, host memory and the free space
//! under the VM directory, sampled once a second on a background thread (the
//! frame loop never blocks — rule of the house).
//!
//! No metrics crate: the four numbers this needs are four small syscalls per
//! platform (`/proc` on Linux, `GetProcessTimes`/`GlobalMemoryStatusEx` and
//! friends on Windows), and every reader degrades to `None` where the host
//! cannot answer — a missing number renders as nothing, never as an error.
//!
//! CPU convention: per-process percentages are top-style (a share of **one**
//! core, so a 2-vCPU guest under load reads ~200%); the host percentage is a
//! share of the whole machine.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::process::Waker;

/// Sample cadence. One second keeps percentages stable and the cost invisible.
const INTERVAL: Duration = Duration::from_secs(1);
/// CPU-history samples kept per VM and for the host (~90 s of sparkline).
const HISTORY: usize = 90;

/// One VM child's live numbers.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VmStats {
    /// Top-style: share of one core, may exceed 100 on multi-vCPU guests.
    pub cpu_percent: Option<f32>,
    /// Working set / resident size — the guest's RAM lives inside the child,
    /// so this tracks what the guest actually touched.
    pub rss_bytes: Option<u64>,
    /// The last [`HISTORY`] CPU samples, oldest first (sparkline data).
    pub history: Vec<f32>,
}

/// The host's live numbers.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HostStats {
    /// Share of the whole machine, 0..=100.
    pub cpu_percent: Option<f32>,
    pub history: Vec<f32>,
    pub mem_total: Option<u64>,
    pub mem_available: Option<u64>,
    /// `(free, total)` of the filesystem holding the VM directory.
    pub vm_dir_space: Option<(u64, u64)>,
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub host: HostStats,
    pub vms: HashMap<String, VmStats>,
}

struct SharedState {
    /// `(vm name, pid)` of every active child, plus the VM directory to
    /// measure — replaced by the UI thread every frame.
    targets: Vec<(String, u32)>,
    vm_dir: PathBuf,
    snapshot: Snapshot,
    stop: bool,
}

/// Handle owned by the app; the sampler thread holds the other `Arc`.
pub struct Metrics {
    shared: Arc<Mutex<SharedState>>,
}

impl Metrics {
    pub fn spawn(waker: Waker) -> Self {
        let shared = Arc::new(Mutex::new(SharedState {
            targets: Vec::new(),
            vm_dir: PathBuf::new(),
            snapshot: Snapshot::default(),
            stop: false,
        }));
        let thread_shared = Arc::clone(&shared);
        let builder = std::thread::Builder::new().name("metrics".to_string());
        if let Err(e) = builder.spawn(move || sample_loop(thread_shared, waker)) {
            // Without the thread every number stays None — the UI simply
            // shows no stats, which is the degraded mode anyway.
            tracing::error!(error = %e, "cannot start the metrics thread");
        }
        Self { shared }
    }

    /// A sampler handle with fixed data and no worker thread. Used by the UI
    /// mock so visual work never probes the real host.
    pub fn fixed(snapshot: Snapshot) -> Self {
        Self {
            shared: Arc::new(Mutex::new(SharedState {
                targets: Vec::new(),
                vm_dir: PathBuf::new(),
                snapshot,
                stop: true,
            })),
        }
    }

    /// Called from the frame loop: which children to measure, and where the
    /// VM directory currently is.
    pub fn set_targets(&self, targets: Vec<(String, u32)>, vm_dir: PathBuf) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.targets = targets;
            shared.vm_dir = vm_dir;
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.shared
            .lock()
            .map(|shared| shared.snapshot.clone())
            .unwrap_or_default()
    }
}

impl Drop for Metrics {
    fn drop(&mut self) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.stop = true;
        }
    }
}

fn sample_loop(shared: Arc<Mutex<SharedState>>, waker: Waker) {
    // Per-pid last (cpu_time_ns, at). A pid that disappears from the targets
    // is forgotten, so a reused pid never inherits a stale baseline.
    let mut process_prev: HashMap<u32, (u64, Instant)> = HashMap::new();
    let mut host_prev: Option<(u64, u64)> = None;
    let mut host_history: Vec<f32> = Vec::new();
    let mut vm_history: HashMap<String, Vec<f32>> = HashMap::new();

    loop {
        let (targets, vm_dir) = {
            let Ok(shared) = shared.lock() else { return };
            if shared.stop {
                return;
            }
            (shared.targets.clone(), shared.vm_dir.clone())
        };

        // Host CPU.
        let host_cpu = host_cpu_times().and_then(|(busy, total)| {
            let percent = host_prev.and_then(|(prev_busy, prev_total)| {
                percent_of(
                    busy.saturating_sub(prev_busy),
                    total.saturating_sub(prev_total),
                )
            });
            host_prev = Some((busy, total));
            percent
        });
        if let Some(percent) = host_cpu {
            push_capped(&mut host_history, percent);
        }

        // Per-child CPU and RSS.
        let now = Instant::now();
        let mut vms = HashMap::new();
        process_prev.retain(|pid, _| targets.iter().any(|(_, p)| p == pid));
        vm_history.retain(|vm, _| targets.iter().any(|(name, _)| name == vm));
        for (vm, pid) in &targets {
            let cpu_time = process_cpu_time_ns(*pid);
            let cpu_percent = match (cpu_time, process_prev.get(pid)) {
                (Some(time), Some((prev_time, prev_at))) => {
                    let wall = now.duration_since(*prev_at).as_nanos() as u64;
                    percent_of(time.saturating_sub(*prev_time), wall)
                }
                _ => None,
            };
            if let Some(time) = cpu_time {
                process_prev.insert(*pid, (time, now));
            }
            let history = vm_history.entry(vm.clone()).or_default();
            if let Some(percent) = cpu_percent {
                push_capped(history, percent);
            }
            vms.insert(
                vm.clone(),
                VmStats {
                    cpu_percent,
                    rss_bytes: process_rss(*pid),
                    history: history.clone(),
                },
            );
        }

        let (mem_total, mem_available) = host_memory().unzip();
        let snapshot = Snapshot {
            host: HostStats {
                cpu_percent: host_cpu,
                history: host_history.clone(),
                mem_total,
                mem_available,
                vm_dir_space: (!vm_dir.as_os_str().is_empty())
                    .then(|| disk_image::disk_space(&vm_dir))
                    .flatten(),
            },
            vms,
        };

        {
            let Ok(mut guard) = shared.lock() else { return };
            if guard.stop {
                return;
            }
            guard.snapshot = snapshot;
        }
        waker();
        std::thread::sleep(INTERVAL);
    }
}

/// `part / whole` as a percentage, `None` for a zero denominator (first
/// sample, or a clock that did not advance).
fn percent_of(part: u64, whole: u64) -> Option<f32> {
    if whole == 0 {
        return None;
    }
    Some((part as f64 / whole as f64 * 100.0) as f32)
}

fn push_capped(history: &mut Vec<f32>, value: f32) {
    history.push(value);
    if history.len() > HISTORY {
        let excess = history.len() - HISTORY;
        history.drain(..excess);
    }
}

// ---------------------------------------------------------------------------
// Platform readers — each answers `None` rather than failing.
// ---------------------------------------------------------------------------

/// Host `(busy, total)` CPU time in platform units (only deltas matter).
#[cfg(target_os = "linux")]
fn host_cpu_times() -> Option<(u64, u64)> {
    // "cpu  user nice system idle iowait irq softirq steal ..."
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let line = stat.lines().next()?;
    let fields: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|f| f.parse().ok())
        .collect();
    if fields.len() < 4 {
        return None;
    }
    let total: u64 = fields.iter().sum();
    let idle = fields[3] + fields.get(4).copied().unwrap_or(0); // idle + iowait
    Some((total.saturating_sub(idle), total))
}

#[cfg(windows)]
fn host_cpu_times() -> Option<(u64, u64)> {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::GetSystemTimes;

    let mut idle = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: three valid out-pointers for the duration of the call.
    unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)) }.ok()?;
    let idle = filetime_100ns(&idle);
    // Kernel time includes idle time, so kernel + user is the total.
    let total = filetime_100ns(&kernel).saturating_add(filetime_100ns(&user));
    Some((total.saturating_sub(idle), total))
}

#[cfg(not(any(target_os = "linux", windows)))]
fn host_cpu_times() -> Option<(u64, u64)> {
    None
}

/// Host `(total, available)` physical memory in bytes.
#[cfg(target_os = "linux")]
fn host_memory() -> Option<(u64, u64)> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let field = |name: &str| -> Option<u64> {
        meminfo
            .lines()
            .find(|l| l.starts_with(name))?
            .split_whitespace()
            .nth(1)?
            .parse::<u64>()
            .ok()
            .map(|kib| kib * 1024)
    };
    Some((field("MemTotal:")?, field("MemAvailable:")?))
}

#[cfg(windows)]
fn host_memory() -> Option<(u64, u64)> {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    let mut status = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: `status` is a valid out-struct with dwLength set, as the API
    // requires, for the duration of the call.
    unsafe { GlobalMemoryStatusEx(&mut status) }.ok()?;
    Some((status.ullTotalPhys, status.ullAvailPhys))
}

#[cfg(not(any(target_os = "linux", windows)))]
fn host_memory() -> Option<(u64, u64)> {
    None
}

/// Cumulative CPU time (kernel + user) of a process, in nanoseconds.
#[cfg(target_os = "linux")]
fn process_cpu_time_ns(pid: u32) -> Option<u64> {
    // /proc/<pid>/stat, fields 14 (utime) and 15 (stime) in clock ticks —
    // counted *after* the closing paren of comm, which may itself contain
    // spaces and parens.
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = &stat[stat.rfind(')')? + 2..];
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // after_comm starts at field 3 ("state"), so utime/stime are at 11/12.
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    // SAFETY: sysconf takes a plain integer selector and returns a value; no
    // memory crosses the boundary.
    let ticks_per_sec = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks_per_sec <= 0 {
        return None;
    }
    Some((utime + stime).saturating_mul(1_000_000_000 / ticks_per_sec as u64))
}

#[cfg(windows)]
fn process_cpu_time_ns(pid: u32) -> Option<u64> {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::GetProcessTimes;

    let process = OpenedProcess::open(pid)?;
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: the handle is open (guard below) and all four out-pointers are
    // valid for the duration of the call.
    unsafe { GetProcessTimes(process.0, &mut creation, &mut exit, &mut kernel, &mut user) }.ok()?;
    Some(
        filetime_100ns(&kernel)
            .saturating_add(filetime_100ns(&user))
            .saturating_mul(100),
    )
}

#[cfg(not(any(target_os = "linux", windows)))]
fn process_cpu_time_ns(_pid: u32) -> Option<u64> {
    None
}

/// Resident set size / working set of a process, in bytes.
#[cfg(target_os = "linux")]
fn process_rss(pid: u32) -> Option<u64> {
    // /proc/<pid>/statm field 2: resident pages.
    let statm = std::fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    // SAFETY: sysconf takes a plain integer selector; no memory crosses.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page_size <= 0 {
        return None;
    }
    Some(pages.saturating_mul(page_size as u64))
}

#[cfg(windows)]
fn process_rss(pid: u32) -> Option<u64> {
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};

    let process = OpenedProcess::open(pid)?;
    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: the handle is open (guard below); `counters` is a valid
    // out-struct of the stated size for the duration of the call.
    unsafe {
        GetProcessMemoryInfo(
            process.0,
            &mut counters,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        )
    }
    .ok()?;
    Some(counters.WorkingSetSize as u64)
}

#[cfg(not(any(target_os = "linux", windows)))]
fn process_rss(_pid: u32) -> Option<u64> {
    None
}

#[cfg(windows)]
fn filetime_100ns(t: &windows::Win32::Foundation::FILETIME) -> u64 {
    (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)
}

/// An open process handle that closes itself — every early return above would
/// otherwise leak one handle per sample.
#[cfg(windows)]
struct OpenedProcess(windows::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl OpenedProcess {
    fn open(pid: u32) -> Option<Self> {
        use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
        // SAFETY: no pointers cross the boundary; a failed open returns an
        // error, not a handle.
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
            .ok()
            .map(Self)
    }
}

#[cfg(windows)]
impl Drop for OpenedProcess {
    fn drop(&mut self) {
        // SAFETY: the handle was opened by us and not closed elsewhere.
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.0) };
    }
}

/// "3h 02m", "12m 05s", "42s" — uptime for the cards.
pub fn format_uptime(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs >= 3600 {
        format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentages_handle_first_samples_and_zero_denominators() {
        assert_eq!(percent_of(50, 0), None);
        assert_eq!(percent_of(50, 100), Some(50.0));
        assert_eq!(percent_of(0, 100), Some(0.0));
        // Top-style: more than one core's worth is legal.
        assert_eq!(percent_of(400, 100), Some(400.0));
    }

    #[test]
    fn history_is_capped() {
        let mut history = Vec::new();
        for i in 0..(HISTORY + 25) {
            push_capped(&mut history, i as f32);
        }
        assert_eq!(history.len(), HISTORY);
        assert_eq!(history[0], 25.0, "oldest samples drop first");
    }

    #[test]
    fn uptime_formats_all_three_shapes() {
        assert_eq!(format_uptime(Duration::from_secs(42)), "42s");
        assert_eq!(format_uptime(Duration::from_secs(125)), "2m 05s");
        assert_eq!(format_uptime(Duration::from_secs(3 * 3600 + 125)), "3h 02m");
    }

    /// The platform readers answer for the current process and host — the
    /// exact numbers are the host's business, the shape is ours.
    #[test]
    fn platform_readers_answer_for_ourselves() {
        let pid = std::process::id();
        if let Some(rss) = process_rss(pid) {
            assert!(rss > 0, "we occupy memory");
        }
        if let Some(time) = process_cpu_time_ns(pid) {
            // Burn a little CPU; cumulative time must not decrease.
            let mut x = 0u64;
            for i in 0..2_000_000u64 {
                x = x.wrapping_add(i);
            }
            std::hint::black_box(x);
            let later = process_cpu_time_ns(pid).expect("still us");
            assert!(later >= time);
        }
        if let Some((total, available)) = host_memory() {
            assert!(total > 0);
            assert!(available <= total);
        }
        if let Some((busy, total)) = host_cpu_times() {
            assert!(busy <= total);
        }
    }
}
