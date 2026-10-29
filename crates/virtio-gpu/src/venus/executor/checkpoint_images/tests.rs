use super::super::{fake::FakeVulkan, harness as h, recording as r};
use super::*;

fn run(context: &mut VulkanContext<FakeVulkan>, mut command: Command<'_>) {
    context.execute(&mut command).unwrap();
    assert!(super::super::context::result_of(&command).is_none_or(|r| r == VK_SUCCESS));
}

fn fixture() -> (Arc<FakeVulkan>, VulkanContext<FakeVulkan>) {
    fixture_extent(None)
}

fn fixture_extent(extent: Option<(u32, u32)>) -> (Arc<FakeVulkan>, VulkanContext<FakeVulkan>) {
    fixture_flags(extent, 0)
}

fn fixture_flags(
    extent: Option<(u32, u32)>,
    flags: u32,
) -> (Arc<FakeVulkan>, VulkanContext<FakeVulkan>) {
    fixture_format(extent, flags, 37)
}

fn fixture_format(
    extent: Option<(u32, u32)>,
    flags: u32,
    format: i32,
) -> (Arc<FakeVulkan>, VulkanContext<FakeVulkan>) {
    let host = Arc::new(FakeVulkan::standard());
    let mut context = VulkanContext::new(1, host.clone());
    run(&mut context, h::create_instance(h::INSTANCE));
    run(
        &mut context,
        h::enumerate(h::INSTANCE, Some(vec![h::PHYSICAL])),
    );
    run(
        &mut context,
        h::create_device(h::PHYSICAL, h::DEVICE, Vec::new()),
    );
    run(&mut context, h::device_queue(h::DEVICE, h::QUEUE, 1));
    run(&mut context, h::create_pool(h::DEVICE, h::POOL));
    let mut info = h::image_info();
    info.flags = flags;
    info.format = format;
    if let Some((width, height)) = extent {
        info.extent.width = width;
        info.extent.height = height;
    }
    info.usage = if (124..=130).contains(&format) {
        0x23
    } else {
        0x13
    };
    run(&mut context, h::create_image(h::DEVICE, h::IMAGE, info));
    run(
        &mut context,
        h::allocate(h::DEVICE, h::MEMORY, 4 << 20, 1, Vec::new()),
    );
    run(
        &mut context,
        h::bind_image(h::DEVICE, h::IMAGE, h::MEMORY, 0),
    );
    run(
        &mut context,
        r::allocate_cbs(h::DEVICE, h::POOL, &[r::CB], false),
    );
    run(&mut context, r::begin(r::CB));
    run(
        &mut context,
        aspect_barrier(
            VkCommandBuffer(r::CB),
            h::IMAGE,
            (
                match format {
                    124..=126 => 2,
                    127 => 4,
                    128..=130 => 6,
                    _ => 1,
                },
                0,
                0,
            ),
            (0, if (124..=130).contains(&format) { 3 } else { 2 }),
            true,
            true,
        ),
    );
    run(&mut context, r::end(r::CB));
    run(&mut context, r::queue_submit(h::QUEUE, &[r::CB], 0));
    (host, context)
}

#[test]
fn depth_and_stencil_aspects_have_independent_payloads_and_survive_a_second_capture() {
    for format in 124..=130 {
        let (host, mut context) = fixture_format(Some((513, 513)), 0, format);
        let wait = Duration::from_secs(2);
        let mut bytes = Vec::new();
        context.save_device_images(&mut bytes, wait).unwrap();
        assert_eq!(
            context.checkpoint_images_length().unwrap(),
            bytes.len() as u64
        );
        let mut input = crate::venus::wire::Decoder::new(&bytes);
        assert_eq!(input.u32().unwrap(), VERSION);
        let blocks = aspect_blocks(format).unwrap();
        assert_eq!(input.u32().unwrap() as usize, blocks.len());
        for (aspect, block) in blocks {
            for expected in [h::DEVICE, h::IMAGE, h::MEMORY, 0] {
                assert_eq!(input.u64().unwrap(), expected);
            }
            for expected in [format as u32, 513, 513, 1, 1, 0, aspect, 0, 3, 2, 0] {
                assert_eq!(input.u32().unwrap(), expected);
            }
            let len = 513 * 513 * u64::from(block.bytes);
            assert_eq!(input.u64().unwrap(), len);
            input.blob(len as usize).unwrap();
        }
        assert_eq!(input.remaining(), 0);
        context.restore_device_images(&bytes, wait).unwrap();
        let mut again = Vec::new();
        context.save_device_images(&mut again, wait).unwrap();
        assert_eq!(bytes, again, "format {format}");
        context.destroy_all();
        assert_eq!(host.live_objects(), 0);
    }
}

