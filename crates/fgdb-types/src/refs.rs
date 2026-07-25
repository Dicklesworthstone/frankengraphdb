//! The typed reference family (Appendix A "Reference semantics", plan ~§1394).
//!
//! Every `ObjectId`-bearing edge in the durable graph declares its retention
//! semantics *in its type*:
//!
//! - [`StrongRef<T>`] — always followed; retains its target.
//! - [`ConditionalCoordinateRef<T>`] — sequence-neutral payload whose
//!   retention sequence comes from its enclosing committed marker; pending /
//!   prepared traversal treats it as strong because no committed-marker cut
//!   context exists yet.
//! - [`ConditionalMarkerRef`] — followed until an authenticated matching
//!   checkpoint/cut on its axis.
//! - [`WeakDigest<T>`] — comparison only; never a reachability edge.
//! - [`MarkerRef`] / [`CommandRef`] — **identities, not reachability by
//!   themselves** (the a01 law): they deliberately do *not* carry a type
//!   parameter or implement any traversal trait; an enclosing tagged
//!   reference supplies reachability.
//!
//! Type parameters are anchored to durable object kinds through
//! [`LogicalObjectKind`], whose `OBJECT_KIND` codes must match rows in
//! `registries/logical_object_kinds.toml`. Target types live in the crates
//! that own their formats; this crate only defines the reference machinery.

use std::marker::PhantomData;

use crate::ids::{BranchId, CommitSeq, GraphId, ObjectId};

const LOGICAL_OBJECT_KIND_REGISTRY: &[u8] =
    include_bytes!("../../../registries/logical_object_kinds.toml");
const ACTIVE_STATUS_ROW: &[u8] = b"status = \"active\"";

const fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.len() > haystack.len() {
        return false;
    }

    let mut start = 0;
    while start <= haystack.len() - needle.len() {
        let mut offset = 0;
        // ubs:ignore — the outer range and offset guard prove both indexes in bounds.
        while offset < needle.len() && haystack[start + offset] == needle[offset] {
            offset += 1;
        }
        if offset == needle.len() {
            return true;
        }
        start += 1;
    }
    false
}

const fn count_bytes(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() || needle.len() > haystack.len() {
        return 0;
    }

    let mut count = 0;
    let mut start = 0;
    while start <= haystack.len() - needle.len() {
        let mut offset = 0;
        // ubs:ignore — the outer range and offset guard prove both indexes in bounds.
        while offset < needle.len() && haystack[start + offset] == needle[offset] {
            offset += 1;
        }
        if offset == needle.len() {
            count += 1;
            start += needle.len();
        } else {
            start += 1;
        }
    }
    count
}

macro_rules! active_logical_object_kinds {
    ($($variant:ident = $code:literal => $name:literal),+ $(,)?) => {
        /// Closed descriptor for every active row in
        /// `registries/logical_object_kinds.toml`.
        ///
        /// Code and name are one value, so a [`LogicalObjectKind`] implementation
        /// cannot pair a registered code with the wrong name or invent an
        /// unregistered active code. The declarations below are checked against
        /// the generated registry projection during compilation using only
        /// `include_bytes!` and const evaluation.
        #[repr(u16)]
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
        pub enum LogicalObjectKindCode {
            $($variant = $code),+
        }

        impl LogicalObjectKindCode {
            /// Active kinds in canonical registry-code order.
            pub const ALL_ACTIVE: &'static [Self] = &[$(Self::$variant),+];

            /// The exact registered `object_kind` value.
            pub const fn code(self) -> u16 {
                self as u16
            }

            /// The exact registered name paired with this code.
            pub const fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => $name),+
                }
            }

            /// Decodes an active registered code. Reserved and unknown codes
            /// remain unavailable until their registry row is activated.
            pub const fn from_code(code: u16) -> Option<Self> {
                match code {
                    $($code => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }

        $(
            const _: () = assert!(contains_bytes(
                LOGICAL_OBJECT_KIND_REGISTRY,
                concat!(
                    "object_kind = ",
                    stringify!($code),
                    "\nname = \"",
                    $name,
                    "\"\nstatus = \"active\""
                )
                .as_bytes(),
            ));
        )+

        const _: () = assert!(
            count_bytes(LOGICAL_OBJECT_KIND_REGISTRY, ACTIVE_STATUS_ROW)
                == LogicalObjectKindCode::ALL_ACTIVE.len()
        );
    };
}

active_logical_object_kinds! {
    LogicalStatePayload = 0x0001 => "LogicalStatePayload",
    LogicalCommandRecord = 0x0002 => "LogicalCommandRecord",
    LogicalStateRoot = 0x0003 => "LogicalStateRoot",
    CommitCommand = 0x0004 => "CommitCommand",
    ControlCommand = 0x0005 => "ControlCommand",
    CommitMarker = 0x0006 => "CommitMarker",
    RootManifest = 0x0007 => "RootManifest",
    AuthorityBindingRecord = 0x0008 => "AuthorityBindingRecord",
    CommitCapsule = 0x000a => "CommitCapsule",
    PreparedCommitRecord = 0x000b => "PreparedCommitRecord",
}

