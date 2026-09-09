//! Application-processor startup, and the firmware's AP-detection window (the
//! long-standing `MpInitLib: Find 1 processors in system` flake).
//!
//! # What the firmware waits for
//!
//! `UefiCpuPkg/Library/MpInitLib/MpLib.c::WakeUpAP` broadcasts INIT-SIPI-SIPI
//! and then, because CloudHv leaves `PcdCpuBootLogicalProcessorNumber` at zero
//! (there is no fw_cfg to read a boot CPU count from), waits like this:
//!
//! ```text
//!   TimedWaitForApFinish (CpuMpData,
//!                         PcdCpuMaxLogicalProcessorNumber - 1,   // 254 - 1
//!                         PcdCpuApInitTimeOutInMicroSeconds);    // 50 000 us
//!   while (CpuMpData->MpCpuExchangeInfo->NumApsExecuting != 0) { CpuPause (); }
//! ```
//!
//! The first wait can never be satisfied on a 2-vCPU guest, so it runs the full
//! 50 ms; the second is unbounded but `NumApsExecuting` is bumped by the AP
//! itself, a `lock inc` a few dozen instructions into the wakeup blob
//! (`X64/MpFuncs.nasm`, "as early as possible"). So an AP that executes its
//! first instruction inside the 50 ms is waited for however long it then needs,
//! and one that has not is abandoned.
//!
//! # What was actually going wrong
//!
//! Not the AP's scheduling. `CheckTimeout()` measures those 50 ms by
//! *differencing successive reads of the ACPI PM timer*, and treats a negative
//! difference as the 24-bit counter having wrapped — adding a whole 4.7-second
//! cycle to its elapsed total and ending the wait on the spot. Our PM timer used
//! to sample the host clock once per **byte** of a 32-bit `IN`, so a carry out
//! of the low byte between byte 0 and byte 1 returned a value up to 255 ticks
//! ahead of the counter and made the *next* read look like a wrap. Rare when the
//! host is idle (the four samples are nanoseconds apart), common when it is
//! loaded (the exit handler itself can be preempted mid-access). Fixed in
//! `machine_x86::acpi::pm`; the unit test that pins it is
//! `a_wide_timer_read_is_one_sample_of_the_counter`.
//!
//! The signature in the numbers below is unmistakable: a healthy boot's verdict
//! lands ~70-95 ms after the IPI (10 ms INIT delay + the 50 ms wait + serial),
//! a torn one at 17-33 ms.
//!
//! # How it is measured, without disturbing what is measured
//!
//! * the BSP's spin loops are visible from the host — `CheckTimeout()` and
//!   `MicroSecondDelay()` read the PM timer at [`PM_TIMER_PORT`] on every
//!   iteration, one VM exit apiece;
//! * the firmware's own progress lines are timestamped as they reach the serial
//!   port, which dates the IPI (`AP Vector: 16-bit = ...`, printed immediately
//!   before it) and the verdict (`MpInitLib: Find N ...`);
//! * the AP's first instruction is visible in `/proc/self/task/<tid>/schedstat`
//!   — a vCPU thread parked in `KVM_RUN` waiting for its SIPI burns no CPU time,
//!   so the sample where its runtime starts moving is when guest code started.
//!   A very short slice is not guest code but KVM returning `EAGAIN` from the
//!   AP's first `KVM_RUN` the moment the INIT is accepted, which is why the two
//!   are reported separately;
//! * the same file's second field is the thread's accumulated run-queue wait.
//!
//! None of those probes touches a vCPU thread, so measuring does not change what
//! is being measured (waking the AP to ask it questions would).
//!
//! # Running it
//!
//! ```text
//! cargo test -p boot-tests --test ap_startup                    # one boot
//! ENTANGLED_AP_BOOTS=30 cargo test -p boot-tests --test ap_startup \
//!     -- --ignored --nocapture ap_startup_campaign              # a campaign
//! ```
//!
//! To reproduce the original flake, load the host first — 64 spinners on 16
//! CPUs took it from 0/12 to 8/16 boots losing an AP.

#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use boot_tests::{artifact, kvm_available};
use machine_x86::acpi::pm::{ACPI_PM_TIMER_HZ, PM_TIMER_PORT};
use machine_x86::boot as x86_boot;
use machine_x86::bus::MachineBus;
use machine_x86::layout;
use machine_x86::serial::SerialConsole;
use vm_memory::{Bytes, GuestAddress};
use vmm_core::hv::ExitHandler;
use vmm_core::{spawn_vcpus, Hypervisor, MachineConfig, Vm};

