//! User-mode networking on WHP, end to end (backlog WHP-1704, closed in
//! EPIC 17 phase 4).
//!
//! A real Linux guest with a **virtio-net device on the smoltcp user-mode NAT**
//! — no TAP, no administrator — proving guest TCP connectivity the way the
//! phase-3 unit tests prove it for a synthetic guest: bytes out of the guest,
//! through the NAT's host socket, into a listener this test runs, and the echo
//! all the way back. The guest's own `entangled.netprobe` is the in-guest half:
//! it configures `eth0` statically (the same numbers the NAT's DHCP server
//! would serve), connects to the host's address and verifies the echo — TX
//! *and* RX, because only returned bytes prove frames were queued to the guest,
//! the RX interrupt fired through the userspace IOAPIC, and the buffers
//! completed.
//!
//! The listener sits on one of the host's own routable addresses because that
//! is what the NAT can legitimately reach: destinations on the guest's segment
//! and loopback are refused by design (`virtio_net::usernet::tcp`), exactly so
//! a guest cannot reach host-local services. Self-skips without such an
//! address, without WHP, or without the artifacts, like every whp_* test.

#![cfg(windows)]

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use linux_boot::{BootConfig, GUEST_READY_MARKER};
use machine_x86::boot as x86_boot;
use machine_x86::bus::MachineBus;
use machine_x86::irqchip::UserspaceIrqChip;
use machine_x86::serial::SerialConsole;
use machine_x86::virtio::VirtioMmioBus;
use virtio_core::VirtioDevice;
use virtio_net::{MacAddr, NetBackend, NetDevice, UserNetBackend};
use vmm_core::whp::{WhpHypervisor, WhpOptions, WhpPartition, WHP_ENABLE_HINT};

mod whp_common;
use whp_common::{
    artifact, complete_line_with, dump_log, kernel, tail, whp_guard, Capture, BOOT_DEADLINE,
    MACHINE,
};

/// An address of this host that a NAT'ed guest may legitimately reach: the one
/// the default route would use. The UDP "connect" sends nothing — it only asks
/// the stack which local address it would pick.
fn routable_host_address() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect("8.8.8.8:53").ok()?;
    match socket.local_addr().ok()? {
        SocketAddr::V4(addr) if !addr.ip().is_loopback() && !addr.ip().is_unspecified() => {
            Some(*addr.ip())
        }
        _ => None,
    }
}

