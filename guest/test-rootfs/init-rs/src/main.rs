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
//!   `entangled.poweroff=1`      power the machine off through ACPI instead of
//!                               rebooting: proves the FADT, the DSDT's `\_S5`
//!                               and the host's ACPI PM block agree. Opt-in,
//!                               because every other test wants the reboot path.
//!   `entangled.netprobe=<ip>/<prefix>,<gateway>,<host>:<port>`
//!                               configures eth0 statically (this initramfs has
//!                               no DHCP client), opens a TCP connection to
//!                               `<host>:<port>` and expects its greeting echoed
//!                               back — the guest-side evidence for a virtio-net
//!                               backend (WHP-1704: the user-mode NAT).

use std::ffi::CString;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
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
    if let Some(spec) = param(&cmdline, "entangled.netprobe=") {
        net_probe(&spec);
    }

    if param(&cmdline, "entangled.poweroff=").as_deref() == Some("1") {
        acpi_power_off();
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
