use super::*;
use crate::renderer::Renderer3d;
use crate::venus::executor::{fake::FakeVulkan, harness as h, s1_tests as s1, ExecutorFactory};

#[test]
fn owner_manifest_checks_actual_allocation_geometry_and_exact_charges() {
    let host = s1::host_with(vec![s1::gpu("payload owners", 7)]);
    let mut source = h::Harness::new(host.clone());
    s1::device_on(&mut source, &[h::PHYSICAL], h::PHYSICAL, s1::S1_EXTENSIONS);
    s1::export(&mut source);
    let mut payloads = SharedPayloads::default();
    let memory = source
        .renderer
        .blob_directory()
        .for_context(h::CTX)
        .memory(s1::EXPORTED_RES)
        .unwrap();
    let crate::venus::renderer::ExportedMemory::Handle(handle) = memory.backing else {
        panic!()
    };
    payloads.register(&handle).unwrap();
    let mut budgets = source.renderer.factory().checkpoint_budget_table().unwrap();
    let owners = payloads
        .checkpoint_budget_owners::<FakeVulkan>(&mut budgets)
        .unwrap();
    let bytes = owners.encode(&budgets).unwrap();
    let decoded = PayloadBudgetOwners::decode(&bytes, &budgets).unwrap();
    assert_eq!(decoded.len(), 1);
    assert!(!decoded.is_empty());
    assert_eq!(decoded.encode(&budgets).unwrap(), bytes);
    decoded
        .validate_handles::<FakeVulkan>(&payloads, &budgets)
        .unwrap();
    assert!(decoded.owner(0).is_err());
    assert!(decoded.owner(2).is_err());
    assert!(decoded
        .validate_handles::<FakeVulkan>(&SharedPayloads::default(), &budgets)
        .is_err());
    // Equal policy and identical numeric owner ids in another table do not
    // authorise reusing a live old-process charge.
    let foreign_factory = ExecutorFactory::new(host.clone());
    let foreign = foreign_factory
        .restore_budget_table(&budgets.encode().unwrap())
        .unwrap();
    assert!(decoded
        .validate_handles::<FakeVulkan>(&payloads, &foreign)
        .is_err());
    for field in 0..4 {
        let mut wrong = decoded.clone();
        match field {
            0 => wrong.records[0].size += 4096,
            1 => wrong.records[0].type_index ^= 1,
            2 => wrong.records[0].uuids.1[0] ^= 1,
            _ => {
                wrong.records[0].owner =
                    budgets.register(&source.renderer.factory().budget).unwrap()
            }
        }
        assert!(wrong
            .validate_handles::<FakeVulkan>(&payloads, &budgets)
            .is_err());
    }
    for len in 0..bytes.len() {
        assert!(PayloadBudgetOwners::decode(&bytes[..len], &budgets).is_err());
    }
    for (at, value) in [(0, 2_u32), (4, u32::MAX), (8, 0), (12, 0), (20, 32)] {
        let mut bad = bytes.clone();
        bad[at..at + 4].copy_from_slice(&value.to_le_bytes());
        assert!(
            PayloadBudgetOwners::decode(&bad, &budgets).is_err(),
            "field {at}"
        );
    }
    let mut trailing = bytes;
    trailing.push(0);
    assert!(PayloadBudgetOwners::decode(&trailing, &budgets).is_err());
    source.renderer.reset();
}
