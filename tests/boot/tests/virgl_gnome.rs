//! Ubuntu Desktop live session with **3D acceleration** (ADR-0004, the
//! GPU-011 acceptance run): the same boot as `desktop_gnome.rs`, but the
//! virtio-gpu device carries the real virglrenderer — so the guest's mesa
//! stack must come up on **virgl instead of llvmpipe**.
//!
//! Evidence collected over the serial console:
//!
//! * `[drm] features: +virgl` — the guest kernel negotiated the 3D feature
//!   (a 2D device logs `-virgl`);
//! * systemd reaching the graphical target — GNOME starts against the 3D
//!   device;
//! * **no** `virtio_gpu_dequeue_ctrl_func` error responses for
//!   `CTX_ATTACH/DETACH_RESOURCE` — the kernel attaches its 2D-created console
//!   framebuffer to a 3D context, and a device that refuses that logs a DRM
//!   error pair on every virgl boot (GPU-006);
//! * the renderer string, read from *inside the guest*: the kernel command
//!   line gets `systemd.debug_shell=ttyS0`, and once the desktop is up the
//!   test types a python-ctypes EGL probe into that root shell. The reply
//!   line `GLPROBE_RENDERER=virgl` is mesa in the guest naming its GL
//!   renderer — the exact thing `glxinfo -B` would print, without needing
//!   mesa-utils on the ISO.
//!
//! `ENTANGLED_VIRGL_SHOT=<path>` additionally writes the scanout as PNG.
//!
//! `#[ignore]`d: needs `/dev/kvm`, the firmware, the ~6 GiB Desktop ISO and a
//! host where virglrenderer initializes (the test self-skips without one).
//!
//! ```bash
//! cargo test -p boot-tests --test virgl_gnome -- --ignored --nocapture
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

/// Firmware + GRUB, the squashfs, then GNOME — on virgl this should be well
/// under the llvmpipe run's minutes, but the deadline stays generous.
const DEADLINE: Duration = Duration::from_secs(12 * 60);
/// Extra time after the graphical target for the debug shell + probe.
const PROBE_DEADLINE: Duration = Duration::from_secs(4 * 60);

const MENU_MARKER: &str = "Try or Install Ubuntu";
const PROMPT: &str = "grub>";
const SMP_MARKER: &str = "smp: Brought up 1 node, 4 CPUS";
const GRAPHICAL_MARKER: &str = "Graphical Interface";
/// The guest kernel's feature line for a device offering VIRGL.
const VIRGL_MARKER: &str = "+virgl";
/// Printed by the in-guest probe; constructed there by concatenation so the
/// *typed command's echo* can never match it.
const PROBE_MARKER: &str = "GLPROBE_RENDERER=";

const VCPUS: u32 = 4;
const MEMORY_MIB: u64 = 4096;
const SHOT_ENV: &str = "ENTANGLED_VIRGL_SHOT";
/// Extra seconds to keep the session running after the probe answered — how
/// long the scanout gets to become a full desktop before the screenshot.
const LINGER_ENV: &str = "ENTANGLED_VIRGL_LINGER";

