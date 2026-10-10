use crate::{
    BoundPlan, Database, EdgeRecord, GqlError, PendingRow, PreparedWrite, ReadError, RelationBind,
    VertexRow, WriteBatch, WriteError,
};
use asupersync::fs::Vfs;
use fgdb_delta_types::{ElementId, LabelId, RelationId};
use fgdb_strata::AdjacencyEntry;
use fgdb_types::{
    Acquired, CanonicalScalar, CommitCx, CommitSeq, EId, EmbeddedTxnCompletion, EmbeddedTxnState,
    ObligationAcquireError, ObligationId, PurposeObligation, TxnCx, VId,
};

/// The keyed permutation that turns an engine allocation counter into an
/// identity (fgdb-hxgm1 channel 2, owner ruling 2026-10-09).
///
/// A sequential counter leaks how many records were created between two of a
/// capability's own creations, including records it cannot see. Issuing
/// `P_k(counter)` instead hides those counts computationally: without the key,
/// consecutive identities carry no usable order.
///
/// The construction is a four-round unbalanced Feistel network over a 63-bit
/// block (31- and 32-bit halves whose widths alternate each round), with a
/// keyed-BLAKE3 round function. It permutes `[0, 2^63)`, and cycle-walking
/// past 0 restricts it to `[1, 2^63)`. Every engine identity therefore fits an
/// i64, the width of openCypher id() and Bolt node ids, and an evaluation
/// costs four keyed hashes. Vertices and edges use separate keys derived from
/// the database's object-identity key (k_oid), so identities are reproducible
/// under the same keys (B5) and unrelated across databases. No new primitive:
/// BLAKE3 is the only one.
#[derive(Clone)]
pub struct IdentityPermutation {
    key: [u8; 32],
}

impl core::fmt::Debug for IdentityPermutation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("IdentityPermutation([REDACTED])")
    }
}

impl IdentityPermutation {
    /// The largest counter, and the largest identity, the permutation maps:
    /// `2^63 - 1`. The domain is `[1, MAX]`.
    pub const MAX: u64 = i64::MAX as u64;
    const ROUNDS: u8 = 4;

    /// The vertex permutation under `keys`.
    #[must_use]
    pub fn vertices(keys: &crate::DatabaseKeys) -> Self {
        Self {
            key: fgdb_crypto::derive_key("fgdb:identity-permutation:v1:vertex", keys.k_oid()),
        }
    }

    /// The edge permutation under `keys`.
    #[must_use]
    pub fn edges(keys: &crate::DatabaseKeys) -> Self {
        Self {
            key: fgdb_crypto::derive_key("fgdb:identity-permutation:v1:edge", keys.k_oid()),
        }
    }

    /// The round function, reduced to `width` bits.
    fn round(&self, round: u8, half: u32, width: u32) -> u32 {
        let mut input = [0u8; 5];
        input[0] = round;
        input[1..].copy_from_slice(&half.to_be_bytes());
        let digest = fgdb_crypto::keyed_hash(&self.key, &input);
        u32::from_be_bytes([digest.0[0], digest.0[1], digest.0[2], digest.0[3]])
            & (u32::MAX >> (32 - width))
    }

    /// The width of the left half entering `round`: 31 bits on even rounds,
    /// 32 on odd ones. The right half has the other width. Four rounds
    /// return the halves to the 31/32 layout they started in.
    fn left_width(round: u8) -> u32 {
        if round.is_multiple_of(2) { 31 } else { 32 }
    }

    /// One pass over `[0, 2^63)`: the top 31 bits enter as the left half.
    fn encrypt(&self, block: u64) -> u64 {
        let (mut left, mut right) = ((block >> 32) as u32, block as u32);
        for round in 0..Self::ROUNDS {
            let f = self.round(round, right, Self::left_width(round));
            (left, right) = (right, left ^ f);
        }
        (u64::from(left) << 32) | u64::from(right)
    }

