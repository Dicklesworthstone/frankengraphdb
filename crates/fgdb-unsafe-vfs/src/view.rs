//! Bounded, lifetime-checked views over a byte range of a file.
//!
//! Two paths reach the same bytes. [`MapPath::Buffered`] reads the range with
//! `std::io` and is compiled everywhere; [`MapPath::Mapped`] maps it with
//! `mmap(2)` and exists only where the syscall ABI is known. Both are planned
//! by the same safe [`plan_range`], so both refuse identical requests for
//! identical reasons — the check that licenses the mapping is exercised by
//! every test that touches either path.

use std::fs::File;
#[cfg(not(unix))]
use std::io::{Read, Seek, SeekFrom};

/// The mapping granularity this crate assumes on the mapped path.
///
/// 4 KiB is the x86-64 Linux ABI value, and the mapped path is compiled only
/// there. It is an assumption rather than a query because there is no `libc` to
/// ask (doctrine #1), and it is a *safe* assumption to get wrong: `mmap`
/// rejects a misaligned offset with `EINVAL`, so a kernel with a different
/// granularity would fail the call rather than return a view of the wrong
/// bytes, and the differential harness compares every view against the buffered
/// path byte for byte regardless.
pub const PAGE_BYTES: u64 = 4096;

/// Which implementation [`open_view`] uses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MapPath {
    /// The portable specification: the range read through `std::io`. No unsafe
    /// anywhere on this path, and no `cfg` on it either.
    Buffered,
    /// The ledgered path: `mmap(2)` issued directly, exposed as a bounded view.
    Mapped,
}

/// Every [`MapPath`] this build actually contains.
///
/// `Mapped` is absent off x86-64 Linux, and [`open_view`] answers `Ok(None)`
/// for a path that is not here. `None` is a real answer and must not be read as
/// a pass: the differential harness asserts it only for paths absent from this
/// list, so a path cannot silently drop out of the matrix and take its coverage
/// with it.
pub const COMPILED_MAP_PATHS: &[MapPath] = &[
    MapPath::Buffered,
    #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
    MapPath::Mapped,
];

/// Why a view could not be opened.
#[derive(Debug)]
pub enum VfsError {
    /// A zero-length view was requested. Refused: `mmap` rejects a zero length
    /// anyway, and a zero-length view is a bound nobody can check.
    EmptyRange,
    /// The requested range reaches past the end of the file. Refused *before*
    /// any mapping is made — a mapping over a hole faults on access with
    /// `SIGBUS` rather than returning an error, so this check is part of the
    /// site's safety argument rather than an ergonomic nicety.
    OutOfRange {
        offset: u64,
        len: usize,
        file_len: u64,
    },
    /// The filesystem refused a metadata or read operation.
    Io(std::io::Error),
    /// A syscall returned `-errno`.
    Syscall { name: &'static str, errno: i32 },
}

impl VfsError {
    /// A stable label, so two paths' refusals can be compared without
    /// comparing an `io::Error` (which is not `PartialEq`).
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::EmptyRange => "empty-range",
            Self::OutOfRange { .. } => "out-of-range",
            Self::Io(_) => "io",
            Self::Syscall { .. } => "syscall",
        }
    }
}

impl std::fmt::Display for VfsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyRange => write!(f, "a zero-length view is refused"),
            Self::OutOfRange {
                offset,
                len,
                file_len,
            } => write!(
                f,
                "range [{offset}, {offset}+{len}) reaches past a {file_len}-byte file"
            ),
            Self::Io(error) => write!(f, "io: {error}"),
            Self::Syscall { name, errno } => write!(f, "{name} failed with errno {errno}"),
        }
    }
}

impl std::error::Error for VfsError {}

