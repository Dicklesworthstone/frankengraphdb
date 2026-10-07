//! `fgdb` — **the spine**: the first path through this database a person can run.
//!
//! Chronicle can make bytes durable and recover them across a crash at any
//! instant of the two-fsync protocol. Strata can fold delta rows into
//! content-addressed adjacency blocks and reopen a partition from a 32-byte
//! identity. `fgdb-reference` can say what any history means. Until this crate
//! existed those were three islands, and the ONLY place they met was inside test
//! files in `fgdb-sim` — so the project was 46% "complete" and 0% usable, and
//! every integration defect was scheduled to surface in W10 against forty crates
//! instead of today against four.
//!
//! This example RUNS — it is not `ignore`d prose. The lab-runtime scaffolding and
//! the scratch path are hidden so the rendered docs show the surface and not the
//! harness, but every hidden line executes: the doctest opens a real database in a
//! temporary directory, commits through the real two-fsync protocol, drops it, and
//! reopens it.
//!
//! ```
//! # use asupersync::Budget;
//! # use fgdb::{Database, DatabaseKeys, WriteBatch};
//! # use fgdb_delta_types::RelationId;
//! # use fgdb_types::context::PurposeContexts;
//! # use fgdb_types::ids::DatabaseSecurityNamespaceId;
//! # use fgdb_types::{EId, VId};
//! # let path = std::env::temp_dir().join(format!("fgdb-doctest-{}", std::process::id()));
//! # let keys = DatabaseKeys::new(
//! #     [0x5a; 32],
//! #     DatabaseSecurityNamespaceId([0x77; 32]),
//! #     [0x3c; 32],
//! # );
//! # let runtime = fgdb::runtime_builder().build().expect("production runtime");
//! # let root = runtime.request_cx_with_budget(Budget::INFINITE);
//! # let cx = &PurposeContexts::narrow_runtime_root(&root).commit();
//! # let path = &path;
//! # runtime.block_on(async move {
//!     let mut db = Database::create(cx, path, keys.clone()).await?;
//!     let mut batch = WriteBatch::new(RelationId(1));
//!     batch.create_vertex(VId(1), vec![], vec![]);
//!     batch.create_vertex(VId(2), vec![], vec![]);
//!     batch.add_edge(EId(10), VId(1), VId(2), vec![]);
//!     db.write(cx, batch).await?;             // real capsule, real marker, two fsyncs
//!     assert_eq!(db.neighbours(VId(1), RelationId(1))?, vec![VId(2)]);
//!     drop(db);
//!     let db = Database::open(cx, path, keys).await?; // path + keys only
//!     assert_eq!(db.neighbours(VId(1), RelationId(1))?, vec![VId(2)]);
//!     Ok::<(), Box<dyn core::error::Error + Send + Sync>>(())
//! # }).expect("the documented production-runtime example must run");
//! ```
//!
//! # This is a SUBSET of the embedded API, never a substitute for it
//!
//! Doctrine 7 permits early code to implement a subset of a final abstraction and
//! forbids a substitute for it, so the boundary is named here rather than left to
//! be discovered. `fgdb-w10-embedded-54r` owns the real `fgdb::Database`, and this
//! slice is to be ABSORBED into it — not left beside it as a second API.
//!
//! **Deliberately absent**, each because it belongs to a workstream that has not
//! landed: the final parameterized session protocol; full GQL; the explicit
//! transaction ownership epoch guard and reattach/renew/expiry
//! (`fgdb-w10-txn-ownership-eab`); capability narrowing and secure views; result
//! stream lifecycle; multiple graphs, branches or partitions; and the server and
//! CLI postures. Bounded MATCH, owned preparation, pinned views and staged
//! transactions are implemented subsets. All MATCH reads lower through GLA.
//!
//! **Thin in SURFACE, real in MECHANISM.** What is here is not a model of the
//! database: [`Database::write`] goes through `CommitCoordinator::commit`, which
//! is the actual two-fsync protocol writing actual capsules and markers, and
//! reads are served from actual `fgdb-strata` tier-D blocks that were encoded,
//! content-addressed, fsynced, and re-read from disk. There is no
//! `HashMap<VId, Vec<EId>>` behind this and there is no in-memory shortcut across
//! a reopen — doctrine 7 prohibits both, and a slice that stubbed the durable
//! path would prove nothing about the durable path.
//!
//! # Checkpoint-selected reopen and Chronicle authority
//!
//! `manifest.root` can select a durable Strata checkpoint and reopen its
//! immutable objects directly. Content identity proves that those objects are
//! authentic, but not that they belong to this database's Chronicle history.
//! Each V2 manifest record therefore carries Chronicle's marker-chain
//! commitment at the root's publication sequence. [`Database::open`] compares
//! that commitment with its independently recovered marker chain before
//! accepting the slot; only the suffix is then applied to the selected
//! checkpoint. A well-formed checkpoint transplanted from another history is
//! refused even when its namespace keys and immutable objects are resolvable.
//!
//! This is the correctness path required by doctrine 5 and FG-INV-18: derived
//! structures are never more authoritative than the commit stream. Checkpoint
//! authentication is one chain lookup and comparison; open still recovers and
//! verifies Chronicle's marker chain, reopens the checkpoint objects, and
//! folds any suffix after the checkpoint. This is a cost-fast checkpoint path,
//! not a claim that total open cost is independent of history or checkpoint
//! size.
//!
//! Writes publish incrementally through tier D; a forced full rebuild remains
//! available as the equivalence oracle for the checkpoint-selected path.
//! The rebuild is deterministic, which is worth more than it sounds: the root is
//! content-addressed, so replaying the same stream twice publishes the SAME
//! `PartitionRootVersion`, and [`Database::partition_root`] exposes it so that
//! law can be asserted.
//!
//! # What tier D indexes
//!
//! Tier D holds ADJACENCY BLOCKS and VERTEX ROW PATCHES. Edges answer through
//! [`Database::neighbours`]; a vertex's labels and properties answer through
//! [`Database::vertex`] (fgdb-3xoi). Deletes go through
//! [`WriteBatch::delete_edge`] and [`WriteBatch::delete_vertex`], and vertex
//! label/property updates through [`WriteBatch::set_vertex_label`] and
//! [`WriteBatch::set_vertex_property`] — all with engine-derived before-images
//! validated by the oracle at replay (fgdb-p3ok, fgdb-stb6). Edge properties
//! and the columnar sealed forms arrive with `fgdb-w3-properties-gou`; the
//! provenance envelopes and `NetEffectNormalForm` canonicalization with
//! `fgdb-w5-effects-normal-form-819`.

#![forbid(unsafe_code)]

mod bulk_load;
pub use bulk_load::{
    BulkEdge, BulkLoadCheckpoint, BulkLoadError, BulkLoadErrorKind, BulkLoadPolicy, BulkRow,
    BulkVertex, ChunkFit,
};

mod fcw;
pub use fcw::FirstCommitterWinsValidator;
mod pinned_tzdb;
pub use pinned_tzdb::{PinnedTzdb, TzdbArtifactError, TzdbTransition, TzdbZone};

mod gql_cert;
mod gql_exec;
mod memvfs;
mod prepared_write;
mod query;
mod scrub;
mod standing_query;
pub use scrub::{LostCapsule, ScrubCrashPoint, ScrubSummary};
pub use standing_query::{
    NativeSubscription, StandingNativeCursor, StandingNativeDeltaCursor, StandingQueryError,
    StandingQueryFailure, StandingQueryHandle, StandingQueryStats, StandingQueryView,
    StandingReplayBatch, StandingReplayWindow, SubscribeError, SubscriptionBatch,
    SubscriptionError, SubscriptionReceipt,
};
mod write_txn;
/// The pinned-GQL surface types callers need to drive
/// [`Database::execute_gql`]: the bind map is caller-supplied (no invented
/// catalog), and the plan is re-exported so tests can state that the executor
/// accepts a [`BoundPlan`] and nothing parse-shaped.
pub use fgdb_gql::{BoundPlan, RelationBind};
/// The replayable certificate [`Database::execute_gql_certified`] returns
/// beside its rows (fgdb-gate-genesis-lce.1): snapshot seq plus statement and
/// bind digests, so the same graph state, text, and bind are auditable as
/// byte-identical.
pub use gql_cert::{
    CertificateDecodeError, GqlCertificate, GqlPlanCertificate, NativeCertificatePlan,
    NativePlanCertificate, NativeProcedureEvidence, NativeReadClass,
};
pub use gql_exec::source::{SnapshotEdgeSource, SnapshotVertexSource};
pub use query::{
    AuthorizedAggregateCursor, AuthorizedBeaconIndex, AuthorizedPreparedFnxCall,
    AuthorizedPreparedRead, AuthorizedReadSession, AuthorizedRowCursor, ExplainRow,
    HYBRID_SEARCH_OUTPUTS, HybridCallError, NativeAggregateCursor, NativeExplainCertificate,
    NativeResultCertificate, NativeResultSpool, NativeSpoolCursor, NativeSpoolError, PinnedIndex,
    PreparedNativeRead, ProcedureError, QueryError, QueryResult, QueryValue, QueryWriteError,
    RefreshReport, ReplayRefusal, ResidentIndex, ResidentIndexError,
};
pub use write_txn::{
    AuthorizedBoundWriteBatch, AuthorizedPreparedWrite, AuthorizedWriteSession, WriteTxn,
    WriteTxnError,
};

/// The in-memory [`Vfs`](asupersync::fs::Vfs) behind the embedded spine's
/// `":memory:` surface: RAM file content over a private sparse shadow
/// namespace. See [`memvfs`] and [`Database::<MemVfs>::open_memory`].
pub use memvfs::{MemVfs, MemVfsFile};

/// The stable law id [`FirstCommitterWinsValidator`] rejects under, keyed on
/// by the typed [`WriteError::FirstCommitterWins`] arm. Kept identical to the
/// private constant in `fcw.rs` the same way `touched_elements` is mirrored
/// there: one product write-set contract, two named sites.
const FCW_LAW_ID: &str = "FG-LAW-FCW-01";

/// The read-set sibling law (fgdb-w4-g1-txn-core-qpmg.5): a validator
/// rejection carrying this id means a `WriteTxn`'s OBSERVED elements — an
/// overlay `vertex` or MATCH read — were written past the pinned basis by a
/// first committer. It routes into the same typed
/// [`WriteError::FirstCommitterWins`] arm, whose `law` field is exactly how
/// a caller tells a lost write-set from a stale read-set; the remedy for
/// both is the same rebuild-against-the-advanced-snapshot, which is why they
/// share the arm and not the generic [`WriteError::Commit`] wrap.
const FCW_READ_LAW_ID: &str = "FG-LAW-FCW-READ-01";

use asupersync::fs::{OpenOptions, UnixVfs, Vfs, VfsFile};
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::{CAPSULE_DIR, CommitCoordinator, CommitError};
use fgdb_chronicle::identity::{CryptoVerificationEvent, IdentifiedObject};
use fgdb_chronicle::marker::{CommitMarker, EffectSource, HeadUpdate};
use fgdb_chronicle::{
    RootBootstrap, RootSelection, RootSlot, RootStore, store::StoreError as SlotStoreError,
};
use fgdb_crypto::{Digest, zeroize::SharedSecret};
use fgdb_delta_types::{
    CanonicalError, CommittedMarker, CoordinateEntry, DeltaRow, ElementId, IndexError, LabelId,
    LocalDeltaBatchIndex, LogicalDeltaBatch, LogicalDeltaTemplate, PropertyKeyId, RelationId,
    SchemaEpoch, fold_target_disjoint,
};
use fgdb_strata::edge_props::BlockProps;
use fgdb_strata::manifest::{ManifestRecord, ManifestVersion, encode_manifest, records_of};
use fgdb_strata::root::{BlockRef, PatchRef, RootError, merge_all_edges_with_props};
use fgdb_strata::store::{BlockStore, PublishReceipts, StoreError};
use fgdb_strata::vertex::{VertexPatchRows, merge_all_vertices, merge_vertex};
use fgdb_strata::writer::{BlockWriter, WriteError as BlockWriteError};
use fgdb_strata::{AdjacencyEntry, PartitionRootVersion};

pub use fgdb_strata::edge_props::EdgePropertyRow;
pub use fgdb_strata::vertex::VertexRow;
use fgdb_types::context::{CommitCx, TxnCx};
use fgdb_types::ids::{DatabaseSecurityNamespaceId, ObjectId};
use fgdb_types::{
    BranchId, CanonicalScalar, CommitSeq, EId, GraphId, MarkerRef, ObligationAcquireError,
    ObligationId, VId,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A type-erased `Send` future. The commit, completion, open and publication
/// boundaries return one, so a caller's `Send` proof stops there instead of
/// descending through Chronicle and Strata. Without it every lab test root
/// overflowed rustc's default recursion limit (fgdb-a5y6m).
pub(crate) type SendFuture<'a, T> = std::pin::Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Re-exported because [`Database::write_with_crash`] takes one: a caller
/// driving the crash-point matrix needs to name the instants, and importing them
/// from Chronicle directly would make the spine's own signature unusable without
/// a second dependency.
pub use fgdb_chronicle::commit::CrashPoint;
pub use fgdb_strata::store::BlockStoreCrashPoint;

/// The runtime the `fgdb` CLI and benchmarks run a database under: asupersync's
/// default runtime plus a blocking pool for filesystem I/O.
///
/// A commit's Strata objects are synced with all of their syncs in flight at
/// once (`BlockPublicationBatch::flush`), and each in-flight sync needs its own
/// pool worker to overlap the others. A runtime without a blocking pool is
/// equally correct, but asupersync then runs every sync inline, one at a time,
/// on the scheduler thread; the lab runtime relies on exactly that.
#[must_use]
pub fn runtime_builder() -> asupersync::runtime::RuntimeBuilder {
    asupersync::runtime::RuntimeBuilder::new()
        .blocking_threads(0, fgdb_strata::store::BATCH_SYNCS_IN_FLIGHT)
}

/// Object kind for a committed effect capsule.
///
/// `0x0274` is the Appendix A reservation for `CommittedEffectCapsule`. It is a
/// constant rather than a typed kind because that kind is `reserved`, not
/// `active`, so naming it in the type system would not compile.
pub const CAPSULE_OBJECT_KIND: u16 = 0x0274;

/// Domain separator, so a template digest can never collide with any other
/// digest in the system by hashing the same bytes under a different meaning.
pub const TEMPLATE_DIGEST_DOMAIN: &[u8] = b"fgdb:logical-delta-template:v1";

/// The single coordinate this slice serves. Multiple graphs, branches and
/// partitions are real concepts with real owners (`fgdb-w2-*`, `fgdb-w3-*`);
/// pretending to support them from one hard-coded coordinate would be the
/// substitute doctrine 7 forbids, so the slice serves exactly one and says so.
const GRAPH: GraphId = GraphId(1);
const BRANCH: BranchId = BranchId(1);
const PARTITION: u64 = 0;

/// The keys a database directory is opened under.
///
/// Key MANAGEMENT is `fgdb-warden`'s, and it does not exist yet. Until it does
/// the caller supplies these, which is honest about where they come from: this
/// slice derives no key material and stores none.
///
/// `Debug` is deliberately redacted. This public value is also retained inside
/// [`Database`], so a derived formatter here would leak both raw keys through
/// direct formatting and transitively through the database handle.
#[derive(Clone)]
pub struct DatabaseKeys {
    /// The immutable object-identity key (§5.1).
    k_oid: SharedSecret<32>,
    pub namespace: DatabaseSecurityNamespaceId,
    /// The data-encryption key for capsules.
    dek: SharedSecret<32>,
    scalar_resolver: Option<Arc<dyn fgdb_types::CanonicalScalarResolver + Send + Sync>>,
}

impl core::fmt::Debug for DatabaseKeys {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("DatabaseKeys([REDACTED])")
    }
}

impl DatabaseKeys {
    /// Move raw key bytes into shared scrub-on-last-drop ownership.
    pub fn new(k_oid: [u8; 32], namespace: DatabaseSecurityNamespaceId, dek: [u8; 32]) -> Self {
        Self {
            k_oid: SharedSecret::new(k_oid),
            namespace,
            dek: SharedSecret::new(dek),
            scalar_resolver: None,
        }
    }

    /// Pins immutable artifact data for admission and every recovery decoder.
    /// Reopening must supply the same artifacts; unknown OIDs fail closed.
    #[must_use]
    pub fn with_scalar_resolver(
        mut self,
        resolver: Arc<dyn fgdb_types::CanonicalScalarResolver + Send + Sync>,
    ) -> Self {
        self.scalar_resolver = Some(resolver);
        self
    }

    fn decode_template(
        &self,
        bytes: &[u8],
    ) -> Result<LogicalDeltaTemplate, fgdb_delta_types::CanonicalError> {
        match self.scalar_resolver.as_deref() {
            Some(resolver) => LogicalDeltaTemplate::decode_canonical_with_resolver(bytes, resolver),
            None => LogicalDeltaTemplate::decode_canonical(bytes),
        }
    }

    /// Borrow the logical-identity key without creating an owned copy.
    pub fn k_oid(&self) -> &[u8; 32] {
        self.k_oid.expose()
    }

    /// Clone ownership of the logical-identity authority without copying it.
    #[doc(hidden)]
    pub fn shared_k_oid(&self) -> SharedSecret<32> {
        self.k_oid.clone()
    }

    /// Borrow the capsule encryption key without creating an owned copy.
    pub fn dek(&self) -> &[u8; 32] {
        self.dek.expose()
    }

    #[doc(hidden)]
    pub fn capsule_keys(&self) -> CapsuleKeys {
        CapsuleKeys::new(
            self.k_oid.clone(),
            self.namespace,
            self.dek.clone(),
            CAPSULE_OBJECT_KIND,
            CapsuleProfile::balanced(),
        )
    }

    fn block_keys(&self) -> (&[u8; 32], DatabaseSecurityNamespaceId) {
        (self.k_oid.expose(), self.namespace)
    }
}

/// The root-slot generation cannot advance any further.
///
/// Recovery selects the highest credible generation, so wrapping or
/// saturating would make the next publication permanently unselectable. A
/// caller must migrate or clone under a fresh fenced identity instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotGenerationExhausted {
    pub current: u64,
}

impl core::fmt::Display for SlotGenerationExhausted {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "root-slot generation {} is exhausted; migrate or clone under a fresh fenced identity",
            self.current
        )
    }
}

/// A commit whose derived partition root could exceed the root's reference
/// ceiling (`MAX_ROOT_BLOCKS` blocks, `MAX_ROOT_PATCHES` vertex patches).
///
/// Refused before Chronicle receives the capsule. The same refusal after the
/// durable point would be permanent: the commit could never be published,
/// and the authoritative rebuild re-derives the same over-full root, so the
/// database would never open again. `added_*` are upper bounds: statements
/// per touched element, one block or patch per statement at most (see
/// `root_growth_bound`). They bind only within a commit's size of the ceiling. `fgdb compact` shrinks
/// the live root. The O(changed) root that removes the ceiling is fgdb-d5vo4.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RootCapacityExceeded {
    pub blocks: usize,
    pub added_blocks: usize,
    pub max_blocks: usize,
    pub patches: usize,
    pub added_patches: usize,
    pub max_patches: usize,
}

impl core::fmt::Display for RootCapacityExceeded {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "the partition root holds {} blocks and {} vertex patches; this commit could add up to {} and {}, past the ceiling of {} and {}; compact the database or split the commit",
            self.blocks,
            self.patches,
            self.added_blocks,
            self.added_patches,
            self.max_blocks,
            self.max_patches
        )
    }
}

/// An upper bound on the (blocks, vertex patches) one commit's fold seals.
/// Every block or patch the fold seals holds at least one statement of this
/// commit (the per-commit seal law), so statements bound what the commit
/// adds. Per touched element, a creation begins one statement and a deletion
/// (a cascade's edges included) retires one. A content change (property or
/// label) retires one and begins its successor: two. Same-commit folds only
/// remove statements. `touched_elements` covers exactly the rows the writer
/// folds into statements.
fn root_growth_bound(template: &LogicalDeltaTemplate) -> (usize, usize) {
    let (mut blocks, mut patches) = (0usize, 0usize);
    let mut touched = std::collections::BTreeSet::new();
    for coordinate in template.coordinate_entries() {
        if (coordinate.graph, coordinate.branch) != (GRAPH, BRANCH) {
            continue;
        }
        for row in &coordinate.rows {
            let statements = match row {
                DeltaRow::LabelMembership { .. } | DeltaRow::Property { .. } => 2,
                _ => 1,
            };
            touched.clear();
            touched_elements(row, &mut touched);
            for element in &touched {
                match element {
                    ElementId::Vertex(_) => patches = patches.saturating_add(statements),
                    ElementId::Edge(_) => blocks = blocks.saturating_add(statements),
                }
            }
        }
    }
    (blocks, patches)
}

/// Admit a commit only if its derived root is certain to stay within the
/// reference ceiling, by [`root_growth_bound`].
fn admit_root_capacity(
    blocks: usize,
    patches: usize,
    template: &LogicalDeltaTemplate,
    max_blocks: usize,
    max_patches: usize,
) -> Result<(), RootCapacityExceeded> {
    let (added_blocks, added_patches) = root_growth_bound(template);
    if blocks.saturating_add(added_blocks) > max_blocks
        || patches.saturating_add(added_patches) > max_patches
    {
        return Err(RootCapacityExceeded {
            blocks,
            added_blocks,
            max_blocks,
            patches,
            added_patches,
            max_patches,
        });
    }
    Ok(())
}

/// Why a database directory could not be opened or created.
#[derive(Debug)]
pub enum OpenError {
    /// The path exists and is not a directory.
    NotADirectory {
        path: PathBuf,
    },
    /// [`Database::open`] was asked for a directory that does not hold a
    /// database.
    ///
    /// **This is the fail-closed law, and it is deliberately not lenient.**
    /// `CommitCoordinator::open` creates its capsule directory when absent, so
    /// an `open` that simply delegated would silently CONVERT any directory
    /// into an empty database and answer queries about it. Naming the missing
    /// component is what makes the refusal actionable.
    NotADatabase {
        path: PathBuf,
        missing: &'static str,
    },
    /// [`Database::create`] was asked for a directory that already holds one.
    AlreadyADatabase {
        path: PathBuf,
    },
    /// The root slot file failed at the storage boundary (fgdb-ge6a).
    Slot(SlotStoreError),
    /// A lagging slot cannot be healed because its generation is already the
    /// largest representable value.
    SlotGenerationExhausted(SlotGenerationExhausted),
    /// The selected slot is well-formed and is NOT this database's: its
    /// identity tuple or PLAIN-opener form disagrees with the keys in hand.
    /// Refused, never reinterpreted — a slot from another database, another
    /// posture, or a tampered one must not steer recovery.
    ForeignSlot {
        path: PathBuf,
    },
    /// The selected opener does not authenticate with the supplied DEK.
    WrongDek {
        path: PathBuf,
    },
    /// The selected slot names a manifest the stream cannot account for —
    /// not the rebuilt one, and not a resolvable ancestor of it. The stream
    /// is the source of truth, and a pointer it cannot explain is damage.
    SlotDisagreesWithStream {
        path: PathBuf,
        slot_manifest: ObjectId,
    },
    /// The root file exists but recovery selected no credible slot.
    SlotUnrecoverable {
        path: PathBuf,
        detail: String,
    },
    /// [`Database::create`] was asked for a non-empty directory that is not a
    /// database. Refused rather than adopted: this slice cannot prove that
    /// foreign contents are not a half-written database.
    NotEmpty {
        path: PathBuf,
    },
    /// A verification probe stopped creation after the database directory
    /// inode was durable but before its name was durable in the parent.
    ///
    /// This is emitted only by [`Database::create_with_vfs_at_crash`]. The
    /// ordinary constructor executes the same path with no stopping point.
    InjectedCreateCrash(DatabaseCreateCrashPoint),
    Io(std::io::Error),
    Commit(CommitError),
    Store(StoreError),
    /// The durable stream could not be rebuilt into a partition.
    Rebuild(RebuildError),
}

/// The creation-only instant that separates a durable database directory inode
/// from a durable name for that directory in its parent.
///
/// A test-facing stop exists because a crash-image test must exercise the same
/// ordering as [`Database::create`], not a copied approximation of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatabaseCreateCrashPoint {
    /// The database directory itself has been synced, but its parent directory
    /// has not yet made the new name durable.
    AfterDatabaseDirectorySyncBeforeParentSync,
}

/// Why rebuilding the tier-D fold from the durable stream failed.
#[derive(Debug)]
pub enum RebuildError {
    /// Cooperative cancellation stopped derived-state reconstruction before
    /// its complete replacement could be published.
    Interrupted(Box<asupersync::error::Error>),
    /// A retained handle whose Chronicle/publication relationship is unknown
    /// cannot run maintenance from its cached snapshot.
    HandleNotHealthy(DatabaseState),
    /// A committed marker names a capsule whose bytes are not on disk. The
    /// marker IS the commit, so its capsule was durable before the marker was
    /// written: absence means something deleted bytes the stream references.
    MissingCapsule {
        commit_seq: u64,
        capsule_oid: ObjectId,
    },
    /// The capsule's bytes do not hash to the digest its marker declared —
    /// FG-INV-09's shape. A reader that skipped this would turn silent
    /// corruption into silently different graph state.
    TemplateDigestMismatch {
        commit_seq: u64,
        declared: Digest,
        recomputed: Digest,
    },
    /// The capsule's bytes are not a decodable template.
    Decode {
        commit_seq: u64,
        error: CanonicalError,
    },
    /// The tier-D writer refused a row the stream committed.
    Fold {
        commit_seq: u64,
        error: BlockWriteError,
    },
    /// Re-deriving the statement-version transcript for the new snapshot
    /// failed after the commit was durable.
    Version {
        commit_seq: u64,
        error: CanonicalError,
    },
    /// A deterministic verification probe stopped derived publication at the
    /// named post-D2 stage. This is emitted only by
    /// [`Database::write_with_publication_failure`]; the durable commit and the
    /// recovery obligation are otherwise identical to a real failure there.
    InjectedPublicationFailure(DerivedPublicationStage),
    Commit(CommitError),
    Store(StoreError),
    /// Advancing the root slot after a durable publish failed (fgdb-ge6a).
    /// The commit and the manifest are durable; the slot is at most one
    /// publication behind, which the next open heals.
    Slot(SlotStoreError),
    /// Maintenance cannot publish a replacement root because the slot
    /// generation is already the largest representable value.
    SlotGenerationExhausted(SlotGenerationExhausted),
    /// The derived in-memory window refused a batch the recovered chain
    /// committed. The stream is the source of truth; this is a derived
    /// reconstruction failure (FG-INV-18), never a second authority.
    Index {
        commit_seq: u64,
        error: IndexError,
    },
}

/// The derived-publication stage that failed after Chronicle made a commit
/// durable.
///
/// This is deliberately coarse enough to remain a stable diagnostic contract,
/// but precise enough to tell an operator which immutable/derived boundary to
/// inspect. The authoritative recovery rule is identical for every variant:
/// reopen and rebuild from Chronicle; never continue from the retained fold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DerivedPublicationStage {
    FoldCommittedTemplate,
    SealPartition,
    PublishEdgeBlocks,
    PublishVertexPatches,
    PublishPartitionRoot,
    PublishManifest,
    PublishRootSlot,
    RefreshEdgeSnapshot,
    RefreshVertexSnapshot,
}

/// Evidence carried by a handle that must be authoritatively recovered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryRequired {
    /// The Chronicle sequence already made durable by D2.
    pub durable_frontier: CommitSeq,
    /// The last sequence represented by the handle's retained snapshot.
    pub published_frontier: CommitSeq,
    /// The derived stage that prevented the handle from catching up.
    pub failed_stage: DerivedPublicationStage,
}

/// Whether this in-process handle can truthfully serve the current database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatabaseState {
    Healthy {
        published_frontier: CommitSeq,
    },
    /// A cancellable Chronicle commit began and the outer write did not
    /// observe a safe pre-marker refusal or a completed D2. Only reopen can
    /// decide whether the marker committed.
    CommitOutcomeUnknown {
        published_frontier: CommitSeq,
    },
    /// D2 completed, but derived publication did not catch the handle up.
    NeedsAuthoritativeRecovery(RecoveryRequired),
}

/// Why a write could not be committed.
#[derive(Debug)]
pub enum WriteError {
    /// A prepared template belongs to a different opened handle lifetime.
    /// Equal keys, paths or commit sequences cannot transfer its ownership.
    ForeignPreparedWrite,
    /// The complete committed suffix needed to validate a prepared basis is
    /// unavailable. Never interpret a missing prefix as no conflicting writes.
    PreparedHistory(IndexError),
    /// Loading the authenticated conflict history failed before this write
    /// entered the durable commit protocol. The current graph remains valid.
    HistoryRebuild(Box<RebuildError>),
    /// The batch was empty. Refused rather than committed as a no-op: an empty
    /// commit consumes a sequence and publishes a marker, and a caller that did
    /// that by accident should be told.
    EmptyBatch,
    /// This writer has no pinned tzdb resolver for durable replay. Refused
    /// during preparation, not misreported as a transaction conflict.
    ZonedTimestampRequiresResolver {
        tzdb_oid: ObjectId,
    },
    /// A delete named an edge this database holds no live version of, at the
    /// point in the batch where the delete sits. Refused before anything
    /// durable happens (fgdb-p3ok).
    UnknownEdge {
        eid: EId,
    },
    /// A delete named a vertex this database holds no live version of, at
    /// the point in the batch where the delete sits.
    UnknownVertex {
        vid: VId,
    },
    /// A create named an identity that is already live. Identities are
    /// permanently spent, and this refusal fires BEFORE the two-fsync commit
    /// (fgdb-kokz): the fold would refuse the same row after it, and a
    /// durable commit its own replay refuses poisons the database.
    AlreadyLive {
        elem: ElementId,
    },
    /// A create named an identity that was spent by earlier history —
    /// including earlier in this very batch. Same pre-commit discipline as
    /// [`WriteError::AlreadyLive`].
    IdentitySpent {
        elem: ElementId,
    },
    /// A [`WriteBatch::add_edge`] / [`WriteBatch::ensure_edge_by_triple`]
    /// named an endpoint that is not live at that point in the batch.
    /// Refused before D2: a durable CreateEdge the oracle cannot apply
    /// (`ApplyError::DanglingEndpoint`) would poison reopen replay.
    DanglingEndpoint {
        eid: EId,
        endpoint: VId,
    },
    /// A [`WriteBatch::compare_and_set_vertex_property`] /
    /// [`WriteBatch::compare_and_set_edge_property`] guard failed under
    /// [`WriteMismatchPolicy::AbortWrite`]. Nothing durable happened.
    CompareAndSetMismatch(Box<CompareAndSetMismatch>),
    Canonical(CanonicalError),
    /// The final vertex row cannot fit the current stored-patch budget.
    /// Refused during preparation, before Chronicle can consume a sequence.
    /// Larger scalars remain canonical; this storage representation cannot
    /// materialize them as an indivisible row.
    ///
    /// The storage and commit sources below are boxed, like the CAS mismatch
    /// above: each was larger than every other arm, and WriteError rides inside
    /// WriteTxnError and every WriteTxn GQL error family, so an inline source
    /// set the size of all of them on every successful Result.
    VertexStorageAdmission {
        vid: VId,
        source: Box<fgdb_strata::vertex::VertexPatchError>,
    },
    /// The final edge property row cannot fit one stored sidecar. The handle
    /// and its published frontier remain unchanged, as for vertex admission.
    EdgeStorageAdmission {
        eid: EId,
        source: Box<fgdb_strata::edge_props::EdgePropertyPatchError>,
    },
    /// [`WriteBatch::extend`] was handed a batch over a different relation
    /// (fgdb-w4-g1-txn-core-qpmg.2). The relation is the batch's template
    /// coordinate; concatenation must not silently re-home rows onto another
    /// one. Nothing was appended.
    MixedRelation {
        expected: RelationId,
        found: RelationId,
    },
    /// The [`TxnCx`] refused to acquire the snapshot-pin obligation
    /// [`Database::begin`] asked for (fgdb-writetxn-pin-l8wb). Nothing was
    /// pinned and no transaction exists; the context's obligation ledger is
    /// the authority on why.
    SnapshotPin(ObligationAcquireError),
    /// The installed commit validator refused this draft under
    /// first-committer-wins (fgdb-fcw-writebatch-6cxf): an element in the
    /// batch's write-set was already committed by a writer this batch's basis
    /// never saw. Nothing durable happened, no sequence was consumed, and the
    /// handle stays Healthy — the remedy is to rebuild the batch against the
    /// advanced snapshot, which is why this is not a [`WriteError::Commit`]
    /// wrap: no transport or protocol repair applies.
    FirstCommitterWins {
        /// The stable law the validator rejected under (`FG-LAW-FCW-01`).
        law: &'static str,
        /// The validator's diagnostic naming the losing element and the
        /// winning commit. Human-facing, never parsed.
        detail: String,
    },
    /// Chronicle failed before the marker could have become durable. Unlike
    /// [`WriteError::CommitOutcomeUnknown`], retrying after correcting the
    /// named cause cannot duplicate an unobserved commit.
    Commit(Box<CommitError>),
    /// Chronicle may or may not have made the marker durable. The live handle
    /// is fenced immediately; reopen is the only authority that can decide.
    CommitOutcomeUnknown {
        published_frontier: CommitSeq,
        source: Box<CommitError>,
    },
    /// A prior call left the handle unable to speak for Chronicle's head.
    HandleCommitOutcomeUnknown {
        published_frontier: CommitSeq,
    },
    /// A prior durable commit failed during derived publication. The handle is
    /// fenced so another write cannot publish from its stale fold.
    RecoveryRequired(RecoveryRequired),
    /// The root slot cannot name another generation. This refusal is computed
    /// before Chronicle receives capsule bytes, so no commit sequence is
    /// consumed and no recovery obligation is created.
    SlotGenerationExhausted(SlotGenerationExhausted),
    /// The derived partition root could exceed its reference ceiling. Like
    /// slot exhaustion, this is computed before Chronicle receives capsule
    /// bytes: no sequence is consumed and the handle stays Healthy.
    /// Boxed like the storage-admission arms: six counters inline would set
    /// the size of every WriteTxnError result.
    RootCapacity(Box<RootCapacityExceeded>),
    /// This call committed at D2, then failed while publishing derived state.
    /// The commit is NOT lost; `recovery` names the exact stale/current split.
    CommittedNeedsRecovery {
        recovery: RecoveryRequired,
        source: Box<RebuildError>,
    },
}

