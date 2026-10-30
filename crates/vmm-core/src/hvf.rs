//! Apple Silicon Hypervisor.framework backend. CPU state is ARM64-specific;
//! guest memory remains the same checked `GuestMem` used by KVM and WHP.
//!
//! Apple associates a vCPU with its creating thread. A !Send marker keeps
//! each CPU there; an Arc retains VM RAM across all owning vCPU threads.
//! Native GIC v3 and checked shared-memory mappings live here; Device Tree
//! and Linux machine assembly live in machine-arm64.

use std::collections::BTreeMap;
use std::ffi::{c_char, c_void};
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use thiserror::Error;
use vm_memory::{Address, GuestMemory, GuestMemoryRegion};

use crate::GuestMem;

pub use crate::hv::arm64::{Exit as Arm64Exit, Register};

mod snapshot;

static VM_EXISTS: AtomicBool = AtomicBool::new(false);
const PAGE_SIZE: usize = 16 * 1024;

#[derive(Debug, Default, Clone, Copy)]
pub struct HvfOptions {
    /// macOS 26+: allow 4 KiB shared-memory mappings for a Linux/Venus guest.
    /// Resolved at runtime so older hosts can still use the default backend.
    pub small_page_mappings: bool,
}

struct VmConfig(NonNull<c_void>);
impl VmConfig {
    fn small_pages() -> Result<Self, HvfError> {
        // SAFETY: Darwin's RTLD_DEFAULT pseudo-handle and a static NUL-terminated
        // SDK symbol; dlsym returns null when the OS lacks the macOS 26 API.
        let symbol = unsafe {
            dlsym(
                (-2isize) as *mut c_void,
                c"hv_vm_config_set_ipa_granule".as_ptr(),
            )
        };
        if symbol.is_null() {
            return Err(HvfError::Memory(
                "4 KiB GPA mappings require macOS 26+".into(),
            ));
        }
        // SAFETY: the resolved SDK function has exactly this C signature.
        let set_granule: unsafe extern "C" fn(*mut c_void, u32) -> i32 =
            unsafe { std::mem::transmute(symbol) };
        // SAFETY: the create function returns one owned configuration reference.
        let config = Self(
            NonNull::new(unsafe { hv_vm_config_create() })
                .ok_or_else(|| HvfError::Memory("cannot allocate VM configuration".into()))?,
        );
        check(
            // SAFETY: the configuration is live; SDK enum 0 requests 4 KiB granules.
            unsafe { set_granule(config.0.as_ptr(), 0) },
            "hv_vm_config_set_ipa_granule",
        )?;
        Ok(config)
    }
}
impl Drop for VmConfig {
    fn drop(&mut self) {
        // SAFETY: this reference was returned by create and is released once.
        unsafe { os_release(self.0.as_ptr()) };
    }
}

#[derive(Debug, Error)]
pub enum HvfError {
    #[error("Hypervisor.framework {operation} failed with {status:#x}; the executable needs the com.apple.security.hypervisor entitlement")]
    Api {
        operation: &'static str,
        status: i32,
    },
    #[error("Hypervisor.framework permits one VM per process")]
    VmExists,
    #[error("invalid ARM64 guest memory: {0}")]
    Memory(String),
    #[error("ARM64 general register index {0} is outside X0..X30")]
    Register(u8),
    #[error("Hypervisor.framework returned a null vCPU exit pointer")]
    NullExit,
}

fn check(status: i32, operation: &'static str) -> Result<(), HvfError> {
    if status == 0 {
        Ok(())
    } else {
        Err(HvfError::Api { operation, status })
    }
}

