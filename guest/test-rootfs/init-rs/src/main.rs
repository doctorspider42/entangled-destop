//! PID 1 of the test initramfs: prints the boot marker the integration
//! tests wait for, optionally runs a scripted probe, then reboots.
//!
//! Probes are requested on the kernel command line and each one prints exactly
//! one `VMHOST_TEST_OK <name> key=value …` or `VMHOST_TEST_FAIL <name> <why>`
//! line, which is all a host harness ever parses (never free-form kernel
//! output). Supported today:
//!
//!   `entangled.blkbench=<mib>`  sequential read of the first `<mib>` MiB of
//!                               `/dev/vda`, reporting bytes and milliseconds
//!                               (used to compare virtio-blk throughput with and
//!                               without the MVP-307 queue-notify offload).
//!   `entangled.pciscan=1`       reports what the kernel enumerated on the PCI
//!                               bus and which virtio drivers bound to it — the
//!                               guest-side evidence for the virtio-pci
//!                               transport (EPIC 19). There is no `lspci` in this
//!                               initramfs, so it reads sysfs directly.
//!   `entangled.trim=<mib>`      fills `<mib>` MiB on `/dev/vda`, frees it and
//!                               asks the kernel to give the space back
//!                               (`FITRIM` on a mounted ext4, or `BLKDISCARD`
//!                               where ext4 is not built in) — the guest half of
//!                               the virtio-blk DISCARD acceptance; the host
//!                               measures the image's allocated size either side.
//!   `entangled.poweroff=1`      power the machine off through ACPI instead of
//!                               rebooting: proves the FADT, the DSDT's `\_S5`
//!                               and the host's ACPI PM block agree. Opt-in,
//!                               because every other test wants the reboot path.
//!   `entangled.heartbeat=<ms>`  print `VMHOST_HEARTBEAT <n>` every `<ms>`
//!                               milliseconds, for ever, instead of rebooting.
//!                               The guest-side evidence for pause and resume
//!                               (ADR-0005): a host that has frozen a VM can
//!                               only prove it by the guest going quiet, and it
//!                               has to be *guest code* that stops, not merely a
//!                               device. Also never returns, so the host decides
//!                               when the VM ends.
//!   `entangled.padprobe=<n>`   the guest half of the gamepad acceptance
//!                               (GAME-2104). Reports what the *kernel* made
//!                               of the virtio-input descriptor — name, ids,
//!                               whether joydev claimed it, how many buttons
//!                               and axes it registered, and the `ABS_INFO`
//!                               ranges it read back — then echoes at most
//!                               `<n>` events the host injects, stopping early
//!                               after 1.5 s of silence so "and nothing after
//!                               that" is answerable too. Two lines:
//!                               `padinfo` is printed *after* the event device
//!                               is open, which is the host's cue that
//!                               injecting will not race the open.
//!   `entangled.netprobe=<ip>/<prefix>,<gateway>,<host>:<port>`
//!                               configures eth0 statically (this initramfs has
//!                               no DHCP client), opens a TCP connection to
//!                               `<host>:<port>` and expects its greeting echoed
//!                               back — the guest-side evidence for a virtio-net
//!                               backend (WHP-1704: the user-mode NAT).

use std::ffi::CString;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::os::fd::AsRawFd as _;
use std::path::Path;
use std::time::{Duration, Instant};

/// Read size for the block probe; large enough to keep the ring busy, small
/// enough that a 64 MiB image still produces many requests.
const CHUNK: usize = 64 * 1024;

/// Never read more than this, whatever the command line asks for: the probe
/// must not turn into an unbounded loop on a malformed cmdline.
const MAX_BENCH_MIB: u64 = 4096;

/// Cap on the PCI functions the scan reports, so a machine that grows a bus full
/// of devices cannot turn one probe line into an unbounded one.
const MAX_SCANNED_FUNCTIONS: usize = 16;

/// `FITRIM` — `_IOWR('X', 121, struct fstrim_range)`, the ioctl `fstrim(8)`
/// issues on a mount point. Spelled out because `libc` does not export it; the
/// cast is because `libc::Ioctl` is `i32` on musl and `u64` on glibc, and the
/// bit pattern is what the kernel compares.
const FITRIM: libc::Ioctl = 0xc018_5879u32 as libc::Ioctl;

/// `BLKDISCARD` — `_IO(0x12, 119)`, the ioctl `blkdiscard(8)` issues on a block
/// device.
const BLKDISCARD: libc::Ioctl = 0x1277u32 as libc::Ioctl;

/// Never echo more than this many input events, whatever the command line
/// asks for: the probe must not become an unbounded loop, and a controller
/// nobody is touching produces nothing at all, so a large number here would
/// only ever mean "wait for the deadline".
const MAX_PAD_EVENTS: usize = 256;

/// How long the gamepad probe waits for the host to inject its sequence. Long
/// enough for a slow boot to have finished settling, short enough that a
/// broken event path is a test failure rather than a hung run.
const PAD_EVENT_WAIT: Duration = Duration::from_secs(15);

/// How long the gamepad probe keeps listening after the last event it saw,
/// once at least one has arrived.
///
/// This is what turns `entangled.padprobe=<n>` from "wait for exactly n" into
/// "collect at most n, then prove the stream stopped". A host test that asks
/// for more events than it injects gets the difference as evidence: a pad that
/// spams the guest after an unplug, or repeats a report, shows up as extra
/// entries in `seq=` instead of being invisible because the probe had already
/// counted enough and left.
const PAD_QUIET: Duration = Duration::from_millis(1500);

/// `EVIOCGABS(axis)` = `_IOR('E', 0x40 + axis, struct input_absinfo)`, spelled
/// out because `libc` does not export the `EVIOC*` family. 24 is
/// `size_of::<AbsInfo>()`; the cast is because `libc::Ioctl` is `i32` on musl
/// and `u64` on glibc, and the bit pattern is what the kernel compares.
const fn eviocgabs(axis: u32) -> libc::Ioctl {
    ((2u32 << 30) | (24u32 << 16) | ((b'E' as u32) << 8) | (0x40 + axis)) as libc::Ioctl
}

