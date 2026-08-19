//! WHP's instruction emulator (`winhvemulation.dll`), the WHP answer to the
//! decoding KVM does for us (backlog WHP-1703).
//!
//! # Why an emulator is unavoidable
//!
//! KVM's `KVM_EXIT_MMIO` arrives fully decoded: address, direction, width, and
//! the data for a write. WHP's `WHvRunVpExitReasonMemoryAccess` carries the guest
//! physical address, the access type and up to 16 raw instruction bytes — and
//! nothing else. There is no width, no data, no register mapping, and RIP is not
//! advanced. Turning that into an [`ExitHandler`] call means decoding an x86
//! instruction, which is why WHP ships a decoder and why every WHP-based VMM uses
//! it.
//!
//! `WHvEmulatorTryIoEmulation` is the same machinery for string and `REP` port
//! I/O (`INS`/`OUTS`), which the plain port-I/O path in
//! [`crate::whp::WhpVcpu::run_loop`] cannot do for the same reason.
//!
//! # The callback table
//!
//! `WHvEmulatorCreateEmulator` takes five callbacks, called synchronously on the
//! calling thread for the duration of one `TryMmioEmulation`/`TryIoEmulation`:
//!
//! | Callback | What we do |
//! |---|---|
//! | `WHvEmulatorMemoryCallback` | `ExitHandler::mmio_read` / `mmio_write` — the point of the exercise |
//! | `WHvEmulatorIoPortCallback` | `ExitHandler::io_in` / `io_out`, for the string-I/O path |
//! | `WHvEmulatorGetVirtualProcessorRegisters` | `WHvGetVirtualProcessorRegisters` on this vCPU |
//! | `WHvEmulatorSetVirtualProcessorRegisters` | `WHvSetVirtualProcessorRegisters` — this is also how **RIP gets advanced**, by the emulator, not by us |
//! | `WHvEmulatorTranslateGvaPage` | `WHvTranslateGva`, for an operand the emulator has to walk the guest page tables for |
//!
//! The context pointer WHP hands back is an [`EmulatorContext`], borrowed for the
//! duration of the call. Because WHP calls back only on this thread and only
//! inside the one call, the `&mut` inside it is never aliased.
//!
//! # Two things the register callbacks must get right
//!
//! * **Alignment.** The value buffer WHP hands the register callbacks is *its*
//!   buffer, and `WHvGet`/`WHvSetVirtualProcessorRegisters` require 16-byte
//!   alignment (see [`crate::whp::regs::Aligned16`] for what happens otherwise).
//!   Rather than trust the emulator's stack layout, both callbacks stage through
//!   an over-aligned buffer of their own.
//! * **No unwinding.** These are `extern "system"`, so a panic crossing them
//!   aborts the process. Every path returns an `HRESULT` instead: a failing
//!   handler surfaces as a failed emulation, which the run loop reports as a
//!   typed error naming the GPA.

use core::ffi::c_void;

use windows::core::HRESULT;
use windows::Win32::Foundation::{E_FAIL, S_OK};
use windows::Win32::System::Hypervisor::{
    WHvEmulatorCreateEmulator, WHvEmulatorDestroyEmulator, WHvEmulatorTryIoEmulation,
    WHvEmulatorTryMmioEmulation, WHvGetVirtualProcessorRegisters, WHvSetVirtualProcessorRegisters,
    WHvTranslateGva, WHvTranslateGvaFlagValidateRead, WHvTranslateGvaFlagValidateWrite,
    WHvTranslateGvaResultSuccess, WHV_EMULATOR_CALLBACKS, WHV_EMULATOR_IO_ACCESS_INFO,
    WHV_EMULATOR_MEMORY_ACCESS_INFO, WHV_EMULATOR_STATUS, WHV_MEMORY_ACCESS_CONTEXT,
    WHV_REGISTER_NAME, WHV_REGISTER_VALUE, WHV_TRANSLATE_GVA_FLAGS, WHV_TRANSLATE_GVA_RESULT,
    WHV_TRANSLATE_GVA_RESULT_CODE, WHV_VP_EXIT_CONTEXT, WHV_X64_IO_PORT_ACCESS_CONTEXT,
};

use crate::hv::ExitHandler;
use crate::whp::partition::whp_err;
use crate::whp::regs::{zeroed_value, Aligned16};
use crate::VmmError;

/// Direction values shared by both emulator access-info structs: 0 is a guest
/// read (we produce the data), 1 is a guest write (we consume it).
const DIRECTION_READ: u8 = 0;

