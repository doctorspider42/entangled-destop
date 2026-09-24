//! Stage S2b ("GNOME on the GPU") against the fake host: the Venus renderer
//! serves `SET_SCANOUT_BLOB` of its own blobs — a page blob read straight
//! out of our pages, a handle blob read through the renderer's scanout
//! device — with every refusal a guest can provoke, the barrier pair the
//! scanout device records around its copy, and teardown.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::error::CommandError;
use crate::protocol::Rect;
use crate::renderer::{Renderer3d, ScanoutBlobSpec};
use crate::venus::protocol::*;
use crate::venus::renderer::SinkFactory as _;
use crate::{FORMAT_B8G8R8A8_UNORM, FORMAT_B8G8R8X8_UNORM};

use super::fake::{texel, FakeBarrier, FakeVulkan};
use super::harness::*;
use super::modifier::DRM_FORMAT_MOD_LINEAR;
use super::policy::QUEUE_FAMILY_FOREIGN as FOREIGN;
use super::recording::*;
use super::s1_tests::{
    create, dedicated_to, device_on, dma_buf_export, export, exporter_image, gpu, host_with,
    import_of, importer_image, modifier_info, requirements, second_context, Named,
    DEVICE_LOCAL_TYPE, EXPORTED_MEM, EXPORTED_RES, EXPORTER, H, HOST_TYPE, IMPORTED_MEM, IMPORTER,
    PITCH, S1_EXTENSIONS, USAGE_COLOR, USAGE_SAMPLED, USAGE_TRANSFER, W,
};
use super::scanout::{LAYOUT_COLOR_ATTACHMENT, LAYOUT_GENERAL, LAYOUT_TRANSFER_SRC, SCANOUT_WAIT};

const STAGE2_COLOR_OUTPUT: u64 = 0x400;
const STAGE2_ALL_COMMANDS: u64 = 0x1_0000;
const ACCESS2_COLOR_WRITE: u64 = 0x100;

/// The spec Mutter's flip of the exporter's buffer sends: XRGB8888, the
/// framebuffer's extent, the pitch GBM reported, offset 0.
fn flip() -> ScanoutBlobSpec {
    ScanoutBlobSpec {
        format: FORMAT_B8G8R8X8_UNORM,
        width: W,
        height: H,
        stride: PITCH as u32,
        offset: 0,
    }
}

fn host() -> Arc<FakeVulkan> {
    host_with(vec![gpu("NVIDIA GeForce RTX 2070", 7)])
}

fn s1_harness() -> (Harness<FakeVulkan>, Arc<FakeVulkan>) {
    let host = host();
    let mut h = Harness::new(Arc::clone(&host));
    device_on(&mut h, &[PHYSICAL], PHYSICAL, S1_EXTENSIONS);
    (h, host)
}

/// Zink's end-of-batch release of `image` (`zink_batch.c:900-918`): no
/// layout change, its last access, `ALL_COMMANDS`, to `FOREIGN`.
fn zink_release(h: &mut Harness<FakeVulkan>, image: u64, layout: i32) {
    let outcome = h.submit_recording(&[
        begin(CB),
        image_barrier2_families(
            CB,
            image,
            (layout, layout),
            (STAGE2_COLOR_OUTPUT, ACCESS2_COLOR_WRITE),
            (STAGE2_ALL_COMMANDS, 0),
            (0, FOREIGN),
        ),
        end(CB),
    ]);
    assert_eq!(outcome, Outcome::Consumed);
    assert!(!h.fatal());
}

fn read(
    h: &mut Harness<FakeVulkan>,
    resource_id: u32,
    rect: Rect,
) -> Result<Vec<u8>, CommandError> {
    let mut out = vec![0xee; 3];
    h.renderer.read_rect_bgra(resource_id, rect, &mut out)?;
    Ok(out)
}

fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
    Rect {
        x,
        y,
        width,
        height,
    }
}

