use super::*;
use crate::venus::executor::fake::FakeVulkan;

mod context;

fn encode(record: &Record<'_>) -> Vec<u8> {
    let version = match record.state {
        SharedImageState::Initial => INITIAL_VERSION,
        SharedImageState::Released(_) => VERSION,
        SharedImageState::ReleasedTransition { .. } => TRANSITION_VERSION,
    };
    let mut bytes = version.to_le_bytes().to_vec();
    let (layout, family) = match record.state {
        SharedImageState::Initial => (0i32, u32::MAX),
        SharedImageState::Released(release)
        | SharedImageState::ReleasedTransition { release, .. } => (release.layout, release.family),
    };
    bytes.extend_from_slice(&record.uuids.0);
    bytes.extend_from_slice(&record.uuids.1);
    bytes.extend_from_slice(&record.size.to_le_bytes());
    for value in [
        record.type_index,
        record.image.format as u32,
        record.image.flags,
        record.image.usage,
        record.image.width,
        record.image.height,
        2,
    ] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    for format in &record.image.view_formats {
        bytes.extend_from_slice(&format.to_le_bytes());
    }
    bytes.extend_from_slice(&layout.to_le_bytes());
    bytes.extend_from_slice(&family.to_le_bytes());
    if let SharedImageState::ReleasedTransition { old_layout, .. } = record.state {
        bytes.extend_from_slice(&old_layout.to_le_bytes());
    }
    bytes.extend_from_slice(&(record.pixels.len() as u64).to_le_bytes());
    bytes.extend_from_slice(record.pixels);
    bytes
}

fn device<H: HostVulkan>(host: Arc<H>) -> ScanoutDevice<H> {
    let instance = host.create_instance(&InstanceRequest::default()).unwrap();
    let uuids = host
        .enumerate_physical_devices(&instance)
        .unwrap()
        .into_iter()
        .find_map(|physical| {
            policy::expose(host.describe_physical_device(&instance, physical))
                .ok()
                .and_then(|guest| guest.uuids())
        })
        .unwrap();
    host.destroy_instance(instance);
    ScanoutDevice::open(host, PageBudget::new(256 << 20), uuids).unwrap()
}

fn seed<H: HostVulkan>(device: &ScanoutDevice<H>, layout: i32) -> Vec<u8> {
    let instance = device.instance.as_ref().unwrap();
    let physical = device
        .host
        .enumerate_physical_devices(instance)
        .unwrap()
        .into_iter()
        .find(|physical| {
            policy::expose(device.host.describe_physical_device(instance, *physical))
                .is_ok_and(|g| g.uuids() == Some(device.uuids))
        })
        .unwrap();
    let canonical =
        super::super::super::modifier::canonical_format(&*device.host, instance, physical, 44)
            .unwrap();
    let image = canonical.image(513, 257);
    let owner = device.device.as_ref().unwrap();
    let temporary = device
        .host
        .create_image(owner, &image.create_info(0), ResourceMemory::Handle)
        .unwrap();
    let mut req = VkMemoryRequirements2::default();
    device
        .host
        .image_memory_requirements(owner, temporary, None, &mut req);
    device.host.destroy_image(owner, temporary);
    let guest = policy::expose(device.host.describe_physical_device(instance, physical)).unwrap();
    let index = guest
        .memory
        .memory_types
        .iter()
        .enumerate()
        .take(guest.memory.memory_type_count as usize)
        .find(|(index, ty)| {
            ty.property_flags & 3 == 1
                && req.memory_requirements.memory_type_bits & (1 << index) != 0
        })
        .map(|(index, _)| index as u32)
        .unwrap();
    // A varying pattern detects row, channel and stride mistakes.
    let mut pixels = vec![0; 513 * 257 * 4];
    for (index, pixel) in pixels.chunks_exact_mut(4).enumerate() {
        let x = index % 513;
        let y = index / 513;
        pixel.copy_from_slice(&[x as u8, y as u8, (x ^ y) as u8, 255]);
    }
    encode(&Record {
        uuids: device.uuids,
        size: req.memory_requirements.size.next_multiple_of(4096),
        type_index: index,
        image,
        state: SharedImageState::Released(ScanoutRelease {
            layout,
            family: policy::QUEUE_FAMILY_FOREIGN,
        }),
        pixels: &pixels,
    })
}

