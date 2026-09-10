//! Vulkan **inside the guest**, on Venus (VEN-2006, ADR-0004): the same
//! Ubuntu Desktop live boot as `virgl_gnome.rs`, but the host renderer is a
//! Venus-capable virglrenderer and the device carries a *host-visible window*
//! backed by real host memory.
//!
//! The bar this project has held for every 3D claim is a real guest kernel,
//! quoted verbatim. For phase 1 that was `GLPROBE_RENDERER=virgl`; here it is
//! mesa's own venus driver enumerating a Vulkan device over virtio-gpu, which
//! it names `Virtio-GPU Venus (...)` — a string no other ICD produces.
//!
//! Evidence collected over the serial console:
//!
//! * `[drm] features: +virgl` and the kernel claiming BAR 2 — the guest sees
//!   the shared-memory region and the blob feature;
//! * systemd reaching the graphical target, so the GL half still works while
//!   Venus exists beside it;
//! * `VKPROBE_DEVICE=` typed into a `systemd.debug_shell` root shell: a
//!   python-ctypes `vkCreateInstance` + `vkEnumeratePhysicalDevices` +
//!   `vkGetPhysicalDeviceProperties`, needing nothing on the ISO but
//!   `libvulkan.so.1`.
//!
//! **What this cannot show.** The only Vulkan ICD on this project's dev host
//! is lavapipe — Vulkan on the CPU. A guest device enumerated here therefore
//! proves the *protocol path* is correct end to end; it says nothing at all
//! about speed, and no frame number should be taken from it (ADR-0004,
//! VEN-2003's amendment).
//!
//! `#[ignore]`d: needs `/dev/kvm`, the firmware, the ~6 GiB Desktop ISO, and a
//! host virglrenderer with Venus in it — build one with
//! `guest/virglrenderer/build-virglrenderer.sh` and point
//! `ENTANGLED_VIRGL_LIB` at the result. Self-skips without any of them.
//!
//! ```bash
//! ENTANGLED_VIRGL_LIB=$HOME/.cache/entangled-virglrenderer/virglrenderer-1.1.0/lib/x86_64-linux-gnu/libvirglrenderer.so.1 \
//!   cargo test -p boot-tests --test venus_vulkan -- --ignored --nocapture
//! ```

#![cfg(target_os = "linux")]

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use boot_tests::{artifact, kvm_available};
use machine_x86::boot as x86_boot;
use machine_x86::bus::MachineBus;
use machine_x86::serial::SerialConsole;
use machine_x86::virtio_pci::VirtioPciBus;
use virtio_core::VirtioDevice;
use vmm_core::{spawn_vcpus, Hypervisor, MachineConfig, Vm};

const DEADLINE: Duration = Duration::from_secs(12 * 60);
const PROBE_DEADLINE: Duration = Duration::from_secs(5 * 60);

const MENU_MARKER: &str = "Try or Install Ubuntu";
const PROMPT: &str = "grub>";
const GRAPHICAL_MARKER: &str = "Graphical Interface";
const VIRGL_MARKER: &str = "+virgl";
/// Printed by the in-guest probe; assembled there by concatenation so the
/// *typed command's echo* can never match it.
const DEVICE_MARKER: &str = "VKPROBE_DEVICE=";
const COUNT_MARKER: &str = "VKPROBE_COUNT=";
/// What mesa's venus driver calls a device it reaches over virtio-gpu. Any
/// other ICD in the guest (there is none on the ISO, but a future one could
/// be) names itself something else, so this is the discriminating string.
const VENUS_NAME: &str = "Virtio-GPU Venus";

const VCPUS: u32 = 4;
const MEMORY_MIB: u64 = 4096;
const SHOT_ENV: &str = "ENTANGLED_VENUS_SHOT";