    /// The inverse of [`Self::encrypt`]. Undoing a round recovers the left
    /// half that entered it, at that round's left width.
    fn decrypt(&self, block: u64) -> u64 {
        let (mut left, mut right) = ((block >> 32) as u32, block as u32);
        for round in (0..Self::ROUNDS).rev() {
            let f = self.round(round, left, Self::left_width(round));
            (left, right) = (right ^ f, left);
        }
        (u64::from(left) << 32) | u64::from(right)
    }

    fn in_domain(value: u64) -> bool {
        (1..=Self::MAX).contains(&value)
    }

    /// The identity issued for `counter`, or `None` outside `[1, MAX]`.
    /// The block permutation maps exactly one value to 0, so cycle-walking
    /// takes a second step for one counter in 2^63. It terminates because
    /// `counter` lies on a cycle of the block permutation that re-enters the
    /// domain.
    #[must_use]
    pub fn permute(&self, counter: u64) -> Option<u64> {
        if !Self::in_domain(counter) {
            return None;
        }
        let mut value = self.encrypt(counter);
        while !Self::in_domain(value) {
            value = self.encrypt(value);
        }
        Some(value)
    }

    /// The counter that issued `identity`, or `None` outside `[1, MAX]`.
    #[must_use]
    pub fn invert(&self, identity: u64) -> Option<u64> {
        if !Self::in_domain(identity) {
            return None;
        }
        let mut value = self.decrypt(identity);
        while !Self::in_domain(value) {
            value = self.decrypt(value);
        }
        Some(value)
    }
}

/// Per-open-handle engine identity reservations (fgdb-hxgm1 channel 2).
///
/// Each kind keeps a counter, and an engine allocation issues
/// [`IdentityPermutation::permute`] of the next one. A counter never rewinds
/// within this handle, so aborted work never reissues. Every engine commit
/// records the counters in its Chronicle marker, and open reads them back from
/// the last marker that carries them; so an identity once committed is never
/// reissued, even after its record is deleted and compacted away. An issued
/// identity that a client chose explicitly is skipped by an existence check
/// against the writer's spent sets.
pub(crate) struct IdentityAllocation {
    counters: fgdb_chronicle::IdentityCounters,
    vertices: IdentityPermutation,
    edges: IdentityPermutation,
}

impl IdentityAllocation {
    /// Seed from the last marker that records counters. A stream written
    /// before the field existed starts at zero: its sequential identities sit
    /// in the low range, and the existence check skips any the permutation
    /// would reissue.
    pub(crate) fn from_chain(
        chain: &fgdb_chronicle::MarkerChain,
        keys: &crate::DatabaseKeys,
    ) -> Self {
        Self {
            counters: chain
                .entries()
                .iter()
                .rev()
                .find_map(|entry| entry.marker.identity_counters)
                .unwrap_or_default(),
            vertices: IdentityPermutation::vertices(keys),
            edges: IdentityPermutation::edges(keys),
        }
    }

    /// The counters the next commit records.
    pub(crate) fn counters(&self) -> fgdb_chronicle::IdentityCounters {
        self.counters
    }

    /// Issue the next identity of one kind that `spent` does not already
    /// hold. Each skipped collision consumes its counter.
    fn issue(&mut self, vertex: bool, spent: impl Fn(u128) -> bool) -> Result<u128, WriteTxnError> {
        let (counter, permutation) = if vertex {
            (&mut self.counters.vertex, &self.vertices)
        } else {
            (&mut self.counters.edge, &self.edges)
        };
        loop {
            *counter = counter
                .checked_add(1)
                .filter(|next| *next <= IdentityPermutation::MAX)
                .ok_or(WriteTxnError::IdentityExhausted)?;
            let identity = u128::from(
                permutation
                    .permute(*counter)
                    .ok_or(WriteTxnError::IdentityExhausted)?,
            );
            if !spent(identity) {
                return Ok(identity);
            }
        }
    }
}

/// The vertex identity the engine issues for `counter` under `keys`: what a
/// law expects in place of a literal sequential id (fgdb-hxgm1 channel 2).
#[cfg(test)]
pub(crate) fn engine_vertex(keys: &crate::DatabaseKeys, counter: u64) -> VId {
    VId(u128::from(
        IdentityPermutation::vertices(keys)
            .permute(counter)
            .expect("an engine counter in [1, 2^63)"),
    ))
}