fn register_number(register: Register) -> Result<u32, HvfError> {
    match register {
        Register::X(index @ 0..=30) => Ok(u32::from(index)),
        Register::X(index) => Err(HvfError::Register(index)),
        Register::Pc => Ok(31),
        Register::Pstate => Ok(34),
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct Exception {
    syndrome: u64,
    virtual_address: u64,
    physical_address: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct RawExit {
    reason: u32,
    exception: Exception,
}

// The signatures and repr(C) layouts follow Apple's hv_vm/hv_vcpu SDK headers.
#[link(name = "Hypervisor", kind = "framework")]
extern "C" {
    fn hv_vm_get_max_vcpu_count(count: *mut u32) -> i32;
    fn hv_vm_create(config: *const c_void) -> i32;
    fn hv_vm_config_create() -> *mut c_void;
    fn hv_vm_destroy() -> i32;
    fn hv_vm_map(address: *mut c_void, ipa: u64, size: usize, flags: u64) -> i32;
    fn hv_vm_unmap(ipa: u64, size: usize) -> i32;
    fn hv_vcpu_create(cpu: *mut u64, exit: *mut *mut RawExit, config: *const c_void) -> i32;
    fn hv_vcpu_destroy(cpu: u64) -> i32;
    fn hv_vcpu_run(cpu: u64) -> i32;
    fn hv_vcpu_get_reg(cpu: u64, reg: u32, value: *mut u64) -> i32;
    fn hv_vcpu_set_reg(cpu: u64, reg: u32, value: u64) -> i32;
    fn hv_vcpus_exit(cpus: *mut u64, count: u32) -> i32;
    fn hv_vcpu_set_sys_reg(cpu: u64, reg: u16, value: u64) -> i32;
    fn hv_vcpu_set_trap_debug_reg_accesses(cpu: u64, trap: bool) -> i32;
    fn hv_gic_config_create() -> *mut c_void;
    fn hv_gic_config_set_distributor_base(config: *mut c_void, address: u64) -> i32;
    fn hv_gic_config_set_redistributor_base(config: *mut c_void, address: u64) -> i32;
    fn hv_gic_create(config: *mut c_void) -> i32;
    fn hv_gic_get_distributor_size(size: *mut usize) -> i32;
    fn hv_gic_get_redistributor_region_size(size: *mut usize) -> i32;
    fn hv_gic_get_redistributor_size(size: *mut usize) -> i32;
    fn hv_gic_get_intid(interrupt: u16, intid: *mut u32) -> i32;
    fn hv_gic_set_spi(intid: u32, level: bool) -> i32;
    fn os_release(object: *mut c_void);
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
}

/// Native GIC v3 (requires macOS 15 or newer). Created before any vCPU;
/// its lifetime and all mappings are retained by the VM.
#[derive(Clone)]
pub struct HvfGic {
    vm: HvfVm,
    layout: crate::hv::arm64::GicLayout,
}

impl HvfGic {
    pub fn new(
        vm: &HvfVm,
        distributor_base: u64,
        redistributor_base: u64,
    ) -> Result<Self, HvfError> {
        if Arc::strong_count(&vm.0) != 1 {
            return Err(HvfError::Memory(
                "create the GIC before sharing the VM or creating CPUs".into(),
            ));
        }
        let mut distributor_size = 0;
        let mut redistributor_size = 0;
        let mut redistributor_stride = 0;
        let mut virtual_timer_irq = 0;
        check(
            // SAFETY: each API writes a single output local of its declared type.
            unsafe { hv_gic_get_distributor_size(&mut distributor_size) },
            "hv_gic_get_distributor_size",
        )?;
        check(
            // SAFETY: the output local is valid and writable for this call.
            unsafe { hv_gic_get_redistributor_region_size(&mut redistributor_size) },
            "hv_gic_get_redistributor_region_size",
        )?;
        check(
            // SAFETY: the output local is valid and writable for this call.
            unsafe { hv_gic_get_redistributor_size(&mut redistributor_stride) },
            "hv_gic_get_redistributor_size",
        )?;
        check(
            // SAFETY: 27 is the SDK's EL1 virtual timer enum, and the output is a u32 local.
            unsafe { hv_gic_get_intid(27, &mut virtual_timer_irq) },
            "hv_gic_get_intid",
        )?;
        for (base, size) in [
            (distributor_base, distributor_size),
            (redistributor_base, redistributor_size),
        ] {
            let end = base
                .checked_add(size as u64)
                .ok_or_else(|| HvfError::Memory("GIC range overflows".into()))?;
            for region in vm.memory().iter() {
                let start = region.start_addr().raw_value();
                let ram_end = start
                    .checked_add(region.len())
                    .ok_or_else(|| HvfError::Memory("RAM range overflows".into()))?;
                if base < ram_end && start < end {
                    return Err(HvfError::Memory("GIC overlaps guest RAM".into()));
                }
            }
        }
        let mut mappings = vm.0.mappings.lock().unwrap_or_else(|e| e.into_inner());
        let mut planned = mappings.clone();
        for (base, size) in [
            (distributor_base, distributor_size),
            (redistributor_base, redistributor_size),
        ] {
            mapping_allowed(&planned, base, size as u64, 0)?;
            planned.insert(
                base,
                Mapping {
                    len: size as u64,
                    owner: 0,
                },
            );
        }
        // SAFETY: a process VM exists, no CPU can exist while this Arc is unique.
        let config = unsafe { hv_gic_config_create() };
        if config.is_null() {
            return Err(HvfError::Memory("cannot allocate GIC configuration".into()));
        }
        let result = (|| {
            check(
                // SAFETY: config is a live configuration; the API validates address alignment.
                unsafe { hv_gic_config_set_distributor_base(config, distributor_base) },
                "hv_gic_config_set_distributor_base",
            )?;
            check(
                // SAFETY: the configuration remains live and the API validates the base.
                unsafe { hv_gic_config_set_redistributor_base(config, redistributor_base) },
                "hv_gic_config_set_redistributor_base",
            )?;
            // SAFETY: the VM has no CPU yet and config remains live until after creation.
            check(unsafe { hv_gic_create(config) }, "hv_gic_create")
        })();
        // SAFETY: the create API returned one owned reference, released exactly once.
        unsafe { os_release(config) };
        result?;
        *mappings = planned;
        Ok(Self {
            vm: vm.clone(),
            layout: crate::hv::arm64::GicLayout {
                distributor_base,
                distributor_size: distributor_size as u64,
                redistributor_base,
                redistributor_size: redistributor_size as u64,
                redistributor_stride: redistributor_stride as u64,
                virtual_timer_irq,
            },
        })
    }

    pub fn layout(&self) -> crate::hv::arm64::GicLayout {
        self.layout
    }

    pub fn set_spi(&self, interrupt: u32, level: bool) -> Result<(), HvfError> {
        let _retain_vm = &self.vm;
        check(
            // SAFETY: the retained VM owns the GIC. The API validates the interrupt ID.
            unsafe { hv_gic_set_spi(interrupt, level) },
            "hv_gic_set_spi",
        )
    }
}

struct HvfGpaMapper {
    vm: HvfVm,
    owner: u64,
}

impl crate::shm::GpaMapper for HvfGpaMapper {
    fn map_range(&self, gpa: u64, range: crate::shm::HostRange) -> Result<(), crate::hv::HvError> {
        let page = self.vm.0.page_size;
        if gpa % page != 0
            || range.addr() % page != 0
            || range.len() % page != 0
            || range.is_empty()
            || range.addr().checked_add(range.len()).is_none()
        {
            return Err(crate::hv::HvError::Registers(
                "HVF shared range is not aligned or overflows".into(),
            ));
        }
        let len = usize::try_from(range.len())
            .map_err(|e| crate::hv::HvError::Registers(e.to_string()))?;
        let mut mappings = self.vm.0.mappings.lock().unwrap_or_else(|e| e.into_inner());
        mapping_allowed(&mappings, gpa, range.len(), self.owner)
            .map_err(|e| crate::hv::HvError::Registers(e.to_string()))?;
        if let Some(old) = mappings.get(&gpa) {
            unmap_or_abort(gpa, old.len);
            mappings.remove(&gpa);
        }
        check(
            // SAFETY: HostRange promises live backing until unmap; the registry
            // excludes RAM/GIC/other windows. The mapper retains the live VM.
            // READ|WRITE only: shared GPU data is never executable.
            unsafe { hv_vm_map(range.addr() as *mut c_void, gpa, len, 3) },
            "hv_vm_map(shm)",
        )
        .map_err(|e| crate::hv::HvError::Registers(e.to_string()))?;
        mappings.insert(
            gpa,
            Mapping {
                len: range.len(),
                owner: self.owner,
            },
        );
        Ok(())
    }

    fn unmap_range(&self, gpa: u64, len: u64) -> Result<(), crate::hv::HvError> {
        let mut mappings = self.vm.0.mappings.lock().unwrap_or_else(|e| e.into_inner());
        if !mappings
            .get(&gpa)
            .is_some_and(|entry| entry.owner == self.owner && entry.len == len)
        {
            return Err(crate::hv::HvError::Registers(
                "HVF unmap does not name this window's live range".into(),
            ));
        }
        unmap_or_abort(gpa, len);
        mappings.remove(&gpa);
        Ok(())
    }

    fn backend(&self) -> &'static str {
        "hvf"
    }
}

fn unmap_or_abort(gpa: u64, len: u64) {
    // SAFETY: callers hold the registry lock and name an exact live mapping;
    // every inserted length was checked to fit usize.
    let status = unsafe { hv_vm_unmap(gpa, len as usize) };
    if status != 0 {
        // SharedWindow's caller can free backing after unmap. Returning from
        // an OS failure would leave a live guest mapping to freed host pages.
        // Stop this VM process before that memory can be reused.
        tracing::error!(
            status,
            gpa,
            len,
            "HVF failed to retire live shared memory; terminating VM process"
        );
        std::process::abort();
    }
}

struct Inner {
    memory: ManuallyDrop<GuestMem>,
    page_size: u64,
    mappings: Mutex<BTreeMap<u64, Mapping>>,
    next_owner: Mutex<u64>,
}

#[derive(Clone, Copy)]
struct Mapping {
    len: u64,
    owner: u64,
}

fn mapping_allowed(
    mappings: &BTreeMap<u64, Mapping>,
    base: u64,
    len: u64,
    owner: u64,
) -> Result<(), HvfError> {
    let end = base
        .checked_add(len)
        .filter(|_| len != 0)
        .ok_or_else(|| HvfError::Memory("mapping range is empty or overflows".into()))?;
    for (&other, mapping) in mappings {
        if other == base && mapping.owner == owner && owner != 0 {
            continue;
        }
        let other_end = other
            .checked_add(mapping.len)
            .ok_or_else(|| HvfError::Memory("existing mapping overflows".into()))?;
        if base < other_end && other < end {
            return Err(HvfError::Memory(
                "mapping overlaps RAM, GIC or another shared window".into(),
            ));
        }
    }
    Ok(())
}

impl Drop for Inner {
    fn drop(&mut self) {
        // SAFETY: the process VM is ours; Arc ensures every owning vCPU has
        // already been destroyed. Destroy removes mappings before RAM is freed.
        let status = unsafe { hv_vm_destroy() };
        if status == 0 {
            // SAFETY: RAM is no longer mapped and is dropped exactly once.
            unsafe { ManuallyDrop::drop(&mut self.memory) };
            VM_EXISTS.store(false, Ordering::Release);
        } else {
            // Retain RAM if the hypervisor might still reference it.
            tracing::error!(status, "hv_vm_destroy failed; retaining mapped RAM");
        }
    }
}

#[derive(Clone)]
pub struct HvfVm(Arc<Inner>);

impl HvfVm {
    /// Hypervisor's actual limit, queried before allocating guest resources.
    pub fn max_vcpu_count() -> Result<u32, HvfError> {
        let mut count = 0;
        check(
            // SAFETY: the SDK writes one u32 to this live output local.
            unsafe { hv_vm_get_max_vcpu_count(&mut count) },
            "hv_vm_get_max_vcpu_count",
        )?;
        Ok(count)
    }
    /// Maps existing RAM. Every region must meet Apple's 16 KiB alignment.
    pub fn new(memory: GuestMem) -> Result<Self, HvfError> {
        Self::with_options(memory, HvfOptions::default())
    }

    pub fn with_options(memory: GuestMem, options: HvfOptions) -> Result<Self, HvfError> {
        for region in memory.iter() {
            let len = usize::try_from(region.len()).map_err(|e| HvfError::Memory(e.to_string()))?;
            let pointer = region
                .get_host_address(vm_memory::MemoryRegionAddress(0))
                .map_err(|e| HvfError::Memory(e.to_string()))?;
            if len == 0
                || len % PAGE_SIZE != 0
                || region.start_addr().raw_value() % PAGE_SIZE as u64 != 0
                || pointer as usize % PAGE_SIZE != 0
            {
                return Err(HvfError::Memory(
                    "regions must be nonempty and 16 KiB aligned".into(),
                ));
            }
        }
        if memory.num_regions() == 0 {
            return Err(HvfError::Memory("no regions".into()));
        }
        let config = if options.small_page_mappings {
            Some(VmConfig::small_pages()?)
        } else {
            None
        };
        VM_EXISTS
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| HvfError::VmExists)?;
        // SAFETY: null requests Apple's default configuration; process ownership
        // is reserved above and no existing VM can be destroyed by this call.
        let config_pointer = config
            .as_ref()
            .map_or(std::ptr::null(), |c| c.0.as_ptr().cast_const());
        if let Err(error) = check(
            // SAFETY: config_pointer is null or retains a live owned configuration.
            unsafe { hv_vm_create(config_pointer) },
            "hv_vm_create",
        ) {
            VM_EXISTS.store(false, Ordering::Release);
            return Err(error);
        }
        let vm = Self(Arc::new(Inner {
            memory: ManuallyDrop::new(memory),
            page_size: if options.small_page_mappings {
                4096
            } else {
                PAGE_SIZE as u64
            },
            mappings: Mutex::new(BTreeMap::new()),
            next_owner: Mutex::new(1),
        }));
        for region in vm.memory().iter() {
            let pointer = region
                .get_host_address(vm_memory::MemoryRegionAddress(0))
                .map_err(|e| HvfError::Memory(e.to_string()))?;
            let len = usize::try_from(region.len()).map_err(|e| HvfError::Memory(e.to_string()))?;
            check(
                // SAFETY: validated page-aligned RAM is retained in Inner until
                // hv_vm_destroy removes every mapping; flags are READ|WRITE|EXEC.
                unsafe { hv_vm_map(pointer.cast(), region.start_addr().raw_value(), len, 7) },
                "hv_vm_map",
            )?;
            vm.0.mappings
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    region.start_addr().raw_value(),
                    Mapping {
                        len: region.len(),
                        owner: 0,
                    },
                );
        }
        Ok(vm)
    }

    pub fn memory(&self) -> &GuestMem {
        &self.0.memory
    }

    pub fn create_shm_window(
        &self,
        len: u64,
        host_mapped: bool,
    ) -> Result<Arc<crate::shm::SharedWindow>, crate::VmmError> {
        if len == 0 || len % self.0.page_size != 0 {
            return Err(crate::VmmError::GuestMemory(
                "window must align to the VM mapping granule".into(),
            ));
        }
        let mut next = self.0.next_owner.lock().unwrap_or_else(|e| e.into_inner());
        let owner = *next;
        *next = owner.checked_add(1).ok_or_else(|| {
            crate::VmmError::GuestMemory("shared-window ID space exhausted".into())
        })?;
        drop(next);
        let mapper = Arc::new(HvfGpaMapper {
            vm: self.clone(),
            owner,
        });
        let window = if host_mapped {
            crate::shm::SharedWindow::new_host_mapped(len, mapper)?
        } else {
            crate::shm::SharedWindow::new(len, mapper)?
        };
        Ok(Arc::new(window))
    }

    pub fn create_vcpu(&self) -> Result<HvfVcpu, HvfError> {
        let mut cpu = 0;
        let mut exit = std::ptr::null_mut();
        check(
            // SAFETY: output locals are writable and null uses the default config.
            unsafe { hv_vcpu_create(&mut cpu, &mut exit, std::ptr::null()) },
            "hv_vcpu_create",
        )?;
        let Some(exit) = NonNull::new(exit) else {
            // SAFETY: cpu was created on this thread immediately above.
            if unsafe { hv_vcpu_destroy(cpu) } != 0 {
                std::mem::forget(Arc::clone(&self.0));
            }
            return Err(HvfError::NullExit);
        };
        Ok(HvfVcpu {
            cpu,
            exit,
            vm: Arc::clone(&self.0),
            canceller: HvfCanceller(Arc::new(Mutex::new(Some(cpu)))),
            owning_thread: PhantomData,
        })
    }
}

