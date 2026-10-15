//! A fixed, single-region MMIO shared window backed by the hypervisor seam.
use std::sync::Arc;
use virtio_core::{ShmAccessError, ShmBacking, ShmMapError};
use vmm_core::shm::{HostRange, SharedWindow};

pub struct WindowBacking(pub Arc<SharedWindow>);

impl ShmBacking for WindowBacking {
    fn len(&self) -> u64 {
        self.0.len()
    }
    fn host_mapped(&self) -> bool {
        self.0.is_host_mapped()
    }
    unsafe fn map_host(&self, offset: u64, host_addr: u64, len: u64) -> Result<(), ShmMapError> {
        if !self.host_mapped() {
            return Err(ShmMapError::Unsupported);
        }
        // SAFETY: the caller promises this range remains live until unmap_host.
        // SharedWindow checks size, alignment and overlap before calling HVF.
        let range = unsafe { HostRange::new(host_addr, len) };
        self.0
            .map_host_range(offset, range)
            .map_err(|e| ShmMapError::Refused(e.to_string()))
    }
    fn unmap_host(&self, offset: u64) {
        if offset < self.len() {
            self.0.unmap_host_range(offset);
        }
    }
    fn read(&self, offset: u64, data: &mut [u8]) -> Result<(), ShmAccessError> {
        self.0.read(offset, data).map_err(|_| ShmAccessError {
            offset,
            len: data.len() as u64,
            window: self.len(),
        })
    }
    fn write(&self, offset: u64, data: &[u8]) -> Result<(), ShmAccessError> {
        self.0.write(offset, data).map_err(|_| ShmAccessError {
            offset,
            len: data.len() as u64,
            window: self.len(),
        })
    }
    fn fill(&self, offset: u64, len: u64, byte: u8) -> Result<(), ShmAccessError> {
        self.0.fill(offset, len, byte).map_err(|_| ShmAccessError {
            offset,
            len,
            window: self.len(),
        })
    }
}
