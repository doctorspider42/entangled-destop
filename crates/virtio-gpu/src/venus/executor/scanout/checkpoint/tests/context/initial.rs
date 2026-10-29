//! The unused dedicated-image case must cross an actual process boundary.
use super::*;
use crate::venus::executor::checkpoint_ownership::SharedImageState;
use crate::venus::renderer::SinkFactory;

pub(in super::super) fn save(
    host: Arc<AshVulkan>,
    arena: &ScanoutDevice<AshVulkan>,
    directory: &Path,
) {
    save_variant(host, arena, directory, false);
}

pub(in super::super) fn save_variant(
    host: Arc<AshVulkan>,
    arena: &ScanoutDevice<AshVulkan>,
    directory: &Path,
    transition: bool,
) {
    let template = super::super::seed(arena, 6);
    let record = parse(&template).unwrap();
    let gate = virtio_core::Quiesce::new();
    let mut factory = crate::venus::executor::ExecutorFactory::new(host);
    factory
        .enable_checkpoint_recipes(&PageBudget::new(MAX_RECIPE_BYTES))
        .unwrap();
    let mut harness = h::Harness::gated_factory(factory, gate.clone());
    start(&mut harness);
    let mut image = s1::exporter_image();
    image.extent.width = record.image.width;
    image.extent.height = record.image.height;
    for command in [
        h::create_image(h::DEVICE, h::IMAGE, image.clone()),
        h::allocate(
            h::DEVICE,
            h::MEMORY,
            record.size,
            record.type_index,
            vec![s1::dma_buf_export(), s1::dedicated_to(h::IMAGE)],
        ),
        h::bind_image(h::DEVICE, h::IMAGE, h::MEMORY, 0),
    ] {
        harness.call(&command).unwrap();
    }
    harness
        .memory_blob(h::CTX, RESOURCE, h::MEMORY, record.size)
        .unwrap();
    harness.use_context(2);
    start(&mut harness);
    harness.renderer.ctx_attach_blob(2, RESOURCE, true);
    for command in [
        h::create_image(h::DEVICE, h::IMAGE, image),
        h::allocate(
            h::DEVICE,
            h::MEMORY,
            record.size,
            record.type_index,
            vec![s1::import_of(RESOURCE), s1::dedicated_to(h::IMAGE)],
        ),
        h::bind_image(h::DEVICE, h::IMAGE, h::MEMORY, 0),
    ] {
        harness.call(&command).unwrap();
    }
    if transition {
        harness.use_context(h::CTX);
        for command in [
            r::allocate_cbs(h::DEVICE, h::POOL, &[CB], false),
            r::begin(CB),
            barrier((0, 7), (u32::MAX, u32::MAX), (0, 0x1000)),
            clear([0.0, 1.0, 0.0, 1.0]),
            barrier((7, 2), (u32::MAX, u32::MAX), (0x1000, 0x180)),
            barrier((2, 1), (0, policy::QUEUE_FAMILY_FOREIGN), (0x180, 0)),
            r::end(CB),
            r::create_fence(h::DEVICE, FENCE, false),
            r::queue_submit(h::QUEUE, &[CB], FENCE),
            r::wait_fences(h::DEVICE, &[FENCE], 2_000_000_000),
        ] {
            harness.call(&command).unwrap();
        }
    }
    gate.pause();
    assert!(gate.wait_until_idle(WAIT));
    let graph = harness.renderer.checkpoint_object_graph(WAIT).unwrap();
    assert_eq!(graph.shared_images.len(), 1);
    let expected = if transition {
        super::transition::state()
    } else {
        SharedImageState::Initial
    };
    assert_eq!(graph.shared_images[0].1, expected);
    if transition {
        super::transition::check(&mut harness.renderer, [0, 255, 0, 255], expected);
    }
    harness
        .renderer
        .save_checkpoint_archive(
            &mut std::fs::File::create(directory.join(if transition {
                "transition-archive"
            } else {
                "initial-archive"
            }))
            .unwrap(),
            WAIT,
        )
        .unwrap();
}

pub(in super::super) fn restore(host: Arc<AshVulkan>, directory: &Path) {
    let gate = virtio_core::Quiesce::new();
    gate.pause();
    let mut renderer = crate::venus::renderer::VenusRenderer::new(
        crate::venus::executor::ExecutorFactory::new(host),
    );
    renderer.set_quiesce(gate);
    renderer.set_host_visible(Arc::new(h::RecordingWindow::default()));
    renderer
        .load_checkpoint_archive(
            &mut std::fs::File::open(directory.join("initial-archive")).unwrap(),
            &PageBudget::new(MAX_RECIPE_BYTES),
            WAIT,
        )
        .unwrap();
    let graph = renderer.checkpoint_object_graph(WAIT).unwrap();
    let (target, state) = &graph.shared_images[0];
    assert_eq!(*state, SharedImageState::Initial);
    let factory = renderer.factory_mut_for_test();
    // Neither hostile scanout metadata nor direct factory entry points may
    // read/present an allocation that has no defined pixels.
    assert!(factory.prepare_scanout(target).is_err());
    assert!(factory.share_scanout(target).is_err());
    assert!(factory.claim_scanout(target).is_err());
    let release = ScanoutRelease {
        layout: 6,
        family: policy::QUEUE_FAMILY_FOREIGN,
    };
    assert!(factory
        .read_scanout(
            target,
            release,
            Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1
            },
            &mut Vec::new()
        )
        .is_err());
    // A second capture must preserve Initial rather than fabricate pixels.
    renderer
        .save_checkpoint_archive(&mut Vec::new(), WAIT)
        .unwrap();
    let factory = renderer.factory_mut_for_test();
    let mut context = crate::venus::executor::lock(factory.contexts.get(&h::CTX).unwrap());
    for command in [
        r::allocate_cbs(h::DEVICE, h::POOL, &[CB], false),
        r::begin(CB),
        barrier((0, 7), (u32::MAX, u32::MAX), (0, 0x1000)),
        clear([0.0, 1.0, 0.0, 1.0]),
        barrier((7, 6), (u32::MAX, u32::MAX), (0x1000, 0x800)),
        barrier((6, 6), (0, policy::QUEUE_FAMILY_FOREIGN), (0x800, 0)),
        r::end(CB),
        r::create_fence(h::DEVICE, FENCE, false),
        r::queue_submit(h::QUEUE, &[CB], FENCE),
        r::wait_fences(h::DEVICE, &[FENCE], 2_000_000_000),
    ] {
        run(&mut context, command);
    }
    drop(context);
    // Run the real capture drain: a signalled guest fence alone does not retire
    // the executor's shared-payload completion claims.
    let graph = renderer.checkpoint_object_graph(WAIT).unwrap();
    let (target, state) = &graph.shared_images[0];
    assert_eq!(*state, SharedImageState::Released(release));
    let mut bytes = Vec::new();
    renderer
        .factory_mut_for_test()
        .save_checkpoint_shared_image(target, *state, &mut bytes)
        .unwrap();
    assert!(parse(&bytes)
        .unwrap()
        .pixels
        .chunks_exact(4)
        .all(|p| p == [0, 255, 0, 255]));
}