impl From<std::io::Error> for VfsError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Bound the request against the file, identically on both paths.
///
/// This is the whole precondition the mapped site relies on, and it is
/// deliberately safe, path-independent code rather than a comment inside the
/// unsafe block: the buffered path calls it too, so every test that opens a
/// view exercises it.
///
/// # Errors
///
/// [`VfsError::EmptyRange`] for a zero length, [`VfsError::OutOfRange`] for a
/// range reaching past the end of the file, [`VfsError::Io`] if the file cannot
/// be measured.
pub fn plan_range(file: &File, offset: u64, len: usize) -> Result<u64, VfsError> {
    if len == 0 {
        return Err(VfsError::EmptyRange);
    }
    let file_len = file.metadata()?.len();
    let end = offset.checked_add(len as u64).ok_or(VfsError::OutOfRange {
        offset,
        len,
        file_len,
    })?;
    if end > file_len {
        return Err(VfsError::OutOfRange {
            offset,
            len,
            file_len,
        });
    }
    Ok(file_len)
}

/// A bounded view of a byte range, valid for as long as the view is held.
#[derive(Debug)]
pub struct FileView {
    backing: Backing,
    path: MapPath,
}

#[derive(Debug)]
enum Backing {
    Buffered(Vec<u8>),
    #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
    Mapped(sys::Mapping),
}

impl FileView {
    /// The bytes of the requested range.
    ///
    /// Borrowed from `&self`, so the slice cannot outlive the mapping, and
    /// exactly the requested length rather than the length the kernel rounded
    /// the mapping up to. That pair of properties is what "bounded,
    /// lifetime-checked view" means in the island's charter.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        match &self.backing {
            Backing::Buffered(buffer) => buffer.as_slice(),
            #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
            Backing::Mapped(mapping) => mapping.view_bytes(),
        }
    }

    /// The path this view was opened through.
    #[must_use]
    pub const fn path(&self) -> MapPath {
        self.path
    }

    /// The length of the view.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes().len()
    }

    /// Always false — a zero-length view is refused at [`open_view`] — but
    /// clippy asks for it beside `len`, and answering honestly is cheaper than
    /// an allow.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes().is_empty()
    }
}

/// Open a view of `len` bytes at `offset` through a named path.
///
/// Returns `Ok(None)` when this build does not contain the requested path.
///
/// # Errors
///
/// Whatever [`plan_range`] refuses, plus [`VfsError::Io`] from the buffered
/// read and [`VfsError::Syscall`] from a failed `mmap`.
pub fn open_view(
    file: &File,
    offset: u64,
    len: usize,
    path: MapPath,
) -> Result<Option<FileView>, VfsError> {
    plan_range(file, offset, len)?;
    match path {
        MapPath::Buffered => {
            let mut buffer = vec![0_u8; len];
            // A positional read where one exists, because a view is a read and
            // a read that moved someone else's file position would be a side
            // effect the mapped path does not have. `try_clone` does NOT buy
            // this — on Unix it dups the descriptor and both share one file
            // *description*, so the seek moves the caller's cursor too. That
            // was written the wrong way first and the cursor row caught it.
            #[cfg(unix)]
            {
                use std::os::unix::fs::FileExt;
                file.read_exact_at(&mut buffer, offset)?;
            }
            // Elsewhere, seek on a cloned handle: portable, and the byte result
            // is identical, which is the whole of the bit-identity claim. The
            // cursor caveat is stated in the ledger row rather than papered
            // over.
            #[cfg(not(unix))]
            {
                let mut handle = file.try_clone()?;
                handle.seek(SeekFrom::Start(offset))?;
                handle.read_exact(&mut buffer)?;
            }
            Ok(Some(FileView {
                backing: Backing::Buffered(buffer),
                path,
            }))
        }
        #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
        MapPath::Mapped => {
            let mapping = sys::map_range(file, offset, len)?;
            Ok(Some(FileView {
                backing: Backing::Mapped(mapping),
                path,
            }))
        }
        #[cfg(not(all(target_arch = "x86_64", target_os = "linux")))]
        MapPath::Mapped => Ok(None),
    }
}