/// The edge counterpart of [`engine_vertex`].
#[cfg(test)]
pub(crate) fn engine_edge(keys: &crate::DatabaseKeys, counter: u64) -> EId {
    EId(u128::from(
        IdentityPermutation::edges(keys)
            .permute(counter)
            .expect("an engine counter in [1, 2^63)"),
    ))
}

#[cfg(test)]
mod identity_allocation_tests {
    use super::*;
    use fgdb_chronicle::{CommitMarker, EffectSource, IdentityCounters, MarkerChain};
    use fgdb_types::DatabaseSecurityNamespaceId;

    fn keys() -> crate::DatabaseKeys {
        crate::DatabaseKeys::new(
            [0x5a; 32],
            DatabaseSecurityNamespaceId([0x5b; 32]),
            [0x5c; 32],
        )
    }

    fn marker(seq: u64, counters: Option<IdentityCounters>) -> CommitMarker {
        let marker = CommitMarker {
            logical_command_seq: seq,
            commit_seq: seq,
            effect_source: EffectSource::Local {
                capsule_ref: fgdb_types::ObjectId([0x31; 32]),
                logical_delta_template_digest: fgdb_crypto::Digest([0x32; 32]),
            },
            prev_global: None,
            head_updates: Vec::new(),
            merge_record_oid: None,
            coordinate_schema_transition_digest: fgdb_crypto::Digest([0x33; 32]),
            topology_epoch: 1,
            policy_epoch: 1,
            revocation_index: 1,
            txn_token: [0x34; 16],
            commit_hlc: seq,
            final_effect_digest: fgdb_crypto::Digest([0x35; 32]),
            authorization_decision_digest: fgdb_crypto::Digest([0x36; 32]),
            resource_effect_digest: fgdb_crypto::Digest([0x37; 32]),
            payload_availability_certificate_oid: None,
            flags: 0,
            identity_counters: None,
        };
        match counters {
            Some(counters) => marker.with_identity_counters(counters),
            None => marker,
        }
    }

    fn chain(markers: Vec<CommitMarker>) -> MarkerChain {
        let mut chain = MarkerChain::new();
        for marker in markers {
            chain.append(marker).unwrap();
        }
        chain
    }

    /// The last marker that records counters seeds the allocator. Later
    /// markers without them (raw or pre-field commits) do not reset it, and
    /// a stream that never recorded any starts at zero.
    #[test]
    fn open_seeds_from_the_last_marker_that_records_counters() {
        let recorded = IdentityCounters { vertex: 9, edge: 4 };
        let seeded = IdentityAllocation::from_chain(
            &chain(vec![
                marker(1, Some(IdentityCounters { vertex: 2, edge: 1 })),
                marker(2, Some(recorded)),
                marker(3, None),
            ]),
            &keys(),
        );
        assert_eq!(seeded.counters(), recorded);
        let legacy =
            IdentityAllocation::from_chain(&chain(vec![marker(1, None), marker(2, None)]), &keys());
        assert_eq!(legacy.counters(), IdentityCounters::default());
        assert_eq!(
            IdentityAllocation::from_chain(&MarkerChain::new(), &keys()).counters(),
            IdentityCounters::default()
        );
    }

    /// The last counter issues the last identity, and the allocator then
    /// refuses without wrapping or moving its counter. A skipped collision
    /// consumes its counter.
    #[test]
    fn issue_skips_spent_identities_and_exhausts_at_the_domain_bound() {
        let permutation = IdentityPermutation::vertices(&keys());
        let mut allocation = IdentityAllocation::from_chain(&MarkerChain::new(), &keys());
        let first = u128::from(permutation.permute(1).unwrap());
        let second = u128::from(permutation.permute(2).unwrap());
        assert_eq!(allocation.issue(true, |id| id == first).unwrap(), second);
        assert_eq!(allocation.counters().vertex, 2);
        assert_eq!(allocation.counters().edge, 0);

        allocation.counters.vertex = IdentityPermutation::MAX - 1;
        assert_eq!(
            allocation.issue(true, |_| false).unwrap(),
            u128::from(permutation.permute(IdentityPermutation::MAX).unwrap())
        );
        for _ in 0..2 {
            assert!(matches!(
                allocation.issue(true, |_| false),
                Err(WriteTxnError::IdentityExhausted)
            ));
            assert_eq!(allocation.counters().vertex, IdentityPermutation::MAX);
        }
        allocation.counters.edge = IdentityPermutation::MAX - 1;
        let last = u128::from(
            IdentityPermutation::edges(&keys())
                .permute(IdentityPermutation::MAX)
                .unwrap(),
        );
        assert!(matches!(
            allocation.issue(false, |id| id == last),
            Err(WriteTxnError::IdentityExhausted)
        ));
    }
}