/// What the fake GPU's copy of `r` yields: its [`texel`]s, packed.
fn texels(r: Rect) -> Vec<u8> {
    (r.y..r.y + r.height)
        .flat_map(|y| (r.x..r.x + r.width).flat_map(move |x| texel(x, y)))
        .collect()
}

fn refusal(result: Result<(), CommandError>) -> String {
    match result {
        Err(CommandError::ScanoutLayout { reason, .. }) => reason,
        other => panic!("expected a scanout refusal, got {other:?}"),
    }
}

// ------------------------------------------------------------ page blobs

const PAGE_MEM: u64 = 0x700;
const PAGE_RES: u32 = 70;
const PAGE_BYTES: u64 = 64 << 10;

/// A host-visible allocation, its blob, and a pattern in its pages where
/// byte `i` is `i * 7 + 3`.
fn page_blob(h: &mut Harness<FakeVulkan>) -> Arc<crate::venus::shmem::RingPages> {
    h.send(&allocate(
        DEVICE,
        PAGE_MEM,
        PAGE_BYTES,
        HOST_TYPE,
        Vec::new(),
    ))
    .expect("host-visible memory");
    h.memory_blob(CTX, PAGE_RES, PAGE_MEM, PAGE_BYTES)
        .expect("a page blob");
    let pages = h.renderer.blob_pages(PAGE_RES).expect("its pages");
    let pattern: Vec<u8> = (0..PAGE_BYTES).map(|i| (i * 7 + 3) as u8).collect();
    pages.write_bytes(0, &pattern).unwrap();
    pages
}

#[test]
fn a_page_blob_is_accepted_exactly_when_the_image_fits_its_pages() {
    let (mut h, _) = s1_harness();
    let pages = page_blob(&mut h);
    let mapped = pages.mapped_len();
    assert_eq!(mapped, PAGE_BYTES);
    let spec = |width: u32, height: u32, stride: u32, offset: u32| ScanoutBlobSpec {
        format: FORMAT_B8G8R8A8_UNORM,
        width,
        height,
        stride,
        offset,
    };
    // `offset + stride × (height − 1) + width × 4` is the last byte: an image
    // whose last row ends exactly at the end is accepted, one byte more is
    // not — even though `stride × height` would pass the end.
    let exact = spec(16, 11, 6000, (mapped - 6000 * 10 - 64) as u32);
    h.renderer
        .scanout_blob(PAGE_RES, &exact)
        .expect("fits to the byte");
    let over = ScanoutBlobSpec {
        offset: exact.offset + 1,
        ..exact
    };
    assert!(refusal(h.renderer.scanout_blob(PAGE_RES, &over)).contains("does not fit"));
    for huge in [
        spec(u32::MAX / 4, 2, u32::MAX, 0),
        spec(1, u32::MAX, u32::MAX, u32::MAX),
        spec(4096, 4096, 16384, 0),
    ] {
        assert!(
            refusal(h.renderer.scanout_blob(PAGE_RES, &huge)).contains("does not fit"),
            "{huge:?}"
        );
    }
    // A refusal kept the accepted layout: the exact one still reads.
    let got = read(&mut h, PAGE_RES, rect(0, 10, 16, 1)).expect("the last row");
    let at = u64::from(exact.offset) + 6000 * 10;
    let want: Vec<u8> = (at..at + 64).map(|i| (i * 7 + 3) as u8).collect();
    assert_eq!(got, want);
    // A ring blob is not an image.
    assert!(refusal(h.renderer.scanout_blob(RING_RES, &exact)).contains("not an image"));
    // An unknown resource.
    assert!(matches!(
        h.renderer.scanout_blob(4242, &exact),
        Err(CommandError::UnknownResource(4242))
    ));
}

