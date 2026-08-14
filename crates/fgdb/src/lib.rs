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
//! # use asupersync::{Budget, runtime::RuntimeBuilder};
//! # use fgdb::{Database, DatabaseKeys, WriteBatch};
//! # use fgdb_delta_types::RelationId;
//! # use fgdb_types::context::PurposeContexts;
//! # use fgdb_types::ids::DatabaseSecurityNamespaceId;
//! # use fgdb_types::{EId, VId};
//! # let path = std::env::temp_dir().join(format!("fgdb-doctest-{}", std::process::id()));
//! # let keys = DatabaseKeys {
//! #     k_oid: [0x5a; 32],
//! #     namespace: DatabaseSecurityNamespaceId([0x77; 32]),
//! #     dek: [0x3c; 32],
//! # };
//! # let runtime = RuntimeBuilder::new().build().expect("production runtime");
//! # let root = runtime.request_cx_with_budget(Budget::INFINITE);
//! # let cx = &PurposeContexts::narrow_runtime_root(&root).commit();
//! # let path = &path;
//! # runtime.block_on(async move {
//!     let mut db = Database::create(cx, path, keys).await?;
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
//! landed: sessions and prepared statements; any query language; the explicit
//! transaction ownership contract, its epoch guard and reattach/renew/expiry
//! (`fgdb-w10-txn-ownership-eab`); capability narrowing and secure views; result
//! stream lifecycle; multiple graphs, branches or partitions; and the server and
//! CLI postures.
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

use asupersync::fs::{UnixVfs, Vfs};
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::{CAPSULE_DIR, CommitCoordinator, CommitError};
use fgdb_chronicle::identity::IdentifiedObject;
use fgdb_chronicle::marker::{CommitMarker, EffectSource, HeadUpdate};
use fgdb_chronicle::{
    RootBootstrap, RootSelection, RootSlot, RootStore, store::StoreError as SlotStoreError,
};
use fgdb_crypto::Digest;
use fgdb_delta_types::{
    CanonicalError, CommittedMarker, CoordinateEntry, DeltaRow, ElementId, IndexError, LabelId,
    LocalDeltaBatchIndex, LogicalDeltaBatch, LogicalDeltaTemplate, PropertyKeyId, RelationId,
    SchemaEpoch, fold_target_disjoint,
};
use fgdb_strata::edge_props::BlockProps;
use fgdb_strata::manifest::{ManifestRecord, ManifestVersion, encode_manifest, records_of};
use fgdb_strata::root::{
    BlockRef, PatchRef, RootError, merge_all_edges_with_props, merge_edge_with_props,
    merge_in_neighbours, merge_neighbours,
};
use fgdb_strata::store::{BlockStore, PublishReceipts, StoreError};
use fgdb_strata::vertex::{merge_all_vertices, merge_vertex};
use fgdb_strata::writer::{BlockWriter, WriteError as BlockWriteError};
use fgdb_strata::{AdjacencyEntry, DeltaBlockVersion, PartitionRootVersion};

pub use fgdb_strata::edge_props::EdgePropertyRow;
pub use fgdb_strata::vertex::VertexRow;
use fgdb_types::context::CommitCx;
use fgdb_types::ids::{DatabaseSecurityNamespaceId, ObjectId};
use fgdb_types::{BranchId, CanonicalScalar, CommitSeq, EId, GraphId, MarkerRef, VId};
use std::path::{Path, PathBuf};

/// Re-exported because [`Database::write_with_crash`] takes one: a caller
/// driving the crash-point matrix needs to name the instants, and importing them
/// from Chronicle directly would make the spine's own signature unusable without
/// a second dependency.
pub use fgdb_chronicle::commit::CrashPoint;
pub use fgdb_strata::store::BlockStoreCrashPoint;

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
#[derive(Clone, Copy, Debug)]
pub struct DatabaseKeys {
    /// The immutable object-identity key (§5.1).
    pub k_oid: [u8; 32],
    pub namespace: DatabaseSecurityNamespaceId,
    /// The data-encryption key for capsules.
    pub dek: [u8; 32],
}

impl DatabaseKeys {
    fn capsule_keys(&self) -> CapsuleKeys {
        CapsuleKeys {
            k_oid: self.k_oid,
            namespace: self.namespace,
            dek: self.dek,
            object_kind: CAPSULE_OBJECT_KIND,
            profile: CapsuleProfile::balanced(),
        }
    }

