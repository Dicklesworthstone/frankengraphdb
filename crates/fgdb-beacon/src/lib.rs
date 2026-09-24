//! Beacon's memory-resident search slices. Chronicle remains the only authority.
//!
//! HNSW is an approximate search, not an exact-search certificate. Exact scans
//! are an explicitly selected alternative, never a hidden fallback. Every
//! potentially long operation consumes caller-owned work and can fail without
//! returning a truncated result as complete. No I/O, lock, runtime, background
//! thread, clock, entropy source, or independent transaction manager lives here.
//!
//! This is an implementation subset, not the full Beacon durable-descriptor,
//! authorization, GLA, or larger-than-memory contract. Callers must narrow the
//! input domain before building a slice; result predicates are not ACL checks.

#![forbid(unsafe_code)]

mod bm25;
mod control;
mod exact_fusion;
mod fabric;
mod hnsw;
mod ranking;
pub mod read;

pub use bm25::{Bm25, Bm25Config, Bm25Stats, TextHit, TextMatch};
pub use control::{BeaconError, WorkBudget, WorkControl};
pub use exact_fusion::{ExactHybridHit, ExactHybridQuery, ExactRrfProfile, ExactRrfScore};
pub use fabric::{
    ApplyReport, BeaconIndex, HybridHit, HybridQuery, IndexConfig, IndexDocument, IndexMutation,
    IndexSnapshot, IndexStats,
};
pub use hnsw::{DistanceMetric, Hnsw, HnswConfig, Neighbor, VectorSearch};