/// The error of a governed WriteTxn GQL entry point: the statement family's
/// error `E` (over [`WriteTxnError`]) or the runtime's interruption. Named once
/// so each signature states only the family it can fail with.
pub(crate) type TxnGqlError<E> = fgdb_gql::GqlQueryError<E, Box<asupersync::error::Error>>;

/// A returning write's outcome: its statistics plus the vertex and edge
/// identities it affected, in the statement's canonical order.
pub(crate) type WithAffectedIds<S> = (S, Vec<VId>, Vec<EId>);

/// A staged CREATE/INSERT RETURN: its creation statistics and governed RETURN
/// rows, or the insertion query's error with allocator error `A`.
pub(crate) type InsertQueryResult<A> = Result<
    (
        fgdb_gql::insertion::GraphInsertStats,
        fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
    ),
    TxnGqlError<fgdb_gql::GraphInsertQueryError<WriteTxnError, A>>,
>;

/// A staged MERGE RETURN: the upsert's statistics, the chosen vertex and the
/// governed RETURN row, or the upsert's error with allocator error `A`.
pub(crate) type UpsertQueryResult<A> = Result<
    (
        fgdb_gql::GraphVertexUpsertStats,
        fgdb_gql::GraphVertexMergeOutcome,
        fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
    ),
    TxnGqlError<fgdb_gql::GraphVertexUpsertError<WriteTxnError, A>>,
>;

/// Failure to prepare an atomic write or stage/finish a bounded transaction.
#[derive(Debug)]
pub enum WriteTxnError {
    /// Capability admission, target/field scope, or a live execution fence.
    Authorization(fgdb_warden::Error),
    /// A scoped mutation refused before publication. Native preparation/source
    /// diagnostics can contain protected identities or CAS values and do not
    /// escape this boundary. Never used for an admitted commit's outcome.
    AuthorizedMutationRefused,
    /// An authorized write named a client-chosen identity for a creation or
    /// ensure intent. Authorized creations take engine-allocated identities
    /// only (fgdb-hxgm1, owner ruling 2026-10-07): a chosen identity would let
    /// the outcome reveal whether a record hidden from the capability occupies
    /// it. Refused before any observation; create through the authorized
    /// insertion surfaces, which allocate.
    AuthorizedClientIdentity,
    NoPreparedWrite,
    Finished,
    /// The supplied database is not the opened handle that began this txn.
    /// Refusal preserves the transaction for use with its actual owner.
    WrongDatabase,
    /// No live savepoint has the requested name. Names are transaction-local.
    UnknownSavepoint,
    /// The bounded embedded workspace has reached its savepoint-count limit.
    SavepointLimit {
        limit: usize,
    },
    RelationMismatch {
        expected: RelationId,
        found: RelationId,
    },
    SnapshotAdvanced {
        pinned: CommitSeq,
        live: CommitSeq,
    },
    /// Reconstructing native writer/version state at a historical preparation
    /// basis failed. No publication occurred and the live handle is unchanged.
    BasisRebuild(Box<crate::RebuildError>),
    /// Independent relation groups cannot silently consume each other's writes.
    AtomicRelationConflict {
        first: RelationId,
        second: RelationId,
        element: ElementId,
    },
    /// The compound command cannot assign distinct u64 intent-visit ordinals.
    AtomicOrdinalOverflow,
    /// Ordered preparation would exceed its admitted evaluator-input rows.
    /// Counts include repeated vertex instructions, not just net effects.
    /// This is a row-replication bound, not a byte or storage-work quota.
    OrderedWriteBudgetExceeded {
        limit: u64,
        required: u128,
    },
    /// Re-staging would give two created elements the same birth ordinal once
    /// the prefix's observed births are retained. Retention never renumbers,
    /// so this refuses instead of publishing ambiguous births.
    BirthOrdinalCollision,
    /// A new delta family needs an explicit compound-write independence law.
    UnsupportedAtomicMutation,
    /// Explicit append rebase cannot preserve this workspace's decisions or
    /// exact creations. Ordinary commit/refresh never select this policy.
    AppendRebaseIneligible,
    /// Explicit field rebase cannot preserve the observed decisions, target
    /// lifetimes, supported intent family or exact field effects.
    FieldRebaseIneligible,
    /// A mixed creation/field/edge-retirement rebase cannot preserve raw decisions,
    /// exposed creation metadata, target lifetimes or exact native effects.
    MixedRebaseIneligible,
    /// The requested 128-bit identity domain has no remaining successor.
    IdentityExhausted,
    /// Cancellation before acceptance. Completion makes the transaction
    /// terminal; snapshot refresh instead preserves its entire active workspace.
    /// Neither interrupted operation publishes a write from this attempt.
    Interrupted(Box<asupersync::error::Error>),
    Read(ReadError),
    Gql(GqlError),
    Write(WriteError),
}