fn fake_host() -> Arc<FakeVulkan> {
    let mut gpu = crate::venus::executor::fake::zink_gpu("checkpoint GPU");
    gpu.info
        .extensions
        .push(crate::venus::protocol::VkExtensionProperties {
            extension_name: policy::name_array(policy::QUEUE_FAMILY_FOREIGN_EXT),
            spec_version: 1,
        });
    Arc::new(FakeVulkan::new(vec![gpu]))
}

#[test]
fn initial_payload_is_header_only_and_rejects_pixels_or_release_metadata() {
    let host = fake_host();
    let device = device(host.clone());
    let template = seed(&device, LAYOUT_TRANSFER_SRC);
    let mut record = parse(&template).unwrap();
    record.state = SharedImageState::Initial;
    record.pixels = &[];
    let bytes = encode(&record);
    assert_eq!(bytes.len(), 96);
    assert_eq!(encoded_len(&record.image, record.state).unwrap(), 96);
    assert_eq!(parse(&bytes).unwrap().state, SharedImageState::Initial);
    for (offset, value) in [(80, 6u32), (84, policy::QUEUE_FAMILY_FOREIGN), (88, 4)] {
        let mut invalid = bytes.clone();
        invalid[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        assert!(parse(&invalid).is_err());
    }
    let mut invalid = bytes.clone();
    invalid.push(0);
    assert!(parse(&invalid).is_err());
    invalid = bytes;
    invalid[..4].copy_from_slice(&VERSION.to_le_bytes());
    assert!(
        parse(&invalid).is_err(),
        "a released payload cannot discard its pixels"
    );
}

#[test]
fn changing_layout_transfer_preserves_both_layouts_and_rejects_invalid_old_layout() {
    let device = device(fake_host());
    let template = seed(&device, 1);
    let mut record = parse(&template).unwrap();
    record.image.usage |= 0x10;
    let release = ScanoutRelease {
        layout: 1,
        family: policy::QUEUE_FAMILY_FOREIGN,
    };
    record.state = SharedImageState::ReleasedTransition {
        old_layout: 2,
        release,
    };
    let bytes = encode(&record);
    let parsed = parse(&bytes).unwrap();
    assert_eq!(parsed.state, record.state);
    assert_eq!(parsed.pixels, record.pixels);
    assert_eq!(
        encoded_len(&record.image, record.state).unwrap(),
        bytes.len() as u64
    );
    assert_eq!(encode(&parsed), bytes);
    for old in [0i32, 1, 8, -1] {
        let mut invalid = bytes.clone();
        invalid[88..92].copy_from_slice(&old.to_le_bytes());
        assert!(parse(&invalid).is_err());
    }
    // A color-attachment old layout also needs color-attachment usage.
    record.image.usage &= !0x10;
    assert!(parse(&encode(&record)).is_err());
}

#[test]
fn malformed_shared_image_records_never_allocate_or_submit() {
    let host = fake_host();
    let mut device = device(host.clone());
    let bytes = seed(&device, LAYOUT_TRANSFER_SRC);
    let calls = host.calls();
    let live = host.live_objects();
    let mut bad: Vec<_> = (0..100)
        .chain([bytes.len() - 1])
        .map(|end| bytes[..end].to_vec())
        .collect();
    for offset in (0..96).step_by(4) {
        let mut changed = bytes.clone();
        changed[offset..offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        bad.push(changed);
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    bad.push(trailing);
    for malformed in bad {
        assert!(device
            .restore_shared_image(1, &malformed, &ContextLimits::default())
            .is_err());
        assert_eq!(host.calls(), calls);
        assert_eq!(host.live_objects(), live);
    }
    drop(device);
    assert_eq!(host.live_objects(), 0);
}

const STAGE: &str = "ENTANGLED_SHARED_IMAGE_CHECKPOINT_STAGE";
const DIRECTORY: &str = "ENTANGLED_SHARED_IMAGE_CHECKPOINT_DIRECTORY";

#[test]
fn an_upload_timeout_retains_native_imports_and_their_heap_charge() {
    use std::sync::atomic::Ordering;
    let host = fake_host();
    let mut device = device(host.clone());
    let bytes = seed(&device, LAYOUT_TRANSFER_SRC);
    let limits = ContextLimits::default();
    host.stuck.store(true, Ordering::SeqCst);
    assert_eq!(
        device.restore_shared_image(7, &bytes, &limits).unwrap_err(),
        ScanoutError::Timeout
    );
    assert_eq!(device.target_count(), 1);
    assert!(!limits.usage().holds_nothing());
    host.stuck.store(false, Ordering::SeqCst);
    device.clear();
    assert_eq!(device.target_count(), 0);
    assert!(limits.usage().holds_nothing());
    drop(device);
    assert_eq!(host.live_objects(), 0);
}

#[test]
fn shared_image_checkpoint_survives_process_exit() {
    struct Fixture(std::path::PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let fixture = Fixture(std::env::temp_dir().join(format!(
        "entangled-shared-image-{}-{nonce}",
        std::process::id()
    )));
    std::fs::create_dir(&fixture.0).unwrap();
    for stage in ["save", "restore"] {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["venus::executor::scanout::checkpoint::tests::shared_image_checkpoint_process_stage", "--exact", "--nocapture", "--test-threads", "1"])
            .env(STAGE, stage).env(DIRECTORY, &fixture.0)
            .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(90);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break Some(status);
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                break None;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let output = child.wait_with_output().unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(status.is_some_and(|s| s.success()), "{stage}: {text}");
        if stage == "save" && !fixture.0.join("image-1").exists() {
            assert!(
                text.lines().any(|line| line.starts_with("skipping:")),
                "{text}"
            );
            eprintln!("skipping: no usable host GPU for shared image checkpoint");
            return;
        }
        eprintln!("shared image checkpoint {stage} passed in a separate process");
    }
    assert!(fixture.0.join("restored-ok").exists());
}

#[test]
fn shared_image_checkpoint_process_stage() {
    let Ok(stage) = std::env::var(STAGE) else {
        return;
    };
    let directory = std::path::PathBuf::from(std::env::var_os(DIRECTORY).unwrap());
    let host = match crate::host_vulkan::AshVulkan::load() {
        Ok(host) => Arc::new(host),
        Err(error) => {
            eprintln!("skipping: {error}");
            return;
        }
    };
    match host.usable_devices() {
        Ok(devices) if !devices.is_empty() => {}
        Ok(_) => {
            eprintln!("skipping: no usable host GPU");
            return;
        }
        Err(error) => {
            eprintln!("skipping: {error}");
            return;
        }
    }
    struct DiskError;
    impl Write for DiskError {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("injected disk error"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    for layout in [1, 2, 5, 6, 7] {
        let mut producer = device(host.clone());
        if layout == 6 {
            if stage == "save" {
                context::save(host.clone(), &mut producer, &directory);
                context::initial::save(host.clone(), &producer, &directory);
                context::initial::save_variant(host.clone(), &producer, &directory, true);
            } else {
                context::restore(host.clone(), &mut producer, &directory);
                context::initial::restore(host.clone(), &directory);
                context::transition::restore(host.clone(), &directory);
            }
        }
        let image_file = directory.join(format!("image-{layout}"));
        let expected_file = directory.join(format!("expected-{layout}"));
        let bytes = if stage == "save" {
            seed(&producer, layout)
        } else {
            std::fs::read(&image_file).unwrap()
        };
        let expected = if stage == "save" {
            bytes.clone()
        } else {
            std::fs::read(&expected_file).unwrap()
        };
        let limits = ContextLimits::default();
        let (target, release) = producer.restore_shared_image(7, &bytes, &limits).unwrap();
        drop(producer);
        // The exporting device is gone. Its handle alone keeps the allocation alive.
        let mut consumer = device(host.clone());
        assert!(consumer
            .save_shared_image(&target, release, &mut DiskError)
            .is_err());
        let mut copied = Vec::new();
        consumer
            .save_shared_image(&target, release, &mut copied)
            .unwrap();
        assert_eq!(
            copied, expected,
            "every pixel survives exporter loss and a failed disk write, layout {layout}"
        );
        drop(target);
        assert!(
            !limits.usage().holds_nothing(),
            "the remaining native import keeps the charge alive"
        );
        drop(consumer);
        assert!(limits.usage().holds_nothing());
        if stage == "save" {
            std::fs::write(image_file, copied).unwrap();
            std::fs::write(expected_file, expected).unwrap();
        }
    }
    if stage == "restore" {
        std::fs::write(directory.join("restored-ok"), []).unwrap();
    }
}
