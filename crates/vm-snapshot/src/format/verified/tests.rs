use super::*;
use std::io::Cursor;
use std::sync::{Arc, Mutex};

fn sample(payload: &[u8]) -> Vec<u8> {
    let mut writer = SnapshotWriter::create(Cursor::new(Vec::new()), HostKind::WhpWindows).unwrap();
    writer
        .put(SectionKind::Metadata, 1, 0, b"neighbor before")
        .unwrap();
    writer.put(SectionKind::Memory, 1, 0, payload).unwrap();
    writer
        .put(SectionKind::Cpu, 1, 0, b"neighbor after")
        .unwrap();
    writer.finish().unwrap().1.into_inner()
}

#[test]
fn verified_random_reads_stay_inside_the_section_and_preserve_tail_bytes() {
    let payload = (0..3 * BLOCK + 17)
        .map(|at| (at % 251) as u8)
        .collect::<Vec<_>>();
    let bytes = sample(&payload);
    let mut reader = SnapshotReader::open(Cursor::new(bytes)).unwrap();
    {
        let mut section = reader.verified_section(SectionKind::Memory, 0).unwrap();
        assert_eq!(section.len(), payload.len() as u64);
        assert!(!section.is_empty());
        for at in [BLOCK - 1, 0, 2 * BLOCK + 3, BLOCK + 11, payload.len() - 19] {
            section.seek(SeekFrom::Start(at as u64)).unwrap();
            let mut bytes = [0; 19];
            section.read_exact(&mut bytes).unwrap();
            assert_eq!(&bytes, &payload[at..at + 19]);
        }
        section.seek(SeekFrom::End(-7)).unwrap();
        let mut tail = [0; 32];
        assert_eq!(section.read(&mut tail).unwrap(), 7);
        assert_eq!(&tail[..7], &payload[payload.len() - 7..]);
        assert_eq!(section.read(&mut tail).unwrap(), 0);
        assert!(section.seek(SeekFrom::End(1)).is_err());
        assert!(section
            .seek(SeekFrom::Start(payload.len() as u64 + 1))
            .is_err());
        section.rewind().unwrap();
        assert!(section.seek(SeekFrom::Current(-1)).is_err());
    }
    assert_eq!(
        reader.read_section(SectionKind::Cpu, 0).unwrap(),
        b"neighbor after"
    );
}

#[derive(Debug, Clone)]
struct SharedFile(Arc<Mutex<Cursor<Vec<u8>>>>);
impl Read for SharedFile {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().read(out)
    }
}
impl Seek for SharedFile {
    fn seek(&mut self, at: SeekFrom) -> std::io::Result<u64> {
        self.0.lock().unwrap().seek(at)
    }
}

#[test]
fn changes_after_verification_or_after_cache_eviction_never_reach_the_caller() {
    let source = SharedFile(Arc::new(Mutex::new(Cursor::new(sample(&vec![
        0x31;
        3 * BLOCK
            + 17
    ])))));
    let mut reader = SnapshotReader::open(source.clone()).unwrap();
    let offset = reader.entry(SectionKind::Memory, 0).unwrap().offset as usize;
    let mut section = reader.verified_section(SectionKind::Memory, 0).unwrap();
    let mut byte = [0];
    section.read_exact(&mut byte).unwrap();
    assert_eq!(byte, [0x31]);
    source.0.lock().unwrap().get_mut()[offset + BLOCK + 5] ^= 1;
    section.seek(SeekFrom::Start(BLOCK as u64 + 5)).unwrap();
    let error = section.read_exact(&mut byte).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("memory[0]"));
    // Evict the original block, then change it too. An earlier successful read
    // cannot exempt a block from verification when it is fetched again.
    section.seek(SeekFrom::Start(2 * BLOCK as u64)).unwrap();
    section.read_exact(&mut byte).unwrap();
    source.0.lock().unwrap().get_mut()[offset] ^= 1;
    section.rewind().unwrap();
    assert_eq!(
        section.read_exact(&mut byte).unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );
}

#[test]
fn a_corrupt_section_is_refused_before_any_random_access_reader_is_returned() {
    let mut bytes = sample(b"checked bytes");
    let offset = SnapshotReader::open(Cursor::new(&bytes))
        .unwrap()
        .entry(SectionKind::Memory, 0)
        .unwrap()
        .offset as usize;
    bytes[offset] ^= 1;
    let mut reader = SnapshotReader::open(Cursor::new(bytes)).unwrap();
    assert!(matches!(
        reader.verified_section(SectionKind::Memory, 0),
        Err(SnapshotError::Corrupt { .. })
    ));
}

#[test]
fn empty_sections_and_signed_seek_overflows_remain_bounded() {
    let mut reader = SnapshotReader::open(Cursor::new(sample(&[]))).unwrap();
    let mut section = reader.verified_section(SectionKind::Memory, 0).unwrap();
    assert!(section.is_empty());
    assert_eq!(section.read(&mut [0; 1]).unwrap(), 0);
    assert!(section.seek(SeekFrom::Current(i64::MIN)).is_err());
    assert!(section.seek(SeekFrom::End(i64::MAX)).is_err());
    assert_eq!(section.stream_position().unwrap(), 0);
}
