//! HVF state is captured on the owning CPU thread, between completed exits.
use super::serror::SerrorApi;
use super::*;
use crate::hv::arm64::CpuState;

#[link(name = "Hypervisor", kind = "framework")]
#[allow(improper_ctypes)] // Apple's ext_vector_type(16) has the native NEON vector ABI.
extern "C" {
    fn hv_vcpu_get_sys_reg(cpu: u64, reg: u16, value: *mut u64) -> i32;
    fn entangled_hvf_get_simd(cpu: u64, reg: u32, value: *mut u8) -> i32;
    fn entangled_hvf_set_simd(cpu: u64, reg: u32, value: *const u8) -> i32;
    fn hv_vcpu_get_vtimer_offset(cpu: u64, value: *mut u64) -> i32;
    fn hv_vcpu_set_vtimer_offset(cpu: u64, value: u64) -> i32;
    fn hv_vcpu_get_vtimer_mask(cpu: u64, value: *mut bool) -> i32;
    fn hv_vcpu_set_vtimer_mask(cpu: u64, value: bool) -> i32;
    fn hv_gic_get_icc_reg(cpu: u64, reg: u16, value: *mut u64) -> i32;
    fn hv_gic_set_icc_reg(cpu: u64, reg: u16, value: u64) -> i32;
    fn hv_gic_state_create() -> *mut c_void;
    fn hv_gic_state_get_size(state: *mut c_void, size: *mut usize) -> i32;
    fn hv_gic_state_get_data(state: *mut c_void, data: *mut c_void) -> i32;
    fn hv_gic_set_state(data: *const c_void, size: usize) -> i32;
}

// Writable EL1/EL0 architectural state for the non-nested, non-SME VM we create.
const SYS: &[u16] = &[
    0x8010, 0x8012, 0xc005, 0xc080, 0xc081, 0xc082, 0xc100, 0xc101, 0xc102, 0xc108, 0xc109, 0xc10a,
    0xc10b, 0xc110, 0xc111, 0xc112, 0xc113, 0xc118, 0xc119, 0xc200, 0xc201, 0xc208, 0xc288, 0xc289,
    0xc290, 0xc300, 0xc3a0, 0xc510, 0xc518, 0xc600, 0xc681, 0xc684, 0xc708, 0xd000, 0xde82, 0xde83,
    0xdf11, 0xdf12, 0xdf19, 0xdf1a, 0xe208,
];
const ICC: &[u16] = &[
    0xc230, 0xc643, 0xc644, 0xc648, 0xc663, 0xc664, 0xc665, 0xc666, 0xc667,
];
const FEATURES: &[u16] = &[
    0xc000, 0xc020, 0xc021, 0xc028, 0xc029, 0xc030, 0xc031, 0xc038, 0xc039, 0xc03a,
];