#[test]
fn mismatched_saved_depth_stencil_states_are_refused_before_upload() {
    let (host, mut context) = fixture_format(None, 0, 130);
    let mut bytes = Vec::new();
    context
        .save_device_images(&mut bytes, Duration::from_secs(2))
        .unwrap();
    let first = 8
        + 64
        + 20
        + h::image_info().extent.width as usize * h::image_info().extent.height as usize * 4;
    // The second record is the stencil aspect; its saved cell must match depth.
    let stencil_layout = first + 64;
    bytes[stencil_layout..stencil_layout + 4].copy_from_slice(&4_u32.to_le_bytes());
    let calls = host.calls();
    let error = context
        .restore_device_images(&bytes, Duration::from_secs(2))
        .unwrap_err();
    assert!(
        error.to_string().contains("paired depth/stencil"),
        "{error}"
    );
    assert_eq!(host.calls(), calls);
    context.destroy_all();
    assert_eq!(host.live_objects(), 0);
}

#[test]
fn mutable_and_extended_view_flags_keep_the_base_pixels_capturable() {
    for flags in [8, 0x108] {
        let (host, mut context) = fixture_flags(None, flags);
        let mut bytes = Vec::new();
        context
            .save_device_images(&mut bytes, Duration::from_secs(2))
            .unwrap();
        context
            .restore_device_images(&bytes, Duration::from_secs(2))
            .unwrap();
        let mut again = Vec::new();
        context
            .save_device_images(&mut again, Duration::from_secs(2))
            .unwrap();
        assert_eq!(bytes, again);
        context.destroy_all();
        assert_eq!(host.live_objects(), 0);
    }
    let (host, mut context) = fixture_flags(None, 0x108);
    let submits = host.submitted().len();
    let features = host
        .format_features
        .load(std::sync::atomic::Ordering::SeqCst);
    host.format_features
        .store(features & !0xc000, std::sync::atomic::Ordering::SeqCst);
    let error = context
        .save_device_images(&mut Vec::new(), Duration::from_secs(2))
        .unwrap_err();
    assert!(error.to_string().contains("base format lacks transfer"));
    assert_eq!(submits, host.submitted().len());
    context.destroy_all();
    assert_eq!(host.live_objects(), 0);
}

#[test]
fn view_compatibility_does_not_admit_protected_sparse_or_alias_flags() {
    for flag in [1, 2, 4, 0x400, 0x800, 0x2000, 0x4000] {
        let (host, mut context) = fixture();
        context
            .objects
            .image_mut(h::DEVICE, h::IMAGE)
            .unwrap()
            .facts
            .flags |= flag;
        let before = host.submitted().len();
        assert!(context
            .save_device_images(&mut Vec::new(), Duration::from_secs(2))
            .is_err());
        assert_eq!(host.submitted().len(), before);
        context.destroy_all();
        assert_eq!(host.live_objects(), 0);
    }
}

#[test]
fn tiled_block_copies_cover_every_pixel_without_overlapping() {
    for format in [9, 23, 37, 131, 137, 157, 165, 184] {
        let block = formats::color_block(format).unwrap();
        for (width, height) in [(1, 1), (513, 513), (400_001, 3), (9, 101)] {
            let facts = ImageFacts {
                width,
                height,
                flags: 0,
                image_type: 1,
                format,
                mip_levels: 1,
                array_layers: 1,
                tiling: 0,
                usage: 3,
                planes: 1,
                depth: 1,
                samples: 1,
            };
            let tiles = Tiles::new(facts, block, 0);
            let total = tiles.byte_len().unwrap();
            let mut bytes = 0;
            let mut area = 0;
            let mut old = None;
            for tile in tiles {
                assert!(tile.bytes > 0 && tile.bytes <= CHUNK_BYTES);
                assert_eq!(tile.x % block.width, 0);
                assert_eq!(tile.y % block.height, 0);
                assert!(tile.x + tile.width == width || tile.width % block.width == 0);
                assert!(tile.y + tile.height == height || tile.height % block.height == 0);
                if let Some((x, y, w, h)) = old {
                    assert!(tile.y >= y + h || (tile.y == y && tile.x >= x + w));
                }
                old = Some((tile.x, tile.y, tile.width, tile.height));
                bytes += tile.bytes as u64;
                area += u64::from(tile.width) * u64::from(tile.height);
            }
            assert_eq!(bytes, total);
            assert_eq!(area, u64::from(width) * u64::from(height));
        }
    }
    assert!(
        formats::color_block(129).is_none(),
        "depth/stencil is separate"
    );
}

