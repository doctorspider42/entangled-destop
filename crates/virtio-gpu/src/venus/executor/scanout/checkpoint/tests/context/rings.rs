//! Resume native contexts through the actual factory and renderer, appending
//! replies at the original window cursor without rebinding it.
use std::time::Instant;

use super::*;
use crate::venus::pump::{RingBacking, RingPump, STATUS_FATAL};
use crate::venus::renderer::checkpoint_rings::RingCheckpointSet;
use crate::venus::renderer::VenusRenderer;
use crate::venus::ring::RingLayout;

pub(super) fn exercise(
    directory: &Path,
    renderer: &mut VenusRenderer<crate::venus::executor::ExecutorFactory<AshVulkan>>,
    pages: &HostPageTable,
    gate: &virtio_core::Quiesce,
    already_restored: bool,
) {
    let saved =
        RingCheckpointSet::decode(&std::fs::read(directory.join("vm-rings")).unwrap()).unwrap();
    exercise_saved(saved, renderer, pages, gate, already_restored);
}

pub(super) fn exercise_saved(
    saved: RingCheckpointSet,
    renderer: &mut VenusRenderer<crate::venus::executor::ExecutorFactory<AshVulkan>>,
    pages: &HostPageTable,
    gate: &virtio_core::Quiesce,
    already_restored: bool,
) {
    let blobs = renderer.blob_directory().clone();
    let mut expected = Vec::new();
    for context in &saved.contexts {
        for saved in &context.rings {
            let page = pages.page(saved.page).unwrap().clone();
            let layout = RingLayout::new(saved.info, page.resource_len()).unwrap();
            let head = if already_restored {
                renderer.ring_state(context.ctx_id, saved.ring).unwrap().0
            } else {
                RingPump::from_checkpoint(layout, page.as_ref(), &saved.worker.pump)
                    .unwrap()
                    .cursor()
            };
            assert_eq!(page.load_guest_word(&layout.tail()), head);
            assert_eq!(saved.seqno, 100 + u64::from(context.ctx_id));
            // Parse the fixed record only after restore_rings validated it.
            let record = &saved.worker.sink;
            let resource = u32::from_le_bytes(record[20..24].try_into().unwrap());
            let offset = u64::from_le_bytes(record[32..40].try_into().unwrap());
            let size = u64::from_le_bytes(record[40..48].try_into().unwrap());
            let cursor = u64::from_le_bytes(record[48..56].try_into().unwrap());
            assert!(cursor > 0);
            let prefix = blobs
                .for_context(context.ctx_id)
                .read(resource, offset, cursor)
                .unwrap();
            let bytes = h::call_bytes(&h::enumerate_instance_version());
            for (i, byte) in bytes.iter().enumerate() {
                let at = head.wrapping_add(i as u32) % layout.buffer().len() as u32;
                page.write_bytes(layout.buffer().start() + u64::from(at), &[*byte])
                    .unwrap();
            }
            let tail = head.wrapping_add(bytes.len() as u32);
            assert!(page.guest_store_word(layout.tail().offset(), tail));
            expected.push((
                context.ctx_id,
                saved.ring,
                page,
                layout,
                tail,
                resource,
                offset,
                size,
                cursor,
                prefix,
            ));
        }
    }
    assert_eq!(expected.len(), 2);
    if !already_restored {
        renderer.restore_rings(&saved, pages).unwrap();
    }
    gate.resume();
    for (ctx, _, page, layout, tail, resource, offset, size, cursor, prefix) in &expected {
        let deadline = Instant::now() + WAIT;
        while page.load_host_word(&layout.head()) != *tail {
            assert_eq!(page.load_host_word(&layout.status()) & STATUS_FATAL, 0);
            assert!(
                Instant::now() < deadline,
                "restored renderer ring did not answer"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        let reply = blobs
            .for_context(*ctx)
            .read(*resource, *offset, *size)
            .unwrap();
        assert_eq!(&reply[..*cursor as usize], prefix.as_slice());
        let encoded = h::call_bytes(&h::enumerate_instance_version());
        let mut command = Command::decode_next(&mut Decoder::new(&encoded)).unwrap().1;
        command
            .decode_reply(&mut Decoder::new(&reply[*cursor as usize..]))
            .unwrap();
        let Command::EnumerateInstanceVersion(args) = &command else {
            panic!()
        };
        assert_eq!(args.ret, VK_SUCCESS);
        assert!(args.p_api_version.is_some_and(|version| version > 0));
    }
    gate.pause();
    assert!(gate.wait_until_idle(WAIT));
    let resumed = renderer
        .checkpoint_rings(&mut HostPageTable::default(), WAIT)
        .unwrap();
    for (ctx, ring, page, layout, tail, _, _, _, cursor, _) in expected {
        let context = resumed
            .contexts
            .iter()
            .find(|context| context.ctx_id == ctx)
            .unwrap();
        let saved = context
            .rings
            .iter()
            .find(|saved| saved.ring == ring)
            .unwrap();
        let pump = RingPump::from_checkpoint(layout, page.as_ref(), &saved.worker.pump).unwrap();
        assert_eq!(pump.cursor(), tail);
        assert_eq!(saved.seqno, 100 + u64::from(ctx));
        let advanced = u64::from_le_bytes(saved.worker.sink[48..56].try_into().unwrap());
        assert!(
            advanced > cursor,
            "the next reply advanced the original window"
        );
    }
    renderer.reset();
    assert_eq!(renderer.live_threads(), 0);
}