impl HvfVcpu {
    fn sys(&self, reg: u16) -> Result<u64, HvfError> {
        let mut value = 0;
        check(
            // SAFETY: this vCPU is live on its owning thread and value is writable.
            unsafe { hv_vcpu_get_sys_reg(self.cpu, reg, &mut value) },
            "hv_vcpu_get_sys_reg(snapshot)",
        )?;
        Ok(value)
    }
    pub fn save_state(&self) -> Result<CpuState, HvfError> {
        let serror = SerrorApi::load()?;
        let features = FEATURES
            .iter()
            .map(|&r| self.sys(r))
            .collect::<Result<Vec<_>, _>>()?;
        if features[1] & (0xf << 32) != 0 || features[2] & (0xf << 24) != 0 {
            return Err(HvfError::Memory(
                "SVE/SME CPU state is not supported by this snapshot version".into(),
            ));
        }
        let mut state = CpuState {
            registers: [0; 35],
            simd: [[0; 16]; 32],
            system: Vec::new(),
            icc: Vec::new(),
            features,
            timer_offset: 0,
            timer_mask: false,
            serror: false,
        };
        for (reg, value) in state.registers.iter_mut().enumerate() {
            check(
                // SAFETY: bounded SDK general register enum, live owning CPU, writable output.
                unsafe { hv_vcpu_get_reg(self.cpu, reg as u32, value) },
                "hv_vcpu_get_reg(snapshot)",
            )?;
        }
        for (reg, value) in state.simd.iter_mut().enumerate() {
            check(
                // SAFETY: Q0..Q31 and a writable sixteen-byte output; the C shim
                // owns the native vector ABI and copies into this byte array.
                unsafe { entangled_hvf_get_simd(self.cpu, reg as u32, value.as_mut_ptr()) },
                "hv_vcpu_get_simd_fp_reg",
            )?;
        }
        for &reg in SYS {
            state.system.push((reg, self.sys(reg)?));
        }
        // These registers were added to the API in macOS 15.2. Older hosts
        // cannot expose them to the guest; never discard an unexpected error.
        for reg in [0xc687, 0xde87, 0xde85] {
            match self.sys(reg) {
                Ok(value) => state.system.push((reg, value)),
                Err(HvfError::Api { status, .. })
                    if matches!(status as u32, 0xfae94003 | 0xfae9400f) => {}
                Err(error) => return Err(error),
            }
        }
        // Debug resources are described by the architectural feature register.
        for (count, offset) in [
            (((state.features[3] >> 12) & 15) + 1, 4),
            (((state.features[3] >> 20) & 15) + 1, 6),
        ] {
            for index in 0..count {
                for reg in [
                    0x8000 + index as u16 * 8 + offset,
                    0x8001 + index as u16 * 8 + offset,
                ] {
                    state.system.push((reg, self.sys(reg)?));
                }
            }
        }
        for &reg in ICC {
            let mut value = 0;
            check(
                // SAFETY: SDK ICC register, live CPU and writable scalar output.
                unsafe { hv_gic_get_icc_reg(self.cpu, reg, &mut value) },
                "hv_gic_get_icc_reg",
            )?;
            state.icc.push((reg, value));
        }
        check(
            // SAFETY: live owning CPU, outputs have exactly the SDK types.
            unsafe { hv_vcpu_get_vtimer_offset(self.cpu, &mut state.timer_offset) },
            "hv_vcpu_get_vtimer_offset",
        )?;
        check(
            // SAFETY: live owning CPU and writable bool output.
            unsafe { hv_vcpu_get_vtimer_mask(self.cpu, &mut state.timer_mask) },
            "hv_vcpu_get_vtimer_mask",
        )?;
        check(
            // SAFETY: live owning CPU and writable bool output.
            unsafe { (serror.get)(self.cpu, &mut state.serror) },
            "hv_vcpu_get_serror",
        )?;
        Ok(state)
    }
    pub fn load_state(&mut self, state: &CpuState) -> Result<(), HvfError> {
        let serror = SerrorApi::load()?;
        let template = self.save_state()?;
        if template.features != state.features
            || template
                .system
                .iter()
                .map(|r| r.0)
                .ne(state.system.iter().map(|r| r.0))
            || template
                .icc
                .iter()
                .map(|r| r.0)
                .ne(state.icc.iter().map(|r| r.0))
        {
            return Err(HvfError::Memory(
                "saved ARM64 CPU features/register inventory differs from this host".into(),
            ));
        }
        for (reg, &value) in state.registers.iter().enumerate() {
            check(
                // SAFETY: bounded SDK general register enum on the owning thread.
                unsafe { hv_vcpu_set_reg(self.cpu, reg as u32, value) },
                "hv_vcpu_set_reg(restore)",
            )?;
        }
        for (reg, value) in state.simd.iter().enumerate() {
            check(
                // SAFETY: bounded SIMD register and a readable sixteen-byte array.
                unsafe { entangled_hvf_set_simd(self.cpu, reg as u32, value.as_ptr()) },
                "hv_vcpu_set_simd_fp_reg",
            )?;
        }
        for &(reg, value) in &state.system {
            check(
                // SAFETY: inventory was matched against the supported native register list above.
                unsafe { hv_vcpu_set_sys_reg(self.cpu, reg, value) },
                "hv_vcpu_set_sys_reg(restore)",
            )?;
        }
        for &(reg, value) in &state.icc {
            check(
                // SAFETY: inventory was matched against the supported ICC register list above.
                unsafe { hv_gic_set_icc_reg(self.cpu, reg, value) },
                "hv_gic_set_icc_reg",
            )?;
        }
        check(
            // Keep guest virtual time at the same counter; disk sleep does not advance it.
            // SAFETY: live owning CPU; the offset uses the same mach_absolute_time ticks.
            unsafe { hv_vcpu_set_vtimer_offset(self.cpu, state.timer_offset) },
            "hv_vcpu_set_vtimer_offset",
        )?;
        check(
            // SAFETY: live owning CPU and a valid boolean mask.
            unsafe { hv_vcpu_set_vtimer_mask(self.cpu, state.timer_mask) },
            "hv_vcpu_set_vtimer_mask",
        )?;
        check(
            // SAFETY: live owning CPU and a valid boolean pending state.
            unsafe { (serror.set)(self.cpu, state.serror) },
            "hv_vcpu_set_serror",
        )
    }
}

impl HvfGic {
    /// Caller parks all vCPUs and quiesces every interrupt producer first.
    pub fn save_state(&self) -> Result<Vec<u8>, HvfError> {
        // SAFETY: retained VM owns this GIC; caller guarantees the stopped state.
        let object = NonNull::new(unsafe { hv_gic_state_create() })
            .ok_or_else(|| HvfError::Memory("GIC cannot be represented as saved state".into()))?;
        let result = (|| {
            let mut size = 0;
            check(
                // SAFETY: object is live and size is a writable SDK-sized output.
                unsafe { hv_gic_state_get_size(object.as_ptr(), &mut size) },
                "hv_gic_state_get_size",
            )?;
            if size == 0 || size > 16 << 20 {
                return Err(HvfError::Memory("invalid GIC state size".into()));
            }
            let mut bytes = vec![0; size];
            check(
                // SAFETY: buffer is exactly the size returned for this retained state.
                unsafe { hv_gic_state_get_data(object.as_ptr(), bytes.as_mut_ptr().cast()) },
                "hv_gic_state_get_data",
            )?;
            Ok(bytes)
        })();
        // SAFETY: releases exactly the owned reference returned by create.
        unsafe { os_release(object.as_ptr()) };
        result
    }
    /// All CPUs must exist, with their affinities restored, and none may have run.
    pub fn load_state(&self, bytes: &[u8]) -> Result<(), HvfError> {
        check(
            // SAFETY: slice remains live for the synchronous call; native API validates opaque bytes.
            unsafe { hv_gic_set_state(bytes.as_ptr().cast(), bytes.len()) },
            "hv_gic_set_state",
        )
    }
}

extern "C" {
    fn mach_absolute_time() -> u64;
}
impl HvfVm {
    /// The counter backing guest CNTVCT and virtual timer offsets.
    pub fn counter() -> u64 {
        // SAFETY: Darwin's monotonic counter has no arguments or pointer contracts.
        unsafe { mach_absolute_time() }
    }
}