/// The Vulkan probe, as one shell line.
///
/// `VkInstanceCreateInfo` is built as a 64-byte zeroed buffer with `sType = 1`
/// rather than as a ctypes `Structure`, because the whole thing has to survive
/// being typed into a serial console: every field but `sType` is zero, and the
/// layout is fixed by the ABI. `VkPhysicalDeviceProperties::deviceName` sits at
/// offset 20 of a struct we never have to describe.
///
/// It prints the ICD list first, so a run on a guest whose mesa has no venus
/// driver says *that* instead of just failing.
const PROBE_COMMAND: &str = concat!(
    "ls /usr/share/vulkan/icd.d/ | tr '\\n' ' ' | sed 's/^/VKPROBE_ICDS=/'; ",
    "python3 -c 'import ctypes; ",
    "v=ctypes.CDLL(\"libvulkan.so.1\"); ",
    "b=bytearray(64); b[0]=1; c=(ctypes.c_char*64).from_buffer(b); ",
    "i=ctypes.c_void_p(); ",
    "r=v.vkCreateInstance(ctypes.byref(c),None,ctypes.byref(i)); ",
    "v.vkEnumeratePhysicalDevices.argtypes=[ctypes.c_void_p,ctypes.c_void_p,ctypes.c_void_p]; ",
    "v.vkGetPhysicalDeviceProperties.argtypes=[ctypes.c_void_p,ctypes.c_void_p]; ",
    "n=ctypes.c_uint32(0); ",
    "v.vkEnumeratePhysicalDevices(i,ctypes.byref(n),None); ",
    "d=(ctypes.c_void_p*max(n.value,1))(); ",
    "v.vkEnumeratePhysicalDevices(i,ctypes.byref(n),d); ",
    "p=(ctypes.c_char*2048)(); ",
    "print(\"VKPROBE_\"+\"COUNT=\"+str(n.value)+\" rc=\"+str(r)); ",
    "[ (v.vkGetPhysicalDeviceProperties(d[k],p), ",
    "print(\"VKPROBE_\"+\"DEVICE=\"+ctypes.string_at(ctypes.addressof(p)+20).decode())) ",
    "for k in range(n.value) ]'",
);

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Ok(mut inner) = self.0.lock() {
            inner.extend_from_slice(buf);
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Capture {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().map(|v| v.clone()).unwrap_or_default()).into_owned()
    }
}

fn desktop_iso() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("ENTANGLED_UBUNTU_DESKTOP_ISO").map(PathBuf::from) {
        return path.is_file().then_some(path);
    }
    let cache = match std::env::var_os("ENTANGLED_CACHE") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".cache/entangled"),
    };
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(cache.join("ubuntu"))
        .ok()?
        .filter_map(Result::ok)
        .flat_map(|release| {
            std::fs::read_dir(release.path())
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .map(|f| f.path())
                .filter(|p| {
                    p.file_name()
                        .is_some_and(|n| n.to_string_lossy().contains("desktop"))
                        && p.extension().is_some_and(|e| e == "iso")
                })
                .collect::<Vec<_>>()
        })
        .collect();
    candidates.sort();
    candidates.pop()
}

struct Typing {
    grub_commands: Vec<String>,
    entered_grub: bool,
    grub_sent: usize,
    probe_sent: bool,
}

impl Typing {
    fn new() -> Self {
        Self {
            grub_commands: vec![
                "linux /casper/vmlinuz console=ttyS0,115200n8 systemd.debug_shell=ttyS0 \
                 systemd.mask=serial-getty@ttyS0.service ---"
                    .into(),
                "initrd /casper/initrd".into(),
                "boot".into(),
            ],
            entered_grub: false,
            grub_sent: 0,
            probe_sent: false,
        }
    }