/// Why a read could not be served.
#[derive(Debug)]
pub enum ReadError {
    /// Boxed: RootError is 96 bytes against 40 for every other arm, and
    /// ReadError rides inside most query error types, so an inline RootError
    /// set their size (128-byte QueryError) on every successful Result.
    Root(Box<RootError>),
    /// Chronicle may have advanced, so the retained snapshot cannot be
    /// presented as current until an authoritative reopen resolves the log.
    CommitOutcomeUnknown { published_frontier: CommitSeq },
    /// Chronicle definitely advanced past the retained derived snapshot.
    RecoveryRequired(RecoveryRequired),
    /// A time-travel read asked about a sequence the published partition has
    /// not reached. Refused rather than clamped: an answer AT the frontier
    /// for a question ABOUT the future would silently change meaning the
    /// moment the next commit lands (fgdb-90jx).
    BeyondFrontier {
        asked: CommitSeq,
        frontier: CommitSeq,
    },
    /// A delta-window cursor precedes the currently retained index. This may
    /// be an unloaded checkpoint prefix or an explicitly retired interval;
    /// answering with only the remaining suffix would be a gapped stream.
    /// This error alone does not claim that durable history was deleted.
    DeltaCursorRetired {
        asked: CommitSeq,
        retained_after: CommitSeq,
        frontier: CommitSeq,
    },
    /// A derived-window query failed for a reason other than a future or
    /// retired cursor. `since` does not construct these; the arm exists so a
    /// new index-query error cannot be silently remapped.
    DeltaWindow(IndexError),
    /// A vertex label in the admitted snapshot has no reverse name in the
    /// caller's catalog or capability scope.
    UnmappedLabel(LabelId),
    /// An edge relation in the admitted snapshot has no reverse name in the
    /// caller's catalog or capability scope.
    UnmappedRelation(RelationId),
}

macro_rules! from_error {
    ($outer:ty, $variant:ident, $inner:ty) => {
        impl From<$inner> for $outer {
            fn from(error: $inner) -> Self {
                Self::$variant(error)
            }
        }
    };
}

from_error!(OpenError, Io, std::io::Error);
from_error!(OpenError, Commit, CommitError);
from_error!(OpenError, Store, StoreError);
from_error!(OpenError, Rebuild, RebuildError);
from_error!(OpenError, SlotGenerationExhausted, SlotGenerationExhausted);
from_error!(RebuildError, Commit, CommitError);
from_error!(RebuildError, Store, StoreError);
from_error!(
    RebuildError,
    SlotGenerationExhausted,
    SlotGenerationExhausted
);
from_error!(WriteError, Canonical, CanonicalError);
impl From<RebuildError> for WriteError {
    fn from(error: RebuildError) -> Self {
        match error {
            RebuildError::Index { error, .. } => Self::PreparedHistory(error),
            error => Self::HistoryRebuild(Box::new(error)),
        }
    }
}
impl From<CommitError> for WriteError {
    fn from(error: CommitError) -> Self {
        Self::Commit(Box::new(error))
    }
}
from_error!(WriteError, SlotGenerationExhausted, SlotGenerationExhausted);
impl From<RootError> for ReadError {
    fn from(error: RootError) -> Self {
        Self::Root(Box::new(error))
    }
}

impl core::fmt::Display for OpenError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotADirectory { path } => {
                write!(f, "{} exists and is not a directory", path.display())
            }
            Self::Slot(error) => write!(f, "root slot: {error}"),
            Self::SlotGenerationExhausted(error) => error.fmt(f),
            Self::WrongDek { path } => write!(
                f,
                "data-encryption key does not authenticate database {}",
                path.display()
            ),
            Self::ForeignSlot { path } => write!(
                f,
                "the root slot in {} is not this database's — identity tuple or \
                 opener form disagrees with the keys in hand",
                path.display()
            ),
            Self::SlotDisagreesWithStream {
                path,
                slot_manifest,
            } => write!(
                f,
                "the root slot in {} names manifest {slot_manifest:?}, which the \
                 commit stream cannot account for",
                path.display()
            ),
            Self::SlotUnrecoverable { path, detail } => write!(
                f,
                "the root file in {} selected no credible slot: {detail}",
                path.display()
            ),
            Self::NotADatabase { path, missing } => write!(
                f,
                "{} is not a database: {missing} is absent",
                path.display()
            ),
            Self::AlreadyADatabase { path } => {
                write!(f, "{} already holds a database", path.display())
            }
            Self::NotEmpty { path } => write!(
                f,
                "{} is not empty and does not hold a database",
                path.display()
            ),
            Self::InjectedCreateCrash(point) => {
                write!(f, "injected database-creation crash at {point:?}")
            }
            Self::Io(error) => write!(f, "io: {error}"),
            Self::Commit(error) => write!(f, "commit stream: {error}"),
            Self::Store(error) => write!(f, "block store: {error}"),
            Self::Rebuild(error) => write!(f, "rebuild: {error}"),
        }
    }
}

impl core::fmt::Display for RebuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Interrupted(error) => error.fmt(f),
            Self::HandleNotHealthy(state) => write!(
                f,
                "maintenance requires a healthy reopened handle, found {state:?}"
            ),
            Self::MissingCapsule {
                commit_seq,
                capsule_oid,
            } => write!(
                f,
                "commit {commit_seq} names capsule {capsule_oid:?}, which is not on disk"
            ),
            Self::TemplateDigestMismatch { commit_seq, .. } => write!(
                f,
                "commit {commit_seq}: capsule bytes do not hash to the declared template digest"
            ),
            Self::Decode { commit_seq, error } => {
                write!(f, "commit {commit_seq}: capsule does not decode: {error}")
            }
            Self::Fold { commit_seq, error } => write!(
                f,
                "commit {commit_seq}: the tier-D writer refused a committed row: {error}"
            ),
            Self::Version { commit_seq, error } => write!(
                f,
                "commit {commit_seq}: statement-version derivation failed: {error}"
            ),
            Self::InjectedPublicationFailure(stage) => {
                write!(f, "injected derived-publication failure at {stage:?}")
            }
            Self::Commit(error) => write!(f, "commit stream: {error}"),
            Self::Store(error) => write!(f, "block store: {error}"),
            Self::Slot(error) => write!(
                f,
                "root slot publication after a durable publish: {error} (the \
                 slot is at most one publication behind; the next open heals it)"
            ),
            Self::SlotGenerationExhausted(error) => error.fmt(f),
            Self::Index { commit_seq, error } => write!(
                f,
                "commit {commit_seq}: derived delta index refused the committed batch: {error}"
            ),
        }
    }
}

impl core::fmt::Display for WriteError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ForeignPreparedWrite => {
                f.write_str("prepared write belongs to a different database handle")
            }
            Self::PreparedHistory(error) => {
                write!(f, "prepared-write conflict history is unavailable: {error}")
            }
            Self::HistoryRebuild(error) => {
                write!(
                    f,
                    "prepared-write conflict history could not be loaded: {error}"
                )
            }
            Self::EmptyBatch => write!(f, "an empty batch consumes a commit sequence for nothing"),
            Self::ZonedTimestampRequiresResolver { .. } => {
                f.write_str("zoned timestamp writes require a pinned tzdb resolver")
            }
            Self::UnknownEdge { eid } => {
                write!(f, "no live version of {eid:?} to delete")
            }
            Self::UnknownVertex { vid } => {
                write!(f, "no live version of {vid:?} to delete")
            }
            Self::AlreadyLive { elem } => {
                write!(
                    f,
                    "{elem:?} is already live; identities are permanently spent"
                )
            }
            Self::IdentitySpent { elem } => {
                write!(
                    f,
                    "{elem:?} was spent by earlier history and can never be re-created"
                )
            }
            Self::DanglingEndpoint { eid, endpoint } => {
                write!(f, "{eid:?} names endpoint {endpoint:?}, which is not live")
            }
            Self::CompareAndSetMismatch(mismatch) => write!(
                f,
                "compare-and-set of {:?} {:?} found a different value \
                 (expected and actual are redacted)",
                mismatch.elem, mismatch.name
            ),
            Self::Canonical(error) => write!(f, "canonical form: {error}"),
            Self::VertexStorageAdmission { vid, source } => {
                write!(f, "vertex {vid:?} storage admission: {source}")
            }
            Self::EdgeStorageAdmission { eid, source } => {
                write!(f, "edge {eid:?} storage admission: {source}")
            }
            Self::MixedRelation { expected, found } => write!(
                f,
                "cannot extend a batch over {expected:?} with rows over {found:?}; \
                 the relation is the batch's template coordinate"
            ),
            Self::SnapshotPin(error) => {
                write!(f, "could not pin the transaction snapshot: {error}")
            }
            Self::FirstCommitterWins { law, detail } => write!(
                f,
                "write lost first-committer-wins under {law}: {detail}; rebuild the \
                 batch against the advanced snapshot and resubmit"
            ),
            Self::Commit(error) => write!(f, "commit stream: {error}"),
            Self::CommitOutcomeUnknown {
                published_frontier,
                source,
            } => write!(
                f,
                "commit outcome is unknown after published frontier {published_frontier:?}: \
                 {source}; reopen before reading or writing"
            ),
            Self::HandleCommitOutcomeUnknown { published_frontier } => write!(
                f,
                "this handle cannot determine whether Chronicle advanced past \
                 {published_frontier:?}; reopen before writing"
            ),
            Self::RecoveryRequired(recovery) => write!(
                f,
                "this handle is at {:?}, but Chronicle durably reached {:?} before \
                 {:?} failed; reopen before writing",
                recovery.published_frontier, recovery.durable_frontier, recovery.failed_stage
            ),
            Self::SlotGenerationExhausted(error) => error.fmt(f),
            Self::RootCapacity(error) => error.fmt(f),
            Self::CommittedNeedsRecovery { recovery, source } => write!(
                f,
                "commit {:?} is durable, but {:?} failed after the handle's published \
                 frontier {:?}: {source}; reopen before reading or writing",
                recovery.durable_frontier, recovery.failed_stage, recovery.published_frontier
            ),
        }
    }
}

impl core::fmt::Display for ReadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Root(error) => write!(f, "partition: {error}"),
            Self::CommitOutcomeUnknown { published_frontier } => write!(
                f,
                "this handle cannot determine whether Chronicle advanced past \
                 {published_frontier:?}; reopen before reading"
            ),
            Self::RecoveryRequired(recovery) => write!(
                f,
                "this handle is at {:?}, but Chronicle durably reached {:?} before \
                 {:?} failed; reopen before reading",
                recovery.published_frontier, recovery.durable_frontier, recovery.failed_stage
            ),
            Self::BeyondFrontier { asked, frontier } => write!(
                f,
                "asked about {asked:?}, beyond the published frontier {frontier:?}"
            ),
            Self::DeltaCursorRetired {
                asked,
                retained_after,
                frontier,
            } => write!(
                f,
                "delta cursor {asked:?} is not retained: window is ({retained_after:?}, {frontier:?}]"
            ),
            Self::DeltaWindow(error) => write!(f, "delta window: {error}"),
            Self::UnmappedLabel(id) => write!(f, "unmapped vertex label id: {id:?}"),
            Self::UnmappedRelation(id) => write!(f, "unmapped edge relation id: {id:?}"),
        }
    }
}

impl core::error::Error for OpenError {}
impl core::error::Error for RebuildError {}
impl core::error::Error for WriteError {}
impl core::error::Error for ReadError {}

/// Why one pinned GQL statement could not be answered
/// (fgdb-w5-parsers-nje.1). The three arms are three different remedies, so
/// they are not collapsed: a parse error means the TEXT is off-grammar, a
/// bind error means the statement is lawful but the caller's
/// [`RelationBind`] cannot name its relation, and a read error means the
/// statement and bind are fine but this handle currently refuses reads
/// (fenced, beyond-frontier, and so on — the same refusals every Rust-API
/// read surfaces).
#[derive(Debug)]
pub enum GqlError {
    /// The source text is not the pinned statement grammar.
    Parse(fgdb_gql::ParseError),
    /// The parsed statement names a relation the caller's bind map does not.
    Bind(fgdb_gql::BindError),
    /// The bound plan was executable but the handle refused the read.
    Read(ReadError),
    /// A `CALL` stage's procedure refused its arguments, graph or result.
    Procedure(ProcedureError),
}

impl core::fmt::Display for GqlError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Parse(error) => write!(f, "gql parse: {error}"),
            Self::Bind(error) => write!(f, "gql bind: {error}"),
            Self::Read(error) => write!(f, "gql read: {error}"),
            Self::Procedure(error) => write!(f, "gql procedure: {error}"),
        }
    }
}

impl core::error::Error for GqlError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Parse(error) => Some(error),
            Self::Bind(error) => Some(error),
            Self::Read(error) => Some(error),
            Self::Procedure(error) => Some(error),
        }
    }
}

/// What a failed CompareAndSet means on a [`WriteBatch`].
///
/// WriteBatch is one atomic write, not a multi-statement transaction.
/// Appendix B's `StatementError` is therefore not an arm — that policy
/// needs the statement machine (`fgdb-w2-txn-lifecycle-mhae`). Naming it
/// here would be a substitute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteMismatchPolicy {
    /// The guard does nothing. The rest of the batch continues.
    NoOp,
    /// Refuse the whole batch before anything durable happens.
    AbortWrite,
}

/// The two values a failed CompareAndSet compared, boxed through
/// [`WriteError::CompareAndSetMismatch`] so the error enum stays small.
///
/// `Debug` deliberately reports the guarded coordinate but redacts both
/// values. Property values are user graph data; a failed guard does not turn
/// them into safe diagnostic metadata.
#[derive(Clone, PartialEq, Eq)]
pub struct CompareAndSetMismatch {
    pub elem: ElementId,
    pub name: PropertyKeyId,
    pub expected: Option<CanonicalScalar>,
    pub actual: Option<CanonicalScalar>,
}

impl core::fmt::Debug for CompareAndSetMismatch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CompareAndSetMismatch")
            .field("elem", &self.elem)
            .field("name", &self.name)
            .field("expected", &"[REDACTED]")
            .field("actual", &"[REDACTED]")
            .finish()
    }
}

/// One batch of graph mutations, committed atomically.
///
/// `Debug` reports only the relation and row count. Pending rows can contain
/// user graph properties and therefore stay redacted until the batch is
/// consumed by the write path.
///
/// **THIS IS NOT THE TRANSACTION MODEL AND MUST NOT BE READ AS ONE.** A batch
/// is a set of rows that become durable together or not at all: it carries no
/// snapshot, no workspace, and no isolation semantics of its own. What IS
/// wired around it (fgdb-fcw-writebatch-6cxf): every `Database` constructor
/// installs [`crate::fcw::FirstCommitterWinsValidator`], and the basis-pinned
/// [`Database::prepare_write`] / [`Database::commit_prepared`] pair enforces
/// first-committer-wins over the canonical delta encoding, so a committed
/// write whose basis went stale is rejected instead of silently landing.
/// Still NOT wired here: multi-statement workspaces, statement lifecycle, and
/// snapshot-isolation/SSI semantics — those live in `fgdb-reference::txn` as
/// executable semantics and in `fgdb-w4-*` as the engine. Doctrine 7's line
/// stands: a subset may do LESS while a substitute pretends to do the same
/// thing; this type is named for what it is so it cannot be mistaken for the
/// other.
///
/// One batch carries one relation, because a `CoordinateEntry` names one.
///
/// **Deletes name the identity and nothing else** (fgdb-p3ok). A durable
/// `DeltaRow::DeleteEdge` carries a `before_version` and `DeleteVertex` a
/// complete cascade before-image, and a caller-supplied image would be an
/// assertion the caller could get wrong — so the ENGINE derives both at
/// commit time, from the fold's live state plus the batch prefix, and the
/// reference oracle re-validates every derived image at replay
/// (`ElementVersionMismatch` / `CascadeImageMismatch` are refusals, which is
/// what keeps the two derivations honest without sharing code). The
/// provenance envelopes and `NetEffectNormalForm` canonicalization stay with
/// `fgdb-w5-effects-normal-form-819`, which absorbs this surface.
#[derive(Clone)]
pub struct WriteBatch {
    relation: RelationId,
    rows: Vec<PendingRow>,
}

/// A batch whose canonical template was derived against one pinned live
/// snapshot by [`Database::prepare_write`], awaiting
/// [`Database::commit_prepared`].
///
/// The fields are private on purpose: the template is exactly what was
/// prepared (committing anything else would silently rebase the batch), and
/// the basis is the snapshot frontier the preparation read — the fact the
/// first-committer-wins verdict is about. It carries no handle borrow, so two
/// prepared writes can coexist against the same `Database`.
/// Commitment requires that same opened handle lifetime. Moving the handle is
/// allowed; reopening it does not transfer old prepared writes to the new one.
#[derive(Clone, Debug)]
pub struct PreparedWrite {
    template: LogicalDeltaTemplate,
    basis: CommitSeq,
    handle_owner: Arc<()>,
    dependencies: prepared_write::PreparedDependencies,
}

impl PreparedWrite {
    /// The snapshot frontier this batch's rows were derived against.
    pub const fn basis(&self) -> CommitSeq {
        self.basis
    }
}

/// The smallest inhabitable Local semantic-command union in the embedded
/// spine.  More lifecycle arms remain reserved in `command_contracts.toml`.
#[derive(Clone, Debug)]
pub enum LocalSemanticCommand {
    WriteBatch(LocalAutocommitWriteSpec),
}

/// Canonical input body for the live Local `WriteBatch` command arm.
#[derive(Clone, Debug)]
pub struct LocalAutocommitWriteSpec {
    pub batch: WriteBatch,
}

/// Client-facing result of applying one Local write batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalWriteBatchCommandResult {
    pub commit_seq: CommitSeq,
}

/// Durable applied-record projection produced by the Local write-batch arm.
///
/// The full batch bytes live in the `LocalDeltaBatchIndex`; this compact value
/// is the typed proof returned by the handler that the indexed batch and its
/// Chronicle marker reached this exact commit sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalWriteBatchAppliedRecord {
    pub commit_seq: CommitSeq,
}

/// Exhaustive result union for [`Database::apply_local_semantic_command`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalSemanticApplyResult {
    WriteBatch {
        result: LocalWriteBatchCommandResult,
        applied_record: LocalWriteBatchAppliedRecord,
    },
}

/// Machine-readable inventory consumed by the G0 command-contract checker.
/// Adding or removing a handler without the matching live registry row is red.
pub const LIVE_LOCAL_SEMANTIC_HANDLER_INVENTORY: &[(&str, &str, &str)] = &[(
    "cc:local:local-autocommit-write-spec",
    "fgdb::Database::apply_local_write_batch",
    "WriteBatch",
)];

impl core::fmt::Debug for WriteBatch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("WriteBatch")
            .field("relation", &self.relation)
            .field("row_count", &self.rows.len())
            .field("rows", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Debug)]
enum PendingRow {
    Vertex {
        vid: VId,
        labels: Vec<LabelId>,
        props: Vec<(PropertyKeyId, CanonicalScalar)>,
        /// `true` is [`WriteBatch::ensure_vertex`]: live identity is a
        /// no-op, not [`WriteError::AlreadyLive`].
        ensure: bool,
    },
    Edge {
        eid: EId,
        src: VId,
        dst: VId,
        props: Vec<(PropertyKeyId, CanonicalScalar)>,
        /// `true` is [`WriteBatch::ensure_edge_by_triple`]: a live
        /// `(src, relation, dst)` is a no-op even under a new eid.
        ensure: bool,
    },
    DeleteEdge {
        eid: EId,
        /// `true` is [`WriteBatch::delete_edge_if_present`]: missing is a
        /// no-op, not [`WriteError::UnknownEdge`].
        if_present: bool,
    },
    DeleteVertex {
        vid: VId,
        /// `true` is [`WriteBatch::delete_vertex_if_present`]: missing is a
        /// no-op, not [`WriteError::UnknownVertex`].
        if_present: bool,
    },
    SetLabel {
        vid: VId,
        label: LabelId,
        member: bool,
    },
    SetEdgeProperty {
        eid: EId,
        key: PropertyKeyId,
        value: Option<CanonicalScalar>,
    },
    SetProperty {
        vid: VId,
        key: PropertyKeyId,
        value: Option<CanonicalScalar>,
    },
    CompareAndSet {
        elem: ElementId,
        key: PropertyKeyId,
        expected: Option<Box<CanonicalScalar>>,
        value: Box<CanonicalScalar>,
        mismatch: WriteMismatchPolicy,
    },
}

impl WriteBatch {
    pub fn new(relation: RelationId) -> Self {
        Self {
            relation,
            rows: Vec::new(),
        }
    }

    /// Append every pending row of `other`, in order, after this batch's own
    /// (fgdb-w4-g1-txn-core-qpmg.2): the concatenation a transaction uses to
    /// make two staged writes one capsule. Rows keep their evaluation order —
    /// prefix semantics (same-batch before-images, cascade ownership,
    /// birth-ordinal numbering) are derived later, at prepare time, over the
    /// combined sequence exactly as if the caller had staged every row into
    /// one batch. Batches over different relations refuse before anything
    /// moves: the batch's relation is its template coordinate, and quietly
    /// re-homing rows onto another coordinate would change what they mean.
    pub fn extend(&mut self, other: WriteBatch) -> Result<(), WriteError> {
        if other.relation != self.relation {
            return Err(WriteError::MixedRelation {
                expected: self.relation,
                found: other.relation,
            });
        }
        self.rows.extend(other.rows);
        Ok(())
    }

    pub fn create_vertex(
        &mut self,
        vid: VId,
        labels: Vec<LabelId>,
        props: Vec<(PropertyKeyId, CanonicalScalar)>,
    ) -> &mut Self {
        self.rows.push(PendingRow::Vertex {
            vid,
            labels,
            props,
            ensure: false,
        });
        self
    }

    /// Create `vid` only if it is not already live. A second evaluation is
    /// a no-op; a second [`WriteBatch::create_vertex`] is still
    /// [`WriteError::AlreadyLive`]. Spent identities refuse
    /// [`WriteError::IdentitySpent`] — ensure is not resurrection.
    pub fn ensure_vertex(
        &mut self,
        vid: VId,
        labels: Vec<LabelId>,
        props: Vec<(PropertyKeyId, CanonicalScalar)>,
    ) -> &mut Self {
        self.rows.push(PendingRow::Vertex {
            vid,
            labels,
            props,
            ensure: true,
        });
        self
    }

    /// Create the edge. Both endpoints must be live at this point in the
    /// batch or the write refuses [`WriteError::DanglingEndpoint`]
    /// before D2 (fgdb-r196).
    pub fn add_edge(
        &mut self,
        eid: EId,
        src: VId,
        dst: VId,
        props: Vec<(PropertyKeyId, CanonicalScalar)>,
    ) -> &mut Self {
        self.rows.push(PendingRow::Edge {
            eid,
            src,
            dst,
            props,
            ensure: false,
        });
        self
    }

    /// Create the edge only if no live `(src, this batch's relation, dst)`
    /// exists. Named `ensure_edge_by_triple` because the constraint-keyed
    /// `EnsureEdge` is not this method (fgdb-ensure-edge-constraint-counterfeit-xa2x).
    /// A new triple still requires live endpoints
    /// ([`WriteError::DanglingEndpoint`]).
    pub fn ensure_edge_by_triple(
        &mut self,
        eid: EId,
        src: VId,
        dst: VId,
        props: Vec<(PropertyKeyId, CanonicalScalar)>,
    ) -> &mut Self {
        self.rows.push(PendingRow::Edge {
            eid,
            src,
            dst,
            props,
            ensure: true,
        });
        self
    }

    /// Delete the edge `eid`. The durable row's `before_version` is derived
    /// by the engine at commit time; deleting an edge this database does not
    /// hold refuses before anything durable happens.
    pub fn delete_edge(&mut self, eid: EId) -> &mut Self {
        self.rows.push(PendingRow::DeleteEdge {
            eid,
            if_present: false,
        });
        self
    }

    /// Delete `eid` only if it is live. A missing or already-deleted edge
    /// is a no-op; [`WriteBatch::delete_edge`] is still
    /// [`WriteError::UnknownEdge`].
    pub fn delete_edge_if_present(&mut self, eid: EId) -> &mut Self {
        self.rows.push(PendingRow::DeleteEdge {
            eid,
            if_present: true,
        });
        self
    }

    /// Delete the vertex `vid` and every edge touching it. The cascade
    /// before-image — the exact incident set, both directions, ascending —
    /// and the `before_version` are derived by the engine at commit time.
    pub fn delete_vertex(&mut self, vid: VId) -> &mut Self {
        self.rows.push(PendingRow::DeleteVertex {
            vid,
            if_present: false,
        });
        self
    }

    /// Delete `vid` only if it is live, with the same cascade as
    /// [`WriteBatch::delete_vertex`]. A missing or already-deleted vertex
    /// is a no-op; `delete_vertex` is still [`WriteError::UnknownVertex`].
    pub fn delete_vertex_if_present(&mut self, vid: VId) -> &mut Self {
        self.rows.push(PendingRow::DeleteVertex {
            vid,
            if_present: true,
        });
        self
    }

    /// Set or clear `vid`'s membership in `label`. The durable row's
    /// before-image is derived by the engine at commit time.
    pub fn set_vertex_label(&mut self, vid: VId, label: LabelId, member: bool) -> &mut Self {
        self.rows.push(PendingRow::SetLabel { vid, label, member });
        self
    }

    /// Set (`Some`) or unset (`None`) one property of `vid`. The durable
    /// row's before-image is derived by the engine at commit time.
    pub fn set_vertex_property(
        &mut self,
        vid: VId,
        key: PropertyKeyId,
        value: Option<CanonicalScalar>,
    ) -> &mut Self {
        self.rows.push(PendingRow::SetProperty { vid, key, value });
        self
    }

    /// Set (`Some`) or unset (`None`) one property of the edge `eid`
    /// (fgdb-ls5b). The durable row's before-image is derived by the engine
    /// at commit time; durably, the live statement retires and a content
    /// successor begins — so pre-update snapshots keep answering the old row.
    pub fn set_edge_property(
        &mut self,
        eid: EId,
        key: PropertyKeyId,
        value: Option<CanonicalScalar>,
    ) -> &mut Self {
        self.rows
            .push(PendingRow::SetEdgeProperty { eid, key, value });
        self
    }

    /// Set `vid`'s `key` to `value` only if it currently equals `expected`.
    ///
    /// [`WriteMismatchPolicy::AbortWrite`] refuses the batch before D2.
    /// [`WriteMismatchPolicy::NoOp`] emits no row on a mismatch. A gone
    /// element is observed as `None`: `expected=Some` is a NoOp mismatch;
    /// `expected=None` matches and then refuses [`WriteError::UnknownVertex`]
    /// / [`WriteError::UnknownEdge`] (fgdb-pmj7). There is no
    /// `StatementError` arm — WriteBatch is one write.
    pub fn compare_and_set_vertex_property(
        &mut self,
        vid: VId,
        key: PropertyKeyId,
        expected: Option<CanonicalScalar>,
        value: CanonicalScalar,
        mismatch: WriteMismatchPolicy,
    ) -> &mut Self {
        self.rows.push(PendingRow::CompareAndSet {
            elem: ElementId::Vertex(vid),
            key,
            expected: expected.map(Box::new),
            value: Box::new(value),
            mismatch,
        });
        self
    }

    /// Set `eid`'s `key` to `value` only if it currently equals `expected`.
    pub fn compare_and_set_edge_property(
        &mut self,
        eid: EId,
        key: PropertyKeyId,
        expected: Option<CanonicalScalar>,
        value: CanonicalScalar,
        mismatch: WriteMismatchPolicy,
    ) -> &mut Self {
        self.rows.push(PendingRow::CompareAndSet {
            elem: ElementId::Edge(eid),
            key,
            expected: expected.map(Box::new),
            value: Box::new(value),
            mismatch,
        });
        self
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }
}

/// One edge's full answer: the winning adjacency statement and the
/// properties its block's hosted patch carries (fgdb-yqor).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdgeRecord {
    pub entry: AdjacencyEntry,
    pub props: EdgePropertyRow,
}

/// The published tier-D snapshot a reader is served from.
#[derive(Clone, Debug)]
struct Snapshot {
    blocks: Vec<Vec<AdjacencyEntry>>,
    /// The derived adjacency index, read through [`Snapshot::adjacency_index`].
    /// Every writable path builds it with the generation (commits extend it
    /// incrementally); a read-only view builds it on first use, because a
    /// point read by property never touches adjacency.
    adjacency: std::sync::OnceLock<Arc<gql_exec::source::AdjacencyIndex>>,
    property_index: Arc<gql_exec::source::PropertyEqualityIndex>,
    /// The root's block references, aligned with `blocks`. Retained so the
    /// next commit can tell which decoded blocks the new root carries forward
    /// unchanged (fgdb-gieu) — content addressing makes the identity the
    /// proof, so an unchanged reference means an unchanged decoded block.
    refs: Vec<BlockRef>,
    /// Each block's decoded property sidecar, aligned with `blocks`
    /// (fgdb-yqor): the locator column plus the hosted patch's rows, or
    /// `None` for a propertyless block.
    block_props: Vec<Option<BlockProps>>,
    /// The decoded vertex row patches, aligned with `patch_refs` — the vertex
    /// half of the snapshot (fgdb-3xoi), under the same carry-forward rule.
    patches: Vec<VertexPatchRows>,
    patch_refs: Vec<PatchRef>,
    frontier: CommitSeq,
    root: PartitionRootVersion,
    /// The manifest published beside `root` (fgdb-63w2) — the identity a
    /// root slot carries, re-derived identically by every rebuild.
    manifest: ManifestVersion,
    /// The ordered local delta window derived from the same Chronicle cut as
    /// every graph field above. Keeping it inside the immutable generation is
    /// what makes a pinned view one coherent graph-and-delta publication:
    /// writes build a successor off-side, and compaction carries this exact
    /// window into its replacement generation.
    delta_index: LocalDeltaBatchIndex,
}

/// What the NEXT commit needs and no read does: the version chain heads and
/// the birth-ordinal allocator of the published generation. They live on the
/// writing handle, beside its [`BlockWriter`], not in the [`Snapshot`] a read
/// view shares. A view can therefore be issued without deriving them, and a
/// commit while a view is pinned no longer copies them with the snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
struct WriteHeads {
    /// The current element-version chain head of every LIVE element,
    /// derived by folding the stream (fgdb-p3ok). This is the state a
    /// delete's `before_version` names, so it is engine state — but the
    /// DERIVATION is deliberately an independent spelling of the reference
    /// oracle's, never shared code: the differential's replay path validates
    /// every emitted image against the oracle's own chains, so a drift in
    /// either implementation is a refusal, not a silent agreement.
    versions: std::collections::BTreeMap<ElementId, ObjectId>,
    /// The next unspent birth ordinal, derived by counting the creations the
    /// durable stream already contains. Derived rather than stored: identity
    /// allocation is `fgdb-w2`'s, and a counter persisted here would be a second
    /// authority beside the stream.
    next_birth_ordinal: u64,
}

fn read_error_from_index(error: IndexError) -> ReadError {
    match error {
        IndexError::BeyondFrontier { asked, frontier } => {
            ReadError::BeyondFrontier { asked, frontier }
        }
        IndexError::CursorRetired {
            asked,
            retained_after,
            frontier,
        } => ReadError::DeltaCursorRetired {
            asked,
            retained_after,
            frontier,
        },
        other => ReadError::DeltaWindow(other),
    }
}

impl Snapshot {
    fn check_frontier(&self, as_of: CommitSeq) -> Result<(), ReadError> {
        if as_of.0 > self.frontier.0 {
            return Err(ReadError::BeyondFrontier {
                asked: as_of,
                frontier: self.frontier,
            });
        }
        Ok(())
    }

    fn neighbours_at(
        &self,
        src: VId,
        relation: RelationId,
        as_of: CommitSeq,
    ) -> Result<Vec<VId>, ReadError> {
        // The generation's blocks passed the whole-history validator when they
        // were published or reopened; the maintained index answers from that
        // admitted state instead of re-collapsing every block per read.
        self.check_frontier(as_of)?;
        Ok(self.adjacency_index().neighbours_at(
            &self.blocks,
            src,
            relation,
            fgdb_gql::algebra::GlaDirection::Forward,
            as_of,
        ))
    }

    /// The generation's adjacency index, built from its blocks on first use
    /// when it was not built with the generation. The build is a pure,
    /// deterministic function of `blocks`, so a lazy index equals an eager one.
    fn adjacency_index(&self) -> &Arc<gql_exec::source::AdjacencyIndex> {
        self.adjacency
            .get_or_init(|| Arc::new(gql_exec::source::AdjacencyIndex::build(&self.blocks)))
    }

    fn in_neighbours_at(
        &self,
        dst: VId,
        relation: RelationId,
        as_of: CommitSeq,
    ) -> Result<Vec<VId>, ReadError> {
        self.check_frontier(as_of)?;
        Ok(self.adjacency_index().neighbours_at(
            &self.blocks,
            dst,
            relation,
            fgdb_gql::algebra::GlaDirection::Reverse,
            as_of,
        ))
    }

    fn edge_at(&self, eid: EId, as_of: CommitSeq) -> Result<Option<EdgeRecord>, ReadError> {
        self.check_frontier(as_of)?;
        Ok(self
            .adjacency_index()
            .statement_at(&self.blocks, eid, as_of)
            .map(|(block, row)| EdgeRecord {
                entry: self.blocks[block][row],
                props: self
                    .block_props
                    .get(block)
                    .and_then(Option::as_ref)
                    .map(|props| props.props_of(row))
                    .unwrap_or_default(),
            }))
    }

    fn vertex_at(&self, vid: VId, as_of: CommitSeq) -> Result<Option<VertexRow>, ReadError> {
        self.check_frontier(as_of)?;
        Ok(merge_vertex(&self.patches, vid, as_of))
    }

    fn vertices_at(&self, as_of: CommitSeq) -> Result<Vec<VertexRow>, ReadError> {
        self.check_frontier(as_of)?;
        Ok(merge_all_vertices(&self.patches, as_of))
    }

    fn edges_at(&self, as_of: CommitSeq) -> Result<Vec<EdgeRecord>, ReadError> {
        self.check_frontier(as_of)?;
        Ok(
            merge_all_edges_with_props(&self.blocks, &self.block_props, as_of)?
                .into_iter()
                .map(|(entry, props)| EdgeRecord { entry, props })
                .collect(),
        )
    }
}

/// An immutable, in-process view of one already-authenticated published root.
///
/// Acquiring a view clones one [`Arc`], not the decoded blocks. A later write
/// uses copy-on-write only when it must preserve an older live view, while
/// compaction publishes a replacement generation without mutating the pinned
/// one. This is deliberately not the plan's posture-closed
/// `ReadSnapshot<Role>`: it has no authority binding, protocol cut, graph
/// bindings, or retention lease. Those and the cross-process retirement
/// protocol remain owned by `fgdb-w2-snapshots-leases-47e3`; this slice does
/// not authorize deletion of immutable objects or promise that a view survives
/// process death.
#[derive(Clone)]
#[must_use = "an embedded read view has no effect unless its pinned generation is read"]
pub struct EmbeddedReadView {
    snapshot: Arc<Snapshot>,
}