/// The syscall surface. Three ledgered sites, and nothing else in the crate is
/// unsafe.
///
/// The module exists only where the ABI is known, which is what makes each
/// site's `allow` unconditional rather than `cfg_attr`-wrapped: the code is not
/// there at all on other targets, so an unconditional relaxation is not
/// standing open over a site that does not exist. It is PRIVATE: `map_range`
/// is sound only behind `plan_range`'s in-file precondition, and a public
/// module would let a caller skip that check and hand out a view whose access
/// faults SIGBUS (probe-proven). Keeping the boundary here makes the
/// precondition universal by construction.
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
mod sys {
    use super::{PAGE_BYTES, VfsError};
    use std::fs::File;
    use std::os::fd::AsRawFd;
    use std::ptr::NonNull;

    /// `mmap` on x86-64 Linux.
    const SYS_MMAP: usize = 9;
    /// `munmap` on x86-64 Linux.
    const SYS_MUNMAP: usize = 11;
    /// `PROT_READ`.
    const PROT_READ: usize = 0x1;
    /// `MAP_PRIVATE`. Private so a write through this mapping — impossible
    /// under `PROT_READ` — could never propagate back to the file. It
    /// privatizes only THIS process's writes: another process's writes to the
    /// file remain observable through the view (there is no copy-on-write to
    /// hide behind), which is why coherence is the caller's contract, not
    /// this crate's claim.
    const MAP_PRIVATE: usize = 0x2;
    /// Linux encodes syscall errors as `-errno` in the range `[-4095, -1]`.
    const MAX_ERRNO: isize = 4095;

    /// A live read-only mapping, and the exact geometry needed to give back
    /// what was taken.
    #[derive(Debug)]
    pub struct Mapping {
        addr: NonNull<u8>,
        map_len: usize,
        delta: usize,
        view_len: usize,
    }

    impl Mapping {
        /// The bounded view.
        ///
        /// LEDGER ROW `vfs-mapping-view-bytes`. Named distinctly from
        /// `FileView::bytes`, which has a byte-identical signature line: the
        /// ledger matches a site by (path, symbol), so two items whose
        /// signatures render the same in one file would make a row ambiguous to
        /// every human reader even though only one of them is a site.
        #[allow(unsafe_code)]
        #[must_use]
        pub fn view_bytes(&self) -> &[u8] {
            // SAFETY: four obligations, all established when the mapping was
            // made and none of them re-derived here.
            //
            // 1. IN BOUNDS. `map_range` computed `map_len = delta + view_len`
            //    and `mmap` returned a mapping of at least `map_len` bytes
            //    (the kernel rounds up to a page, never down), so
            //    `addr + delta` and the `view_len` bytes after it lie inside
            //    one mapping.
            // 2. LIVE. `addr` is unmapped only by `Drop`, which consumes the
            //    `Mapping`, and the returned slice borrows `&self`, so the
            //    mapping outlives every reference produced here.
            // 3. INITIALIZED AND READABLE. The mapping is `PROT_READ` over a
            //    range that `plan_range` proved lies wholly inside the file, so
            //    every byte is backed by file contents rather than by a hole.
            //    A range past EOF would fault with SIGBUS on access, which is
            //    why that check is a precondition of the mapping rather than a
            //    convenience.
            // 4. NO MUTABLE ALIAS. The mapping is read-only and this crate
            //    never forms a `&mut` to it. Another process's writes to the
            //    file ARE observable through the view — `MAP_PRIVATE`
            //    privatizes only this process's own writes (copy-on-write,
            //    which `PROT_READ` makes unreachable) — so the bytes are NOT a
            //    snapshot; coherence with a live writer is the caller's
            //    contract, and the ledger row disclaims it in both directions.
            unsafe {
                core::slice::from_raw_parts(self.addr.as_ptr().add(self.delta), self.view_len)
            }
        }
    }

