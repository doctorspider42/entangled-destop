//! virtio-mmio register layout (VirtIO spec 1.2, section 4.2.2, "MMIO Device
//! Register Layout"). Offsets are relative to the device's slot base address.
//!
//! Only the modern interface (version 2) is implemented; legacy (version 1)
//! guests are not supported.

/// Value of [`MAGIC_VALUE`]: little-endian "virt".
pub const MAGIC: u32 = 0x7472_6976;

/// Device version we report; 2 = modern virtio-mmio.
pub const VERSION: u32 = 2;

// Read-only registers.
pub const MAGIC_VALUE: u64 = 0x000;
pub const VERSION_REG: u64 = 0x004;
pub const DEVICE_ID: u64 = 0x008;
pub const VENDOR_ID: u64 = 0x00c;
pub const DEVICE_FEATURES: u64 = 0x010;
pub const QUEUE_NUM_MAX: u64 = 0x034;
pub const INTERRUPT_STATUS: u64 = 0x060;
pub const CONFIG_GENERATION: u64 = 0x0fc;

// Write-only registers.
pub const DEVICE_FEATURES_SEL: u64 = 0x014;
pub const DRIVER_FEATURES: u64 = 0x020;
pub const DRIVER_FEATURES_SEL: u64 = 0x024;
pub const QUEUE_SEL: u64 = 0x030;
pub const QUEUE_NUM: u64 = 0x038;
pub const QUEUE_NOTIFY: u64 = 0x050;
pub const INTERRUPT_ACK: u64 = 0x064;

// Read-write registers.
pub const QUEUE_READY: u64 = 0x044;
pub const STATUS: u64 = 0x070;
pub const QUEUE_DESC_LOW: u64 = 0x080;
pub const QUEUE_DESC_HIGH: u64 = 0x084;
pub const QUEUE_DRIVER_LOW: u64 = 0x090;
pub const QUEUE_DRIVER_HIGH: u64 = 0x094;
pub const QUEUE_DEVICE_LOW: u64 = 0x0a0;
pub const QUEUE_DEVICE_HIGH: u64 = 0x0a4;

// Shared-memory region registers (spec 4.2.2). Entangled Desktop exposes no shared
// memory regions; per spec, reads of SHM_LEN for a non-existent region must
// return all-ones (a zero would look like a real zero-length region at
// address 0 — Linux virtio_gpu then tries to reserve it and fails its probe).
pub const SHM_SEL: u64 = 0x0ac;
pub const SHM_LEN_LOW: u64 = 0x0b0;
pub const SHM_LEN_HIGH: u64 = 0x0b4;
pub const SHM_BASE_LOW: u64 = 0x0b8;
pub const SHM_BASE_HIGH: u64 = 0x0bc;

/// Start of the device-specific configuration space.
pub const CONFIG_SPACE: u64 = 0x100;

/// Interrupt status bit: a used buffer was added to a queue.
pub const INT_VRING: u32 = 1 << 0;
/// Interrupt status bit: the device configuration changed.
pub const INT_CONFIG: u32 = 1 << 1;

/// Our vendor id ("VMH" is taken from nothing official — virtio-mmio vendor
/// ids carry no registry; Linux ignores the value).
pub const VMHOST_VENDOR_ID: u32 = 0x564d_4800;

/// Formats the kernel command-line clause announcing one virtio-mmio slot to
/// the guest, e.g. `virtio_mmio.device=4K@0xd0000000:5`.
pub fn cmdline_clause(slot_base: u64, irq: u32) -> String {
    format!("virtio_mmio.device=4K@{slot_base:#x}:{irq}")
}

#[cfg(test)]
mod tests {
    #[test]
    fn cmdline_clause_matches_kernel_format() {
        assert_eq!(
            super::cmdline_clause(0xd000_0000, 5),
            "virtio_mmio.device=4K@0xd0000000:5"
        );
    }
}