impl EmbeddedReadView {
    /// The exact published frontier this view is pinned to.
    #[must_use]
    pub fn frontier(&self) -> CommitSeq {
        self.snapshot.frontier
    }

    /// The manifest identity authenticated before this view was issued.
    #[must_use]
    pub fn manifest(&self) -> ManifestVersion {
        self.snapshot.manifest
    }

    /// The immutable partition-root identity this view reads.
    #[must_use]
    pub fn partition_root(&self) -> PartitionRootVersion {
        self.snapshot.root
    }

    /// The ordered delta window belonging to this exact graph generation.
    ///
    /// The returned reference points into the same shared [`Snapshot`] as the
    /// graph reads; no batch or index clone occurs when a view is acquired.
    /// A checkpoint-opened generation may retain only its frontier boundary.
    /// Inspect its retained floor or use [`Self::delta_since`], which refuses
    /// older cursors. Materializing the database's window does not mutate an
    /// already pinned view; acquire a new view after materialization.
    #[must_use]
    pub fn delta_index(&self) -> &LocalDeltaBatchIndex {
        &self.snapshot.delta_index
    }

    /// The delta frontier of this exact graph generation.
    #[must_use]
    pub fn delta_frontier(&self) -> CommitSeq {
        self.snapshot.delta_index.frontier()
    }

    /// Retained committed batches strictly after `after`, in commit order.
    pub fn delta_since(
        &self,
        after: CommitSeq,
    ) -> Result<impl Iterator<Item = &LogicalDeltaBatch> + '_, ReadError> {
        self.snapshot
            .delta_index
            .since(after)
            .map_err(read_error_from_index)
    }

    /// The live destinations of `src` over `relation` at this view's frontier.
    pub fn neighbours(&self, src: VId, relation: RelationId) -> Result<Vec<VId>, ReadError> {
        self.neighbours_at(src, relation, self.snapshot.frontier)
    }

    /// The destinations of `src` over `relation` at a sequence within this view.
    pub fn neighbours_at(
        &self,
        src: VId,
        relation: RelationId,
        as_of: CommitSeq,
    ) -> Result<Vec<VId>, ReadError> {
        self.snapshot.neighbours_at(src, relation, as_of)
    }

    /// The live sources of edges arriving at `dst` at this view's frontier.
    pub fn in_neighbours(&self, dst: VId, relation: RelationId) -> Result<Vec<VId>, ReadError> {
        self.in_neighbours_at(dst, relation, self.snapshot.frontier)
    }

    /// The sources of edges arriving at `dst` at a sequence within this view.
    pub fn in_neighbours_at(
        &self,
        dst: VId,
        relation: RelationId,
        as_of: CommitSeq,
    ) -> Result<Vec<VId>, ReadError> {
        self.snapshot.in_neighbours_at(dst, relation, as_of)
    }

    /// The edge `eid` at this view's frontier.
    pub fn edge(&self, eid: EId) -> Result<Option<EdgeRecord>, ReadError> {
        self.edge_at(eid, self.snapshot.frontier)
    }

    /// The edge `eid` at a sequence within this view.
    pub fn edge_at(&self, eid: EId, as_of: CommitSeq) -> Result<Option<EdgeRecord>, ReadError> {
        self.snapshot.edge_at(eid, as_of)
    }

    /// The vertex `vid` at this view's frontier.
    pub fn vertex(&self, vid: VId) -> Result<Option<VertexRow>, ReadError> {
        self.vertex_at(vid, self.snapshot.frontier)
    }

    /// The vertex `vid` at a sequence within this view.
    pub fn vertex_at(&self, vid: VId, as_of: CommitSeq) -> Result<Option<VertexRow>, ReadError> {
        self.snapshot.vertex_at(vid, as_of)
    }

    /// Every vertex at this view's frontier, in ascending VId order.
    pub fn vertices(&self) -> Result<Vec<VertexRow>, ReadError> {
        self.vertices_at(self.snapshot.frontier)
    }

    /// Every vertex at a sequence within this view.
    pub fn vertices_at(&self, as_of: CommitSeq) -> Result<Vec<VertexRow>, ReadError> {
        self.snapshot.vertices_at(as_of)
    }

    /// Every edge at this view's frontier, in ascending EId order.
    pub fn edges(&self) -> Result<Vec<EdgeRecord>, ReadError> {
        self.edges_at(self.snapshot.frontier)
    }

    /// Every edge at a sequence within this view.
    pub fn edges_at(&self, as_of: CommitSeq) -> Result<Vec<EdgeRecord>, ReadError> {
        self.snapshot.edges_at(as_of)
    }

    /// Verification seam for the zero-copy acquisition law. This compares
    /// only the private in-process allocation and carries no durable identity.
    #[doc(hidden)]
    #[must_use]
    pub fn shares_decoded_state_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.snapshot, &other.snapshot)
    }
}

/// An open database.
///
/// Holding one holds the commit stream's single-writer lease, so a second
/// `Database` over the same directory is refused by Chronicle rather than by a
/// convention here.
/// `opener_kind` 2 — PLAIN_STRATA_OBJECT (ruled on fgdb-ge6a, 2026-08-09,
/// under the owner's delegation). The slot's `root_manifest_oid` resolves
/// through the content-addressed [`BlockStore`], whose read-time identity
/// verification discharges the bootstrap's self-description duty. Under this
/// kind the object kind and byte lengths are REAL and every crypto/FEC
/// descriptor field is ZERO AND MUST BE ZERO — validated at open, refused
/// nonzero. A future FEC-backed posture is a DIFFERENT opener kind, never a
/// reinterpretation of this one.
pub const SLOT_OPENER_PLAIN_STRATA_OBJECT: u16 = 2;

/// The registered `IncarnationContinuityProfile` id for `DirectoryBound` —
/// the W1 embedded posture, which holds no external continuity head.
const SLOT_PROFILE_DIRECTORY_BOUND: u16 = 1;

/// The documented stand-in for Appendix A's `DatabaseId` until the root
/// stack owns it (the fgdb-sim precedent): deterministic in the security
/// namespace, replaced by the real field without changing the slot format.
fn spine_database_id(namespace: &DatabaseSecurityNamespaceId) -> [u8; 16] {
    let mut hasher = fgdb_crypto::Hasher::new();
    hasher.update(b"fgdb.spine.database-id.v1");
    hasher.update(&namespace.0);
    hasher.finalize().0[..16]
        .try_into()
        .expect("a 32-byte digest always yields 16")
}

/// The PLAIN opener's bootstrap: real object kind and lengths, zeros
/// everywhere the FEC/crypto machinery would live.
fn plain_bootstrap(manifest_len: u64, keys: &DatabaseKeys) -> RootBootstrap {
    let mut opener_payload = [0u8; fgdb_chronicle::root::OPENER_PAYLOAD_LEN];
    opener_payload[..2].copy_from_slice(&fgdb_strata::manifest::MANIFEST_OBJECT_KIND.to_le_bytes());
    // PLAIN bundle v1: object kind, bundle version, DEK-keyed authenticator.
    opener_payload[2..4].copy_from_slice(&1u16.to_le_bytes());
    let mut authenticator = fgdb_crypto::Hasher::new_keyed(keys.dek());
    authenticator.update(b"fgdb.spine.dek-commitment.v1");
    authenticator.update(&keys.namespace.0);
    opener_payload[4..36].copy_from_slice(&authenticator.finalize().0);
    RootBootstrap {
        root_encoding_id: [0; 32],
        root_placement_id: [0; 32],
        root_placement_epoch: 0,
        failure_domain_policy_id: 0,
        root_failure_domain_id: 0,
        segment_id: 0,
        offset: 0,
        encoded_len: manifest_len,
        root_symbol_inventory_digest: [0; 32],
        object_kind: fgdb_strata::manifest::MANIFEST_OBJECT_KIND,
        canonical_plaintext_len: manifest_len,
        codec_profile: 0,
        compressed_len: manifest_len,
        data_crypto_profile: 0,
        dek_id: [0; 16],
        nonce_len: 0,
        nonce_or_siv: [0; fgdb_chronicle::root::NONCE_CAPACITY],
        object_tag_len: 0,
        fec_profile: 0,
        transfer_length: manifest_len,
        oti_common: 0,
        oti_scheme: 0,
        symbol_size: 0,
        source_block_count: 0,
        symbol_auth_profile: 0,
        ciphertext_id: [0; 32],
        ciphertext_digest: [0; 32],
        opener_kind: SLOT_OPENER_PLAIN_STRATA_OBJECT,
        oid_key_id: [0; 16],
        opener_payload_len: 36,
        opener_payload,
        opener_digest: [0; 32],
    }
}

fn spine_slot(
    keys: &DatabaseKeys,
    generation: u64,
    manifest: ManifestVersion,
    manifest_len: u64,
) -> RootSlot {
    RootSlot {
        format_major: 1,
        format_minor: fgdb_chronicle::root::ROOT_FORMAT_MINOR,
        slot_generation: generation,
        local_writer_fence_epoch: 1,
        database_id: spine_database_id(&keys.namespace),
        database_security_namespace_id: keys.namespace.0,
        cluster_incarnation: 1,
        incarnation_continuity_profile_id: SLOT_PROFILE_DIRECTORY_BOUND,
        cluster_incarnation_continuity_digest: [0; 32],
        continuity_cas_version: 0,
        service_visibility_epoch: 0,
        root_manifest_oid: manifest.0.0,
        bootstrap: plain_bootstrap(manifest_len, keys),
    }
}

/// Derive the one lawful successor generation.
///
/// Keep this authority shared by ordinary writes, maintenance publication,
/// and open-time healing: duplicating the arithmetic would let one path wrap
/// after the others learned to fail closed.
fn next_slot_generation(current: u64) -> Result<u64, SlotGenerationExhausted> {
    current
        .checked_add(1)
        .ok_or(SlotGenerationExhausted { current })
}

