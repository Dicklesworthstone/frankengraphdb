//! Beacon's memory-resident search slices. Chronicle remains the only authority.
//!
//! HNSW is approximate, not an exact-search certificate. Exact scans are an
//! explicitly selected alternative, never a hidden fallback. Every potentially
//! long operation consumes caller-owned work and can fail without returning a
//! truncated result as complete. No I/O, lock, runtime, background thread, clock,
//! entropy source, or independent transaction manager lives here.
//!
//! Callers must narrow the input domain before building a slice; result
//! predicates are not authorization checks. Durable descriptors, secure-view
//! bindings and larger-than-memory execution are separate integration work.

#![forbid(unsafe_code)]

mod control;
mod hnsw;
mod ranking;

pub use control::{BeaconError, WorkBudget, WorkControl};
pub use hnsw::{DistanceMetric, Hnsw, HnswConfig, Neighbor, VectorSearch};
