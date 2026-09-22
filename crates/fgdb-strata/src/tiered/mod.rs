//! Strata's tiered-storage components, sharing the existing Tier-D authority.
//!
//! Inline/CSR adjacency images are sealed from authenticated stored partitions.
//! Their opaque anchors authorize reload without pinning resident image bytes.
//! The existing manifest remains authoritative; automatic run publication and
//! database-wide extent routing are not implied by these component exports.

pub mod buffer;
pub mod inline;
pub mod memory;
pub mod sealed;