/// The zero-validation half of the PLAIN opener ruling: a slot whose
/// identity tuple, opener form, or must-be-zero region disagrees is not this
/// database's slot and is refused, never reinterpreted.
fn validate_plain_slot(slot: &RootSlot, keys: &DatabaseKeys, path: &Path) -> Result<(), OpenError> {
    let mut expected = spine_slot(
        keys,
        slot.slot_generation,
        ManifestVersion(ObjectId(slot.root_manifest_oid)),
        slot.bootstrap.canonical_plaintext_len,
    );
    expected.format_minor = slot.format_minor;
    let mut difference = 0u8;
    if slot.format_minor == 0 {
        // Legacy slots carry no DEK evidence; capsule authentication remains
        // authoritative for these pre-v1 databases.
        expected.bootstrap.opener_payload_len = 2;
        expected.bootstrap.opener_payload[2..36].fill(0);
    } else {
        for (actual, wanted) in slot.bootstrap.opener_payload[4..36]
            .iter()
            .zip(&expected.bootstrap.opener_payload[4..36])
        {
            difference |= actual ^ wanted;
        }
        // Structural equality must not short-circuit on the authenticator.
        expected.bootstrap.opener_payload[4..36]
            .copy_from_slice(&slot.bootstrap.opener_payload[4..36]);
    }
    if *slot != expected {
        return Err(OpenError::ForeignSlot {
            path: path.to_path_buf(),
        });
    }
    if difference != 0 {
        return Err(OpenError::WrongDek {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

/// The canonical byte length of the snapshot's single-record manifest —
/// recomputed rather than stored, because the slot's bootstrap carries it
/// and a stored copy could drift from the encoder.
fn manifest_bytes_len(snapshot: &Snapshot) -> Result<u64, OpenError> {
    let records = [ManifestRecord {
        graph: GRAPH,
        branch: BRANCH,
        partition: PARTITION,
        root: snapshot.root,
        // Length-only computation: every V2 record is RECORD_LEN regardless of
        // the commitment value, so the zero digest cannot drift the answer.
        published_chain_hash: Digest([0u8; 32]),
    }];
    let bytes = encode_manifest(&records).map_err(|_| OpenError::NotADatabase {
        path: PathBuf::new(),
        missing: "an encodable manifest",
    })?;
    Ok(bytes.len() as u64)
}

/// The Chronicle chain commitment at `at` (fgdb-90hw): the chain value AFTER
/// the marker committed at that sequence, the origin for the empty stream, or
/// `None` when the recovered chain is SHORTER than `at` — which is exactly the
/// future-frontier slot a checkpoint binding must refuse.
fn chain_commitment_at(chain: &fgdb_chronicle::MarkerChain, at: CommitSeq) -> Option<Digest> {
    if at.0 == 0 {
        return Some(fgdb_chronicle::marker::CHAIN_ORIGIN);
    }
    let entry = chain.entries().get((at.0 - 1) as usize)?;
    debug_assert_eq!(entry.marker.commit_seq, at.0, "the chain is gap-free");
    Some(entry.chain_hash)
}

/// An embedded database handle.
///
/// Its `Debug` view is deliberately structural: state, publication frontier,
/// and retained-shape counts are visible, while keys, pending durability
/// internals, and the decoded graph snapshot remain redacted.
pub struct Database<V: Vfs = UnixVfs> {
    /// In-process ownership of this opened writer lifetime. Retaining this
    /// opaque allocation prevents prepared writes/transactions from migrating
    /// to another handle, even with identical keys, paths or frontiers. It
    /// survives moves and publication, but reopening earns a fresh identity.
    /// This is not a durable identity or an authorization/session protocol.
    handle_owner: Arc<()>,
    standing_queries: Vec<standing_query::StandingQuery>,
    coordinator: CommitCoordinator<V>,
    store: BlockStore<V>,
    /// The ONE mutable object in the directory (doctrine 5): the dual-slot
    /// root file whose selected slot names the current manifest (fgdb-ge6a,
    /// PLAIN opener ruling). Published after every manifest, reconciled at
    /// every open.
    slot_store: RootStore<V>,
    /// The generation the NEXT slot publication will carry; monotone.
    slot_generation: u64,
    keys: DatabaseKeys,
    snapshot: Arc<Snapshot>,
    /// The persistent fold over every committed row, retained so a commit can
    /// fold only its own template instead of re-reading the whole history
    /// (`fgdb-fujt`: the per-commit rebuild's capsule re-read loop measured at
    /// 95% of an O(history) marginal write cost, ffe05f6). Never authoritative:
    /// it is seeded by the full rebuild at open, `rebuild()` remains the only
    /// recovery path, and `incremental_publish_equals_rebuild.rs` pins that a
    /// clone-publish of this writer is byte-identical to that rebuild.
    writer: BlockWriter,
    /// The published generation's version heads and birth-ordinal allocator,
    /// derived with the writer and replaced with it at every publication.
    heads: WriteHeads,
    /// Per-open-handle, engine-owned identity reservations. Reopen seeds the
    /// durable floor from all admitted partition history, including tombstones.
    identity_allocation: std::sync::Arc<std::sync::Mutex<crate::write_txn::IdentityAllocation>>,
    /// Prefix omitted by checkpoint open, distinct from a subsequently retired
    /// window. Only explicit authenticated reconstruction can lower this cut.
    delta_materialized_after: CommitSeq,
    /// Exact native preparation state retained before the first mutation of a
    /// lazy-open writer. Historical staging at or above that cut can replay the
    /// loaded suffix without reading the omitted origin prefix. Captured only
    /// when the writer changes, and released after origin materialization.
    preparation_anchor: Option<prepared_write::PreparationAnchor>,
    /// Truthfulness fence for the retained writer/snapshot pair. D2 moves
    /// this out of `Healthy` before any derived work can fail; only completing
    /// the snapshot swap (or constructing a fresh handle in `open`) moves it
    /// back. Keeping the stale values allocated is harmless because every
    /// public graph read and every write checks this state first.
    state: DatabaseState,
    /// Durability-and-admission receipts for the objects the current
    /// generation names (fgdb-gieu). Session-scoped like the writer above:
    /// never authoritative and never persisted. Open earns them by publishing,
    /// or seeds them from the admission of the root the slot selected
    /// (fgdb-ibbuq).
    receipts: PublishReceipts,
    /// The durable I/O authority retained so same-handle recovery reopens
    /// through the SAME injected filesystem rather than silently escaping to
    /// `UnixVfs`. Chronicle and Strata's `BlockStore` both open through this
    /// one value (fgdb-tvg8.1), so an injected fault plan bites the commit
    /// log, `manifest.root`, and the derived Tier-D objects alike.
    vfs: V,
    /// Secret-free verification records emitted while this handle reopened
    /// Chronicle capsules. Retained on the handle so product callers can
    /// forward them to the eventual observatory without a silent/no-op sink.
    /// They are diagnostic evidence only, never recovery authority.
    crypto_verification_events: Vec<CryptoVerificationEvent>,
    /// Monotone source of fresh [`ObligationId`]s for [`Database::begin`]'s
    /// snapshot pins (fgdb-writetxn-pin-l8wb). Handle-scoped like the writer:
    /// obligation identity is per-context liveness bookkeeping, never durable
    /// state, so a fresh process restarting from 1 is correct by design.
    next_txn_obligation: u64,
}

impl<V: Vfs> core::fmt::Debug for Database<V> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "Database {{ state: {:?}, slot_generation: {}, published_frontier: {:?}, \
             snapshot_block_count: {}, snapshot_patch_count: {}, coordinator: \
             CommitCoordinator(CapsuleKeys([REDACTED])), store: BlockStore([REDACTED]), \
             keys: DatabaseKeys([REDACTED]), graph_state: \"[REDACTED]\" }}",
            self.state,
            self.slot_generation,
            self.snapshot.frontier,
            self.snapshot.blocks.len(),
            self.snapshot.patches.len()
        )
    }
}

impl Database<UnixVfs> {
    /// Create a database in `path`, which must be absent or an empty directory.
    ///
    /// When `path` is absent, its immediate parent must already exist. Creation
    /// is deliberately one component at a time: after creating the database
    /// directory, this method syncs that directory and then its parent before
    /// opening Chronicle or Strata beneath it. Recursively creating missing
    /// ancestors without an inode-to-parent barrier for every component would
    /// let a successful create disappear after a crash.
    ///
    /// For the in-memory equivalent, see [`Database::<MemVfs>::open_memory`].
    pub async fn create(
        cx: &CommitCx,
        path: impl AsRef<Path>,
        keys: DatabaseKeys,
    ) -> Result<Self, OpenError> {
        Self::create_with_vfs(cx, UnixVfs::new(), path, keys).await
    }

    /// Open the database in `path`.
    ///
    /// There is no `":memory:"` branch here on purpose: this method's `Self`
    /// is the [`UnixVfs`]-backed database, while a memory database is a
    /// `Database<MemVfs>` — a different type no path-string branch can
    /// return. The explicit constructor is the honest shape; see
    /// [`Database::<MemVfs>::open_memory`].
    ///
    /// Fails closed when `path` does not hold one. See
    /// [`OpenError::NotADatabase`] for why this cannot simply delegate to
    /// `CommitCoordinator::open`.
    pub async fn open(
        cx: &CommitCx,
        path: impl AsRef<Path>,
        keys: DatabaseKeys,
    ) -> Result<Self, OpenError> {
        Self::open_with_vfs(cx, UnixVfs::new(), path, keys).await
    }

    /// Make a database directory that was copied without a sync safe to open
    /// for writing (fgdb-ibbuq).
    ///
    /// A writable open trusts publication. Every object the root slot's root
    /// names was synced before the slot named it, so [`Database::open`] seeds
    /// its publish receipts from the reopen, and the first commit syncs only
    /// what it writes. A copy made with `cp -r`, an archive tool or similar
    /// has had no such sync. Its files can sit in the page cache only, and a
    /// commit that trusted them could publish a root over bytes a crash then
    /// loses. Adopt such a copy once, before its first writable open.
    ///
    /// Adopting syncs every regular file under `path`, then every directory
    /// from the deepest up, then `path`'s parent, so each name becomes durable
    /// after the inode it names. It follows no symlinks, needs no keys, and
    /// refuses a `path` that does not hold a database. Adopting a database
    /// that was never copied is harmless; it only costs the syncs.
    pub async fn adopt(cx: &CommitCx, path: impl AsRef<Path>) -> Result<Adopted, OpenError> {
        Self::adopt_with_vfs(cx, UnixVfs::new(), path).await
    }

    /// Open `path` for reads only, returning a view of its published
    /// generation.
    ///
    /// Before trusting a root, this authenticates exactly what
    /// [`Database::open`] does: Chronicle recovery under the writer lease, the
    /// root slot, and the selected manifest's chain binding (one shared
    /// selection), then the same verified reopen of every block and patch the
    /// root names, with the same root admission. What it skips is the state
    /// only a writer needs: the retained fold, the element-version heads, the
    /// birth-ordinal allocator, and the delta history. The view's delta window
    /// is empty at its frontier, so a change cursor older than the frontier is
    /// refused as retired, never answered from a partial window.
    ///
    /// The fast path needs durable state that is already current: a slot that
    /// names a root published at the recovered chain's frontier. Anything else
    /// (no slot yet, or commits past the root after a crash or lag) needs the
    /// writes a full open performs to heal it. In that case this opens fully
    /// and keeps that handle's generation. Either way the writer lease is
    /// released before this returns: the view owns no lease and cannot write.
    pub async fn open_read_view(
        cx: &CommitCx,
        path: impl AsRef<Path>,
        keys: DatabaseKeys,
    ) -> Result<EmbeddedReadView, OpenError> {
        Self::open_read_view_with_vfs(cx, UnixVfs::new(), path, keys).await
    }

    /// Open the commit stream and the block store, then rebuild the fold.
    ///
    /// The database-ness decision belongs to the two callers above; by here it
    /// has been made.
    /// The forced-rebuild face for checkpoint equivalence: identical to
    /// [`Database::open`] except that the manifest-selected checkpoint is
    /// bypassed and the whole stream is folded into a fresh root. Ordinary
    /// open verifies the selected root's marker-chain binding and replays only
    /// its suffix; this face remains the independent full-fold oracle.
    #[doc(hidden)]
    pub async fn open_rebuilding(
        cx: &CommitCx,
        path: impl AsRef<Path>,
        keys: DatabaseKeys,
    ) -> Result<Self, OpenError> {
        let path = path.as_ref();
        if !path.join(CAPSULE_DIR).is_dir() {
            return Err(OpenError::NotADatabase {
                path: path.to_path_buf(),
                missing: CAPSULE_DIR,
            });
        }
        Self::bind_with_vfs(cx, UnixVfs::new(), path, keys, true).await
    }
}

impl Database<MemVfs> {
    /// Open a fresh, private, in-memory database — the embedded spine's
    /// `":memory:"` surface (the README's `Database::open(":memory:")`).
    ///
    /// The literal-string form cannot live on [`Database::open`]: that method
    /// is inherent to `Database<UnixVfs>` and returns `Self`, while a memory
    /// database is a `Database<MemVfs>` — a different type, so one signature
    /// cannot branch on the path text and return both. This explicit
    /// constructor is the honest shape, and it is not a second write path:
    /// it builds a private [`MemVfs`] and then goes through the SAME
    /// [`Database::create_with_vfs`] law a disk database obeys (empty-root
    /// check, directory-durability ordering, Chronicle open, Strata open,
    /// fold rebuild) over a filesystem whose bytes live in RAM. The two-fsync
    /// commit protocol runs in full; its barriers are trivially satisfied
    /// because every reader observes the same memory (the `memvfs` module
    /// docs say exactly why that is correct and not a lie).
    ///
    /// The storage is born empty and dies with the handle: when the returned
    /// `Database` drops, its `MemVfs` drops with it, and the database is
    /// gone — a memory database is never recoverable from a path. To reopen
    /// one within the process, retain a clone of a `MemVfs` you created
    /// yourself and use the explicit-VFS constructors:
    ///
    /// [`Database::create`]'s law (refuse an existing database) under open's
    /// name: what `open(":memory:")` opens is a database that cannot have a
    /// prior history.
    pub async fn open_memory(cx: &CommitCx, keys: DatabaseKeys) -> Result<Self, OpenError> {
        let vfs = MemVfs::new()?;
        let dir = vfs.database_dir();
        Self::create_with_vfs(cx, vfs, dir, keys).await
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Create through an explicit filesystem and cross the database root's own
    /// namespace durability barrier before creating any children.
    ///
    /// Production uses [`Database::create`]. The explicit form is the lab seam
    /// that proves the root directory obeys the same inode-then-parent ordering
    /// as Chronicle and Strata entries below it. The Strata block store opens
    /// through this same `vfs` (fgdb-tvg8.1), so creation faults reach its
    /// directory and lock-inode barriers too.
    #[doc(hidden)]
    pub async fn create_with_vfs(
        cx: &CommitCx,
        vfs: V,
        path: impl AsRef<Path>,
        keys: DatabaseKeys,
    ) -> Result<Self, OpenError> {
        Self::create_with_vfs_inner(cx, vfs, path.as_ref(), keys, None).await
    }

    /// Stop creation at the named root-directory durability instant.
    ///
    /// The ordinary constructor delegates to the same implementation with no
    /// crash point, so a crash matrix cannot accidentally exercise a weaker
    /// copy. The point is reached only when `path` was absent and this call
    /// created it; an already-existing empty directory has no new root name to
    /// lose. This stops before Chronicle or Strata can create child entries.
    #[doc(hidden)]
    pub async fn create_with_vfs_at_crash(
        cx: &CommitCx,
        vfs: V,
        path: impl AsRef<Path>,
        keys: DatabaseKeys,
        crash_at: DatabaseCreateCrashPoint,
    ) -> Result<Self, OpenError> {
        Self::create_with_vfs_inner(cx, vfs, path.as_ref(), keys, Some(crash_at)).await
    }

    async fn create_with_vfs_inner(
        cx: &CommitCx,
        vfs: V,
        path: &Path,
        keys: DatabaseKeys,
        crash_at: Option<DatabaseCreateCrashPoint>,
    ) -> Result<Self, OpenError> {
        let path_buf = path.to_path_buf();
        let created_root = match cx.with_restriction_async(vfs.symlink_metadata(path)).await {
            Ok(metadata) if !metadata.file_type().is_dir() => {
                return Err(OpenError::NotADirectory { path: path_buf });
            }
            Ok(_) => false,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                cx.with_restriction_async(vfs.create_dir(path)).await?;
                true
            }
            Err(error) => return Err(OpenError::Io(error)),
        };

        match cx
            .with_restriction_async(vfs.symlink_metadata(&path.join(CAPSULE_DIR)))
            .await
        {
            Ok(_) => return Err(OpenError::AlreadyADatabase { path: path_buf }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(OpenError::Io(error)),
        }

        let mut entries = cx.with_restriction_async(vfs.read_dir(path)).await?;
        if cx
            .with_restriction_async(entries.next_entry())
            .await?
            .is_some()
        {
            return Err(OpenError::NotEmpty { path: path_buf });
        }

        sync_vfs_directory(cx, &vfs, path).await?;
        if created_root
            && crash_at
                == Some(DatabaseCreateCrashPoint::AfterDatabaseDirectorySyncBeforeParentSync)
        {
            return Err(OpenError::InjectedCreateCrash(
                DatabaseCreateCrashPoint::AfterDatabaseDirectorySyncBeforeParentSync,
            ));
        }
        let parent = path
            .parent()
            .filter(|candidate| !candidate.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        sync_vfs_directory(cx, &vfs, parent).await?;

        Self::bind_with_vfs(cx, vfs, path, keys, false).await
    }

    /// Open the integrated database while interposing `vfs` on Chronicle and
    /// `manifest.root` I/O.
    ///
    /// This is the lab-runtime seam for faults at the two-fsync authority
    /// protocol, the derived root-slot publication that follows it, and — as
    /// of fgdb-tvg8.1 — the Tier-D `BlockStore`'s block, patch, root, and
    /// manifest publication. One injected filesystem is the whole I/O plane;
    /// the only `std::fs` residue is process-liveness locking and a
    /// link-count probe, neither of which carries durable bytes.
    /// Production callers use [`Database::open`], which supplies [`UnixVfs`].
    #[doc(hidden)]
    pub async fn open_with_vfs(
        cx: &CommitCx,
        vfs: V,
        path: impl AsRef<Path>,
        keys: DatabaseKeys,
    ) -> Result<Self, OpenError> {
        let path = path.as_ref();
        require_database_dir(cx, &vfs, path).await?;
        Self::bind_with_vfs(cx, vfs, path, keys, false).await
    }

    /// [`Database::adopt`] through an explicit filesystem: the lab seam.
    #[doc(hidden)]
    pub async fn adopt_with_vfs(
        cx: &CommitCx,
        vfs: V,
        path: impl AsRef<Path>,
    ) -> Result<Adopted, OpenError> {
        let path = path.as_ref();
        require_database_dir(cx, &vfs, path).await?;
        let mut adopted = Adopted::default();
        let mut directories = vec![path.to_path_buf()];
        let mut next = 0;
        while let Some(directory) = directories.get(next).cloned() {
            next += 1;
            let mut entries = cx.with_restriction_async(vfs.read_dir(&directory)).await?;
            while let Some(entry) = cx.with_restriction_async(entries.next_entry()).await? {
                let kind = cx.with_restriction_async(entry.file_type()).await?;
                if kind.is_dir() {
                    directories.push(entry.path());
                } else if kind.is_file() {
                    let file = entry.path();
                    cx.with_restriction_async(async {
                        let file = vfs.open(&file, &OpenOptions::new().read(true)).await?;
                        file.sync_all().await
                    })
                    .await?;
                    adopted.files += 1;
                }
            }
        }
        // Breadth-first discovery lists every directory after its parent, so
        // the reverse order syncs each one after all of its descendants.
        for directory in directories.iter().rev() {
            sync_vfs_directory(cx, &vfs, directory).await?;
            adopted.directories += 1;
        }
        let parent = path
            .parent()
            .filter(|candidate| !candidate.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        sync_vfs_directory(cx, &vfs, parent).await?;
        Ok(adopted)
    }

    /// [`Database::open_read_view`] through an explicit filesystem: the lab
    /// seam, as [`Database::open_with_vfs`] is for the writable open.
    #[doc(hidden)]
    pub async fn open_read_view_with_vfs(
        cx: &CommitCx,
        vfs: V,
        path: impl AsRef<Path>,
        keys: DatabaseKeys,
    ) -> Result<EmbeddedReadView, OpenError> {
        let path = path.as_ref();
        require_database_dir(cx, &vfs, path).await?;
        if let Some(view) = Self::read_current_generation(cx, &vfs, path, &keys).await? {
            return Ok(view);
        }
        // The durable state needs healing first, and healing writes: open
        // fully, then keep only the generation that open published.
        let database = Self::bind_with_vfs(cx, vfs, path, keys, false).await?;
        Ok(EmbeddedReadView {
            snapshot: Arc::clone(&database.snapshot),
        })
    }

    /// The read-only open's fast path: `None` unless the slot selects a root
    /// published at the recovered chain's frontier, in which case nothing is
    /// left to fold or heal. The coordinator, and with it the writer lease,
    /// drops before this returns, so a fallback open can take it.
    async fn read_current_generation(
        cx: &CommitCx,
        vfs: &V,
        path: &Path,
        keys: &DatabaseKeys,
    ) -> Result<Option<EmbeddedReadView>, OpenError> {
        let coordinator =
            CommitCoordinator::open_with_vfs(cx, vfs.clone(), path, keys.capsule_keys()).await?;
        let store = open_block_store(cx, vfs, path, keys).await?;
        let probe = RootStore::with_vfs(vfs.clone(), path);
        let Some(checkpoint) =
            select_checkpoint(cx, &coordinator, &store, &probe, keys, path).await?
        else {
            return Ok(None);
        };
        let chain_frontier = coordinator
            .chain()
            .entries()
            .last()
            .map_or(0, |entry| entry.marker.commit_seq);
        if chain_frontier != checkpoint.published_at.0 {
            return Ok(None);
        }
        let (root, blocks, block_props, patches) = store.reopen(cx, checkpoint.root_id).await?;
        let mut snapshot = current_generation(
            keys,
            coordinator.chain(),
            checkpoint.root_id,
            root,
            blocks,
            block_props,
            patches,
        );
        snapshot.delta_index = LocalDeltaBatchIndex::empty_at(snapshot.frontier);
        Ok(Some(EmbeddedReadView {
            snapshot: Arc::new(snapshot),
        }))
    }

    /// The derived element-version heads, exposed for the fast-open
    /// equivalence law only. The v3 head is one hash per LIVE element over its
    /// statement chain; the graph-answer comparisons cannot see a head that
    /// chained through the wrong statements (an updated element's answers come
    /// from its final row alone), so the law compares this map directly —
    /// without it, checkpoint-derived heads could drift from the fold's and
    /// every gate would stay green (GoldBarn's review, thread fgdb-l96k).
    #[doc(hidden)]
    pub fn element_versions(
        &self,
    ) -> Result<&std::collections::BTreeMap<ElementId, ObjectId>, ReadError> {
        self.ensure_readable()?;
        Ok(&self.heads.versions)
    }

    // Type-erased because every constructor funnels through this bind: a
    // caller's `Send` proof stops at `dyn Future + Send` instead of descending
    // through recovery, rebuild and publication (fgdb-a5y6m).
    fn bind_with_vfs<'a>(
        cx: &'a CommitCx,
        vfs: V,
        path: &'a Path,
        keys: DatabaseKeys,
        force_rebuild: bool,
    ) -> SendFuture<'a, Result<Self, OpenError>>
    where
        V: 'a,
    {
        Box::pin(Self::bind_with_vfs_inner(
            cx,
            vfs,
            path,
            keys,
            force_rebuild,
        ))
    }

    async fn bind_with_vfs_inner(
        cx: &CommitCx,
        vfs: V,
        path: &Path,
        keys: DatabaseKeys,
        force_rebuild: bool,
    ) -> Result<Self, OpenError> {
        let mut coordinator =
            CommitCoordinator::open_with_vfs(cx, vfs.clone(), path, keys.capsule_keys()).await?;
        // Hold Chronicle's writer lease while authenticating the selected
        // root, before either checkpoint replay or forced rebuilding.
        let probe = RootStore::with_vfs(vfs.clone(), path);
        match probe.current(cx).await {
            Ok(slot) => validate_plain_slot(&slot, &keys, path)?,
            Err(SlotStoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(OpenError::Slot(error)),
        }
        // The product instance is first-committer-wins from the first commit
        // (fgdb-fcw-writebatch-6cxf): every `Database` constructor funnels
        // through this bind, so no product handle ever commits under
        // Chronicle's PassThrough default — that fixture remains only on a
        // bare coordinator. The write path re-pins the instance per basis
        // (see `write_with_faults` / `commit_prepared`); installing it here
        // guarantees the interval between open and first write is already
        // governed.
        coordinator.set_validator(Box::new(
            FirstCommitterWinsValidator::default()
                .with_scalar_resolver(keys.scalar_resolver.clone()),
        ));
        let store = open_block_store(cx, &vfs, path, &keys).await?;
        let mut crypto_verification_events = Vec::new();
        // CHECKPOINT-SELECTED PATH (fgdb-ge6a): select_checkpoint accepts the
        // slot's partition only once its chain binding holds;
        // reopen_from_verified_checkpoint then reopens that partition and
        // folds only the suffix. A missing slot falls back to a full rebuild
        // (and the reconciliation below creates it).
        let selected = if force_rebuild {
            None
        } else {
            select_checkpoint(cx, &coordinator, &store, &probe, &keys, path).await?
        };
        let checkpoint_selected = selected.is_some();
        let (mut snapshot, writer, heads, receipts) = match selected {
            Some(checkpoint) => {
                reopen_from_verified_checkpoint(
                    cx,
                    &coordinator,
                    &store,
                    &keys,
                    checkpoint.root_id,
                    &mut crypto_verification_events,
                )
                .await?
            }
            None => {
                rebuild(
                    cx,
                    &coordinator,
                    &store,
                    &keys,
                    &mut crypto_verification_events,
                )
                .await?
            }
        };
        // RECONCILE THE ROOT SLOT (fgdb-ge6a, the PLAIN opener ruling). The
        // stream is the source of truth and the rebuild just derived its
        // manifest; the slot is the durable pointer checkpoint-selected open
        // verifies and uses, so every open leaves it CURRENT or refuses:
        //   - missing file: an interrupted create (the crash window between
        //     the coordinator's birth and the first slot write) — create it;
        //   - naming the rebuilt manifest: continue from its generation;
        //   - naming a RESOLVABLE older manifest: the crash window between a
        //     commit's manifest and its slot — heal forward;
        //   - anything else: not this database's lawful slot; refuse.
        let slot_store = RootStore::with_vfs(vfs.clone(), path);
        let manifest_len = manifest_bytes_len(&snapshot)?;
        let slot_generation = match slot_store.recover(cx).await {
            Err(SlotStoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                let slot = spine_slot(&keys, 1, snapshot.manifest, manifest_len);
                slot_store
                    .create(cx, &slot)
                    .await
                    .map_err(OpenError::Slot)?;
                1
            }
            Err(error) => return Err(OpenError::Slot(error)),
            Ok(RootSelection::Selected { slot, .. })
            | Ok(RootSelection::IdenticalPair { slot }) => {
                validate_plain_slot(&slot, &keys, path)?;
                if slot.root_manifest_oid == snapshot.manifest.0.0 {
                    slot.slot_generation
                } else {
                    let stale = ManifestVersion(ObjectId(slot.root_manifest_oid));
                    let resolvable = match store.resolve_manifest(cx, stale).await {
                        Ok(resolved) if resolved.len() == 1 => {
                            let (record, root) = &resolved[0];
                            record.graph == GRAPH
                                && record.branch == BRANCH
                                && record.partition == PARTITION
                                && root.graph == GRAPH
                                && root.branch == BRANCH
                                && root.partition == PARTITION
                                // The same chain binding as checkpoint-selected
                                // open (fgdb-90hw): a stale-but-OURS manifest
                                // heals forward; a foreign or future one refuses.
                                && chain_commitment_at(coordinator.chain(), root.published_at)
                                    .is_some_and(|expected| {
                                        expected == record.published_chain_hash
                                    })
                        }
                        _ => false,
                    };
                    if !resolvable {
                        return Err(OpenError::SlotDisagreesWithStream {
                            path: path.to_path_buf(),
                            slot_manifest: ObjectId(slot.root_manifest_oid),
                        });
                    }
                    let healed = next_slot_generation(slot.slot_generation)?;
                    let next = spine_slot(&keys, healed, snapshot.manifest, manifest_len);
                    slot_store
                        .publish_evidenced(cx, &next)
                        .await
                        .map_err(OpenError::Slot)?;
                    healed
                }
            }
            Ok(selection) => {
                return Err(OpenError::SlotUnrecoverable {
                    path: path.to_path_buf(),
                    detail: format!("{selection:?}"),
                });
            }
        };
        // The authenticated partition already contains the complete graph.
        // A checkpoint-selected open retains only the exact marker boundary;
        // historical delta consumers explicitly materialize the missing prefix.
        // Full recovery retains its previous whole-stream window.
        let delta_materialized_after = if checkpoint_selected {
            snapshot.delta_index = empty_delta_window_at(cx, &coordinator, snapshot.frontier)?;
            snapshot.frontier
        } else {
            snapshot.delta_index =
                rebuild_delta_index(cx, &coordinator, &keys, &mut crypto_verification_events)
                    .await?;
            CommitSeq::ORIGIN
        };
        let identity_allocation =
            crate::write_txn::IdentityAllocation::from_partition(cx, &snapshot)?;
        let published_frontier = snapshot.frontier;
        Ok(Self {
            coordinator,
            store,
            slot_store,
            slot_generation,
            keys,
            state: DatabaseState::Healthy { published_frontier },
            snapshot: Arc::new(snapshot),
            writer,
            heads,
            // Earned by the rebuild's or suffix's publication, or seeded from
            // the slot-selected root's admission (fgdb-ibbuq).
            receipts,
            vfs,
            crypto_verification_events,
            next_txn_obligation: 0,
            identity_allocation: Arc::new(std::sync::Mutex::new(identity_allocation)),
            delta_materialized_after,
            preparation_anchor: None,
            handle_owner: Arc::new(()),
            standing_queries: Vec::new(),
        })
    }

    /// The handle's truthfulness state. This is diagnostic state, not a
    /// recovery authority: a fenced handle stays fenced; a successful
    /// publication or a fresh [`Database::open`] is what yields `Healthy`.
    pub fn state(&self) -> DatabaseState {
        self.state
    }

    /// Secret-free crypto/identity verification evidence from this handle's
    /// open/recovery work. The fixed records carry public descriptor facts and
    /// typed outcomes only; no keys, nonces, plaintext, or ciphertext bytes.
    #[must_use]
    pub fn crypto_verification_events(&self) -> &[CryptoVerificationEvent] {
        &self.crypto_verification_events
    }

    /// Pin the current healthy published generation for in-process reads.
    ///
    /// The returned view shares the decoded immutable state and owns no writer
    /// lease. It therefore remains readable while this handle commits or
    /// compacts into a successor generation. Acquisition from a fenced handle
    /// is refused: only a view known to match a published Chronicle frontier is
    /// issued.
    pub fn pinned_read_view(&self) -> Result<EmbeddedReadView, ReadError> {
        self.ensure_readable()?;
        Ok(EmbeddedReadView {
            snapshot: Arc::clone(&self.snapshot),
        })
    }

    /// Consume this handle and return one rebuilt from the authoritative
    /// durable stream when recovery is required.
    ///
    /// A post-D2 publication failure leaves the retained writer and snapshot
    /// deliberately fenced, while an interrupted D2 leaves the coordinator
    /// unable to decide whether its marker committed. Both cases need the
    /// ordinary open path: it re-reads Chronicle, authenticates or repairs the
    /// published root, and constructs a fresh derived snapshot. Consuming
    /// `self` is load-bearing because dropping its coordinator releases the
    /// exclusive writer lease before [`Database::open`] acquires a new one.
    ///
    /// A healthy handle is returned unchanged. Recovery failure consumes the
    /// old handle and returns the exact [`OpenError`] from authoritative open;
    /// it never falls back to the fenced snapshot or claims rollback.
    pub async fn recover_authoritatively(self, cx: &CommitCx) -> Result<Self, OpenError> {
        match self.state {
            DatabaseState::Healthy { .. } => Ok(self),
            DatabaseState::CommitOutcomeUnknown { .. }
            | DatabaseState::NeedsAuthoritativeRecovery(_) => {
                let path = self.path().to_path_buf();
                let keys = self.keys.clone();
                let vfs = self.vfs.clone();
                drop(self);
                Self::open_with_vfs(cx, vfs, path, keys).await
            }
        }
    }

    fn ensure_writable(&self) -> Result<(), WriteError> {
        match self.state {
            DatabaseState::Healthy { .. } => Ok(()),
            DatabaseState::CommitOutcomeUnknown { published_frontier } => {
                Err(WriteError::HandleCommitOutcomeUnknown { published_frontier })
            }
            DatabaseState::NeedsAuthoritativeRecovery(recovery) => {
                Err(WriteError::RecoveryRequired(recovery))
            }
        }
    }

    fn ensure_readable(&self) -> Result<(), ReadError> {
        match self.state {
            DatabaseState::Healthy { .. } => Ok(()),
            DatabaseState::CommitOutcomeUnknown { published_frontier } => {
                Err(ReadError::CommitOutcomeUnknown { published_frontier })
            }
            DatabaseState::NeedsAuthoritativeRecovery(recovery) => {
                Err(ReadError::RecoveryRequired(recovery))
            }
        }
    }

    fn mark_recovery_stage(
        &mut self,
        recovery: &mut RecoveryRequired,
        stage: DerivedPublicationStage,
    ) {
        recovery.failed_stage = stage;
        self.state = DatabaseState::NeedsAuthoritativeRecovery(*recovery);
    }

    fn fail_publication_if_requested(
        recovery: RecoveryRequired,
        fail_at: Option<DerivedPublicationStage>,
    ) -> Result<(), WriteError> {
        if fail_at == Some(recovery.failed_stage) {
            return Err(WriteError::CommittedNeedsRecovery {
                recovery,
                source: Box::new(RebuildError::InjectedPublicationFailure(
                    recovery.failed_stage,
                )),
            });
        }
        Ok(())
    }

    /// Commit a batch through the real two-fsync protocol, then republish the
    /// derived partition.
    ///
    /// Returns the sequence the batch landed at. Durability is established by
    /// the commit; the republish that follows is derived work, and its failure
    /// is reported without pretending the commit did not happen — a reopen
    /// rebuilds the same partition from the stream.
    pub async fn write(
        &mut self,
        cx: &CommitCx,
        batch: WriteBatch,
    ) -> Result<CommitSeq, WriteError> {
        let applied = self
            .apply_local_semantic_command(
                cx,
                LocalSemanticCommand::WriteBatch(LocalAutocommitWriteSpec { batch }),
            )
            .await?;
        let LocalSemanticApplyResult::WriteBatch { result, .. } = applied;
        Ok(result.commit_seq)
    }

    /// Exhaustively apply one inhabitable Local semantic command.
    pub async fn apply_local_semantic_command(
        &mut self,
        cx: &CommitCx,
        command: LocalSemanticCommand,
    ) -> Result<LocalSemanticApplyResult, WriteError> {
        match command {
            LocalSemanticCommand::WriteBatch(body) => self.apply_local_write_batch(cx, body).await,
        }
    }

    /// Apply the live Local write-batch arm through Chronicle D2 and the
    /// existing derived-state publication path.
    pub async fn apply_local_write_batch(
        &mut self,
        cx: &CommitCx,
        body: LocalAutocommitWriteSpec,
    ) -> Result<LocalSemanticApplyResult, WriteError> {
        let commit_seq = self
            .write_with_faults(cx, body.batch, None, None, None)
            .await?;
        Ok(LocalSemanticApplyResult::WriteBatch {
            result: LocalWriteBatchCommandResult { commit_seq },
            applied_record: LocalWriteBatchAppliedRecord { commit_seq },
        })
    }

    /// Begin the bounded one-batch write transaction
    /// (fgdb-writetxn-pin-l8wb): pin this handle's published frontier under a
    /// fresh [`TxnCx`] obligation and hand back the [`WriteTxn`] that
    /// prepares and commits against exactly that basis. The pin is
    /// context-visible liveness — the lab's obligation-leak oracle sees an
    /// abandoned transaction — not durable state. A fenced handle refuses
    /// here with the same typed errors as [`Database::write`], before any
    /// obligation is acquired. The ordinary `write` path is untouched.
    pub fn begin(&mut self, txn: &TxnCx) -> Result<WriteTxn, WriteError> {
        self.ensure_writable()?;
        self.next_txn_obligation = self
            .next_txn_obligation
            .checked_add(1)
            .expect("a handle cannot begin 2^64 transactions");
        let id = ObligationId::new(self.next_txn_obligation)
            .expect("the pin counter starts above zero and only increments");
        WriteTxn::begin(
            self.snapshot.frontier,
            txn,
            id,
            Arc::clone(&self.handle_owner),
        )
        .map_err(WriteError::SnapshotPin)
    }

    /// Prepare one immutable canonical template and retain the compact
    /// dependencies that its evaluation observed, including conditional no-ops.
    /// This performs no publication and never silently rebases an older write.
    pub fn prepare_write(&mut self, batch: WriteBatch) -> Result<PreparedWrite, WriteError> {
        self.prepare_write_checked(batch)
    }

    /// Commit exactly the template prepared at its original basis. Validation
    /// reconstructs the complete intervening committed history for this write;
    /// unrelated ordinary writes cannot erase its conflict memory. A retired
    /// history prefix is a typed refusal, not permission to commit stale data.
    pub async fn commit_prepared(
        &mut self,
        cx: &CommitCx,
        prepared: PreparedWrite,
    ) -> Result<CommitSeq, WriteError> {
        self.commit_prepared_with_crash(cx, prepared, None).await
    }

    /// The same prepared-write validation and publication path with an optional
    /// production crash point. Foreign-owner and missing-history refusals occur
    /// before installing a validator or changing the handle's durability state.
    #[doc(hidden)]
    pub async fn commit_prepared_with_crash(
        &mut self,
        cx: &CommitCx,
        prepared: PreparedWrite,
        crash_at: Option<CrashPoint>,
    ) -> Result<CommitSeq, WriteError> {
        self.commit_prepared_checked(cx, prepared, crash_at).await
    }

    /// Commit a batch, optionally stopping the durable protocol at `crash_at`.
    ///
    /// **Public rather than test-gated, and the reason is the crash-point
    /// matrix** (§15). `fgdb-chronicle::CommitCoordinator::commit_with_crash` and
    /// `fgdb-strata::BlockStore::put_with_crash` are public for the same reason:
    /// the crash path must be the SAME code as the durable path up to the
    /// stopping instant, and a `#[cfg(test)]` twin would be a second
    /// implementation that no longer says anything about the real protocol.
    ///
    /// A crash point returns `Err` and does NOT republish the derived partition —
    /// which is exactly right, because the process this models is not around to
    /// republish anything. Drop the `Database` and reopen to see what survived.
    pub async fn write_with_crash(
        &mut self,
        cx: &CommitCx,
        batch: WriteBatch,
        crash_at: Option<CrashPoint>,
    ) -> Result<CommitSeq, WriteError> {
        self.write_with_faults(cx, batch, crash_at, None, None)
            .await
    }

    /// Commit through Chronicle D2, then stop at one exact derived-publication
    /// stage with the same fenced-handle result a real failure produces.
    ///
    /// This is a verification surface for the §15 fault matrix, not a cheaper
    /// write path. It executes the ordinary durable commit and ordinary
    /// publication code up to `fail_at`; the injected error is checked only
    /// after D2 and after the handle records that exact recovery stage. Drop the
    /// fenced handle and reopen to recover the committed graph from Chronicle.
    pub async fn write_with_publication_failure(
        &mut self,
        cx: &CommitCx,
        batch: WriteBatch,
        fail_at: DerivedPublicationStage,
    ) -> Result<CommitSeq, WriteError> {
        self.write_with_faults(cx, batch, None, Some(fail_at), None)
            .await
    }

    /// Commit through Chronicle D2, then drive the first newly published
    /// Strata block through one real block-store crash instant.
    ///
    /// The ordinary receipt-earning publisher remains in the path: this is the
    /// integrated §15 crash matrix seam, not a test-only storage substitute.
    /// A batch that emits no new edge block cannot reach a block publication
    /// crash point and therefore completes normally; matrix callers must assert
    /// the expected committed-needs-recovery result.
    pub async fn write_with_block_store_crash(
        &mut self,
        cx: &CommitCx,
        batch: WriteBatch,
        crash_at: BlockStoreCrashPoint,
    ) -> Result<CommitSeq, WriteError> {
        self.write_with_faults(cx, batch, None, None, Some(crash_at))
            .await
    }

    async fn write_with_faults(
        &mut self,
        cx: &CommitCx,
        batch: WriteBatch,
        crash_at: Option<CrashPoint>,
        publication_failure: Option<DerivedPublicationStage>,
        block_store_crash_at: Option<BlockStoreCrashPoint>,
    ) -> Result<CommitSeq, WriteError> {
        self.ensure_writable()?;
        let template = self.build_write_template(batch)?;
        // An immediate write was prepared under this same exclusive borrow at
        // the current frontier. Prepared writes instead reconstruct their own
        // committed suffix in prepared_write before choosing the validator.
        self.coordinator.set_validator(Box::new(
            FirstCommitterWinsValidator::default()
                .with_scalar_resolver(self.keys.scalar_resolver.clone()),
        ));
        self.commit_template(
            cx,
            template,
            crash_at,
            publication_failure,
            block_store_crash_at,
        )
        .await
    }

    /// Derive the canonical logical-delta template for `batch` against the
    /// live fold: every preflight refusal (`AlreadyLive`, `IdentitySpent`,
    /// compare-and-set mismatch) and every before-image comes from the
    /// CURRENT snapshot plus the batch prefix. Shared verbatim by
    /// [`Database::write`], which commits it immediately, and
    /// [`Database::prepare_write`], which retains it so a second batch can be
    /// built against this same basis before the first commits.
    fn build_write_template(&self, batch: WriteBatch) -> Result<LogicalDeltaTemplate, WriteError> {
        if batch.is_empty() {
            return Err(WriteError::EmptyBatch);
        }
        // Validate before normalization/ensure can erase an unsupported input.
        // Validate against exactly the artifacts retained by recovery.
        let admit = |value: &CanonicalScalar| -> Result<(), WriteError> {
            if let CanonicalScalar::Timestamp(timestamp) = value
                && let Some(zone) = timestamp.zone()
            {
                let Some(resolver) = self.keys.scalar_resolver.as_deref() else {
                    return Err(WriteError::ZonedTimestampRequiresResolver {
                        tzdb_oid: zone.tzdb_oid(),
                    });
                };
                timestamp.validate_tzdb_binding(resolver).map_err(|_| {
                    WriteError::ZonedTimestampRequiresResolver {
                        tzdb_oid: zone.tzdb_oid(),
                    }
                })?;
            }
            Ok(())
        };
        for pending in &batch.rows {
            match pending {
                PendingRow::Vertex { props, .. } | PendingRow::Edge { props, .. } => {
                    for (_, value) in props {
                        admit(value)?;
                    }
                }
                PendingRow::SetProperty { value, .. }
                | PendingRow::SetEdgeProperty { value, .. } => {
                    if let Some(value) = value {
                        admit(value)?;
                    }
                }
                PendingRow::CompareAndSet {
                    expected, value, ..
                } => {
                    if let Some(expected) = expected {
                        admit(expected)?;
                    }
                    admit(value)?;
                }
                PendingRow::DeleteEdge { .. }
                | PendingRow::DeleteVertex { .. }
                | PendingRow::SetLabel { .. } => {}
            }
        }

        // Build durable rows SEQUENTIALLY, deriving every delete's
        // before-image from the fold's live state PLUS the batch prefix: a
        // create-then-delete in one batch must image the version the create
        // just minted, exactly as the oracle will re-derive it at replay.
        let mut rows = Vec::with_capacity(batch.rows.len());
        // The batch-prefix overlay: identities this batch created or deleted
        // ahead of the row being built, with the versions the prefix minted.
        let mut prefix_versions: std::collections::BTreeMap<ElementId, ObjectId> =
            std::collections::BTreeMap::new();
        let mut prefix_edges: std::collections::BTreeMap<EId, (VId, VId)> =
            std::collections::BTreeMap::new();
        // Edge CONTENT for update targets, seeded lazily from the live
        // statement's row so an update's before-image reflects the batch
        // prefix — the edge half of `prefix_content`.
        let mut prefix_edge_rows: std::collections::BTreeMap<EId, EdgePropertyRow> =
            std::collections::BTreeMap::new();
        let mut prefix_deleted_edges: std::collections::BTreeSet<EId> =
            std::collections::BTreeSet::new();
        let mut prefix_deleted_vertices: std::collections::BTreeSet<VId> =
            std::collections::BTreeSet::new();
        // Vertex CONTENT for update targets, seeded lazily from the merged
        // committed row so an update's before-image reflects the batch
        // prefix: (labels, props), both kept in canonical order.
        let mut prefix_content: std::collections::BTreeMap<VId, VertexContent> =
            std::collections::BTreeMap::new();
        // BirthOrdinal is the 1-based visit index in this batch, including
        // no-ops — not graph cardinality (fgdb-4cyg / spa1).
        for (visit, pending) in batch.rows.into_iter().enumerate() {
            let intent_ordinal = u64::try_from(visit)
                .ok()
                .and_then(|visit| visit.checked_add(1))
                .expect("a batch cannot contain 2^64 rows");
            let row = match pending {
                PendingRow::Vertex {
                    vid,
                    labels,
                    props,
                    ensure,
                } => {
                    let live_now = !prefix_deleted_vertices.contains(&vid)
                        && (prefix_versions.contains_key(&ElementId::Vertex(vid))
                            || self.writer.is_vertex_live(vid));
                    if ensure && live_now {
                        continue;
                    }
                    // Same-batch delete spends the id. prefix_versions is not
                    // cleared, so this must beat AlreadyLive (fgdb-yuvu).
                    // Historical spent is checked after AlreadyLive: the
                    // writer's spent set includes every admitted (still-live)
                    // identity.
                    if prefix_deleted_vertices.contains(&vid) {
                        return Err(WriteError::IdentitySpent {
                            elem: ElementId::Vertex(vid),
                        });
                    }
                    // The fold's refusals, preflighted (fgdb-kokz): a create
                    // that the writer would refuse AFTER the two-fsync commit
                    // must refuse before it. Ensure is not resurrection.
                    if !ensure
                        && (prefix_versions.contains_key(&ElementId::Vertex(vid))
                            || self.writer.is_vertex_live(vid))
                    {
                        return Err(WriteError::AlreadyLive {
                            elem: ElementId::Vertex(vid),
                        });
                    }
                    if self.writer.is_vertex_spent(vid) {
                        return Err(WriteError::IdentitySpent {
                            elem: ElementId::Vertex(vid),
                        });
                    }
                    let mut labels = labels;
                    let mut props = props;
                    sort_write_labels_and_props(&mut labels, &mut props);
                    let row = DeltaRow::CreateVertex {
                        vid,
                        birth_ordinal: intent_ordinal,
                        labels,
                        props,
                        valid_time: None,
                    };
                    if let DeltaRow::CreateVertex {
                        birth_ordinal: ordinal,
                        labels,
                        props,
                        ..
                    } = &row
                    {
                        let transcript = vertex_statement_transcript(vid, *ordinal, labels, props)?;
                        prefix_versions.insert(
                            ElementId::Vertex(vid),
                            statement_successor(None, &transcript),
                        );
                        prefix_content.insert(vid, (labels.clone(), props.clone()));
                    }
                    row
                }
                PendingRow::Edge {
                    eid,
                    src,
                    dst,
                    props,
                    ensure,
                } => {
                    // Parallel edges are legal: only ENSURE observes whether
                    // the triple already exists. Plain insertion checks EId
                    // uniqueness below, without scanning committed edges.
                    if ensure
                        && triple_is_live(
                            &self.writer,
                            &prefix_edges,
                            &prefix_deleted_edges,
                            src,
                            dst,
                            batch.relation,
                        )
                    {
                        continue;
                    }
                    if prefix_deleted_edges.contains(&eid) {
                        return Err(WriteError::IdentitySpent {
                            elem: ElementId::Edge(eid),
                        });
                    }
                    if prefix_edges.contains_key(&eid) || self.writer.live_edge(eid).is_some() {
                        return Err(WriteError::AlreadyLive {
                            elem: ElementId::Edge(eid),
                        });
                    }
                    if self.writer.is_edge_spent(eid) {
                        return Err(WriteError::IdentitySpent {
                            elem: ElementId::Edge(eid),
                        });
                    }
                    // Referential integrity BEFORE D2 (fgdb-r196). The
                    // oracle refuses `DanglingEndpoint` at apply; a
                    // durable row it cannot replay poisons the spine.
                    for endpoint in [src, dst] {
                        let live_now = !prefix_deleted_vertices.contains(&endpoint)
                            && (prefix_versions.contains_key(&ElementId::Vertex(endpoint))
                                || prefix_content.contains_key(&endpoint)
                                || self.writer.is_vertex_live(endpoint));
                        if !live_now {
                            return Err(WriteError::DanglingEndpoint { eid, endpoint });
                        }
                    }
                    let mut props = props;
                    sort_write_props(&mut props);
                    let row = DeltaRow::CreateEdge {
                        eid,
                        birth_ordinal: intent_ordinal,
                        src,
                        relation: batch.relation,
                        dst,
                        canonical_key: None,
                        props,
                        valid_time: None,
                    };
                    prefix_edges.insert(eid, (src, dst));
                    if let DeltaRow::CreateEdge { props, .. } = &row {
                        let transcript =
                            edge_statement_transcript(eid, src, batch.relation, dst, props)?;
                        prefix_versions
                            .insert(ElementId::Edge(eid), statement_successor(None, &transcript));
                        prefix_edge_rows.insert(eid, props.clone());
                    }
                    row
                }
                PendingRow::DeleteEdge { eid, if_present } => {
                    let live_now = !prefix_deleted_edges.contains(&eid)
                        && (prefix_edges.contains_key(&eid)
                            || self.writer.live_edge(eid).is_some());
                    if !live_now {
                        // Already retired in this batch (cascade or an
                        // earlier DeleteEdge): Nothing, like the reference
                        // and like fold's cascade_owned ignore (fgdb-v31u).
                        if if_present || prefix_deleted_edges.contains(&eid) {
                            continue;
                        }
                        return Err(WriteError::UnknownEdge { eid });
                    }
                    let before_version = prefix_versions
                        .get(&ElementId::Edge(eid))
                        .or_else(|| self.heads.versions.get(&ElementId::Edge(eid)))
                        .copied()
                        .expect("a live edge always has a version chain head");
                    prefix_deleted_edges.insert(eid);
                    DeltaRow::DeleteEdge {
                        eid,
                        before_version,
                    }
                }
                PendingRow::DeleteVertex { vid, if_present } => {
                    let live_now = !prefix_deleted_vertices.contains(&vid)
                        && (prefix_versions.contains_key(&ElementId::Vertex(vid))
                            || self.writer.is_vertex_live(vid));
                    if !live_now {
                        if if_present || prefix_deleted_vertices.contains(&vid) {
                            continue;
                        }
                        return Err(WriteError::UnknownVertex { vid });
                    }
                    let before_version = prefix_versions
                        .get(&ElementId::Vertex(vid))
                        .or_else(|| self.heads.versions.get(&ElementId::Vertex(vid)))
                        .copied()
                        .expect("a live vertex always has a version chain head");
                    // The cascade image is the incident set the FOLD will
                    // see when it applies this DeleteVertex. NENF emits
                    // vertices before edges (fgdb-17ht), so a prefix
                    // DeleteEdge of an incident eid is absorbed into this
                    // cascade rather than stripped from it — stripping
                    // produced an empty image that applied while the edge
                    // was still live.
                    let mut cascade: std::collections::BTreeSet<EId> =
                        self.writer.live_incident_edges(vid).into_iter().collect();
                    for (eid, (src, dst)) in &prefix_edges {
                        if *src == vid || *dst == vid {
                            cascade.insert(*eid);
                        }
                    }
                    for eid in &cascade {
                        prefix_deleted_edges.insert(*eid);
                    }
                    prefix_deleted_vertices.insert(vid);
                    prefix_content.remove(&vid);
                    DeltaRow::DeleteVertex {
                        vid,
                        before_version,
                        sorted_retired_incident_edges: cascade.into_iter().collect(),
                    }
                }
                PendingRow::SetLabel { vid, label, member } => {
                    let live_now = !prefix_deleted_vertices.contains(&vid)
                        && (prefix_content.contains_key(&vid)
                            || prefix_versions.contains_key(&ElementId::Vertex(vid))
                            || self.writer.is_vertex_live(vid));
                    if !live_now {
                        // Not a member of a gone vertex is already true.
                        if !member {
                            continue;
                        }
                        return Err(WriteError::UnknownVertex { vid });
                    }
                    let (labels, _) =
                        vertex_content_entry(&mut prefix_content, &self.snapshot, vid);
                    let before = labels.binary_search(&label).is_ok();
                    let row = DeltaRow::LabelMembership {
                        vid,
                        label,
                        before,
                        after: member,
                    };
                    match labels.binary_search(&label) {
                        Ok(at) => {
                            if !member {
                                labels.remove(at);
                            }
                        }
                        Err(at) => {
                            if member {
                                labels.insert(at, label);
                            }
                        }
                    }
                    // v3 (fgdb-ge6a): updates do not advance the batch's
                    // version overlay — the chain steps once per COMMIT over
                    // the durable statement. Delete-after-update is folded to
                    // a single delete against this durable head.
                    row
                }
                PendingRow::SetEdgeProperty { eid, key, value } => {
                    let live_now = !prefix_deleted_edges.contains(&eid)
                        && (prefix_edges.contains_key(&eid)
                            || self.writer.live_edge(eid).is_some());
                    if !live_now {
                        // RemoveProp of an already-absent property is Nothing,
                        // including when the edge itself is gone (fgdb-vsgw).
                        if value.is_none() {
                            continue;
                        }
                        return Err(WriteError::UnknownEdge { eid });
                    }
                    let props = prefix_edge_rows
                        .entry(eid)
                        .or_insert_with(|| self.writer.live_edge_row(eid).unwrap_or_default());
                    let position = props.binary_search_by_key(&key, |(k, _)| *k);
                    let before = position.ok().map(|at| props[at].1.clone());
                    let row = DeltaRow::Property {
                        elem: ElementId::Edge(eid),
                        property: key,
                        before,
                        after: value.clone(),
                    };
                    match position {
                        Ok(at) => match value {
                            Some(value) => props[at].1 = value,
                            None => {
                                props.remove(at);
                            }
                        },
                        Err(at) => {
                            if let Some(value) = value {
                                props.insert(at, (key, value));
                            }
                        }
                    }
                    // v3 (fgdb-ge6a): updates do not advance the batch's
                    // version overlay — the chain steps once per COMMIT over
                    // the durable statement. Delete-after-update is folded to
                    // a single delete against this durable head.
                    row
                }
                PendingRow::SetProperty { vid, key, value } => {
                    let live_now = !prefix_deleted_vertices.contains(&vid)
                        && (prefix_content.contains_key(&vid)
                            || prefix_versions.contains_key(&ElementId::Vertex(vid))
                            || self.writer.is_vertex_live(vid));
                    if !live_now {
                        if value.is_none() {
                            continue;
                        }
                        return Err(WriteError::UnknownVertex { vid });
                    }
                    let (_, props) = vertex_content_entry(&mut prefix_content, &self.snapshot, vid);
                    let position = props.binary_search_by_key(&key, |(k, _)| *k);
                    let before = position.ok().map(|at| props[at].1.clone());
                    let row = DeltaRow::Property {
                        elem: ElementId::Vertex(vid),
                        property: key,
                        before,
                        after: value.clone(),
                    };
                    match position {
                        Ok(at) => match value {
                            Some(value) => props[at].1 = value,
                            None => {
                                props.remove(at);
                            }
                        },
                        Err(at) => {
                            if let Some(value) = value {
                                props.insert(at, (key, value));
                            }
                        }
                    }
                    // v3 (fgdb-ge6a): updates do not advance the batch's
                    // version overlay — the chain steps once per COMMIT over
                    // the durable statement. Delete-after-update is folded to
                    // a single delete against this durable head.
                    row
                }
                PendingRow::CompareAndSet {
                    elem,
                    key,
                    expected,
                    value,
                    mismatch,
                } => {
                    let live_now = match elem {
                        ElementId::Vertex(vid) => {
                            !prefix_deleted_vertices.contains(&vid)
                                && (prefix_content.contains_key(&vid)
                                    || prefix_versions.contains_key(&ElementId::Vertex(vid))
                                    || self.writer.is_vertex_live(vid))
                        }
                        ElementId::Edge(eid) => {
                            !prefix_deleted_edges.contains(&eid)
                                && (prefix_edges.contains_key(&eid)
                                    || self.writer.live_edge(eid).is_some())
                        }
                    };
                    if !live_now {
                        // Reference property_of on a gone element is None.
                        // expected=Some is a mismatch: NoOp is Nothing
                        // (fgdb-q6zj). expected=None matches, then set-Some
                        // on a gone element is Unknown* — NoOp is not a
                        // license to invent the element (fgdb-pmj7).
                        // AbortWrite still names the missing id.
                        if expected.is_some() && matches!(mismatch, WriteMismatchPolicy::NoOp) {
                            continue;
                        }
                        return Err(match elem {
                            ElementId::Vertex(vid) => WriteError::UnknownVertex { vid },
                            ElementId::Edge(eid) => WriteError::UnknownEdge { eid },
                        });
                    }
                    let actual = match elem {
                        ElementId::Vertex(vid) => {
                            let (_, props) =
                                vertex_content_entry(&mut prefix_content, &self.snapshot, vid);
                            let position = props.binary_search_by_key(&key, |(k, _)| *k);
                            position.ok().map(|at| props[at].1.clone())
                        }
                        ElementId::Edge(eid) => {
                            let props = prefix_edge_rows.entry(eid).or_insert_with(|| {
                                self.writer.live_edge_row(eid).unwrap_or_default()
                            });
                            let position = props.binary_search_by_key(&key, |(k, _)| *k);
                            position.ok().map(|at| props[at].1.clone())
                        }
                    };
                    if actual.as_ref() != expected.as_deref() {
                        match mismatch {
                            WriteMismatchPolicy::NoOp => continue,
                            WriteMismatchPolicy::AbortWrite => {
                                return Err(WriteError::CompareAndSetMismatch(Box::new(
                                    CompareAndSetMismatch {
                                        elem,
                                        name: key,
                                        expected: expected.map(|v| *v),
                                        actual,
                                    },
                                )));
                            }
                        }
                    }
                    if actual.as_ref() == Some(value.as_ref()) {
                        continue;
                    }
                    let after = (*value).clone();
                    let row = DeltaRow::Property {
                        elem,
                        property: key,
                        before: actual,
                        after: Some(after.clone()),
                    };
                    match elem {
                        ElementId::Vertex(vid) => {
                            let (_, props) =
                                vertex_content_entry(&mut prefix_content, &self.snapshot, vid);
                            match props.binary_search_by_key(&key, |(k, _)| *k) {
                                Ok(at) => props[at].1 = after,
                                Err(at) => props.insert(at, (key, after)),
                            }
                        }
                        ElementId::Edge(eid) => {
                            let props = prefix_edge_rows.entry(eid).or_insert_with(|| {
                                self.writer.live_edge_row(eid).unwrap_or_default()
                            });
                            match props.binary_search_by_key(&key, |(k, _)| *k) {
                                Ok(at) => props[at].1 = after,
                                Err(at) => props.insert(at, (key, after)),
                            }
                        }
                    }
                    row
                }
            };
            rows.push(row);
        }

        // Fold evaluation-order rows to a target-disjoint net before the
        // template byte-sorts them (fgdb-w5-effects-normal-form-819.2).
        // Two sets on one field, set-then-delete, and create-then-delete
        // become one row or none; byte order is then applicability-safe.
        // Shared DeleteVertex cascade eids are kept on the smallest VId
        // inside the fold (fgdb-s9ja).
        let rows = fold_target_disjoint(rows);

        let template = LogicalDeltaTemplate::build(
            intent_semantics_oid(),
            [0u8; 32],
            vec![CoordinateEntry {
                graph: GRAPH,
                branch: BRANCH,
                relation: batch.relation,
                schema_epoch: SchemaEpoch(0),
                schema_transition: None,
                rows,
            }],
        )?;
        // Admit final after-images, not transient evaluation-order values:
        // set-then-remove and create-then-delete need no oversized durable
        // row. Preparation is shared by ordinary, prepared and transaction
        // writes. FCW protects these after-images across intervening commits;
        // fixed-width lifetime fields cannot change their encoded size.
        for (vid, (labels, props)) in &prefix_content {
            fgdb_strata::vertex::admit_row_content(labels, props).map_err(|source| {
                WriteError::VertexStorageAdmission {
                    vid: *vid,
                    source: Box::new(source),
                }
            })?;
        }
        for (eid, props) in &prefix_edge_rows {
            if !prefix_deleted_edges.contains(eid) {
                fgdb_strata::edge_props::admitted_row_bytes(props).map_err(|source| {
                    WriteError::EdgeStorageAdmission {
                        eid: *eid,
                        source: Box::new(source),
                    }
                })?;
            }
        }
        Ok(template)
    }

    /// The durable tail every commit path shares: seal the capsule, run the
    /// two-fsync protocol through whichever commit validator the caller just
    /// installed, fold the template into the retained writer, and publish the
    /// derived Tier-D objects. Which validator instance judges the draft is
    /// the CALLER's statement about the template's basis — see
    /// `write_with_faults` and [`Database::commit_prepared`].
    ///
    /// The future is type-erased here because every commit path crosses this
    /// one boundary. A caller's `Send` proof then stops at `dyn Future + Send`
    /// instead of descending through Chronicle and Strata, which overflowed
    /// rustc's default recursion limit in every lab test root (fgdb-a5y6m).
    fn commit_template<'a>(
        &'a mut self,
        cx: &'a CommitCx,
        template: LogicalDeltaTemplate,
        crash_at: Option<CrashPoint>,
        publication_failure: Option<DerivedPublicationStage>,
        block_store_crash_at: Option<BlockStoreCrashPoint>,
    ) -> SendFuture<'a, Result<CommitSeq, WriteError>> {
        Box::pin(self.commit_template_inner(
            cx,
            template,
            crash_at,
            publication_failure,
            block_store_crash_at,
        ))
    }

    async fn commit_template_inner(
        &mut self,
        cx: &CommitCx,
        template: LogicalDeltaTemplate,
        crash_at: Option<CrashPoint>,
        publication_failure: Option<DerivedPublicationStage>,
        mut block_store_crash_at: Option<BlockStoreCrashPoint>,
    ) -> Result<CommitSeq, WriteError> {
        // Slot exhaustion is a publication impossibility, not a post-commit
        // recovery condition. Refuse before Chronicle receives the capsule so
        // D2 cannot make a commit whose derived root has no selectable name.
        let next_generation = next_slot_generation(self.slot_generation)?;
        // The same for the root's reference ceiling: a commit whose fold
        // could not be encoded would be durable but never publishable, and
        // every rebuild would refuse it again (fgdb-a5y6m probe).
        admit_root_capacity(
            self.writer.sealed().len(),
            self.writer.sealed_patches().len(),
            &template,
            fgdb_strata::root::MAX_ROOT_BLOCKS as usize,
            fgdb_strata::root::MAX_ROOT_PATCHES as usize,
        )
        .map_err(|error| WriteError::RootCapacity(Box::new(error)))?;
        let capsule = prepare_capsule(self.keys.k_oid(), self.keys.namespace, &template)?;
        self.retain_preparation_anchor();
        let published_frontier = self.snapshot.frontier;
        // `commit_with_crash` is cancellable at every VFS await. Chronicle
        // poisons its own coordinator immediately before marker append, but a
        // dropped OUTER future never executes the error arm below that copies
        // the poison into this public handle. Fence before the first await;
        // only an explicit error from an unpoisoned coordinator proves that it
        // is safe to restore the old published generation as Healthy.
        self.state = DatabaseState::CommitOutcomeUnknown { published_frontier };
        let marker_ref = match self
            .coordinator
            .commit_with_crash(
                cx,
                &capsule.bytes,
                |seq, oid| marker_for_capsule(seq, oid, &capsule, Vec::new()),
                crash_at,
            )
            .await
        {
            Ok(marker_ref) => marker_ref,
            Err(source) if self.coordinator.is_poisoned() => {
                return Err(WriteError::CommitOutcomeUnknown {
                    published_frontier,
                    source: Box::new(source),
                });
            }
            Err(source) => {
                // A refusal from an unpoisoned coordinator left nothing
                // durable and freed its sequence, so the handle stays
                // Healthy. A first-committer-wins verdict additionally gets
                // its own arm: the caller repairs by REBUILDING the batch
                // against the advanced snapshot, which is a different remedy
                // than any transport or protocol failure in `Commit`.
                self.state = DatabaseState::Healthy { published_frontier };
                return Err(match source {
                    CommitError::Rejected(rejection)
                        if rejection.law == FCW_LAW_ID || rejection.law == FCW_READ_LAW_ID =>
                    {
                        WriteError::FirstCommitterWins {
                            law: rejection.law,
                            detail: rejection.detail,
                        }
                    }
                    other => WriteError::Commit(Box::new(other)),
                });
            }
        };

        // Incremental snapshot maintenance (fgdb-fujt): the template in hand IS
        // the delta the durable commit just appended, so fold exactly it into
        // the retained writer instead of re-reading every historical capsule —
        // the loop ffe05f6 measured at 95% of an O(history) marginal write.
        // FG-INV-09's recompute-from-registered-bytes check is a re-READ law
        // and still runs on every path that reads capsules back (open,
        // recovery, the replica probe); this path never re-reads, it folds the
        // bytes it just made durable. The retained writer is MOVED into the
        // fold, not cloned (a clone copies every sealed block and live row, so
        // it grew with history): the handle is fenced below before the first
        // fold operation, every reader of `self.writer` requires a Healthy
        // handle, and recovery reopens from the stream, so a failure never
        // exposes the partially folded writer.
        let frontier = marker_ref.commit_seq;
        let mut recovery = RecoveryRequired {
            durable_frontier: frontier,
            published_frontier,
            failed_stage: DerivedPublicationStage::FoldCommittedTemplate,
        };
        // D2 has completed. From this assignment until the final snapshot
        // swap, every early return leaves the handle fenced off from its stale
        // writer and snapshot. The assignment intentionally precedes even the
        // first in-memory fold operation: a panic caught by an outer boundary
        // is no excuse to make the old handle callable again.
        self.state = DatabaseState::NeedsAuthoritativeRecovery(recovery);
        // Plan:397 — insert the batch and advance the frontier in the same
        // write that applied the durable commit. The index is derived: a
        // refusal here is CommittedNeedsRecovery, not an uncommitted write,
        // and a crash before this line heals on reopen from the chain.
        let batch = LogicalDeltaBatch::order(
            &template,
            capsule.template_digest.0,
            CommittedMarker::attest(marker_ref, cx),
        );
        // The delta index and statement versions are TAKEN from the fenced
        // handle, not cloned (a clone copied every retained batch and version
        // on every commit). The handle has been fenced since before this fold,
        // and `make_mut` copies only a snapshot a pinned read view still
        // shares, so that view keeps its own unchanged generation. The
        // versions are the handle's own, so a pinned view never copies them.
        let previous = Arc::make_mut(&mut self.snapshot);
        let mut next_delta_index = std::mem::take(&mut previous.delta_index);
        let mut new_versions = std::mem::take(&mut self.heads.versions);
        next_delta_index
            .insert(batch)
            .map_err(|error| WriteError::CommittedNeedsRecovery {
                recovery,
                source: Box::new(RebuildError::Index {
                    commit_seq: frontier.0,
                    error,
                }),
            })?;
        Self::fail_publication_if_requested(recovery, publication_failure)?;
        let mut folded =
            std::mem::replace(&mut self.writer, BlockWriter::new(GRAPH, BRANCH, PARTITION));
        let mut next_birth_ordinal = self.heads.next_birth_ordinal;
        let mut touched: std::collections::BTreeSet<ElementId> = std::collections::BTreeSet::new();
        for coordinate in template.coordinate_entries() {
            if (coordinate.graph, coordinate.branch) != (GRAPH, BRANCH) {
                continue;
            }
            for row in &coordinate.rows {
                if matches!(
                    row,
                    DeltaRow::CreateVertex { .. } | DeltaRow::CreateEdge { .. }
                ) {
                    next_birth_ordinal += 1;
                }
                folded
                    .apply(self.keys.block_keys(), frontier, row)
                    .map_err(|error| WriteError::CommittedNeedsRecovery {
                        recovery,
                        source: Box::new(RebuildError::Fold {
                            commit_seq: frontier.0,
                            error,
                        }),
                    })?;
                touched_elements(row, &mut touched);
            }
        }
        fold_statement_versions(&mut new_versions, &touched, &folded).map_err(|error| {
            WriteError::CommittedNeedsRecovery {
                recovery,
                source: Box::new(RebuildError::Version {
                    commit_seq: frontier.0,
                    error,
                }),
            }
        })?;
        self.mark_recovery_stage(&mut recovery, DerivedPublicationStage::SealPartition);
        Self::fail_publication_if_requested(recovery, publication_failure)?;
        // The per-commit seal law — see `fold_stream`'s twin comment: the
        // RETAINED fold seals this commit's statements now, so the durable
        // layout never depends on which writer held unsealed rows.
        folded.seal(self.keys.block_keys()).map_err(|error| {
            WriteError::CommittedNeedsRecovery {
                recovery,
                source: Box::new(RebuildError::Fold {
                    commit_seq: frontier.0,
                    error,
                }),
            }
        })?;
        folded
            .seal_vertices(self.keys.block_keys())
            .map_err(|error| WriteError::CommittedNeedsRecovery {
                recovery,
                source: Box::new(RebuildError::Fold {
                    commit_seq: frontier.0,
                    error,
                }),
            })?;
        let root = folded
            .publish_in_place(self.keys.block_keys(), frontier)
            .map_err(|error| WriteError::CommittedNeedsRecovery {
                recovery,
                source: Box::new(RebuildError::Fold {
                    commit_seq: frontier.0,
                    error,
                }),
            })?;
        let (blocks, patches) = (folded.sealed(), folded.sealed_patches());
        // The immutable prefix remains memoized. New block, hosted-property,
        // and vertex objects share publication authority and one directory
        // barrier; no receipt escapes before the entire data batch completes.
        // An object these receipts already hold was verified and admitted
        // under its identity, so it is not re-hashed here; every other sealed
        // object, including any an earlier publish left behind, still takes
        // the full verified path.
        // References inside the root prefix these receipts already verified
        // hold receipts by construction (the retained writer's sealed objects
        // extend the last published root in order), so only the rest is probed.
        let (block_prefix, patch_prefix) = self.receipts.verified_root_prefix(PARTITION);
        let unpublished_blocks: Vec<&fgdb_strata::writer::SealedBlock> = blocks
            [block_prefix.min(blocks.len())..]
            .iter()
            .filter(|block| {
                !self
                    .receipts
                    .holds(fgdb_strata::DeltaBlockVersion(block.block_id))
            })
            .collect();
        let unpublished_patches: Vec<&fgdb_strata::writer::SealedPatch> = patches
            [patch_prefix.min(patches.len())..]
            .iter()
            .filter(|patch| {
                !self
                    .receipts
                    .holds_patch(fgdb_strata::vertex::VertexPatchVersion(patch.patch_id))
            })
            .collect();
        self.mark_recovery_stage(&mut recovery, DerivedPublicationStage::PublishEdgeBlocks);
        Self::fail_publication_if_requested(recovery, publication_failure)?;
        let mut publication = self
            .store
            .publication_batch(cx, &mut self.receipts, block_store_crash_at.take())
            .map_err(|error| WriteError::CommittedNeedsRecovery {
                recovery,
                source: Box::new(RebuildError::from(error)),
            })?;
        for block in unpublished_blocks {
            publication
                .put_verified(
                    cx,
                    &block.bytes,
                    block
                        .property_patch
                        .as_ref()
                        .map(|patch| patch.bytes.as_slice()),
                )
                .await
                .map_err(|error| WriteError::CommittedNeedsRecovery {
                    recovery,
                    source: Box::new(RebuildError::from(error)),
                })?;
        }
        // The edge blocks and their property patches become canonical inside
        // this stage, their inode syncs in flight together.
        publication
            .flush(cx)
            .await
            .map_err(|error| WriteError::CommittedNeedsRecovery {
                recovery,
                source: Box::new(RebuildError::from(error)),
            })?;
        recovery.failed_stage = DerivedPublicationStage::PublishVertexPatches;
        self.state = DatabaseState::NeedsAuthoritativeRecovery(recovery);
        Self::fail_publication_if_requested(recovery, publication_failure)?;
        for patch in unpublished_patches {
            publication
                .put_patch_verified(cx, &patch.bytes)
                .await
                .map_err(|error| WriteError::CommittedNeedsRecovery {
                    recovery,
                    source: Box::new(RebuildError::from(error)),
                })?;
        }
        publication
            .finish(cx)
            .await
            .map_err(|error| WriteError::CommittedNeedsRecovery {
                recovery,
                source: Box::new(RebuildError::from(error)),
            })?;
        self.mark_recovery_stage(&mut recovery, DerivedPublicationStage::PublishPartitionRoot);
        Self::fail_publication_if_requested(recovery, publication_failure)?;
        // Every root-scope law runs here. The root itself is written below,
        // together with the manifest that names it (fgdb-90i03).
        let verified_root = self
            .store
            .verify_root(cx, &root, &mut self.receipts)
            .await
            .map_err(|error| WriteError::CommittedNeedsRecovery {
                recovery,
                source: Box::new(RebuildError::from(error)),
            })?;
        let root_id = verified_root.id();
        // The manifest names the published root (fgdb-63w2) and binds it to
        // the chain commitment at this very commit (fgdb-90hw): the durable
        // path from this directory to its partition advances in the same
        // publish, carrying the authority claim open verifies.
        let published_chain_hash = chain_commitment_at(self.coordinator.chain(), root.published_at)
            .expect("the commit that published this root is on its own chain");
        let manifest_records = records_of(&[(root.clone(), root_id, published_chain_hash)])
            .expect("one root is one canonical record");
        self.mark_recovery_stage(&mut recovery, DerivedPublicationStage::PublishManifest);
        Self::fail_publication_if_requested(recovery, publication_failure)?;
        // Root and manifest share one concurrent inode sync, each one's
        // durable read-back, and one directory barrier. Neither is reachable
        // until the slot below names the manifest.
        let (manifest, manifest_len) = self
            .store
            .publish_root_and_manifest(cx, verified_root, &manifest_records, &mut self.receipts)
            .await
            .map_err(|error| WriteError::CommittedNeedsRecovery {
                recovery,
                source: Box::new(RebuildError::from(error)),
            })?;
        let manifest_len = manifest_len as u64;
        // The slot advances in the same publish (fgdb-ge6a): a crash before
        // this line leaves the slot exactly one publication behind, which is
        // the shape open() heals; there is no window where it runs ahead.
        self.mark_recovery_stage(&mut recovery, DerivedPublicationStage::PublishRootSlot);
        Self::fail_publication_if_requested(recovery, publication_failure)?;
        self.slot_store
            .publish_evidenced(
                cx,
                &spine_slot(&self.keys, next_generation, manifest, manifest_len),
            )
            .await
            .map_err(|error| WriteError::CommittedNeedsRecovery {
                recovery,
                source: Box::new(RebuildError::Slot(error)),
            })?;
        self.slot_generation = next_generation;

        // Refresh the snapshot without re-reading the partition: carry forward
        // the decoded blocks whose references are unchanged, and decode the new
        // ones from the exact bytes `put_verified` just content-addressed and
        // fsynced. The encode→address→fsync→decode round trip rebuild's doc
        // demands still happens — over the in-memory bytes the disk now holds —
        // and `incremental_snapshot.rs` pins that a from-scratch reopen derives
        // this same root and adjacency. The handle stays fenced until the swap.
        //
        // The retained writer carries every sealed object as an unchanged
        // prefix. Replacement writers (compaction and recovery) rebuild from
        // their replacement objects instead; coordinates never cross writers.
        // So the previous decoded blocks and patches are kept in place and only
        // this publication's suffix is decoded, and the root's references pair
        // with the writer's sealed objects by position. Nothing here rebuilds
        // an identity map over every object of the partition.
        assert!(root.blocks.starts_with(&self.snapshot.refs));
        assert!(root.vertex_patches.starts_with(&self.snapshot.patch_refs));
        assert_eq!(root.blocks.len(), blocks.len());
        assert_eq!(root.vertex_patches.len(), patches.len());
        let (block_prefix, patch_prefix) =
            (self.snapshot.refs.len(), self.snapshot.patch_refs.len());
        self.mark_recovery_stage(&mut recovery, DerivedPublicationStage::RefreshEdgeSnapshot);
        Self::fail_publication_if_requested(recovery, publication_failure)?;
        let mut fresh: Vec<(Vec<AdjacencyEntry>, Option<BlockProps>)> =
            Vec::with_capacity(root.blocks.len() - block_prefix);
        for (reference, sealed) in root.blocks[block_prefix..]
            .iter()
            .zip(&blocks[block_prefix..])
        {
            assert_eq!(
                reference.block_id, sealed.block_id,
                "a publish's root names its sealed blocks in order"
            );
            let (entries, hosted) = fgdb_strata::decode_block_with_properties(&sealed.bytes)
                .map_err(|error| WriteError::CommittedNeedsRecovery {
                    recovery,
                    source: Box::new(RebuildError::Store(StoreError::Malformed(error))),
                })?;
            // A propertied block's rows decode from the exact patch bytes the
            // same publish sealed — the durable path, not the writer's memory.
            let props = match hosted {
                Some((_, locators)) => {
                    let patch = sealed.property_patch.as_ref().expect(
                        "a sealed block declaring a hosted patch was sealed beside that patch",
                    );
                    let rows = match self.keys.scalar_resolver.as_deref() {
                        Some(resolver) => {
                            fgdb_strata::edge_props::decode_property_patch_with_resolver(
                                &patch.bytes,
                                resolver,
                            )
                        }
                        None => fgdb_strata::edge_props::decode_property_patch(&patch.bytes),
                    }
                    .map_err(|error| WriteError::CommittedNeedsRecovery {
                        recovery,
                        source: Box::new(RebuildError::Store(
                            StoreError::MalformedEdgePropertyPatch(error),
                        )),
                    })?;
                    Some(BlockProps { locators, rows })
                }
                None => None,
            };
            fresh.push((entries, props));
        }
        // A pinned read view may still own the previous Arc. `make_mut`
        // preserves it through copy-on-write; without a live view the Arc is
        // unique and this remains the old zero-copy move-forward path.
        let previous = Arc::make_mut(&mut self.snapshot);
        let mut decoded = std::mem::take(&mut previous.blocks);
        let mut decoded_props = std::mem::take(&mut previous.block_props);
        assert_eq!(decoded.len(), block_prefix);
        assert_eq!(decoded_props.len(), block_prefix);
        for (entries, props) in fresh {
            decoded.push(entries);
            decoded_props.push(props);
        }
        // The identical carry-forward rule for the vertex half: an unchanged
        // patch reference means an unchanged decoded patch, and new patches
        // decode from the exact bytes `put_patch_verified` just fsynced.
        self.mark_recovery_stage(
            &mut recovery,
            DerivedPublicationStage::RefreshVertexSnapshot,
        );
        Self::fail_publication_if_requested(recovery, publication_failure)?;
        let mut fresh_patches: Vec<VertexPatchRows> =
            Vec::with_capacity(root.vertex_patches.len() - patch_prefix);
        for (reference, sealed) in root.vertex_patches[patch_prefix..]
            .iter()
            .zip(&patches[patch_prefix..])
        {
            assert_eq!(
                reference.patch_id, sealed.patch_id,
                "a publish's root names its sealed patches in order"
            );
            fresh_patches.push(
                match self.keys.scalar_resolver.as_deref() {
                    Some(resolver) => {
                        fgdb_strata::vertex::decode_patch_with_resolver(&sealed.bytes, resolver)
                    }
                    None => fgdb_strata::vertex::decode_patch(&sealed.bytes),
                }
                .map_err(|error| WriteError::CommittedNeedsRecovery {
                    recovery,
                    source: Box::new(RebuildError::Store(StoreError::MalformedPatch(error))),
                })?,
            );
        }
        let previous = Arc::make_mut(&mut self.snapshot);
        let mut decoded_patches = std::mem::take(&mut previous.patches);
        assert_eq!(decoded_patches.len(), patch_prefix);
        decoded_patches.extend(fresh_patches);
        self.writer = folded;
        self.heads = WriteHeads {
            versions: new_versions,
            next_birth_ordinal,
        };
        self.snapshot = Arc::new(Snapshot {
            adjacency: std::sync::OnceLock::from(Arc::new(
                self.snapshot
                    .adjacency_index()
                    .extend(&decoded, self.snapshot.refs.len()),
            )),
            property_index: Arc::new(
                self.snapshot
                    .property_index
                    .extend(&decoded_patches, self.snapshot.patch_refs.len()),
            ),
            blocks: decoded,
            block_props: decoded_props,
            refs: root.blocks,
            patches: decoded_patches,
            patch_refs: root.vertex_patches,
            frontier,
            root: root_id,
            manifest,
            delta_index: next_delta_index,
        });
        self.state = DatabaseState::Healthy {
            published_frontier: self.snapshot.frontier,
        };
        // Maintenance is derived, synchronous and fail-closed. A refusal must
        // never turn this already-durable successful publication into an abort.
        if !self.standing_queries.is_empty() {
            let batch = self
                .snapshot
                .delta_index
                .get(frontier)
                .expect("successful publication retains its committed delta");
            standing_query::publish(&mut self.standing_queries, cx, batch);
        }
        Ok(self.snapshot.frontier)
    }

    /// Work performed by the last generation's derived-index maintenance.
    /// Counts processed rows/properties and persistent-tree visits/allocations;
    /// excludes storage publication and source query execution.
    pub fn index_maintenance_work(&self) -> Result<(u64, u64), ReadError> {
        self.ensure_readable()?;
        Ok((
            self.snapshot.adjacency_index().maintenance_work(),
            self.snapshot.property_index.maintenance_work(),
        ))
    }

    /// Compare both derived indexes against fresh construction from the admitted
    /// generation. This diagnostic does not publish or alter either index.
    pub fn verify_snapshot_indexes(&self) -> Result<bool, ReadError> {
        self.ensure_readable()?;
        Ok(self
            .snapshot
            .adjacency_index()
            .equivalent(&gql_exec::source::AdjacencyIndex::build(
                &self.snapshot.blocks,
            ))
            && self.snapshot.property_index.equivalent(
                &gql_exec::source::PropertyEqualityIndex::build(&self.snapshot.patches),
            ))
    }

    /// The live destinations of `src` over `relation`, at the published
    /// frontier.
    pub fn neighbours(&self, src: VId, relation: RelationId) -> Result<Vec<VId>, ReadError> {
        self.neighbours_at(src, relation, self.snapshot.frontier)
    }

    /// The live SOURCES of edges arriving at `dst` over `relation`, at the
    /// published frontier (fgdb-x164) — the reverse face of
    /// [`Database::neighbours`], served as an honest derived scan until the
    /// Tier-R reverse family exists.
    pub fn in_neighbours(&self, dst: VId, relation: RelationId) -> Result<Vec<VId>, ReadError> {
        self.in_neighbours_at(dst, relation, self.snapshot.frontier)
    }

    /// [`Database::in_neighbours`] as of `as_of`, under the same frontier
    /// refusal as every `*_at` read.
    pub fn in_neighbours_at(
        &self,
        dst: VId,
        relation: RelationId,
        as_of: CommitSeq,
    ) -> Result<Vec<VId>, ReadError> {
        self.ensure_readable()?;
        self.snapshot.in_neighbours_at(dst, relation, as_of)
    }

    /// [`Database::neighbours`] as of `as_of` — the system-time read B1 makes
    /// core (fgdb-90jx). History in the spine is whole, so every historical
    /// answer is served from the same durable blocks the frontier answer is;
    /// a sequence beyond the published frontier is refused, never clamped.
    pub fn neighbours_at(
        &self,
        src: VId,
        relation: RelationId,
        as_of: CommitSeq,
    ) -> Result<Vec<VId>, ReadError> {
        self.ensure_readable()?;
        self.snapshot.neighbours_at(src, relation, as_of)
    }

    /// Execute one pinned GQL statement — `MATCH (a)-[:R]->(b) RETURN b` —
    /// over this handle's live snapshot (fgdb-w5-parsers-nje.1).
    ///
    /// Parse and bind belong to `fgdb-gql`; execution consumes the resulting
    /// [`BoundPlan`] and ONLY the plan (doctrine 7: no parser-interprets-AST
    /// engine — no parse-tree type crosses into `gql_exec`). The relation
    /// name binds through the caller's [`RelationBind`]; there is no invented
    /// catalog. Rows come back CGSE-deterministic: destination `VId`
    /// ascending, so the same graph, statement, and bind answer
    /// byte-identically. An off-grammar text, an unbound relation name, and a
    /// refusing handle are three typed [`GqlError`] arms because they are
    /// three different remedies.
    pub fn execute_gql(&self, src: &str, bind: &RelationBind) -> Result<Vec<VId>, GqlError> {
        // `RelationBind::bind` is fgdb-gql's whole public entry: the AST never
        // leaves that crate, so a BoundPlan is the only thing this method CAN
        // hand the executor. The nested parse failure is re-split here because
        // off-grammar text and an unbound relation name are different remedies.
        let plan = bind.bind(src).map_err(|error| match error {
            fgdb_gql::BindError::Parse(parse) => GqlError::Parse(parse),
            unbound => GqlError::Bind(unbound),
        })?;
        gql_exec::execute(&plan, self).map_err(GqlError::Read)
    }

    /// Execute the pinned GQL MATCH against one historical commit sequence.
    pub fn execute_gql_at(
        &self,
        src: &str,
        bind: &RelationBind,
        as_of: CommitSeq,
    ) -> Result<Vec<VId>, GqlError> {
        let plan = bind.bind(src).map_err(|error| match error {
            fgdb_gql::BindError::Parse(parse) => GqlError::Parse(parse),
            unbound => GqlError::Bind(unbound),
        })?;
        gql_exec::execute_at(&plan, self, as_of).map_err(GqlError::Read)
    }

    /// [`Database::execute_gql`], plus the replayable certificate Genesis
    /// criterion 5 asks for (fgdb-gate-genesis-lce.1): the rows AND a
    /// [`GqlCertificate`] binding them to this handle's published frontier,
    /// the exact statement bytes, and the canonical bind encoding.
    ///
    /// The rows come from the SAME path as [`Database::execute_gql`] — this
    /// method calls it, so the two can never drift. Determinism follows: the
    /// same database state, source text, and bind produce byte-identical rows
    /// and a byte-identical certificate, while any intervening commit
    /// advances `snapshot_seq` and therefore changes the certificate. An
    /// off-grammar text fails as [`GqlError::Parse`] before any certificate
    /// exists. No-claim: this is a certificate over (snapshot seq, statement,
    /// bind), not plan cost, not an operator tree, not FG-INV-19 whole-engine
    /// replay.
    pub fn execute_gql_certified(
        &self,
        src: &str,
        bind: &RelationBind,
    ) -> Result<(Vec<VId>, GqlCertificate), GqlError> {
        let rows = self.execute_gql(src, bind)?;
        // Read the frontier only after the rows succeeded: a refused read
        // yields its typed error with no certificate, and under `&self` on
        // the single-writer handle no commit can interleave between the scan
        // and this observation.
        let snapshot_seq = self.frontier().map_err(GqlError::Read)?;
        let certificate = GqlCertificate {
            snapshot_seq,
            statement_digest: gql_cert::digest_statement(src),
            bind_digest: gql_cert::digest_bind(bind),
        };
        Ok((rows, certificate))
    }

    /// Execute and certify the pinned GQL MATCH at one historical sequence.
    pub fn execute_gql_certified_at(
        &self,
        src: &str,
        bind: &RelationBind,
        as_of: CommitSeq,
    ) -> Result<(Vec<VId>, GqlCertificate), GqlError> {
        let rows = self.execute_gql_at(src, bind, as_of)?;
        let certificate = GqlCertificate {
            snapshot_seq: as_of,
            statement_digest: gql_cert::digest_statement(src),
            bind_digest: gql_cert::digest_bind(bind),
        };
        Ok((rows, certificate))
    }

    /// The plan-level certificate for one pinned statement WITHOUT executing
    /// it (fgdb-gql-oracle-cert-jjn0): parse and bind exactly as
    /// [`Database::execute_gql`] does, then certify the [`BoundPlan`] against
    /// this handle's published frontier via `gql_cert::certify`.
    ///
    /// A fenced handle refuses with the same typed [`GqlError::Read`]
    /// recovery error every other read surfaces — the frontier accessor runs
    /// the ordinary readability check before any certificate exists. An
    /// off-grammar text is [`GqlError::Parse`]; an unbound relation name is
    /// [`GqlError::Bind`].
    pub fn gql_plan_certificate(
        &self,
        src: &str,
        bind: &RelationBind,
    ) -> Result<GqlPlanCertificate, GqlError> {
        let snapshot_seq = self.frontier().map_err(GqlError::Read)?;
        let plan = bind.bind(src).map_err(|error| match error {
            fgdb_gql::BindError::Parse(parse) => GqlError::Parse(parse),
            unbound => GqlError::Bind(unbound),
        })?;
        Ok(gql_cert::certify(&plan, snapshot_seq))
    }

    pub fn gql_plan_certificate_at(
        &self,
        src: &str,
        bind: &RelationBind,
        as_of: CommitSeq,
    ) -> Result<GqlPlanCertificate, GqlError> {
        let plan = bind.bind(src).map_err(|error| match error {
            fgdb_gql::BindError::Parse(parse) => GqlError::Parse(parse),
            unbound => GqlError::Bind(unbound),
        })?;
        Ok(gql_cert::certify(&plan, as_of))
    }

    /// The edge `eid` — its endpoints, relation, lifetime, AND properties —
    /// at the published frontier, or `None` when no visible version exists.
    ///
    /// Served from the durable tier-D blocks and their hosted property
    /// patches (fgdb-yqor), under the same whole-history validation as
    /// [`Database::neighbours`]: the properties made the full encode →
    /// content-address → fsync → admit → decode round trip before a reader
    /// can see them.
    pub fn edge(&self, eid: EId) -> Result<Option<EdgeRecord>, ReadError> {
        self.edge_at(eid, self.snapshot.frontier)
    }

    /// [`Database::edge`] as of `as_of` (fgdb-90jx). A version retired at
    /// `r` answers for every `as_of` in `[created_at, r)` and never after —
    /// the same visibility rule the frontier read applies, at an older
    /// sequence.
    pub fn edge_at(&self, eid: EId, as_of: CommitSeq) -> Result<Option<EdgeRecord>, ReadError> {
        self.ensure_readable()?;
        self.snapshot.edge_at(eid, as_of)
    }

    /// The vertex `vid` — its labels and properties — at the published
    /// frontier, or `None` when no visible row exists (fgdb-3xoi).
    ///
    /// Served from the durable tier-D vertex patches the snapshot decoded,
    /// exactly as [`Database::neighbours`] is served from blocks: the row made
    /// the full encode → content-address → fsync → decode round trip before a
    /// reader can see it.
    pub fn vertex(&self, vid: VId) -> Result<Option<VertexRow>, ReadError> {
        self.vertex_at(vid, self.snapshot.frontier)
    }

    /// [`Database::vertex`] as of `as_of` (fgdb-90jx): the version chain's
    /// statement visible at that sequence, or `None` when the vertex did not
    /// exist yet — or no longer did.
    pub fn vertex_at(&self, vid: VId, as_of: CommitSeq) -> Result<Option<VertexRow>, ReadError> {
        self.ensure_readable()?;
        self.snapshot.vertex_at(vid, as_of)
    }

    /// Every vertex visible at the published frontier, in ascending VId
    /// order — the whole-graph scan a query layer starts from (fgdb-9k5w).
    pub fn vertices(&self) -> Result<Vec<VertexRow>, ReadError> {
        self.vertices_at(self.snapshot.frontier)
    }

    /// [`Database::vertices`] as of `as_of`, under the same frontier refusal
    /// as every `*_at` read.
    pub fn vertices_at(&self, as_of: CommitSeq) -> Result<Vec<VertexRow>, ReadError> {
        self.ensure_readable()?;
        self.snapshot.vertices_at(as_of)
    }

    /// Every edge visible at the published frontier — endpoints, relation,
    /// lifetime, and properties — in ascending EId order (fgdb-9k5w).
    pub fn edges(&self) -> Result<Vec<EdgeRecord>, ReadError> {
        self.edges_at(self.snapshot.frontier)
    }

    /// [`Database::edges`] as of `as_of`, under the same frontier refusal as
    /// every `*_at` read and the identical whole-history validation and
    /// last-statement-wins precedence as the point lookups.
    pub fn edges_at(&self, as_of: CommitSeq) -> Result<Vec<EdgeRecord>, ReadError> {
        self.ensure_readable()?;
        self.snapshot.edges_at(as_of)
    }

    /// The sequence the healthy derived partition has caught up to.
    ///
    /// A fenced handle must not expose its retained frontier as though it were
    /// current: after D2, Chronicle may already be ahead of this snapshot.
    /// The stale/current split remains available in [`Database::state`] and
    /// [`RecoveryRequired`], while this state-bearing read follows the same
    /// typed recovery fence as graph reads.
    pub fn frontier(&self) -> Result<CommitSeq, ReadError> {
        self.ensure_readable()?;
        Ok(self.snapshot.frontier)
    }

    /// The derived window over committed delta batches.
    ///
    /// Reads check `Healthy` like every other graph read: a fenced handle
    /// must not present a window that may have been inserted after D2 while
    /// the retained snapshot is still one commit behind.
    /// Checkpoint open initially retains no batches before its frontier.
    /// [`Self::ensure_delta_window`] authenticates a requested prefix on demand;
    /// the returned index always exposes its actual retained floor.
    pub fn delta_index(&self) -> Result<&LocalDeltaBatchIndex, ReadError> {
        self.ensure_readable()?;
        Ok(&self.snapshot.delta_index)
    }

    /// How far the derived delta window reaches. After a successful write
    /// this equals the new [`CommitSeq`]; after a fresh create it is the
    /// origin. Distinct from [`Database::frontier`] only in name — both
    /// report the same sequence on a healthy handle — so Ripple/CDC can
    /// ask for the window without importing the snapshot's vocabulary.
    pub fn delta_frontier(&self) -> Result<CommitSeq, ReadError> {
        self.ensure_readable()?;
        Ok(self.snapshot.delta_index.frontier())
    }

    /// The retained committed batches strictly after `after`, in commit order.
    ///
    /// This is the live frontier-stream face (og6n subset): a consumer names
    /// the last sequence it has applied and receives the gap-free suffix, or
    /// a refusal. A cursor past the frontier is [`ReadError::BeyondFrontier`];
    /// a cursor below the retained floor is [`ReadError::DeltaCursorRetired`].
    /// After checkpoint open, call [`Self::ensure_delta_window`] before asking
    /// below its initially unloaded floor. This synchronous accessor never
    /// treats unloaded history as an empty result.
    /// Reads check `Healthy` like every other graph read.
    pub fn delta_since(
        &self,
        after: CommitSeq,
    ) -> Result<impl Iterator<Item = &LogicalDeltaBatch> + '_, ReadError> {
        self.ensure_readable()?;
        self.snapshot
            .delta_index
            .since(after)
            .map_err(read_error_from_index)
    }

    /// Authenticate and retain every committed delta strictly after `after`.
    ///
    /// Checkpoint-selected open starts with an empty window at the published
    /// frontier. This operation reads only the capsules missing between the
    /// requested cut and the current materialization floor, verifies each
    /// against its recovered marker, and publishes one complete replacement.
    /// Failure or cancellation preserves the graph and the previous window.
    /// Existing immutable read views keep their original window; reacquire a
    /// view after this call when it needs the newly admitted history.
    ///
    /// This lowers only the prefix omitted by open. It cannot undo an explicit
    /// retirement, invent a missing capsule, or weaken first-committer-wins.
    /// Synchronous historical preparation, change-feed and resident-index
    /// callers may use this explicit I/O boundary before their existing APIs.
    /// Prepared-write and transaction completion also call it before validating
    /// any basis that lies below the loaded floor.
    pub async fn ensure_delta_window(
        &mut self,
        cx: &CommitCx,
        after: CommitSeq,
    ) -> Result<(), RebuildError> {
        if !matches!(self.state, DatabaseState::Healthy { .. }) {
            return Err(RebuildError::HandleNotHealthy(self.state));
        }
        let index_error = |error| RebuildError::Index {
            commit_seq: after.0,
            error,
        };
        let floor = self.delta_materialized_after;
        let retained = self.snapshot.delta_index.retained_after_commit_seq();
        if after.0 >= floor.0 || retained != floor {
            // This also retains existing future and genuinely retired cursor
            // refusals. A caller changing retention cannot turn that removal
            // into implicit rehydration from the commit stream.
            // Only the refusal matters here; the cursor itself is unused.
            self.snapshot
                .delta_index
                .since(after)
                .map(drop)
                .map_err(index_error)?;
            return Ok(());
        }
        cx.checkpoint().map_err(RebuildError::Interrupted)?;
        let mut candidate = empty_delta_window_at(cx, &self.coordinator, after)?;
        let entries = self.coordinator.chain().entries();
        let start = entries.partition_point(|entry| entry.marker.commit_seq <= after.0);
        let end = entries.partition_point(|entry| entry.marker.commit_seq <= floor.0);
        for entry in &entries[start..end] {
            let batch = read_delta_batch(
                cx,
                &self.coordinator,
                &self.keys,
                entry,
                &mut self.crypto_verification_events,
            )
            .await?;
            candidate.insert(batch).map_err(index_error)?;
        }
        if candidate.frontier() != floor {
            return Err(index_error(IndexError::Gapped {
                expected: floor,
                found: candidate.frontier(),
            }));
        }
        for batch in self
            .snapshot
            .delta_index
            .since(floor)
            .map_err(index_error)?
        {
            cx.checkpoint().map_err(RebuildError::Interrupted)?;
            candidate.insert(batch.clone()).map_err(index_error)?;
        }
        cx.checkpoint().map_err(RebuildError::Interrupted)?;
        Arc::make_mut(&mut self.snapshot).delta_index = candidate;
        self.delta_materialized_after = after;
        if after == CommitSeq::ORIGIN {
            self.preparation_anchor = None;
        }
        Ok(())
    }

    /// Consolidate the partition's durable history: fewer blocks, the SAME
    /// answer at EVERY committed sequence (fgdb-ge6a).
    ///
    /// **CONSOLIDATION ONLY — the floor is zero.** Time-travel reads promise
    /// every committed sequence, and deciding that no reader can ask below
    /// some sequence is the transaction layer's snapshot-tracking question;
    /// until it exists, nothing is droppable and this method refuses to
    /// guess. What it does reclaim: cross-block restatements collapse, and
    /// the block count stops growing with tombstone churn.
    ///
    /// **DURABLE because open selects the manifest**: the compacted root is
    /// republished through manifest and slot, and checkpoint-selected open
    /// lands on it after authenticating its temporal projection.
    /// The full-stream rebuild remains the AUTHORITATIVE recovery and
    /// re-derives the uncompacted layout by design (doctrine 5: derived
    /// state is discarded and rebuilt) — its answers are identical, and its
    /// republication simply supersedes the compacted root again.
    ///
    /// **CRASH-SAFE BY SHAPE, not by hooks**: every durable step before the
    /// final slot publication is a content-addressed APPEND — patches,
    /// blocks, root, manifest — so a crash anywhere in them leaves only
    /// unreferenced objects and the slot still naming the previous
    /// generation, which the next open lands on unchanged. The slot swap
    /// itself is the dual-slot atomic publication the root-store laws pin,
    /// and the slot law's lag case covers the one observable window.
    ///
    /// **CANCELLATION FENCES THE BORROWED HANDLE.** The root-slot await can be
    /// dropped after bytes moved but before its evidence returns. Immediately
    /// before that await this method enters [`DatabaseState::NeedsAuthoritativeRecovery`]
    /// at the unchanged semantic frontier; only the successful, same-poll
    /// snapshot/generation swap restores `Healthy`. A caller can therefore
    /// never reuse the old generation after an ambiguous compaction publish.
    pub async fn compact(&mut self, cx: &CommitCx) -> Result<(), RebuildError> {
        if !matches!(self.state, DatabaseState::Healthy { .. }) {
            return Err(RebuildError::HandleNotHealthy(self.state));
        }
        // Compaction is optional derived publication. Do not write even an
        // unreferenced replacement object when no successor slot can ever
        // select it.
        let next_generation = next_slot_generation(self.slot_generation)?;
        self.retain_preparation_anchor();
        let compaction = fgdb_strata::compact::compact_with_props(
            &self.snapshot.blocks,
            &self.snapshot.block_props,
            CommitSeq(0),
        )
        .map_err(|error| RebuildError::Store(StoreError::MalformedRoot(error)))?;

        // Encode the replacement generation: chains RESTART per family
        // (state-chain semantics, fgdb-4391) and multi-chunk families link
        // in emission order — the contract compact_with_props documents.
        let mut chain_heads: std::collections::BTreeMap<
            (VId, RelationId),
            fgdb_strata::DeltaBlockVersion,
        > = std::collections::BTreeMap::new();
        let mut sealed = Vec::with_capacity(compaction.blocks.len());
        for (entries, props) in compaction.blocks.iter().zip(&compaction.block_props) {
            let family = entries
                .first()
                .map(|entry| (entry.src, entry.relation))
                .expect("the packer emits no empty blocks");
            let predecessor = chain_heads.get(&family).copied();
            let (bytes, property_patch) = match props {
                Some(props) => {
                    let patch_bytes = fgdb_strata::edge_props::encode_property_patch(&props.rows)
                        .map_err(|error| {
                        RebuildError::Store(StoreError::MalformedEdgePropertyPatch(error))
                    })?;
                    let patch_id = fgdb_strata::edge_props::property_patch_id(
                        self.keys.k_oid(),
                        self.keys.namespace,
                        &patch_bytes,
                    );
                    let bytes = fgdb_strata::encode_block_with_properties(
                        PARTITION,
                        predecessor,
                        entries,
                        patch_id,
                        &props.locators,
                        &props.rows,
                    )
                    .map_err(|error| RebuildError::Store(StoreError::Malformed(error)))?;
                    (
                        bytes,
                        Some(fgdb_strata::writer::SealedPropertyPatch {
                            patch_id,
                            bytes: patch_bytes,
                        }),
                    )
                }
                None => (
                    fgdb_strata::encode_block(PARTITION, predecessor, entries)
                        .map_err(|error| RebuildError::Store(StoreError::Malformed(error)))?,
                    None,
                ),
            };
            let (first_seq, last_seq) =
                fgdb_strata::root::span_of(entries).expect("the packer emits no empty blocks");
            let block_id = fgdb_strata::block_id(self.keys.k_oid(), self.keys.namespace, &bytes);
            chain_heads.insert(family, fgdb_strata::DeltaBlockVersion(block_id));
            sealed.push(fgdb_strata::writer::SealedBlock {
                block_id,
                bytes,
                first_seq,
                last_seq,
                property_patch,
            });
        }

        // The vertex half consolidates the same way: restatements collapse,
        // canonical repack, spans re-derived from the rows themselves.
        let (compacted_patches, _superseded) =
            fgdb_strata::compact::compact_vertex_patches(&self.snapshot.patches, CommitSeq(0))
                .map_err(|error| RebuildError::Store(StoreError::MalformedPatch(error)))?;
        let mut sealed_patches = Vec::with_capacity(compacted_patches.len());
        for rows in &compacted_patches {
            let bytes = fgdb_strata::vertex::encode_patch(rows)
                .map_err(|error| RebuildError::Store(StoreError::MalformedPatch(error)))?;
            let (first_seq, last_seq) =
                fgdb_strata::vertex::span_of_rows(rows).expect("the packer emits no empty patches");
            sealed_patches.push(fgdb_strata::writer::SealedPatch {
                patch_id: fgdb_strata::vertex::vertex_patch_id(
                    self.keys.k_oid(),
                    self.keys.namespace,
                    &bytes,
                ),
                bytes,
                first_seq,
                last_seq,
            });
        }

        let frontier = self.snapshot.frontier;
        let writer = BlockWriter::from_published_partition(
            GRAPH,
            BRANCH,
            PARTITION,
            sealed,
            sealed_patches,
            &compaction.blocks,
            &compaction.block_props,
            &compacted_patches,
            frontier,
        )
        .map_err(|error| RebuildError::Store(StoreError::MalformedRoot(error)))?;

        // The logical state is unchanged, so the handle's version heads and
        // allocator stay as they are; the shared tail republishes and reopens
        // from disk.
        let published_chain_hash = chain_commitment_at(self.coordinator.chain(), frontier)
            .expect("a healthy handle's frontier is on its own recovered chain");
        let (mut snapshot, writer, receipts) = publish_and_snapshot(
            cx,
            &self.store,
            &self.keys,
            writer,
            frontier,
            published_chain_hash,
        )
        .await?;
        snapshot.delta_index = self.snapshot.delta_index.clone();
        // The slot advances so the compacted generation is what the next
        // checkpoint-selected open lands on.
        let manifest_records = [ManifestRecord {
            graph: GRAPH,
            branch: BRANCH,
            partition: PARTITION,
            root: snapshot.root,
            // Length-only computation, as in manifest_bytes_len: every V2
            // record is RECORD_LEN regardless of the commitment value.
            published_chain_hash: Digest([0u8; 32]),
        }];
        let manifest_len = encode_manifest(&manifest_records)
            .map(|bytes| bytes.len() as u64)
            .expect("one root is one canonical record");
        // The slot write is the first cancellable operation that can make the
        // replacement generation externally observable. Fence BEFORE the
        // await, even though Chronicle's frontier is unchanged: dropping the
        // future can otherwise return the mutable borrow with `slot_generation`
        // still naming the predecessor while durable storage may already hold
        // `next_generation`. The next write would then try to reuse a durable
        // generation for different content. There is no await after a
        // successful publication before the generation/snapshot swap and
        // Healthy restoration, so those actions complete in the same poll.
        let recovery = RecoveryRequired {
            durable_frontier: frontier,
            published_frontier: frontier,
            failed_stage: DerivedPublicationStage::PublishRootSlot,
        };
        self.state = DatabaseState::NeedsAuthoritativeRecovery(recovery);
        self.slot_store
            .publish_evidenced(
                cx,
                &spine_slot(&self.keys, next_generation, snapshot.manifest, manifest_len),
            )
            .await
            .map_err(RebuildError::Slot)?;
        self.slot_generation = next_generation;
        self.snapshot = Arc::new(snapshot);
        self.writer = writer;
        // The old receipts describe the superseded generation. The ones the
        // compacted publication earned describe the replacement exactly.
        self.receipts = receipts;
        self.state = DatabaseState::Healthy {
            published_frontier: frontier,
        };
        Ok(())
    }

    /// The identity of the healthy partition manifest (fgdb-63w2) — what a
    /// root slot carries, republished beside every root under the same
    /// determinism law as [`Database::partition_root`]. A fenced handle
    /// returns the same typed recovery error as graph reads.
    pub fn manifest(&self) -> Result<ManifestVersion, ReadError> {
        self.ensure_readable()?;
        Ok(self.snapshot.manifest)
    }

    /// The identity of the healthy partition root.
    ///
    /// Exposed because the rebuild is deterministic and content-addressed, so
    /// "reopening the same stream publishes the same root" is a law a caller can
    /// assert rather than a property the crate merely claims. A fenced handle
    /// cannot expose the stale retained identity as current.
    pub fn partition_root(&self) -> Result<PartitionRootVersion, ReadError> {
        self.ensure_readable()?;
        Ok(self.snapshot.root)
    }

    pub fn path(&self) -> &Path {
        self.coordinator.database_dir()
    }
}

/// What [`Database::adopt`] synced under the database directory, the
/// directory itself included. The parent's sync is not counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Adopted {
    pub files: u64,
    pub directories: u64,
}

