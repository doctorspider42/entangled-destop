//! The real Venus path on the real host library (VEN-2003, ADR-0004).
//!
//! What phase 1 could only assert about *symbols* this asserts about
//! behaviour: a venus-typed context, a `HOST3D` blob, and
//! `virgl_renderer_resource_map` handing back host memory that actually lands
//! in the device's host-visible window at the offset the guest named.
//!
//! Self-skips — like every other `*_host.rs` here — when the host has no
//! library, no Venus in it, or no usable EGL. On this project's dev machine
//! that means: green and skipped against jammy's packaged 0.9.1, green and
//! *exercised* against the build from
//! `guest/virglrenderer/build-virglrenderer.sh` with `ENTANGLED_VIRGL_LIB`
//! pointing at it.
//!
//! It deliberately does **not** need a hypervisor. The window's job here is to
//! be the seam (`virtio_core::ShmBacking::map_host`), and what the test proves
//! about the pointer that arrives is that it is real host memory: page
//! aligned, at least as long as the blob, and readable and writable. Whether a
//! *guest* can then reach it is `machine-x86`'s question and `shm_bus.rs`
//! answers it.

#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use virtio_core::{ShmAccessError, ShmBacking, ShmMapError};
use virtio_gpu::protocol::{
    ResourceCreateBlob, BLOB_FLAG_USE_MAPPABLE, BLOB_MEM_HOST3D, MAP_CACHE_MASK,
};
use virtio_gpu::virgl::VirglRenderer;
use virtio_gpu::{Gpu3d, Renderer3d};

const WINDOW: u64 = 256 << 20;
const BLOB_BYTES: u64 = 64 << 10;

/// A host-visible window with no hypervisor behind it: it records what the
/// renderer asks it to map, which is exactly the seam under test.
#[derive(Default)]
struct RecordingWindow {
    /// offset -> (host address, length)
    mapped: Mutex<BTreeMap<u64, (u64, u64)>>,
}

impl ShmBacking for RecordingWindow {
    fn len(&self) -> u64 {
        WINDOW
    }

    fn host_mapped(&self) -> bool {
        true
    }

    fn read(&self, offset: u64, buf: &mut [u8]) -> Result<(), ShmAccessError> {
        Err(ShmAccessError {
            offset,
            len: buf.len() as u64,
            window: 0,
        })
    }

    fn write(&self, offset: u64, data: &[u8]) -> Result<(), ShmAccessError> {
        Err(ShmAccessError {
            offset,
            len: data.len() as u64,
            window: 0,
        })
    }

    fn fill(&self, offset: u64, len: u64, _byte: u8) -> Result<(), ShmAccessError> {
        Err(ShmAccessError {
            offset,
            len,
            window: 0,
        })
    }

    unsafe fn map_host(&self, offset: u64, host_addr: u64, len: u64) -> Result<(), ShmMapError> {
        if offset.checked_add(len).is_none_or(|end| end > WINDOW) {
            return Err(ShmMapError::Refused("outside the window".into()));
        }
        self.mapped
            .lock()
            .expect("window lock")
            .insert(offset, (host_addr, len));
        Ok(())
    }

    fn unmap_host(&self, offset: u64) {
        self.mapped.lock().expect("window lock").remove(&offset);
    }
}

#[test]
fn a_venus_blob_is_mapped_into_the_host_visible_window() {
    let renderer = match VirglRenderer::load() {
        Ok(renderer) => renderer,
        Err(e) => {
            eprintln!("skipping: {e}");
            return;
        }
    };
    if !renderer
        .capsets()
        .iter()
        .any(|c| c.id == virtio_gpu::CAPSET_VENUS)
    {
        eprintln!(
            "skipping: this host's libvirglrenderer has no Venus \
             (build one with guest/virglrenderer/build-virglrenderer.sh and set \
             ENTANGLED_VIRGL_LIB)"
        );
        return;
    }
    let window = Arc::new(RecordingWindow::default());
    let mut gpu = Gpu3d::new(Box::new(renderer));
    gpu.set_host_visible(window.clone());

    // A venus-typed context. This is the first call that brings EGL up *and*
    // starts virglrenderer's render server, so a host with the library but no
    // working GL/Vulkan is a skip rather than a failure.
    if let Err(e) = gpu.ctx_create(1, virtio_gpu::CAPSET_VENUS, "venus-host-test") {
        eprintln!("skipping: Venus is advertised but a venus context will not start: {e}");
        return;
    }

    let args = ResourceCreateBlob {
        resource_id: 10,
        blob_mem: BLOB_MEM_HOST3D,
        blob_flags: BLOB_FLAG_USE_MAPPABLE,
        nr_entries: 0,
        // `blob_id` 0 is what mesa's venus driver uses for the shmem it
        // allocates before any `VkDeviceMemory` exists — its command ring.
        blob_id: 0,
        size: BLOB_BYTES,
    };
    let mem = Arc::new(virtio_core::testing::guest_memory(1 << 20));
    gpu.create_blob(1, &args, &mem, &[])
        .expect("a host3d blob on a venus context");

    // The guest names the offset; the device has already bounded it. Use one
    // that is neither zero nor the first page, so a renderer that ignored it
    // would be visible.
    const OFFSET: u64 = 8 * 4096;
    let mapping = gpu
        .map_blob(10, OFFSET, BLOB_BYTES)
        .expect("resource_map put the blob in the window");
    assert_ne!(
        mapping.wire() & MAP_CACHE_MASK,
        0,
        "the guest must be told a caching type it can act on"
    );

    let (addr, len) = *window
        .mapped
        .lock()
        .expect("window lock")
        .get(&OFFSET)
        .expect("the renderer mapped at the offset it was given");
    assert_eq!(
        len, BLOB_BYTES,
        "exactly the blob, never more of the window"
    );
    assert_eq!(
        addr % 4096,
        0,
        "a hypervisor mapping needs a page-aligned host address, and \
         vkMapMemory only promises minMemoryMapAlignment"
    );

    // The point of the whole exercise: that address is real host memory. If it
    // were not, this is where a Venus guest would fault instead.
    // SAFETY: `addr` is the base of `len` bytes the library mapped for this
    // blob and holds until `unmap_blob` below; nothing else writes it, and the
    // slice never outlives the mapping.
    let bytes = unsafe { std::slice::from_raw_parts_mut(addr as *mut u8, len as usize) };
    bytes[..8].copy_from_slice(b"VENUS-OK");
    bytes[len as usize - 1] = 0x5a;
    assert_eq!(&bytes[..8], b"VENUS-OK");
    assert_eq!(bytes[len as usize - 1], 0x5a);

    // Mapping the same blob twice is refused rather than silently re-mapped:
    // `virgl_renderer_resource_map` refuses it too, and an in-band error the
    // device can explain beats a renderer error it cannot.
    assert!(gpu.map_blob(10, OFFSET + BLOB_BYTES, BLOB_BYTES).is_err());

    gpu.unmap_blob(10, OFFSET);
    assert!(
        window.mapped.lock().expect("window lock").is_empty(),
        "the guest mapping comes down before the renderer frees the pages"
    );

    // …and the blob can be mapped again afterwards, so the refusal above is
    // not a latch.
    gpu.map_blob(10, 0, BLOB_BYTES).expect("map again");
    assert!(window.mapped.lock().expect("window lock").contains_key(&0));

    // A device reset must take it out of the guest even though the library
    // teardown is deferred to the worker thread.
    gpu.reset();
    assert!(
        window.mapped.lock().expect("window lock").is_empty(),
        "a reset leaves no window mapping pointing at freed renderer memory"
    );
}
