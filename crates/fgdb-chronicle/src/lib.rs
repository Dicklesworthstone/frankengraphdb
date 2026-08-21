//! fgdb-chronicle — the landed content-addressed durability substrate (bet B1).
//!
//! Chronicle currently provides an append-only commit stream whose durable
//! objects are authenticated, content-addressed, and RaptorQ-coded. The landed
//! implementation includes:
//!
//! - [`identity`] and [`symbol`]: the noncircular object-identity pipeline and
//!   authenticated durable `SymbolRecord` framing;
//! - [`symbolize`] and [`capsule`]: asupersync-backed RaptorQ symbolization,
//!   capsule sealing/container framing, erasure recovery, and fail-closed
//!   recomputation of the requested `ObjectId`;
//! - [`marker`]: canonical `CommitMarker` encoding, hash-chained history, and
//!   compare-and-swap head updates for the landed local effect source;
//! - [`commit`] and [`store`]: [`CommitCoordinator`], its sole-writer lock,
//!   validation seam, the capsule-D1/marker-D2 two-fsync protocol, bounded
//!   capsule reads, torn-tail recovery, orphan discovery, and directory
//!   durability barriers;
//! - [`root`]: the two-slot `manifest.root` format, authenticated bootstrap,
//!   selection/recovery rules, and durable root publication support;
//! - [`scrub`]: authenticated symbol inspection and repair-budget-aware scrub
//!   verdicts for encoded objects;
//! - [`pack`]: deterministic packing metadata for protected object groups.
//!
//! DELIBERATELY ABSENT: retention cooling; `BranchManifest` and product-level
//! database branches; replication; a real SSI validator (the landed
//! [`PassThroughValidator`] is only the coordinator's validation seam); and
//! capsule sealing of Strata objects. The marker/head primitives above do not
//! by themselves claim those product capabilities.
#![forbid(unsafe_code)]

pub mod capsule;
pub mod commit;
pub mod identity;
pub mod marker;
pub mod pack;
pub mod root;
pub mod scrub;
pub mod store;
pub mod symbol;
pub mod symbolize;
pub mod validate;

pub use commit::{CommitCoordinator, CommitError, CrashPoint};
pub use identity::{
    CipherDescriptor, CryptoVerificationEvent, CryptoVerificationSink, EncodedObject,
    EncodingDescriptor, IdentifiedObject, IdentityMismatch, LocationForm, PlacedObject,
    PlacementDescriptor, ProtectedObject, RecoveredObjectError, VerificationFailureClass,
    VerificationOperation, VerificationOutcome,
};
pub use marker::{
    ChainError, ChainedMarker, CommitMarker, EffectSource, HeadCasMismatch, HeadUpdate, MarkerChain,
};
pub use pack::{
    DomainAxis, PackBuilder, PackDomain, PackError, PackProtectionProfile, PackedObjectGroup,
    SubobjectLocator, WriteKeyDomain,
};
pub use root::{
    IdentityTuple, RootBootstrap, RootRecoveryError, RootSelection, RootSlot, SlotError,
    recover_root_object, select_root,
};
pub use scrub::{LostReason, ScrubReport, ScrubVerdict, scrub_object};
pub use store::{
    ContinuityAuthority, ContinuityHead, RootPublicationEvidence, RootStore, StoreError,
};
pub use symbol::{SymbolError, SymbolRecord};
pub use symbolize::{RecoveryTarget, SymbolizeError, decode_object, encode_object};
pub use validate::{CommitDraft, CommitValidator, PassThroughValidator, ValidationRejection};