pub struct HvfVcpu {
    cpu: u64,
    exit: NonNull<RawExit>,
    vm: Arc<Inner>,
    canceller: HvfCanceller,
    owning_thread: PhantomData<Rc<()>>,
}

/// Cross-thread cancellation. The lock retires IDs before destruction, so a
/// late kick cannot cancel a different vCPU that reused an old ID.
#[derive(Clone)]
pub struct HvfCanceller(Arc<Mutex<Option<u64>>>);

impl HvfCanceller {
    pub fn cancel(&self) -> Result<(), HvfError> {
        let guard = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        if let Some(mut cpu) = *guard {
            // SAFETY: the locked ID remains live until this call returns;
            // Apple permits hv_vcpus_exit from a different thread.
            check(unsafe { hv_vcpus_exit(&mut cpu, 1) }, "hv_vcpus_exit")?;
        }
        Ok(())
    }
}

impl crate::lifecycle::VcpuKick for HvfCanceller {
    fn kick(&self) {
        if let Err(error) = self.cancel() {
            tracing::error!(%error, "cannot kick ARM64 vCPU");
        }
    }
}

impl HvfVcpu {
    /// Allow the guest to use the framework's virtual debug registers.
    pub fn allow_guest_debug_registers(&mut self) -> Result<(), HvfError> {
        check(
            // SAFETY: this vCPU remains live on its owning thread.
            unsafe { hv_vcpu_set_trap_debug_reg_accesses(self.cpu, false) },
            "hv_vcpu_set_trap_debug_reg_accesses",
        )
    }
    pub fn set_system_register(
        &mut self,
        register: crate::hv::arm64::SystemRegister,
        value: u64,
    ) -> Result<(), HvfError> {
        use crate::hv::arm64::SystemRegister;
        let reg = match register {
            SystemRegister::Mpidr => 0xc005,
            SystemRegister::Sctlr => 0xc080,
            SystemRegister::Cpacr => 0xc082,
            SystemRegister::SpEl1 => 0xe208,
            SystemRegister::Ttbr0 => 0xc100,
            SystemRegister::Ttbr1 => 0xc101,
            SystemRegister::Tcr => 0xc102,
            SystemRegister::Mair => 0xc510,
            SystemRegister::Vbar => 0xc600,
            SystemRegister::ContextId => 0xc681,
            SystemRegister::VirtualTimerControl => 0xdf19,
        };
        check(
            // SAFETY: the CPU is on its owning thread and reg is a named SDK system register.
            unsafe { hv_vcpu_set_sys_reg(self.cpu, reg, value) },
            "hv_vcpu_set_sys_reg",
        )
    }
    pub fn canceller(&self) -> HvfCanceller {
        self.canceller.clone()
    }