/// The EGL probe, one line of python-ctypes: surfaceless EGL display →
/// GL context → `glGetString(GL_RENDERER)`. Works in the live session with
/// nothing installed (python3, libEGL and libGL all ship in the squashfs).
const PROBE_COMMAND: &str = concat!(
    "python3 -c 'import ctypes; ",
    "e=ctypes.CDLL(\"libEGL.so.1\"); ",
    "e.eglGetPlatformDisplay.restype=ctypes.c_void_p; ",
    "e.eglGetPlatformDisplay.argtypes=[ctypes.c_uint,ctypes.c_void_p,ctypes.c_void_p]; ",
    "d=e.eglGetPlatformDisplay(0x31DD,None,None); ", // EGL_PLATFORM_SURFACELESS_MESA
    "e.eglInitialize.argtypes=[ctypes.c_void_p,ctypes.c_void_p,ctypes.c_void_p]; ",
    "e.eglInitialize(d,None,None); ",
    "e.eglBindAPI(0x30A2); ", // EGL_OPENGL_API
    "a=(ctypes.c_int*3)(0x3033,1,0x3038); ", // SURFACE_TYPE=PBUFFER, NONE
    "c=ctypes.c_void_p(); n=ctypes.c_int(); ",
    "e.eglChooseConfig.argtypes=[ctypes.c_void_p,ctypes.c_void_p,ctypes.c_void_p,ctypes.c_int,ctypes.c_void_p]; ",
    "e.eglChooseConfig(d,a,ctypes.byref(c),1,ctypes.byref(n)); ",
    "e.eglCreateContext.restype=ctypes.c_void_p; ",
    "e.eglCreateContext.argtypes=[ctypes.c_void_p,ctypes.c_void_p,ctypes.c_void_p,ctypes.c_void_p]; ",
    "x=e.eglCreateContext(d,c,None,None); ",
    "e.eglMakeCurrent.argtypes=[ctypes.c_void_p,ctypes.c_void_p,ctypes.c_void_p,ctypes.c_void_p]; ",
    "e.eglMakeCurrent(d,None,None,x); ",
    "g=ctypes.CDLL(\"libGL.so.1\"); g.glGetString.restype=ctypes.c_char_p; ",
    "print(\"GLPROBE_\"+\"RENDERER=\"+(g.glGetString(0x1F01) or b\"none\").decode())'",
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

/// The typing plan: GRUB first (with the virgl-run's extra cmdline), then —
/// once the desktop is up — the EGL probe into the debug shell.
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
                // `systemd.debug_shell=ttyS0` gives a root shell on the
                // serial line *next to* the console, without touching the
                // graphical session — the probe types into it later. The
                // getty systemd would also spawn there (because of
                // `console=ttyS0`) is masked: two readers on one tty steal
                // bytes from each other and the typed probe arrives mangled.
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
        // The probe: once the graphical target is reached, give the session a
        // moment (mutter creates its GL context in those seconds — which is
        // itself part of the evidence, in the host device logs), then ask.
        if !self.probe_sent {
            let ready = graphical_at.is_some_and(|at| at.elapsed() >= Duration::from_secs(45));
            if ready {
                self.probe_sent = true;
                println!("typing the EGL probe into the debug shell");
                return Some(format!("\r{PROBE_COMMAND}\r").into_bytes());
            }
        }
        None
    }
}

#[test]
#[ignore = "boots the GNOME live desktop on virgl: needs KVM, the firmware, the Desktop ISO and a virglrenderer host"]
fn the_desktop_session_runs_on_virgl_not_llvmpipe() {
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
    eprintln!("booting {} with {}", firmware.display(), iso.display());

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
    let pci = VirtioPciBus::attach(vm.fd_shared(), mem, devices).expect("virtio-pci bus");
    let bus = MachineBus::with_virtio_pci(serial, pci).with_firmware_platform();

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

    let linger = std::env::var(LINGER_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or_default();

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
            if probed_at.get().is_none() && text.contains(PROBE_MARKER) {
                probed_at.set(Some(Instant::now()));
                println!("probe answered after {:?}", started.elapsed());
            }
            if let Some(at) = probed_at.get() {
                // All the evidence is in; the linger is for the screenshot.
                return at.elapsed() >= linger;
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
            "smp:",
            "virtio_gpu",
            "[drm]",
            "Command line:",
            GRAPHICAL_MARKER,
            PROBE_MARKER,
            "GNOME",
            "gdm",
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
    let _ = SMP_MARKER; // covered by desktop_gnome.rs; not this test's subject
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
    // GPU-006: the kernel attaches its own 2D-created console framebuffer to
    // the DRM client's 3D context and detaches it again while fb0 is set up. A
    // device that refuses those (ERR_INVALID_RESOURCE_ID, because the id lives
    // in the 2D table and not in the renderer) makes the guest driver log this
    // error pair on every single virgl boot.
    let attach_errors: Vec<&str> = log
        .lines()
        .filter(|line| {
            line.contains("*ERROR* response")
                && (line.contains("(command 0x202)") || line.contains("(command 0x203)"))
        })
        .map(str::trim)
        .collect();
    assert!(
        attach_errors.is_empty(),
        "the guest logged CTX_ATTACH/DETACH_RESOURCE failures:\n{}",
        attach_errors.join("\n")
    );
    let renderer_line = log
        .lines()
        .find(|l| l.contains(PROBE_MARKER) && !l.contains("GLPROBE_\"+\"")) // skip the typed echo
        .unwrap_or("");
    assert!(
        renderer_line.to_lowercase().contains("virgl"),
        "the in-guest EGL probe did not report virgl (got: {renderer_line:?});\n\
         probe-adjacent log:\n{}",
        log.lines().rev().take(30).collect::<Vec<_>>().join("\n")
    );
    println!("guest GL renderer: {renderer_line}");
}
