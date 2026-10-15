//! ARM64 contracts independent of the host hypervisor. Machine code uses
//! these types instead of Apple's FFI register identifiers.
use super::HvError;

#[derive(Debug, Clone, Copy)]
pub enum Register {
    X(u8),
    Pc,
    Pstate,
}

#[derive(Debug, Clone, Copy)]
pub enum SystemRegister {
    Mpidr,
    Sctlr,
    Cpacr,
    SpEl1,
    Ttbr0,
    Ttbr1,
    Tcr,
    Mair,
    Vbar,
    ContextId,
    VirtualTimerControl,
}

#[derive(Debug, Clone, Copy)]
pub struct GicLayout {
    pub distributor_base: u64,
    pub distributor_size: u64,
    pub redistributor_base: u64,
    pub redistributor_size: u64,
    pub redistributor_stride: u64,
    pub virtual_timer_irq: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Canceled,
    Exception {
        syndrome: u64,
        virtual_address: u64,
        physical_address: u64,
    },
    VirtualTimer,
    Unknown(u32),
}

pub trait VcpuRegisters {
    fn register(&self, register: Register) -> Result<u64, HvError>;
    fn set_register(&mut self, register: Register, value: u64) -> Result<(), HvError>;
}
