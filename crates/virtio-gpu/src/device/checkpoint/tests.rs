use super::*;
use crate::venus::executor::{fake::FakeVulkan, harness as h, ExecutorFactory};
use crate::venus::renderer::VenusRenderer;
use crate::venus::shmem::PageBudget;
use std::io::Cursor;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex,
};
use virtio_core::Quiesce;

#[derive(Default, Clone)]
struct Sink {
    pixels: Arc<Mutex<Vec<u8>>>,
    fail: Arc<AtomicBool>,
}
impl ScanoutSink for Sink {
    fn resolution(&self) -> (u32, u32) {
        (2, 2)
    }
    fn set_resolution(&self, _: u32, _: u32) -> Result<(), crate::SinkError> {
        if self.fail.load(Ordering::Acquire) {
            Err(crate::SinkError::new("injected display failure"))
        } else {
            Ok(())
        }
    }
    fn update_scanout(
        &self,
        _: u32,
        _: u32,
        _: u32,
        _: u32,
        data: &[u8],
    ) -> Result<(), crate::SinkError> {
        self.set_resolution(2, 2)?;
        *self.pixels.lock().unwrap() = data.to_vec();
        Ok(())
    }
    fn set_cursor(
        &self,
        _: u32,
        _: u32,
        _: u32,
        _: u32,
        _: u32,
        _: u32,
        _: &[u8],
    ) -> Result<(), crate::SinkError> {
        Ok(())
    }
    fn move_cursor(&self, _: u32, _: u32) -> Result<(), crate::SinkError> {
        Ok(())
    }
    fn hide_cursor(&self) -> Result<(), crate::SinkError> {
        Ok(())
    }
}