#[test]
fn a_page_blob_reads_exactly_the_rows_of_a_partial_rect_at_an_odd_stride() {
    let (mut h, _) = s1_harness();
    page_blob(&mut h);
    // An odd stride and offset, far from any power of two.
    let spec = ScanoutBlobSpec {
        format: FORMAT_B8G8R8X8_UNORM,
        width: 37,
        height: 23,
        stride: 37 * 4 + 13,
        offset: 29,
    };
    h.renderer.scanout_blob(PAGE_RES, &spec).expect("fits");
    let expect = |r: Rect| -> Vec<u8> {
        let mut out = Vec::new();
        for y in r.y..r.y + r.height {
            let at =
                u64::from(spec.offset) + u64::from(y) * u64::from(spec.stride) + u64::from(r.x) * 4;
            out.extend((at..at + u64::from(r.width) * 4).map(|i| (i * 7 + 3) as u8));
        }
        out
    };
    for r in [
        rect(0, 0, 37, 23),
        rect(5, 3, 7, 9),
        rect(36, 22, 1, 1),
        rect(1, 0, 36, 1),
    ] {
        let got = read(&mut h, PAGE_RES, r).expect("inside the image");
        assert_eq!(got.len() as u64, r.pixels() * 4, "{r:?}");
        assert_eq!(got, expect(r), "{r:?}");
    }
    // Outside the accepted image: refused, never read.
    for r in [rect(30, 0, 8, 1), rect(0, 23, 1, 1), rect(0, 0, 0, 1)] {
        assert!(read(&mut h, PAGE_RES, r).is_err(), "{r:?}");
    }
    // A blob never accepted has nothing to read.
    h.send(&allocate(DEVICE, 0x701, 4096, HOST_TYPE, Vec::new()))
        .unwrap();
    h.memory_blob(CTX, 71, 0x701, 4096).unwrap();
    assert!(matches!(
        read(&mut h, 71, rect(0, 0, 1, 1)),
        Err(CommandError::UnknownResource(71))
    ));
    // Unref forgets the layout with the blob.
    h.renderer.destroy_blob(PAGE_RES);
    assert!(read(&mut h, PAGE_RES, rect(0, 0, 1, 1)).is_err());
}

// ---------------------------------------------------------- handle blobs

#[test]
fn a_handle_blob_is_scanned_out_only_with_a_canonical_image_bound_to_its_memory() {
    // The blob before the bind (as Mesa: the blob is made inside the
    // allocation), and nothing bound yet.
    let (mut h, host) = s1_harness();
    assert_eq!(create(&mut h, EXPORTER, exporter_image()), VK_SUCCESS);
    let req = requirements(&mut h, EXPORTER);
    h.send(&allocate(
        DEVICE,
        EXPORTED_MEM,
        req.size,
        DEVICE_LOCAL_TYPE,
        vec![dma_buf_export(), dedicated_to(EXPORTER)],
    ))
    .unwrap();
    let blob = req.size.next_multiple_of(4096);
    h.memory_blob(CTX, EXPORTED_RES, EXPORTED_MEM, blob)
        .unwrap();
    assert!(refusal(h.renderer.scanout_blob(EXPORTED_RES, &flip()))
        .contains("no canonical DRM-modifier image"));
    assert_eq!(h.renderer.factory().scanout_targets(), 0);
    assert!(h.renderer.factory().scanout().is_none(), "nothing opened");
    // Bound now: the bind records the image on the blob.
    h.send(&bind_image(DEVICE, EXPORTER, EXPORTED_MEM, 0))
        .unwrap();
    h.renderer
        .scanout_blob(EXPORTED_RES, &flip())
        .expect("the canonical image matches");
    assert_eq!(h.renderer.factory().scanout_targets(), 1);
    // The scanout device imported the export and created exactly the
    // exporter's host image over it.
    let infos = host.image_infos();
    assert_eq!(
        infos[infos.len() - 1],
        infos[0],
        "the same canonical create info"
    );
    let imports = host.handle_imports();
    assert_eq!(imports.len(), 1);
    assert_eq!(
        imports[0].payload,
        host.exportable_allocations()[0].0,
        "the exporter's payload"
    );

    // The image goes: the record goes with it, and a new layout question
    // finds nothing.
    h.send(&destroy_image(DEVICE, EXPORTER)).unwrap();
    let other = ScanoutBlobSpec {
        format: FORMAT_B8G8R8A8_UNORM,
        ..flip()
    };
    assert!(refusal(h.renderer.scanout_blob(EXPORTED_RES, &other))
        .contains("no canonical DRM-modifier image"));

    // The other order — bound before the blob exists — records too.
    let (mut h, _) = s1_harness();
    export(&mut h);
    h.renderer
        .scanout_blob(EXPORTED_RES, &flip())
        .expect("recorded when the blob was made");
}