/// Make every namespace entry currently visible in `directory` durable through
/// the caller's commit authority.
async fn sync_vfs_directory<V: Vfs>(
    cx: &CommitCx,
    vfs: &V,
    directory: &Path,
) -> std::io::Result<()> {
    cx.with_restriction_async(async {
        let directory = vfs.open(directory, &OpenOptions::new().read(true)).await?;
        directory.sync_all().await
    })
    .await
}

/// The element-version chain domain.
///
/// Deliberately the same BYTES as the reference oracle's
/// `ELEMENT_VERSION_DOMAIN`, and deliberately not the same CONSTANT: the
/// engine and the oracle each derive versions from their own spelling of the
/// law, and the differential's replay validates every engine-emitted
/// `before_version` against the oracle's chains — shared code here would gut
/// that check (§15.2).
/// Domain v3 — STATEMENT-CHAIN versions (ruled on fgdb-ge6a): the chain
/// advances once per DURABLE STATEMENT, hashing exactly what the statement
/// durably is — element identity plus content — never DeltaRow bytes. Same-
/// commit folds (create+update in place) therefore advance the chain once,
/// which is what makes the head a pure function of durable state: a
/// manifest-selected reopen recomputes identical chains from blocks and
/// patches alone after authenticating the prefix. Commit sequences are
/// deliberately OUTSIDE the transcript —
/// the predecessor link already orders the chain, and a batch stamps a
/// same-batch create's head before any sequence exists. Deliberately
/// duplicated in `fgdb-reference` (§15.2).
const ELEMENT_VERSION_DOMAIN: &[u8] = b"fgdb.reference.element-version.v3";

