//! Strata's tiered-storage components, sharing the existing Tier-D authority.
//!
//! Resident memory admission and immutable extent residency are explicit here;
//! they are not wired into every database read merely by exporting the modules.
//! A generation lease and a buffer pin are separate lifetimes. No new manifest,
//! durable object kind, or graph visibility authority is invented by this layer.

pub mod buffer;
pub mod memory;