/// `struct input_absinfo`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct AbsInfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

/// `struct input_event`: a timestamp in front of type/code/value.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct RawInputEvent {
    time: libc::timeval,
    event_type: u16,
    code: u16,
    value: i32,
}

/// How long the trim probe waits for `/dev/vda` to be created. virtio-blk
/// probes asynchronously, so PID 1 can win the race on a fast boot.
const DEVICE_WAIT: Duration = Duration::from_secs(5);

fn main() {
    // The marker must match linux_boot::GUEST_READY_MARKER. Printed before any
    // probe runs, so the host's time-to-ready measurement is pure boot time.
    println!("VMHOST_GUEST_READY");

    // /proc is where the command line lives, so it has to be mounted before we
    // can find out whether a probe was requested at all.
    mount("proc", "/proc", "proc");
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    if param(&cmdline, "entangled.pciscan=").is_some() {
        // /sys before /dev: the scan wants sysfs, and it runs first so its
        // verdict is on the console even if the block probe then hangs.
        mount("sysfs", "/sys", "sysfs");
        pci_scan();
    }
    if let Some(mib) = param(&cmdline, "entangled.blkbench=").and_then(|v| v.parse::<u64>().ok()) {
        mount("devtmpfs", "/dev", "devtmpfs");
        blk_bench(mib.min(MAX_BENCH_MIB));
    }
    if let Some(mib) = param(&cmdline, "entangled.trim=").and_then(|v| v.parse::<u64>().ok()) {
        mount("devtmpfs", "/dev", "devtmpfs");
        mount("sysfs", "/sys", "sysfs");
        trim_probe(mib);
    }
    if let Some(spec) = param(&cmdline, "entangled.netprobe=") {
        net_probe(&spec);
    }
    if let Some(count) = param(&cmdline, "entangled.padprobe=").and_then(|v| v.parse::<usize>().ok())
    {
        mount("devtmpfs", "/dev", "devtmpfs");
        pad_probe(count.min(MAX_PAD_EVENTS));
    }

    if param(&cmdline, "entangled.poweroff=").as_deref() == Some("1") {
        acpi_power_off();
    }

    if let Some(period) = param(&cmdline, "entangled.heartbeat=").and_then(|v| v.parse::<u64>().ok())
    {
        heartbeat(period);
    }

    // Restart, not power-off, by default: the reboot path works on every
    // machine we boot, ACPI or not. With `reboot=k` the kernel's restart chain
    // ends in a triple fault, which reaches the host as KVM_EXIT_SHUTDOWN and
    // terminates the VM cleanly. `entangled.poweroff=1` takes the ACPI route
    // instead.
    // SAFETY: plain syscalls; as PID 1 we hold CAP_SYS_BOOT. tcdrain flushes
    // the serial console before the reboot triple-faults the machine.
    unsafe {
        libc::tcdrain(1);
        libc::sync();
        libc::reboot(libc::LINUX_REBOOT_CMD_RESTART);
    }
    // Unreachable unless the kernel refuses; never return from PID 1 (that
    // would panic the kernel with a confusing message).
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

/// Prints a numbered line every `period_ms`, for ever.
///
/// The one probe whose *absence* is the measurement: the host pauses the VM and
/// asserts that no further line arrives. That is why it sleeps rather than
/// spinning (a spinning guest would still be stopped, but it would also make the
/// host's own timing noisy) and why it never returns — a probe that ended would
/// make "the console went quiet" ambiguous.
///
/// The period is clamped: the command line is host-controlled here, but the same
/// init runs in guests booted from a profile, and a zero-millisecond heartbeat
/// would be a serial-console flood rather than a probe.
fn heartbeat(period_ms: u64) {
    let period = Duration::from_millis(period_ms.clamp(10, 10_000));
    let mut tick: u64 = 0;
    loop {
        println!("VMHOST_HEARTBEAT {tick}");
        // Flushed explicitly: stdout to a serial console is line-buffered only
        // when it is a tty, and the host is counting *arrivals*.
        let _ = std::io::stdout().flush();
        tick = tick.wrapping_add(1);
        std::thread::sleep(period);
    }
}

/// Asks the kernel to power the machine off through ACPI and reports what
/// happened on the way in.
///
/// `reboot(LINUX_REBOOT_CMD_POWER_OFF)` only reaches the ACPI path if
/// `acpi_sleep_init()` registered a power-off handler, which needs a FADT *and*
/// a `\_S5` package in the DSDT. When it did not, the kernel prints
/// "Power off not available" (older kernels) or halts, and the syscall returns
/// here — so a returning syscall is a real failure, not a race.
fn acpi_power_off() {
    // Which ACPI tables the guest actually parsed, straight from the kernel —
    // stronger evidence than a dmesg line, and it names them.
    mount("sysfs", "/sys", "sysfs");
    let mut tables: Vec<String> = std::fs::read_dir("/sys/firmware/acpi/tables")
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name != "data" && name != "dynamic")
                .collect()
        })
        .unwrap_or_default();
    tables.sort();
    // What the host harness greps for. Printed before the syscall, because
    // afterwards there is no userspace left to print anything.
    println!(
        "VMHOST_TEST_OK poweroff via=acpi-s5 tables={}",
        if tables.is_empty() {
            "none".to_string()
        } else {
            tables.join(",")
        }
    );
    // SAFETY: plain syscalls; as PID 1 we hold CAP_SYS_BOOT. tcdrain flushes the
    // serial console first, because an ACPI power-off stops the machine between
    // one instruction and the next.
    unsafe {
        libc::tcdrain(1);
        libc::sync();
        libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF);
    }
    // Only reachable if the kernel had no way to power off.
    println!("VMHOST_TEST_FAIL poweroff kernel-refused-power-off");
}