/// One vertex statement's transcript: identity, birth ordinal, content.
fn vertex_statement_transcript(
    vid: VId,
    birth_ordinal: u64,
    labels: &[LabelId],
    props: &[(PropertyKeyId, CanonicalScalar)],
) -> Result<Vec<u8>, CanonicalError> {
    let mut out = vec![0x01];
    out.extend_from_slice(&vid.0.to_le_bytes());
    out.extend_from_slice(&birth_ordinal.to_le_bytes());
    out.extend_from_slice(&(labels.len() as u32).to_le_bytes());
    for label in labels {
        out.extend_from_slice(&label.0.to_le_bytes());
    }
    append_props_transcript(&mut out, props)?;
    Ok(out)
}

/// One edge statement's transcript: identity, immutable topology, content.
fn edge_statement_transcript(
    eid: EId,
    src: VId,
    relation: RelationId,
    dst: VId,
    props: &[(PropertyKeyId, CanonicalScalar)],
) -> Result<Vec<u8>, CanonicalError> {
    let mut out = vec![0x02];
    out.extend_from_slice(&eid.0.to_le_bytes());
    out.extend_from_slice(&src.0.to_le_bytes());
    out.extend_from_slice(&relation.0.to_le_bytes());
    out.extend_from_slice(&dst.0.to_le_bytes());
    append_props_transcript(&mut out, props)?;
    Ok(out)
}

fn append_props_transcript(
    out: &mut Vec<u8>,
    props: &[(PropertyKeyId, CanonicalScalar)],
) -> Result<(), CanonicalError> {
    out.extend_from_slice(&(props.len() as u32).to_le_bytes());
    for (key, value) in props {
        let encoded = value.encode().map_err(|_| CanonicalError::Scalar)?;
        out.extend_from_slice(&key.0.to_le_bytes());
        out.extend_from_slice(&(encoded.len() as u32).to_le_bytes());
        out.extend_from_slice(&encoded);
    }
    Ok(())
}

/// Extend one element's version chain with one canonical effect — the
/// engine's independent spelling of the reference derivation: a domain, a
/// predecessor tag distinguishing creation from an all-zero prior digest, a
/// self-delimiting row length, and the row's canonical bytes. No branch
/// population, wall clock, or commit sequence enters it.
fn statement_successor(previous: Option<ObjectId>, transcript: &[u8]) -> ObjectId {
    let mut hasher = fgdb_crypto::Hasher::new();
    hasher.update(ELEMENT_VERSION_DOMAIN);
    match previous {
        None => {
            hasher.update(&[0]);
        }
        Some(version) => {
            hasher.update(&[1]);
            hasher.update(&version.0);
        }
    }
    hasher.update(&(transcript.len() as u64).to_le_bytes());
    hasher.update(transcript);
    ObjectId(hasher.finalize().0)
}

/// Endpoint insertion witnesses complement the element write-set vocabulary.
/// Existing edge deletions/updates are witnessed by observed EIds; a newly
/// inserted edge has no previously observable EId, so both endpoints matter.
/// Table-scan phantoms are separately tracked by WriteTxn's scan witnesses.
pub(crate) fn adjacency_endpoints(
    row: &DeltaRow,
    endpoints: &mut std::collections::BTreeSet<ElementId>,
) {
    if let DeltaRow::CreateEdge { src, dst, .. } = row {
        endpoints.insert(ElementId::Vertex(*src));
        endpoints.insert(ElementId::Vertex(*dst));
    }
}

fn touched_elements(row: &DeltaRow, touched: &mut std::collections::BTreeSet<ElementId>) {
    match row {
        DeltaRow::CreateVertex { vid, .. } => {
            touched.insert(ElementId::Vertex(*vid));
        }
        DeltaRow::CreateEdge { eid, .. } => {
            touched.insert(ElementId::Edge(*eid));
        }
        DeltaRow::DeleteVertex {
            vid,
            sorted_retired_incident_edges,
            ..
        } => {
            touched.insert(ElementId::Vertex(*vid));
            for eid in sorted_retired_incident_edges {
                touched.insert(ElementId::Edge(*eid));
            }
        }
        DeltaRow::DeleteEdge { eid, .. } => {
            touched.insert(ElementId::Edge(*eid));
        }
        DeltaRow::LabelMembership { vid, .. } => {
            touched.insert(ElementId::Vertex(*vid));
        }
        DeltaRow::Property { elem, .. } => {
            touched.insert(*elem);
        }
        _ => {}
    }
}

/// Advance the version map by ONE COMMIT (fgdb-ge6a v3): after the writer
/// folded every row, each touched element's head steps once over the
/// STATEMENT the fold left live — or leaves the map when the element did.
/// The map still holds pre-commit heads when this runs, and each element is
/// visited once, so `prev` is exactly the durable chain's predecessor.
fn fold_statement_versions(
    versions: &mut std::collections::BTreeMap<ElementId, ObjectId>,
    touched: &std::collections::BTreeSet<ElementId>,
    writer: &BlockWriter,
) -> Result<(), CanonicalError> {
    for elem in touched {
        match elem {
            ElementId::Vertex(vid) => match writer.live_vertex_row(*vid) {
                Some(row) => {
                    let transcript = vertex_statement_transcript(
                        row.vid,
                        row.birth_ordinal,
                        &row.labels,
                        &row.props,
                    )?;
                    let previous = versions.get(elem).copied();
                    versions.insert(*elem, statement_successor(previous, &transcript));
                }
                None => {
                    versions.remove(elem);
                }
            },
            ElementId::Edge(eid) => match writer.live_edge_statement(*eid) {
                Some((src, relation, dst, _created_at, props)) => {
                    let transcript = edge_statement_transcript(*eid, src, relation, dst, &props)?;
                    let previous = versions.get(elem).copied();
                    versions.insert(*elem, statement_successor(previous, &transcript));
                }
                None => {
                    versions.remove(elem);
                }
            },
        }
    }
    Ok(())
}

/// One vertex's mutable batch-prefix content: `(labels, props)`, both in
/// canonical order.
type VertexContent = (Vec<LabelId>, Vec<(PropertyKeyId, CanonicalScalar)>);

fn sort_write_labels_and_props(
    labels: &mut Vec<LabelId>,
    props: &mut Vec<(PropertyKeyId, CanonicalScalar)>,
) {
    labels.sort_unstable();
    labels.dedup();
    sort_write_props(props);
}

/// Sort by key. Identical `(key, value)` repeats collapse (the reference's
/// `canonical_props`); a key with two different values is left adjacent so
/// template build refuses `NonCanonicalPropertyOrder` (fgdb-nsrv).
fn sort_write_props(props: &mut Vec<(PropertyKeyId, CanonicalScalar)>) {
    props.sort_by_key(|(key, _)| *key);
    let mut kept = 0usize;
    for i in 0..props.len() {
        if kept > 0 && props[i].0 == props[kept - 1].0 && props[i].1 == props[kept - 1].1 {
            continue;
        }
        if kept != i {
            props[kept] = props[i].clone();
        }
        kept += 1;
    }
    props.truncate(kept);
}

/// The batch-prefix content entry for `vid`, seeded from the merged committed
/// row on first touch, so an update's before-image reflects everything the
/// batch prefix already did to that vertex.
fn vertex_content_entry<'content>(
    prefix_content: &'content mut std::collections::BTreeMap<VId, VertexContent>,
    snapshot: &Snapshot,
    vid: VId,
) -> &'content mut VertexContent {
    prefix_content.entry(vid).or_insert_with(|| {
        let row = merge_vertex(&snapshot.patches, vid, snapshot.frontier)
            .expect("liveness was proven before content is materialized");
        (row.labels, row.props)
    })
}

/// The pinned intent semantics this slice commits under.
///
/// A real `IntentSemanticsOid` names a registered semantics version; that
/// registry is `fgdb-w5-intent-log-94z`'s. A fixed constant here is a subset —
/// every capsule this slice writes declares the same semantics, which is true —
/// and it is deliberately not a fabricated registry lookup.
fn intent_semantics_oid() -> ObjectId {
    ObjectId([0x11; 32])
}

/// A template prepared for commit: its canonical bytes, the identity those bytes
/// have, and the digest the marker will declare.
///
/// Built in one place so the three can never disagree. A caller that computed
/// the oid from one byte string and the digest from another would produce a
/// commit that passes every check at write time and fails to recover.
#[derive(Clone)]
pub struct PreparedCapsule {
    pub bytes: Vec<u8>,
    pub object_id: ObjectId,
    pub template_digest: Digest,
}

impl core::fmt::Debug for PreparedCapsule {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedCapsule")
            .field("bytes_len", &self.bytes.len())
            .field("bytes", &"[REDACTED]")
            .field("object_id", &self.object_id)
            .field("template_digest", &self.template_digest)
            .finish()
    }
}

/// The digest a marker declares for its template — a plain hash of the exact
/// canonical bytes the capsule holds.
pub fn template_digest(bytes: &[u8]) -> Digest {
    let mut hasher = fgdb_crypto::Hasher::new();
    hasher.update(TEMPLATE_DIGEST_DOMAIN);
    hasher.update(bytes);
    hasher.finalize()
}

/// Prepare a template for commit: encode it, identify it, digest it.
///
/// Takes the two key primitives rather than a [`DatabaseKeys`] because that is
/// all it needs, and because the verification layer calls this too. Coupling a
/// shared helper to the embedded API's key struct would force every caller to
/// build one just to hash some bytes.
pub fn prepare_capsule(
    k_oid: &[u8; 32],
    namespace: DatabaseSecurityNamespaceId,
    template: &LogicalDeltaTemplate,
) -> Result<PreparedCapsule, CanonicalError> {
    let bytes = template.canonical_bytes()?;
    // The §5.1 keyed identity over the same bytes the capsule will hold. The
    // header is empty because the canonical bytes ARE the whole object — the
    // transcript concatenates header and payload, so passing the bytes as the
    // payload reproduces exactly the intended stream.
    let identified = IdentifiedObject::new(k_oid, namespace, CAPSULE_OBJECT_KIND, &[], &bytes);
    Ok(PreparedCapsule {
        object_id: identified.object_id(),
        template_digest: template_digest(&bytes),
        bytes,
    })
}

/// Build the marker for a prepared capsule at an allocated sequence.
///
/// The marker's `capsule_ref` and `logical_delta_template_digest` both come from
/// the same [`PreparedCapsule`], so the write-time cross-check and the
/// recovery-time cross-check are asking about the same object by construction.
pub fn marker_for_capsule(
    commit_seq: u64,
    capsule_oid: ObjectId,
    capsule: &PreparedCapsule,
    head_updates: Vec<HeadUpdate>,
) -> CommitMarker {
    CommitMarker {
        logical_command_seq: commit_seq,
        commit_seq,
        effect_source: EffectSource::Local {
            capsule_ref: capsule_oid,
            logical_delta_template_digest: capsule.template_digest,
        },
        prev_global: None,
        head_updates,
        merge_record_oid: None,
        coordinate_schema_transition_digest: Digest([0u8; 32]),
        topology_epoch: 1,
        policy_epoch: 1,
        revocation_index: 0,
        txn_token: [0u8; 16],
        commit_hlc: commit_seq,
        final_effect_digest: capsule.template_digest,
        authorization_decision_digest: Digest([0u8; 32]),
        resource_effect_digest: Digest([0u8; 32]),
        payload_availability_certificate_oid: None,
        flags: 0,
    }
}

/// **Rebuild the derived partition from the durable commit stream.**
///
/// Walks the recovered marker chain in commit order, proves each capsule's bytes
/// are the ones its marker committed to, folds the rows into a tier-D writer,
/// publishes blocks and a root, and re-reads them from the store.
///
/// Re-reading rather than returning the writer's own entries is deliberate: it
/// means every snapshot a reader is served from has made the full round trip
/// through encode, content-address, fsync and decode, so an encoder/decoder
/// disagreement cannot hide behind in-memory state.
///
/// Only markers reach this loop, so an orphan capsule — bytes on disk that no
/// marker names — contributes nothing without needing to be excluded. That is
/// the marker-is-the-commit rule doing the work.
/// Derive the version map from a resolved partition alone (fgdb-ge6a v3):
/// fold each LIVE element's durable statement chain, oldest first. Spent
/// counts ride along because every create spent exactly one identity, which
/// is also what the birth-ordinal allocator counted.
fn derive_versions_and_ordinal(
    blocks: &[Vec<AdjacencyEntry>],
    block_props: &[Option<BlockProps>],
    patches: &[VertexPatchRows],
    frontier: CommitSeq,
) -> Result<(std::collections::BTreeMap<ElementId, ObjectId>, u64), CanonicalError> {
    let mut versions = std::collections::BTreeMap::new();

    // Vertices: statements keyed (vid, created_at), later patches restate.
    let mut vertex_statements: std::collections::BTreeMap<(VId, u64), &VertexRow> =
        std::collections::BTreeMap::new();
    for rows in patches {
        for row in rows {
            vertex_statements.insert((row.vid, row.created_at.0), row);
        }
    }
    let mut spent_vertices = std::collections::BTreeSet::new();
    let mut head: Option<(VId, ObjectId, bool)> = None;
    for ((vid, _), row) in &vertex_statements {
        spent_vertices.insert(*vid);
        let previous = match &head {
            Some((prev_vid, version, _)) if prev_vid == vid => Some(*version),
            _ => None,
        };
        let transcript =
            vertex_statement_transcript(row.vid, row.birth_ordinal, &row.labels, &row.props)?;
        let version = statement_successor(previous, &transcript);
        let live =
            row.retired_at.is_none_or(|r| r.0 > frontier.0) && row.created_at.0 <= frontier.0;
        head = Some((*vid, version, live));
        if live {
            versions.insert(ElementId::Vertex(*vid), version);
        } else {
            versions.remove(&ElementId::Vertex(*vid));
        }
    }

    // Edges: statements keyed (eid, created_at) across publication order,
    // later blocks restate (tombstone supersede).
    let mut edge_statements: std::collections::BTreeMap<
        (EId, u64),
        (AdjacencyEntry, EdgePropertyRow),
    > = std::collections::BTreeMap::new();
    for (block, props) in blocks.iter().zip(block_props) {
        for (index, entry) in block.iter().enumerate() {
            let row = props
                .as_ref()
                .map(|props| props.props_of(index))
                .unwrap_or_default();
            edge_statements.insert((entry.eid, entry.created_at.0), (*entry, row));
        }
    }
    let mut spent_edges = std::collections::BTreeSet::new();
    let mut head: Option<(EId, ObjectId)> = None;
    for ((eid, _), (entry, row)) in &edge_statements {
        spent_edges.insert(*eid);
        let previous = match &head {
            Some((prev_eid, version)) if prev_eid == eid => Some(*version),
            _ => None,
        };
        let transcript =
            edge_statement_transcript(*eid, entry.src, entry.relation, entry.dst, row)?;
        let version = statement_successor(previous, &transcript);
        head = Some((*eid, version));
        let live =
            entry.retired_at.is_none_or(|r| r.0 > frontier.0) && entry.created_at.0 <= frontier.0;
        if live {
            versions.insert(ElementId::Edge(*eid), version);
        } else {
            versions.remove(&ElementId::Edge(*eid));
        }
    }

    Ok((versions, (spent_vertices.len() + spent_edges.len()) as u64))
}

/// Post-verification checkpoint reopen (fgdb-ge6a): resolve the slot's
/// manifest to a partition, reopen it from disk, derive the writer and version
/// state the fold would have built, then replay only the Chronicle suffix past
/// the partition's publication. The manifest record's chain binding has
/// already been verified against the recovered marker chain (fgdb-90hw) —
/// one comparison, no prefix fold — so this path is O(partition + suffix).
/// WHAT the checkpoint contains is pinned by the equivalence law against
/// [`rebuild`] on generated histories, including the element-version heads.
async fn reopen_from_verified_checkpoint<V: Vfs>(
    cx: &CommitCx,
    coordinator: &CommitCoordinator<V>,
    store: &BlockStore<V>,
    keys: &DatabaseKeys,
    root_id: PartitionRootVersion,
    crypto_verification_events: &mut Vec<CryptoVerificationEvent>,
) -> Result<OpenedGeneration, RebuildError> {
    // The sealed lists a retained writer holds come from the same verified
    // reads as the decoded state.
    let fgdb_strata::store::ReopenedPartition {
        root,
        blocks,
        block_props,
        patches,
        sealed_blocks,
        sealed_patches,
        admission,
    } = store.reopen_sealed(cx, root_id).await?;
    let published_at = root.published_at;

    let mut writer = BlockWriter::from_published_partition(
        GRAPH,
        BRANCH,
        PARTITION,
        sealed_blocks,
        sealed_patches,
        &blocks,
        &block_props,
        &patches,
        published_at,
    )
    .map_err(|error| RebuildError::Store(StoreError::MalformedRoot(error)))?;
    let (mut versions, mut next_birth_ordinal) =
        derive_versions_and_ordinal(&blocks, &block_props, &patches, published_at).map_err(
            |error| RebuildError::Decode {
                commit_seq: published_at.0,
                error,
            },
        )?;

    // The SUFFIX: everything the crash window or plain lag left past the
    // resolved publication.
    let frontier = fold_stream(
        cx,
        coordinator,
        keys,
        &mut FoldState {
            writer: &mut writer,
            versions: &mut versions,
            next_birth_ordinal: &mut next_birth_ordinal,
            crypto_verification_events,
        },
        published_at,
    )
    .await?;

    let heads = WriteHeads {
        versions,
        next_birth_ordinal,
    };
    if frontier.0 > published_at.0 {
        // The suffix advanced the fold: republish through the shared tail so
        // the durable root/manifest catch up (the slot heals in bind). That
        // publication syncs every object again and earns its own receipts.
        let published_chain_hash = chain_commitment_at(coordinator.chain(), frontier)
            .expect("the fold's frontier is on the recovered chain it folded");
        let (snapshot, writer, receipts) =
            publish_and_snapshot(cx, store, keys, writer, frontier, published_chain_hash).await?;
        return Ok((snapshot, writer, heads, receipts));
    }

    // No suffix: the partition IS current, and the snapshot assembles from
    // what the reopen already decoded — no publish, no O(blocks) writes.
    let snapshot = current_generation(
        keys,
        coordinator.chain(),
        root_id,
        root,
        blocks,
        block_props,
        patches,
    );
    // A writable handle extends the adjacency index on every commit, so it
    // is built with the generation rather than on first read.
    snapshot.adjacency_index();
    // The root slot selected this root, so publication already made every
    // object it names durable; the reopen just admitted each one. Owner
    // ruling 2026-10-06 (fgdb-ibbuq, "trust publication"): seed the receipts
    // from that admission instead of re-syncing all of them on the first
    // commit. A copy made without syncing must be adopted before this open
    // ([`Database::adopt`]).
    let receipts = PublishReceipts::for_published_root(admission);
    Ok((snapshot, writer, heads, receipts))
}