/// Implemented by every durable logical object type that references can
/// target. Implementors select one closed [`LogicalObjectKindCode`] value, so
/// code/name consistency and active registry membership are compile-time
/// properties rather than parallel raw constants.
///
/// ```compile_fail,E0308
/// use fgdb_types::{LogicalObjectKind, LogicalObjectKindCode};
///
/// struct UnregisteredObject;
///
/// impl LogicalObjectKind for UnregisteredObject {
///     const OBJECT_KIND: LogicalObjectKindCode = 0xffff;
/// }
/// ```
pub trait LogicalObjectKind {
    const OBJECT_KIND: LogicalObjectKindCode;
}

macro_rules! fmt_ref_debug {
    ($name:literal) => {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, concat!($name, "<{}>"), T::OBJECT_KIND.name())
        }
    };
}

/// Always-followed retaining reference to a `T`.
pub struct StrongRef<T: LogicalObjectKind> {
    oid: ObjectId,
    _target: PhantomData<fn() -> T>,
}

impl<T: LogicalObjectKind> StrongRef<T> {
    pub const fn new(oid: ObjectId) -> Self {
        StrongRef {
            oid,
            _target: PhantomData,
        }
    }

    pub const fn oid(&self) -> ObjectId {
        self.oid
    }

    /// The registered kind code of the target type.
    pub const fn target_kind() -> u16 {
        T::OBJECT_KIND.code()
    }
}

// Manual impls: derive would bound them on `T: Clone` etc., but the phantom
// target type never affects the value.
impl<T: LogicalObjectKind> Clone for StrongRef<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: LogicalObjectKind> Copy for StrongRef<T> {}
impl<T: LogicalObjectKind> PartialEq for StrongRef<T> {
    fn eq(&self, other: &Self) -> bool {
        self.oid == other.oid
    }
}
impl<T: LogicalObjectKind> Eq for StrongRef<T> {}
impl<T: LogicalObjectKind> std::hash::Hash for StrongRef<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.oid.hash(state);
    }
}
impl<T: LogicalObjectKind> std::fmt::Debug for StrongRef<T> {
    fmt_ref_debug!("StrongRef");
}

/// Sequence-neutral conditional reference: the payload names its target and
/// branch coordinates, and the *enclosing committed marker* supplies the
/// retention sequence.
pub struct ConditionalCoordinateRef<T: LogicalObjectKind> {
    oid: ObjectId,
    graph: GraphId,
    branch: BranchId,
    _target: PhantomData<fn() -> T>,
}

impl<T: LogicalObjectKind> ConditionalCoordinateRef<T> {
    pub const fn new(oid: ObjectId, graph: GraphId, branch: BranchId) -> Self {
        ConditionalCoordinateRef {
            oid,
            graph,
            branch,
            _target: PhantomData,
        }
    }
    pub const fn oid(&self) -> ObjectId {
        self.oid
    }
    pub const fn graph(&self) -> GraphId {
        self.graph
    }
    pub const fn branch(&self) -> BranchId {
        self.branch
    }
}

impl<T: LogicalObjectKind> Clone for ConditionalCoordinateRef<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: LogicalObjectKind> Copy for ConditionalCoordinateRef<T> {}
impl<T: LogicalObjectKind> PartialEq for ConditionalCoordinateRef<T> {
    fn eq(&self, other: &Self) -> bool {
        (self.oid, self.graph, self.branch) == (other.oid, other.graph, other.branch)
    }
}
impl<T: LogicalObjectKind> Eq for ConditionalCoordinateRef<T> {}
impl<T: LogicalObjectKind> std::hash::Hash for ConditionalCoordinateRef<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (self.oid, self.graph, self.branch).hash(state);
    }
}
impl<T: LogicalObjectKind> std::fmt::Debug for ConditionalCoordinateRef<T> {
    fmt_ref_debug!("ConditionalCoordinateRef");
}

/// Comparison-only digest of a `T`; never followed, never retains.
pub struct WeakDigest<T: LogicalObjectKind> {
    digest: [u8; 32],
    _target: PhantomData<fn() -> T>,
}

impl<T: LogicalObjectKind> WeakDigest<T> {
    pub const fn new(digest: [u8; 32]) -> Self {
        WeakDigest {
            digest,
            _target: PhantomData,
        }
    }
    pub const fn digest(&self) -> &[u8; 32] {
        &self.digest
    }
}

impl<T: LogicalObjectKind> Clone for WeakDigest<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: LogicalObjectKind> Copy for WeakDigest<T> {}
impl<T: LogicalObjectKind> PartialEq for WeakDigest<T> {
    fn eq(&self, other: &Self) -> bool {
        self.digest == other.digest // ubs:ignore — non-secret provenance, not an auth token.
    }
}
impl<T: LogicalObjectKind> Eq for WeakDigest<T> {}
impl<T: LogicalObjectKind> std::hash::Hash for WeakDigest<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.digest.hash(state);
    }
}
impl<T: LogicalObjectKind> std::fmt::Debug for WeakDigest<T> {
    fmt_ref_debug!("WeakDigest");
}

