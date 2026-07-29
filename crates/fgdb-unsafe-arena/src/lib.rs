//! `fgdb-unsafe-arena` — the region/arena unsafe boundary island
//! (bead `fgdb-w1-unsafe-islands-eqrq`; plan §1 constraint 2, §18.1).
//!
//! # Why this crate is a separate crate
//!
//! AGENTS.md constraint 2 makes memory safety *structural*: every ordinary
//! crate root and the workspace default use `unsafe_code = "forbid"`, and
//! Rust's `forbid` cannot be lowered by an inner `allow`. Raw-pointer work
//! therefore cannot live in an ordinary crate at all. It lives here, in a
//! separately named island whose manifest omits the inherited lint table and
//! whose root uses [`deny`] instead, with narrowly scoped `allow(unsafe_code)`
//! sites each enumerated in `registries/unsafe_boundary_ledger.toml`.
//!
//! # The two things this island exists for
//!
//! An arena is *not* automatically an unsafe crate, and it would have been
//! dishonest to open an island and then fill it with unsafe that safe Rust
//! already expresses. Everything a region allocator does — carving a fixed
//! chunk into blocks, aligning a block, tracking generations, refusing a stale
//! handle, counting bytes in and out — is written here in safe code with no
//! relaxation at all. Alignment in particular: [`Region`] reads the chunk's
//! base address with `as_ptr()`, which is safe, and picks the offset that makes
//! `base + offset` aligned, so an aligned block needs no unsafe to hand out.
//!
//! One operation cannot be written safely, and it is the one ART needs
//! most: **N simultaneous exclusive views into disjoint blocks of the same
//! region**. Splitting a node means holding `&mut` to a parent and a child at
//! once; the borrow checker cannot see that two byte ranges carved from one
//! chunk do not overlap, so [`Region::blocks_mut`] proves it at runtime and
//! forms the views in a single ledgered block. Together with the sealed
//! allocator adapter below, that makes the whole unsafe surface of this crate:
//! two sites with separate invariants and ledger rows.
//!
//! # What a site must carry
//!
//! Each one:
//!
//! * a `// SAFETY:` note stating the local invariant, discharged obligation by
//!   obligation;
//! * a ledger row with that invariant, the evidence that exercises it, the
//!   fallback it must agree with, and its `no_claim_boundary` — what the site
//!   does *not* guarantee, so a clean run cannot be inflated into a proof;
//! * a **bit-identical fallback that cross-compiles to every target**. Here
//!   that is [`EditPath::Sequential`]: the same batch of edits applied one
//!   single-borrow call at a time, entirely in safe code, with no `cfg` on it
//!   anywhere. [`Region::apply`] runs a named path exactly the way
//!   `fgdb-unsafe-simd`'s `classify_via` does, so the differential harness
//!   iterates a matrix instead of testing whichever path the build selected.
//!   The relationship asserted is bit-identity of the whole region image, not
//!   agreement on a summary.
//!
//! # The seam
//!
//! [`RegionAlloc`] is the trait ART, succinct, and hash storage are meant to be
//! parameterized over, so that "which allocator" becomes a type parameter
//! rather than a rewrite. It is deliberately byte-oriented: blocks are
//! `[u8]`, because the durable node encodings (Appendix A) are byte images
//! already, and a `T`-placement arena would need drop glue, `MaybeUninit`, and
//! several more unsafe sites to buy something no consumer has asked for.
//!
//! **No consumer is wired to it yet.** That is remainder item 9 on
//! `fgdb-w1-unsafe-islands-eqrq` (consumer integration), and the ledger row for
//! this island says so in its `no_claim_boundary` rather than letting the
//! existence of a trait imply an integration.
//!
//! The second operation is the sealed allocator adapter behind [`RegionVec`].
//! Rust's allocator API makes the trait and its deallocation callback unsafe;
//! the adapter therefore lives entirely inside this island. Its type, pointer
//! vocabulary, allocator-parameterized `Vec`, and unsafe callbacks never cross
//! the public surface. Safe consumers see a typed container whose allocating
//! methods require a [`fgdb_types::QueryCx`].
//!
//! # Reclamation is an audited claim, not a hope
//!
//! [`Region::close`] and [`Region::cancel`] both return a [`RegionAudit`], and
//! both must report `bytes_reclaimed == bytes_allocated`. Cancellation is the
//! interesting half: an arena that balances only on the happy path is exactly
//! the arena that leaks when a query is cancelled mid-build, which under
//! asupersync's obligation model is a routine event rather than an error.
//!
//! # What is deliberately NOT here
//!
//! No public raw pointers and no public `unsafe fn`: every export is safe to
//! call from a `forbid(unsafe_code)` crate. Typed placement is available only
//! through [`RegionVec`], which delegates initialization, moves, and drop glue
//! to the standard library's `Vec<T, A>` implementation. No `Send`/`Sync`
//! claim beyond what the compiler derives, and no concurrent allocation claim
//! is made: [`RegionScope`] is task-local and its vectors borrow it.

#![deny(unsafe_code)]
#![feature(allocator_api)]

pub mod region;

pub use region::{
    ArenaError, Edit, EditPath, Handle, Region, RegionAlloc, RegionAudit, RegionFinishError,
    RegionOutcome, RegionScope, RegionVec, RegionVecError,
};