    pub fn set_register(&mut self, register: Register, value: u64) -> Result<(), HvfError> {
        let reg = register_number(register)?;
        check(
            // SAFETY: the !Send marker keeps cpu on its owning thread; reg is valid.
            unsafe { hv_vcpu_set_reg(self.cpu, reg, value) },
            "hv_vcpu_set_reg",
        )
    }

    pub fn register(&self, register: Register) -> Result<u64, HvfError> {
        let reg = register_number(register)?;
        let mut value = 0;
        check(
            // SAFETY: cpu belongs to this thread and value is a writable output local.
            unsafe { hv_vcpu_get_reg(self.cpu, reg, &mut value) },
            "hv_vcpu_get_reg",
        )?;
        Ok(value)
    }

    pub fn run(&mut self) -> Result<Arm64Exit, HvfError> {
        // SAFETY: cpu belongs to this thread and retained RAM backs its mappings.
        check(unsafe { hv_vcpu_run(self.cpu) }, "hv_vcpu_run")?;
        // SAFETY: Apple's exit pointer stays live until cpu destruction; the
        // owning thread has completed run, so the result is no longer changing.
        let exit = unsafe { self.exit.as_ref() };
        Ok(match exit.reason {
            0 => Arm64Exit::Canceled,
            1 => Arm64Exit::Exception {
                syndrome: exit.exception.syndrome,
                virtual_address: exit.exception.virtual_address,
                physical_address: exit.exception.physical_address,
            },
            2 => Arm64Exit::VirtualTimer,
            reason => Arm64Exit::Unknown(reason),
        })
    }
}