/// What an open derives: the generation readers share, the retained fold, the
/// write heads beside it, and the receipts for what is already published.
type OpenedGeneration = (Snapshot, BlockWriter, WriteHeads, PublishReceipts);

/// What a publication yields: the generation, the retained fold, and the
/// receipts it earned.
type PublishedGeneration = (Snapshot, BlockWriter, PublishReceipts);

/// Refuse a `path` that does not hold a database before anything is opened
/// under it. Shared by the writable and the read-only open; see
/// [`OpenError::NotADatabase`] for why this cannot simply delegate to
/// `CommitCoordinator::open`.
async fn require_database_dir<V: Vfs>(
    cx: &CommitCx,
    vfs: &V,
    path: &Path,
) -> Result<(), OpenError> {
    match cx.with_restriction_async(vfs.symlink_metadata(path)).await {
        Ok(metadata) if !metadata.file_type().is_dir() => {
            return Err(OpenError::NotADirectory {
                path: path.to_path_buf(),
            });
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(OpenError::NotADatabase {
                path: path.to_path_buf(),
                missing: "the directory itself",
            });
        }
        Err(error) => return Err(OpenError::Io(error)),
    }
    match cx
        .with_restriction_async(vfs.symlink_metadata(&path.join(CAPSULE_DIR)))
        .await
    {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
        Ok(_) => Err(OpenError::NotADatabase {
            path: path.to_path_buf(),
            missing: CAPSULE_DIR,
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(OpenError::NotADatabase {
                path: path.to_path_buf(),
                missing: CAPSULE_DIR,
            })
        }
        Err(error) => Err(OpenError::Io(error)),
    }
}

/// The Strata block store under `path`, opened through the handle's one
/// filesystem and carrying the keys' scalar resolver when they have one.
async fn open_block_store<V: Vfs + Clone>(
    cx: &CommitCx,
    vfs: &V,
    path: &Path,
    keys: &DatabaseKeys,
) -> Result<BlockStore<V>, OpenError> {
    let store =
        BlockStore::open_with_vfs(cx, vfs.clone(), path, keys.k_oid.clone(), keys.namespace)
            .await?;
    Ok(match &keys.scalar_resolver {
        Some(resolver) => store.with_scalar_resolver(resolver.clone()),
        None => store,
    })
}

/// The checkpoint a root slot selects, once verified (fgdb-ge6a).
struct SelectedCheckpoint {
    root_id: PartitionRootVersion,
    published_at: CommitSeq,
}

/// Select the checkpoint the root slot names, accepting it only when the
/// manifest describes the spine and its record is bound to the recovered
/// chain (fgdb-90hw). `None` when there is no slot file: an interrupted
/// create, which the caller rebuilds from the stream. A present slot that is
/// foreign, malformed, or unaccountable refuses rather than being silently
/// rebuilt over. The writable open and the read-only open both select here, so
/// neither accepts a root the other refuses.
async fn select_checkpoint<V: Vfs>(
    cx: &CommitCx,
    coordinator: &CommitCoordinator<V>,
    store: &BlockStore<V>,
    probe: &RootStore<V>,
    keys: &DatabaseKeys,
    path: &Path,
) -> Result<Option<SelectedCheckpoint>, OpenError> {
    let slot = match probe.current(cx).await {
        Ok(slot) => slot,
        Err(SlotStoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) => return Err(OpenError::Slot(error)),
    };
    validate_plain_slot(&slot, keys, path)?;
    let disagrees = || OpenError::SlotDisagreesWithStream {
        path: path.to_path_buf(),
        slot_manifest: ObjectId(slot.root_manifest_oid),
    };
    let claimed = ManifestVersion(ObjectId(slot.root_manifest_oid));
    let resolved = match store.resolve_manifest(cx, claimed).await {
        Ok(resolved) if resolved.len() == 1 => resolved,
        _ => return Err(disagrees()),
    };
    let (record, root) = &resolved[0];
    let describes_spine = record.graph == GRAPH
        && record.branch == BRANCH
        && record.partition == PARTITION
        && root.graph == GRAPH
        && root.branch == BRANCH
        && root.partition == PARTITION;
    // THE CHAIN BINDING (fgdb-90hw): the record claims "the history whose
    // chain at published_at hashes to exactly this published my root", and the
    // recovered chain is the judge — one comparison, no capsule folding. A
    // future-frontier root falls off the chain (None); a same-namespace FOREIGN
    // history hashes differently; a lagging root matches at its own seq and
    // heals in bind. WHAT was published stays the equivalence law's question —
    // this binding answers WHO published it.
    let bound = chain_commitment_at(coordinator.chain(), root.published_at)
        .is_some_and(|expected| expected == record.published_chain_hash);
    if !describes_spine || !bound {
        return Err(disagrees());
    }
    Ok(Some(SelectedCheckpoint {
        root_id: record.root,
        published_at: root.published_at,
    }))
}

/// The readable generation of a verified partition that is current with the
/// recovered chain: the decoded state, its derived indexes, and the manifest
/// identity re-derived from the one record that publishes it. Its delta window
/// starts empty; each caller states what history it retains.
fn current_generation(
    keys: &DatabaseKeys,
    chain: &fgdb_chronicle::MarkerChain,
    root_id: PartitionRootVersion,
    root: fgdb_strata::root::PartitionRoot,
    blocks: Vec<Vec<AdjacencyEntry>>,
    block_props: Vec<Option<BlockProps>>,
    patches: Vec<VertexPatchRows>,
) -> Snapshot {
    let published_at = root.published_at;
    let published_chain_hash = chain_commitment_at(chain, published_at)
        .expect("select_checkpoint bound this publication to the recovered chain");
    let manifest_records = records_of(&[(root.clone(), root_id, published_chain_hash)])
        .expect("one root is one canonical record");
    let manifest_bytes =
        encode_manifest(&manifest_records).expect("records_of proved these records canonical");
    let manifest = ManifestVersion(fgdb_strata::manifest::manifest_id(
        keys.k_oid(),
        keys.namespace,
        &manifest_bytes,
    ));
    Snapshot {
        // Built on first use; a writable caller forces it at once.
        adjacency: std::sync::OnceLock::new(),
        property_index: Arc::new(gql_exec::source::PropertyEqualityIndex::build(&patches)),
        blocks,
        refs: root.blocks,
        block_props,
        patches,
        patch_refs: root.vertex_patches,
        frontier: published_at,
        root: root_id,
        manifest,
        delta_index: LocalDeltaBatchIndex::new(),
    }
}

async fn rebuild<V: Vfs>(
    cx: &CommitCx,
    coordinator: &CommitCoordinator<V>,
    store: &BlockStore<V>,
    keys: &DatabaseKeys,
    crypto_verification_events: &mut Vec<CryptoVerificationEvent>,
) -> Result<OpenedGeneration, RebuildError> {
    let mut writer = BlockWriter::new(GRAPH, BRANCH, PARTITION);
    let mut next_birth_ordinal = 0u64;
    let mut versions = std::collections::BTreeMap::new();
    let frontier = fold_stream(
        cx,
        coordinator,
        keys,
        &mut FoldState {
            writer: &mut writer,
            versions: &mut versions,
            next_birth_ordinal: &mut next_birth_ordinal,
            crypto_verification_events,
        },
        CommitSeq(0),
    )
    .await?;
    let published_chain_hash = chain_commitment_at(coordinator.chain(), frontier)
        .expect("the fold's frontier is on the recovered chain it folded");
    let (snapshot, writer, receipts) =
        publish_and_snapshot(cx, store, keys, writer, frontier, published_chain_hash).await?;
    Ok((
        snapshot,
        writer,
        WriteHeads {
            versions,
            next_birth_ordinal,
        },
        receipts,
    ))
}

/// Retain a marker-authenticated boundary without manufacturing a delta batch.
fn empty_delta_window_at<V: Vfs>(
    cx: &CommitCx,
    coordinator: &CommitCoordinator<V>,
    at: CommitSeq,
) -> Result<LocalDeltaBatchIndex, RebuildError> {
    if at == CommitSeq::ORIGIN {
        return Ok(LocalDeltaBatchIndex::new());
    }
    let entries = coordinator.chain().entries();
    let position = entries.partition_point(|entry| entry.marker.commit_seq < at.0);
    let entry = entries
        .get(position)
        .filter(|entry| entry.marker.commit_seq == at.0)
        .ok_or(CommitError::ChainDiverged { commit_seq: at.0 })?;
    let EffectSource::Local {
        logical_delta_template_digest,
        ..
    } = &entry.marker.effect_source;
    LocalDeltaBatchIndex::empty_at_committed(
        CommittedMarker::attest(
            MarkerRef {
                marker_oid: entry.marker_oid,
                commit_seq: at,
            },
            cx,
        ),
        logical_delta_template_digest.0,
    )
    .map_err(|error| RebuildError::Index {
        commit_seq: at.0,
        error,
    })
}

/// The single authenticated capsule-to-delta reconstruction path used by
/// eager recovery and lazy prefix admission alike.
async fn read_delta_batch<V: Vfs>(
    cx: &CommitCx,
    coordinator: &CommitCoordinator<V>,
    keys: &DatabaseKeys,
    entry: &fgdb_chronicle::marker::ChainedMarker,
    crypto_verification_events: &mut Vec<CryptoVerificationEvent>,
) -> Result<LogicalDeltaBatch, RebuildError> {
    cx.checkpoint().map_err(RebuildError::Interrupted)?;
    let commit_seq = CommitSeq(entry.marker.commit_seq);
    let EffectSource::Local {
        capsule_ref,
        logical_delta_template_digest,
    } = &entry.marker.effect_source;
    if !coordinator.capsule_exists(cx, *capsule_ref).await {
        return Err(RebuildError::MissingCapsule {
            commit_seq: commit_seq.0,
            capsule_oid: *capsule_ref,
        });
    }
    let bytes = coordinator
        .read_capsule(cx, *capsule_ref, crypto_verification_events)
        .await?;
    let recomputed = template_digest(&bytes);
    // ubs:ignore -- non-secret content digest over local capsule bytes, not authentication material.
    if recomputed != *logical_delta_template_digest {
        return Err(RebuildError::TemplateDigestMismatch {
            commit_seq: commit_seq.0,
            declared: *logical_delta_template_digest,
            recomputed,
        });
    }
    let template = keys
        .decode_template(&bytes)
        .map_err(|error| RebuildError::Decode {
            commit_seq: commit_seq.0,
            error,
        })?;
    Ok(LogicalDeltaBatch::order(
        &template,
        logical_delta_template_digest.0,
        CommittedMarker::attest(
            MarkerRef {
                marker_oid: entry.marker_oid,
                commit_seq,
            },
            cx,
        ),
    ))
}

/// Rebuild the derived delta window from the FULL recovered marker chain.
///
/// Used when no admitted checkpoint was selected or the caller explicitly
/// forces stream recovery. Ordinary checkpoint open instead retains an
/// authenticated floor and reconstructs older batches on demand. The index
/// is never a second source of truth (FG-INV-18): every row is derived from
/// the exact capsule named by its recovered marker.
async fn rebuild_delta_index<V: Vfs>(
    cx: &CommitCx,
    coordinator: &CommitCoordinator<V>,
    keys: &DatabaseKeys,
    crypto_verification_events: &mut Vec<CryptoVerificationEvent>,
) -> Result<LocalDeltaBatchIndex, RebuildError> {
    let mut index = LocalDeltaBatchIndex::new();
    for entry in coordinator.chain().entries() {
        let commit_seq = CommitSeq(entry.marker.commit_seq);
        let batch =
            read_delta_batch(cx, coordinator, keys, entry, crypto_verification_events).await?;
        index.insert(batch).map_err(|error| RebuildError::Index {
            commit_seq: commit_seq.0,
            error,
        })?;
    }
    Ok(index)
}

/// Is `(src, relation, dst)` live in the batch prefix or the retained fold?
fn triple_is_live(
    writer: &BlockWriter,
    prefix_edges: &std::collections::BTreeMap<EId, (VId, VId)>,
    prefix_deleted_edges: &std::collections::BTreeSet<EId>,
    src: VId,
    dst: VId,
    relation: RelationId,
) -> bool {
    for (eid, (prefix_src, prefix_dst)) in prefix_edges {
        if !prefix_deleted_edges.contains(eid) && *prefix_src == src && *prefix_dst == dst {
            return true;
        }
    }
    for eid in writer.live_incident_edges(src) {
        #[cfg(test)]
        commit_growth_laws::INCIDENT_VISITS.with(|count| count.set(count.get() + 1));
        if prefix_deleted_edges.contains(&eid) {
            continue;
        }
        if let Some((live_src, live_relation, live_dst, _)) = writer.live_edge(eid)
            && live_src == src
            && live_dst == dst
            && live_relation == relation
        {
            return true;
        }
    }
    false
}

/// Fold every committed template with `commit_seq > after` into the writer,
/// versions map, and birth-ordinal allocator — the one stream fold shared by
/// the from-scratch rebuild (`after = 0`) and the selected checkpoint's suffix
/// replay past `published_at` (fgdb-ge6a).
struct FoldState<'a> {
    writer: &'a mut BlockWriter,
    versions: &'a mut std::collections::BTreeMap<ElementId, ObjectId>,
    next_birth_ordinal: &'a mut u64,
    crypto_verification_events: &'a mut Vec<CryptoVerificationEvent>,
}

async fn fold_stream<V: Vfs>(
    cx: &CommitCx,
    coordinator: &CommitCoordinator<V>,
    keys: &DatabaseKeys,
    state: &mut FoldState<'_>,
    after: CommitSeq,
) -> Result<CommitSeq, RebuildError> {
    let mut frontier = after;
    let mut touched: std::collections::BTreeSet<ElementId> = std::collections::BTreeSet::new();

    for entry in coordinator.chain().entries() {
        let commit_seq = CommitSeq(entry.marker.commit_seq);
        if commit_seq.0 <= after.0 {
            continue;
        }
        frontier = commit_seq;
        let EffectSource::Local {
            capsule_ref,
            logical_delta_template_digest,
        } = &entry.marker.effect_source;

        if !coordinator.capsule_exists(cx, *capsule_ref).await {
            return Err(RebuildError::MissingCapsule {
                commit_seq: commit_seq.0,
                capsule_oid: *capsule_ref,
            });
        }
        let bytes = coordinator
            .read_capsule(cx, *capsule_ref, state.crypto_verification_events)
            .await?;
        let recomputed = template_digest(&bytes);
        // FG-INV-09's recompute-from-registered-bytes check. Skipping it would
        // turn silent corruption into silently different graph state, which is
        // the whole failure a content-addressed store exists to prevent.
        //
        // The annotation below must stay on the line IMMEDIATELY above the
        // comparison: UBS anchors it to the next line, so prose between the two
        // silently un-suppresses the finding (measured — a four-line comment
        // with the annotation on top still reported the critical).
        // ubs:ignore -- non-secret content digest over local capsule bytes, not authentication material.
        if recomputed != *logical_delta_template_digest {
            return Err(RebuildError::TemplateDigestMismatch {
                commit_seq: commit_seq.0,
                declared: *logical_delta_template_digest,
                recomputed,
            });
        }
        let template = keys
            .decode_template(&bytes)
            .map_err(|error| RebuildError::Decode {
                commit_seq: commit_seq.0,
                error,
            })?;

        for coordinate in template.coordinate_entries() {
            if (coordinate.graph, coordinate.branch) != (GRAPH, BRANCH) {
                continue;
            }
            for row in &coordinate.rows {
                if matches!(
                    row,
                    DeltaRow::CreateVertex { .. } | DeltaRow::CreateEdge { .. }
                ) {
                    *state.next_birth_ordinal += 1;
                }
                state
                    .writer
                    .apply(keys.block_keys(), commit_seq, row)
                    .map_err(|error| RebuildError::Fold {
                        commit_seq: commit_seq.0,
                        error,
                    })?;
                touched_elements(row, &mut touched);
            }
        }
        fold_statement_versions(state.versions, &touched, state.writer).map_err(|error| {
            RebuildError::Decode {
                commit_seq: commit_seq.0,
                error,
            }
        })?;
        touched.clear();
        // THE PER-COMMIT SEAL LAW (fgdb-ge6a): every commit's statements seal
        // at that commit, so the durable layout is a function of the STREAM —
        // never of which writer happened to hold unsealed rows. Without this,
        // a retained writer re-coalesces pending rows across commits and a
        // checkpoint-selected open (which can only see SEALED durable state)
        // would republish a different — equally lawful, but not identical —
        // root, breaking the reopening-publishes-the-same-root determinism law.
        state
            .writer
            .seal(keys.block_keys())
            .map_err(|error| RebuildError::Fold {
                commit_seq: commit_seq.0,
                error,
            })?;
        state
            .writer
            .seal_vertices(keys.block_keys())
            .map_err(|error| RebuildError::Fold {
                commit_seq: commit_seq.0,
                error,
            })?;
    }
    Ok(frontier)
}

/// The publication tail every open path shares: publish from a clone, make
/// the blocks/patches/root/manifest durable, and assemble the snapshot from
/// a from-disk reopen — the encode -> address -> fsync -> decode round trip.
/// The receipts this publication earned come back with it, so the handle's
/// first commit does not re-sync what this call just synced (fgdb-ibbuq).
///
/// Type-erased because open, recovery and compaction all end here: a caller's
/// `Send` proof stops at `dyn Future + Send` instead of descending through
/// Strata publication (fgdb-a5y6m).
fn publish_and_snapshot<'a, V: Vfs>(
    cx: &'a CommitCx,
    store: &'a BlockStore<V>,
    keys: &'a DatabaseKeys,
    writer: BlockWriter,
    frontier: CommitSeq,
    published_chain_hash: Digest,
) -> SendFuture<'a, Result<PublishedGeneration, RebuildError>> {
    Box::pin(publish_and_snapshot_inner(
        cx,
        store,
        keys,
        writer,
        frontier,
        published_chain_hash,
    ))
}

async fn publish_and_snapshot_inner<V: Vfs>(
    cx: &CommitCx,
    store: &BlockStore<V>,
    keys: &DatabaseKeys,
    writer: BlockWriter,
    frontier: CommitSeq,
    published_chain_hash: Digest,
) -> Result<PublishedGeneration, RebuildError> {
    // Publish from a clone and hand the fold state back: the caller retains it
    // so later commits fold only their own template (fgdb-fujt). The strata
    // equality law pins clone-publish == this very rebuild, byte for byte.
    let (root, blocks, patches) = writer
        .clone()
        .publish(keys.block_keys(), frontier)
        .map_err(|error| RebuildError::Fold {
            commit_seq: frontier.0,
            error,
        })?;
    // The commit path's publisher: data objects are staged and synced
    // together (BATCH_SYNCS_IN_FLIGHT at a time, one directory barrier per
    // flush), each earning a receipt by passing decode and history admission
    // as it is written, so root admission needs no second read of every
    // object. Per-object puts cost a file and a directory sync each, in
    // series, and put_root then re-read every block: a 900k-edge compaction
    // was 798 s of wall for 108 s of CPU. Nothing is reachable until the
    // root slot names the manifest, which the caller publishes after this.
    let mut receipts = fgdb_strata::store::PublishReceipts::default();
    let mut batch = store.publication_batch(cx, &mut receipts, None)?;
    for block in &blocks {
        batch
            .put_verified(
                cx,
                &block.bytes,
                block
                    .property_patch
                    .as_ref()
                    .map(|patch| patch.bytes.as_slice()),
            )
            .await?;
    }
    batch.flush(cx).await?;
    for patch in &patches {
        batch.put_patch_verified(cx, &patch.bytes).await?;
    }
    batch.finish(cx).await?;
    let verified_root = store.verify_root(cx, &root, &mut receipts).await?;
    let root_id = verified_root.id();
    let manifest_records = records_of(&[(root.clone(), root_id, published_chain_hash)])
        .expect("one root is one canonical record");
    let (manifest, _) = store
        .publish_root_and_manifest(cx, verified_root, &manifest_records, &mut receipts)
        .await?;
    let (reopened_root, decoded, decoded_props, decoded_patches) =
        store.reopen(cx, root_id).await?;

    Ok((
        Snapshot {
            adjacency: std::sync::OnceLock::from(Arc::new(
                gql_exec::source::AdjacencyIndex::build(&decoded),
            )),
            property_index: Arc::new(gql_exec::source::PropertyEqualityIndex::build(
                &decoded_patches,
            )),
            blocks: decoded,
            refs: reopened_root.blocks,
            block_props: decoded_props,
            patches: decoded_patches,
            patch_refs: reopened_root.vertex_patches,
            frontier,
            root: root_id,
            manifest,
            delta_index: LocalDeltaBatchIndex::new(),
        },
        writer,
        receipts,
    ))
}

#[cfg(test)]
mod lazy_delta_laws {
    use super::*;
    use asupersync::fs::{Metadata, OpenOptions, Permissions, ReadDir};
    use asupersync::lab::run_async_under_lab;
    use fgdb_types::PurposeContexts;
    use std::io;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Count capsule read opens/whole-file reads through the VFS boundary.
    /// Refusals are injected only when requested, without altering disk bytes.
    #[derive(Clone, Debug)]
    struct CountingVfs {
        memory: MemVfs,
        reads: Arc<AtomicUsize>,
        refuse_at: Arc<AtomicUsize>,
    }