#[test]
fn malformed_image_files_never_upload_or_change_source_layouts() {
    let (host, mut context) = fixture();
    let wait = Duration::from_secs(2);
    let mut bytes = Vec::new();
    context.save_device_images(&mut bytes, wait).unwrap();
    let calls = host.calls();
    let mut bad: Vec<_> = (0..92)
        .chain([bytes.len() - 1])
        .map(|end| bytes[..end].to_vec())
        .collect();
    for offset in (0..92).step_by(4) {
        let mut changed = bytes.clone();
        changed[offset..offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        bad.push(changed);
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    bad.push(trailing);
    for malformed in bad {
        assert!(context.restore_device_images(&malformed, wait).is_err());
        assert_eq!(host.calls(), calls);
        assert_eq!(
            context.checkpoint_image_layouts(h::IMAGE).unwrap()[0]
                .cell
                .layout,
            2
        );
        assert!(!context.is_fatal());
    }
    context.restore_device_images(&bytes, wait).unwrap();
    assert_eq!(
        context.checkpoint_image_layouts(h::IMAGE).unwrap()[0]
            .cell
            .layout,
        2
    );
    context.destroy_all();
    assert_eq!(host.live_objects(), 0);
}

#[test]
fn a_readback_timeout_keeps_the_destroyed_image_until_the_gpu_finishes() {
    use std::sync::atomic::Ordering;
    let (host, mut context) = fixture();
    host.stuck.store(true, Ordering::SeqCst);
    assert!(context
        .save_device_images(&mut Vec::new(), Duration::from_secs(2))
        .is_err());
    run(&mut context, h::destroy_image(h::DEVICE, h::IMAGE));
    assert_eq!(host.live("image"), 1);
    assert_eq!(host.live("buffer"), 1, "private staging survives too");
    assert!(!context.is_fatal());
    host.stuck.store(false, Ordering::SeqCst);
    context
        .checkpoint_wait_idle(Instant::now() + Duration::from_secs(2))
        .unwrap();
    assert_eq!(host.live("image"), 0);
    assert_eq!(host.live("buffer"), 0);
    context.destroy_all();
    assert_eq!(host.live_objects(), 0);
}

#[test]
fn a_failed_native_image_upload_discards_the_partial_context() {
    let (host, mut context) = fixture();
    let mut bytes = Vec::new();
    context
        .save_device_images(&mut bytes, Duration::from_secs(2))
        .unwrap();
    host.lose_at("vkQueueSubmit");
    assert!(context
        .restore_device_images(&bytes, Duration::from_secs(2))
        .is_err());
    assert_eq!(context.object_count(), 0);
    assert_eq!(context.budget.used(), 0);
    assert_eq!(host.live_objects(), 0);
}

#[test]
fn a_later_streamed_image_tile_io_failure_discards_the_partial_context() {
    use std::io::{Cursor, Read, Seek, SeekFrom};
    struct FailSecondTile {
        source: Cursor<Vec<u8>>,
        saw_tile: bool,
    }
    impl Read for FailSecondTile {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            assert!(bytes.len() <= CHUNK_BYTES);
            if bytes.len() > 8 {
                if self.saw_tile {
                    return Err(std::io::Error::other("simulated later tile I/O failure"));
                }
                self.saw_tile = true;
            }
            self.source.read(bytes)
        }
    }
    impl Seek for FailSecondTile {
        fn seek(&mut self, at: SeekFrom) -> std::io::Result<u64> {
            self.source.seek(at)
        }
    }
    let (host, mut context) = fixture_extent(Some((513, 513)));
    let mut bytes = Vec::new();
    context
        .save_device_images(&mut bytes, Duration::from_secs(2))
        .unwrap();
    let mut reader = FailSecondTile {
        source: Cursor::new(bytes),
        saw_tile: false,
    };
    assert!(context
        .restore_device_images_reader(&mut reader, Duration::from_secs(2))
        .is_err());
    assert!(reader.saw_tile);
    assert_eq!(context.object_count(), 0);
    assert_eq!(context.budget.used(), 0);
    assert_eq!(host.live_objects(), 0);
}