/// Proves the virtio-net path end to end: configures `eth0` statically,
/// connects out over TCP and expects the greeting echoed back.
///
/// `spec` is `<ip>/<prefix>,<gateway>,<host>:<port>`. Static configuration
/// rather than DHCP because this initramfs has no DHCP client and the bootstrap
/// kernel no `CONFIG_IP_PNP` — the ioctls below *are* the whole network stack
/// setup, which also keeps the probe's evidence about the datapath rather than
/// about a client implementation.
///
/// The echo matters: a SYN alone proves the guest's TX path, but only bytes
/// coming *back* prove RX delivery — frames queued by the host, an RX interrupt
/// raised, buffers completed.
fn net_probe(spec: &str) {
    let fail = |why: String| println!("VMHOST_TEST_FAIL netprobe {why}");
    let Some((address, rest)) = spec.split_once(',') else {
        return fail("malformed-spec".into());
    };
    let Some((gateway, target)) = rest.split_once(',') else {
        return fail("malformed-spec".into());
    };
    let Some((ip, prefix)) = address.split_once('/') else {
        return fail("malformed-address".into());
    };
    let (Ok(ip), Ok(prefix), Ok(gateway)) = (
        ip.parse::<Ipv4Addr>(),
        prefix.parse::<u32>(),
        gateway.parse::<Ipv4Addr>(),
    ) else {
        return fail("malformed-address".into());
    };
    let Ok(SocketAddr::V4(target)) = target.parse::<SocketAddr>() else {
        return fail("malformed-target".into());
    };
    let netmask = Ipv4Addr::from(u32::MAX.checked_shl(32 - prefix.min(32)).unwrap_or(0));

    if let Err(why) = configure_eth0(ip, netmask, gateway) {
        return fail(why);
    }

    let started = Instant::now();
    let mut stream = match TcpStream::connect_timeout(&SocketAddr::V4(target), CONNECT_TIMEOUT) {
        Ok(stream) => stream,
        Err(e) => return fail(format!("connect-{target}:{e}")),
    };
    let greeting = b"ENTANGLED_NETPROBE ping\n";
    if let Err(e) = stream.write_all(greeting) {
        return fail(format!("send:{e}"));
    }
    let _ = stream.set_read_timeout(Some(CONNECT_TIMEOUT));
    let mut echoed = Vec::new();
    let mut buf = [0u8; 64];
    while echoed.len() < greeting.len() {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => echoed.extend_from_slice(&buf[..n]),
            Err(e) => return fail(format!("recv:{e}")),
        }
    }
    if !echoed.starts_with(greeting) {
        return fail(format!(
            "echo-mismatch:{}",
            String::from_utf8_lossy(&echoed).trim()
        ));
    }
    println!(
        "VMHOST_TEST_OK netprobe ip={ip}/{prefix} gw={gateway} target={target} \
         echoed={} ms={}",
        echoed.len(),
        started.elapsed().as_millis()
    );
}

/// How long the network probe waits for a connect and for the echo.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Brings `eth0` up with a static address and a default route, through the
/// classic SIOCSIF*/SIOCADDRT ioctls — the smallest network configuration that
/// exists on every Linux, no netlink library required.
fn configure_eth0(ip: Ipv4Addr, netmask: Ipv4Addr, gateway: Ipv4Addr) -> Result<(), String> {
    // SAFETY: a plain socket() call; the fd is closed below.
    let sock = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if sock < 0 {
        return Err(format!("socket:{}", std::io::Error::last_os_error()));
    }
    // Everything below returns through here, so the fd cannot leak.
    let result = configure_eth0_on(sock, ip, netmask, gateway);
    // SAFETY: closing the fd opened above, exactly once.
    unsafe { libc::close(sock) };
    result
}

/// A `sockaddr` holding an IPv4 address, as the ifreq/rtentry ioctls want it.
fn inet_sockaddr(addr: Ipv4Addr) -> libc::sockaddr {
    let sin = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: 0,
        sin_addr: libc::in_addr {
            s_addr: u32::from_ne_bytes(addr.octets()),
        },
        sin_zero: [0; 8],
    };
    // SAFETY: sockaddr_in is the AF_INET arm of sockaddr; both are plain,
    // same-size C structs and every byte of the source is initialised.
    unsafe { std::mem::transmute(sin) }
}