/// Generous: the interesting failure is a wrong CPU count, not a hang.
const DEADLINE: Duration = Duration::from_secs(90);
const BOOT_MANAGER: &str = "No bootable option or device was found";
/// Two samples per millisecond: fine enough to place the AP's first
/// instruction, coarse enough that the sampler itself is invisible.
const SAMPLE_PERIOD: Duration = Duration::from_micros(500);

// ---------------------------------------------------------------------------
// probes

/// The serial console, timestamped. The firmware's own progress lines are the
/// only clock inside the guest we can read, and knowing *when* `MpInitLib`
/// printed its verdict is what turns a scheduling trace into a story.
#[derive(Clone)]
struct Capture {
    bytes: Arc<Mutex<Vec<u8>>>,
    /// `(elapsed_us, line)` for every complete line, in order.
    lines: Arc<Mutex<Vec<(u64, String)>>>,
    partial: Arc<Mutex<Vec<u8>>>,
    start: Instant,
}

impl Capture {
    fn new(start: Instant) -> Self {
        Self {
            bytes: Arc::default(),
            lines: Arc::default(),
            partial: Arc::default(),
            start,
        }
    }
}

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let at = self.start.elapsed().as_micros() as u64;
        if let Ok(mut inner) = self.bytes.lock() {
            inner.extend_from_slice(buf);
        }
        if let (Ok(mut partial), Ok(mut lines)) = (self.partial.lock(), self.lines.lock()) {
            for &byte in buf {
                if byte == b'\n' {
                    let line = String::from_utf8_lossy(&partial).trim().to_string();
                    partial.clear();
                    if !line.is_empty() {
                        lines.push((at, line));
                    }
                } else {
                    partial.push(byte);
                }
            }
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Capture {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes.lock().map(|v| v.clone()).unwrap_or_default())
            .into_owned()
    }

    fn timeline(&self) -> Vec<(u64, String)> {
        self.lines.lock().map(|l| l.clone()).unwrap_or_default()
    }
}

/// Wraps the machine bus and timestamps every ACPI PM timer read, which is how
/// the BSP's spin windows become visible from the host.
struct PmTimerProbe {
    inner: MachineBus,
    start: Instant,
    /// `(elapsed_us, tick)` per read: the tick value is what the firmware's
    /// `CheckTimeout()` accumulates, so it says how much of the 50 ms budget
    /// the guest thinks it has spent.
    reads: Arc<Mutex<Vec<(u64, u32)>>>,
}

impl ExitHandler for PmTimerProbe {
    fn io_out(&mut self, port: u16, data: &[u8]) {
        self.inner.io_out(port, data);
    }
    fn io_in(&mut self, port: u16, data: &mut [u8]) {
        self.inner.io_in(port, data);
        if port == PM_TIMER_PORT && data.len() == 4 {
            let at = self.start.elapsed().as_micros() as u64;
            let tick = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
            if let Ok(mut reads) = self.reads.lock() {
                reads.push((at, tick));
            }
        }
    }
    fn mmio_write(&mut self, addr: u64, data: &[u8]) {
        self.inner.mmio_write(addr, data);
    }
    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) {
        self.inner.mmio_read(addr, data);
    }
    fn shutdown_requested(&self) -> bool {
        self.inner.shutdown_requested()
    }
    fn reset_requested(&self) -> bool {
        self.inner.reset_requested()
    }
}

/// One `/proc/self/task/<tid>/schedstat` reading: nanoseconds on CPU,
/// nanoseconds waiting on a run queue, and how many times the thread has been
/// scheduled.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct SchedStat {
    on_cpu_ns: u64,
    run_delay_ns: u64,
    slices: u64,
}

fn read_schedstat(tid: u64) -> Option<SchedStat> {
    let raw = std::fs::read_to_string(format!("/proc/self/task/{tid}/schedstat")).ok()?;
    let mut fields = raw.split_whitespace();
    Some(SchedStat {
        on_cpu_ns: fields.next()?.parse().ok()?,
        run_delay_ns: fields.next()?.parse().ok()?,
        slices: fields.next()?.parse().ok()?,
    })
}

