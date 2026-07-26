//! `fgdb-unsafe-simd` — the SIMD/vector unsafe boundary island
//! (bead `fgdb-w1-unsafe-islands-eqrq`; plan §1 constraint 2, §8.7, §18.1).
//!
//! # Why this crate is a separate crate
//!
//! AGENTS.md constraint 2 makes memory safety *structural*: every ordinary
//! crate root and the workspace default use `unsafe_code = "forbid"`, and
//! Rust's `forbid` cannot be lowered by an inner `allow`. That is not a
//! stylistic preference — it is what makes "this database is memory-safe Rust"
//! a checkable claim rather than a slogan. The consequence is that raw-pointer
//! and vector-intrinsic work cannot live in an ordinary crate at all. It lives
//! here, in a separately named island whose manifest omits the inherited lint
//! table and whose root uses [`deny`] instead, with narrowly scoped
//! `allow(unsafe_code)` sites that are each enumerated in
//! `registries/unsafe_boundary_ledger.toml`.
//!
//! `deny` can be lowered — that is the difference from `forbid`, and it is why
//! the ledger and its checker exist. `unsafe-ledger-check` scans this crate,
//! finds every relaxation structurally (including a `cfg_attr`-wrapped one,
//! which is how [`control_group::prefetch_controls`] is written), and fails CI
//! if any site lacks a matching row or any row outlives its site.
//!
//! # What a site must carry
//!
//! Each one:
//!
//! * a `// SAFETY:` note stating the local invariant the site relies on;
//! * a ledger row with that invariant, the evidence that exercises it, the
//!   fallback it must agree with, and its `no_claim_boundary` — what the site
//!   does *not* guarantee, so a Miri-clean run cannot be inflated into a proof;
//! * a **bit-identical scalar fallback that cross-compiles to every target**
//!   (§8.7). The fallback is not a degraded mode; it is the specification. A
//!   STRICT kernel is *defined* by the portable scalar profile, and every
//!   dispatch path must agree with it bit for bit.
//!
//! # The dispatch matrix
//!
//! [`control_group::COMPILED_PATHS`] enumerates the paths this build actually
//! contains, and [`control_group::classify_via`] runs a named one. That pair
//! exists so the differential harness can iterate the matrix instead of
//! asserting against whatever the current target happens to select — a harness
//! that can only reach one path proves one path.
//!
//! # What is deliberately NOT here
//!
//! No public raw pointers and no public `unsafe fn`: every export is safe to
//! call from a `forbid(unsafe_code)` crate. Prefetch and other
//! memory-level-parallelism policy is *physical only* (§8.7) — it may never
//! change a result or a logical order, so [`control_group::prefetch_controls`]
//! returns nothing and is a no-op wherever the hint does not exist.

#![deny(unsafe_code)]

pub mod control_group;

pub use control_group::{
    CONTROL_GROUP_WIDTH, COMPILED_PATHS, DELETED_CONTROL, DispatchPath, EMPTY_CONTROL, GroupMasks,
    active_path, classify, classify_scalar, classify_via, prefetch_controls,
};