fn configure_eth0_on(
    sock: libc::c_int,
    ip: Ipv4Addr,
    netmask: Ipv4Addr,
    gateway: Ipv4Addr,
) -> Result<(), String> {
    const NAME: &[u8] = b"eth0\0";
    // SAFETY: ifreq is a plain C struct for which zero is a valid pattern.
    let mut ifr: libc::ifreq = unsafe { std::mem::zeroed() };
    for (dst, src) in ifr.ifr_name.iter_mut().zip(NAME) {
        *dst = *src as libc::c_char;
    }

    // The SIOC* constants are `c_ulong` while musl's `ioctl` takes a `c_int`
    // request; the values are small, so the cast is lossless on both ABIs.
    fn apply(
        sock: libc::c_int,
        ifr: &mut libc::ifreq,
        request: libc::c_ulong,
        what: &str,
    ) -> Result<(), String> {
        // SAFETY: `ifr` is a live, fully initialised ifreq and `request` is one
        // of the SIOCSIF* codes that read exactly one ifreq.
        if unsafe { libc::ioctl(sock, request as libc::Ioctl, ifr) } < 0 {
            return Err(format!("{what}:{}", std::io::Error::last_os_error()));
        }
        Ok(())
    }

    ifr.ifr_ifru.ifru_addr = inet_sockaddr(ip);
    apply(sock, &mut ifr, libc::SIOCSIFADDR, "set-address")?;
    ifr.ifr_ifru.ifru_addr = inet_sockaddr(netmask);
    apply(sock, &mut ifr, libc::SIOCSIFNETMASK, "set-netmask")?;
    ifr.ifr_ifru.ifru_flags = (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
    apply(sock, &mut ifr, libc::SIOCSIFFLAGS, "link-up")?;

    // The default route through the gateway: what turns "the segment" into
    // "everywhere", and the counterpart of the `set_any_ip` finding on the host
    // side — without a route the guest's stack refuses the connect locally.
    // SAFETY: rtentry is a plain C struct for which zero is a valid pattern.
    let mut route: libc::rtentry = unsafe { std::mem::zeroed() };
    route.rt_dst = inet_sockaddr(Ipv4Addr::UNSPECIFIED);
    route.rt_genmask = inet_sockaddr(Ipv4Addr::UNSPECIFIED);
    route.rt_gateway = inet_sockaddr(gateway);
    route.rt_flags = libc::RTF_UP | libc::RTF_GATEWAY;
    // SAFETY: `route` is a live, fully initialised rtentry and SIOCADDRT reads
    // exactly one.
    if unsafe { libc::ioctl(sock, libc::SIOCADDRT as libc::Ioctl, &mut route) } < 0 {
        return Err(format!(
            "add-default-route:{}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

/// Value of a `key=` parameter on the kernel command line.
fn param(cmdline: &str, key: &str) -> Option<String> {
    cmdline
        .split_ascii_whitespace()
        .find_map(|word| word.strip_prefix(key))
        .map(|value| value.to_string())
}

/// Mounts one pseudo-filesystem, ignoring failure: an already-mounted or
/// unavailable filesystem is not worth aborting the boot marker over.
fn mount(source: &str, target: &str, fstype: &str) {
    let _ = std::fs::create_dir_all(target);
    let (Ok(source), Ok(target), Ok(fstype)) = (
        CString::new(source),
        CString::new(target),
        CString::new(fstype),
    ) else {
        return;
    };
    // SAFETY: three valid NUL-terminated strings that outlive the call and a
    // null data pointer, which is how pseudo-filesystems are mounted.
    unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            0,
            std::ptr::null(),
        );
    }
}

/// Drives the guest kernel's own discard path against `/dev/vda` and reports
/// what it managed to give back — the guest half of the thin-provisioning
/// acceptance (`VIRTIO_BLK_F_DISCARD`).
///
/// Two routes, in order of how much they prove:
///
/// 1. **`FITRIM`** — mount the ext4 filesystem the host put on the disk, fill a
///    file with `mib` MiB, delete it, then run the same `FITRIM` ioctl that
///    `fstrim(8)` runs. That is the real production path: ext4 walks its block
///    groups, calls `blkdev_issue_discard` for the free extents, and the block
///    layer turns those into `VIRTIO_BLK_T_DISCARD` requests.
/// 2. **`BLKDISCARD`** — used when the mount fails, which it will on a kernel
///    that has ext4 as a module (the Debian installer kernel does). Writes the
///    same `mib` MiB straight to the device, then discards that range with the
///    ioctl `blkdiscard(8)` uses. One layer shallower, and it exercises the same
///    `blkdev_issue_discard` → virtio-blk path with the same guest-supplied
///    ranges.
///
/// Either way the host measures the image's allocated size before and after, so
/// the number that matters is not printed here at all — this only has to make
/// the guest really ask.
fn trim_probe(mib: u64) {
    let fail = |why: String| println!("VMHOST_TEST_FAIL trim {why}");
    let bytes = mib.min(MAX_BENCH_MIB).saturating_mul(1 << 20);
    if bytes == 0 {
        return fail("nothing-to-fill".into());
    }

    // The device node appears when virtio-blk finishes probing, which can be
    // after PID 1 starts. Wait for it rather than racing it.
    if let Err(why) = wait_for_device(Path::new("/dev/vda")) {
        return fail(why);
    }

    // What the block layer thinks the device can discard: straight from sysfs,
    // so the config-space fields the device published are visible in the report
    // rather than assumed.
    let limits = discard_limits();

    match fitrim_route(bytes) {
        Ok(trimmed) => println!(
            "VMHOST_TEST_OK trim path=fitrim filled={bytes} trimmed={trimmed} {limits}"
        ),
        // The device refused the trim, or the filesystem could not run it. The
        // filesystem is intact and must stay that way: falling back to a raw
        // BLKDISCARD here would write straight over it, and a later boot of the
        // same image would then have no filesystem left to trim. This branch is
        // exactly what the host's "reclaim withheld" phase expects to see.
        Err(TrimFailure::Refused(why)) => fail(format!("fitrim={why}")),
        // No ext4 in this kernel at all, so there was never a filesystem to
        // damage: drive the block device's own discard instead.
        Err(TrimFailure::NoFilesystem(why)) => match blkdiscard_route(bytes) {
            Ok(discarded) => println!(
                "VMHOST_TEST_OK trim path=blkdiscard filled={bytes} trimmed={discarded} \
                 fitrim={why} {limits}"
            ),
            Err(second) => fail(format!("fitrim={why} blkdiscard={second}")),
        },
    }
}

/// Why the `FITRIM` route produced no number — and, crucially, whether a
/// filesystem exists that a fallback would destroy.
enum TrimFailure {
    /// The device could not be mounted as ext4, so there is nothing to lose.
    NoFilesystem(String),
    /// A filesystem is there; the fill, the unlink or the ioctl failed.
    Refused(String),
}

/// Waits up to [`DEVICE_WAIT`] for a device node to appear, reporting what /dev
/// did contain if it never does — a missing `/dev/vda` and a `/dev` that was
/// never populated are different failures.
fn wait_for_device(path: &Path) -> Result<(), String> {
    let deadline = Instant::now() + DEVICE_WAIT;
    while !path.exists() {
        if Instant::now() >= deadline {
            let mut seen: Vec<String> = std::fs::read_dir("/dev")
                .map(|entries| {
                    entries
                        .filter_map(|e| e.ok())
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .take(24)
                        .collect()
                })
                .unwrap_or_default();
            seen.sort();
            return Err(format!(
                "no-{} dev-has={}",
                path.display(),
                if seen.is_empty() {
                    "nothing".into()
                } else {
                    seen.join(",")
                }
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

/// `/sys/block/vda/queue/discard_*`, as the guest kernel derived them from the
/// device's config space. `discard_granularity` of 0 means the kernel does not
/// believe the device can discard at all, which is the failure this reports
/// rather than hides.
fn discard_limits() -> String {
    let read = |name: &str| {
        std::fs::read_to_string(format!("/sys/block/vda/queue/{name}"))
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "?".into())
    };
    format!(
        "granularity={} max_discard={} max_write_zeroes={}",
        read("discard_granularity"),
        read("discard_max_bytes"),
        read("write_zeroes_max_bytes")
    )
}

/// Route 1: a real filesystem, a real `fstrim`.
fn fitrim_route(bytes: u64) -> Result<u64, TrimFailure> {
    let _ = std::fs::create_dir_all("/mnt");
    let (Ok(source), Ok(target), Ok(fstype)) = (
        CString::new("/dev/vda"),
        CString::new("/mnt"),
        CString::new("ext4"),
    ) else {
        return Err(TrimFailure::NoFilesystem("bad-strings".into()));
    };
    // SAFETY: three valid NUL-terminated strings that outlive the call, no
    // flags and a null options pointer — the ordinary way to mount a block
    // device.
    let rc = unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            0,
            std::ptr::null(),
        )
    };
    if rc != 0 {
        // ENODEV here means the kernel has no ext4 at all (it is a module in
        // the Debian installer kernel), which is worth saying out loud rather
        // than leaving as a bare errno.
        return Err(TrimFailure::NoFilesystem(format!(
            "mount-errno-{}-fs[{}]",
            std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or_default(),
            std::fs::read_to_string("/proc/filesystems")
                .unwrap_or_default()
                .lines()
                .filter(|line| !line.starts_with("nodev"))
                .map(|line| line.trim().to_string())
                .collect::<Vec<_>>()
                .join("+")
        )));
    }

    // Fill, sync, delete, sync: the blocks have to have really been allocated
    // and really been freed before FITRIM can hand them back.
    let path = Path::new("/mnt/fill.bin");
    if let Err(e) = fill_file(path, bytes) {
        let _ = std::fs::remove_file(path);
        return Err(TrimFailure::Refused(format!("fill:{e}")));
    }
    if let Err(e) = std::fs::remove_file(path) {
        return Err(TrimFailure::Refused(format!("unlink:{e}")));
    }
    // SAFETY: no arguments, no memory; flushes the ext4 journal so the freed
    // extents are visible to the FITRIM walk.
    unsafe { libc::sync() };

    let dir = std::fs::File::open("/mnt")
        .map_err(|e| TrimFailure::Refused(format!("open-mnt:{e}")))?;
    // struct fstrim_range { __u64 start; __u64 len; __u64 minlen; }
    let mut range: [u64; 3] = [0, u64::MAX, 0];
    // SAFETY: `dir` is a live directory fd on the mounted filesystem and
    // `range` is a valid, fully initialised 24-byte fstrim_range for the
    // duration of the call — exactly what FITRIM reads and writes back.
    let rc = unsafe { libc::ioctl(dir.as_raw_fd(), FITRIM, &mut range) };
    if rc != 0 {
        let errno = std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or_default();
        // Unmount either way: the filesystem must survive a refused trim
        // untouched, because the host boots this same image again.
        if let Ok(target) = CString::new("/mnt") {
            // SAFETY: one valid NUL-terminated path that outlives the call.
            unsafe { libc::umount(target.as_ptr()) };
        }
        return Err(TrimFailure::Refused(format!("fitrim-errno-{errno}")));
    }
    // FITRIM writes the number of bytes trimmed back into `len`.
    let trimmed = range[1];
    // Unmount so the host sees a clean filesystem afterwards.
    if let Ok(target) = CString::new("/mnt") {
        // SAFETY: one valid NUL-terminated path that outlives the call.
        unsafe { libc::umount(target.as_ptr()) };
    }
    Ok(trimmed)
}

/// Route 2: no filesystem, the raw block-device discard `blkdiscard(8)` uses.
fn blkdiscard_route(bytes: u64) -> Result<u64, String> {
    let mut device = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/vda")
        .map_err(|e| format!("open:{e}"))?;
    let chunk = vec![0xa5u8; CHUNK];
    let mut written = 0u64;
    while written < bytes {
        let n = ((bytes - written) as usize).min(chunk.len());
        device
            .write_all(&chunk[..n])
            .map_err(|e| format!("write:{e}"))?;
        written += n as u64;
    }
    device.sync_all().map_err(|e| format!("sync:{e}"))?;

    // BLKDISCARD takes { u64 start; u64 len; }.
    let range: [u64; 2] = [0, bytes];
    // SAFETY: `device` is a live block-device fd opened for writing and `range`
    // is a valid, fully initialised 16-byte argument for the duration of the
    // call. The range is inside the device: the write above succeeded over
    // exactly it.
    let rc = unsafe { libc::ioctl(device.as_raw_fd(), BLKDISCARD, &range) };
    if rc != 0 {
        return Err(format!(
            "errno-{}",
            std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or_default()
        ));
    }
    device.sync_all().map_err(|e| format!("post-sync:{e}"))?;
    Ok(bytes)
}

/// Writes `bytes` bytes of recognisable data to `path` and fsyncs it, so the
/// filesystem really has allocated blocks to free afterwards.
fn fill_file(path: &Path, bytes: u64) -> std::io::Result<()> {
    let mut file = std::fs::File::create(path)?;
    let chunk = vec![0x5au8; CHUNK];
    let mut written = 0u64;
    while written < bytes {
        let n = ((bytes - written) as usize).min(chunk.len());
        file.write_all(&chunk[..n])?;
        written += n as u64;
    }
    file.sync_all()
}

/// The guest half of the gamepad acceptance (GAME-2104).
///
/// The interesting question is not "did an event arrive" but **"what did the
/// kernel make of the descriptor"** — a virtio-input device the host is happy
/// with can still be registered as a tablet, or as a joystick with the wrong
/// axis ranges, or not handed to `joydev` at all, and every one of those is a
/// pad no game can use. So the probe reports the kernel's own conclusions:
///
/// * from `/proc/bus/input/devices` — the name, the `input_id`, how many `KEY`
///   and `ABS` codes the input core registered, and which handlers claimed the
///   device (`js0` there is `joydev` saying yes);
/// * from `EVIOCGABS` on the event node — the ranges the kernel read out of
///   our `ABS_INFO`, which is the descriptor coming back the other way.
///
/// Then it echoes the first `count` events the host injects, so the whole path
/// (host push → virtqueue → `virtio_input` → input core → evdev) is proven end
/// to end rather than inferred from the device merely existing.
///
/// Two lines, and the order matters: `padinfo` is printed **after** the event
/// node is open, because evdev only buffers for clients that already exist —
/// a host that injected on seeing the device would race the open and lose the
/// events.
fn pad_probe(count: usize) {
    let Some(device) = find_input_device("Entangled Gamepad") else {
        let names = input_device_names().join(",");
        println!(
            "VMHOST_TEST_FAIL padprobe not-enumerated devices={}",
            if names.is_empty() { "none" } else { &names }
        );
        return;
    };

    let Some(node) = device.event_node.clone() else {
        println!(
            "VMHOST_TEST_FAIL padprobe no-event-node handlers={}",
            if device.handlers.is_empty() {
                "none".into()
            } else {
                device.handlers.join(",")
            }
        );
        return;
    };
    let path = format!("/dev/input/{node}");
    if let Err(why) = wait_for_device(Path::new(&path)) {
        println!("VMHOST_TEST_FAIL padprobe {why}");
        return;
    }
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) => {
            println!("VMHOST_TEST_FAIL padprobe cannot-open-{node} {error}");
            return;
        }
    };
    let fd = file.as_raw_fd();

    // The descriptor, read back through the kernel. ABS_X (0) is a stick,
    // ABS_Z (2) a trigger and ABS_HAT0X (0x10) the D-pad: one of each, because
    // they are the three different shapes the device publishes.
    let range = |axis: u32| match abs_info(fd, axis) {
        Some(info) => format!(
            "{}:{}:{}:{}",
            info.minimum, info.maximum, info.fuzz, info.flat
        ),
        None => "absent".to_string(),
    };
    // "joydev claimed it" is two claims, and only the second one matters to a
    // game: the handler bound (`Handlers=` mentions a `js*`) *and* the node it
    // promised is really there and really opens. A `js0` in that line with no
    // openable `/dev/input/js0` behind it is what a missing devtmpfs entry or
    // a permission problem looks like, and it is worth telling apart from
    // joydev never having bound at all.
    let js_node = device
        .handlers
        .iter()
        .find(|handler| handler.starts_with("js"))
        .cloned();
    let js_open = js_node
        .as_ref()
        .is_some_and(|js| std::fs::File::open(format!("/dev/input/{js}")).is_ok());
    // Whether the *kernel* has a `joydev` handler at all, which is a different
    // question from whether it bound to this device. `CONFIG_INPUT_JOYDEV` is a
    // separate symbol from `CONFIG_INPUT_EVDEV` and is a module in a stock
    // distribution kernel, so `js=0` on a kernel without it says nothing about
    // the descriptor — and a host test that could not tell the two apart would
    // report the wrong bug, loudly, at whoever changed the device last.
    let joydev_present = std::fs::read_to_string("/proc/bus/input/handlers")
        .unwrap_or_default()
        .lines()
        .any(|line| {
            line.split_ascii_whitespace()
                .any(|field| field == "Name=joydev")
        });
    println!(
        "VMHOST_TEST_OK padinfo name={} bus={:04x} vendor={:04x} product={:04x} \
version={:04x} node={} js={} jsnode={} jsopen={} joydev={} handlers={} keys={} \
axes={} absx={} absz={} abshat={}",
        device.name.replace(' ', "_"),
        device.bus,
        device.vendor,
        device.product,
        device.version,
        node,
        u8::from(js_node.is_some()),
        js_node.as_deref().unwrap_or("none"),
        u8::from(js_open),
        u8::from(joydev_present),
        device.handlers.join(","),
        device.key_count,
        device.abs_count,
        range(0x00),
        range(0x02),
        range(0x10),
    );

    // …and now the round trip. Blocking reads with a deadline enforced by the
    // kernel rather than by a spin: `poll(2)` on the one descriptor.
    //
    // Two deadlines, because the interesting failure is not only "nothing
    // arrived". Until the first event the probe waits [`PAD_EVENT_WAIT`], the
    // patience a slow boot needs; after it, [`PAD_QUIET`] of silence ends the
    // collection. So a host that asks for more events than it injects is
    // asking "and then nothing more, yes?", and gets an answer.
    let mut seen: Vec<String> = Vec::new();
    let mut syn = 0usize;
    let mut deadline = Instant::now() + PAD_EVENT_WAIT;
    while seen.len() + syn < count && Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let millis = i32::try_from(remaining.as_millis()).unwrap_or(i32::MAX);
        let mut fds = [libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        }];
        // SAFETY: one initialised `pollfd` in local storage, a matching count,
        // and a descriptor kept alive by `file` for the whole call. `poll`
        // writes only `revents`.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), 1, millis) };
        if ready <= 0 {
            break;
        }
        let mut buffer = [RawInputEvent::default(); 32];
        // SAFETY: `fd` is live, and the destination is a local array of
        // `#[repr(C)]` plain-old-data whose own size in bytes is the length —
        // so the kernel cannot write past it and any bytes it does write are a
        // valid value.
        let read = unsafe {
            libc::read(
                fd,
                buffer.as_mut_ptr().cast::<libc::c_void>(),
                std::mem::size_of_val(&buffer),
            )
        };
        if read <= 0 {
            break;
        }
        let records = (read as usize) / std::mem::size_of::<RawInputEvent>();
        for event in buffer.iter().take(records) {
            match event.event_type {
                0x00 => syn += 1,
                0x01 => seen.push(format!("k{:x}={}", event.code, event.value)),
                0x03 => seen.push(format!("a{:x}={}", event.code, event.value)),
                other => seen.push(format!("t{other:x}c{:x}={}", event.code, event.value)),
            }
        }
        if records > 0 {
            deadline = Instant::now() + PAD_QUIET;
        }
    }
    println!(
        "VMHOST_TEST_OK padprobe events={} syn={} seq={}",
        seen.len(),
        syn,
        if seen.is_empty() {
            "none".to_string()
        } else {
            seen.join(",")
        }
    );
}

/// One device as `/proc/bus/input/devices` describes it.
#[derive(Debug, Default)]
struct InputDeviceInfo {
    name: String,
    bus: u32,
    vendor: u32,
    product: u32,
    version: u32,
    handlers: Vec<String>,
    event_node: Option<String>,
    /// Set bits in the `B: KEY=` bitmap — the number of buttons the *input
    /// core* registered, not the number the device claimed.
    key_count: u32,
    /// Set bits in the `B: ABS=` bitmap.
    abs_count: u32,
}

/// Names of every input device the kernel registered, for a failure message.
fn input_device_names() -> Vec<String> {
    std::fs::read_to_string("/proc/bus/input/devices")
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.strip_prefix("N: Name=\""))
        .map(|rest| rest.trim_end_matches('"').replace(' ', "_"))
        .collect()
}