/// Maps `vcpuN` thread names to thread ids, by scanning this process's own
/// tasks. The names come from `std::thread::Builder::name` in
/// `vmm_core::spawn_vcpus`, which is what `prctl(PR_SET_NAME)` publishes.
fn vcpu_tids() -> BTreeMap<u32, u64> {
    let mut found = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir("/proc/self/task") else {
        return found;
    };
    for entry in entries.flatten() {
        let Ok(tid) = entry.file_name().to_string_lossy().parse::<u64>() else {
            continue;
        };
        let Ok(comm) = std::fs::read_to_string(entry.path().join("comm")) else {
            continue;
        };
        if let Some(index) = comm.trim().strip_prefix("vcpu") {
            if let Ok(index) = index.parse::<u32>() {
                found.insert(index, tid);
            }
        }
    }
    found
}

/// What the sampler saw of one vCPU thread.
#[derive(Debug, Default, Clone)]
struct VcpuTrace {
    /// Total nanoseconds the thread spent runnable-but-not-running.
    run_delay_ns: u64,
    /// The largest single-sample jump in that figure: the longest stall.
    worst_stall_ns: u64,
    /// When that worst stall was observed.
    worst_stall_at_us: u64,
    on_cpu_ns: u64,
    /// `(elapsed_us, on_cpu_ns delta)` for every sample where the thread made
    /// progress. An AP parked in `KVM_RUN` waiting for its SIPI contributes
    /// nothing, so this is a timeline of when it actually ran.
    activity: Vec<(u64, u64)>,
}

/// A slice this short is a wake-up that did no guest work: on KVM an AP's
/// first `KVM_RUN` returns `EAGAIN` the moment the INIT is accepted
/// (`kvm_arch_vcpu_ioctl_run`'s `KVM_MP_STATE_UNINITIALIZED` path), which costs
/// the thread a scheduling round trip and tens of microseconds of CPU. Guest
/// code costs milliseconds.
const WAKE_SLICE_NS: u64 = 500_000;

impl VcpuTrace {
    /// The first sign of life after `after_us`, however brief.
    fn first_wake_after(&self, after_us: u64) -> Option<u64> {
        self.activity
            .iter()
            .find(|(at, _)| *at >= after_us)
            .map(|(at, _)| *at)
    }

    /// The first slice after `after_us` long enough to be guest code.
    fn first_run_after(&self, after_us: u64) -> Option<u64> {
        self.activity
            .iter()
            .find(|(at, ran)| *at >= after_us && *ran >= WAKE_SLICE_NS)
            .map(|(at, _)| *at)
    }
}

/// Samples `/proc` for the lifetime of the boot. Passive: it never touches the
/// vCPU threads, so it cannot perturb their scheduling.
fn sample_vcpus(start: Instant, stop: &AtomicBool, expected: u32) -> BTreeMap<u32, VcpuTrace> {
    let mut traces: BTreeMap<u32, VcpuTrace> = BTreeMap::new();
    let mut last: BTreeMap<u32, SchedStat> = BTreeMap::new();
    while !stop.load(Ordering::Acquire) {
        let now = start.elapsed().as_micros() as u64;
        let tids = vcpu_tids();
        if tids.len() as u32 >= expected {
            for (&index, &tid) in &tids {
                let Some(stat) = read_schedstat(tid) else {
                    continue;
                };
                let trace = traces.entry(index).or_default();
                trace.run_delay_ns = stat.run_delay_ns;
                trace.on_cpu_ns = stat.on_cpu_ns;
                if let Some(previous) = last.get(&index) {
                    let stalled = stat.run_delay_ns.saturating_sub(previous.run_delay_ns);
                    if stalled > trace.worst_stall_ns {
                        trace.worst_stall_ns = stalled;
                        trace.worst_stall_at_us = now;
                    }
                    let ran = stat.on_cpu_ns.saturating_sub(previous.on_cpu_ns);
                    // 20 us of CPU in half a millisecond: enough to exclude
                    // accounting noise, far below what a mode switch costs.
                    if ran > 20_000 {
                        trace.activity.push((now, ran));
                    }
                }
                last.insert(index, stat);
            }
        }
        std::thread::sleep(SAMPLE_PERIOD);
    }
    traces
}

// ---------------------------------------------------------------------------
// one boot

/// One dense run of PM-timer reads: the host-visible shape of a firmware spin
/// loop, with how much of the guest's own clock it consumed.
#[derive(Debug, Clone, Copy)]
struct Burst {
    start_us: u64,
    end_us: u64,
    reads: usize,
    /// Ticks the guest saw pass, at 3.579545 MHz. `CheckTimeout()` accumulates
    /// exactly this, so it is the firmware's own measure of the wait.
    ticks: u32,
}

