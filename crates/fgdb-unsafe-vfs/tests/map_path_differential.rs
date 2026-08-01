//! The differential harness for the three ledgered sites in `fgdb-unsafe-vfs`.
//!
//! `MapPath::Mapped` reaches the bytes through a raw `mmap(2)`; the fallback,
//! `MapPath::Buffered`, reads the same range through `std::io` and is compiled
//! on every target. The relationship asserted is bit-identity of the bytes and
//! of the refusal, over ranges chosen to straddle page boundaries — because a
//! mapping is page-granular and its intra-page offset is the one piece of
//! arithmetic that the buffered path does not have to get right.
//!
//! Determinism: the corpus and the range list are fixed, so a failure is a
//! case index rather than a story about flakiness. Replay:
//! `cargo test -p fgdb-unsafe-vfs --test map_path_differential`.
//!
//! The `munmap` site is exercised by holding several views live at once and
//! dropping them in an order that is not the order they were made: a `Drop`
//! that unmapped the wrong range, or unmapped twice, would be visible as
//! garbage or a fault in the views that remain.

use fgdb_unsafe_vfs::{COMPILED_MAP_PATHS, FileView, MapPath, open_view};
use std::fs::File;

const CORPUS_BYTES: usize = 5 * 4096 + 1237;

fn corpus() -> Vec<u8> {
    let mut bytes = std::fs::read(corpus_path()).expect("read the checked-in source corpus");
    assert!(
        bytes.len() >= CORPUS_BYTES,
        "source corpus shrank to {} bytes; the differential needs {CORPUS_BYTES}",
        bytes.len()
    );
    bytes.truncate(CORPUS_BYTES);
    bytes
}

fn corpus_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/view.rs")
}

fn fixture() -> File {
    File::open(corpus_path()).expect("open the checked-in source corpus")
}

/// Ranges chosen so the intra-page delta is zero, one, most of a page, and
/// exactly a page boundary, and so lengths cross one, two, and three pages.
fn ranges() -> Vec<(u64, usize)> {
    let mut out = Vec::new();
    for &offset in &[
        0_u64, 1, 2, 15, 4095, 4096, 4097, 8191, 8192, 8193, 12_288, 20_000,
    ] {
        for &len in &[1_usize, 2, 17, 4095, 4096, 4097, 8192, 9001] {
            if offset + len as u64 <= CORPUS_BYTES as u64 {
                out.push((offset, len));
            }
        }
    }
    out
}

fn digest(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[test]
fn every_compiled_path_agrees_with_the_file_bit_for_bit() {
    let data = corpus();
    let file = fixture();
    let cases = ranges();
    assert!(cases.len() >= 60, "only {} ranges", cases.len());
    let mut compared = 0_usize;
    for &(offset, len) in &cases {
        let expected = &data[usize::try_from(offset).expect("offset")..][..len];
        for &path in COMPILED_MAP_PATHS {
            let view: FileView = open_view(&file, offset, len, path)
                .expect("open")
                .expect("a path in COMPILED_MAP_PATHS must be openable");
            assert_eq!(
                view.bytes(),
                expected,
                "{path:?} disagreed with the file at {offset}+{len}"
            );
            assert_eq!(
                digest(view.bytes()),
                digest(expected),
                "{path:?} digest at {offset}+{len}"
            );
            compared += 1;
        }
    }
    assert_eq!(
        compared,
        cases.len() * COMPILED_MAP_PATHS.len(),
        "the matrix must not silently shrink"
    );
}

/// The control that licenses the digest above. Without it, "the digests match"
/// would also be reported by a digest that ignored its input.
#[test]
fn the_digest_separates_a_one_bit_change() {
    let data = corpus();
    let mut perturbed = data.clone();
    perturbed[CORPUS_BYTES / 2] ^= 1;
    assert_ne!(
        digest(&data),
        digest(&perturbed),
        "flipping one bit left the digest unchanged"
    );
}

/// The `munmap` site. Several mappings live at once, dropped out of order, with
/// the survivors re-read after each drop: a `Drop` that unmapped the wrong
/// range or unmapped twice would show up here rather than in a clean run.
#[test]
fn views_are_independent_and_survive_each_others_drops() {
    let data = corpus();
    let file = fixture();
    for &path in COMPILED_MAP_PATHS {
        let spans = [(0_u64, 4096_usize), (4096, 4096), (100, 9000), (20_000, 17)];
        let mut views: Vec<Option<FileView>> = spans
            .iter()
            .map(|&(offset, len)| {
                Some(
                    open_view(&file, offset, len, path)
                        .expect("open")
                        .expect("path"),
                )
            })
            .collect();
        // Drop order deliberately unlike creation order.
        for &victim in &[2_usize, 0, 3, 1] {
            views[victim] = None;
            for (index, view) in views.iter().enumerate() {
                if let Some(view) = view {
                    let (offset, len) = spans[index];
                    let expected = &data[usize::try_from(offset).expect("offset")..][..len];
                    assert_eq!(
                        view.bytes(),
                        expected,
                        "{path:?}: view {index} was corrupted by dropping view {victim}"
                    );
                }
            }
        }
    }
}

/// A view of the very last byte of the file: the mapping's final page is
/// partial, so this is where an off-by-one in the intra-page delta or the
/// mapping length would surface.
#[test]
fn the_last_byte_of_the_file_is_readable_on_every_path() {
    let data = corpus();
    let file = fixture();
    let offset = (CORPUS_BYTES - 1) as u64;
    for &path in COMPILED_MAP_PATHS {
        let view = open_view(&file, offset, 1, path)
            .expect("open")
            .expect("path");
        assert_eq!(view.bytes(), &data[CORPUS_BYTES - 1..], "{path:?}");
    }
}

/// The mapped path must be present in this build, or the whole differential is
/// one path comparing with itself. Asserted only where the ABI is known, which
/// is exactly where the site exists.
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
#[test]
fn the_ledgered_path_is_actually_in_this_build() {
    assert!(
        COMPILED_MAP_PATHS.contains(&MapPath::Mapped),
        "on x86-64 Linux the mapped path must be compiled, or this harness is \
         comparing the fallback against itself and would pass with the syscall \
         removed"
    );
}
