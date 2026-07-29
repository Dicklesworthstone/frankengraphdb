//! fgdb-chronicle — the content-addressed durability substrate (bet B1).
//!
//! Chronicle is the "One Version Universe": MVCC versions, time-travel
//! history, replication, change subscriptions, and git-style branches are all
//! the same append-only, content-addressed, RaptorQ-coded commit stream. This
//! crate lands that substrate in dependency order, and the first thing every
//! later layer needs is object identity.
//!
//! LANDED SO FAR:
//! - [`identity`] — plan §5.1's noncircular identity pipeline: keyed
//!   `ObjectId` → object AEAD → `CiphertextId` → `EncodingId` →
//!   `PlacementId`, as four types where each can only be built from the
//!   previous one.
//! - [`symbol`] — the durable `SymbolRecord` wire format and its total
//!   authentication transcript, verifiable only against an authenticated
//!   encoding.
//!
//! Later increments add the RaptorQ symbolization itself (consuming
//! asupersync's RFC 6330 implementation), the `CommitCapsule`/`CommitMarker`
//! two-fsync protocol, the `WriteCoordinator`, retention tiers, and branches.
#![forbid(unsafe_code)]

pub mod identity;
pub mod pack;
pub mod root;
pub mod scrub;
pub mod symbol;
pub mod symbolize;

pub use identity::{
    CipherDescriptor, EncodedObject, EncodingDescriptor, IdentifiedObject, LocationForm,
    PlacedObject, PlacementDescriptor, ProtectedObject,
};
pub use pack::{
    DomainAxis, PackBuilder, PackDomain, PackError, PackedObjectGroup, SubobjectLocator,
    WriteKeyDomain,
};
pub use root::{IdentityTuple, RootBootstrap, RootSelection, RootSlot, SlotError, select_root};
pub use scrub::{LostReason, ScrubReport, ScrubVerdict, scrub_object};
pub use symbol::{SymbolError, SymbolRecord};
pub use symbolize::{RecoveryTarget, SymbolizeError, decode_object, encode_object};