impl core::fmt::Display for WriteTxnError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Authorization(source) => {
                write!(formatter, "write authorization refused: {source}")
            }
            Self::AuthorizedMutationRefused => formatter.write_str("authorized mutation refused"),
            Self::AuthorizedClientIdentity => formatter.write_str(
                "authorized creation takes an engine-allocated identity, not a chosen one",
            ),
            Self::NoPreparedWrite => formatter.write_str("write transaction has no batch"),
            Self::Finished => formatter.write_str("write transaction is already finished"),
            Self::WrongDatabase => {
                formatter.write_str("write transaction belongs to a different database handle")
            }
            Self::UnknownSavepoint => formatter.write_str("unknown transaction savepoint"),
            Self::SavepointLimit { limit } => {
                write!(formatter, "transaction savepoint limit {limit} reached")
            }
            Self::RelationMismatch { expected, found } => write!(
                formatter,
                "write transaction relation mismatch: expected {expected:?}, found {found:?}"
            ),
            Self::SnapshotAdvanced { pinned, live } => write!(
                formatter,
                "write transaction pinned {pinned:?}, but the live snapshot advanced to {live:?}"
            ),
            Self::BasisRebuild(source) => {
                write!(
                    formatter,
                    "could not reconstruct the preparation basis: {source}"
                )
            }
            Self::AtomicRelationConflict {
                first,
                second,
                element,
            } => write!(
                formatter,
                "atomic relation groups {first:?} and {second:?} are not independent at {element:?}"
            ),
            Self::AtomicOrdinalOverflow => {
                formatter.write_str("atomic write intent ordinal overflow")
            }
            Self::OrderedWriteBudgetExceeded { limit, required } => write!(
                formatter,
                "ordered write requires {required} evaluator-input rows, exceeding limit {limit}"
            ),
            Self::BirthOrdinalCollision => formatter
                .write_str("retained birth ordinals would collide with newly assigned births"),
            Self::IdentityExhausted => formatter.write_str("element identity domain exhausted"),
            Self::UnsupportedAtomicMutation => formatter
                .write_str("atomic write contains a mutation without a defined independence law"),
            Self::AppendRebaseIneligible => {
                formatter.write_str("transaction is not eligible for unobserved append rebase")
            }
            Self::FieldRebaseIneligible => {
                formatter.write_str("transaction is not eligible for disjoint-field rebase")
            }
            Self::MixedRebaseIneligible => {
                formatter.write_str("transaction is not eligible for mixed mutation rebase")
            }
            Self::Interrupted(source) => {
                write!(formatter, "transaction operation interrupted: {source}")
            }
            Self::Read(source) => write!(formatter, "could not read the pinned snapshot: {source}"),
            Self::Gql(source) => write!(formatter, "transaction GQL failed: {source}"),
            Self::Write(source) => write!(formatter, "write transaction failed: {source}"),
        }
    }
}

