use super::*;
use crate::venus::renderer::{CaptureSink, SinkFactory, VenusRenderer};

fn source() -> Gpu3d {
    let mut source = Gpu3d::new(Box::new(crate::NullRenderer::with_venus()));
    source.ctx_create(7, crate::CAPSET_VENUS, "second").unwrap();
    source.ctx_create(2, crate::CAPSET_VENUS, "first").unwrap();
    source
}

struct Factory;
impl SinkFactory for Factory {
    type Sink = CaptureSink;
    fn sink_for(&mut self, _: u32, _: u64) -> std::io::Result<CaptureSink> {
        Ok(CaptureSink::new())
    }
}

#[test]
fn restoring_frontend_contexts_does_not_recreate_native_contexts() {
    let source = source();
    let saved = source.checkpoint_contexts().unwrap();
    let bytes = saved.encode().unwrap();
    let saved = ContextCheckpoint::decode(&bytes).unwrap();
    assert_eq!(
        saved.contexts().collect::<Vec<_>>(),
        [(2, crate::CAPSET_VENUS), (7, crate::CAPSET_VENUS)]
    );
    let mut native = VenusRenderer::new(Factory);
    for (id, capset) in saved.contexts() {
        native.ctx_create(id, capset, "restored").unwrap();
        assert!(native.ctx_create(id, capset, "duplicate").is_err());
    }
    let mut cold = Gpu3d::new(Box::new(native));
    cold.restore_checkpoint_contexts(&saved).unwrap();
    assert_eq!(cold.context_count(), 2);
    assert_eq!(cold.checkpoint_contexts().unwrap().encode().unwrap(), bytes);
    assert!(cold
        .ctx_create(2, crate::CAPSET_VENUS, "duplicate")
        .is_err());
    cold.ctx_destroy(2).unwrap();
    cold.ctx_create(2, crate::CAPSET_VENUS, "reused").unwrap();
    assert!(cold.restore_checkpoint_contexts(&saved).is_err());
    assert_eq!(cold.context_count(), 2);
    let mut incompatible = Gpu3d::new(Box::new(crate::NullRenderer::new()));
    assert!(incompatible.restore_checkpoint_contexts(&saved).is_err());
    assert_eq!(incompatible.context_count(), 0);
}

#[test]
fn malformed_context_tables_refuse_before_adoption_and_classic_state_is_not_lost() {
    let mut source = source();
    let bytes = source.checkpoint_contexts().unwrap().encode().unwrap();
    for len in 0..bytes.len() {
        assert!(ContextCheckpoint::decode(&bytes[..len]).is_err());
    }
    for (offset, value) in [(0, 2), (4, u32::MAX), (8, 0), (12, 0), (16, 2), (20, 2)] {
        let mut bad = bytes.clone();
        bad[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        assert!(
            ContextCheckpoint::decode(&bad).is_err(),
            "bad field {offset}"
        );
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(ContextCheckpoint::decode(&trailing).is_err());
    assert_eq!(
        source.checkpoint_contexts().unwrap().encode().unwrap(),
        bytes
    );
    source.ctx_create(9, 0, "classic").unwrap();
    assert!(source.checkpoint_contexts().is_err());
    source.ctx_destroy(9).unwrap();
    source.resources.insert(
        1,
        TrackedResource {
            desc: Resource3dDesc {
                target: 2,
                format: 1,
                width: 1,
                height: 1,
                depth: 1,
                array_size: 1,
                last_level: 0,
            },
            backing_len: 0,
        },
    );
    assert!(source.checkpoint_contexts().is_err());
    assert!(source.resources.contains_key(&1));
}
