use std::io::ErrorKind;

use aether_archive::range::{FileRange, RangeRead, SubRange, read_vec};

#[test]
fn slice_reads_and_bounds() {
    let data: Vec<u8> = (0..100u8).collect();
    let mut buf = [0u8; 4];
    data.read_at(10, &mut buf).unwrap();
    assert_eq!(buf, [10, 11, 12, 13]);
    data[..].read_at(96, &mut buf).unwrap();
    assert_eq!(buf, [96, 97, 98, 99]);
    let err = data.read_at(97, &mut buf).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UnexpectedEof);
    assert_eq!(
        data.read_at(u64::MAX, &mut buf).unwrap_err().kind(),
        ErrorKind::UnexpectedEof
    );
    assert_eq!(RangeRead::len(&data), 100);
}

#[test]
fn read_vec_checks_bounds_before_allocating() {
    let data = vec![7u8; 16];
    assert_eq!(read_vec(&data, 4, 4).unwrap(), vec![7; 4]);
    // A corrupt length field must fail fast, not try to allocate 2^63 bytes.
    let err = read_vec(&data, 0, u64::MAX / 2).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UnexpectedEof);
    assert_eq!(
        read_vec(&data, u64::MAX, 2).unwrap_err().kind(),
        ErrorKind::UnexpectedEof
    );
}

#[test]
fn sub_range_windows_its_parent() {
    let data: Vec<u8> = (0..50u8).collect();
    let sub = SubRange::new(&data, 10..20).unwrap();
    assert_eq!(sub.len(), 10);
    assert_eq!(read_vec(&sub, 0, 3).unwrap(), vec![10, 11, 12]);
    assert_eq!(read_vec(&sub, 8, 2).unwrap(), vec![18, 19]);
    assert!(read_vec(&sub, 8, 3).is_err());
    assert!(SubRange::new(&data, 40..51).is_err());
}

#[test]
fn file_range_reads_positioned() {
    // Cargo's per-target scratch dir for integration tests (not the system temp dir).
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("range-test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("f.bin");
    std::fs::write(&path, (0..=255u8).collect::<Vec<_>>()).unwrap();
    let f = FileRange::open(&path).unwrap();
    assert_eq!(f.len(), 256);
    assert_eq!(
        read_vec(&f, 250, 6).unwrap(),
        vec![250, 251, 252, 253, 254, 255]
    );
    assert!(read_vec(&f, 251, 6).is_err());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn arc_and_box_are_range_reads() {
    use std::sync::Arc;
    let data: Vec<u8> = (0..10u8).collect();
    let a: Arc<dyn RangeRead + Send + Sync> = Arc::new(data.clone());
    let b: Box<dyn RangeRead> = Box::new(data);
    assert_eq!(read_vec(&a, 2, 3).unwrap(), vec![2, 3, 4]);
    assert_eq!(read_vec(&b, 7, 3).unwrap(), vec![7, 8, 9]);
    assert_eq!(a.len(), 10);
}

#[test]
fn range_cursor_reads_and_seeks() {
    use aether_archive::range::RangeCursor;
    use std::io::{Read, Seek, SeekFrom};
    let data: Vec<u8> = (0..100u8).collect();
    let mut c = RangeCursor::new(&data);
    let mut buf = [0u8; 4];
    c.read_exact(&mut buf).unwrap();
    assert_eq!(buf, [0, 1, 2, 3]);
    assert_eq!(c.seek(SeekFrom::End(-2)).unwrap(), 98);
    let mut rest = Vec::new();
    c.read_to_end(&mut rest).unwrap();
    assert_eq!(rest, [98, 99]);
    assert_eq!(c.seek(SeekFrom::Current(-50)).unwrap(), 50);
    assert!(c.seek(SeekFrom::Current(-51)).is_err());
    // Seeking past the end is allowed; reads there return 0 bytes.
    c.seek(SeekFrom::Start(500)).unwrap();
    assert_eq!(c.read(&mut buf).unwrap(), 0);
}
