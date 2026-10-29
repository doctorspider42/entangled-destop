//! Pending layout-changing ownership transfers survive native process exit.
use super::*;
use crate::venus::executor::checkpoint_ownership::SharedImageState;

pub(super) fn state() -> SharedImageState {
    SharedImageState::ReleasedTransition {
        old_layout: 2,
        release: ScanoutRelease {
            layout: 1,
            family: policy::QUEUE_FAMILY_FOREIGN,
        },
    }
}

pub(super) fn check(
    renderer: &mut crate::venus::renderer::VenusRenderer<
        crate::venus::executor::ExecutorFactory<AshVulkan>,
    >,
    pixel: [u8; 4],
    expected: SharedImageState,
) {
    let graph = renderer.checkpoint_object_graph(WAIT).unwrap();
    let (target, state) = &graph.shared_images[0];
    assert_eq!(*state, expected);
    struct DiskError;
    impl Write for DiskError {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("injected write failure"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    assert!(renderer
        .factory_mut_for_test()
        .save_checkpoint_shared_image(target, *state, &mut DiskError)
        .is_err());
    // A failed disk write must leave the exact transfer and all pixels intact.
    let mut bytes = Vec::new();
    renderer
        .factory_mut_for_test()
        .save_checkpoint_shared_image(target, *state, &mut bytes)
        .unwrap();
    let record = parse(&bytes).unwrap();
    assert_eq!(record.state, expected);
    assert!(!record.pixels.is_empty());
    assert!(record.pixels.chunks_exact(4).all(|p| p == pixel));
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
            &mut std::fs::File::open(directory.join("transition-archive")).unwrap(),
            &PageBudget::new(MAX_RECIPE_BYTES),
            WAIT,
        )
        .unwrap();
    check(&mut renderer, [0, 255, 0, 255], state());
    renderer
        .save_checkpoint_archive(&mut Vec::new(), WAIT)
        .unwrap();
    let factory = renderer.factory_mut_for_test();
    let mut context = crate::venus::executor::lock(factory.contexts.get(&h::CTX).unwrap());
    for command in [
        Command::ResetCommandBuffer(ResetCommandBufferArgs {
            command_buffer: VkCommandBuffer(CB),
            flags: 0,
            ret: VK_ERROR_UNKNOWN,
        }),
        r::begin(CB),
        // The resumed guest acquires using the original distinct layout pair.
        barrier((2, 1), (policy::QUEUE_FAMILY_FOREIGN, 0), (0, 0x1000)),
        barrier((1, 7), (u32::MAX, u32::MAX), (0, 0x1000)),
        clear([0.0, 0.0, 1.0, 1.0]),
        barrier((7, 1), (u32::MAX, u32::MAX), (0x1000, 0)),
        barrier((1, 1), (0, policy::QUEUE_FAMILY_FOREIGN), (0, 0)),
        r::end(CB),
        r::reset_fences(h::DEVICE, &[FENCE]),
        r::queue_submit(h::QUEUE, &[CB], FENCE),
        r::wait_fences(h::DEVICE, &[FENCE], 2_000_000_000),
    ] {
        run(&mut context, command);
    }
    drop(context);
    check(
        &mut renderer,
        [255, 0, 0, 255],
        SharedImageState::Released(ScanoutRelease {
            layout: 1,
            family: policy::QUEUE_FAMILY_FOREIGN,
        }),
    );
}