#[derive(Debug)]
struct BootReport {
    /// What `MpInitLib` printed, if anything.
    firmware_cpus: Option<u32>,
    reached_boot_manager: bool,
    elapsed: Duration,
    /// Every run of PM-timer reads between the IPI and the verdict, at 300 us
    /// resolution, so the firmware's individual spin loops can be told apart.
    fine: Vec<Burst>,
    traces: BTreeMap<u32, VcpuTrace>,
    outcomes: Vec<String>,
    /// `(elapsed_us, line)` for every serial line the firmware printed.
    timeline: Vec<(u64, String)>,
    /// What the low 1 MiB looked like when the run ended — the region an
    /// abandoned application processor can reach, because a SIPI leaves it in
    /// real mode with `ds` at zero.
    low_memory: LowMemory,
}

/// The host structures a runaway real-mode CPU could overwrite, checked after
/// the run: the RSDP the firmware re-reads at the end of DXE, the PVH hand-off
/// block it reads it *through*, and the wakeup page the firmware pointed the AP
/// at and then restored.
#[derive(Debug)]
struct LowMemory {
    rsdp_signature: [u8; 8],
    handoff_magic: u32,
    handoff_rsdp: u64,
    /// The first bytes of the page named by `WakeupBufferStart`: what an
    /// abandoned AP is executing once `FreeResetVector()` has put the original
    /// contents back.
    wakeup_page: Vec<u8>,
    wakeup_at: u64,
}

impl LowMemory {
    fn intact(&self) -> bool {
        &self.rsdp_signature == b"RSD PTR "
            && self.handoff_magic == uefi_boot::pvh::XEN_HVM_START_MAGIC_VALUE
            && self.handoff_rsdp == layout::ACPI_RSDP_START
    }
}

/// Printed by `AllocateResetVector()` immediately before `WakeUpAP` sends
/// INIT-SIPI-SIPI: the closest thing to a host-side timestamp for the IPI.
const WAKEUP_LINE: &str = "AP Vector: 16-bit = ";
/// `MpInitLib`'s verdict, printed once the AP sweep is over.
const VERDICT_LINE: &str = "MpInitLib: Find ";

impl BootReport {
    fn line_at(&self, needle: &str) -> Option<u64> {
        self.timeline
            .iter()
            .find(|(_, line)| line.contains(needle))
            .map(|(at, _)| *at)
    }

    /// When the BSP sent INIT-SIPI-SIPI, within a millisecond.
    fn sipi_us(&self) -> Option<u64> {
        self.line_at(WAKEUP_LINE)
    }

    /// When the BSP announced how many processors it had found.
    fn verdict_us(&self) -> Option<u64> {
        self.line_at(VERDICT_LINE)
    }

    /// When the AP thread first woke after the IPI — on KVM this is the INIT
    /// being accepted, which returns to userspace without entering the guest.
    fn ap_wake_us(&self) -> Option<u64> {
        let sipi = self.sipi_us()?;
        self.traces.get(&1)?.first_wake_after(sipi)
    }

    /// When the AP executed its first instruction: the first slice after the
    /// IPI long enough to be guest code rather than a wake-up.
    fn ap_first_us(&self) -> Option<u64> {
        let sipi = self.sipi_us()?;
        self.traces.get(&1)?.first_run_after(sipi)
    }

    /// How long after the IPI the AP got going.
    fn ap_latency_us(&self) -> Option<u64> {
        Some(self.ap_first_us()?.saturating_sub(self.sipi_us()?))
    }

    /// How much time the AP had left when it started: positive means it beat
    /// the BSP's verdict, negative means the BSP had already given up on it.
    fn ap_margin_us(&self) -> Option<i64> {
        Some(self.verdict_us()? as i64 - self.ap_first_us()? as i64)
    }