/// Finds one device by name in `/proc/bus/input/devices`.
///
/// That file rather than sysfs because it carries the `Handlers=` line, which
/// is the only place the kernel says out loud that `joydev` bound the device —
/// and "is it a joystick" is half of what this probe exists to answer.
fn find_input_device(want: &str) -> Option<InputDeviceInfo> {
    let text = std::fs::read_to_string("/proc/bus/input/devices").ok()?;
    for block in text.split("\n\n") {
        let mut info = InputDeviceInfo::default();
        let mut matched = false;
        for line in block.lines() {
            let line = line.trim_end();
            if let Some(rest) = line.strip_prefix("I: ") {
                for field in rest.split_ascii_whitespace() {
                    let Some((key, value)) = field.split_once('=') else {
                        continue;
                    };
                    let value = u32::from_str_radix(value, 16).unwrap_or(0);
                    match key {
                        "Bus" => info.bus = value,
                        "Vendor" => info.vendor = value,
                        "Product" => info.product = value,
                        "Version" => info.version = value,
                        _ => {}
                    }
                }
            } else if let Some(rest) = line.strip_prefix("N: Name=\"") {
                info.name = rest.trim_end_matches('"').to_string();
                matched = info.name == want;
            } else if let Some(rest) = line.strip_prefix("H: Handlers=") {
                info.handlers = rest.split_ascii_whitespace().map(str::to_string).collect();
                info.event_node = info
                    .handlers
                    .iter()
                    .find(|h| h.starts_with("event"))
                    .cloned();
            } else if let Some(rest) = line.strip_prefix("B: KEY=") {
                info.key_count = count_bitmap_bits(rest);
            } else if let Some(rest) = line.strip_prefix("B: ABS=") {
                info.abs_count = count_bitmap_bits(rest);
            }
        }
        if matched {
            return Some(info);
        }
    }
    None
}