    fn step(&mut self, log: &str, graphical_at: Option<Instant>) -> Option<Vec<u8>> {
        if !self.entered_grub {
            if !log.contains(MENU_MARKER) {
                return None;
            }
            self.entered_grub = true;
            return Some(b"c".to_vec());
        }
        if self.grub_sent < self.grub_commands.len() {
            if log.matches(PROMPT).count() <= self.grub_sent {
                return None;
            }
            let line = self.grub_commands[self.grub_sent].clone();
            self.grub_sent += 1;
            println!("typing into GRUB: {line}");
            return Some(format!("{line}\r").into_bytes());
        }
        if !self.probe_sent {
            let ready = graphical_at.is_some_and(|at| at.elapsed() >= Duration::from_secs(45));
            if ready {
                self.probe_sent = true;
                println!("typing the Vulkan probe into the debug shell");
                return Some(format!("\r{PROBE_COMMAND}\r").into_bytes());
            }
        }
        None
    }
}

#[test]
#[ignore = "boots the GNOME live desktop on Venus: needs KVM, the firmware, the Desktop ISO and a Venus-capable virglrenderer"]
fn a_guest_enumerates_a_vulkan_device_over_venus() {
    if !kvm_available() {
        return;
    }
    let Some(firmware) = artifact("firmware/CLOUDHV.fd") else {
        eprintln!("skipping: no artifacts/firmware/CLOUDHV.fd");
        return;
    };
    let Some(iso) = desktop_iso() else {
        eprintln!("skipping: no Desktop ISO — run scripts/fetch-ubuntu-iso.sh desktop");
        return;
    };
    let renderer = match virtio_gpu::virgl::VirglRenderer::load() {
        Ok(renderer) => renderer,
        Err(e) => {
            eprintln!("skipping: {e}");
            return;
        }
    };
    let support = virtio_gpu::Renderer3d::blob_support(&renderer);
    if support.host_visible_bytes.is_none() {
        eprintln!(
            "skipping: this host's virglrenderer has no Venus \
             ({support:?}); build one with guest/virglrenderer/build-virglrenderer.sh \
             and set ENTANGLED_VIRGL_LIB"
        );
        return;
    }
    eprintln!(
        "booting {} with {} and a {} MiB host-visible window",
        firmware.display(),
        iso.display(),
        support.host_visible_bytes.unwrap_or(0) >> 20
    );

    let machine = MachineConfig {
        memory_mib: MEMORY_MIB,
        vcpu_count: VCPUS,
    };
    let hv = Hypervisor::open().expect("kvm");
    let mut vm = Vm::new(&hv, &machine).expect("vm");
    let mem_size = machine.memory_mib << 20;

    machine_x86::mptable::write(vm.memory(), machine.vcpu_count).expect("mp table");
    machine_x86::acpi::write(vm.memory(), machine.vcpu_count).expect("acpi tables");

    let capture = Capture::default();
    let serial = SerialConsole::new(vm.fd(), Box::new(capture.clone())).expect("serial");

    let mut devices: Vec<Box<dyn VirtioDevice>> = Vec::new();
    let cdrom = virtio_block::BlockDevice::open(&iso, false).expect("cdrom");
    devices.push(Box::new(cdrom));
    let display = display::DisplayHandle::detached(1920, 1080).expect("detached scanout");
    devices.push(Box::new(virtio_gpu::GpuDevice::with_renderer(
        display.clone(),
        Box::new(renderer),
    )));
    devices.push(Box::new(virtio_input::InputDevice::keyboard()));
    devices.push(Box::new(virtio_input::InputDevice::absolute_pointer()));

    let mem = Arc::new(vm.memory().clone());
    // Scoped so the window allocator's borrow of `vm` ends before
    // `take_vcpus` needs it mutably.
    let bus = {
        let allocate = |len: u64, host_mapped: bool| vm.create_shm_window(len, host_mapped);
        let pci = VirtioPciBus::attach_with_shm(
            vm.fd_shared(),
            mem,
            devices,
            Default::default(),
            Default::default(),
            Some(machine_x86::shm::ShmSupport {
                mem_bytes: mem_size,
                allocate: &allocate,
            }),
        )
        .expect("virtio-pci bus with a host-visible window");
        MachineBus::with_virtio_pci(serial, pci).with_firmware_platform()
    };

    let image = uefi_boot::FirmwareConfig {
        firmware: firmware.clone(),
    }
    .open()
    .expect("firmware image");
    let boot = uefi_boot::load_pvh(vm.memory(), &image, mem_size).expect("load firmware");

    let vcpus = vm.take_vcpus();
    for vcpu in &vcpus {
        x86_boot::setup_pvh_sregs(vm.memory(), vcpu).expect("pvh sregs");
        if vcpu.index == 0 {
            x86_boot::setup_pvh_regs(vcpu, boot.entry, boot.start_info_addr).expect("pvh regs");
        }
    }

    let started = Instant::now();
    let threads = spawn_vcpus(vcpus, |_| Box::new(bus.clone())).expect("vcpus");
    let typing = Mutex::new(Typing::new());
    let graphical_at = std::cell::Cell::new(None::<Instant>);
    let probed_at = std::cell::Cell::new(None::<Instant>);
    let outcomes = threads.join_or_stop(
        || {
            let text = capture.text();
            if graphical_at.get().is_none() && text.contains(GRAPHICAL_MARKER) {
                graphical_at.set(Some(Instant::now()));
                println!("graphical target reached after {:?}", started.elapsed());
            }
            if let Ok(mut typing) = typing.lock() {
                if let Some(keys) = typing.step(&text, graphical_at.get()) {
                    bus.push_serial_input(&keys);
                }
            }
            if probed_at.get().is_none() && text.contains(COUNT_MARKER) {
                probed_at.set(Some(Instant::now()));
                println!("probe answered after {:?}", started.elapsed());
            }
            if let Some(at) = probed_at.get() {
                // Give the device-name lines a moment to arrive after the
                // count line that ends the probe's first print.
                return at.elapsed() >= Duration::from_secs(5);
            }
            match graphical_at.get() {
                Some(at) => at.elapsed() >= PROBE_DEADLINE,
                None => started.elapsed() >= DEADLINE,
            }
        },
        Duration::from_millis(50),
    );
    let log = capture.text();

    println!("--- guest evidence ({:?}) ---", started.elapsed());
    for line in log.lines().filter(|line| {
        [
            "virtio_gpu",
            "[drm]",
            "Command line:",
            "resource2",
            "BAR 2",
            GRAPHICAL_MARKER,
            "VKPROBE_",
        ]
        .iter()
        .any(|needle| line.contains(needle))
    }) {
        println!("{}", line.trim());
    }
    println!("--- vCPU outcomes: {outcomes:?} ---");

    if let Some(path) = std::env::var_os(SHOT_ENV) {
        match display.screenshot(&path) {
            Ok(()) => println!("scanout written to {}", PathBuf::from(&path).display()),
            Err(e) => println!("could not write the scanout: {e}"),
        }
    }

    assert!(
        log.contains("console=ttyS0"),
        "the typed GRUB command line never reached the kernel; full log:\n{log}"
    );
    assert!(
        log.contains(VIRGL_MARKER),
        "the guest kernel did not negotiate VIRGL; its drm lines:\n{}",
        log.lines()
            .filter(|l| l.contains("drm") || l.contains("virtio_gpu"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(
        log.contains(GRAPHICAL_MARKER),
        "systemd never reached the graphical target within {DEADLINE:?}"
    );
    // The typed command contains `VKPROBE_"+"DEVICE=`, never the joined
    // string, so an echo cannot satisfy this.
    let devices: Vec<&str> = log
        .lines()
        .filter(|l| l.contains(DEVICE_MARKER) && !l.contains("VKPROBE_\"+\""))
        .map(str::trim)
        .collect();
    assert!(
        devices.iter().any(|line| line.contains(VENUS_NAME)),
        "no in-guest Vulkan device came from Venus (saw: {devices:?});\n\
         probe-adjacent log:\n{}",
        log.lines()
            .filter(|l| l.contains("VKPROBE_"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    for line in devices {
        println!("guest Vulkan device: {line}");
    }
}