    fn summary(&self) -> String {
        let cpus = self
            .firmware_cpus
            .map_or_else(|| "?".into(), |n| n.to_string());
        let ms =
            |v: Option<u64>| v.map_or_else(|| "?".into(), |v| format!("{:.1}", v as f64 / 1e3));
        let signed =
            |v: Option<i64>| v.map_or_else(|| "?".into(), |v| format!("{:+.1}", v as f64 / 1e3));
        let stalls: Vec<String> = self
            .traces
            .iter()
            .map(|(index, trace)| {
                format!(
                    "vcpu{index} queued {:.1} ms (worst {:.1} ms)",
                    trace.run_delay_ns as f64 / 1e6,
                    trace.worst_stall_ns as f64 / 1e6,
                )
            })
            .collect();
        format!(
            "found {cpus} CPUs in {:.2} s{}; AP woke {} ms and ran {} ms after \
             the IPI, {} ms before the verdict (verdict at IPI+{} ms); {}",
            self.elapsed.as_secs_f64(),
            if self.reached_boot_manager {
                ""
            } else {
                " (no Boot Manager)"
            },
            ms(self
                .ap_wake_us()
                .and_then(|w| Some(w.saturating_sub(self.sipi_us()?)))),
            ms(self.ap_latency_us()),
            signed(self.ap_margin_us()),
            ms(self
                .verdict_us()
                .and_then(|v| Some(v.saturating_sub(self.sipi_us()?)))),
            stalls.join(", ")
        )
    }

    /// The evidence dump for a boot that lost an AP.
    fn detail(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("  outcomes: {:?}\n", self.outcomes));
        let from = self.sipi_us().map_or(0, |at| at.saturating_sub(20_000));
        for burst in self.fine.iter().take(24) {
            out.push_str(&format!(
                "  pm spin at {:8.1} ms for {:6.1} ms ({:5} reads, {:6.1} ms of guest clock)\n",
                burst.start_us as f64 / 1e3,
                (burst.end_us - burst.start_us) as f64 / 1e3,
                burst.reads,
                f64::from(burst.ticks) * 1e3 / ACPI_PM_TIMER_HZ as f64,
            ));
        }
        // vCPU 0 runs continuously and says nothing useful; the APs are the
        // subject.
        for (index, trace) in self.traces.iter().filter(|(index, _)| **index > 0) {
            let activity: Vec<String> = trace
                .activity
                .iter()
                .filter(|(at, _)| *at >= from)
                .take(10)
                .map(|(at, ran)| format!("{:.1}ms/{}us", *at as f64 / 1e3, ran / 1000))
                .collect();
            out.push_str(&format!(
                "  vcpu{index} on-cpu {:.1} ms, first slices after the IPI: {}\n",
                trace.on_cpu_ns as f64 / 1e6,
                activity.join(" ")
            ));
        }
        for (at, line) in self.timeline.iter().filter(|(at, _)| *at >= from).take(12) {
            out.push_str(&format!("  {:8.1} ms | {line}\n", *at as f64 / 1e3));
        }
        out.push_str(&format!(
            "  low memory {}: rsdp {:?}, handoff magic {:#x} rsdp_paddr {:#x}; \
             wakeup page {:#x} now holds {}\n",
            if self.low_memory.intact() {
                "intact"
            } else {
                "SCRIBBLED"
            },
            String::from_utf8_lossy(&self.low_memory.rsdp_signature),
            self.low_memory.handoff_magic,
            self.low_memory.handoff_rsdp,
            self.low_memory.wakeup_at,
            self.low_memory
                .wakeup_page
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(""),
        ));
        out
    }
}

/// Every run of PM-timer reads with no internal gap longer than `gap` and a
/// span of at least `floor`.
fn bursts(reads: &[(u64, u32)], gap: u64, floor: u64) -> Vec<Burst> {
    let mut out = Vec::new();
    let Some(&(first_at, first_tick)) = reads.first() else {
        return out;
    };
    let (mut start_us, mut start_tick) = (first_at, first_tick);
    let (mut previous, mut previous_tick, mut count) = (first_at, first_tick, 1usize);
    let flush =
        |start_us, start_tick: u32, end_us: u64, end_tick: u32, reads, out: &mut Vec<Burst>| {
            if end_us - start_us >= floor {
                out.push(Burst {
                    start_us,
                    end_us,
                    reads,
                    // The counter is 24 bits and wraps every 4.69 s; a burst is
                    // never that long, so a wrapping difference is the right one.
                    ticks: end_tick.wrapping_sub(start_tick) & 0x00ff_ffff,
                });
            }
        };
    for &(at, tick) in &reads[1..] {
        if at - previous > gap {
            flush(
                start_us,
                start_tick,
                previous,
                previous_tick,
                count,
                &mut out,
            );
            start_us = at;
            start_tick = tick;
            count = 0;
        }
        previous = at;
        previous_tick = tick;
        count += 1;
    }
    flush(
        start_us,
        start_tick,
        previous,
        previous_tick,
        count,
        &mut out,
    );
    out
}