#[test]
fn a_layout_that_is_not_the_bound_image_is_refused_and_keeps_the_last_one() {
    let (mut h, _) = s1_harness();
    export(&mut h);
    h.renderer.scanout_blob(EXPORTED_RES, &flip()).unwrap();
    for (spec, why) in [
        (
            ScanoutBlobSpec {
                stride: PITCH as u32 + 256,
                ..flip()
            },
            "pitch",
        ),
        (
            ScanoutBlobSpec {
                stride: W * 4 + 4,
                ..flip()
            },
            "pitch",
        ),
        (
            ScanoutBlobSpec {
                offset: 256,
                ..flip()
            },
            "offset",
        ),
        (
            ScanoutBlobSpec {
                height: H - 1,
                ..flip()
            },
            "is 1920x1080",
        ),
        (
            ScanoutBlobSpec {
                width: 1280,
                ..flip()
            },
            "is 1920x1080",
        ),
        (
            ScanoutBlobSpec {
                format: 3,
                ..flip()
            },
            "not a BGRA one",
        ),
    ] {
        let reason = refusal(h.renderer.scanout_blob(EXPORTED_RES, &spec));
        assert!(reason.contains(why), "{spec:?}: {reason}");
    }
    assert_eq!(
        h.renderer.factory().scanout_targets(),
        1,
        "the old import kept"
    );

    // An RGBA-ordered canonical image under a BGRA scanout: refused rather
    // than swizzled.
    let (mut h, _) = s1_harness();
    let rgba = modifier_info(
        RGBA8,
        USAGE_COLOR | USAGE_SAMPLED | USAGE_TRANSFER,
        0x8,
        Some(vec![RGBA8, 43]),
        Named::List(vec![DRM_FORMAT_MOD_LINEAR]),
    );
    assert_eq!(create(&mut h, EXPORTER, rgba), VK_SUCCESS);
    let req = requirements(&mut h, EXPORTER);
    h.send(&allocate(
        DEVICE,
        EXPORTED_MEM,
        req.size,
        DEVICE_LOCAL_TYPE,
        vec![dma_buf_export(), dedicated_to(EXPORTER)],
    ))
    .unwrap();
    h.send(&bind_image(DEVICE, EXPORTER, EXPORTED_MEM, 0))
        .unwrap();
    h.memory_blob(
        CTX,
        EXPORTED_RES,
        EXPORTED_MEM,
        req.size.next_multiple_of(4096),
    )
    .unwrap();
    assert!(refusal(h.renderer.scanout_blob(EXPORTED_RES, &flip())).contains("not B8G8R8A8"));
}

