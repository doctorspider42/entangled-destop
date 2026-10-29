use super::*;
use std::sync::Arc;

use super::super::{fake::FakeVulkan, harness as h, s1_tests as s1};
use crate::renderer::Renderer3d;

fn unused_shared_fixture(dedicated: bool) -> (h::Harness<FakeVulkan>, SharedHandle) {
    let host = s1::host_with(vec![s1::gpu("unused image GPU", 7)]);
    let mut factory = ExecutorFactory::new(host);
    factory
        .enable_checkpoint_recipes(&crate::venus::shmem::PageBudget::new(
            super::super::checkpoint::MAX_RECIPE_BYTES,
        ))
        .unwrap();
    let mut harness = h::Harness::gated_factory(factory, virtio_core::Quiesce::new());
    for ctx in [h::CTX, 2] {
        if ctx == 2 {
            harness.use_context(2);
        }
        s1::device_on(&mut harness, &[h::PHYSICAL], h::PHYSICAL, s1::S1_EXTENSIONS);
        if ctx == 2 {
            harness.renderer.ctx_attach_blob(2, 88, true);
        }
        harness
            .call(&h::create_image(h::DEVICE, h::IMAGE, s1::exporter_image()))
            .unwrap();
        let mut links = vec![if ctx == h::CTX {
            s1::dma_buf_export()
        } else {
            s1::import_of(88)
        }];
        if dedicated {
            links.push(s1::dedicated_to(h::IMAGE));
        }
        harness
            .call(&h::allocate(
                h::DEVICE,
                h::MEMORY,
                s1::PLANE,
                s1::DEVICE_LOCAL_TYPE,
                links,
            ))
            .unwrap();
        harness
            .call(&h::bind_image(h::DEVICE, h::IMAGE, h::MEMORY, 0))
            .unwrap();
        if ctx == h::CTX {
            harness
                .memory_blob(h::CTX, 88, h::MEMORY, s1::PLANE)
                .unwrap();
        }
    }
    let context = super::super::lock(&harness.renderer.factory().contexts[&h::CTX]);
    let handle = context
        .objects
        .memory(h::DEVICE, h::MEMORY)
        .unwrap()
        .shared_handle
        .clone()
        .unwrap();
    drop(context);
    (harness, handle)
}

#[test]
fn unused_shared_images_require_dedication_in_every_live_allocation() {
    for dedicated in [false, true] {
        let (harness, handle) = unused_shared_fixture(dedicated);
        let state = harness.renderer.factory().shared_checkpoint_state(&handle);
        assert_eq!(state.is_ok(), dedicated);
        if let Ok((_, state)) = state {
            assert_eq!(state, SharedImageState::Initial);
        }
    }
}

#[test]
fn unused_shared_proof_rejects_recycled_image_ids_and_defined_importers() {
    let (harness, handle) = unused_shared_fixture(true);
    let factory = harness.renderer.factory();
    assert!(factory.shared_checkpoint_state(&handle).is_ok());
    let mut context = super::super::lock(&factory.contexts[&h::CTX]);
    let memory = context.objects.memory_by_id_mut(h::MEMORY).unwrap();
    let original = memory.dedicated_identity.replace(Arc::new(()));
    drop(context);
    assert!(factory.shared_checkpoint_state(&handle).is_err());
    let mut context = super::super::lock(&factory.contexts[&h::CTX]);
    context
        .objects
        .memory_by_id_mut(h::MEMORY)
        .unwrap()
        .dedicated_identity = original;
    drop(context);
    assert!(factory.shared_checkpoint_state(&handle).is_ok());
    let mut importer = super::super::lock(&factory.contexts[&2]);
    importer
        .image_tracking
        .images
        .get_mut(&h::IMAGE)
        .unwrap()
        .spans[0]
        .cell = Cell {
        layout: 6,
        owner: Owner::Queue(0),
    };
    drop(importer);
    assert!(factory.shared_checkpoint_state(&handle).is_err());
    assert!(handle
        .0
        .downcast_ref::<super::super::memory::HandleExport<FakeVulkan>>()
        .is_some());
}