fn source(renderer_scanout: bool) -> (Arc<FakeVulkan>, GpuDevice<Sink>, Vec<u8>) {
    let mut gpu = crate::venus::executor::fake::gpu("checkpoint fixture GPU");
    // Exercise larger native allocations even on hosts with 4 KiB pages.
    gpu.info.host_import_alignment = 16 << 10;
    let host = Arc::new(FakeVulkan::new(vec![
        crate::venus::executor::fake::cpu(),
        gpu,
    ]));
    let gate = Quiesce::new();
    let mut factory = ExecutorFactory::new(host.clone());
    factory
        .enable_checkpoint_recipes(&PageBudget::new(8 << 20))
        .unwrap();
    let mut harness = h::Harness::gated_factory(factory, gate.clone());
    h::with_device(&mut harness);
    const IMAGE_RESOURCE: u32 = 11;
    const IMAGE_BYTES: u64 = 8192;
    if renderer_scanout {
        harness
            .send(&h::allocate(h::DEVICE, h::MEMORY, IMAGE_BYTES, 3, vec![]))
            .unwrap();
        harness
            .memory_blob(h::CTX, IMAGE_RESOURCE, h::MEMORY, IMAGE_BYTES)
            .unwrap();
        assert!(
            harness
                .renderer
                .blob_pages(IMAGE_RESOURCE)
                .unwrap()
                .mapped_len()
                > IMAGE_BYTES
        );
    }
    harness
        .renderer
        .map_blob(h::REPLY_RES, 0, h::REPLY_BYTES)
        .unwrap();
    gate.pause();
    assert!(gate.wait_until_idle(Duration::from_secs(5)));
    let mut ids = vec![h::RING_RES, h::REPLY_RES];
    if renderer_scanout {
        ids.push(IMAGE_RESOURCE);
    }
    let sizes: Vec<_> = ids
        .into_iter()
        .map(|id| {
            // The guest memory blob names its requested span, not the larger
            // native allocation used to meet imported-host-pointer alignment.
            let size = if id == IMAGE_RESOURCE {
                IMAGE_BYTES
            } else {
                harness.renderer.blob_pages(id).unwrap().resource_len()
            };
            (id, size)
        })
        .collect();
    let spec = ScanoutBlobSpec {
        format: crate::FORMAT_B8G8R8A8_UNORM,
        width: 2,
        height: 2,
        stride: 8,
        offset: 4096,
    };
    let pixels = vec![0xa7; 16];
    if renderer_scanout {
        harness
            .renderer
            .blob_pages(IMAGE_RESOURCE)
            .unwrap()
            .write_bytes(4096, &pixels)
            .unwrap();
        harness
            .renderer
            .scanout_blob(IMAGE_RESOURCE, &spec)
            .unwrap();
    }
    let mut device = GpuDevice::with_renderer(Sink::default(), Box::new(harness.renderer));
    assert!(
        device.snapshot_refusal().is_some(),
        "default native-state refusal stays enabled"
    );
    // The fixture already enabled the factory before constructing its guest
    // objects; mirror the device opt-in that real assembly sets beforehand.
    device.accelerated_checkpoints = true;
    assert_eq!(device.snapshot_refusal(), None);
    device.mem = Some(harness.mem);
    let mut front = Gpu3d::new(Box::new(crate::NullRenderer::with_venus()));
    front
        .ctx_create(h::CTX, crate::CAPSET_VENUS, "fixture")
        .unwrap();
    device
        .three_d
        .as_mut()
        .unwrap()
        .restore_checkpoint_contexts(&front.checkpoint_contexts().unwrap())
        .unwrap();
    for (id, size) in sizes {
        let args = crate::protocol::ResourceCreateBlob {
            resource_id: id,
            blob_mem: crate::protocol::BLOB_MEM_HOST3D,
            blob_flags: crate::protocol::BLOB_FLAG_USE_MAPPABLE,
            nr_entries: 0,
            blob_id: if id == IMAGE_RESOURCE { h::MEMORY } else { 0 },
            size,
        };
        let len = device
            .blobs
            .validate(&args, device.blob_support, &[])
            .unwrap();
        device.blobs.insert(&args, &[], len).unwrap();
    }
    device.blobs.reserve_mapping(h::REPLY_RES, 0).unwrap();
    device.blobs.commit_mapping(h::REPLY_RES, 0);
    let rect = Rect {
        x: 0,
        y: 0,
        width: 2,
        height: 2,
    };
    device
        .resources
        .create(99, crate::FORMAT_B8G8R8A8_UNORM, 2, 2)
        .unwrap();
    device
        .mem
        .as_ref()
        .unwrap()
        .write_slice(&pixels, GuestAddress(4096))
        .unwrap();
    let resource = device.resources.get_mut(99).unwrap();
    resource.attach(vec![MemEntry {
        addr: 4096,
        length: 16,
    }]);
    resource
        .transfer_from_backing(device.mem.as_ref().unwrap().as_ref(), rect, 0)
        .unwrap();
    let source = if renderer_scanout {
        device.blobs.set_scanout_spec(IMAGE_RESOURCE, spec);
        ScanoutSource::RendererBlob { spec }
    } else {
        ScanoutSource::TwoD
    };
    device.scanout = Some(ScanoutBinding {
        resource_id: if renderer_scanout { IMAGE_RESOURCE } else { 99 },
        rect,
        source,
    });
    device.events_read.store(1, Ordering::Release);
    (host, device, pixels)
}

fn cold(
    host: Arc<FakeVulkan>,
    mem: Arc<GuestMem>,
    legacy: &[u8],
) -> (GpuDevice<Sink>, Arc<h::RecordingWindow>) {
    let gate = Quiesce::new();
    gate.pause();
    let window = Arc::new(h::RecordingWindow::default());
    let mut renderer = VenusRenderer::new(ExecutorFactory::new(host));
    renderer.set_quiesce(gate);
    renderer.set_host_visible(window.clone());
    let mut device = GpuDevice::with_renderer(Sink::default(), Box::new(renderer));
    device.enable_accelerated_checkpoints(8 << 20).unwrap();
    device.mem = Some(mem);
    device.load_device(legacy).unwrap();
    assert!(device.restored_3d_lost);
    (device, window)
}