    fn block_keys(&self) -> (&[u8; 32], DatabaseSecurityNamespaceId) {
        (&self.k_oid, self.namespace)
    }
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
    /// The selected slot is well-formed and is NOT this database's: its
    /// identity tuple or PLAIN-opener form disagrees with the keys in hand.
    /// Refused, never reinterpreted — a slot from another database, another
    /// posture, or a tampered one must not steer recovery.
    ForeignSlot {
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
    Io(std::io::Error),
    Commit(CommitError),
    Store(StoreError),
    /// The durable stream could not be rebuilt into a partition.
    Rebuild(RebuildError),
}

/// Why rebuilding the tier-D fold from the durable stream failed.
#[derive(Debug)]
pub enum RebuildError {
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
    /// Chronicle crossed the point where a marker may be durable but did not
    /// complete D2 observably. Only reopen can decide whether it committed.
    CommitOutcomeUnknown {
        published_frontier: CommitSeq,
    },
    /// D2 completed, but derived publication did not catch the handle up.
    NeedsAuthoritativeRecovery(RecoveryRequired),
}

/// Why a write could not be committed.
#[derive(Debug)]
pub enum WriteError {
    /// The batch was empty. Refused rather than committed as a no-op: an empty
    /// commit consumes a sequence and publishes a marker, and a caller that did
    /// that by accident should be told.
    EmptyBatch,
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
    /// Chronicle failed before the marker could have become durable. Unlike
    /// [`WriteError::CommitOutcomeUnknown`], retrying after correcting the
    /// named cause cannot duplicate an unobserved commit.
    Commit(CommitError),
    /// Chronicle may or may not have made the marker durable. The live handle
    /// is fenced immediately; reopen is the only authority that can decide.
    CommitOutcomeUnknown {
        published_frontier: CommitSeq,
        source: CommitError,
    },
    /// A prior call left the handle unable to speak for Chronicle's head.
    HandleCommitOutcomeUnknown {
        published_frontier: CommitSeq,
    },
    /// A prior durable commit failed during derived publication. The handle is
    /// fenced so another write cannot publish from its stale fold.
    RecoveryRequired(RecoveryRequired),
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
    Root(RootError),
    /// Chronicle may have advanced, so the retained snapshot cannot be
    /// presented as current until an authoritative reopen resolves the log.
    CommitOutcomeUnknown {
        published_frontier: CommitSeq,
    },
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
    /// A delta-window cursor names a sequence the retained index no longer
    /// holds. The batches between the cursor and `retained_after` were
    /// retired; answering with the remaining suffix would be a gapped stream.
    DeltaCursorRetired {
        asked: CommitSeq,
        retained_after: CommitSeq,
        frontier: CommitSeq,
    },
    /// A derived-window query failed for a reason other than a future or
    /// retired cursor. `since` does not construct these; the arm exists so a
    /// new index-query error cannot be silently remapped.
    DeltaWindow(IndexError),
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
from_error!(RebuildError, Commit, CommitError);
from_error!(RebuildError, Store, StoreError);
from_error!(WriteError, Canonical, CanonicalError);
from_error!(WriteError, Commit, CommitError);
from_error!(ReadError, Root, RootError);

impl core::fmt::Display for OpenError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotADirectory { path } => {
                write!(f, "{} exists and is not a directory", path.display())
            }
            Self::Slot(error) => write!(f, "root slot: {error}"),
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
            Self::EmptyBatch => write!(f, "an empty batch consumes a commit sequence for nothing"),
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
                "compare-and-set of {:?} {:?} expected {:?}, found {:?}",
                mismatch.elem, mismatch.name, mismatch.expected, mismatch.actual
            ),
            Self::Canonical(error) => write!(f, "canonical form: {error}"),
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
                "delta cursor {asked:?} was retired: window is ({retained_after:?}, {frontier:?}]"
            ),
            Self::DeltaWindow(error) => write!(f, "delta window: {error}"),
        }
    }
}

impl core::error::Error for OpenError {}
impl core::error::Error for RebuildError {}
impl core::error::Error for WriteError {}
impl core::error::Error for ReadError {}

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompareAndSetMismatch {
    pub elem: ElementId,
    pub name: PropertyKeyId,
    pub expected: Option<CanonicalScalar>,
    pub actual: Option<CanonicalScalar>,
}