#[test]
fn a_handle_blob_is_read_back_between_an_acquire_and_a_release_matching_the_guests() {
    let (mut h, host) = s1_harness();
    export(&mut h);
    h.renderer.scanout_blob(EXPORTED_RES, &flip()).unwrap();
    // Nothing released yet: there is no frame to acquire.
    let err = read(&mut h, EXPORTED_RES, rect(0, 0, 4, 4)).expect_err("no release");
    assert!(err.to_string().contains("not released"), "{err}");

    // Zink's release, left as a colour attachment.
    zink_release(&mut h, EXPORTER, LAYOUT_COLOR_ATTACHMENT);
    let before = host.image_barriers().len();
    let r = rect(100, 50, 16, 8);
    let got = read(&mut h, EXPORTED_RES, r).expect("read back");
    assert_eq!(got, texels(r), "exactly the rect, packed");

    let barriers = host.image_barriers();
    let ours = &barriers[before..];
    assert_eq!(ours.len(), 2, "one acquire and one release");
    let family = h.renderer.factory().scanout().unwrap().family();
    let image = ours[0].image;
    assert_ne!(image, 0);
    assert_eq!(
        ours[0],
        FakeBarrier {
            image,
            layouts: (LAYOUT_COLOR_ATTACHMENT, LAYOUT_TRANSFER_SRC),
            families: (FOREIGN, family),
            src: (0x1, 0),        // TOP_OF_PIPE
            dst: (0x1000, 0x800), // TRANSFER, TRANSFER_READ
        },
        "acquired from FOREIGN, from the layout the guest released in"
    );
    assert_eq!(
        ours[1],
        FakeBarrier {
            image,
            layouts: (LAYOUT_TRANSFER_SRC, LAYOUT_COLOR_ATTACHMENT),
            families: (family, FOREIGN),
            src: (0x1000, 0),
            dst: (0x2000 | 0x4000, 0), // BOTTOM_OF_PIPE | HOST (the buffer's)
        },
        "released back to FOREIGN in the guest's own layout"
    );
    assert_ne!(
        ours[0].layouts.0, 0,
        "never UNDEFINED: that would discard the frame"
    );

    // A second frame, released GENERAL (Zink's `general_layout` drivers):
    // no transition at all, and the same import.
    let images = host.image_requests();
    zink_release(&mut h, EXPORTER, LAYOUT_GENERAL);
    let before = host.image_barriers().len();
    let r = rect(0, 0, W, 2);
    assert_eq!(read(&mut h, EXPORTED_RES, r).unwrap(), texels(r));
    let ours = &host.image_barriers()[before..];
    assert_eq!(ours[0].layouts, (LAYOUT_GENERAL, LAYOUT_GENERAL));
    assert_eq!(ours[1].layouts, (LAYOUT_GENERAL, LAYOUT_GENERAL));
    assert_eq!(ours[0].families, (FOREIGN, family));
    assert_eq!(ours[1].families, (family, FOREIGN));
    assert_eq!(host.image_requests(), images, "the import is reused");

    // A release in a layout nothing may acquire from is refused, and the
    // GPU is never asked.
    zink_release(&mut h, EXPORTER, 0);
    let before = host.image_barriers().len();
    let err = read(&mut h, EXPORTED_RES, rect(0, 0, 1, 1)).expect_err("UNDEFINED");
    assert!(err.to_string().contains("layout 0"), "{err}");
    assert_eq!(host.image_barriers().len(), before);
}

#[test]
fn an_importers_image_is_on_record_too_and_outlives_the_exporters() {
    let (mut h, _) = s1_harness();
    let blob = export(&mut h);
    second_context(&mut h, &[PHYSICAL], PHYSICAL);
    h.renderer.ctx_attach_blob(2, EXPORTED_RES, true);
    assert_eq!(create(&mut h, IMPORTER, importer_image(PITCH)), VK_SUCCESS);
    h.send(&allocate(
        DEVICE,
        IMPORTED_MEM,
        blob,
        DEVICE_LOCAL_TYPE,
        vec![import_of(EXPORTED_RES)],
    ))
    .unwrap();
    h.send(&bind_image(DEVICE, IMPORTER, IMPORTED_MEM, 0))
        .unwrap();
    // The exporting context goes, and with it its image's record.
    h.renderer.ctx_destroy(CTX);
    h.renderer
        .scanout_blob(EXPORTED_RES, &flip())
        .expect("the importer's canonical image");
    // The importer's release is the one acquired from.
    zink_release(&mut h, IMPORTER, LAYOUT_TRANSFER_SRC);
    let r = rect(7, 9, 3, 2);
    assert_eq!(read(&mut h, EXPORTED_RES, r).unwrap(), texels(r));
}