/// The run loop's own diagnostics (the triple-fault site) go to stderr, once
/// per process. `RUST_LOG` still wins when it is set.
fn install_tracing() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .try_init();
    });
}

fn boot_firmware(vcpu_count: u32) -> Result<BootReport, String> {
    install_tracing();
    let firmware = artifact("firmware/CLOUDHV.fd").ok_or("no artifacts/firmware/CLOUDHV.fd")?;
    let started = Instant::now();
    let machine = MachineConfig {
        memory_mib: 2048,
        vcpu_count,
    };
    let hv = Hypervisor::open().map_err(|e| e.to_string())?;
    let mut vm = Vm::new(&hv, &machine).map_err(|e| e.to_string())?;
    let mem_size = machine.memory_mib << 20;

    machine_x86::mptable::write(vm.memory(), machine.vcpu_count).map_err(|e| e.to_string())?;
    machine_x86::acpi::write(vm.memory(), machine.vcpu_count).map_err(|e| e.to_string())?;

    let capture = Capture::new(started);
    let serial =
        SerialConsole::new(vm.fd(), Box::new(capture.clone())).map_err(|e| e.to_string())?;
    let bus = MachineBus::new(serial).with_firmware_platform();

    let image = uefi_boot::FirmwareConfig { firmware }
        .open()
        .map_err(|e| e.to_string())?;
    let boot = uefi_boot::load_pvh(vm.memory(), &image, mem_size).map_err(|e| e.to_string())?;

    let vcpus = vm.take_vcpus();
    for vcpu in &vcpus {
        x86_boot::setup_pvh_sregs(vm.memory(), vcpu).map_err(|e| e.to_string())?;
        if vcpu.index == 0 {
            x86_boot::setup_pvh_regs(vcpu, boot.entry, boot.start_info_addr)
                .map_err(|e| e.to_string())?;
        }
    }

    let reads = Arc::new(Mutex::new(Vec::new()));
    let probe_reads = Arc::clone(&reads);
    let threads = spawn_vcpus(vcpus, |_| {
        Box::new(PmTimerProbe {
            inner: bus.clone(),
            start: started,
            reads: Arc::clone(&probe_reads),
        })
    })
    .map_err(|e| e.to_string())?;

    let stop = Arc::new(AtomicBool::new(false));
    let sampler = {
        let stop = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("apsample".into())
            .spawn(move || sample_vcpus(started, &stop, vcpu_count))
            .map_err(|e| e.to_string())?
    };

    let outcomes = threads.join_or_stop(
        || {
            let text = capture.text();
            text.contains(BOOT_MANAGER) || started.elapsed() >= DEADLINE
        },
        Duration::from_millis(20),
    );
    let elapsed = started.elapsed();
    stop.store(true, Ordering::Release);
    let traces = sampler.join().unwrap_or_default();

    let timeline = capture.timeline();
    // The firmware names the page it points the AP at; read it back, because
    // that is the code an abandoned AP is executing.
    let wakeup_at = timeline
        .iter()
        .find_map(|(_, line)| line.split_once("WakeupBufferStart = "))
        .and_then(|(_, rest)| rest.split(',').next())
        .and_then(|hex| u64::from_str_radix(hex.trim(), 16).ok())
        .unwrap_or(0);
    let mut wakeup_page = vec![0u8; 32];
    if wakeup_at != 0 {
        let _ = vm
            .memory()
            .read_slice(&mut wakeup_page, GuestAddress(wakeup_at));
    }
    let mut rsdp_signature = [0u8; 8];
    let _ = vm
        .memory()
        .read_slice(&mut rsdp_signature, GuestAddress(layout::ACPI_RSDP_START));
    let low_memory = LowMemory {
        rsdp_signature,
        handoff_magic: vm
            .memory()
            .read_obj(GuestAddress(layout::PVH_START_INFO_START))
            .unwrap_or(0),
        handoff_rsdp: vm
            .memory()
            .read_obj(GuestAddress(layout::PVH_START_INFO_START + 0x20))
            .unwrap_or(0),
        wakeup_page,
        wakeup_at,
    };

    let log = capture.text();
    let firmware_cpus = log
        .lines()
        .find_map(|line| {
            line.split_once("MpInitLib: Find ")
                .map(|(_, rest)| rest.to_string())
        })
        .and_then(|rest| rest.split_whitespace().next().and_then(|n| n.parse().ok()));

    let reads = reads.lock().map(|r| r.clone()).unwrap_or_default();
    let fine: Vec<Burst> = {
        let timeline = capture.timeline();
        let at = |needle: &str| {
            timeline
                .iter()
                .find(|(_, line)| line.contains(needle))
                .map(|(at, _)| *at)
        };
        match (at(WAKEUP_LINE), at(VERDICT_LINE)) {
            (Some(from), Some(to)) => bursts(&reads, 300, 0)
                .into_iter()
                .filter(|b| b.end_us >= from && b.start_us <= to + 2_000)
                .collect(),
            _ => Vec::new(),
        }
    };

    Ok(BootReport {
        firmware_cpus,
        reached_boot_manager: log.contains(BOOT_MANAGER),
        elapsed,
        fine,
        traces,
        outcomes: outcomes.iter().map(|o| format!("{o:?}")).collect(),
        timeline,
        low_memory,
    })
}