/// Counts set bits in a `/proc/bus/input/devices` bitmap: space-separated hex
/// words, most significant first. Only the population matters here, not which
/// bits, so the word order is irrelevant.
fn count_bitmap_bits(text: &str) -> u32 {
    text.split_ascii_whitespace()
        .filter_map(|word| u64::from_str_radix(word, 16).ok())
        .map(u64::count_ones)
        .sum()
}

/// `EVIOCGABS(axis)`.
fn abs_info(fd: std::os::fd::RawFd, axis: u32) -> Option<AbsInfo> {
    let mut info = AbsInfo::default();
    // SAFETY: `fd` is live for the call; the request is `EVIOCGABS(axis)`,
    // whose encoded payload size is `size_of::<AbsInfo>()`, and the
    // destination is one such `#[repr(C)]` struct in local storage.
    let result = unsafe { libc::ioctl(fd, eviocgabs(axis), std::ptr::addr_of_mut!(info)) };
    (result >= 0).then_some(info)
}

/// Reports what the kernel found on the PCI bus, and how much of it bound to a
/// virtio driver.
///
/// The whole point of the virtio-pci transport is that the guest discovers its
/// devices instead of being told where they are, so the evidence has to come from
/// the guest's own enumeration. `/sys/bus/pci/devices/*` is that enumeration:
/// each entry's `vendor`, `device` and `class` are what the kernel read out of
/// configuration space, and a `driver` symlink means a driver claimed it.
///
/// One line, machine-readable, like every other probe:
/// `VMHOST_TEST_OK pciscan functions=2 virtio=1 bound=1 msix=2 devices=8086:0d57/060000,1af4:1042/018000:virtio-pci`
///
/// `msix` is the total number of message vectors the kernel allocated across the
/// virtio functions (`msi_irqs/`, which exists only when MSI or MSI-X is enabled).
/// Zero means every function is on INTx.
fn pci_scan() {
    let root = Path::new("/sys/bus/pci/devices");
    let Ok(entries) = std::fs::read_dir(root) else {
        // No sysfs directory at all means the kernel has no PCI bus — either
        // CONFIG_PCI is off or configuration mechanism #1 did not answer.
        println!("VMHOST_TEST_FAIL pciscan no-pci-bus-in-sysfs");
        return;
    };

    // Sorted so the line is stable across boots: readdir order is not.
    let mut addresses: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    addresses.sort();

    let mut described = Vec::new();
    let mut functions = 0usize;
    let mut virtio = 0usize;
    let mut bound = 0usize;
    let mut msix = 0usize;
    for address in addresses.iter().take(MAX_SCANNED_FUNCTIONS) {
        let dir = root.join(address);
        let field = |name: &str| {
            std::fs::read_to_string(dir.join(name))
                .map(|v| v.trim().trim_start_matches("0x").to_string())
                .unwrap_or_default()
        };
        let (vendor, device, class) = (field("vendor"), field("device"), field("class"));
        functions += 1;
        // 0x1af4 is the virtio vendor; a modern device id is 0x1040 + type.
        let is_virtio = vendor == "1af4";
        if is_virtio {
            virtio += 1;
            // `msi_irqs/` exists, with one entry per allocated vector, exactly
            // when the kernel has MSI or MSI-X enabled on the function. Counting
            // the entries is the guest saying "I am using N message vectors on
            // this device", which no interrupt count can tell you.
            let vectors = std::fs::read_dir(dir.join("msi_irqs"))
                .map(|entries| entries.flatten().count())
                .unwrap_or(0);
            msix += vectors;
            println!("entangled-pciscan: {address} msi_irqs={vectors}");
            // Which IRQ the kernel settled on for this function. Worth reporting
            // on its own: x86 without ACPI cannot *route* a PCI interrupt, so it
            // logs "probably buggy MP table" and keeps the line the host wrote
            // into the interrupt_line register. If that fell through to 0, INTx
            // is broken even though everything else looks fine.
            println!("entangled-pciscan: {address} irq={}", field("irq"));
        }
        // The driver symlink's target name is the driver that claimed it, which
        // for a modern virtio function must be `virtio-pci`.
        let driver = std::fs::read_link(dir.join("driver"))
            .ok()
            .and_then(|target| {
                target
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
            })
            .unwrap_or_default();
        if is_virtio && !driver.is_empty() {
            bound += 1;
        }
        described.push(match driver.is_empty() {
            true => format!("{vendor}:{device}/{class}"),
            false => format!("{vendor}:{device}/{class}:{driver}"),
        });
    }

    if functions == 0 {
        println!("VMHOST_TEST_FAIL pciscan bus-present-but-empty");
        return;
    }
    println!(
        "VMHOST_TEST_OK pciscan functions={functions} virtio={virtio} bound={bound} \
         msix={msix} devices={}",
        described.join(",")
    );
}

