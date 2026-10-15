//! Actual Linux boot acceptance on Apple Silicon. Ubuntu kernel plus a tiny
//! static initramfs must print the marker and power off through PSCI.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    use machine_arm64::{
        boot, decode_mmio, device_tree, Pl011, TreeConfig, GIC_DISTRIBUTOR, GIC_REDISTRIBUTOR,
        RAM_BASE, UART_BASE,
    };
    use std::io::Write;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use vm_memory::{Bytes, GuestAddress};
    use vmm_core::hv::arm64::{Exit, Register, SystemRegister};
    use vmm_core::hvf::{HvfGic, HvfOptions, HvfVm};

    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let mut data = self.0.lock().unwrap_or_else(|e| e.into_inner());
            let room = (2usize << 20).saturating_sub(data.len());
            data.extend_from_slice(&bytes[..bytes.len().min(room)]);
            std::io::stdout().write_all(bytes)?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            std::io::stdout().flush()
        }
    }
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if !(2..=3).contains(&args.len()) {
        return Err("usage: hvf_linux KERNEL INITRAMFS [UBUNTU_RAW_DISK]".into());
    }
    let writable = std::env::var_os("ENTANGLED_HVF_WRITABLE").is_some();
    let init_command = std::env::var("ENTANGLED_HVF_INIT_COMMAND").ok();
    let gpu_enabled = std::env::var_os("ENTANGLED_HVF_GPU").is_some();
    let ram_bytes = 512 << 20;
    let mem = vmm_core::GuestMem::from_ranges(&[(GuestAddress(RAM_BASE), ram_bytes as usize)])?;
    let kernel = boot::read_image(std::path::Path::new(&args[0]))?;
    let entry = boot::load_kernel(&mem, &kernel)?;
    let initrd = boot::write_initrd(
        &mem,
        &boot::read_image(std::path::Path::new(&args[1]))?,
        ram_bytes,
    )?;
    let vm = HvfVm::with_options(
        mem,
        HvfOptions {
            small_page_mappings: gpu_enabled
                || std::env::var_os("ENTANGLED_HVF_SMALL_PAGES").is_some(),
        },
    )?;
    let gic = HvfGic::new(&vm, GIC_DISTRIBUTOR, GIC_REDISTRIBUTOR)?;
    struct GicLine {
        gic: HvfGic,
        irq: u32,
        pulse: Mutex<()>,
    }
    impl virtio_core::IrqLine for GicLine {
        fn trigger(&self) -> Result<(), virtio_core::InterruptError> {
            let _guard = self.pulse.lock().unwrap_or_else(|e| e.into_inner());
            self.gic
                .set_spi(self.irq, true)
                .and_then(|()| self.gic.set_spi(self.irq, false))
                .map_err(|e| virtio_core::InterruptError::Signal(e.to_string()))
        }
    }
    let mut devices = Vec::new();
    let has_disk = args.len() == 3;
    if let Some(path) = args.get(2) {
        let net = virtio_net::NetDevice::new(
            virtio_net::UserNetBackend::new(virtio_net::UserNetConfig::for_host())?,
            virtio_net::MacAddr::derive("macos-arm64-acceptance"),
        );
        for (slot, device) in [
            Box::new(virtio_block::BlockDevice::open(
                std::path::Path::new(path),
                writable,
            )?) as Box<dyn virtio_core::VirtioDevice>,
            Box::new(net),
        ]
        .into_iter()
        .enumerate()
        {
            let line = Arc::new(GicLine {
                gic: gic.clone(),
                irq: machine_arm64::VIRTIO_IRQ + slot as u32,
                pulse: Mutex::new(()),
            });
            devices.push(machine_arm64::virtio::AttachedDevice {
                device,
                line,
                shm: None,
            });
        }
    }
    if gpu_enabled {
        use virtio_core::VirtioDevice;
        let host = virtio_gpu::host_vulkan::AshVulkan::load()?;
        for device in host.usable_devices()? {
            eprintln!("Venus host GPU: {}", device.name());
        }
        let renderer =
            virtio_gpu::VenusRenderer::new(virtio_gpu::ExecutorFactory::new(Arc::new(host)));
        let mut gpu = virtio_gpu::GpuDevice::with_renderer(
            display::DisplayHandle::detached(1024, 768)?,
            Box::new(renderer),
        );
        let region = gpu
            .shm_region()
            .ok_or("Venus did not declare shared memory")?;
        if !region.host_mapped {
            return Err("Venus window must be host mapped".into());
        }
        let window = vm.create_shm_window(region.len, true)?;
        const GPU_WINDOW: u64 = 0x1_0000_0000;
        window.place(GPU_WINDOW)?;
        gpu.set_shm_backing(
            region.id,
            Arc::new(machine_arm64::shm::WindowBacking(window)),
        );
        let slot = devices.len();
        devices.push(machine_arm64::virtio::AttachedDevice {
            device: Box::new(gpu),
            line: Arc::new(GicLine {
                gic: gic.clone(),
                irq: machine_arm64::VIRTIO_IRQ + slot as u32,
                pulse: Mutex::new(()),
            }),
            shm: Some((region.id, GPU_WINDOW)),
        });
    }
    if let Some(path) = std::env::var_os("ENTANGLED_HVF_PROBES_DIR") {
        let server = virtio_9p::Server::new(std::path::Path::new(&path), true)?;
        let slot = devices.len();
        devices.push(machine_arm64::virtio::AttachedDevice {
            device: Box::new(virtio_9p::NinePDevice::new("probes", server)?),
            line: Arc::new(GicLine {
                gic: gic.clone(),
                irq: machine_arm64::VIRTIO_IRQ + slot as u32,
                pulse: Mutex::new(()),
            }),
            shm: None,
        });
    }
    let devices = machine_arm64::virtio::MmioBus::new(Arc::new(vm.memory().clone()), devices)?;
    let mut cmdline = if has_disk {
        String::from("console=ttyAMA0 earlycon=pl011,0x9000000 loglevel=7 panic=-1 root=/dev/vda ro rootwait net.ifnames=0 init=/bin/sh -- -c \"cat /etc/os-release; echo VMHOST_UBUNTU_ROOT_READY; modprobe virtio_net; ip link set eth0 up; ip addr add 192.168.74.15/24 dev eth0; ip route add default via 192.168.74.1; ping -c 1 -W 3 192.168.74.1 && echo VMHOST_NET_READY; ")
    } else {
        String::from("console=ttyAMA0 earlycon=pl011,0x9000000 loglevel=7 panic=-1 rdinit=/init")
    };
    if has_disk {
        if gpu_enabled {
            cmdline.push_str(
                "modprobe virtio_gpu && test -e /dev/dri/renderD128 && echo VMHOST_GPU_READY; ",
            );
        }
        let unix_seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        // The acceptance has no RTC yet. Set real wall time before package
        // signature/date checks; the product machine still needs an RTC.
        cmdline.push_str(&format!("date -s @{unix_seconds}; "));
        if let Some(command) = &init_command {
            if command.contains('"') || command.contains('\0') {
                return Err("init command cannot contain double quotes or NUL".into());
            }
            cmdline.push_str(command);
            cmdline.push_str("; ");
        }
        cmdline.push_str("cat /proc/interrupts; sync; ");
        if writable {
            cmdline.push_str("mount -o remount,ro /; ");
        }
        cmdline.push_str("/sbin/poweroff -f\"");
    }
    eprintln!("GIC: {:?}; kernel entry={entry:#x}", gic.layout());
    let tree = device_tree(&TreeConfig {
        memory_bytes: ram_bytes,
        cpus: 1,
        gic: gic.layout(),
        initrd: Some(initrd),
        virtio_slots: devices.len() as u32,
        cmdline: &cmdline,
    })?;
    let dtb = RAM_BASE + ram_bytes - boot::DTB_RESERVE;
    vm.memory().write_slice(&tree, GuestAddress(dtb))?;
    let mut cpu = vm.create_vcpu()?;
    cpu.allow_guest_debug_registers()?;
    cpu.set_system_register(SystemRegister::Mpidr, 1 << 31)?;
    cpu.set_system_register(SystemRegister::Sctlr, 0x30d00800)?;
    cpu.set_register(Register::Pc, entry)?;
    cpu.set_register(Register::Pstate, 0x3c5)?;
    cpu.set_register(Register::X(0), dtb)?;
    for index in 1..=3 {
        cpu.set_register(Register::X(index), 0)?;
    }
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut uart = Pl011::new(Box::new(Capture(log.clone())));
    let cancel = cpu.canceller();
    let finished = Arc::new(AtomicBool::new(false));
    let stopped = finished.clone();
    let timeout: u64 = std::env::var("ENTANGLED_HVF_TIMEOUT_SECS")
        .ok()
        .map(|s| s.parse())
        .transpose()?
        .unwrap_or(30);
    if !(1..=900).contains(&timeout) {
        return Err("timeout must be 1..900 seconds".into());
    }
    let watchdog = std::thread::spawn(move || {
        for _ in 0..timeout * 10 {
            if stopped.load(Ordering::Acquire) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = cancel.cancel();
    });
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        loop {
            match cpu.run()? {
                Exit::Exception {
                    syndrome,
                    physical_address,
                    ..
                } => {
                    let pc = cpu.register(Register::Pc)?;
                    if syndrome >> 26 == 0x16 {
                        let function = cpu.register(Register::X(0))?;
                        match machine_arm64::psci(function, cpu.register(Register::X(1))?) {
                            machine_arm64::PsciResult::PowerOff => break,
                            machine_arm64::PsciResult::Return(value) => {
                                cpu.set_register(Register::X(0), value)?
                            }
                        }
                        // HVC exits report the next instruction; unlike a data
                        // abort they must not advance PC a second time.
                    } else if let Some((register, read)) =
                        machine_arm64::debug_lock_access(syndrome)
                    {
                        if read && register != 31 {
                            cpu.set_register(Register::X(register), 0)?;
                        }
                        cpu.set_register(Register::Pc, pc.checked_add(4).ok_or("PC overflow")?)?;
                    } else {
                        let access = decode_mmio(syndrome)?;
                        if physical_address
                            .checked_sub(machine_arm64::VIRTIO_BASE)
                            .filter(|offset| {
                                *offset < u64::from(machine_arm64::MAX_VIRTIO_SLOTS) * 0x1000
                            })
                            .is_some()
                        {
                            let mut data = [0u8; 8];
                            let data = &mut data[..usize::from(access.bytes)];
                            if access.write {
                                let value = if access.register == 31 {
                                    0
                                } else {
                                    cpu.register(Register::X(access.register))?
                                };
                                data.copy_from_slice(&value.to_le_bytes()[..data.len()]);
                                devices.write(physical_address, data)?;
                            } else {
                                devices.read(physical_address, data)?;
                                let mut value = [0u8; 8];
                                value[..data.len()].copy_from_slice(data);
                                if access.register != 31 {
                                    cpu.set_register(
                                        Register::X(access.register),
                                        u64::from_le_bytes(value),
                                    )?;
                                }
                            }
                            cpu.set_register(
                                Register::Pc,
                                pc.checked_add(4).ok_or("PC overflow")?,
                            )?;
                            continue;
                        }
                        let offset = physical_address.checked_sub(UART_BASE)
                            .filter(|offset| *offset < 0x1000)
                            .ok_or_else(|| format!("unhandled MMIO {physical_address:#x}, PC={pc:#x}, ESR={syndrome:#x}"))?;
                        if access.write {
                            let value = if access.register == 31 {
                                0
                            } else {
                                cpu.register(Register::X(access.register))?
                            };
                            uart.write(offset, value)?;
                        } else if access.register != 31 {
                            cpu.set_register(
                                Register::X(access.register),
                                u64::from(uart.read(offset)),
                            )?;
                        }
                        cpu.set_register(Register::Pc, pc.checked_add(4).ok_or("PC overflow")?)?;
                    }
                }
                Exit::Canceled => return Err("Linux boot timed out".into()),
                other => {
                    return Err(format!(
                        "unhandled ARM64 exit {other:?}, PC={:#x}",
                        cpu.register(Register::Pc)?
                    )
                    .into())
                }
            }
        }
        let data = log.lock().unwrap_or_else(|e| e.into_inner());
        let marker = if has_disk {
            "VMHOST_UBUNTU_ROOT_READY"
        } else {
            "VMHOST_GUEST_READY ARM64"
        };
        let text = String::from_utf8_lossy(&data);
        if !text.lines().any(|line| line.trim() == marker) {
            return Err("guest powered off without a userspace marker".into());
        }
        if has_disk {
            if !text.lines().any(|line| line.trim() == "NAME=\"Ubuntu\"")
                || !text.lines().any(|line| line.trim() == "VMHOST_NET_READY")
            {
                return Err("Ubuntu identity or network acceptance failed".into());
            }
            for name in ["virtio0", "virtio1"] {
                let serviced = text.lines().any(|line| {
                    let words: Vec<_> = line.split_whitespace().collect();
                    words.last() == Some(&name)
                        && words
                            .get(1)
                            .and_then(|count| count.parse::<u64>().ok())
                            .is_some_and(|count| count > 0)
                });
                if !serviced {
                    return Err(format!("no guest-serviced interrupts for {name}").into());
                }
            }
        }
        if gpu_enabled && has_disk && !text.lines().any(|line| line.trim() == "VMHOST_GPU_READY") {
            return Err("guest DRM driver did not initialize the Venus GPU".into());
        }
        if init_command.is_some()
            && !text
                .lines()
                .any(|line| line.trim() == "VMHOST_CUSTOM_READY")
        {
            return Err("custom guest command did not report success".into());
        }
        Ok(())
    })();
    finished.store(true, Ordering::Release);
    watchdog.join().map_err(|_| "watchdog panicked")?;
    devices.shutdown();
    result?;
    if has_disk {
        println!("PASS: Ubuntu ARM64 rootfs, virtio disk/network interrupts, network RX/TX and PSCI shutdown");
    } else {
        println!("PASS: Linux ARM64 reached userspace and powered off through PSCI");
    }
    Ok(())
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn main() {
    eprintln!("hvf_linux requires Apple Silicon and macOS 15+");
    std::process::exit(1);
}