/// Phase 4 acceptance: a WHP guest gets TCP connectivity from the user-mode
/// NAT — greeting out, echo back, and the host-side flow counters agree.
#[test]
fn a_whp_guest_reaches_a_host_listener_through_the_usernet_nat() {
    let _guard = whp_guard();
    let hv = match WhpHypervisor::open() {
        Ok(hv) => hv,
        Err(e) => {
            eprintln!("skipping: {e}");
            eprintln!("(to run this test: {WHP_ENABLE_HINT})");
            return;
        }
    };
    let (Some((kernel, which)), Some(initramfs)) =
        (kernel(), artifact("tests/test-initramfs.cpio.gz"))
    else {
        eprintln!("skipping: test artifacts missing");
        return;
    };
    let Some(host_ip) = routable_host_address() else {
        eprintln!("skipping: this host has no routable IPv4 address for the NAT to connect to");
        return;
    };

    // ---- the host side the guest will talk to: a one-connection echo ----
    let listener = TcpListener::bind((host_ip, 0)).expect("bind the echo listener");
    let port = listener.local_addr().expect("listener address").port();
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let stop = Arc::new(AtomicBool::new(false));
    let echo_stop = Arc::clone(&stop);
    let echo = std::thread::spawn(move || -> Option<usize> {
        // Poll-accept so the thread can be stopped even if no SYN ever arrives.
        let stream = loop {
            if echo_stop.load(Ordering::Acquire) {
                return None;
            }
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(e) => {
                    eprintln!("echo listener accept failed: {e}");
                    return None;
                }
            }
        };
        let mut stream = stream;
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(1)));
        let mut buf = [0u8; 256];
        let mut echoed = 0usize;
        let mut done = false;
        // Echo until the newline that ends the guest's greeting — and then keep
        // our half open until the *guest* closes. Closing right after the last
        // write is the racy-peer shape: the FIN chases the echoed bytes through
        // the NAT's teardown and can beat them to the guest (measured: ~1 in 4
        // runs read EOF before data). The guest verifies, closes first, and our
        // read then sees the EOF.
        loop {
            if echo_stop.load(Ordering::Acquire) {
                return Some(echoed);
            }
            match stream.read(&mut buf) {
                Ok(0) => return Some(echoed),
                Ok(n) if !done => {
                    if stream.write_all(&buf[..n]).is_err() {
                        return Some(echoed);
                    }
                    echoed += n;
                    if buf[..n].contains(&b'\n') {
                        let _ = stream.flush();
                        done = true;
                    }
                }
                Ok(_) => {}
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(_) => return Some(echoed),
            }
        }
    });

    // ---- the machine: one virtio-net device on the user-mode NAT ----
    let backend = Arc::new(UserNetBackend::with_defaults().expect("user-mode NAT backend"));
    let config = *backend.config();
    eprintln!(
        "booting the {which} kernel; NAT segment {}, echo listener {host_ip}:{port}",
        backend.name()
    );

    let mut partition = WhpPartition::with_options(&hv, &MACHINE, WhpOptions::for_guest())
        .expect("WHP partition with a local APIC");
    machine_x86::mptable::write(partition.memory(), MACHINE.vcpu_count).unwrap();
    machine_x86::acpi::write(partition.memory(), MACHINE.vcpu_count).unwrap();
    let irqchip = UserspaceIrqChip::new(partition.interrupt_delivery(), MACHINE.vcpu_count)
        .expect("userspace irqchip");

    let device = NetDevice::with_backend(
        Arc::clone(&backend) as Arc<dyn NetBackend>,
        MacAddr::derive("whp-usernet-acceptance"),
    );
    let devices: Vec<Box<dyn VirtioDevice>> = vec![Box::new(device)];
    let mem = Arc::new(partition.memory().clone());
    let virtio = VirtioMmioBus::attach_userspace(mem, devices, &irqchip)
        .expect("attach virtio-mmio on the userspace irqchip");
    let clauses = virtio.cmdline_clauses();

    let capture = Capture::default();
    let serial = SerialConsole::with_trigger(irqchip.serial_line(), Box::new(capture.clone()));
    let bus = MachineBus::with_virtio(serial, virtio).with_irqchip(Arc::clone(&irqchip));

    let boot = BootConfig {
        kernel,
        initramfs: Some(initramfs),
        cmdline: format!(
            "console=ttyS0 earlyprintk=serial panic=1 reboot=k \
             entangled.netprobe={}/{},{},{host_ip}:{port} {clauses}",
            config.guest,
            config.prefix_len(),
            config.gateway,
        ),
    };
    let loaded = linux_boot::load(partition.memory(), &boot, MACHINE.memory_mib << 20)
        .expect("bzImage + initramfs load");

    let mut vcpus = partition.take_vcpus();
    {
        let vcpu = &mut vcpus[0];
        x86_boot::setup_long_mode_sregs(partition.memory(), vcpu).unwrap();
        x86_boot::setup_boot_regs(vcpu, loaded.entry, loaded.boot_params_addr).unwrap();
    }
    let threads = vmm_core::whp::spawn_vcpus(vcpus, |_| Box::new(bus.clone())).unwrap();

    let start = Instant::now();
    let mut ready = false;
    let mut done = false;
    let mut panicked = false;
    while start.elapsed() < BOOT_DEADLINE {
        let text = capture.text();
        ready |= text.contains(GUEST_READY_MARKER);
        if complete_line_with(&text, "VMHOST_TEST_OK netprobe ")
            || complete_line_with(&text, "VMHOST_TEST_FAIL netprobe ")
        {
            done = true;
            break;
        }
        if text.contains("Kernel panic - not syncing") {
            panicked = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let outcomes = threads.stop();
    stop.store(true, Ordering::Release);
    let text = capture.text();
    dump_log(&text);

    let stats = backend.stats();
    eprintln!(
        "usernet stats: from_guest={} to_guest={} tcp_flows={} dropped_malformed={} \
         dropped_unsupported={} dropped_full={}",
        stats.frames_from_guest.load(Ordering::Relaxed),
        stats.frames_to_guest.load(Ordering::Relaxed),
        stats.tcp_flows.load(Ordering::Relaxed),
        stats.dropped_malformed.load(Ordering::Relaxed),
        stats.dropped_unsupported.load(Ordering::Relaxed),
        stats.dropped_full.load(Ordering::Relaxed),
    );

    assert!(
        !panicked,
        "the guest panicked; serial tail:\n{}",
        tail(&text, 40)
    );
    if !done && ready && !text.contains("netprobe") {
        // The guest booted and rebooted without ever mentioning the probe: this
        // initramfs predates it. A skip with a hint beats a misleading failure.
        eprintln!(
            "skipping: the test initramfs has no netprobe — rebuild it with \
             scripts/build-test-initramfs.sh"
        );
        let _ = echo.join();
        return;
    }
    assert!(
        done,
        "no netprobe verdict within {BOOT_DEADLINE:?}; serial tail:\n{}",
        tail(&text, 60)
    );
    assert!(
        !text.contains("VMHOST_TEST_FAIL netprobe"),
        "the guest's network probe failed; serial tail:\n{}",
        tail(&text, 40)
    );

    let echoed = capture
        .probe_value("netprobe", "echoed")
        .expect("the netprobe line carries the echoed byte count");
    assert!(echoed > 0, "the echo carried no bytes");
    let served = echo.join().expect("echo thread");
    assert_eq!(
        served,
        Some(echoed as usize),
        "host listener and guest disagree about the echo"
    );

    // The host-side counters tell the same story: guest frames arrived, a TCP
    // flow was opened, and frames went back.
    assert!(stats.frames_from_guest.load(Ordering::Relaxed) > 0);
    assert!(stats.frames_to_guest.load(Ordering::Relaxed) > 0);
    assert!(stats.tcp_flows.load(Ordering::Relaxed) >= 1);

    for outcome in outcomes {
        outcome.expect("vCPU outcome");
    }
    drop(partition);
    drop(irqchip);
}
