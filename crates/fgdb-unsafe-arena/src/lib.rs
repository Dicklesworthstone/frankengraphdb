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
//! relaxation at all. Alignment in particular: every chunk is over-allocated
//! by `MAX_BLOCK_ALIGN - 1` and its `base_pad` recorded once at creation, so
//! each chunk's usable window starts at an address that is `0 mod
//! MAX_BLOCK_ALIGN` and placement padding derives only from offsets the
//! region itself assigned — never from where the global allocator happened
//! to put the chunk. Fit decisions are therefore heap-history independent
//! (fgdb-owje), and they need no unsafe to be made.
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
//! [`RegionAlloc`] remains the byte-oriented trait for raw arena consumers and
//! differential verification. Blocks are `[u8]` because durable node encodings
//! (Appendix A) are byte images already. It is not the typed collection seam:
//! exposing it to ART, succinct, or hash storage would export byte handles
//! instead of preserving generic value ownership and drop glue.
//!
//! The second operation is the sealed allocator adapter behind [`RegionVec`].
//! Rust's allocator API makes the trait and its deallocation callback unsafe;
//! the adapter therefore lives entirely inside this island. Its type, pointer
//! vocabulary, allocator-parameterized `Vec`, and unsafe callbacks never cross
//! the public surface. Safe consumers see a typed container whose allocating
//! methods require a [`fgdb_types::QueryCx`]. ART, succinct, and deterministic
//! hash storage consume that surface directly; the safe collection integration
//! never names the private allocator or raw pointer vocabulary.
//!
//! # Reclamation is an audited claim, not a hope
//!
//! [`Region::close`] and [`Region::cancel`] both return a [`RegionAudit`], and
//! both must report `bytes_reclaimed == bytes_allocated`. Cancellation is the
//! interesting half: an arena that balances only on the happy path is exactly
//! the arena that leaks when a query is cancelled mid-build, which under
//! asupersync's obligation model is a routine event rather than an error.
//!
//! # Two budgets, not one ambiguous one
//!
//! Every region carries an explicit, totally ordered budget triple
//! (`0 < chunk_bytes <= max_live_bytes <= max_resident_bytes`, asserted at
//! construction). The **live-logical-byte budget** caps the sum of live block
//! lengths and is returned by `release` — the operator working-set contract.
//! The **resident-byte limit** caps the sum of `Vec::capacity()` over
//! retained chunks — charged per chunk at creation, at the capacity the
//! allocator actually accepted (`try_reserve_exact` contractually bounds
//! capacity only from below) — and is never returned before the region ends:
//! chunks are never freed early, because the allocator site's provenance
//! argument rests on their stability. Padding lives inside charged chunks, so
//! the resident number is the honest retained footprint, and it is the value
//! admission control binds to the plan's `resident_bytes` axis. Region
//! metadata and allocator-internal slack beyond `Vec::capacity` are outside
//! the number by design. [`RegionAudit`] reports `chunks_allocated`,
//! `peak_resident_bytes`, and `alignment_padding_bytes` so fragmentation is
//! an audited quantity rather than an invisible tax.
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
