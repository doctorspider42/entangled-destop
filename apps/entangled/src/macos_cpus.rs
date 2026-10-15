//! One owning host thread per ARM vCPU, with PSCI power state and coordinated exit.
use machine_arm64::smp::{Action, CpuPower, CpuStart};
use machine_arm64::virtio::MmioBus;
use machine_arm64::{decode_mmio, Pl011, RAM_BASE, UART_BASE};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use vmm_core::hv::arm64::{Exit, Register, SystemRegister};
use vmm_core::hvf::{HvfCanceller, HvfVcpu, HvfVm};

pub(super) struct CpuGroup {
    power: CpuPower,
    cancellers: Mutex<Vec<HvfCanceller>>,
    error: Mutex<Option<String>>,
}

impl CpuGroup {
    pub fn stopping(&self) -> bool {
        self.power.stopped()
    }
    pub fn kick_all(&self) {
        for cancel in self
            .cancellers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
            if let Err(error) = cancel.cancel() {
                tracing::error!(%error, "cannot kick ARM64 CPU group");
            }
        }
    }
    pub fn stop(&self) {
        self.power.stop();
        self.kick_all();
    }
    fn fail(&self, error: String) {
        self.error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert(error);
        self.stop();
    }
    fn register(&self, cpu: &HvfVcpu) {
        self.cancellers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(cpu.canceller());
    }
}

pub(super) struct Machine {
    group: Arc<CpuGroup>,
    bus: Arc<MmioBus>,
    uart: Mutex<Pl011>,
    powered_off: Option<Arc<AtomicBool>>,
}

impl Machine {
    pub fn new(
        count: u32,
        ram: u64,
        bus: Arc<MmioBus>,
        output: Box<dyn Write + Send>,
        powered_off: Option<Arc<AtomicBool>>,
    ) -> Result<Arc<Self>, String> {
        Ok(Arc::new(Self {
            group: Arc::new(CpuGroup {
                power: CpuPower::new(count, RAM_BASE, ram).map_err(|e| e.to_string())?,
                cancellers: Mutex::new(Vec::with_capacity(count as usize)),
                error: Mutex::new(None),
            }),
            bus,
            uart: Mutex::new(Pl011::new(output)),
            powered_off,
        }))
    }
}

/// Always retire every owning thread before devices or RAM can be reset.
struct Workers {
    group: Arc<CpuGroup>,
    workers: Vec<JoinHandle<()>>,
}
impl Drop for Workers {
    fn drop(&mut self) {
        self.group.stop();
        for worker in self.workers.drain(..) {
            // Repeated kicks cover a stop racing between the loop's check and
            // hv_vcpu_run. Never wait for a powered-off CPU to enter the guest.
            while !worker.is_finished() {
                self.group.kick_all();
                std::thread::sleep(Duration::from_millis(1));
            }
            if worker.join().is_err() {
                self.group
                    .fail("ARM64 secondary CPU thread panicked".into());
            }
        }
    }
}

fn create_cpu(vm: &HvfVm, index: u32) -> Result<HvfVcpu, String> {
    let mut cpu = vm.create_vcpu().map_err(|e| e.to_string())?;
    cpu.allow_guest_debug_registers()
        .map_err(|e| e.to_string())?;
    cpu.set_system_register(SystemRegister::Mpidr, (1 << 31) | u64::from(index))
        .map_err(|e| e.to_string())?;
    Ok(cpu)
}

fn reset_cpu(cpu: &mut HvfVcpu, start: CpuStart) -> Result<(), String> {
    // PSCI warm boot enters EL1h with MMU off, masked interrupts and x0=context.
    // Disable the previous virtual timer before reusing a powered-off vCPU.
    for (register, value) in [
        (SystemRegister::VirtualTimerControl, 2),
        (SystemRegister::Sctlr, 0x30d00800),
        (SystemRegister::Cpacr, 0),
        (SystemRegister::SpEl1, 0),
        (SystemRegister::Ttbr0, 0),
        (SystemRegister::Ttbr1, 0),
        (SystemRegister::Tcr, 0),
        (SystemRegister::Mair, 0),
        (SystemRegister::Vbar, 0),
        (SystemRegister::ContextId, 0),
    ] {
        cpu.set_system_register(register, value)
            .map_err(|e| e.to_string())?;
    }
    for index in 0..=30 {
        cpu.set_register(Register::X(index), 0)
            .map_err(|e| e.to_string())?;
    }
    cpu.set_register(Register::X(0), start.context)
        .map_err(|e| e.to_string())?;
    cpu.set_register(Register::Pc, start.entry)
        .map_err(|e| e.to_string())?;
    cpu.set_register(Register::Pstate, 0x3c5)
        .map_err(|e| e.to_string())
}