#[test]
fn initial_fallback_never_overrides_known_unreleased_work_or_tracking_failure() {
    let (harness, handle) = unused_shared_fixture(true);
    let factory = harness.renderer.factory();
    let (canonical, _) = factory.shared_checkpoint_state(&handle).unwrap();
    factory.payloads.ownership.record(
        SharedRef::of(&handle),
        factory.payloads.ownership.reserve_epoch(),
        canonical,
        None,
    );
    assert!(factory.shared_checkpoint_state(&handle).is_err());
    factory.payloads.ownership.invalidate("unknown shared work");
    assert!(factory.shared_checkpoint_state(&handle).is_err());
}

fn image() -> CanonicalImage {
    CanonicalImage {
        format: 44,
        flags: 8,
        usage: 3,
        width: 8,
        height: 8,
        view_formats: vec![44, 50],
    }
}

#[test]
fn delayed_older_commit_never_overwrites_newer_executed_ownership() {
    let ownership = SharedOwnership::default();
    assert_eq!(ownership.reserve_epoch(), 0);
    ownership.enable();
    let handle = SharedHandle(Arc::new(17u64));
    let first = ownership.reserve_epoch();
    let second = ownership.reserve_epoch();
    let a = ScanoutRelease {
        layout: 6,
        family: super::super::policy::QUEUE_FAMILY_FOREIGN,
    };
    let b = ScanoutRelease {
        layout: 1,
        family: super::super::policy::QUEUE_FAMILY_EXTERNAL,
    };
    ownership.record(SharedRef::of(&handle), second, image(), Some(b));
    ownership.record(SharedRef::of(&handle), first, image(), Some(a));
    assert_eq!(ownership.released(&handle).unwrap().1, b);
    // Several command buffers/barriers in one accepted submit apply in order.
    ownership.record(SharedRef::of(&handle), second, image(), Some(a));
    assert_eq!(ownership.released(&handle).unwrap().1, a);
    assert!(ownership.released(&SharedHandle(Arc::new(17u64))).is_err());
    ownership.record(
        SharedRef::of(&handle),
        ownership.reserve_epoch(),
        image(),
        None,
    );
    assert!(
        ownership.released(&handle).is_err(),
        "a later acquire does not retain an old release"
    );
}

#[test]
fn unordered_or_unrepresentable_work_invalidates_capture_without_losing_source_handles() {
    let ownership = SharedOwnership::default();
    ownership.enable();
    let handle = SharedHandle(Arc::new(5u64));
    ownership.record(
        SharedRef::of(&handle),
        ownership.reserve_epoch(),
        image(),
        Some(ScanoutRelease {
            layout: 6,
            family: super::super::policy::QUEUE_FAMILY_FOREIGN,
        }),
    );
    assert!(ownership.released(&handle).is_ok());
    ownership.invalidate("shared GPU work could not be ordered");
    ownership.record(
        SharedRef::of(&handle),
        ownership.reserve_epoch(),
        image(),
        Some(ScanoutRelease {
            layout: 6,
            family: super::super::policy::QUEUE_FAMILY_FOREIGN,
        }),
    );
    assert!(ownership.released(&handle).is_err());
    assert_eq!(*handle.0.downcast_ref::<u64>().unwrap(), 5);
    ownership.reset();
    assert!(
        ownership.released(&handle).is_err(),
        "a reset does not inherit the old session's release"
    );
    ownership.record(
        SharedRef::of(&handle),
        ownership.reserve_epoch(),
        image(),
        Some(ScanoutRelease {
            layout: 6,
            family: super::super::policy::QUEUE_FAMILY_FOREIGN,
        }),
    );
    assert!(
        ownership.released(&handle).is_ok(),
        "a new session can retain executed ownership again"
    );
}

#[test]
fn incompatible_canonical_aliases_cannot_be_saved_as_one_pixel_payload() {
    let ownership = SharedOwnership::default();
    ownership.enable();
    let handle = SharedHandle(Arc::new(19u64));
    let release = Some(ScanoutRelease {
        layout: 6,
        family: super::super::policy::QUEUE_FAMILY_FOREIGN,
    });
    ownership.record(
        SharedRef::of(&handle),
        ownership.reserve_epoch(),
        image(),
        release,
    );
    let mut alias = image();
    alias.width *= 2;
    ownership.record(
        SharedRef::of(&handle),
        ownership.reserve_epoch(),
        alias,
        release,
    );
    assert!(ownership.released(&handle).is_err());
    assert_eq!(*handle.0.downcast_ref::<u64>().unwrap(), 19);
}
