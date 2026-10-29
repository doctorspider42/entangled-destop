use super::*;
use crate::venus::executor::checkpoint_budgets::BudgetRoot;

fn root(whole: &Arc<PageBudget>) -> BudgetRoot {
    BudgetRoot {
        key: BudgetRootKey::HostPages,
        whole: whole.clone(),
        share_limit: 32768,
    }
}

#[test]
fn page_owners_charge_the_original_shares_once_and_preserve_anonymous_ring_pages() {
    let whole = PageBudget::new(65536);
    let first = PageBudget::share(&whole, 32768);
    let second = PageBudget::share(&whole, 32768);
    let a = Arc::new(RingPages::for_memory(4096, page_size() as u64, &first).unwrap());
    let b = Arc::new(RingPages::for_memory(8192, page_size() as u64, &first).unwrap());
    let c = Arc::new(RingPages::for_memory(4096, page_size() as u64, &second).unwrap());
    let ring = Arc::new(RingPages::new(17).unwrap());
    let first_bytes = a.mapped_len() + b.mapped_len();
    let second_bytes = c.mapped_len();
    ring.write_mapped_bytes(ring.mapped_len() - 1, &[0x77])
        .unwrap();
    let mut source = HostPageTable::default();
    for pages in [&a, &b, &c, &ring, &a] {
        source.register(pages).unwrap();
    }
    let mut budgets = BudgetTable::new(vec![root(&whole)]).unwrap();
    let owners = source.checkpoint_budget_owners(&mut budgets).unwrap();
    assert_eq!(owners.len(), 4);
    assert_eq!(owners.owner(1).unwrap(), owners.owner(2).unwrap());
    assert_ne!(owners.owner(1).unwrap(), owners.owner(3).unwrap());
    assert_eq!(owners.owner(4).unwrap(), 0);
    let owner_bytes = owners.encode(&budgets).unwrap();
    let budget_bytes = budgets.encode().unwrap();
    let mut data = Vec::new();
    source.save(&mut data).unwrap();
    drop((source, a, b, c, ring, first, second, budgets));
    assert_eq!(whole.used(), 0);
    let whole = PageBudget::new(65536);
    let mut budgets = BudgetTable::restore(&budget_bytes, |_| Ok(root(&whole))).unwrap();
    let owners = PageBudgetOwners::decode(&owner_bytes, &budgets).unwrap();
    let restored = HostPageTable::restore_owned(&data, &owners, &budgets).unwrap();
    owners.validate_pages(&restored, &budgets).unwrap();
    let foreign_whole = PageBudget::new(65536);
    let foreign = BudgetTable::restore(&budget_bytes, |_| Ok(root(&foreign_whole))).unwrap();
    assert!(owners.validate_pages(&restored, &foreign).is_err());
    assert!(owners
        .validate_pages(&HostPageTable::default(), &budgets)
        .is_err());
    assert_eq!(whole.used(), first_bytes + second_bytes);
    assert_eq!(
        budgets.budget(owners.owner(1).unwrap()).unwrap().used(),
        first_bytes
    );
    assert_eq!(
        budgets.budget(owners.owner(3).unwrap()).unwrap().used(),
        second_bytes
    );
    assert!(restored.page(4).unwrap().checkpoint_budget().is_none());
    let mut padding = [0];
    restored
        .page(4)
        .unwrap()
        .read_mapped_bytes(restored.page(4).unwrap().mapped_len() - 1, &mut padding)
        .unwrap();
    assert_eq!(padding, [0x77]);
    assert_eq!(
        restored
            .checkpoint_budget_owners(&mut budgets)
            .unwrap()
            .encode(&budgets)
            .unwrap(),
        owner_bytes
    );
    drop(restored);
    assert_eq!(whole.used(), 0);
}

#[test]
fn malformed_page_owner_roles_counts_and_trailing_bytes_are_refused() {
    let whole = PageBudget::new(65536);
    let owner = PageBudget::share(&whole, 32768);
    let class = PageBudget::new(65536);
    let mut budgets = BudgetTable::new(vec![
        root(&whole),
        BudgetRoot {
            key: BudgetRootKey::Class(0),
            whole: class.clone(),
            share_limit: 32768,
        },
    ])
    .unwrap();
    let mut pages = HostPageTable::default();
    pages
        .register(&Arc::new(
            RingPages::for_memory(4096, page_size() as u64, &owner).unwrap(),
        ))
        .unwrap();
    let owners = pages.checkpoint_budget_owners(&mut budgets).unwrap();
    let bytes = owners.encode(&budgets).unwrap();
    for len in 0..bytes.len() {
        assert!(PageBudgetOwners::decode(&bytes[..len], &budgets).is_err());
    }
    let class_id = budgets.register(&PageBudget::share(&class, 32768)).unwrap();
    for (at, value) in [(0, 2_u32), (4, u32::MAX), (8, u32::MAX), (8, class_id)] {
        let mut bad = bytes.clone();
        bad[at..at + 4].copy_from_slice(&value.to_le_bytes());
        assert!(PageBudgetOwners::decode(&bad, &budgets).is_err());
    }
    let mut bad = bytes;
    bad.push(0);
    assert!(PageBudgetOwners::decode(&bad, &budgets).is_err());
    let mut data = Vec::new();
    pages.save(&mut data).unwrap();
    assert!(HostPageTable::restore_owned(
        &data,
        &PageBudgetOwners { owners: Vec::new() },
        &budgets
    )
    .is_err());
    assert!(owners.owner(0).is_err());
    assert!(owners.owner(2).is_err());
}

