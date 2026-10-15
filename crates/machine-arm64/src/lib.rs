//! ARM64 platform independent of the host hypervisor's API types.
use std::io::Write;

use thiserror::Error;
use vm_fdt::FdtWriter;
use vmm_core::hv::arm64::GicLayout;

pub const RAM_BASE: u64 = 0x4000_0000;
pub const UART_BASE: u64 = 0x0900_0000;
pub const GIC_DISTRIBUTOR: u64 = 0x0800_0000;
pub const GIC_REDISTRIBUTOR: u64 = 0x0a00_0000;
pub const UART_IRQ: u32 = 32;
pub const MAX_CPUS: u32 = 64;
pub const VIRTIO_BASE: u64 = 0x0c00_0000;
pub const VIRTIO_IRQ: u32 = 33;
pub const MAX_VIRTIO_SLOTS: u32 = 8;

#[derive(Debug, Error)]
pub enum MachineError {
    #[error("invalid ARM64 machine: {0}")]
    Invalid(String),
    #[error("Device Tree: {0}")]
    Fdt(#[from] vm_fdt::Error),
    #[error("ARM64 boot: {0}")]
    Boot(String),
    #[error("serial output: {0}")]
    Serial(#[from] std::io::Error),
    #[error("virtio transport: {0}")]
    Transport(#[from] virtio_core::TransportError),
}

pub struct TreeConfig<'a> {
    pub memory_bytes: u64,
    pub cpus: u32,
    pub gic: GicLayout,
    pub cmdline: &'a str,
    pub initrd: Option<(u64, u64)>,
    pub virtio_slots: u32,
}

pub mod smp;

pub fn device_tree(config: &TreeConfig<'_>) -> Result<Vec<u8>, MachineError> {
    if config.cpus == 0 || config.cpus > MAX_CPUS || config.memory_bytes < 64 << 20 {
        return Err(MachineError::Invalid(
            "CPU count must be 1..64 and RAM at least 64 MiB".into(),
        ));
    }
    if config.cmdline.len() > 4095 || config.cmdline.contains('\0') {
        return Err(MachineError::Invalid(
            "kernel command line is too long or contains NUL".into(),
        ));
    }
    if config.virtio_slots > MAX_VIRTIO_SLOTS {
        return Err(MachineError::Invalid("too many virtio slots".into()));
    }
    let ram_end = RAM_BASE
        .checked_add(config.memory_bytes)
        .ok_or_else(|| MachineError::Invalid("RAM address overflows".into()))?;
    if let Some((start, len)) = config.initrd {
        if len == 0 || start < RAM_BASE || start.checked_add(len).is_none_or(|end| end > ram_end) {
            return Err(MachineError::Invalid("initrd is outside RAM".into()));
        }
    }
    let ranges = [
        (RAM_BASE, config.memory_bytes),
        (UART_BASE, 0x1000),
        (VIRTIO_BASE, u64::from(MAX_VIRTIO_SLOTS) * 0x1000),
        (config.gic.distributor_base, config.gic.distributor_size),
        (config.gic.redistributor_base, config.gic.redistributor_size),
    ];
    for (index, &(base, len)) in ranges.iter().enumerate() {
        let end = base
            .checked_add(len)
            .filter(|_| len != 0)
            .ok_or_else(|| MachineError::Invalid("empty or overflowing platform range".into()))?;
        for &(other, size) in &ranges[..index] {
            let other_end = other
                .checked_add(size)
                .ok_or_else(|| MachineError::Invalid("platform range overflows".into()))?;
            if base < other_end && other < end {
                return Err(MachineError::Invalid("platform ranges overlap".into()));
            }
        }
    }
    let required_redistributors = config
        .gic
        .redistributor_stride
        .checked_mul(u64::from(config.cpus));
    if config.gic.redistributor_stride < 0x20000
        || required_redistributors.is_none_or(|size| size > config.gic.redistributor_size)
    {
        return Err(MachineError::Invalid(
            "GIC redistributor region is too small".into(),
        ));
    }
    let mut fdt = FdtWriter::new()?;
    let root = fdt.begin_node("")?;
    fdt.property_string("compatible", "entangled,arm64-virt")?;
    fdt.property_string("model", "Entangled ARM64")?;
    fdt.property_u32("#address-cells", 2)?;
    fdt.property_u32("#size-cells", 2)?;
    fdt.property_u32("interrupt-parent", 1)?;
    let aliases = fdt.begin_node("aliases")?;
    fdt.property_string("serial0", "/pl011@9000000")?;
    fdt.end_node(aliases)?;
    let chosen = fdt.begin_node("chosen")?;
    fdt.property_string("bootargs", config.cmdline)?;
    fdt.property_string("stdout-path", "serial0:115200n8")?;
    if let Some((start, len)) = config.initrd {
        fdt.property_u64("linux,initrd-start", start)?;
        fdt.property_u64(
            "linux,initrd-end",
            start
                .checked_add(len)
                .ok_or_else(|| MachineError::Invalid("initrd overflows".into()))?,
        )?;
    }
    fdt.end_node(chosen)?;
    let memory = fdt.begin_node("memory@40000000")?;
    fdt.property_string("device_type", "memory")?;
    fdt.property_array_u64("reg", &[RAM_BASE, config.memory_bytes])?;
    fdt.end_node(memory)?;
    let cpus = fdt.begin_node("cpus")?;
    fdt.property_u32("#address-cells", 2)?;
    fdt.property_u32("#size-cells", 0)?;
    for index in 0..config.cpus {
        let cpu = fdt.begin_node(&format!("cpu@{index:x}"))?;
        fdt.property_string("device_type", "cpu")?;
        fdt.property_string("compatible", "arm,arm-v8")?;
        fdt.property_string("enable-method", "psci")?;
        fdt.property_u64("reg", u64::from(index))?;
        fdt.end_node(cpu)?;
    }
    fdt.end_node(cpus)?;
    let psci = fdt.begin_node("psci")?;
    fdt.property_string("compatible", "arm,psci-0.2")?;
    fdt.property_string("method", "hvc")?;
    fdt.end_node(psci)?;
    let gic = fdt.begin_node(&format!(
        "interrupt-controller@{:x}",
        config.gic.distributor_base
    ))?;
    fdt.property_string("compatible", "arm,gic-v3")?;
    fdt.property_phandle(1)?;
    fdt.property_null("interrupt-controller")?;
    fdt.property_u32("#interrupt-cells", 3)?;
    fdt.property_u32("#address-cells", 2)?;
    fdt.property_u32("#size-cells", 2)?;
    fdt.property_null("ranges")?;
    fdt.property_array_u64(
        "reg",
        &[
            config.gic.distributor_base,
            config.gic.distributor_size,
            config.gic.redistributor_base,
            config.gic.redistributor_size,
        ],
    )?;
    fdt.property_u64("redistributor-stride", config.gic.redistributor_stride)?;
    fdt.property_array_u32("interrupts", &[1, 9, 4])?;
    fdt.end_node(gic)?;
    let timer = fdt.begin_node("timer")?;
    fdt.property_string("compatible", "arm,armv8-timer")?;
    let virtual_ppi = config
        .gic
        .virtual_timer_irq
        .checked_sub(16)
        .filter(|irq| *irq < 16)
        .ok_or_else(|| MachineError::Invalid("virtual timer must be a PPI".into()))?;
    fdt.property_array_u32(
        "interrupts",
        &[1, 13, 4, 1, 14, 4, 1, virtual_ppi, 4, 1, 10, 4],
    )?;
    fdt.end_node(timer)?;
    let clock = fdt.begin_node("uart-clock")?;
    fdt.property_string("compatible", "fixed-clock")?;
    fdt.property_phandle(2)?;
    fdt.property_u32("#clock-cells", 0)?;
    fdt.property_u32("clock-frequency", 24_000_000)?;
    fdt.end_node(clock)?;
    let uart = fdt.begin_node("pl011@9000000")?;
    fdt.property("compatible", b"arm,pl011\0arm,primecell\0")?;
    fdt.property_array_u64("reg", &[UART_BASE, 0x1000])?;
    fdt.property_array_u32("clocks", &[2, 2])?;
    fdt.property("clock-names", b"uartclk\0apb_pclk\0")?;
    fdt.property_array_u32("interrupts", &[0, UART_IRQ - 32, 1])?;
    fdt.end_node(uart)?;
    for slot in 0..config.virtio_slots {
        let base = VIRTIO_BASE + u64::from(slot) * 0x1000;
        let node = fdt.begin_node(&format!("virtio_mmio@{base:x}"))?;
        fdt.property_string("compatible", "virtio,mmio")?;
        fdt.property_array_u64("reg", &[base, 0x1000])?;
        fdt.property_array_u32("interrupts", &[0, VIRTIO_IRQ + slot - 32, 1])?;
        fdt.property_null("dma-coherent")?;
        fdt.end_node(node)?;
    }
    fdt.end_node(root)?;
    Ok(fdt.finish()?)
}

/// Minimal PL011: transmit and register identification. Receive/interrupt
/// support is added with the interactive guest path; TX never blocks.
pub struct Pl011 {
    output: Box<dyn Write + Send>,
}

impl Pl011 {
    pub fn new(output: Box<dyn Write + Send>) -> Self {
        Self { output }
    }
    pub fn read(&self, offset: u64) -> u32 {
        match offset {
            0x18 => 0x90, // TXFE | RXFE
            0xfe0 => 0x11,
            0xfe4 => 0x10,
            0xfe8 => 0x14,
            0xff0 => 0x0d,
            0xff4 => 0xf0,
            0xff8 => 0x05,
            0xffc => 0xb1,
            _ => 0,
        }
    }
    pub fn write(&mut self, offset: u64, value: u64) -> Result<(), MachineError> {
        if offset == 0 {
            self.output.write_all(&[value as u8])?;
            self.output.flush()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MmioAccess {
    pub register: u8,
    pub bytes: u8,
    pub write: bool,
    pub sign_extend: bool,
    pub register_64: bool,
}

pub fn decode_mmio(syndrome: u64) -> Result<MmioAccess, MachineError> {
    if !matches!(syndrome >> 26, 0x24 | 0x25) || syndrome & (1 << 24) == 0 {
        return Err(MachineError::Invalid(format!(
            "unsupported data abort {syndrome:#x}"
        )));
    }
    Ok(MmioAccess {
        register: ((syndrome >> 16) & 31) as u8,
        bytes: 1 << ((syndrome >> 22) & 3),
        write: syndrome & (1 << 6) != 0,
        sign_extend: syndrome & (1 << 21) != 0,
        register_64: syndrome & (1 << 15) != 0,
    })
}

/// OS debug locks are not implemented by this virtual platform. They are
/// separate from the framework's virtual breakpoint/watchpoint registers.
/// Return the operand register and direction only for the three lock registers;
/// unknown system accesses must remain errors rather than silently succeeding.
pub fn debug_lock_access(syndrome: u64) -> Option<(u8, bool)> {
    if syndrome >> 26 != 0x18 {
        return None;
    }
    let op0 = (syndrome >> 20) & 3;
    let op1 = (syndrome >> 14) & 7;
    let op2 = (syndrome >> 17) & 7;
    let crn = (syndrome >> 10) & 15;
    let crm = (syndrome >> 1) & 15;
    let read = syndrome & 1 != 0;
    if (op0, op1, crn, op2) != (2, 0, 1, 4) {
        return None;
    }
    match (crm, read) {
        (0, false) | (1, true) | (3, _) => Some((((syndrome >> 5) & 31) as u8, read)),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PsciResult {
    Return(u64),
    PowerOff,
}

/// Single-CPU bring-up firmware contract. CPU_ON/SUSPEND/RESET are not yet
/// implemented, so FEATURES must not claim support for those operations.
pub fn psci(function: u64, argument: u64) -> PsciResult {
    const NOT_SUPPORTED: u64 = u64::MAX;
    PsciResult::Return(match function {
        0x8400_0000 | 0x8000_0000 => 0x1_0001,
        0x8400_0008 => return PsciResult::PowerOff,
        0x8400_000a => match argument {
            0x8400_0000 | 0x8400_0008 | 0x8400_000a | 0x8000_0000 => 0,
            _ => NOT_SUPPORTED,
        },
        _ => NOT_SUPPORTED,
    })
}

#[cfg(target_arch = "aarch64")]
pub mod boot;
pub mod shm;
pub mod virtio;

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> TreeConfig<'static> {
        TreeConfig {
            memory_bytes: 512 << 20,
            cpus: 1,
            cmdline: "console=ttyAMA0",
            initrd: None,
            virtio_slots: 2,
            gic: GicLayout {
                distributor_base: GIC_DISTRIBUTOR,
                distributor_size: 0x10000,
                redistributor_base: GIC_REDISTRIBUTOR,
                redistributor_size: 32 << 20,
                redistributor_stride: 0x20000,
                virtual_timer_irq: 27,
            },
        }
    }

    #[test]
    fn tree_refuses_invalid_platform_before_emitting_a_blob() {
        let mut c = config();
        assert!(device_tree(&c).is_ok());
        c.memory_bytes = u64::MAX;
        assert!(device_tree(&c).is_err());
        c = config();
        c.gic.redistributor_base = UART_BASE;
        assert!(device_tree(&c).is_err());
        c = config();
        c.gic.redistributor_size = 0x10000;
        assert!(device_tree(&c).is_err());
        c = config();
        c.initrd = Some((RAM_BASE - 1, 1));
        assert!(device_tree(&c).is_err());
        c.initrd = Some((RAM_BASE, u64::MAX));
        assert!(device_tree(&c).is_err());
        c = config();
        c.virtio_slots = MAX_VIRTIO_SLOTS + 1;
        assert!(device_tree(&c).is_err());
        c = config();
        c.cpus = MAX_CPUS + 1;
        assert!(device_tree(&c).is_err());
        c = config();
        c.gic.virtual_timer_irq = 32;
        assert!(device_tree(&c).is_err());
    }

    #[test]
    fn abort_decoder_requires_valid_syndrome_and_preserves_zero_register() {
        assert!(decode_mmio(0x96000044).is_err()); // stage-1 abort without ISV
        assert!(decode_mmio(0x622807e6).is_err()); // trapped system register
        let access =
            decode_mmio((0x24 << 26) | (1 << 24) | (3 << 22) | (31 << 16) | (1 << 6)).unwrap();
        assert_eq!(access.register, 31);
        assert_eq!(access.bytes, 8);
        assert!(access.write);
    }

    #[test]
    fn unsupported_system_accesses_do_not_become_no_ops() {
        assert_eq!(debug_lock_access(0x622807e6), Some((31, false))); // OSDLR
        assert_eq!(debug_lock_access(0x622807e0), Some((31, false))); // OSLAR
        assert_eq!(debug_lock_access(0x622807e1), None); // write-only OSLAR read
        assert_eq!(debug_lock_access(0x622807e3), Some((31, true))); // OSLSR
        assert_eq!(debug_lock_access(0x622807e7), Some((31, true))); // OSDLR read
        assert_eq!(debug_lock_access(0x622803e6), None); // other CRn
    }

    #[test]
    fn firmware_advertises_only_operations_it_implements() {
        assert_eq!(psci(0x8400000a, 0xc4000003), PsciResult::Return(u64::MAX));
        assert_eq!(psci(0xc4000003, 0), PsciResult::Return(u64::MAX));
        assert_eq!(psci(0x8400000a, 0x84000008), PsciResult::Return(0));
        assert_eq!(psci(0x84000008, 0), PsciResult::PowerOff);
    }
}