/// Widest access either callback can carry (`WHV_EMULATOR_MEMORY_ACCESS_INFO::Data`).
const MAX_ACCESS_SIZE: usize = 8;

/// Register batches the emulator asks for are small (an instruction's operands
/// plus RIP/RFLAGS); this is the staging buffer size before falling back to the
/// heap.
const REGISTER_SCRATCH: usize = 32;

/// Everything the callbacks need, passed to WHP as an opaque context pointer.
///
/// Not `Send`/`Sync` and deliberately borrow-scoped: it exists only for the
/// duration of one `TryMmioEmulation`/`TryIoEmulation` call.
struct EmulatorContext<'a> {
    partition: windows::Win32::System::Hypervisor::WHV_PARTITION_HANDLE,
    vp_index: u32,
    handler: &'a mut dyn ExitHandler,
}

/// Reconstructs the context WHP is handing back.
///
/// # Safety
///
/// `context` must be the pointer passed to `WHvEmulatorTry*Emulation`, i.e. a
/// live `&mut EmulatorContext` that outlives the call, and the caller must be
/// inside that call (so no other reference to it is live).
unsafe fn context<'a>(context: *const c_void) -> Option<&'a mut EmulatorContext<'a>> {
    if context.is_null() {
        return None;
    }
    // SAFETY: guaranteed by this function's contract.
    Some(unsafe { &mut *(context as *mut EmulatorContext<'a>) })
}

unsafe extern "system" fn memory_callback(
    ctx: *const c_void,
    access: *mut WHV_EMULATOR_MEMORY_ACCESS_INFO,
) -> HRESULT {
    // SAFETY: WHP calls this only from inside `TryMmioEmulation`, with the
    // context we passed and a live, writable access-info record.
    let (Some(ctx), Some(access)) = (unsafe { context(ctx) }, unsafe { access.as_mut() }) else {
        return E_FAIL;
    };
    let size = usize::from(access.AccessSize);
    if size == 0 || size > MAX_ACCESS_SIZE {
        return E_FAIL;
    }
    if access.Direction == DIRECTION_READ {
        ctx.handler
            .mmio_read(access.GpaAddress, &mut access.Data[..size]);
    } else {
        ctx.handler
            .mmio_write(access.GpaAddress, &access.Data[..size]);
    }
    S_OK
}

unsafe extern "system" fn io_port_callback(
    ctx: *const c_void,
    access: *mut WHV_EMULATOR_IO_ACCESS_INFO,
) -> HRESULT {
    // SAFETY: as `memory_callback`, for `TryIoEmulation`.
    let (Some(ctx), Some(access)) = (unsafe { context(ctx) }, unsafe { access.as_mut() }) else {
        return E_FAIL;
    };
    let size = usize::from(access.AccessSize);
    if size == 0 || size > 4 {
        return E_FAIL;
    }
    if access.Direction == DIRECTION_READ {
        let mut data = [0u8; 4];
        ctx.handler.io_in(access.Port, &mut data[..size]);
        access.Data = u32::from_le_bytes(data);
    } else {
        let data = access.Data.to_le_bytes();
        ctx.handler.io_out(access.Port, &data[..size]);
    }
    S_OK
}

/// Copies `count` register values through a 16-byte-aligned buffer, because the
/// emulator's own buffer has no such guarantee and WHP faults inside the call on
/// a misaligned one.
fn with_aligned<R>(
    count: usize,
    fill: impl FnOnce(&mut [WHV_REGISTER_VALUE]) -> R,
) -> Option<(R, Vec<WHV_REGISTER_VALUE>)> {
    if count > REGISTER_SCRATCH {
        // `Vec<Aligned16<_>>` is allocated at `align_of::<Aligned16<_>>() == 16`,
        // and each element is exactly one `WHV_REGISTER_VALUE`, so the backing
        // store has the same layout as `[WHV_REGISTER_VALUE; count]` — just
        // over-aligned. Kept as the unbounded fallback; no real instruction needs
        // more than a handful of registers.
        let mut heap: Vec<Aligned16<WHV_REGISTER_VALUE>> =
            (0..count).map(|_| Aligned16(zeroed_value())).collect();
        // SAFETY: `Aligned16<T>` is `repr(C, align(16))` around a single `T` of
        // size 16, so a `[Aligned16<T>]` and a `[T]` of the same length have
        // identical layout; the slice is live for the borrow.
        let values: &mut [WHV_REGISTER_VALUE] = unsafe {
            core::slice::from_raw_parts_mut(heap.as_mut_ptr().cast::<WHV_REGISTER_VALUE>(), count)
        };
        let out = fill(values);
        return Some((out, values.to_vec()));
    }
    let mut scratch = Aligned16([zeroed_value(); REGISTER_SCRATCH]);
    let out = fill(&mut scratch.0[..count]);
    Some((out, scratch.0[..count].to_vec()))
}

unsafe extern "system" fn get_registers_callback(
    ctx: *const c_void,
    names: *const WHV_REGISTER_NAME,
    count: u32,
    values: *mut WHV_REGISTER_VALUE,
) -> HRESULT {
    // SAFETY: as `memory_callback`; `names` and `values` are WHP's arrays of
    // exactly `count` elements, live for the duration of the call.
    let Some(ctx) = (unsafe { context(ctx) }) else {
        return E_FAIL;
    };
    if values.is_null() || names.is_null() {
        return E_FAIL;
    }
    let Ok(len) = usize::try_from(count) else {
        return E_FAIL;
    };
    let result = with_aligned(len, |staged| {
        // SAFETY: `staged` is `len`-long and 16-byte aligned, `names` is WHP's
        // array of `count` names, and this VP is the one being emulated.
        unsafe {
            WHvGetVirtualProcessorRegisters(
                ctx.partition,
                ctx.vp_index,
                names,
                count,
                staged.as_mut_ptr(),
            )
        }
    });
    let Some((call, staged)) = result else {
        return E_FAIL;
    };
    if call.is_err() {
        return E_FAIL;
    }
    // SAFETY: `values` points at WHP's array of `count` elements, which it asked
    // us to fill; `staged` holds exactly that many.
    unsafe { core::ptr::copy_nonoverlapping(staged.as_ptr(), values, len) };
    S_OK
}

unsafe extern "system" fn set_registers_callback(
    ctx: *const c_void,
    names: *const WHV_REGISTER_NAME,
    count: u32,
    values: *const WHV_REGISTER_VALUE,
) -> HRESULT {
    // SAFETY: as `get_registers_callback`, with `values` read-only.
    let Some(ctx) = (unsafe { context(ctx) }) else {
        return E_FAIL;
    };
    if values.is_null() || names.is_null() {
        return E_FAIL;
    }
    let Ok(len) = usize::try_from(count) else {
        return E_FAIL;
    };
    let result = with_aligned(len, |staged| {
        // SAFETY: `values` is WHP's array of `count` initialised elements and
        // `staged` has the same length.
        unsafe { core::ptr::copy_nonoverlapping(values, staged.as_mut_ptr(), len) };
        // SAFETY: `staged` is `len`-long, 16-byte aligned and fully initialised;
        // WHP only reads it.
        unsafe {
            WHvSetVirtualProcessorRegisters(
                ctx.partition,
                ctx.vp_index,
                names,
                count,
                staged.as_ptr(),
            )
        }
    });
    match result {
        Some((Ok(()), _)) => S_OK,
        _ => E_FAIL,
    }
}

unsafe extern "system" fn translate_gva_callback(
    ctx: *const c_void,
    gva: u64,
    flags: WHV_TRANSLATE_GVA_FLAGS,
    result: *mut WHV_TRANSLATE_GVA_RESULT_CODE,
    gpa: *mut u64,
) -> HRESULT {
    // SAFETY: as `memory_callback`; `result` and `gpa` are live out-parameters.
    let Some(ctx) = (unsafe { context(ctx) }) else {
        return E_FAIL;
    };
    if result.is_null() || gpa.is_null() {
        return E_FAIL;
    }
    let mut translation = WHV_TRANSLATE_GVA_RESULT::default();
    let mut address = 0u64;
    // WHP wants the access-validation flags the emulator asked for, but the
    // emulator passes them through unchanged, so nothing is added here.
    let _ = (
        WHvTranslateGvaFlagValidateRead,
        WHvTranslateGvaFlagValidateWrite,
    );
    // SAFETY: the partition is the one this VP belongs to and both
    // out-parameters are live locals.
    let call = unsafe {
        WHvTranslateGva(
            ctx.partition,
            ctx.vp_index,
            gva,
            flags,
            &raw mut translation,
            &raw mut address,
        )
    };
    if call.is_err() {
        return E_FAIL;
    }
    // SAFETY: both pointers are live out-parameters WHP asked us to fill.
    unsafe {
        *result = translation.ResultCode;
        *gpa = if translation.ResultCode == WHvTranslateGvaResultSuccess {
            address
        } else {
            0
        };
    }
    S_OK
}

const CALLBACKS: WHV_EMULATOR_CALLBACKS = WHV_EMULATOR_CALLBACKS {
    Size: size_of::<WHV_EMULATOR_CALLBACKS>() as u32,
    Reserved: 0,
    WHvEmulatorIoPortCallback: Some(io_port_callback),
    WHvEmulatorMemoryCallback: Some(memory_callback),
    WHvEmulatorGetVirtualProcessorRegisters: Some(get_registers_callback),
    WHvEmulatorSetVirtualProcessorRegisters: Some(set_registers_callback),
    WHvEmulatorTranslateGvaPage: Some(translate_gva_callback),
};

/// An owned `winhvemulation.dll` emulator handle.
///
/// Created lazily, on the first exit that needs decoding, so a guest that only
/// does port I/O (the phase-1 smoke guests) never loads the DLL.
pub(super) struct Emulator {
    handle: *mut c_void,
}

// SAFETY: the handle is an opaque pointer to emulator state that WHP documents
// as usable from the thread that drives it; `Emulator` is owned by exactly one
// `WhpVcpu`'s run loop, so it is only ever used from that thread. The `Send` is
// needed because a `WhpVcpu` is moved onto its own thread before running.
unsafe impl Send for Emulator {}

impl Emulator {
    pub(super) fn new() -> Result<Self, VmmError> {
        let mut handle: *mut c_void = core::ptr::null_mut();
        // SAFETY: `CALLBACKS` is a live, fully initialised callback table whose
        // `Size` matches its type, and `handle` is a live out-parameter. WHP
        // copies the table, so it need not outlive the call.
        unsafe { WHvEmulatorCreateEmulator(&CALLBACKS, &raw mut handle) }
            .map_err(|e| whp_err("WHvEmulatorCreateEmulator", e))?;
        if handle.is_null() {
            return Err(VmmError::Whp {
                call: "WHvEmulatorCreateEmulator",
                message: "returned a null emulator handle".into(),
            });
        }
        Ok(Self { handle })
    }

    /// Decodes and completes the faulting MMIO instruction, dispatching the
    /// access to `handler` and letting the emulator advance RIP.
    pub(super) fn emulate_mmio(
        &self,
        partition: windows::Win32::System::Hypervisor::WHV_PARTITION_HANDLE,
        vp_index: u32,
        handler: &mut dyn ExitHandler,
        vp_context: &WHV_VP_EXIT_CONTEXT,
        access: &WHV_MEMORY_ACCESS_CONTEXT,
    ) -> Result<(), VmmError> {
        let mut ctx = EmulatorContext {
            partition,
            vp_index,
            handler,
        };
        // SAFETY: `self.handle` is a live emulator, `&mut ctx` outlives the call
        // and is the pointer every callback reconstructs, and both context
        // records are live for the duration.
        let status = unsafe {
            WHvEmulatorTryMmioEmulation(
                self.handle,
                (&raw mut ctx).cast::<c_void>(),
                vp_context,
                access,
            )
        }
        .map_err(|e| whp_err("WHvEmulatorTryMmioEmulation", e))?;
        check(status, "MMIO", access.Gpa, vp_context.Rip)
    }

    /// Decodes and completes a string or `REP` port-I/O instruction.
    pub(super) fn emulate_io(
        &self,
        partition: windows::Win32::System::Hypervisor::WHV_PARTITION_HANDLE,
        vp_index: u32,
        handler: &mut dyn ExitHandler,
        vp_context: &WHV_VP_EXIT_CONTEXT,
        access: &WHV_X64_IO_PORT_ACCESS_CONTEXT,
    ) -> Result<(), VmmError> {
        let mut ctx = EmulatorContext {
            partition,
            vp_index,
            handler,
        };
        // SAFETY: as `emulate_mmio`.
        let status = unsafe {
            WHvEmulatorTryIoEmulation(
                self.handle,
                (&raw mut ctx).cast::<c_void>(),
                vp_context,
                access,
            )
        }
        .map_err(|e| whp_err("WHvEmulatorTryIoEmulation", e))?;
        check(
            status,
            "string port I/O",
            u64::from(access.PortNumber),
            vp_context.Rip,
        )
    }
}

impl Drop for Emulator {
    fn drop(&mut self) {
        // SAFETY: `handle` came from a successful `WHvEmulatorCreateEmulator` and
        // is destroyed exactly once (`Drop::drop` runs once, `Emulator` is not
        // `Clone`), with no emulation in flight — the run loop that owns it is
        // gone by then.
        if let Err(e) = unsafe { WHvEmulatorDestroyEmulator(self.handle) } {
            tracing::warn!(error = %e, "WHvEmulatorDestroyEmulator failed");
        }
    }
}

/// Bit 0 of `WHV_EMULATOR_STATUS` is `EmulationSuccessful`; the rest name which
/// part gave up. From WinHvEmulation.h, LSB first.
const STATUS_SUCCESSFUL: u32 = 1 << 0;

const STATUS_REASONS: [(u32, &str); 9] = [
    (1 << 1, "internal emulation failure"),
    (1 << 2, "the I/O port callback failed"),
    (1 << 3, "the memory callback failed"),
    (1 << 4, "the GVA translation callback failed"),
    (1 << 5, "the translated GPA was not page aligned"),
    (1 << 6, "the register read callback failed"),
    (1 << 7, "the register write callback failed"),
    (1 << 8, "an interrupt caused an intercept"),
    (1 << 9, "the guest cannot be faulted"),
];

fn check(status: WHV_EMULATOR_STATUS, what: &str, address: u64, rip: u64) -> Result<(), VmmError> {
    // SAFETY: `WHV_EMULATOR_STATUS` is a `repr(C)` union of a 32-bit bitfield
    // struct and `AsUINT32: u32`; reading `AsUINT32` reads the bitfield's
    // storage.
    let raw = unsafe { status.AsUINT32 };
    if raw & STATUS_SUCCESSFUL != 0 {
        return Ok(());
    }
    let reason = STATUS_REASONS
        .iter()
        .find(|(bit, _)| raw & bit != 0)
        .map_or("no reason reported", |(_, text)| *text);
    Err(VmmError::WhpUnsupportedExit(format!(
        "the WHP instruction emulator could not complete {what} at {address:#x} \
         (rip {rip:#x}): {reason} (status {raw:#010x})"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The callback table's `Size` field is how WHP versions the struct; a
    /// mismatch is rejected with an opaque `E_INVALIDARG`.
    #[test]
    fn callback_table_declares_its_own_size_and_every_callback() {
        assert_eq!(CALLBACKS.Size as usize, size_of::<WHV_EMULATOR_CALLBACKS>());
        assert!(CALLBACKS.WHvEmulatorMemoryCallback.is_some());
        assert!(CALLBACKS.WHvEmulatorIoPortCallback.is_some());
        assert!(CALLBACKS.WHvEmulatorGetVirtualProcessorRegisters.is_some());
        assert!(CALLBACKS.WHvEmulatorSetVirtualProcessorRegisters.is_some());
        assert!(CALLBACKS.WHvEmulatorTranslateGvaPage.is_some());
    }

    fn status(raw: u32) -> WHV_EMULATOR_STATUS {
        let mut status = WHV_EMULATOR_STATUS::default();
        status.AsUINT32 = raw;
        status
    }

    #[test]
    fn a_successful_status_is_ok_and_a_failure_names_its_reason() {
        assert!(check(status(STATUS_SUCCESSFUL), "MMIO", 0xfec0_0000, 0x100).is_ok());

        let err = check(status(1 << 3), "MMIO", 0xfec0_0000, 0x100).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("memory callback failed"), "{text}");
        assert!(
            text.contains("fec00000"),
            "the GPA must be in the message: {text}"
        );
        assert!(
            text.contains("100"),
            "the RIP must be in the message: {text}"
        );

        // An all-zero status still has to produce an error, not a silent success.
        assert!(check(status(0), "MMIO", 0, 0).is_err());
    }

    /// The aligned staging buffer is what keeps a misaligned emulator buffer from
    /// faulting inside WHP, on both the inline and the heap path.
    #[test]
    fn staged_register_buffers_are_16_byte_aligned() {
        for count in [1usize, 4, REGISTER_SCRATCH, REGISTER_SCRATCH + 1, 100] {
            let (aligned, staged) = with_aligned(count, |values| {
                assert_eq!(values.len(), count);
                values.as_ptr() as usize % 16 == 0
            })
            .expect("staging must always succeed");
            assert!(aligned, "count {count} produced a misaligned buffer");
            assert_eq!(staged.len(), count);
        }
    }
}