#[test]
fn failed_later_page_allocation_refunds_earlier_original_owner_charges() {
    let whole = PageBudget::new(65536);
    let first = PageBudget::share(&whole, 32768);
    let second = PageBudget::share(&whole, 32768);
    let mut pages = HostPageTable::default();
    for owner in [&first, &second] {
        pages
            .register(&Arc::new(
                RingPages::for_memory(4096, page_size() as u64, owner).unwrap(),
            ))
            .unwrap();
    }
    let mut budgets = BudgetTable::new(vec![root(&whole)]).unwrap();
    let owners = pages.checkpoint_budget_owners(&mut budgets).unwrap();
    let owner_bytes = owners.encode(&budgets).unwrap();
    let budget_bytes = budgets.encode().unwrap();
    let mut data = Vec::new();
    pages.save(&mut data).unwrap();
    let whole = PageBudget::new(65536);
    let budgets = BudgetTable::restore(&budget_bytes, |_| Ok(root(&whole))).unwrap();
    let owners = PageBudgetOwners::decode(&owner_bytes, &budgets).unwrap();
    let prefill = budgets
        .budget(owners.owner(2).unwrap())
        .unwrap()
        .charge(32768)
        .unwrap();
    assert!(HostPageTable::restore_owned(&data, &owners, &budgets).is_err());
    assert_eq!(budgets.budget(owners.owner(1).unwrap()).unwrap().used(), 0);
    assert_eq!(whole.used(), 32768);
    drop(prefill);
    assert_eq!(whole.used(), 0);
}

#[test]
fn native_import_alignment_cannot_be_declared_an_anonymous_ring_allocation() {
    let whole = PageBudget::new(65536);
    let owner = PageBudget::share(&whole, 32768);
    let align = page_size() as u64 * 2;
    let mut pages = HostPageTable::default();
    pages
        .register(&Arc::new(
            RingPages::for_memory(align, align, &owner).unwrap(),
        ))
        .unwrap();
    let mut data = Vec::new();
    pages.save(&mut data).unwrap();
    let budgets = BudgetTable::new(vec![root(&whole)]).unwrap();
    assert!(
        HostPageTable::restore_owned(&data, &PageBudgetOwners { owners: vec![0] }, &budgets)
            .is_err()
    );
}

#[test]
fn streamed_pages_bound_reads_and_refund_allocations_after_a_mid_payload_failure() {
    use std::io::{Cursor, Read, Seek, SeekFrom};
    struct Reader {
        cursor: Cursor<Vec<u8>>,
        max_read: usize,
        fail_at: Option<u64>,
    }
    impl Read for Reader {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            self.max_read = self.max_read.max(bytes.len());
            if self.fail_at.is_some_and(|at| self.cursor.position() >= at) {
                return Err(std::io::Error::other("simulated page I/O failure"));
            }
            self.cursor.read(bytes)
        }
    }
    impl Seek for Reader {
        fn seek(&mut self, at: SeekFrom) -> std::io::Result<u64> {
            self.cursor.seek(at)
        }
    }
    let policy = |whole: &Arc<PageBudget>| BudgetRoot {
        key: BudgetRootKey::HostPages,
        whole: whole.clone(),
        share_limit: 1 << 20,
    };
    let source_whole = PageBudget::new(2 << 20);
    let source_owner = PageBudget::share(&source_whole, 1 << 20);
    let page = Arc::new(RingPages::for_memory(65537, 65536, &source_owner).unwrap());
    page.write_mapped_bytes(page.mapped_len() - 1, &[0xa7])
        .unwrap();
    let mut source = HostPageTable::default();
    source.register(&page).unwrap();
    let mut budgets = BudgetTable::new(vec![policy(&source_whole)]).unwrap();
    let owners = source.checkpoint_budget_owners(&mut budgets).unwrap();
    let owner_bytes = owners.encode(&budgets).unwrap();
    let budget_bytes = budgets.encode().unwrap();
    let mut bytes = Vec::new();
    source.save(&mut bytes).unwrap();
    let whole = PageBudget::new(2 << 20);
    let budgets = BudgetTable::restore(&budget_bytes, |_| Ok(policy(&whole))).unwrap();
    let owners = PageBudgetOwners::decode(&owner_bytes, &budgets).unwrap();
    let mut reader = Reader {
        cursor: Cursor::new(bytes.clone()),
        max_read: 0,
        fail_at: None,
    };
    let restored = HostPageTable::restore_owned_reader(&mut reader, &owners, &budgets).unwrap();
    assert!(reader.max_read <= CHUNK);
    assert_eq!(reader.cursor.position(), bytes.len() as u64);
    owners.validate_pages(&restored, &budgets).unwrap();
    let mut last = [0];
    restored
        .page(1)
        .unwrap()
        .read_mapped_bytes(page.mapped_len() - 1, &mut last)
        .unwrap();
    assert_eq!(last, [0xa7]);
    drop(restored);
    assert_eq!(whole.used(), 0);
    let mut reader = Reader {
        cursor: Cursor::new(bytes.clone()),
        max_read: 0,
        fail_at: Some(32 + CHUNK as u64),
    };
    assert!(HostPageTable::restore_owned_reader(&mut reader, &owners, &budgets).is_err());
    assert_eq!(
        whole.used(),
        0,
        "a partial streamed page is unpublished and refunded"
    );
    bytes.pop();
    assert!(
        HostPageTable::restore_owned_reader(&mut Cursor::new(bytes), &owners, &budgets).is_err()
    );
    assert_eq!(whole.used(), 0, "truncated framing fails before allocation");
}