// ---------------------------------------------------------------------------
// tests

/// One boot, asserting the configured count. The numbers it prints are the
/// point: they say how much of the firmware's 50 ms window this host used.
#[test]
fn the_firmware_finds_every_configured_processor() {
    if !kvm_available() {
        return;
    }
    let report = match boot_firmware(2) {
        Ok(report) => report,
        Err(e) if e.starts_with("no artifacts") => {
            eprintln!("skipping: {e} — run guest/firmware/build-cloudhv.sh");
            return;
        }
        Err(e) => panic!("boot failed: {e}"),
    };
    eprintln!("{}", report.summary());
    assert_eq!(
        report.firmware_cpus,
        Some(2),
        "the firmware found the wrong number of processors\n{}",
        report.detail(),
    );
    assert!(
        report.reached_boot_manager,
        "the firmware did not reach the Boot Manager within {DEADLINE:?}\n{}",
        report.detail(),
    );
    // An abandoned application processor is left running in real mode with `ds`
    // at zero, so the whole low megabyte is in reach — including the RSDP the
    // firmware re-reads at the end of DXE and the PVH block it reads it
    // through.
    assert!(
        report.low_memory.intact(),
        "the host structures in low memory did not survive the boot\n{}",
        report.detail(),
    );
    // The margin is printed rather than asserted: the firmware's own count
    // above is the ground truth, and this figure carries the sampler's
    // half-millisecond resolution.
}

/// The campaign form: boot `$ENTANGLED_AP_BOOTS` times and report the rate.
/// Ignored by default — it is a measurement, not an assertion.
#[test]
#[ignore = "load campaign; set ENTANGLED_AP_BOOTS and run with --nocapture"]
fn ap_startup_campaign() {
    if !kvm_available() {
        return;
    }
    let boots: u32 = std::env::var("ENTANGLED_AP_BOOTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let vcpus: u32 = std::env::var("ENTANGLED_AP_VCPUS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2);
    let mut lost = 0;
    let mut latencies = Vec::new();
    let mut margins: Vec<i64> = Vec::new();
    for iteration in 1..=boots {
        let report = match boot_firmware(vcpus) {
            Ok(report) => report,
            Err(e) => {
                eprintln!("skipping: {e}");
                return;
            }
        };
        if let Some(latency) = report.ap_latency_us() {
            latencies.push(latency);
        }
        if !report.low_memory.intact() {
            eprintln!("  low memory did not survive this boot");
        }
        if let Some(margin) = report.ap_margin_us() {
            margins.push(margin);
        }
        let ok = report.firmware_cpus == Some(vcpus);
        if !ok {
            lost += 1;
        }
        eprintln!(
            "boot {iteration}/{boots} {}: {}",
            if ok { "ok  " } else { "LOST" },
            report.summary()
        );
        if !ok || !report.low_memory.intact() {
            eprint!("{}", report.detail());
        }
    }
    latencies.sort_unstable();
    let percentile = |p: usize| {
        latencies
            .get(latencies.len().saturating_sub(1) * p / 100)
            .copied()
            .unwrap_or_default()
    };
    eprintln!(
        "=== {lost}/{boots} boots lost an AP; SIPI-to-first-instruction \
         p50 {} us, p90 {} us, max {} us",
        percentile(50),
        percentile(90),
        latencies.last().copied().unwrap_or_default(),
    );
}