    impl CountingVfs {
        fn new() -> Self {
            Self {
                memory: MemVfs::new().unwrap(),
                reads: Arc::new(AtomicUsize::new(0)),
                refuse_at: Arc::new(AtomicUsize::new(0)),
            }
        }
        fn count(&self) -> usize {
            self.reads.load(Ordering::SeqCst)
        }
        fn reset(&self) {
            self.reads.store(0, Ordering::SeqCst);
        }
        fn observe(&self, path: &Path) -> io::Result<()> {
            if path
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name == "capsules")
            {
                let count = self.reads.fetch_add(1, Ordering::SeqCst) + 1;
                if self.refuse_at.load(Ordering::SeqCst) == count {
                    return Err(io::Error::other("injected capsule read refusal"));
                }
            }
            Ok(())
        }
    }

    impl Vfs for CountingVfs {
        type File = MemVfsFile;

        async fn open(&self, path: &Path, options: &OpenOptions) -> io::Result<Self::File> {
            self.memory.open(path, options).await
        }
        async fn open_read(&self, path: &Path) -> io::Result<Self::File> {
            self.observe(path)?;
            self.memory.open_read(path).await
        }
        async fn metadata(&self, path: &Path) -> io::Result<Metadata> {
            self.memory.metadata(path).await
        }
        async fn symlink_metadata(&self, path: &Path) -> io::Result<Metadata> {
            self.memory.symlink_metadata(path).await
        }
        async fn set_permissions(&self, path: &Path, permissions: Permissions) -> io::Result<()> {
            self.memory.set_permissions(path, permissions).await
        }
        async fn create_dir(&self, path: &Path) -> io::Result<()> {
            self.memory.create_dir(path).await
        }
        async fn create_dir_all(&self, path: &Path) -> io::Result<()> {
            self.memory.create_dir_all(path).await
        }
        async fn remove_dir(&self, path: &Path) -> io::Result<()> {
            self.memory.remove_dir(path).await
        }
        async fn remove_file(&self, path: &Path) -> io::Result<()> {
            self.memory.remove_file(path).await
        }
        async fn read_dir(&self, path: &Path) -> io::Result<ReadDir> {
            self.memory.read_dir(path).await
        }
        async fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
            self.memory.remove_dir_all(path).await
        }
        async fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.memory.rename(from, to).await
        }
        async fn copy(&self, from: &Path, to: &Path) -> io::Result<u64> {
            self.memory.copy(from, to).await
        }
        async fn hard_link(&self, original: &Path, link: &Path) -> io::Result<()> {
            self.memory.hard_link(original, link).await
        }
        async fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
            self.memory.canonicalize(path).await
        }
        async fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
            self.memory.read_link(path).await
        }
        async fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
            self.observe(path)?;
            self.memory.read(path).await
        }
        async fn read_to_string(&self, path: &Path) -> io::Result<String> {
            self.observe(path)?;
            self.memory.read_to_string(path).await
        }
        async fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
            self.memory.write(path, bytes).await
        }
    }

    fn keys() -> DatabaseKeys {
        DatabaseKeys::new(
            [0xa6; 32],
            DatabaseSecurityNamespaceId([0xa7; 32]),
            [0xa8; 32],
        )
    }

    fn create(id: u128) -> WriteBatch {
        let mut batch = WriteBatch::new(RelationId(1));
        batch.create_vertex(
            VId(id),
            vec![],
            vec![(PropertyKeyId(1), CanonicalScalar::Int(10))],
        );
        batch
    }

    #[test]
    fn checkpoint_open_reads_zero_capsules_and_materializes_only_requested_prefixes() {
        let ((), report) = run_async_under_lab(0xa671, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            for count in [8_u64, 384] {
                let vfs = CountingVfs::new();
                let path = vfs.memory.database_dir();
                let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys())
                    .await
                    .unwrap();
                for id in 1..=count {
                    db.write(&cx, create(u128::from(id))).await.unwrap();
                }
                let expected = db.delta_index().unwrap().clone();
                drop(db);
                vfs.reset();
                let mut db = Database::open_with_vfs(&cx, vfs.clone(), &path, keys())
                    .await
                    .unwrap();
                assert_eq!(vfs.count(), 0, "checkpoint open must not read any capsule");
                assert!(db.vertex(VId(u128::from(count))).unwrap().is_some());
                assert_eq!(
                    db.allocate_identity(
                        &contexts.query(),
                        fgdb_gql::insertion::GraphInsertRequest::Vertex { row: 0, vertex: 0 }
                    )
                    .unwrap(),
                    ElementId::Vertex(VId(u128::from(count) + 1))
                );
                assert_eq!(
                    vfs.count(),
                    0,
                    "point reads and identity allocation need no history I/O"
                );
                let pinned = db.pinned_read_view().unwrap();
                let after = CommitSeq(count - 2);
                assert!(
                    matches!(db.delta_since(after), Err(ReadError::DeltaCursorRetired { retained_after, .. }) if retained_after == CommitSeq(count))
                );
                assert_eq!(db.delta_since(CommitSeq(count)).unwrap().count(), 0);
                db.ensure_delta_window(&cx, after).await.unwrap();
                assert_eq!(vfs.count(), 2, "only the two missing capsules are loaded");
                assert_eq!(
                    db.delta_since(after).unwrap().collect::<Vec<_>>(),
                    expected.since(after).unwrap().collect::<Vec<_>>()
                );
                assert!(
                    pinned.delta_since(after).is_err(),
                    "an issued immutable view keeps its original window"
                );
                db.ensure_delta_window(&cx, after).await.unwrap();
                assert_eq!(
                    vfs.count(),
                    2,
                    "an already materialized cut performs no I/O"
                );
                db.write(&cx, create(u128::from(count) + 1)).await.unwrap();
                vfs.reset();
                db.ensure_delta_window(&cx, CommitSeq::ORIGIN)
                    .await
                    .unwrap();
                assert_eq!(vfs.count(), usize::try_from(count - 2).unwrap());
                assert_eq!(
                    db.delta_since(CommitSeq::ORIGIN)
                        .unwrap()
                        .take(count as usize)
                        .collect::<Vec<_>>(),
                    expected.iter().collect::<Vec<_>>()
                );
                assert_eq!(db.delta_frontier().unwrap(), CommitSeq(count + 1));
                assert_eq!(db.delta_index().unwrap().len(), count as usize + 1);
                db.delta_index().unwrap().verify().unwrap();
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn failed_prefix_admission_is_atomic_retryable_and_cannot_rehydrate_retirement() {
        let ((), report) = run_async_under_lab(0xa672, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let vfs = CountingVfs::new();
            let path = vfs.memory.database_dir();
            let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys())
                .await
                .unwrap();
            for id in 1..=4 {
                db.write(&cx, create(id)).await.unwrap();
            }
            drop(db);
            let mut db = Database::open_with_vfs(&cx, vfs.clone(), &path, keys())
                .await
                .unwrap();
            let before = Arc::clone(&db.snapshot);
            vfs.reset();
            vfs.refuse_at.store(2, Ordering::SeqCst);
            assert!(
                db.ensure_delta_window(&cx, CommitSeq::ORIGIN)
                    .await
                    .is_err()
            );
            assert!(Arc::ptr_eq(&db.snapshot, &before));
            assert_eq!(db.delta_materialized_after, CommitSeq(4));
            assert_eq!(
                db.state(),
                DatabaseState::Healthy {
                    published_frontier: CommitSeq(4)
                }
            );
            assert!(db.vertex(VId(4)).unwrap().is_some());
            vfs.refuse_at.store(0, Ordering::SeqCst);
            vfs.reset();
            db.ensure_delta_window(&cx, CommitSeq::ORIGIN)
                .await
                .unwrap();
            assert_eq!(
                vfs.count(),
                4,
                "a failed candidate exposes no partial admitted prefix"
            );
            Arc::make_mut(&mut db.snapshot)
                .delta_index
                .retire_prefix(CommitSeq(3))
                .unwrap();
            vfs.reset();
            assert!(matches!(
                db.ensure_delta_window(&cx, CommitSeq(2)).await,
                Err(RebuildError::Index {
                    error: IndexError::CursorRetired { .. },
                    ..
                })
            ));
            assert_eq!(
                vfs.count(),
                0,
                "explicit retirement is not a lazy cache miss"
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn prepared_and_transaction_completion_reload_the_complete_conflict_interval() {
        let ((), report) = run_async_under_lab(0xa673, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            for mode in 0..4 {
                let mut db = Database::open_memory(&cx, keys()).await.unwrap();
                let basis = db.write(&cx, create(1)).await.unwrap();
                let mut change = WriteBatch::new(RelationId(1));
                change.set_vertex_property(
                    VId(1),
                    PropertyKeyId(1),
                    Some(CanonicalScalar::Int(20)),
                );
                if mode >= 2 {
                    change.ensure_vertex(VId(1), vec![], vec![]);
                }
                let mut transaction = db.begin(&contexts.txn()).unwrap();
                if mode == 3 {
                    assert!(transaction.vertex(&db, VId(1)).unwrap().is_some());
                }
                let prepared = if mode != 0 {
                    transaction.write(&mut db, change.clone()).unwrap();
                    None
                } else {
                    Some(db.prepare_write(change.clone()).unwrap())
                };
                change.set_vertex_property(
                    VId(1),
                    PropertyKeyId(1),
                    Some(CanonicalScalar::Int(30)),
                );
                db.write(&cx, change).await.unwrap();
                // Evict only derived payloads to exercise an old, same-owner
                // preparation across the same unloaded-prefix shape as open.
                // The original read basis and ownership token stay untouched.
                Arc::make_mut(&mut db.snapshot).delta_index =
                    empty_delta_window_at(&cx, &db.coordinator, CommitSeq(2)).unwrap();
                db.delta_materialized_after = CommitSeq(2);
                if let Some(prepared) = prepared {
                    assert!(matches!(
                        db.commit_prepared(&cx, prepared).await,
                        Err(WriteError::FirstCommitterWins { .. })
                    ));
                    transaction.abort();
                } else if mode == 1 {
                    assert!(matches!(
                        transaction.finish(&mut db, &cx).await,
                        Err(WriteTxnError::Write(WriteError::FirstCommitterWins { .. }))
                    ));
                } else {
                    assert!(matches!(
                        transaction
                            .commit_idempotent_rebased(&mut db, &cx, 100)
                            .await,
                        Err(WriteTxnError::Write(WriteError::FirstCommitterWins { .. }))
                    ));
                }
                assert_eq!(db.delta_materialized_after, basis);
                assert_eq!(db.frontier().unwrap(), CommitSeq(2));
                assert_eq!(
                    db.vertex(VId(1)).unwrap().unwrap().props[0].1,
                    CanonicalScalar::Int(30)
                );
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn historical_preparation_after_reopen_requires_explicit_history_and_keeps_conflicts() {
        let ((), report) = run_async_under_lab(0xa675, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let vfs = CountingVfs::new();
            let path = vfs.memory.database_dir();
            let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys())
                .await
                .unwrap();
            let basis = db.write(&cx, create(1)).await.unwrap();
            let mut update = WriteBatch::new(RelationId(1));
            update.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(20)));
            db.write(&cx, update.clone()).await.unwrap();
            drop(db);
            vfs.reset();
            let mut db = Database::open_with_vfs(&cx, vfs.clone(), &path, keys())
                .await
                .unwrap();
            assert!(matches!(
                db.prepare_write_at(basis, update.clone()),
                Err(WriteTxnError::Write(WriteError::PreparedHistory(
                    IndexError::CursorRetired { .. }
                )))
            ));
            assert_eq!(
                vfs.count(),
                0,
                "a synchronous refusal performs no hidden I/O"
            );
            db.ensure_delta_window(&cx, CommitSeq::ORIGIN)
                .await
                .unwrap();
            let prepared = db.prepare_write_at(basis, update).unwrap();
            assert_eq!(prepared.basis(), basis);
            assert!(matches!(
                db.commit_prepared(&cx, prepared).await,
                Err(WriteError::FirstCommitterWins { .. })
            ));
            assert_eq!(db.frontier().unwrap(), CommitSeq(2));
            assert_eq!(
                vfs.count(),
                2,
                "reconstruction reads the exact two committed capsules"
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn reopened_transactions_stage_at_their_exact_basis_without_loading_origin_history() {
        let ((), report) = run_async_under_lab(0xa676, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            let update = |id, value| {
                let mut batch = WriteBatch::new(RelationId(1));
                batch.set_vertex_property(
                    VId(id),
                    PropertyKeyId(1),
                    Some(CanonicalScalar::Int(value)),
                );
                batch
            };
            for recover_suffix in [false, true] {
                let vfs = CountingVfs::new();
                let path = vfs.memory.database_dir();
                let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys())
                    .await
                    .unwrap();
                let mut seed = create(1);
                seed.create_vertex(
                    VId(2),
                    vec![],
                    vec![(PropertyKeyId(1), CanonicalScalar::Int(10))],
                );
                seed.create_vertex(VId(7), vec![], vec![]);
                seed.add_edge(EId(9), VId(1), VId(7), vec![]);
                db.write(&cx, seed).await.unwrap();
                let mut retire = WriteBatch::new(RelationId(1));
                retire.delete_vertex(VId(7));
                if recover_suffix {
                    assert!(matches!(
                        db.write_with_publication_failure(
                            &cx,
                            retire,
                            DerivedPublicationStage::FoldCommittedTemplate,
                        )
                        .await,
                        Err(WriteError::CommittedNeedsRecovery { .. })
                    ));
                } else {
                    db.write(&cx, retire).await.unwrap();
                }
                drop(db);
                let mut db = Database::open_with_vfs(&cx, vfs.clone(), &path, keys())
                    .await
                    .unwrap();
                assert!(
                    db.preparation_anchor.is_none(),
                    "open itself needs no writer clone"
                );
                let basis = db.frontier().unwrap();
                assert_eq!(basis, CommitSeq(2));
                let expected = db.prepare_write(update(1, 11)).unwrap().template;
                let mut txn = db.begin(&txcx).unwrap();
                if !recover_suffix {
                    db.compact(&cx).await.unwrap();
                }
                let mut winner = update(2, 22);
                winner.create_vertex(VId(99), vec![], vec![]);
                db.write(&cx, winner).await.unwrap();
                vfs.reset();
                assert!(db.preparation_anchor.is_some());
                assert_eq!(
                    db.prepare_write_at(basis, update(1, 11)).unwrap().template,
                    expected
                );
                assert!(matches!(
                    db.prepare_write_at(basis, create(7)),
                    Err(WriteTxnError::Write(WriteError::IdentitySpent {
                        elem: ElementId::Vertex(VId(7))
                    }))
                ));
                let mut spent_edge = WriteBatch::new(RelationId(1));
                spent_edge.add_edge(EId(9), VId(1), VId(2), vec![]);
                assert!(matches!(
                    db.prepare_write_at(basis, spent_edge),
                    Err(WriteTxnError::Write(WriteError::IdentitySpent {
                        elem: ElementId::Edge(EId(9))
                    }))
                ));
                // The ordinary evaluator still rejects reuse within a single
                // statement even when normalization would erase its birth.
                let mut same_statement = create(8);
                same_statement.delete_vertex(VId(8));
                same_statement.create_vertex(VId(8), vec![], vec![]);
                assert!(matches!(
                    db.prepare_write_at(basis, same_statement),
                    Err(WriteTxnError::Write(WriteError::IdentitySpent {
                        elem: ElementId::Vertex(VId(8))
                    }))
                ));
                // A future identity must not leak into the pinned writer's
                // spent set. It prepares as absent, then ordinary FCW refuses.
                let future_identity = db.prepare_write_at(basis, create(99)).unwrap();
                assert!(matches!(
                    db.commit_prepared(&cx, future_identity).await,
                    Err(WriteError::FirstCommitterWins { .. })
                ));
                txn.write_at_basis(&mut db, update(1, 11)).unwrap();
                assert_eq!(txn.basis(), basis);
                assert_eq!(
                    txn.vertex(&db, VId(2)).unwrap().unwrap().props[0].1,
                    CanonicalScalar::Int(10)
                );
                // Returning the old V2 value adds a real dependency. Its
                // changed winner must conflict even though the V1 write is
                // disjoint; the original read basis never advances silently.
                assert!(matches!(
                    txn.commit(&mut db, &cx).await,
                    Err(WriteTxnError::Write(WriteError::FirstCommitterWins { .. }))
                ));
                let mut txn = db.begin(&txcx).unwrap();
                let current = txn.basis();
                let mut unrelated = update(2, 33);
                unrelated.create_vertex(VId(100), vec![], vec![]);
                db.write(&cx, unrelated).await.unwrap();
                txn.write_at_basis(&mut db, update(1, 11)).unwrap();
                assert_eq!(txn.basis(), current);
                assert_eq!(txn.commit(&mut db, &cx).await.unwrap(), CommitSeq(5));
                assert_eq!(
                    db.vertex(VId(2)).unwrap().unwrap().props[0].1,
                    CanonicalScalar::Int(33)
                );

                // The ordinary API retains its explicit refresh contract, and
                // that path also needs only this handle's post-open suffix.
                let mut txn = db.begin(&txcx).unwrap();
                assert!(txn.vertex(&db, VId(1)).unwrap().is_some());
                let expected = db.prepare_write(update(1, 12)).unwrap().template;
                let stable = txn.basis();
                db.write(&cx, update(2, 44)).await.unwrap();
                assert_eq!(
                    db.prepare_write_at(stable, update(1, 12)).unwrap().template,
                    expected
                );
                assert!(matches!(
                    txn.write(&mut db, update(1, 12)),
                    Err(WriteTxnError::SnapshotAdvanced { .. })
                ));
                assert_eq!(txn.refresh_snapshot(&db, &txcx).unwrap(), CommitSeq(6));
                txn.write(&mut db, update(1, 12)).unwrap();
                assert_eq!(txn.commit(&mut db, &cx).await.unwrap(), CommitSeq(7));
                assert_eq!(
                    vfs.count(),
                    0,
                    "stable staging, refresh, and completion do not load origin"
                );
                assert_eq!(db.delta_materialized_after, basis);
                db.ensure_delta_window(&cx, CommitSeq::ORIGIN)
                    .await
                    .unwrap();
                assert_eq!(
                    vfs.count(),
                    2,
                    "only the original missing prefix is admitted"
                );
                assert!(db.preparation_anchor.is_none());
                assert_eq!(
                    db.prepare_write_at(stable, update(1, 12)).unwrap().template,
                    expected
                );
                assert_eq!(txcx.outstanding_obligations(), 0);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn reopened_historical_resident_index_refresh_requires_and_uses_its_exact_delta_suffix() {
        let ((), report) = run_async_under_lab(0xa677, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let query = contexts.query();
            let vfs = CountingVfs::new();
            let path = vfs.memory.database_dir();
            let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys())
                .await
                .unwrap();
            let mut seed = WriteBatch::new(RelationId(1));
            seed.create_vertex(
                VId(1),
                vec![],
                vec![(
                    PropertyKeyId(1),
                    CanonicalScalar::ucs_basic_text("graph").unwrap(),
                )],
            );
            db.write(&cx, seed).await.unwrap();
            let mut update = WriteBatch::new(RelationId(1));
            update.set_vertex_property(
                VId(1),
                PropertyKeyId(1),
                Some(CanonicalScalar::ucs_basic_text("storage").unwrap()),
            );
            db.write(&cx, update).await.unwrap();
            drop(db);
            vfs.reset();
            let mut db = Database::open_with_vfs(&cx, vfs.clone(), &path, keys())
                .await
                .unwrap();
            let policy = fgdb_beacon::read::ReadPolicy::default();
            let search = fgdb_beacon::read::Search::Text {
                query: "graph",
                k: 10,
                mode: fgdb_beacon::TextMatch::Any,
            };
            let mut options = query_beacon::Options::text(PropertyKeyId(1));
            options.as_of = Some(CommitSeq(1));
            let mut resident = db.prepare_beacon_index(&query, &options).unwrap();
            let pinned = resident.snapshot();
            let before = resident.search(&query, search, policy).unwrap();
            assert_eq!(resident.source_sequence(), CommitSeq(1));
            assert_eq!(pinned.stats().documents, 1);
            assert!(matches!(
                resident.refresh(&query, &db, None, policy),
                Err(query_beacon::ResidentIndexError::Source(
                    ReadError::DeltaCursorRetired {
                        asked: CommitSeq(1),
                        retained_after: CommitSeq(2),
                        frontier: CommitSeq(2),
                    }
                ))
            ));
            assert_eq!(resident.source_sequence(), CommitSeq(1));
            assert_eq!(resident.search(&query, search, policy).unwrap(), before);
            assert_eq!(
                vfs.count(),
                0,
                "historical graph projection needs no capsule reads"
            );
            db.ensure_delta_window(&cx, CommitSeq(1)).await.unwrap();
            let refreshed = resident.refresh(&query, &db, None, policy).unwrap();
            assert_eq!(
                (
                    refreshed.from,
                    refreshed.through,
                    refreshed.commits,
                    refreshed.touched_vertices
                ),
                (CommitSeq(1), CommitSeq(2), 1, 1)
            );
            options.as_of = None;
            let expected = db.beacon_search(&query, &options, search).unwrap();
            assert_ne!(before, expected);
            assert_eq!(resident.search(&query, search, policy).unwrap(), expected);
            assert_eq!(pinned.search(&query, search, policy).unwrap(), before);
            assert_eq!(
                vfs.count(),
                1,
                "refresh reads only the one requested capsule"
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn standing_reachability_anchors_at_checkpoint_without_reading_a_capsule() {
        let ((), report) = run_async_under_lab(0xa674, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let query = contexts.query();
            let vfs = CountingVfs::new();
            let path = vfs.memory.database_dir();
            let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys())
                .await
                .unwrap();
            let mut seed = create(1);
            seed.create_vertex(VId(2), vec![], vec![]);
            seed.create_vertex(VId(3), vec![], vec![]);
            seed.add_edge(EId(1), VId(1), VId(2), vec![]);
            db.write(&cx, seed).await.unwrap();
            drop(db);
            vfs.reset();
            let mut db = Database::open_with_vfs(&cx, vfs.clone(), &path, keys())
                .await
                .unwrap();
            let policy = fgdb_gql::GqlQueryPolicy::new(10_000, 10_000, 100_000, 100_000);
            let handle = db
                .register_standing_reachability(&query, RelationId(1), policy)
                .unwrap();
            let rows = db.standing_reachability(&query, &handle).unwrap();
            assert_eq!(rows.rows().len(), 1);
            assert_eq!(rows.rows().iter().next().unwrap().0, &(VId(1), VId(2)));
            assert_eq!(vfs.count(), 0);
            let mut suffix = WriteBatch::new(RelationId(1));
            suffix.add_edge(EId(2), VId(2), VId(3), vec![]);
            db.write(&cx, suffix).await.unwrap();
            let rows = db.standing_reachability(&query, &handle).unwrap();
            assert_eq!(rows.rows().len(), 3);
            assert_eq!(rows.frontier(), CommitSeq(2));
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}

#[cfg(test)]
mod delta_read_error_laws {
    use super::*;

    #[test]
    fn the_shared_delta_cursor_mapper_preserves_a_retired_cut_exactly() {
        let asked = CommitSeq(3);
        let retained_after = CommitSeq(7);
        let frontier = CommitSeq(11);
        assert!(matches!(
            read_error_from_index(IndexError::CursorRetired {
                asked,
                retained_after,
                frontier,
            }),
            ReadError::DeltaCursorRetired {
                asked: found_asked,
                retained_after: found_retained_after,
                frontier: found_frontier,
            } if found_asked == asked
                && found_retained_after == retained_after
                && found_frontier == frontier
        ));
    }
}

#[cfg(test)]
mod version_transcript_laws {
    use super::*;

    /// The v3 statement-chain laws, witnessed on THIS deliberate duplicate:
    /// the predecessor binds the chain, the element family and identity bind
    /// the transcript, durable content binds it — and nothing else does.
    #[test]
    fn the_chain_steps_over_statement_transcripts() {
        let edge = edge_statement_transcript(
            EId(10),
            VId(1),
            RelationId(1),
            VId(2),
            &[(PropertyKeyId(3), CanonicalScalar::Int(1))],
        )
        .expect("encodes");
        let base = statement_successor(None, &edge);
        assert_ne!(
            base,
            statement_successor(Some(base), &edge),
            "the predecessor binds the chain — a restated statement still advances"
        );
        let other_eid = edge_statement_transcript(
            EId(11),
            VId(1),
            RelationId(1),
            VId(2),
            &[(PropertyKeyId(3), CanonicalScalar::Int(1))],
        )
        .expect("encodes");
        assert_ne!(
            base,
            statement_successor(None, &other_eid),
            "identity binds"
        );
        let other_content = edge_statement_transcript(
            EId(10),
            VId(1),
            RelationId(1),
            VId(2),
            &[(PropertyKeyId(3), CanonicalScalar::Int(2))],
        )
        .expect("encodes");
        assert_ne!(
            base,
            statement_successor(None, &other_content),
            "content binds"
        );
        // Family separation: a vertex whose fields shadow the edge's bytes
        // cannot alias it — the tag byte is load-bearing.
        let vertex = vertex_statement_transcript(VId(10), 0, &[], &[]).expect("encodes");
        assert_ne!(
            statement_successor(None, &vertex),
            statement_successor(
                None,
                &edge_statement_transcript(EId(10), VId(0), RelationId(0), VId(0), &[])
                    .expect("encodes")
            ),
        );
    }
}

/// Point reads answer from the maintained adjacency index. These laws hold it
/// to the whole-history merge — the reference semantics — for every vertex,
/// relation, direction, EId and sequence of a randomized history with
/// parallel edges, tombstones, content-version successors and compaction.
#[cfg(test)]
mod point_read_index_laws {
    use super::*;
    use fgdb_strata::root::{merge_edge_with_props, merge_in_neighbours, merge_neighbours};

    struct Lcg(u64);
    impl Lcg {
        fn below(&mut self, bound: usize) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((self.0 >> 33) as usize) % bound.max(1)
        }
    }

    fn assert_index_matches_merge(db: &Database<MemVfs>, vids: &[VId], eids: &[EId], label: &str) {
        let snapshot = &db.snapshot;
        let relations = [RelationId(1), RelationId(2), RelationId(3)];
        for as_of in 0..=snapshot.frontier.0 {
            let as_of = CommitSeq(as_of);
            for &vid in vids {
                for relation in relations {
                    assert_eq!(
                        snapshot.neighbours_at(vid, relation, as_of).unwrap(),
                        merge_neighbours(&snapshot.blocks, vid, relation, as_of).unwrap(),
                        "{label}: neighbours({vid:?}, {relation:?}) at {as_of:?}"
                    );
                    assert_eq!(
                        snapshot.in_neighbours_at(vid, relation, as_of).unwrap(),
                        merge_in_neighbours(&snapshot.blocks, vid, relation, as_of).unwrap(),
                        "{label}: in_neighbours({vid:?}, {relation:?}) at {as_of:?}"
                    );
                }
            }
            for &eid in eids {
                let expected =
                    merge_edge_with_props(&snapshot.blocks, &snapshot.block_props, eid, as_of)
                        .unwrap()
                        .map(|(entry, props)| EdgeRecord { entry, props });
                assert_eq!(
                    snapshot.edge_at(eid, as_of).unwrap(),
                    expected,
                    "{label}: edge({eid:?}) at {as_of:?}"
                );
            }
        }
    }

    #[test]
    fn index_point_reads_equal_the_whole_history_merge_at_every_sequence() {
        let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
        let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
        let contexts = fgdb_types::context::PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        for seed in [1_u64, 0x5eed, 0xfeed_beef] {
            let keys = DatabaseKeys::new(
                [0x5a; 32],
                DatabaseSecurityNamespaceId([0x77; 32]),
                [0x3c; 32],
            );
            let mut db = runtime.block_on(Database::open_memory(&cx, keys)).unwrap();
            let mut random = Lcg(seed);
            let mut vids: Vec<VId> = (1..=12).map(VId).collect();
            let mut initial = WriteBatch::new(RelationId(1));
            for &vid in &vids {
                initial.create_vertex(vid, vec![], vec![]);
            }
            runtime.block_on(db.write(&cx, initial)).unwrap();
            // (eid, relation) of every live edge; every EId ever created.
            let mut live: Vec<(EId, RelationId)> = Vec::new();
            let mut eids: Vec<EId> = Vec::new();
            let mut next_eid = 1_u128;
            for commit in 1..=60_u64 {
                let relation = RelationId(1 + commit % 3);
                let mut batch = WriteBatch::new(relation);
                let vid = VId(100 + u128::from(commit));
                batch.create_vertex(vid, vec![], vec![]);
                for _ in 0..3 {
                    // Endpoints may repeat, so parallel edges and self loops occur.
                    let src = vids[random.below(vids.len())];
                    let dst = vids[random.below(vids.len())];
                    let eid = EId(next_eid);
                    next_eid += 1;
                    batch.add_edge(
                        eid,
                        src,
                        dst,
                        vec![(PropertyKeyId(1), CanonicalScalar::Int(commit as i64))],
                    );
                    live.push((eid, relation));
                    eids.push(eid);
                }
                let same: Vec<usize> = (0..live.len())
                    .filter(|&at| live[at].1 == relation && live[at].0.0 < next_eid - 3)
                    .collect();
                if same.len() >= 2 {
                    let doomed = same[random.below(same.len())];
                    batch.delete_edge(live[doomed].0);
                    let kept: Vec<usize> =
                        same.iter().copied().filter(|&at| at != doomed).collect();
                    let touched = live[kept[random.below(kept.len())]].0;
                    batch.set_edge_property(
                        touched,
                        PropertyKeyId(2),
                        Some(CanonicalScalar::Int(-(commit as i64))),
                    );
                    live.remove(doomed);
                }
                runtime.block_on(db.write(&cx, batch)).unwrap();
                vids.push(vid);
                if commit == 30 {
                    runtime.block_on(db.compact(&cx)).unwrap();
                    assert_index_matches_merge(
                        &db,
                        &vids,
                        &eids,
                        &format!("seed {seed:#x} after compaction"),
                    );
                }
            }
            assert_index_matches_merge(&db, &vids, &eids, &format!("seed {seed:#x} final"));
            assert!(
                db.verify_snapshot_indexes().unwrap(),
                "maintained index drifted from a rebuild"
            );
        }
    }
}

#[cfg(test)]
mod root_capacity_laws {
    use super::*;

    struct Lcg(u64);
    impl Lcg {
        fn below(&mut self, bound: usize) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((self.0 >> 33) as usize) % bound.max(1)
        }
    }

    /// The admission bound must never be below what the fold really seals,
    /// or a commit could pass admission and still overflow the root after
    /// its durable point. Random commits of every statement-producing kind:
    /// vertex and edge creation, vertex and edge property updates, label
    /// changes, edge deletes, and vertex deletes whose cascade retires edges
    /// in other sources' families. Also across a compaction.
    #[test]
    fn the_growth_bound_covers_every_block_and_patch_a_commit_seals() {
        let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
        let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
        let contexts = fgdb_types::context::PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        for seed in [3_u64, 0xb10c, 0xcafe_f00d] {
            let keys = DatabaseKeys::new(
                [0x5a; 32],
                DatabaseSecurityNamespaceId([0x77; 32]),
                [0x3c; 32],
            );
            let mut db = runtime.block_on(Database::open_memory(&cx, keys)).unwrap();
            let mut random = Lcg(seed);
            // Live state, updated only after a commit succeeds.
            let mut vids: Vec<VId> = Vec::new();
            let mut edges: Vec<(EId, VId, VId)> = Vec::new();
            let (mut next_vid, mut next_eid) = (1_u128, 1_u128);
            let (mut cascades, mut both_kinds) = (0, 0);
            for commit in 1..=48_i64 {
                let mut batch = WriteBatch::new(RelationId(1));
                let mut live_vids = vids.clone();
                let created: Vec<VId> = (0..4).map(|at| VId(next_vid + at)).collect();
                for &vid in &created {
                    batch.create_vertex(
                        vid,
                        vec![LabelId(1)],
                        vec![(PropertyKeyId(1), CanonicalScalar::Int(commit))],
                    );
                }
                live_vids.extend(&created);
                // Chosen first, so updates and edge deletes avoid it. New
                // edges may still touch it: its cascade then retires them in
                // their own sources' families, in the same commit.
                let doomed_vertex =
                    (commit % 4 == 0 && !vids.is_empty()).then(|| vids[random.below(vids.len())]);
                let mut new_edges = Vec::new();
                for at in 0..6 {
                    let src = live_vids[random.below(live_vids.len())];
                    let dst = live_vids[random.below(live_vids.len())];
                    let eid = EId(next_eid + at);
                    batch.add_edge(
                        eid,
                        src,
                        dst,
                        vec![(PropertyKeyId(2), CanonicalScalar::Int(commit))],
                    );
                    new_edges.push((eid, src, dst));
                }
                let spared: Vec<(EId, VId, VId)> = edges
                    .iter()
                    .copied()
                    .filter(|&(_, src, dst)| {
                        Some(src) != doomed_vertex && Some(dst) != doomed_vertex
                    })
                    .collect();
                let mut doomed_edge = None;
                if spared.len() >= 2 {
                    let updated = spared[random.below(spared.len())].0;
                    batch.set_edge_property(
                        updated,
                        PropertyKeyId(2),
                        Some(CanonicalScalar::Int(-commit)),
                    );
                    if commit % 3 == 0 {
                        let candidates: Vec<EId> = spared
                            .iter()
                            .map(|&(eid, _, _)| eid)
                            .filter(|&eid| eid != updated)
                            .collect();
                        let eid = candidates[random.below(candidates.len())];
                        batch.delete_edge(eid);
                        doomed_edge = Some(eid);
                    }
                }
                let relabel: Vec<VId> = vids
                    .iter()
                    .copied()
                    .filter(|&vid| Some(vid) != doomed_vertex)
                    .collect();
                if !relabel.is_empty() {
                    let vid = relabel[random.below(relabel.len())];
                    batch.set_vertex_label(vid, LabelId(10 + commit as u64), true);
                    batch.set_vertex_property(
                        vid,
                        PropertyKeyId(1),
                        Some(CanonicalScalar::Int(-commit)),
                    );
                }
                if let Some(vid) = doomed_vertex {
                    batch.delete_vertex(vid);
                }
                let template = db
                    .build_write_template(batch.clone())
                    .unwrap_or_else(|error| {
                        panic!("seed {seed:#x} commit {commit}: generator made {error:?}")
                    });
                let (bound_blocks, bound_patches) = root_growth_bound(&template);
                let before = (db.writer.sealed().len(), db.writer.sealed_patches().len());
                runtime.block_on(db.write(&cx, batch)).unwrap();
                let after = (db.writer.sealed().len(), db.writer.sealed_patches().len());
                let (added_blocks, added_patches) = (after.0 - before.0, after.1 - before.1);
                assert!(
                    added_blocks <= bound_blocks && added_patches <= bound_patches,
                    "seed {seed:#x} commit {commit}: sealed {added_blocks} blocks and \
                     {added_patches} patches, bound {bound_blocks} and {bound_patches}"
                );
                both_kinds += usize::from(added_blocks > 0 && added_patches > 0);
                vids = live_vids;
                edges.extend(new_edges);
                next_vid += 4;
                next_eid += 6;
                if let Some(eid) = doomed_edge {
                    edges.retain(|&(live, _, _)| live != eid);
                }
                if let Some(vid) = doomed_vertex {
                    cascades += 1;
                    vids.retain(|&live| live != vid);
                    edges.retain(|&(_, src, dst)| src != vid && dst != vid);
                }
                if commit == 24 {
                    runtime.block_on(db.compact(&cx)).unwrap();
                }
            }
            // The law is only as strong as its coverage: every commit sealed
            // both kinds, and cascades ran.
            assert_eq!(both_kinds, 48, "seed {seed:#x}");
            assert_eq!(cascades, 12, "seed {seed:#x}");
        }
    }

    /// A vertex delete's cascade retires each incident edge in its own
    /// source's family. Deleting a hub with 24 in-edges from 24 sources
    /// seals 24 edge blocks from one row, so a bound that counted only the
    /// deleted vertex would fall far short.
    #[test]
    fn a_hub_delete_is_bounded_by_its_whole_cascade() {
        let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
        let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
        let contexts = fgdb_types::context::PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let keys = DatabaseKeys::new(
            [0x5a; 32],
            DatabaseSecurityNamespaceId([0x77; 32]),
            [0x3c; 32],
        );
        let mut db = runtime.block_on(Database::open_memory(&cx, keys)).unwrap();
        let hub = VId(1000);
        let mut setup = WriteBatch::new(RelationId(1));
        setup.create_vertex(hub, vec![], vec![]);
        for source in 1..=24 {
            setup.create_vertex(VId(source), vec![], vec![]);
            setup.add_edge(EId(source), VId(source), hub, vec![]);
        }
        runtime.block_on(db.write(&cx, setup)).unwrap();
        let mut delete = WriteBatch::new(RelationId(1));
        delete.delete_vertex(hub);
        let template = db.build_write_template(delete.clone()).unwrap();
        let (bound_blocks, bound_patches) = root_growth_bound(&template);
        let before = (db.writer.sealed().len(), db.writer.sealed_patches().len());
        runtime.block_on(db.write(&cx, delete)).unwrap();
        let added = (
            db.writer.sealed().len() - before.0,
            db.writer.sealed_patches().len() - before.1,
        );
        assert_eq!(added.0, 24, "one retirement block per source family");
        assert!(added.0 <= bound_blocks && added.1 <= bound_patches);
        // A deletion retires one statement per element: the bound is tight.
        assert_eq!((bound_blocks, bound_patches), (24, 1));
    }

    /// Admission is exact against its bound: a commit that fits exactly is
    /// admitted, one reference more is refused with the full accounting, and
    /// the vertex-patch ceiling binds independently of the block ceiling.
    #[test]
    fn admission_refuses_one_reference_past_either_ceiling() {
        let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
        let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
        let contexts = fgdb_types::context::PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let keys = DatabaseKeys::new(
            [0x5a; 32],
            DatabaseSecurityNamespaceId([0x77; 32]),
            [0x3c; 32],
        );
        let mut db = runtime.block_on(Database::open_memory(&cx, keys)).unwrap();
        let mut vertices = WriteBatch::new(RelationId(1));
        for vid in 1..=3 {
            vertices.create_vertex(VId(vid), vec![], vec![]);
        }
        runtime.block_on(db.write(&cx, vertices)).unwrap();
        let mut batch = WriteBatch::new(RelationId(1));
        batch.add_edge(EId(1), VId(1), VId(2), vec![]);
        batch.add_edge(EId(2), VId(2), VId(3), vec![]);
        batch.create_vertex(VId(4), vec![], vec![]);
        batch.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(7)));
        let template = db.build_write_template(batch).unwrap();
        // Two creations of one statement each, plus one vertex creation and
        // one vertex content change (retirement and successor).
        assert_eq!(root_growth_bound(&template), (2, 3));
        assert_eq!(admit_root_capacity(98, 7, &template, 100, 10), Ok(()));
        assert_eq!(
            admit_root_capacity(99, 7, &template, 100, 10),
            Err(RootCapacityExceeded {
                blocks: 99,
                added_blocks: 2,
                max_blocks: 100,
                patches: 7,
                added_patches: 3,
                max_patches: 10,
            })
        );
        assert!(admit_root_capacity(0, 8, &template, 100, 10).is_err());
    }
}

#[cfg(test)]
mod commit_growth_laws {
    use super::*;

    std::thread_local! {
        pub(super) static INCIDENT_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    #[test]
    fn plain_edge_preparation_work_does_not_grow_with_committed_history() {
        let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
        let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
        let contexts = fgdb_types::context::PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let keys = DatabaseKeys::new(
            [0x5a; 32],
            DatabaseSecurityNamespaceId([0x77; 32]),
            [0x3c; 32],
        );
        let mut db = runtime.block_on(Database::open_memory(&cx, keys)).unwrap();
        let mut initial = WriteBatch::new(RelationId(1));
        for vid in [VId(1), VId(2), VId(3)] {
            initial.create_vertex(vid, vec![], vec![]);
        }
        runtime.block_on(db.write(&cx, initial)).unwrap();
        let mut samples = Vec::new();
        for commit in 1..=40u128 {
            let mut batch = WriteBatch::new(RelationId(1));
            for offset in 0..8 {
                batch.add_edge(EId(commit * 8 + offset), VId(1), VId(2), vec![]);
            }
            runtime.block_on(db.write(&cx, batch)).unwrap();
            if commit == 10 || commit == 40 {
                let mut next = WriteBatch::new(RelationId(1));
                next.add_edge(EId(10_000), VId(1), VId(3), vec![]);
                INCIDENT_VISITS.with(|count| count.set(0));
                db.prepare_write(next).unwrap();
                samples.push(INCIDENT_VISITS.with(std::cell::Cell::get));
            }
        }
        // Plain insertion must still CREATE edges: neighbours answers distinct
        // destinations, and the final commit's parallel edges are all live.
        assert!(
            samples[1] <= samples[0].max(1) * 2,
            "plain insertion scanned committed history: early={} late={}",
            samples[0],
            samples[1]
        );
        assert_eq!(
            db.neighbours(VId(1), RelationId(1)).unwrap(),
            vec![VId(2)],
            "plain add_edge rows must survive the guarded preparation"
        );
        // The loop committed EIds 8..=320; every plain-inserted row is live.
        let all = db.edges().unwrap();
        assert_eq!(all.len(), 320);
        assert!(all.iter().any(|record| record.entry.eid == EId(320)));
        assert!(all.iter().all(|record| record.entry.eid != EId(10_000)));
    }
}

#[cfg(test)]
mod publish_receipt_laws {
    use super::*;

    fn keys() -> DatabaseKeys {
        DatabaseKeys::new(
            [0x5a; 32],
            DatabaseSecurityNamespaceId([0x77; 32]),
            [0x3c; 32],
        )
    }

    /// One new vertex and one edge to it: each commit adds a block and a
    /// vertex patch.
    fn commit_batch(commit: u128) -> WriteBatch {
        let mut batch = WriteBatch::new(RelationId(1));
        batch.create_vertex(VId(100 + commit), vec![], vec![]);
        batch.add_edge(EId(commit), VId(1), VId(100 + commit), vec![]);
        batch
    }

    /// Whether the handle holds a receipt for every block and vertex patch
    /// its published root names, with that whole root as its verified prefix.
    fn receipts_cover_the_root<V: Vfs + Clone>(db: &Database<V>) -> bool {
        let snapshot = &db.snapshot;
        db.receipts.verified_root_prefix(PARTITION)
            == (snapshot.refs.len(), snapshot.patch_refs.len())
            && snapshot.refs.iter().all(|reference| {
                db.receipts
                    .holds(fgdb_strata::DeltaBlockVersion(reference.block_id))
            })
            && snapshot.patch_refs.iter().all(|reference| {
                db.receipts
                    .holds_patch(fgdb_strata::vertex::VertexPatchVersion(reference.patch_id))
            })
    }

    /// **EVERY OPEN PATH LEAVES THE HANDLE HOLDING RECEIPTS FOR ITS WHOLE
    /// PUBLISHED ROOT** (fgdb-ibbuq; owner ruling 2026-10-06, "trust
    /// publication"). The checkpoint open seeds them from the slot-selected
    /// root's admission. The rebuild open and compaction keep the ones their
    /// own publication earned. So the first commit after any of them syncs
    /// only what it writes. The seeded handle's next commit publishes the same
    /// root as a twin that never closed, so seeding changes no published byte.
    #[test]
    fn every_open_path_holds_receipts_for_its_published_root() {
        let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
        let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
        let contexts = fgdb_types::context::PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();

        let vfs = MemVfs::new().unwrap();
        let dir = vfs.database_dir();
        let twin_vfs = MemVfs::new().unwrap();
        let mut db = runtime
            .block_on(Database::create_with_vfs(&cx, vfs.clone(), &dir, keys()))
            .unwrap();
        let mut twin = runtime
            .block_on(Database::create_with_vfs(
                &cx,
                twin_vfs.clone(),
                twin_vfs.database_dir(),
                keys(),
            ))
            .unwrap();
        let mut origin = WriteBatch::new(RelationId(1));
        origin.create_vertex(VId(1), vec![], vec![]);
        runtime.block_on(db.write(&cx, origin)).unwrap();
        let mut origin = WriteBatch::new(RelationId(1));
        origin.create_vertex(VId(1), vec![], vec![]);
        runtime.block_on(twin.write(&cx, origin)).unwrap();
        for commit in 1..=12 {
            runtime
                .block_on(db.write(&cx, commit_batch(commit)))
                .unwrap();
            runtime
                .block_on(twin.write(&cx, commit_batch(commit)))
                .unwrap();
        }
        assert_eq!(db.partition_root().unwrap(), twin.partition_root().unwrap());
        assert!(
            db.snapshot.refs.len() > 1 && db.snapshot.patch_refs.len() > 1,
            "the fixture must publish several blocks and vertex patches"
        );
        drop(db);

        let mut db = runtime
            .block_on(Database::open_with_vfs(&cx, vfs.clone(), &dir, keys()))
            .unwrap();
        assert!(
            receipts_cover_the_root(&db),
            "the checkpoint open seeds receipts for the slot-selected root"
        );
        runtime.block_on(db.write(&cx, commit_batch(13))).unwrap();
        runtime.block_on(twin.write(&cx, commit_batch(13))).unwrap();
        assert_eq!(
            db.partition_root().unwrap(),
            twin.partition_root().unwrap(),
            "the seeded handle publishes the never-closed twin's root"
        );
        assert!(receipts_cover_the_root(&db));

        runtime.block_on(db.compact(&cx)).unwrap();
        assert!(
            receipts_cover_the_root(&db),
            "compaction keeps the receipts its publication earned"
        );
        drop(db);

        let db = runtime
            .block_on(Database::bind_with_vfs(
                &cx,
                vfs.clone(),
                &dir,
                keys(),
                true,
            ))
            .unwrap();
        assert!(
            receipts_cover_the_root(&db),
            "the rebuild open keeps the receipts its publication earned"
        );
    }

    /// The regular files and directories under `dir`, `dir` included, found
    /// without following a symlink.
    fn count_tree(dir: &Path) -> (u64, u64) {
        let (mut files, mut directories) = (0, 1);
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                let (below_files, below_directories) = count_tree(&entry.path());
                files += below_files;
                directories += below_directories;
            } else if kind.is_file() {
                files += 1;
            }
        }
        (files, directories)
    }

    /// **ADOPTING A COPY SYNCS EVERY FILE AND DIRECTORY IN IT** (fgdb-ibbuq).
    /// A writable open trusts that the published root's objects are durable,
    /// which a directory copied without a sync breaks; adopt is the step that
    /// restores the premise. It reaches exactly what an independent walk of
    /// the tree finds, the database then opens and writes, and a directory
    /// that is not a database is refused before anything is synced.
    #[test]
    fn adopt_syncs_every_file_and_directory_and_refuses_a_non_database() {
        let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
        let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
        let contexts = fgdb_types::context::PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let dir = std::env::temp_dir().join(format!("fgdb-adopt-law-{}", std::process::id()));

        let mut db = runtime
            .block_on(Database::create(&cx, &dir, keys()))
            .unwrap();
        let mut origin = WriteBatch::new(RelationId(1));
        origin.create_vertex(VId(1), vec![], vec![]);
        runtime.block_on(db.write(&cx, origin)).unwrap();
        for commit in 1..=3 {
            runtime
                .block_on(db.write(&cx, commit_batch(commit)))
                .unwrap();
        }
        drop(db);

        let adopted = runtime.block_on(Database::adopt(&cx, &dir)).unwrap();
        let (files, directories) = count_tree(&dir);
        assert!(files > 0 && directories > 1, "the fixture has a tree");
        assert_eq!(adopted, Adopted { files, directories });

        let mut db = runtime.block_on(Database::open(&cx, &dir, keys())).unwrap();
        runtime.block_on(db.write(&cx, commit_batch(4))).unwrap();

        let plain = dir.with_extension("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert!(matches!(
            runtime.block_on(Database::adopt(&cx, &plain)),
            Err(OpenError::NotADatabase { .. })
        ));
    }
}