/// What `/proc/interrupts` says about the virtio devices' interrupt lines.
struct VirtioIrqs {
    /// Interrupts delivered across every virtio line.
    total: u64,
    /// One entry per virtio line: `(name, controller)`, e.g.
    /// `("virtio0-req.0", "PCI-MSIX-0000:00:01.0")` or `("virtio0", "IO-APIC")`.
    lines: Vec<(String, String)>,
}

impl VirtioIrqs {
    /// How the interrupts are being delivered, from the controller column — the
    /// one place the guest states it outright.
    fn mode(&self) -> &'static str {
        if self.lines.is_empty() {
            "none"
        } else if self.lines.iter().any(|(_, c)| c.contains("PCI-MSI")) {
            "msix"
        } else {
            "intx"
        }
    }

    fn names(&self) -> String {
        self.lines
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// Interrupts delivered to virtio devices so far, from `/proc/interrupts`.
///
/// Reported alongside the block probe because it answers a question the byte
/// count cannot: *how* the completions arrived. A device whose interrupts are
/// lost can still finish a read — the driver notices used buffers the next time
/// something else wakes it — so "the read succeeded" is not evidence that the
/// interrupt path works. A count that climbs with the request count is.
///
/// The controller column is reported too, because with MSI-X there is a second
/// question of the same kind: the read succeeding, and even a climbing count, does
/// not say *which* mechanism carried it. `PCI-MSIX-…` in that column does.
///
/// Returns `None` when `/proc/interrupts` has no virtio line at all, which is
/// itself the interesting answer: the driver never registered a handler.
fn virtio_interrupts() -> Option<VirtioIrqs> {
    let text = std::fs::read_to_string("/proc/interrupts").ok()?;
    let mut irqs = VirtioIrqs {
        total: 0,
        lines: Vec::new(),
    };
    for line in text.lines() {
        // "  5:   142    IO-APIC   5-edge   virtio0", or under MSI-X
        // " 24:   512    PCI-MSIX-0000:00:01.0   1-edge   virtio0-req.0".
        // Everything after the "n:" label is per-CPU counts, then the controller,
        // then the trigger, then the device name.
        let Some((_, rest)) = line.split_once(':') else {
            continue;
        };
        let fields: Vec<&str> = rest.split_ascii_whitespace().collect();
        let Some(name) = fields.last() else {
            continue;
        };
        if !name.starts_with("virtio") {
            continue;
        }
        let counts = fields
            .iter()
            .take_while(|f| f.parse::<u64>().is_ok())
            .filter_map(|f| f.parse::<u64>().ok());
        for count in counts {
            irqs.total = irqs.total.saturating_add(count);
        }
        let controller = fields
            .iter()
            .find(|f| f.parse::<u64>().is_err())
            .copied()
            .unwrap_or("?");
        // Echoed verbatim so the serial log is itself the evidence, next to the
        // one machine-readable line the harness parses.
        println!("entangled-irq: {}", line.trim());
        irqs.lines.push((name.to_string(), controller.to_string()));
        if irqs.lines.len() >= MAX_SCANNED_FUNCTIONS {
            break;
        }
    }
    (!irqs.lines.is_empty()).then_some(irqs)
}

/// Sequentially reads `mib` MiB from /dev/vda and reports the rate, plus how
/// many interrupts the device raised while doing it.
fn blk_bench(mib: u64) {
    let mut file = match std::fs::File::open("/dev/vda") {
        Ok(file) => file,
        Err(e) => {
            println!("VMHOST_TEST_FAIL blkbench cannot-open-/dev/vda:{e}");
            return;
        }
    };
    // `/proc` is already mounted by the caller (the command line came from it).
    let before = virtio_interrupts();
    let target = mib.saturating_mul(1 << 20);
    let mut buf = vec![0u8; CHUNK];
    let mut read_total: u64 = 0;
    let started = Instant::now();
    while read_total < target {
        match file.read(&mut buf) {
            Ok(0) => break, // end of device
            Ok(n) => read_total = read_total.saturating_add(n as u64),
            Err(e) => {
                println!("VMHOST_TEST_FAIL blkbench read-error:{e}");
                return;
            }
        }
    }
    let ms = started.elapsed().as_millis().max(1);
    let kib_per_s = read_total / 1024 * 1000 / ms as u64;
    let after = virtio_interrupts();
    // -1 distinguishes "no virtio interrupt line exists" from "zero interrupts
    // on the line that does", which are different failures.
    let irqs = match (&before, &after) {
        (Some(before), Some(after)) => after.total.saturating_sub(before.total) as i64,
        _ => -1,
    };
    let (mode, names) = match &after {
        Some(after) => (after.mode(), after.names()),
        None => ("none", String::new()),
    };
    println!(
        "VMHOST_TEST_OK blkbench bytes={read_total} ms={ms} kib_per_s={kib_per_s} \
         irqs={irqs} irqmode={mode} irqnames={names}"
    );
}