impl crate::hv::arm64::VcpuRegisters for HvfVcpu {
    fn register(&self, register: Register) -> Result<u64, crate::hv::HvError> {
        HvfVcpu::register(self, register)
            .map_err(|error| crate::hv::HvError::Registers(error.to_string()))
    }

    fn set_register(&mut self, register: Register, value: u64) -> Result<(), crate::hv::HvError> {
        HvfVcpu::set_register(self, register, value)
            .map_err(|error| crate::hv::HvError::Registers(error.to_string()))
    }
}

impl Drop for HvfVcpu {
    fn drop(&mut self) {
        let mut guard = self
            .canceller
            .0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        *guard = None;
        // SAFETY: cpu is destroyed once, on its owning thread, outside run.
        let status = unsafe { hv_vcpu_destroy(self.cpu) };
        if status != 0 {
            tracing::error!(status, "hv_vcpu_destroy failed; retaining VM and RAM");
            std::mem::forget(Arc::clone(&self.vm));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vm_memory::GuestAddress;

    #[test]
    fn shared_mappings_cannot_replace_protected_or_foreign_ranges() {
        let mappings = BTreeMap::from([
            (
                0x4000_0000,
                Mapping {
                    len: 16384,
                    owner: 0,
                },
            ),
            (
                0x1_0000_0000,
                Mapping {
                    len: 4096,
                    owner: 1,
                },
            ),
            (
                0x1_0000_1000,
                Mapping {
                    len: 4096,
                    owner: 2,
                },
            ),
        ]);
        assert!(mapping_allowed(&mappings, 0x4000_0000, 16384, 1).is_err());
        assert!(mapping_allowed(&mappings, 0x4000_1000, 4096, 1).is_err());
        assert!(mapping_allowed(&mappings, 0x1_0000_0000, 4096, 2).is_err());
        assert!(mapping_allowed(&mappings, 0x1_0000_0000, 8192, 1).is_err());
        assert!(mapping_allowed(&mappings, 0x1_0000_0000, 4096, 1).is_ok());
        assert!(mapping_allowed(&mappings, u64::MAX - 1, 4096, 1).is_err());
        assert!(mapping_allowed(&mappings, 0x8000_0000, 0, 1).is_err());
    }

    #[test]
    fn invalid_registers_are_rejected_before_ffi() {
        assert_eq!(register_number(Register::X(30)).unwrap(), 30);
        assert!(matches!(
            register_number(Register::X(31)),
            Err(HvfError::Register(31))
        ));
        assert!(matches!(
            register_number(Register::X(255)),
            Err(HvfError::Register(255))
        ));
    }

    #[test]
    fn short_ram_is_rejected_before_vm_creation() {
        let memory = GuestMem::from_ranges(&[(GuestAddress(0x4000_0000), 4096)]).unwrap();
        assert!(matches!(HvfVm::new(memory), Err(HvfError::Memory(_))));
    }

    #[test]
    fn misaligned_guest_base_is_rejected_before_vm_creation() {
        let memory = GuestMem::from_ranges(&[(GuestAddress(0x4000_1000), PAGE_SIZE)]).unwrap();
        assert!(matches!(HvfVm::new(memory), Err(HvfError::Memory(_))));
    }
}
