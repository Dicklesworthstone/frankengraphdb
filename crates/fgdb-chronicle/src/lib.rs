//! fgdb-chronicle — the content-addressed durability substrate (bet B1).
//!
//! Chronicle is the "One Version Universe": MVCC versions, time-travel
//! history, replication, change subscriptions, and git-style branches are all
//! the same append-only, content-addressed, RaptorQ-coded commit stream. This
//! crate lands that substrate in dependency order, and the first thing every
//! later layer needs is object identity.
//!
//! THIS INCREMENT: plan §5.1's noncircular identity pipeline
//! ([`identity`]) — keyed `ObjectId` → object AEAD → `CiphertextId` →
//! `EncodingId` → `PlacementId`, as four types where each can only be built
//! from the previous one. Later increments add the RaptorQ symbolization and
//! `SymbolRecord` wire format, the `CommitCapsule`/`CommitMarker` two-fsync
//! protocol, the `WriteCoordinator`, retention tiers, and branches.
#![forbid(unsafe_code)]

pub mod identity;

pub use identity::{
    CipherDescriptor, EncodedObject, EncodingDescriptor, IdentifiedObject, LocationForm,
    PlacedObject, PlacementDescriptor, ProtectedObject,
};