    impl Drop for Mapping {
        /// LEDGER ROW `vfs-mapping-munmap`.
        #[allow(unsafe_code)]
        fn drop(&mut self) {
            // SAFETY: `addr` and `map_len` are exactly the address and length
            // `mmap` returned and were never modified, so this unmaps precisely
            // what was mapped and nothing else. `Drop` runs at most once per
            // `Mapping`, and `Mapping` is neither `Copy` nor `Clone`, so the
            // range cannot be unmapped twice. Every slice handed out by
            // `bytes` borrows `&self` and is therefore dead before this point.
            // The syscall itself follows the x86-64 Linux ABI: number in rax,
            // arguments in rdi and rsi, rcx and r11 clobbered by `syscall`.
            let result: isize;
            unsafe {
                core::arch::asm!(
                    "syscall",
                    inlateout("rax") SYS_MUNMAP as isize => result,
                    in("rdi") self.addr.as_ptr(),
                    in("rsi") self.map_len,
                    lateout("rcx") _,
                    lateout("r11") _,
                    options(nostack)
                );
            }
            debug_assert!(result == 0, "munmap failed with {result}");
        }
    }

    /// Map `len` bytes at `offset` of `file`, read-only.
    ///
    /// LEDGER ROW `vfs-mmap-readonly`.
    ///
    /// # Errors
    ///
    /// [`VfsError::Syscall`] carrying the kernel's `errno`.
    ///
    /// # Panics
    ///
    /// Never: the `NonNull` construction below cannot see a null address,
    /// because `mmap` reports failure as `-errno` and that range is checked
    /// first.
    #[allow(unsafe_code)]
    pub fn map_range(file: &File, offset: u64, len: usize) -> Result<Mapping, VfsError> {
        // Everything up to the syscall is safe arithmetic, and deliberately so:
        // the page-aligned base, the intra-page delta, and the mapping length
        // are the inputs the site's bounds argument rests on.
        let page_offset = offset & !(PAGE_BYTES - 1);
        let delta = usize::try_from(offset - page_offset).expect("intra-page delta fits usize");
        let map_len = delta.checked_add(len).ok_or(VfsError::Syscall {
            name: "mmap",
            errno: 22, // EINVAL — the length the kernel would have refused.
        })?;
        let fd = file.as_raw_fd();

        // SAFETY: the `asm!` block issues one `syscall` instruction under the
        // x86-64 Linux ABI: number in rax, arguments in rdi, rsi, rdx, r10, r8,
        // r9, result in rax, and rcx and r11 clobbered by the instruction
        // itself — all of which are declared. `options(nostack)` is licensed
        // because the block neither pushes nor pops; `preserves_flags` is
        // deliberately NOT claimed, because `syscall` saves rflags into r11 and
        // masks it.
        //
        // The call cannot violate any memory invariant of this program on its
        // own: `addr` is null, so the kernel chooses a fresh range and cannot be
        // asked to replace an existing mapping; `PROT_READ | MAP_PRIVATE` means
        // the new range is read-only and unshared; `fd` is borrowed from a live
        // `File` for the duration of the call. Nothing is read or written
        // through a pointer here — the returned address is only inspected as an
        // integer until `Mapping::bytes` bounds it.
        let result: isize;
        unsafe {
            core::arch::asm!(
                "syscall",
                inlateout("rax") SYS_MMAP as isize => result,
                in("rdi") 0_usize,
                in("rsi") map_len,
                in("rdx") PROT_READ,
                in("r10") MAP_PRIVATE,
                in("r8") fd as usize,
                in("r9") page_offset as usize,
                lateout("rcx") _,
                lateout("r11") _,
                options(nostack)
            );
        }
        if (-MAX_ERRNO..0).contains(&result) {
            return Err(VfsError::Syscall {
                name: "mmap",
                errno: i32::try_from(-result).unwrap_or(i32::MAX),
            });
        }
        let addr = NonNull::new(result as *mut u8).ok_or(VfsError::Syscall {
            name: "mmap",
            errno: 12, // ENOMEM — a null return that was not an error encoding.
        })?;
        Ok(Mapping {
            addr,
            map_len,
            delta,
            view_len: len,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{COMPILED_MAP_PATHS, MapPath, VfsError, open_view, plan_range};
    use std::fs::File;
    use std::io::Write;

    fn fixture(tag: &str, bytes: &[u8]) -> (std::path::PathBuf, File) {
        let path = std::env::temp_dir().join(format!(
            "fgdb-unsafe-vfs-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let mut file = File::create(&path).expect("create");
        file.write_all(bytes).expect("write");
        file.sync_all().expect("sync");
        (path.clone(), File::open(&path).expect("open"))
    }

    fn pseudorandom(len: usize) -> Vec<u8> {
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                u8::try_from(state & 0xff).expect("byte")
            })
            .collect()
    }

    #[test]
    fn every_compiled_path_returns_the_same_bytes() {
        let data = pseudorandom(3 * 4096 + 137);
        let (_path, file) = fixture("same-bytes", &data);
        let mut compared = 0_usize;
        for offset in [0_u64, 1, 7, 4095, 4096, 4097, 8192, 9000] {
            for len in [1_usize, 2, 63, 4096, 4097, 5000] {
                if offset + len as u64 > data.len() as u64 {
                    continue;
                }
                let expected = &data[usize::try_from(offset).expect("offset")..][..len];
                for &path in COMPILED_MAP_PATHS {
                    let view = open_view(&file, offset, len, path)
                        .expect("open")
                        .expect("path is compiled");
                    assert_eq!(view.path(), path);
                    assert_eq!(view.len(), len, "{path:?} at {offset}+{len}");
                    assert_eq!(
                        view.bytes(),
                        expected,
                        "{path:?} disagreed with the file at {offset}+{len}"
                    );
                    compared += 1;
                }
            }
        }
        assert!(compared >= 40, "only {compared} views compared");
    }

    #[test]
    fn both_paths_refuse_identically() {
        let data = pseudorandom(100);
        let (_path, file) = fixture("refuse", &data);
        for (offset, len) in [(0_u64, 0_usize), (0, 101), (99, 2), (100, 1), (u64::MAX, 1)] {
            let labels: Vec<&str> = COMPILED_MAP_PATHS
                .iter()
                .map(|&path| match open_view(&file, offset, len, path) {
                    Ok(Some(_)) => "ok",
                    Ok(None) => "absent",
                    Err(error) => error.label(),
                })
                .collect();
            assert!(
                labels.windows(2).all(|w| w[0] == w[1]),
                "paths disagreed at {offset}+{len}: {labels:?}"
            );
            assert_ne!(labels[0], "ok", "{offset}+{len} was meant to be refused");
        }
    }

    #[test]
    fn a_range_past_the_end_is_refused_before_anything_is_mapped() {
        let (_path, file) = fixture("past-end", &[0_u8; 16]);
        assert!(matches!(
            plan_range(&file, 8, 16),
            Err(VfsError::OutOfRange {
                offset: 8,
                len: 16,
                file_len: 16
            })
        ));
        assert!(matches!(plan_range(&file, 0, 0), Err(VfsError::EmptyRange)));
        assert!(plan_range(&file, 0, 16).is_ok());
    }

    #[test]
    fn the_buffered_path_is_always_compiled() {
        assert!(
            COMPILED_MAP_PATHS.contains(&MapPath::Buffered),
            "the fallback must exist on every target, or the bit-identity claim \
             has nothing to stand on"
        );
    }

    /// Opening a view must not disturb the caller's file position on any path.
    ///
    /// This row exists because the first version of the buffered path got it
    /// wrong: it cloned the handle with `try_clone` and seeked, on the belief
    /// that a clone has its own cursor. On Unix `try_clone` dups the
    /// descriptor, both share one file *description*, and the seek moved the
    /// caller's position — an observable difference between the two paths that
    /// no byte comparison would have found.
    #[cfg(unix)]
    #[test]
    fn a_view_does_not_move_the_callers_cursor() {
        use std::io::{Read, Seek, SeekFrom};
        let data = pseudorandom(64);
        let (_path, file) = fixture("cursor", &data);
        let mut file = file;
        file.seek(SeekFrom::Start(8)).expect("seek");
        for &path in COMPILED_MAP_PATHS {
            let _view = open_view(&file, 32, 16, path).expect("open").expect("path");
        }
        let mut byte = [0_u8; 1];
        file.read_exact(&mut byte).expect("read");
        assert_eq!(byte[0], data[8], "a view moved the caller's file position");
    }
}