impl core::error::Error for WriteTxnError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Authorization(source) => Some(source),
            Self::Interrupted(source) => Some(source.as_ref()),
            Self::BasisRebuild(source) => Some(source.as_ref()),
            Self::Read(source) => Some(source),
            Self::Gql(source) => Some(source),
            Self::Write(source) => Some(source),
            Self::AuthorizedMutationRefused
            | Self::AuthorizedClientIdentity
            | Self::NoPreparedWrite
            | Self::Finished
            | Self::WrongDatabase
            | Self::UnknownSavepoint
            | Self::SavepointLimit { .. }
            | Self::SnapshotAdvanced { .. }
            | Self::AtomicRelationConflict { .. }
            | Self::RelationMismatch { .. }
            | Self::AtomicOrdinalOverflow
            | Self::OrderedWriteBudgetExceeded { .. }
            | Self::BirthOrdinalCollision
            | Self::IdentityExhausted
            | Self::AppendRebaseIneligible
            | Self::FieldRebaseIneligible
            | Self::MixedRebaseIneligible
            | Self::UnsupportedAtomicMutation => None,
        }
    }
}

impl From<ReadError> for WriteTxnError {
    fn from(source: ReadError) -> Self {
        Self::Read(source)
    }
}

impl From<WriteError> for WriteTxnError {
    fn from(source: WriteError) -> Self {
        Self::Write(source)
    }
}

impl From<GqlError> for WriteTxnError {
    fn from(source: GqlError) -> Self {
        Self::Gql(source)
    }
}

/// Write batches staged against a snapshot pinned by a [`TxnCx`].
///
/// This is deliberately not SSI. Same-relation prefixes retain call order;
/// `write_atomic` explicitly admits independent relation groups at the same
/// pinned basis. Each successful staging refreshes one canonical template.
/// Commit validates observations and conservative scan witnesses before
/// delegating the whole template to the existing publication coordinator.
/// The basis stays pinned unless `refresh_snapshot` explicitly validates and
/// advances it; ordinary reads and writes never refresh it implicitly.
/// `commit_append_only_rebased` separately opts an unobserved append-only
/// workspace into re-evaluation at finalization, never an active read refresh.
/// `commit_disjoint_fields_rebased` separately admits field-local writes whose
/// raw property/label dependencies have not changed, without widening reads.
/// `commit_mixed_rebased` admits both families plus identity-addressed edge
/// retirement in one ordered native program,
/// preserving original read witnesses and publishing the whole effect once.
pub struct WriteTxn {
    handle_owner: std::sync::Arc<()>,
    basis: CommitSeq,
    staged: Vec<WriteBatch>,
    prepared: Option<PreparedWrite>,
    savepoints: Vec<EmbeddedSavepoint>,
    /// Enabled only while a mixed write program owns its rollback workspace.
    program_multi_relation: bool,
    read_set: std::cell::RefCell<std::collections::BTreeSet<ElementId>>,
    /// Explicit field getters retain point domains without narrowing full reads.
    point_reads: std::cell::RefCell<PointReads>,
    match_expansions: std::cell::RefCell<std::collections::BTreeSet<(VId, RelationId)>>,
    scanned_vertex_labels: std::cell::RefCell<std::collections::BTreeSet<LabelId>>,
    /// A scan depends on absent rows too, even if it returned nothing or its
    /// result was discarded by a budget check. Raw scans remain table-wide;
    /// labelled node scans separately retain their required label. Neither
    /// witness models arbitrary property predicates or full predicate SSI.
    scanned_vertices: std::cell::Cell<bool>,
    scanned_edges: std::cell::Cell<bool>,
    /// A plan-driven edge scan's phantom witness: any concurrent edge
    /// creation in one of these relations could add a row. The plan reads no
    /// other relation's edges, so it is not the whole-table `scanned_edges`.
    scanned_edge_relations: std::cell::RefCell<std::collections::BTreeSet<RelationId>>,
    state: EmbeddedTxnState,
    pin: Option<PurposeObligation<Acquired>>,
}
