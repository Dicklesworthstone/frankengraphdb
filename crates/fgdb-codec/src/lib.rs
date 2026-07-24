//! Safe scalar codec kernels for FrankenGraphDB.
//!
//! The registry-independent layer contains canonical unsigned LEB128 and
//! delta-varint, checked bitpacking/frame-of-reference and Elias-Fano kernels,
//! deterministic block compression, roaring-style sets, the honest closed
//! neighbor-codec family, type-safe identity-column mechanics, scalar dispatch
//! traits, immutable origin-order keys, canonical bounded identity-payload
//! decode, a registry-independent logical neighbor digest validator, and
//! structured diagnostic evidence. Durable codec identifiers, envelopes, and
//! SIMD implementations remain owned by their still-gated format and
//! unsafe-boundary work.

#![forbid(unsafe_code)]

pub mod bitpack;
pub mod block;
pub mod delta_varint;
pub mod elias_fano;
pub mod evidence;
pub mod identity;
pub mod kernel;
pub mod logical;
pub mod neighbor;
pub mod roaring;
pub mod varint;
