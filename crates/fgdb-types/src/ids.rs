//! Identity newtypes.
//!
//! Every durable identity in the system is a distinct Rust type so that a
//! `VId` can never flow where an `EId` is expected and an `ObjectId` can
//! never be confused with a digest. Widths follow Appendix A / §5.1:
//! `ObjectId` is the full 256-bit content address as stored in durable
//! records (`[u8;32]` fields like `marker_oid`), vertex/edge logical IDs are
//! 128-bit never-recycled identities (§6.2), and the epoch/sequence scalars
//! are `u64`.

/// 256-bit content-addressed object identity (`[u8;32]` in durable records).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectId(pub [u8; 32]);

impl ObjectId {
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ObjectId(")?;
        for b in &self.0 {
            write!(f, "{b:02x}")?;
        }
        write!(f, ")")
    }
}

macro_rules! u128_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
        pub struct $name(pub u128);
    };
}

macro_rules! u64_scalar {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
        pub struct $name(pub u64);
    };
}

u128_id! {
    /// 128-bit never-recycled logical vertex identity (§6.2).
    VId
}
u128_id! {
    /// 128-bit never-recycled logical edge identity (§6.2).
    EId
}
u128_id! {
    /// Graph identity inside a database.
    GraphId
}
u128_id! {
    /// Branch identity inside a graph.
    BranchId
}

/// 128-bit database identity (`database_id:[u8;16]` in `RootSlot`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct DatabaseId(pub [u8; 16]);

/// 256-bit database security namespace (`[u8;32]` in `RootSlot` and
/// `ConsensusDomain`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct DatabaseSecurityNamespaceId(pub [u8; 32]);

u64_scalar! {
    /// Gap-free global commit sequence assigned by the `WriteCoordinator`.
    CommitSeq
}

/// The gap-free commit-sequence domain has no value after its persisted
/// frontier.
///
/// Exhaustion is a permanent database condition, not an arithmetic detail: a
/// successor may never wrap to the reserved origin or saturate at the current
/// commit. The frontier travels with the error so every layer can preserve the
/// exact refusal evidence in its own error vocabulary.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CommitSeqExhausted {
    pub frontier: CommitSeq,
}

impl core::fmt::Display for CommitSeqExhausted {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "commit sequence space is exhausted at {:?}",
            self.frontier
        )
    }
}

impl core::error::Error for CommitSeqExhausted {}

impl CommitSeq {
    /// The reserved pre-commit frontier.
    pub const ORIGIN: Self = Self(0);

    /// The first assignable commit sequence.
    pub const FIRST: Self = Self(1);

    /// Return the one legal successor without wrapping or saturating.
    pub const fn checked_successor(self) -> Result<Self, CommitSeqExhausted> {
        match self.0.checked_add(1) {
            Some(value) => Ok(Self(value)),
            None => Err(CommitSeqExhausted { frontier: self }),
        }
    }
}

u64_scalar! {
    /// Stream-wide semantic command sequence.
    ///
    /// Transaction commits occupy positions in this domain, but control
    /// commands do too, so it is intentionally distinct from [`CommitSeq`] and
    /// need only advance rather than remain gap-free across commits.
    LogicalCommandSeq
}
u64_scalar! {
    /// Local writer fence epoch (`local_writer_fence_epoch` in `RootSlot`).
    WriterFenceEpoch
}
u64_scalar! {
    /// Service visibility epoch (`service_visibility_epoch` in `RootSlot`).
    ServiceVisibilityEpoch
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_types_are_distinct_and_ordered() {
        // A VId and an EId with equal bits are different types — this is the
        // whole point; the assertions below just pin basic derives.
        let v = VId(7);
        let e = EId(7);
        assert_eq!(v, VId(7));
        assert_eq!(e, EId(7));
        assert!(VId(1) < VId(2));
        assert!(CommitSeq(1) < CommitSeq(2));
        assert!(LogicalCommandSeq(1) < LogicalCommandSeq(2));
        let a = ObjectId([0u8; 32]);
        let mut hi = [0u8; 32];
        hi[0] = 1;
        assert!(a < ObjectId(hi));
        assert_eq!(
            format!("{:?}", ObjectId([0xab; 32])).matches("ab").count(),
            32
        );
    }

    #[test]
    fn commit_sequence_successor_is_exact_and_fail_closed() {
        assert_eq!(CommitSeq::ORIGIN.checked_successor(), Ok(CommitSeq::FIRST));
        assert_eq!(
            CommitSeq(u64::MAX - 1).checked_successor(),
            Ok(CommitSeq(u64::MAX))
        );
        assert_eq!(
            CommitSeq(u64::MAX).checked_successor(),
            Err(CommitSeqExhausted {
                frontier: CommitSeq(u64::MAX)
            })
        );
    }
}
