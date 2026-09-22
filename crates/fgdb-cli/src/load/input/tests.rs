use super::*;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

fn fixture(bytes: &[u8]) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "fgdb-checked-input-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .unwrap();
    file.write_all(bytes).unwrap();
    path
}
fn open(path: &Path) -> Input {
    Input::open_controlled(
        path,
        b"bindings",
        MAX_INPUT_BYTES,
        MAX_RECORD_BYTES,
        1_000_000,
        &mut || Ok(()),
    )
    .unwrap()
}
fn collect(mut reader: Reader) -> Vec<String> {
    let mut lines = Vec::new();
    while let Some(line) = reader.next_controlled(&mut || Ok(())).unwrap() {
        assert!(reader.cache.capacity() <= BLOCK_BYTES);
        lines.push(line);
    }
    lines
}

#[test]
fn standard_line_semantics_across_blocks_unicode_and_independent_clones() {
    for text in ["", "\n", "\r", "a\r\n\r\nx\n", "a\nb\r"]
        .map(str::to_owned)
        .into_iter()
        .chain(
            [
                BLOCK_BYTES - 2,
                BLOCK_BYTES - 1,
                BLOCK_BYTES,
                BLOCK_BYTES + 1,
            ]
            .map(|n| format!("{}λ\r\nnext\n\r\nlast\r", "x".repeat(n))),
        )
    {
        let path = fixture(text.as_bytes());
        let input = open(&path);
        let expected = text.lines().map(str::to_owned).collect::<Vec<_>>();
        assert_eq!(input.records(), expected.len());
        let mut first = input.reader();
        let second = first.clone();
        assert!(Arc::ptr_eq(&first.image, &second.image));
        let head = first.next_controlled(&mut || Ok(())).unwrap();
        let remaining = first.clone();
        assert_eq!(collect(second), expected);
        assert_eq!(head.as_deref(), expected.first().map(String::as_str));
        assert_eq!(collect(remaining), expected.get(1..).unwrap_or_default());
        assert_eq!(collect(first), expected.get(1..).unwrap_or_default());
    }
}

#[test]
fn exact_source_record_and_count_bounds_are_checked_during_sealing() {
    let path = fixture(b"ab\r\nlast");
    let make = |bytes, record, count| {
        Input::open_controlled(&path, b"", bytes, record, count, &mut || Ok(()))
    };
    assert!(make(8, 4, 2).is_ok());
    for (bytes, record, count, expected) in [
        (7, 4, 2, "input_bytes"),
        (8, 3, 2, "record_bytes"),
        (8, 4, 1, "source_rows"),
    ] {
        let error = make(bytes, record, count).err().expect("one-below limit");
        assert!(error.to_string().contains(expected));
    }
    let empty = fixture(b"");
    assert!(Input::open_controlled(&empty, b"", 0, 0, 0, &mut || Ok(())).is_ok());
    let blank = fixture(b"\n");
    assert!(Input::open_controlled(&blank, b"", 1, 0, 1, &mut || Ok(())).is_ok());
    assert!(Input::open_controlled(&blank, b"", 1, 0, 0, &mut || Ok(())).is_err());
    let long = fixture(&vec![b'x'; BLOCK_BYTES + 1]);
    assert!(Input::open_controlled(&long, b"", u64::MAX, BLOCK_BYTES, 1, &mut || Ok(())).is_err());
}

#[test]
fn complete_hash_keeps_the_existing_raw_bytes_plus_bindings_transcript() {
    let bytes = format!("{}\r\nλ", "z".repeat(BLOCK_BYTES + 3)).into_bytes();
    let path = fixture(&bytes);
    let input = open(&path);
    let mut expected = Hasher::new();
    expected.update(&bytes);
    expected.update(b"bindings");
    assert_eq!(input.source_hash(), &expected.finalize());
    assert_eq!(input.image.blocks.len(), bytes.len().div_ceil(BLOCK_BYTES));
    let changed = Input::open_controlled(
        &path,
        b"other",
        MAX_INPUT_BYTES,
        MAX_RECORD_BYTES,
        10,
        &mut || Ok(()),
    )
    .unwrap();
    assert_ne!(input.source_hash(), changed.source_hash());
}

#[test]
fn changed_uncached_blocks_are_rejected_instead_of_trusting_clone() {
    let path = fixture(format!("first\n{}\n", "x".repeat(BLOCK_BYTES + 20)).as_bytes());
    let input = open(&path);
    let mut reader = input.reader();
    assert_eq!(
        reader.next_controlled(&mut || Ok(())).unwrap().as_deref(),
        Some("first")
    );
    let mut cloned = reader.clone();
    let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.seek(SeekFrom::Start(BLOCK_BYTES as u64)).unwrap();
    file.write_all(b"y").unwrap();
    for reader in [&mut reader, &mut cloned] {
        assert!(
            reader
                .next_controlled(&mut || Ok(()))
                .unwrap_err()
                .to_string()
                .contains("SourceChanged")
        );
        assert!(
            reader
                .next_controlled(&mut || panic!("terminal reader must not perform I/O"))
                .unwrap()
                .is_none()
        );
    }
}