/// One batch of graph mutations, committed atomically.
///
/// **THIS IS NOT THE TRANSACTION MODEL AND MUST NOT BE READ AS ONE.** There is
/// no snapshot, no conflict detection, no isolation level, and no abort: a batch
/// is a set of rows that become durable together or not at all. The real
/// transaction model — workspaces, statement lifecycle, first-committer-wins,
/// SSI — lives in `fgdb-reference::txn` as executable semantics and in
/// `fgdb-w4-*` as the engine, and neither is wired here. Doctrine 7's line is
/// that a subset may do LESS while a substitute pretends to do the same thing;
/// this type is named for what it is so it cannot be mistaken for the other.
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
#[derive(Clone, Debug)]
pub struct WriteBatch {
    relation: RelationId,
    rows: Vec<PendingRow>,
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
    /// [`WriteMismatchPolicy::NoOp`] emits no row. There is no
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
#[derive(Debug)]
struct Snapshot {
    blocks: Vec<Vec<AdjacencyEntry>>,
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
    patches: Vec<Vec<VertexRow>>,
    patch_refs: Vec<PatchRef>,
    frontier: CommitSeq,
    root: PartitionRootVersion,
    /// The manifest published beside `root` (fgdb-63w2) — the identity a
    /// root slot carries, re-derived identically by every rebuild.
    manifest: ManifestVersion,
    /// The next unspent birth ordinal, derived by counting the creations the
    /// durable stream already contains. Derived rather than stored: identity
    /// allocation is `fgdb-w2`'s, and a counter persisted here would be a second
    /// authority beside the stream.
    next_birth_ordinal: u64,
    /// The current element-version chain head of every LIVE element,
    /// derived by folding the stream (fgdb-p3ok). This is the state a
    /// delete's `before_version` names, so it is engine state — but the
    /// DERIVATION is deliberately an independent spelling of the reference
    /// oracle's, never shared code: the differential's replay path validates
    /// every emitted image against the oracle's own chains, so a drift in
    /// either implementation is a refusal, not a silent agreement.
    versions: std::collections::BTreeMap<ElementId, ObjectId>,
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
fn plain_bootstrap(manifest_len: u64) -> RootBootstrap {
    let mut opener_payload = [0u8; fgdb_chronicle::root::OPENER_PAYLOAD_LEN];
    opener_payload[..2].copy_from_slice(&fgdb_strata::manifest::MANIFEST_OBJECT_KIND.to_le_bytes());
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
        opener_payload_len: 2,
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
        format_minor: 0,
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
        bootstrap: plain_bootstrap(manifest_len),
    }
}

/// The zero-validation half of the PLAIN opener ruling: a slot whose
/// identity tuple, opener form, or must-be-zero region disagrees is not this
/// database's slot and is refused, never reinterpreted.
fn validate_plain_slot(slot: &RootSlot, keys: &DatabaseKeys) -> bool {
    let expected_zeroed = {
        let mut probe = spine_slot(
            keys,
            slot.slot_generation,
            ManifestVersion(ObjectId(slot.root_manifest_oid)),
            slot.bootstrap.canonical_plaintext_len,
        );
        probe.root_manifest_oid = slot.root_manifest_oid;
        probe
    };
    *slot == expected_zeroed
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

#[derive(Debug)]
pub struct Database<V: Vfs = UnixVfs> {
    coordinator: CommitCoordinator<V>,
    store: BlockStore,
    /// The ONE mutable object in the directory (doctrine 5): the dual-slot
    /// root file whose selected slot names the current manifest (fgdb-ge6a,
    /// PLAIN opener ruling). Published after every manifest, reconciled at
    /// every open.
    slot_store: RootStore<V>,
    /// The generation the NEXT slot publication will carry; monotone.
    slot_generation: u64,
    keys: DatabaseKeys,
    snapshot: Snapshot,
    /// The persistent fold over every committed row, retained so a commit can
    /// fold only its own template instead of re-reading the whole history
    /// (`fgdb-fujt`: the per-commit rebuild's capsule re-read loop measured at
    /// 95% of an O(history) marginal write cost, ffe05f6). Never authoritative:
    /// it is seeded by the full rebuild at open, `rebuild()` remains the only
    /// recovery path, and `incremental_publish_equals_rebuild.rs` pins that a
    /// clone-publish of this writer is byte-identical to that rebuild.
    writer: BlockWriter,
    /// Truthfulness fence for the retained writer/snapshot pair. D2 moves
    /// this out of `Healthy` before any derived work can fail; only completing
    /// the snapshot swap (or constructing a fresh handle in `open`) moves it
    /// back. Keeping the stale values allocated is harmless because every
    /// public graph read and every write checks this state first.
    state: DatabaseState,
    /// Durability-and-admission receipts for the blocks this session has
    /// already published (fgdb-gieu). Session-scoped like the writer above,
    /// and with the same trust story: never authoritative, never persisted —
    /// a fresh process re-earns every proof from disk via the receipts'
    /// fallback path on its first publication.
    receipts: PublishReceipts,
    /// The durable I/O authority retained so same-handle recovery reopens
    /// through the SAME injected filesystem rather than silently escaping to
    /// `UnixVfs`. Strata's `BlockStore` does not yet accept a VFS; callers of
    /// `open_with_vfs` must not infer that its object files are faulted too.
    vfs: V,
    /// Derived window over committed delta batches (plan:397). Never
    /// authoritative and never persisted: every open rebuilds it from the
    /// recovered marker chain, and a crash after D2 before the in-memory
    /// insert heals the same way (FG-INV-18).
    delta_index: LocalDeltaBatchIndex,
}

impl Database<UnixVfs> {
    /// Create a database in `path`, which must be absent or an empty directory.
    pub async fn create(
        cx: &CommitCx,
        path: impl AsRef<Path>,
        keys: DatabaseKeys,
    ) -> Result<Self, OpenError> {
        let path = path.as_ref();
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if !metadata.is_dir() => {
                return Err(OpenError::NotADirectory {
                    path: path.to_path_buf(),
                });
            }
            Ok(_) => {
                if path.join(CAPSULE_DIR).exists() {
                    return Err(OpenError::AlreadyADatabase {
                        path: path.to_path_buf(),
                    });
                }
                if std::fs::read_dir(path)?.next().is_some() {
                    return Err(OpenError::NotEmpty {
                        path: path.to_path_buf(),
                    });
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir_all(path)?;
            }
            Err(error) => return Err(OpenError::Io(error)),
        }
        Self::bind_with_vfs(cx, UnixVfs::new(), path, keys, false).await
    }

    /// Open the database in `path`.
    ///
    /// Fails closed when `path` does not hold one. See
    /// [`OpenError::NotADatabase`] for why this cannot simply delegate to
    /// `CommitCoordinator::open`.
    pub async fn open(
        cx: &CommitCx,
        path: impl AsRef<Path>,
        keys: DatabaseKeys,
    ) -> Result<Self, OpenError> {
        let path = path.as_ref();
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if !metadata.is_dir() => {
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
        if !path.join(CAPSULE_DIR).is_dir() {
            return Err(OpenError::NotADatabase {
                path: path.to_path_buf(),
                missing: CAPSULE_DIR,
            });
        }
        Self::bind_with_vfs(cx, UnixVfs::new(), path, keys, false).await
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

impl<V: Vfs + Clone> Database<V> {
    /// Open the integrated database while interposing `vfs` on Chronicle and
    /// `manifest.root` I/O.
    ///
    /// This is the lab-runtime seam for faults at the two-fsync authority
    /// protocol and the derived root-slot publication that follows it. The
    /// current Tier-D `BlockStore` still uses its own Unix filesystem path, so
    /// this method deliberately makes no claim about faulting Strata objects.
    /// Production callers use [`Database::open`], which supplies [`UnixVfs`].
    #[doc(hidden)]
    pub async fn open_with_vfs(
        cx: &CommitCx,
        vfs: V,
        path: impl AsRef<Path>,
        keys: DatabaseKeys,
    ) -> Result<Self, OpenError> {
        let path = path.as_ref();
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if !metadata.is_dir() => {
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
        if !path.join(CAPSULE_DIR).is_dir() {
            return Err(OpenError::NotADatabase {
                path: path.to_path_buf(),
                missing: CAPSULE_DIR,
            });
        }
        Self::bind_with_vfs(cx, vfs, path, keys, false).await
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
        Ok(&self.snapshot.versions)
    }

    async fn bind_with_vfs(
        cx: &CommitCx,
        vfs: V,
        path: &Path,
        keys: DatabaseKeys,
        force_rebuild: bool,
    ) -> Result<Self, OpenError> {
        let coordinator =
            CommitCoordinator::open_with_vfs(cx, vfs.clone(), path, keys.capsule_keys()).await?;
        let store = BlockStore::open(cx, path, keys.k_oid, keys.namespace)?;
        // CHECKPOINT-SELECTED PATH (fgdb-ge6a): a lawful slot names a
        // resolvable manifest. Before accepting it, bind verifies the selected
        // partition's V2 marker-chain commitment against Chronicle's recovered
        // chain; reopen_from_verified_checkpoint then reopens that partition
        // and folds only the suffix. A missing slot falls back to a full
        // rebuild (and the reconciliation below creates it), while a present
        // slot that is foreign, malformed, or unaccountable refuses rather
        // than being silently rebuilt over.
        let probe = RootStore::with_vfs(vfs.clone(), path);
        let (snapshot, writer) = if force_rebuild {
            rebuild(cx, &coordinator, &store, &keys).await?
        } else {
            match probe.current(cx).await {
                Ok(slot) => {
                    if !validate_plain_slot(&slot, &keys) {
                        return Err(OpenError::ForeignSlot {
                            path: path.to_path_buf(),
                        });
                    }
                    let claimed = ManifestVersion(ObjectId(slot.root_manifest_oid));
                    match store.resolve_manifest(cx, claimed) {
                        Ok(resolved) if resolved.len() == 1 => {
                            let (record, root) = &resolved[0];
                            let describes_spine = record.graph == GRAPH
                                && record.branch == BRANCH
                                && record.partition == PARTITION
                                && root.graph == GRAPH
                                && root.branch == BRANCH
                                && root.partition == PARTITION;
                            // THE CHAIN BINDING (fgdb-90hw): the record claims
                            // "the history whose chain at published_at hashes
                            // to exactly this published my root", and the
                            // recovered chain is the judge — one comparison,
                            // no capsule folding. A future-frontier root falls
                            // off the chain (None); a same-namespace FOREIGN
                            // history hashes differently; a lagging root
                            // matches at its own seq and heals below. WHAT was
                            // published stays the equivalence law's question —
                            // this binding answers WHO published it.
                            let bound = chain_commitment_at(coordinator.chain(), root.published_at)
                                .is_some_and(|expected| expected == record.published_chain_hash);
                            if !describes_spine || !bound {
                                return Err(OpenError::SlotDisagreesWithStream {
                                    path: path.to_path_buf(),
                                    slot_manifest: ObjectId(slot.root_manifest_oid),
                                });
                            }
                            reopen_from_verified_checkpoint(
                                cx,
                                &coordinator,
                                &store,
                                &keys,
                                record.root,
                            )
                            .await?
                        }
                        _ => {
                            return Err(OpenError::SlotDisagreesWithStream {
                                path: path.to_path_buf(),
                                slot_manifest: ObjectId(slot.root_manifest_oid),
                            });
                        }
                    }
                }
                Err(SlotStoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                    rebuild(cx, &coordinator, &store, &keys).await?
                }
                Err(error) => return Err(OpenError::Slot(error)),
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
                if !validate_plain_slot(&slot, &keys) {
                    return Err(OpenError::ForeignSlot {
                        path: path.to_path_buf(),
                    });
                }
                if slot.root_manifest_oid == snapshot.manifest.0.0 {
                    slot.slot_generation
                } else {
                    let stale = ManifestVersion(ObjectId(slot.root_manifest_oid));
                    let resolvable = match store.resolve_manifest(cx, stale) {
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
                    let healed = slot.slot_generation + 1;
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
        // The index is derived from the FULL recovered chain, not the
        // checkpoint suffix the writer just folded. A suffix-only rebuild
        // would open a window starting at `published_at`, and the next
        // insert would see a gap (plan:397, FG-INV-18).
        let delta_index = rebuild_delta_index(cx, &coordinator).await?;
        Ok(Self {
            coordinator,
            store,
            slot_store,
            slot_generation,
            keys,
            state: DatabaseState::Healthy {
                published_frontier: snapshot.frontier,
            },
            snapshot,
            writer,
            // Deliberately empty rather than seeded from the rebuild: the first
            // publication's fallback re-earns every block's admission from disk
            // through the same checks, so an open session starts from proven
            // state without a second trust-bearing constructor (fgdb-gieu).
            receipts: PublishReceipts::new(),
            vfs,
            delta_index,
        })
    }

    /// The handle's truthfulness state. This is diagnostic state, not a
    /// recovery authority: a fenced handle stays fenced; a successful
    /// publication or a fresh [`Database::open`] is what yields `Healthy`.
    pub fn state(&self) -> DatabaseState {
        self.state
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
                let keys = self.keys;
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
        self.write_with_faults(cx, batch, None, None, None).await
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
        mut block_store_crash_at: Option<BlockStoreCrashPoint>,
    ) -> Result<CommitSeq, WriteError> {
        self.ensure_writable()?;
        if batch.is_empty() {
            return Err(WriteError::EmptyBatch);
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
                    let triple_live = triple_is_live(
                        &self.writer,
                        &prefix_edges,
                        &prefix_deleted_edges,
                        src,
                        dst,
                        batch.relation,
                    );
                    if ensure && triple_live {
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
                        .or_else(|| self.snapshot.versions.get(&ElementId::Edge(eid)))
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
                        .or_else(|| self.snapshot.versions.get(&ElementId::Vertex(vid)))
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
                    let actual = match elem {
                        ElementId::Vertex(vid) => {
                            let live_now = !prefix_deleted_vertices.contains(&vid)
                                && (prefix_content.contains_key(&vid)
                                    || prefix_versions.contains_key(&ElementId::Vertex(vid))
                                    || self.writer.is_vertex_live(vid));
                            if !live_now {
                                return Err(WriteError::UnknownVertex { vid });
                            }
                            let (_, props) =
                                vertex_content_entry(&mut prefix_content, &self.snapshot, vid);
                            let position = props.binary_search_by_key(&key, |(k, _)| *k);
                            position.ok().map(|at| props[at].1.clone())
                        }
                        ElementId::Edge(eid) => {
                            let live_now = !prefix_deleted_edges.contains(&eid)
                                && (prefix_edges.contains_key(&eid)
                                    || self.writer.live_edge(eid).is_some());
                            if !live_now {
                                return Err(WriteError::UnknownEdge { eid });
                            }
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

        let capsule = prepare_capsule(&self.keys.k_oid, self.keys.namespace, &template)?;
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
                let published_frontier = self.snapshot.frontier;
                self.state = DatabaseState::CommitOutcomeUnknown { published_frontier };
                return Err(WriteError::CommitOutcomeUnknown {
                    published_frontier,
                    source,
                });
            }
            Err(source) => return Err(WriteError::Commit(source)),
        };

        // Incremental snapshot maintenance (fgdb-fujt): the template in hand IS
        // the delta the durable commit just appended, so fold exactly it into
        // the retained writer instead of re-reading every historical capsule —
        // the loop ffe05f6 measured at 95% of an O(history) marginal write.
        // FG-INV-09's recompute-from-registered-bytes check is a re-READ law
        // and still runs on every path that reads capsules back (open,
        // recovery, the replica probe); this path never re-reads, it folds the
        // bytes it just made durable. Fold-then-swap: a failure leaves
        // `self.writer` at the pre-commit fold exactly as a rebuild failure
        // leaves the snapshot stale — reopen rebuilds from the stream.
        let frontier = marker_ref.commit_seq;
        let mut recovery = RecoveryRequired {
            durable_frontier: frontier,
            published_frontier: self.snapshot.frontier,
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
        self.delta_index
            .insert(batch)
            .map_err(|error| WriteError::CommittedNeedsRecovery {
                recovery,
                source: Box::new(RebuildError::Index {
                    commit_seq: frontier.0,
                    error,
                }),
            })?;
        Self::fail_publication_if_requested(recovery, publication_failure)?;
        let mut folded = self.writer.clone();
        let mut next_birth_ordinal = self.snapshot.next_birth_ordinal;
        let mut new_versions = self.snapshot.versions.clone();
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
        let (root, blocks, patches) = folded
            .clone()
            .publish(self.keys.block_keys(), frontier)
            .map_err(|error| WriteError::CommittedNeedsRecovery {
                recovery,
                source: Box::new(RebuildError::Fold {
                    commit_seq: frontier.0,
                    error,
                }),
            })?;
        // Strata-side incremental publish (fgdb-gieu): the sealed prefix of a
        // partition is immutable and content-addressed, so of everything the
        // clone-publish returned, only blocks this session has not already made
        // durable cost any I/O — `put_verified` skips receipted identities, and
        // `put_root_verified` admits receipted references without re-reading
        // them from disk. Measured pre-fix: every commit re-put every sealed
        // block (read + hash + two fsyncs each) and re-read + re-decoded the
        // whole partition twice more (admission, reopen) — O(blocks) disk work
        // per commit with no new information in it.
        self.mark_recovery_stage(&mut recovery, DerivedPublicationStage::PublishEdgeBlocks);
        Self::fail_publication_if_requested(recovery, publication_failure)?;
        for block in &blocks {
            let crash_at = if self.receipts.holds(DeltaBlockVersion(block.block_id)) {
                None
            } else {
                block_store_crash_at.take()
            };
            self.store
                .put_verified_with_crash(
                    cx,
                    &block.bytes,
                    block
                        .property_patch
                        .as_ref()
                        .map(|patch| patch.bytes.as_slice()),
                    &mut self.receipts,
                    crash_at,
                )
                .map_err(|error| WriteError::CommittedNeedsRecovery {
                    recovery,
                    source: Box::new(RebuildError::from(error)),
                })?;
        }
        self.mark_recovery_stage(&mut recovery, DerivedPublicationStage::PublishVertexPatches);
        Self::fail_publication_if_requested(recovery, publication_failure)?;
        for patch in &patches {
            self.store
                .put_patch_verified(cx, &patch.bytes, &mut self.receipts)
                .map_err(|error| WriteError::CommittedNeedsRecovery {
                    recovery,
                    source: Box::new(RebuildError::from(error)),
                })?;
        }
        self.mark_recovery_stage(&mut recovery, DerivedPublicationStage::PublishPartitionRoot);
        Self::fail_publication_if_requested(recovery, publication_failure)?;
        let root_id = self
            .store
            .put_root_verified(cx, &root, &mut self.receipts)
            .map_err(|error| WriteError::CommittedNeedsRecovery {
                recovery,
                source: Box::new(RebuildError::from(error)),
            })?;
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
        let manifest = self
            .store
            .put_manifest(cx, &manifest_records)
            .map_err(|error| WriteError::CommittedNeedsRecovery {
                recovery,
                source: Box::new(RebuildError::from(error)),
            })?;
        // The slot advances in the same publish (fgdb-ge6a): a crash before
        // this line leaves the slot exactly one publication behind, which is
        // the shape open() heals; there is no window where it runs ahead.
        let manifest_len = encode_manifest(&manifest_records)
            .map(|bytes| bytes.len() as u64)
            .expect("records_of already proved these records canonical");
        let next_generation = self.slot_generation + 1;
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
        // this same root and adjacency. Decode failures refuse here, before the
        // old snapshot is disturbed (fold-then-swap, as above).
        self.mark_recovery_stage(&mut recovery, DerivedPublicationStage::RefreshEdgeSnapshot);
        Self::fail_publication_if_requested(recovery, publication_failure)?;
        let mut fresh: std::collections::BTreeMap<
            ObjectId,
            (Vec<AdjacencyEntry>, Option<BlockProps>),
        > = std::collections::BTreeMap::new();
        let carried: std::collections::BTreeSet<ObjectId> =
            self.snapshot.refs.iter().map(|r| r.block_id).collect();
        for reference in &root.blocks {
            if carried.contains(&reference.block_id) || fresh.contains_key(&reference.block_id) {
                continue;
            }
            let sealed = blocks
                .iter()
                .find(|block| block.block_id == reference.block_id)
                .expect("every reference in a publish's root names a block that publish returned");
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
                    let rows = fgdb_strata::edge_props::decode_property_patch(&patch.bytes)
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
            fresh.insert(reference.block_id, (entries, props));
        }
        let mut carried: std::collections::BTreeMap<
            ObjectId,
            (Vec<AdjacencyEntry>, Option<BlockProps>),
        > = self
            .snapshot
            .refs
            .iter()
            .map(|r| r.block_id)
            .zip(
                std::mem::take(&mut self.snapshot.blocks)
                    .into_iter()
                    .zip(std::mem::take(&mut self.snapshot.block_props)),
            )
            .collect();
        let (decoded, decoded_props): (Vec<Vec<AdjacencyEntry>>, Vec<Option<BlockProps>>) = root
            .blocks
            .iter()
            .map(|reference| {
                carried
                    .remove(&reference.block_id)
                    .or_else(|| fresh.remove(&reference.block_id))
                    .expect(
                        "every root reference resolves: EIds are spend-once, so one \
                         publication cannot name the same block identity twice",
                    )
            })
            .unzip();
        // The identical carry-forward rule for the vertex half: an unchanged
        // patch reference means an unchanged decoded patch, and new patches
        // decode from the exact bytes `put_patch_verified` just fsynced.
        self.mark_recovery_stage(
            &mut recovery,
            DerivedPublicationStage::RefreshVertexSnapshot,
        );
        Self::fail_publication_if_requested(recovery, publication_failure)?;
        let mut fresh_patches: std::collections::BTreeMap<ObjectId, Vec<VertexRow>> =
            std::collections::BTreeMap::new();
        let carried_patch_ids: std::collections::BTreeSet<ObjectId> = self
            .snapshot
            .patch_refs
            .iter()
            .map(|r| r.patch_id)
            .collect();
        for reference in &root.vertex_patches {
            if carried_patch_ids.contains(&reference.patch_id)
                || fresh_patches.contains_key(&reference.patch_id)
            {
                continue;
            }
            let sealed = patches
                .iter()
                .find(|patch| patch.patch_id == reference.patch_id)
                .expect("every reference in a publish's root names a patch that publish returned");
            fresh_patches.insert(
                reference.patch_id,
                fgdb_strata::vertex::decode_patch(&sealed.bytes).map_err(|error| {
                    WriteError::CommittedNeedsRecovery {
                        recovery,
                        source: Box::new(RebuildError::Store(StoreError::MalformedPatch(error))),
                    }
                })?,
            );
        }
        let mut carried_patches: std::collections::BTreeMap<ObjectId, Vec<VertexRow>> = self
            .snapshot
            .patch_refs
            .iter()
            .map(|r| r.patch_id)
            .zip(std::mem::take(&mut self.snapshot.patches))
            .collect();
        let decoded_patches = root
            .vertex_patches
            .iter()
            .map(|reference| {
                carried_patches
                    .remove(&reference.patch_id)
                    .or_else(|| fresh_patches.remove(&reference.patch_id))
                    .expect(
                        "every root patch reference resolves: VIds are spend-once, so one \
                         publication cannot name the same patch identity twice",
                    )
            })
            .collect();
        self.writer = folded;
        self.snapshot = Snapshot {
            blocks: decoded,
            block_props: decoded_props,
            refs: root.blocks,
            patches: decoded_patches,
            patch_refs: root.vertex_patches,
            frontier,
            root: root_id,
            manifest,
            next_birth_ordinal,
            versions: new_versions,
        };
        self.state = DatabaseState::Healthy {
            published_frontier: self.snapshot.frontier,
        };
        Ok(self.snapshot.frontier)
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
        self.check_frontier(as_of)?;
        Ok(merge_in_neighbours(
            &self.snapshot.blocks,
            dst,
            relation,
            as_of,
        )?)
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
        self.check_frontier(as_of)?;
        Ok(merge_neighbours(
            &self.snapshot.blocks,
            src,
            relation,
            as_of,
        )?)
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
        self.check_frontier(as_of)?;
        Ok(merge_edge_with_props(
            &self.snapshot.blocks,
            &self.snapshot.block_props,
            eid,
            as_of,
        )?
        .map(|(entry, props)| EdgeRecord { entry, props }))
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
        self.check_frontier(as_of)?;
        Ok(merge_vertex(&self.snapshot.patches, vid, as_of))
    }

    /// Every vertex visible at the published frontier, in ascending VId
    /// order — the whole-graph scan a query layer starts from (fgdb-9k5w).
    pub fn vertices(&self) -> Result<Vec<VertexRow>, ReadError> {
        self.vertices_at(self.snapshot.frontier)
    }

    /// [`Database::vertices`] as of `as_of`, under the same frontier refusal
    /// as every `*_at` read.
    pub fn vertices_at(&self, as_of: CommitSeq) -> Result<Vec<VertexRow>, ReadError> {
        self.check_frontier(as_of)?;
        Ok(merge_all_vertices(&self.snapshot.patches, as_of))
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
        self.check_frontier(as_of)?;
        Ok(
            merge_all_edges_with_props(&self.snapshot.blocks, &self.snapshot.block_props, as_of)?
                .into_iter()
                .map(|(entry, props)| EdgeRecord { entry, props })
                .collect(),
        )
    }

    /// The shared refusal every `*_at` read applies before answering: the
    /// published frontier bounds what this snapshot can truthfully say.
    fn check_frontier(&self, as_of: CommitSeq) -> Result<(), ReadError> {
        self.ensure_readable()?;
        if as_of.0 > self.snapshot.frontier.0 {
            return Err(ReadError::BeyondFrontier {
                asked: as_of,
                frontier: self.snapshot.frontier,
            });
        }
        Ok(())
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
    pub fn delta_index(&self) -> Result<&LocalDeltaBatchIndex, ReadError> {
        self.ensure_readable()?;
        Ok(&self.delta_index)
    }

    /// How far the derived delta window reaches. After a successful write
    /// this equals the new [`CommitSeq`]; after a fresh create it is the
    /// origin. Distinct from [`Database::frontier`] only in name — both
    /// report the same sequence on a healthy handle — so Ripple/CDC can
    /// ask for the window without importing the snapshot's vocabulary.
    pub fn delta_frontier(&self) -> Result<CommitSeq, ReadError> {
        self.ensure_readable()?;
        Ok(self.delta_index.frontier())
    }

    /// The retained committed batches strictly after `after`, in commit order.
    ///
    /// This is the live frontier-stream face (og6n subset): a consumer names
    /// the last sequence it has applied and receives the gap-free suffix, or
    /// a refusal. A cursor past the frontier is [`ReadError::BeyondFrontier`];
    /// a cursor below the retained floor is [`ReadError::DeltaCursorRetired`].
    /// Reads check `Healthy` like every other graph read.
    pub fn delta_since(
        &self,
        after: CommitSeq,
    ) -> Result<impl Iterator<Item = &LogicalDeltaBatch> + '_, ReadError> {
        self.ensure_readable()?;
        self.delta_index.since(after).map_err(|error| match error {
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
        })
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
    pub async fn compact(&mut self, cx: &CommitCx) -> Result<(), RebuildError> {
        if !matches!(self.state, DatabaseState::Healthy { .. }) {
            return Err(RebuildError::HandleNotHealthy(self.state));
        }
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
                        &self.keys.k_oid,
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
            let block_id = fgdb_strata::block_id(&self.keys.k_oid, self.keys.namespace, &bytes);
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
            fgdb_strata::compact::compact_vertex_patches(&self.snapshot.patches, CommitSeq(0));
        let mut sealed_patches = Vec::with_capacity(compacted_patches.len());
        for rows in &compacted_patches {
            let bytes = fgdb_strata::vertex::encode_patch(rows)
                .map_err(|error| RebuildError::Store(StoreError::MalformedPatch(error)))?;
            let (first_seq, last_seq) =
                fgdb_strata::vertex::span_of_rows(rows).expect("the packer emits no empty patches");
            sealed_patches.push(fgdb_strata::writer::SealedPatch {
                patch_id: fgdb_strata::vertex::vertex_patch_id(
                    &self.keys.k_oid,
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

        // The logical state is unchanged, so versions and the allocator pass
        // through; the shared tail republishes and reopens from disk.
        let versions = self.snapshot.versions.clone();
        let next_birth_ordinal = self.snapshot.next_birth_ordinal;
        let published_chain_hash = chain_commitment_at(self.coordinator.chain(), frontier)
            .expect("a healthy handle's frontier is on its own recovered chain");
        let (snapshot, writer) = publish_and_snapshot(
            cx,
            &self.store,
            &self.keys,
            writer,
            versions,
            frontier,
            next_birth_ordinal,
            published_chain_hash,
        )?;
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
        let next_generation = self.slot_generation + 1;
        self.slot_store
            .publish_evidenced(
                cx,
                &spine_slot(&self.keys, next_generation, snapshot.manifest, manifest_len),
            )
            .await
            .map_err(RebuildError::Slot)?;
        self.slot_generation = next_generation;
        self.snapshot = snapshot;
        self.writer = writer;
        // Receipts describe the superseded generation; the replacement earns
        // its own on the next publish.
        self.receipts = PublishReceipts::new();
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

/// The elements one row touches for the version chain — deletes touch to
/// REMOVE, everything else to advance; the cascade names its members.
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
    labels: &mut [LabelId],
    props: &mut [(PropertyKeyId, CanonicalScalar)],
) {
    labels.sort_unstable();
    sort_write_props(props);
}

fn sort_write_props(props: &mut [(PropertyKeyId, CanonicalScalar)]) {
    props.sort_by_key(|(key, _)| *key);
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
#[derive(Clone, Debug)]
pub struct PreparedCapsule {
    pub bytes: Vec<u8>,
    pub object_id: ObjectId,
    pub template_digest: Digest,
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
    patches: &[Vec<VertexRow>],
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
    store: &BlockStore,
    keys: &DatabaseKeys,
    root_id: PartitionRootVersion,
) -> Result<(Snapshot, BlockWriter), RebuildError> {
    let (root, blocks, block_props, patches) = store.reopen(cx, root_id)?;
    let published_at = root.published_at;

    // The sealed lists a retained writer would hold: bytes re-read from the
    // store, which verifies identity on every read.
    let mut sealed = Vec::with_capacity(root.blocks.len());
    for (reference, decoded) in root.blocks.iter().zip(&blocks) {
        let bytes = store.get_bytes(cx, fgdb_strata::DeltaBlockVersion(reference.block_id))?;
        let property_patch = match fgdb_strata::decode_block_with_properties(&bytes)
            .map_err(StoreError::Malformed)?
            .1
        {
            Some((patch_id, _)) => Some(fgdb_strata::writer::SealedPropertyPatch {
                patch_id,
                bytes: store.get_edge_property_patch_bytes(cx, patch_id)?,
            }),
            None => None,
        };
        let _ = decoded;
        sealed.push(fgdb_strata::writer::SealedBlock {
            block_id: reference.block_id,
            bytes,
            first_seq: reference.first_seq,
            last_seq: reference.last_seq,
            property_patch,
        });
    }
    let mut sealed_patches = Vec::with_capacity(root.vertex_patches.len());
    for reference in &root.vertex_patches {
        sealed_patches.push(fgdb_strata::writer::SealedPatch {
            patch_id: reference.patch_id,
            bytes: store.get_patch_bytes(
                cx,
                fgdb_strata::vertex::VertexPatchVersion(reference.patch_id),
            )?,
            first_seq: reference.first_seq,
            last_seq: reference.last_seq,
        });
    }

    let mut writer = BlockWriter::from_published_partition(
        GRAPH,
        BRANCH,
        PARTITION,
        sealed,
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
        &mut writer,
        &mut versions,
        &mut next_birth_ordinal,
        published_at,
    )
    .await?;

    if frontier.0 > published_at.0 {
        // The suffix advanced the fold: republish through the shared tail so
        // the durable root/manifest catch up (the slot heals in bind).
        let published_chain_hash = chain_commitment_at(coordinator.chain(), frontier)
            .expect("the fold's frontier is on the recovered chain it folded");
        let result = publish_and_snapshot(
            cx,
            store,
            keys,
            writer,
            versions,
            frontier,
            next_birth_ordinal,
            published_chain_hash,
        );
        return result;
    }

    // No suffix: the partition IS current, and the snapshot assembles from
    // what the reopen already decoded — no publish, no O(blocks) writes.
    let published_chain_hash = chain_commitment_at(coordinator.chain(), published_at)
        .expect("bind verified this publication against the recovered chain");
    let manifest_records = records_of(&[(root.clone(), root_id, published_chain_hash)])
        .expect("one root is one canonical record");
    let manifest_bytes =
        encode_manifest(&manifest_records).expect("records_of proved these records canonical");
    let manifest = ManifestVersion(fgdb_strata::manifest::manifest_id(
        &keys.k_oid,
        keys.namespace,
        &manifest_bytes,
    ));
    Ok((
        Snapshot {
            blocks,
            refs: root.blocks,
            block_props,
            patches,
            patch_refs: root.vertex_patches,
            frontier: published_at,
            root: root_id,
            manifest,
            next_birth_ordinal,
            versions,
        },
        writer,
    ))
}

async fn rebuild<V: Vfs>(
    cx: &CommitCx,
    coordinator: &CommitCoordinator<V>,
    store: &BlockStore,
    keys: &DatabaseKeys,
) -> Result<(Snapshot, BlockWriter), RebuildError> {
    let mut writer = BlockWriter::new(GRAPH, BRANCH, PARTITION);
    let mut next_birth_ordinal = 0u64;
    let mut versions = std::collections::BTreeMap::new();
    let frontier = fold_stream(
        cx,
        coordinator,
        keys,
        &mut writer,
        &mut versions,
        &mut next_birth_ordinal,
        CommitSeq(0),
    )
    .await?;
    let published_chain_hash = chain_commitment_at(coordinator.chain(), frontier)
        .expect("the fold's frontier is on the recovered chain it folded");
    publish_and_snapshot(
        cx,
        store,
        keys,
        writer,
        versions,
        frontier,
        next_birth_ordinal,
        published_chain_hash,
    )
}

/// Rebuild the derived delta window from the FULL recovered marker chain.
///
/// Checkpoint-selected open folds only the suffix into the writer. The
/// index must still cover `(0, frontier]`: a suffix-only window would
/// start at `published_at` and the next [`LocalDeltaBatchIndex::insert`]
/// would refuse as a gap. The index is never a second source of truth
/// (FG-INV-18); every open reconstructs it from capsules the markers name.
async fn rebuild_delta_index<V: Vfs>(
    cx: &CommitCx,
    coordinator: &CommitCoordinator<V>,
) -> Result<LocalDeltaBatchIndex, RebuildError> {
    let mut index = LocalDeltaBatchIndex::new();
    for entry in coordinator.chain().entries() {
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
        let bytes = coordinator.read_capsule(cx, *capsule_ref).await?;
        let recomputed = template_digest(&bytes);
        // The annotation must sit on the line immediately above the
        // comparison: UBS anchors it to the next line.
        // ubs:ignore -- non-secret content digest over local capsule bytes, not authentication material.
        if recomputed != *logical_delta_template_digest {
            return Err(RebuildError::TemplateDigestMismatch {
                commit_seq: commit_seq.0,
                declared: *logical_delta_template_digest,
                recomputed,
            });
        }
        let template = LogicalDeltaTemplate::decode_canonical(&bytes).map_err(|error| {
            RebuildError::Decode {
                commit_seq: commit_seq.0,
                error,
            }
        })?;
        let batch = LogicalDeltaBatch::order(
            &template,
            logical_delta_template_digest.0,
            CommittedMarker::attest(
                MarkerRef {
                    marker_oid: entry.marker_oid,
                    commit_seq,
                },
                cx,
            ),
        );
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
async fn fold_stream<V: Vfs>(
    cx: &CommitCx,
    coordinator: &CommitCoordinator<V>,
    keys: &DatabaseKeys,
    writer: &mut BlockWriter,
    versions: &mut std::collections::BTreeMap<ElementId, ObjectId>,
    next_birth_ordinal: &mut u64,
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
        let bytes = coordinator.read_capsule(cx, *capsule_ref).await?;
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
        let template = LogicalDeltaTemplate::decode_canonical(&bytes).map_err(|error| {
            RebuildError::Decode {
                commit_seq: commit_seq.0,
                error,
            }
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
                    *next_birth_ordinal += 1;
                }
                writer
                    .apply(keys.block_keys(), commit_seq, row)
                    .map_err(|error| RebuildError::Fold {
                        commit_seq: commit_seq.0,
                        error,
                    })?;
                touched_elements(row, &mut touched);
            }
        }
        fold_statement_versions(versions, &touched, writer).map_err(|error| {
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
        writer
            .seal(keys.block_keys())
            .map_err(|error| RebuildError::Fold {
                commit_seq: commit_seq.0,
                error,
            })?;
        writer
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
#[allow(clippy::too_many_arguments)]
fn publish_and_snapshot(
    cx: &CommitCx,
    store: &BlockStore,
    keys: &DatabaseKeys,
    writer: BlockWriter,
    versions: std::collections::BTreeMap<ElementId, ObjectId>,
    frontier: CommitSeq,
    next_birth_ordinal: u64,
    published_chain_hash: Digest,
) -> Result<(Snapshot, BlockWriter), RebuildError> {
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
    for block in &blocks {
        if let Some(patch) = &block.property_patch {
            store.put_edge_property_patch(cx, &patch.bytes)?;
        }
        store.put(cx, &block.bytes)?;
    }
    for patch in &patches {
        store.put_patch(cx, &patch.bytes)?;
    }
    let root_id = store.put_root(cx, &root)?;
    let manifest_records = records_of(&[(root.clone(), root_id, published_chain_hash)])
        .expect("one root is one canonical record");
    let manifest = store.put_manifest(cx, &manifest_records)?;
    let (reopened_root, decoded, decoded_props, decoded_patches) = store.reopen(cx, root_id)?;

    Ok((
        Snapshot {
            blocks: decoded,
            refs: reopened_root.blocks,
            block_props: decoded_props,
            patches: decoded_patches,
            patch_refs: reopened_root.vertex_patches,
            frontier,
            root: root_id,
            manifest,
            next_birth_ordinal,
            versions,
        },
        writer,
    ))
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
