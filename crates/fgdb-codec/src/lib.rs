//! Safe scalar codec kernels for FrankenGraphDB.
//!
//! This initial registry-independent slice implements canonical unsigned
//! LEB128, checked bitpacking/frame-of-reference coding, and Elias-Fano
//! monotone sequences. Durable codec identifiers and envelopes remain owned by
//! the generated format layer; this crate defines only their reusable scalar
//! mechanics.

#![forbid(unsafe_code)]

pub mod bitpack;
pub mod block;
pub mod delta_varint;
pub mod elias_fano;
pub mod neighbor;
pub mod roaring;
pub mod varint;
