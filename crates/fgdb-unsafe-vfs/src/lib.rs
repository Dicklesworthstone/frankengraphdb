//! `fgdb-unsafe-vfs` — the file/mapping unsafe boundary island
//! (bead `fgdb-w1-unsafe-islands-eqrq`; plan §1 constraint 2, §18.1).
//!
//! # Why this crate is a separate crate
//!
//! AGENTS.md constraint 2 makes memory safety *structural*: every ordinary
//! crate root and the workspace default use `unsafe_code = "forbid"`, and
//! Rust's `forbid` cannot be lowered by an inner `allow`. Raw-pointer and
//! syscall work therefore cannot live in an ordinary crate at all. It lives
//! here, in a separately named island whose manifest omits the inherited lint
//! table and whose root uses [`deny`] instead, with narrowly scoped
//! `allow(unsafe_code)` sites each enumerated in
//! `registries/unsafe_boundary_ledger.toml`.
//!
//! # Why this island cannot be avoided, and where it stops
//!
//! The dependency universe is closed (doctrine #1): `core`, `alloc`, `std`, the
//! pinned nightly, and the three foundations. There is no `libc`, so there is
//! no `mmap` to call — the syscall has to be issued directly, and on x86-64
//! Linux that means one `core::arch::asm!` instruction. Everything else this
//! crate does is safe: opening the file, measuring it, bounding the request,
//! computing the page-aligned base, and the entire [`MapPath::Buffered`] path
//! are ordinary safe Rust with no relaxation at all.
//!
//! Three sites, each one syscall or one slice:
//!
//! * [`sys::mmap_readonly`] — the `mmap(2)` syscall;
//! * [`sys::munmap`] — the `munmap(2)` syscall, from `Drop`;
//! * `Mapping::bytes` — forming the bounded `&[u8]` the caller actually sees.
//!
//! # What a site must carry
//!
//! Each one:
//!
//! * a `// SAFETY:` note stating the local invariant, discharged obligation by
//!   obligation;
//! * a ledger row with that invariant, the evidence that exercises it, the
//!   fallback it must agree with, and its `no_claim_boundary`;
//! * a **bit-identical fallback that cross-compiles to every target**. Here
//!   that is [`MapPath::Buffered`], which reads the same byte range through
//!   `std::io` and is compiled on every target the workspace supports. The
//!   mapped path exists only where the syscall ABI is known, so
//!   [`open_view`] returns `Ok(None)` for it elsewhere — a real answer, not a
//!   pass — exactly the way `fgdb-unsafe-simd`'s `classify_via` reports a
//!   dispatch path this build does not contain.
//!
//! # The bounded, lifetime-checked view
//!
//! The charter is that a mapping reaches safe callers only as a bounded,
//! lifetime-checked view. [`FileView::bytes`] returns `&[u8]` borrowed from
//! `&self`, so the slice cannot outlive the mapping, and its length is the
//! length the caller asked for rather than the length the kernel rounded up to.
//! No pointer, no length, and no file descriptor is public.
//!
//! # The obligation the caller keeps
//!
//! A mapped range that is truncated away underneath a live mapping faults on
//! access — `SIGBUS`, not a `Result`. That is why [`open_view`] measures the
//! file and refuses any request reaching past its end, and why the ledger rows
//! say plainly that keeping the file from shrinking for the life of the view is
//! the caller's contract. Torn writes and bit rot are likewise the caller's
//! contract: this crate reports the bytes that are there.

#![deny(unsafe_code)]

pub mod view;

pub use view::{COMPILED_MAP_PATHS, FileView, MapPath, VfsError, open_view};