#[test]
fn device_checkpoint_restores_scanout_frontend_and_gpu_without_requesting_driver_reset() {
    for renderer_scanout in [false, true] {
        let (host, mut source, pixels) = source(renderer_scanout);
        let mem = source.mem.clone().unwrap();
        let legacy = source.save_device();
        let mut bytes = Vec::new();
        assert!(source.has_streamed_state());
        source.prepare_snapshot().unwrap();
        source.save_streamed_state(&mut bytes).unwrap();
        source.three_d.as_mut().unwrap().reset();
        assert_eq!(host.live_objects(), 0);
        let (mut cold, window) = cold(host.clone(), mem, &legacy);
        assert!(cold.has_streamed_state());
        cold.load_streamed_state(&mut Cursor::new(&bytes)).unwrap();
        assert_eq!(cold.blob_count(), if renderer_scanout { 3 } else { 2 });
        assert_eq!(cold.blobs.get(h::REPLY_RES).unwrap().mapped_at(), Some(0));
        assert_eq!(cold.three_d.as_ref().unwrap().context_count(), 1);
        assert_eq!(window.count(), 1);
        assert_eq!(*cold.display.pixels.lock().unwrap(), pixels);
        assert_eq!(cold.events_read.load(Ordering::Acquire), 1);
        assert!(!cold.restored_3d_lost);
        assert!(!cold.renderer_lost);
        assert!(
            matches!(
                cold.scanout.unwrap().source,
                ScanoutSource::RendererBlob { .. }
            ) == renderer_scanout
        );
        assert!(cold
            .load_accelerated_checkpoint(&mut Cursor::new(&bytes), Duration::from_secs(5))
            .is_err());
        assert_eq!(
            window.count(),
            1,
            "non-cold load must preserve the restored session"
        );
        let mut second = Vec::new();
        cold.save_accelerated_checkpoint(&mut second, Duration::from_secs(5))
            .unwrap();
        cold.three_d.as_mut().unwrap().reset();
        assert_eq!(window.count(), 0);
        assert_eq!(host.live_objects(), 0);
    }
}

#[test]
fn malformed_device_metadata_or_display_failure_cannot_leave_native_resources_published() {
    let (host, mut source, _) = source(true);
    let mem = source.mem.clone().unwrap();
    let legacy = source.save_device();
    let mut bytes = Vec::new();
    source
        .save_accelerated_checkpoint(&mut bytes, Duration::from_secs(5))
        .unwrap();
    source.three_d.as_mut().unwrap().reset();
    let (mut cold, window) = cold(host.clone(), mem, &legacy);
    for (at, value) in [
        (8, 2_u32),
        (16, u32::MAX),
        (20, u32::MAX),
        (24, 4),
        (28, 99),
        (40, u32::MAX),
        (48, 0),
        (56, 1),
    ] {
        let mut bad = bytes.clone();
        bad[at..at + 4].copy_from_slice(&value.to_le_bytes());
        assert!(
            cold.load_accelerated_checkpoint(&mut Cursor::new(bad), Duration::from_secs(5))
                .is_err(),
            "bad field {at}"
        );
        assert_eq!(window.count(), 0);
        assert_eq!(host.live_objects(), 0);
        assert!(cold.blobs.is_empty());
    }
    cold.display.fail.store(true, Ordering::Release);
    assert!(cold
        .load_accelerated_checkpoint(&mut Cursor::new(&bytes), Duration::from_secs(5))
        .is_err());
    assert_eq!(host.live_objects(), 0);
    assert_eq!(window.count(), 0);
    assert!(cold.blobs.is_empty());
    assert!(cold.restored_3d_lost);
    cold.display.fail.store(false, Ordering::Release);
    cold.load_accelerated_checkpoint(&mut Cursor::new(&bytes), Duration::from_secs(5))
        .unwrap();
    assert_eq!(window.count(), 1);
    cold.three_d.as_mut().unwrap().reset();
    assert_eq!(window.count(), 0);
}
