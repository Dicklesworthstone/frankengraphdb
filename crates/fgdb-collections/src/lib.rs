//! Deterministic in-memory collection kernels for FrankenGraphDB.
//!
//! This crate owns non-durable data structures used by Strata, Loom, and the
//! full-text term dictionary. It intentionally defines no persistent bytes.
//! Safe scalar implementations are the semantic reference for future
//! ledgered SIMD kernels and region-heap storage adapters.

#![forbid(unsafe_code)]

pub mod art;
pub mod hash_table;
pub mod levenshtein;
pub mod succinct;