pub(super) fn run(
    vm: &HvfVm,
    count: u32,
    boot: CpuStart,
    machine: Arc<Machine>,
    ready: impl FnOnce(Arc<CpuGroup>) -> Result<(), String>,
) -> Result<(), String> {
    // Create in deterministic order: native GIC redistributor indices and DT
    // affinities then refer to the same CPUs, regardless of host scheduling.
    let mut primary = create_cpu(vm, 0)?;
    machine.group.register(&primary);
    let mut workers = Workers {
        group: machine.group.clone(),
        workers: Vec::new(),
    };
    for index in 1..count {
        let vm = vm.clone();
        let machine = machine.clone();
        let (tx, rx) = mpsc::sync_channel(1);
        workers.workers.push(
            std::thread::Builder::new()
                .name(format!("entangled-vcpu-{index}"))
                .spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let created = create_cpu(&vm, index);
                        if let Err(error) = &created {
                            let _ = tx.send(Err(error.clone()));
                        }
                        let mut cpu = created?;
                        machine.group.register(&cpu);
                        tx.send(Ok(()))
                            .map_err(|_| "ARM64 CPU coordinator stopped".to_string())?;
                        if let Some(start) = machine
                            .group
                            .power
                            .wait_for_start(index)
                            .map_err(|e| e.to_string())?
                        {
                            run_cpu(&mut cpu, index, start, &machine)?;
                        }
                        Ok::<(), String>(())
                    }))
                    .map_err(|_| format!("ARM64 CPU {index} panicked"))
                    .and_then(|result| result);
                    if let Err(error) = result {
                        machine.group.fail(error);
                    }
                })
                .map_err(|e| e.to_string())?,
        );
        rx.recv_timeout(Duration::from_secs(10))
            .map_err(|e| format!("ARM64 CPU {index} creation: {e}"))??;
    }
    ready(machine.group.clone())?;
    let result = run_cpu(&mut primary, 0, boot, &machine);
    machine.group.stop();
    drop(workers);
    result?;
    if let Some(error) = machine
        .group
        .error
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
    {
        return Err(error);
    }
    Ok(())
}

fn run_cpu(
    cpu: &mut HvfVcpu,
    index: u32,
    start: CpuStart,
    machine: &Machine,
) -> Result<(), String> {
    let mut start = start;
    'power: loop {
        if machine.group.stopping() {
            return Ok(());
        }
        reset_cpu(cpu, start)?;
        tracing::info!(cpu = index, entry = start.entry, "ARM64 vCPU running");
        loop {
            if machine.group.stopping() {
                return Ok(());
            }
            match cpu.run().map_err(|e| e.to_string())? {
                Exit::Canceled => continue,
                Exit::Exception {
                    syndrome,
                    physical_address,
                    ..
                } => {
                    if syndrome >> 26 == 0x16 {
                        let [a, b, c] = [1, 2, 3].map(|register| {
                            cpu.register(Register::X(register))
                                .map_err(|e| e.to_string())
                        });
                        let args = [a?, b?, c?];
                        match machine.group.power.psci(
                            index,
                            cpu.register(Register::X(0)).map_err(|e| e.to_string())?,
                            args,
                        ) {
                            Action::Return(value) => cpu
                                .set_register(Register::X(0), value)
                                .map_err(|e| e.to_string())?,
                            Action::SystemOff => {
                                if let Some(off) = &machine.powered_off {
                                    off.store(true, Ordering::Release);
                                }
                                machine.group.stop();
                                return Ok(());
                            }
                            Action::CpuOff => {
                                cpu.set_system_register(SystemRegister::VirtualTimerControl, 2)
                                    .map_err(|e| e.to_string())?;
                                tracing::info!(cpu = index, "ARM64 vCPU powered off");
                                match machine
                                    .group
                                    .power
                                    .wait_for_start(index)
                                    .map_err(|e| e.to_string())?
                                {
                                    Some(next) => {
                                        start = next;
                                        continue 'power;
                                    }
                                    None => return Ok(()),
                                }
                            }
                        }
                        continue;
                    }
                    let pc = cpu.register(Register::Pc).map_err(|e| e.to_string())?;
                    if let Some((register, read)) = machine_arm64::debug_lock_access(syndrome) {
                        if read && register != 31 {
                            cpu.set_register(Register::X(register), 0)
                                .map_err(|e| e.to_string())?;
                        }
                    } else {
                        mmio(cpu, syndrome, physical_address, machine)?;
                    }
                    cpu.set_register(Register::Pc, pc.checked_add(4).ok_or("PC overflows")?)
                        .map_err(|e| e.to_string())?;
                }
                other => return Err(format!("unexpected ARM64 CPU {index} exit: {other:?}")),
            }
        }
    }
}

fn mmio(cpu: &mut HvfVcpu, syndrome: u64, address: u64, machine: &Machine) -> Result<(), String> {
    let access = decode_mmio(syndrome).map_err(|e| e.to_string())?;
    let value = if access.register == 31 {
        0
    } else {
        cpu.register(Register::X(access.register))
            .map_err(|e| e.to_string())?
    };
    let width = usize::from(access.bytes);
    let mut data = [0u8; 8];
    if let Some(offset) = address
        .checked_sub(UART_BASE)
        .filter(|offset| *offset < 0x1000)
    {
        let mut uart = machine.uart.lock().unwrap_or_else(|e| e.into_inner());
        if access.write {
            uart.write(offset, value).map_err(|e| e.to_string())?;
        } else {
            data[..4].copy_from_slice(&uart.read(offset).to_le_bytes());
        }
    } else if access.write {
        machine
            .bus
            .write(address, &value.to_le_bytes()[..width])
            .map_err(|e| e.to_string())?;
    } else {
        machine
            .bus
            .read(address, &mut data[..width])
            .map_err(|e| e.to_string())?;
    }
    if !access.write && access.register != 31 {
        let bits = u32::from(access.bytes) * 8;
        let raw = u64::from_le_bytes(data) & (u64::MAX >> (64 - bits));
        let extended = if access.sign_extend {
            (((raw << (64 - bits)) as i64) >> (64 - bits)) as u64
        } else {
            raw
        };
        cpu.set_register(
            Register::X(access.register),
            if access.register_64 {
                extended
            } else {
                u64::from(extended as u32)
            },
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}