/// Bare marker identity: `{marker_oid, commit_seq}`. **Not reachability by
/// itself** — an enclosing tagged reference supplies that (a01 law). This
/// type therefore exposes no `oid()`-style traversal accessor naming and no
/// target type parameter.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct MarkerRef {
    pub marker_oid: ObjectId,
    pub commit_seq: CommitSeq,
}

/// Bare command identity: `{command_record_oid, logical_command_seq}`.
/// Like [`MarkerRef`], an identity — never a retention edge on its own.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct CommandRef {
    pub command_record_oid: ObjectId,
    pub logical_command_seq: u64,
}

/// Cut axis for a [`ConditionalMarkerRef`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ConditionalMarkerAxis {
    /// Followed until a verified matching global checkpoint cut.
    Global,
    /// Followed until a verified matching cut on one branch's axis.
    Branch { graph: GraphId, branch: BranchId },
}

/// Marker reference followed until an authenticated matching checkpoint/cut
/// on its declared axis.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ConditionalMarkerRef {
    pub marker: MarkerRef,
    pub axis: ConditionalMarkerAxis,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    struct CommitCapsule;
    impl LogicalObjectKind for CommitCapsule {
        const OBJECT_KIND: LogicalObjectKindCode = LogicalObjectKindCode::CommitCapsule;
    }

    struct CommitMarker;
    impl LogicalObjectKind for CommitMarker {
        const OBJECT_KIND: LogicalObjectKindCode = LogicalObjectKindCode::CommitMarker;
    }

    fn oid(fill: u8) -> ObjectId {
        ObjectId([fill; 32])
    }

    #[test]
    fn strong_refs_to_different_kinds_are_different_types() {
        let a: StrongRef<CommitCapsule> = StrongRef::new(oid(1));
        let b: StrongRef<CommitMarker> = StrongRef::new(oid(1));
        // Same oid, different target kind: comparing them is a compile error
        // (uncomment to verify), and the kinds are observably distinct.
        // assert_eq!(a, b);
        assert_ne!(
            StrongRef::<CommitCapsule>::target_kind(),
            StrongRef::<CommitMarker>::target_kind()
        );
        assert_eq!(StrongRef::<CommitCapsule>::target_kind(), 0x000a);
        assert_eq!(a.oid(), b.oid());
        assert_eq!(format!("{a:?}"), "StrongRef<CommitCapsule>");
    }

    #[test]
    fn active_kind_descriptors_round_trip_in_registry_order() {
        let mut previous = None;
        for &kind in LogicalObjectKindCode::ALL_ACTIVE {
            assert_eq!(LogicalObjectKindCode::from_code(kind.code()), Some(kind));
            assert!(!kind.name().is_empty());
            if let Some(previous) = previous {
                assert!(previous < kind.code());
            }
            previous = Some(kind.code());
        }
        assert_eq!(LogicalObjectKindCode::from_code(0x0009), None);
        assert_eq!(LogicalObjectKindCode::from_code(0xffff), None);
    }

    #[test]
    fn reference_equality_and_hash_are_value_based() {
        let a: StrongRef<CommitCapsule> = StrongRef::new(oid(9));
        let b: StrongRef<CommitCapsule> = StrongRef::new(oid(9));
        assert_eq!(a, b);
        let mut h1 = DefaultHasher::new();
        let mut h2 = DefaultHasher::new();
        a.hash(&mut h1);
        b.hash(&mut h2);
        assert_eq!(h1.finish(), h2.finish());
    }

    #[test]
    fn marker_and_command_identities_carry_no_traversal_surface() {
        // The a01 law in type form: MarkerRef/CommandRef expose only their
        // identity fields; only the enclosing tagged types add axis/traversal
        // meaning.
        let m = MarkerRef {
            marker_oid: oid(3),
            commit_seq: CommitSeq(41999),
        };
        let c = ConditionalMarkerRef {
            marker: m,
            axis: ConditionalMarkerAxis::Global,
        };
        assert_eq!(c.marker, m);
        let br = ConditionalMarkerRef {
            marker: m,
            axis: ConditionalMarkerAxis::Branch {
                graph: GraphId(1),
                branch: BranchId(2),
            },
        };
        assert_ne!(c, br, "axis participates in identity");
        let cr = CommandRef {
            command_record_oid: oid(4),
            logical_command_seq: 7,
        };
        assert_eq!(cr, cr);
    }

    #[test]
    fn coordinate_ref_identity_includes_coordinates() {
        let x: ConditionalCoordinateRef<CommitCapsule> =
            ConditionalCoordinateRef::new(oid(5), GraphId(1), BranchId(1));
        let y: ConditionalCoordinateRef<CommitCapsule> =
            ConditionalCoordinateRef::new(oid(5), GraphId(1), BranchId(2));
        assert_ne!(x, y);
        let w: WeakDigest<CommitCapsule> = WeakDigest::new([7; 32]);
        assert_eq!(w, WeakDigest::new([7; 32]));
    }
}