#[test]
fn a_clone_revalidates_the_previously_cached_block() {
    let path = fixture(b"first\nsecond\n");
    let input = open(&path);
    let mut reader = input.reader();
    reader.next_controlled(&mut || Ok(())).unwrap();
    let mut cloned = reader.clone();
    std::fs::write(&path, b"first\nCHANGE\n").unwrap();
    // Equal lengths prevent a metadata-only checker from passing this test.
    assert_eq!(std::fs::metadata(&path).unwrap().len(), input.image.len);
    assert!(
        cloned
            .next_controlled(&mut || Ok(()))
            .unwrap_err()
            .to_string()
            .contains("block changed")
    );
    // Already retained immutable bytes still denote the sealed source image.
    assert_eq!(
        reader.next_controlled(&mut || Ok(())).unwrap().as_deref(),
        Some("second")
    );
}

#[test]
fn length_drift_is_checked_at_eof_as_well_as_at_first_fetch() {
    for truncate in [false, true] {
        let path = fixture(b"first\n");
        let input = open(&path);
        let mut at_end = input.reader();
        at_end.next_controlled(&mut || Ok(())).unwrap();
        let mut fresh = input.reader();
        if truncate {
            std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap()
                .set_len(2)
                .unwrap();
        } else {
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(b"extra\n")
                .unwrap();
        }
        for reader in [&mut at_end, &mut fresh] {
            assert!(
                reader
                    .next_controlled(&mut || Ok(()))
                    .unwrap_err()
                    .to_string()
                    .contains("length changed")
            );
            assert!(reader.next_controlled(&mut || Ok(())).unwrap().is_none());
        }
    }
}

#[test]
fn utf8_failure_retains_its_absolute_line_and_fuses() {
    let path = fixture(b"ok\n\xff\nlast");
    let input = open(&path);
    let mut reader = input.reader();
    assert_eq!(
        reader.next_controlled(&mut || Ok(())).unwrap().as_deref(),
        Some("ok")
    );
    assert_eq!(reader.line(), 2);
    assert!(
        reader
            .next_controlled(&mut || Ok(()))
            .unwrap_err()
            .to_string()
            .contains("UTF-8")
    );
    assert_eq!(reader.line(), 2);
    assert!(
        reader
            .clone()
            .next_controlled(&mut || Ok(()))
            .unwrap()
            .is_none()
    );
}

#[cfg(unix)]
#[test]
fn replacing_the_path_does_not_switch_the_admitted_file_handle() {
    let path = fixture(b"original\n");
    let input = open(&path);
    let retained = path.with_extension("retained");
    std::fs::rename(&path, &retained).unwrap();
    std::fs::write(&path, b"replaced\n").unwrap();
    assert_eq!(collect(input.reader()), ["original"]);
    assert_eq!(collect(open(&path).reader()), ["replaced"]);
}

#[test]
fn every_io_checkpoint_can_stop_sealing_or_reading_without_a_successful_prefix() {
    let path = fixture(format!("{}λ\nlast", "x".repeat(BLOCK_BYTES + 5)).as_bytes());
    let mut calls = 0;
    Input::open_controlled(
        &path,
        b"",
        MAX_INPUT_BYTES,
        MAX_RECORD_BYTES,
        10,
        &mut || {
            calls += 1;
            Ok(())
        },
    )
    .unwrap();
    for stop in 1..=calls {
        let mut seen = 0;
        let result = Input::open_controlled(
            &path,
            b"",
            MAX_INPUT_BYTES,
            MAX_RECORD_BYTES,
            10,
            &mut || {
                seen += 1;
                if seen == stop {
                    Err(io::Error::from(io::ErrorKind::Interrupted))
                } else {
                    Ok(())
                }
            },
        );
        assert!(result.is_err());
        assert_eq!(seen, stop);
    }
    let input = open(&path);
    let mut calls = 0;
    input
        .reader()
        .next_controlled(&mut || {
            calls += 1;
            Ok(())
        })
        .unwrap();
    for stop in 1..=calls {
        let mut reader = input.reader();
        let mut seen = 0;
        let result = reader.next_controlled(&mut || {
            seen += 1;
            if seen == stop {
                Err(io::Error::from(io::ErrorKind::Interrupted))
            } else {
                Ok(())
            }
        });
        assert!(result.is_err());
        assert_eq!(seen, stop);
        assert!(
            reader
                .next_controlled(&mut || panic!("must remain terminal"))
                .unwrap()
                .is_none()
        );
    }
    assert_eq!(collect(input.reader()).len(), 2);
}

#[test]
fn generated_source_retains_only_a_block_cache_and_seal_metadata() {
    let path = fixture(b"");
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    let record = format!("{}\n", "x".repeat(10_000));
    for _ in 0..1000 {
        file.write_all(record.as_bytes()).unwrap();
    }
    drop(file);
    let input = open(&path);
    assert_eq!(input.records(), 1000);
    let mut reader = input.reader();
    let mut count = 0;
    while let Some(line) = reader.next_controlled(&mut || Ok(())).unwrap() {
        assert_eq!(line.len(), 10_000);
        assert!(reader.cache.capacity() <= BLOCK_BYTES);
        count += 1;
    }
    assert_eq!(count, 1000);
    assert_eq!(
        input.image.blocks.len(),
        (record.len() * 1000).div_ceil(BLOCK_BYTES)
    );
}
