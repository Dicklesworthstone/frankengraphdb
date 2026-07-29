//! Deterministic in-memory collection kernels for FrankenGraphDB.
//!
//! This crate owns non-durable data structures used by Strata, Loom, and the
//! full-text term dictionary. It intentionally defines no persistent bytes.
//! Safe scalar implementations are the semantic reference for future
//! ledgered SIMD kernels. Resident collection buffers live in task-local
//! `RegionVec` storage so query cancellation has an auditable reclamation path.

#![forbid(unsafe_code)]

pub mod art;
pub mod hash_table;
pub mod levenshtein;
pub mod probe;
pub mod succinct;