#[test]
fn unref_a_new_layout_and_a_reset_drop_the_scanout_devices_imports() {
    let (mut h, host) = s1_harness();
    export(&mut h);
    h.renderer.scanout_blob(EXPORTED_RES, &flip()).unwrap();
    zink_release(&mut h, EXPORTER, LAYOUT_COLOR_ATTACHMENT);
    read(&mut h, EXPORTED_RES, rect(0, 0, 8, 8)).unwrap();
    let images = host.live("image");
    let memories = host.live("memory");

    // Unref: the import, its image and its staging go.
    h.renderer.destroy_blob(EXPORTED_RES);
    assert_eq!(h.renderer.factory().scanout_targets(), 0);
    assert_eq!(host.live("image"), images - 1);
    assert_eq!(host.live("memory"), memories - 2);
    assert_eq!(host.live("buffer"), 0);
    assert_eq!(
        host.live_shared_handles(),
        0,
        "no import holds the handle open"
    );

    // A reset: everything goes but the scanout device itself.
    let (mut h, host) = s1_harness();
    export(&mut h);
    h.renderer.scanout_blob(EXPORTED_RES, &flip()).unwrap();
    assert_eq!(h.renderer.factory().scanout_targets(), 1);
    h.renderer.reset();
    assert_eq!(h.renderer.factory().scanout_targets(), 0);
    assert!(
        h.renderer.factory().scanout().is_some(),
        "the device is kept"
    );
    assert_eq!(host.live("image"), 0);
    assert_eq!(host.live("memory"), 0);
    assert_eq!(host.live("buffer"), 0);
    assert_eq!(host.live("device"), 1, "only the scanout device");
    assert_eq!(host.live("instance"), 1);
    assert_eq!(host.live_shared_handles(), 0);
    // Dropping the renderer takes the device too.
    drop(h);
    assert_eq!(host.live_objects(), 0, "nothing left on the host");
}

#[test]
fn a_gpu_that_does_not_finish_or_is_lost_costs_the_flush_and_nothing_else() {
    let (mut h, host) = s1_harness();
    export(&mut h);
    h.renderer.scanout_blob(EXPORTED_RES, &flip()).unwrap();
    zink_release(&mut h, EXPORTER, LAYOUT_COLOR_ATTACHMENT);
    // A GPU that never finishes: the wait is bounded.
    host.stuck.store(true, Ordering::SeqCst);
    let start = std::time::Instant::now();
    let err = read(&mut h, EXPORTED_RES, rect(0, 0, 2, 2)).expect_err("timed out");
    assert!(err.to_string().contains("did not finish"), "{err}");
    assert!(start.elapsed() < SCANOUT_WAIT * 10);
    // It recovers once the GPU does.
    host.stuck.store(false, Ordering::SeqCst);
    let r = rect(1, 1, 2, 2);
    assert_eq!(read(&mut h, EXPORTED_RES, r).unwrap(), texels(r));
    // A lost device: the flush fails, the device is dropped, and the next
    // read makes a new one.
    host.lost.store(true, Ordering::SeqCst);
    let err = read(&mut h, EXPORTED_RES, r).expect_err("lost");
    assert!(err.to_string().contains("lost"), "{err}");
    assert!(h.renderer.factory().scanout().is_none());
    host.lost.store(false, Ordering::SeqCst);
    assert_eq!(read(&mut h, EXPORTED_RES, r).unwrap(), texels(r));
    assert!(h.renderer.factory().scanout().is_some());
}
