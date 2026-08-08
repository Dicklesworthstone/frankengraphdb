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
//! # use asupersync::lab::run_async_under_lab;
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
//! # let (outcome, report) = run_async_under_lab(7, move |root| async move {
//! # let cx = &PurposeContexts::narrow_runtime_root(&root).commit();
//! # let path = &path;
//! let mut db = Database::create(cx, path, keys).await?;
//! let mut batch = WriteBatch::new(RelationId(1));
//! batch.create_vertex(VId(1), vec![], vec![]);
//! batch.create_vertex(VId(2), vec![], vec![]);
//! batch.add_edge(EId(10), VId(1), VId(2), vec![]);
//! db.write(cx, batch).await?;                 // real capsule, real marker, two fsyncs
//! assert_eq!(db.neighbours(VId(1), RelationId(1))?, vec![VId(2)]);
//! drop(db);
//! let db = Database::open(cx, path, keys).await?;   // nothing carried but the path and the keys
//! assert_eq!(db.neighbours(VId(1), RelationId(1))?, vec![VId(2)]);
//! # Ok::<(), Box<dyn core::error::Error + Send + Sync>>(())
//! # });
//! # outcome.expect("the documented example must actually run");
//! # assert!(report.lab_test_passed(), "lab run failed: {report:?}");
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
//! # Why reopen replays the commit stream, and why that is not a gap
//!
//! `RootSlot.root_manifest_oid` points at an object nobody has defined yet
//! (`fgdb-ge6a`), so there is no durable index from "here is a database
//! directory" to "here are its partition roots". This crate therefore does not
//! use one. [`Database::open`] recovers the commit stream — which IS reachable
//! from the directory alone, via `CommitCoordinator::open` — and REBUILDS the
//! tier-D fold from it.
//!
//! That is not a workaround for the missing manifest; it is doctrine 5 and
//! FG-INV-18's primary path, spelled out: *derived structures are never more
//! authoritative than the commit stream, and recovery discards and rebuilds
//! them.* Adjacency blocks are derived. What `fgdb-ge6a` will add is the FAST
//! path — resolving published roots directly so a reopen need not replay — and
//! when it lands, [`Database::open`] gains a fast path and keeps this one as the
//! authority it is checked against.
//!
//! The cost is stated rather than hidden: **rebuilding is O(history), and it runs
//! on open AND after every write.** Incremental publication is part of tier-D's
//! writer lifecycle (`fgdb-w3-tier-d-ctj`) and is deliberately not invented here.
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

/// **Tripwire: the production `Cx` path is still closed upstream** (`fgdb-r8fa`).
///
/// `fgdb-0b8r` established that no external crate can obtain a `Cx` in a
/// production build at asupersync `3e8d08e`, which is why
/// `examples/open_a_database.rs` drives the spine through the LAB runtime. That
/// is a fact about a pinned dependency, and facts about pinned dependencies rot
/// silently: the revision gets bumped, the path opens, and nobody notices that
/// the workaround is now unnecessary.
///
/// These doctests are that notice. They assert the path is CLOSED, so they start
/// failing the moment it opens — a `compile_fail` test fails by compiling. When
/// either one goes red, the fix is not to delete it: swap `run_async_under_lab`
/// for the runtime entry in the example and in any production caller, then
/// retire this module and close `fgdb-r8fa`.
///
/// **What is closed is the NON-ESCALATING path, not every path.**
/// `Cx::for_testing` is reachable here — `test-internals` is a real asupersync
/// feature and this crate enables it under `[dev-dependencies]`, which is why
/// tests, examples and doctests can construct one. That is deliberately NOT used:
/// it mints `Budget::INFINITE` with full capabilities and inherits no runtime
/// cap-mask, which is the "external-crate capability injection" escape
/// asupersync's own doc warns about. Using it would satisfy the type checker and
/// break doctrine 6.
///
/// So there is no tripwire on `for_testing` — it is available and must stay
/// unused. The tripwire is on the path that would let us stop using the lab
/// runtime honestly: `Runtime::request_cx_with_budget`, which asupersync's
/// documentation calls "the only ambient-free way to mint a Cx in production"
/// and which is declared `pub(crate)`:
///
/// ```compile_fail
/// let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
/// let _cx = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
/// ```
#[cfg(doctest)]
mod production_cx_path_tripwire {}

use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::{CAPSULE_DIR, CommitCoordinator, CommitError};
use fgdb_chronicle::identity::IdentifiedObject;
use fgdb_chronicle::marker::{CommitMarker, EffectSource, HeadUpdate};
use fgdb_crypto::Digest;
use fgdb_delta_types::{
    CanonicalError, CoordinateEntry, DeltaRow, ElementId, LabelId, LogicalDeltaTemplate,
    PropertyKeyId, RelationId, SchemaEpoch,
};
use fgdb_strata::root::{BlockRef, PatchRef, RootError, merge_edge, merge_neighbours};
use fgdb_strata::store::{BlockStore, PublishReceipts, StoreError};
use fgdb_strata::vertex::merge_vertex;
use fgdb_strata::writer::{BlockWriter, WriteError as BlockWriteError};
use fgdb_strata::{AdjacencyEntry, PartitionRootVersion};

pub use fgdb_strata::vertex::VertexRow;
use fgdb_types::context::CommitCx;
use fgdb_types::ids::{DatabaseSecurityNamespaceId, ObjectId};
use fgdb_types::{BranchId, CanonicalScalar, CommitSeq, EId, GraphId, VId};
use std::path::{Path, PathBuf};

/// Re-exported because [`Database::write_with_crash`] takes one: a caller
/// driving the crash-point matrix needs to name the instants, and importing them
/// from Chronicle directly would make the spine's own signature unusable without
/// a second dependency.
pub use fgdb_chronicle::commit::CrashPoint;

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
    Commit(CommitError),
    Store(StoreError),
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
    Canonical(CanonicalError),
    Commit(CommitError),
    /// The write was durable, and republishing the derived partition failed.
    /// The commit is NOT lost — a reopen rebuilds it from the stream.
    Rebuild(RebuildError),
}

/// Why a read could not be served.
#[derive(Debug)]
pub enum ReadError {
    Root(RootError),
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
from_error!(WriteError, Rebuild, RebuildError);
from_error!(ReadError, Root, RootError);

impl core::fmt::Display for OpenError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotADirectory { path } => {
                write!(f, "{} exists and is not a directory", path.display())
            }
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
            Self::Commit(error) => write!(f, "commit stream: {error}"),
            Self::Store(error) => write!(f, "block store: {error}"),
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
            Self::Canonical(error) => write!(f, "canonical form: {error}"),
            Self::Commit(error) => write!(f, "commit stream: {error}"),
            Self::Rebuild(error) => write!(
                f,
                "the commit is durable but the derived partition did not republish: {error}"
            ),
        }
    }
}

impl core::fmt::Display for ReadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Root(error) => write!(f, "partition: {error}"),
        }
    }
}

impl core::error::Error for OpenError {}
impl core::error::Error for RebuildError {}
impl core::error::Error for WriteError {}
impl core::error::Error for ReadError {}

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
    },
    Edge {
        eid: EId,
        src: VId,
        dst: VId,
        props: Vec<(PropertyKeyId, CanonicalScalar)>,
    },
    DeleteEdge {
        eid: EId,
    },
    DeleteVertex {
        vid: VId,
    },
    SetLabel {
        vid: VId,
        label: LabelId,
        member: bool,
    },
    SetProperty {
        vid: VId,
        key: PropertyKeyId,
        value: Option<CanonicalScalar>,
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
        self.rows.push(PendingRow::Vertex { vid, labels, props });
        self
    }

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
        });
        self
    }

    /// Delete the edge `eid`. The durable row's `before_version` is derived
    /// by the engine at commit time; deleting an edge this database does not
    /// hold refuses before anything durable happens.
    pub fn delete_edge(&mut self, eid: EId) -> &mut Self {
        self.rows.push(PendingRow::DeleteEdge { eid });
        self
    }

    /// Delete the vertex `vid` and every edge touching it. The cascade
    /// before-image — the exact incident set, both directions, ascending —
    /// and the `before_version` are derived by the engine at commit time.
    pub fn delete_vertex(&mut self, vid: VId) -> &mut Self {
        self.rows.push(PendingRow::DeleteVertex { vid });
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

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }
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
    /// The decoded vertex row patches, aligned with `patch_refs` — the vertex
    /// half of the snapshot (fgdb-3xoi), under the same carry-forward rule.
    patches: Vec<Vec<VertexRow>>,
    patch_refs: Vec<PatchRef>,
    frontier: CommitSeq,
    root: PartitionRootVersion,
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
#[derive(Debug)]
pub struct Database {
    coordinator: CommitCoordinator,
    store: BlockStore,
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
    /// Durability-and-admission receipts for the blocks this session has
    /// already published (fgdb-gieu). Session-scoped like the writer above,
    /// and with the same trust story: never authoritative, never persisted —
    /// a fresh process re-earns every proof from disk via the receipts'
    /// fallback path on its first publication.
    receipts: PublishReceipts,
}

impl Database {
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
        Self::bind(cx, path, keys).await
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
        Self::bind(cx, path, keys).await
    }

    /// Open the commit stream and the block store, then rebuild the fold.
    ///
    /// The database-ness decision belongs to the two callers above; by here it
    /// has been made.
    async fn bind(cx: &CommitCx, path: &Path, keys: DatabaseKeys) -> Result<Self, OpenError> {
        let coordinator = CommitCoordinator::open(cx, path, keys.capsule_keys()).await?;
        let store = BlockStore::open(cx, path, keys.k_oid, keys.namespace)?;
        let (snapshot, writer) = rebuild(cx, &coordinator, &store, &keys).await?;
        Ok(Self {
            coordinator,
            store,
            keys,
            snapshot,
            writer,
            // Deliberately empty rather than seeded from the rebuild: the first
            // publication's fallback re-earns every block's admission from disk
            // through the same checks, so an open session starts from proven
            // state without a second trust-bearing constructor (fgdb-gieu).
            receipts: PublishReceipts::new(),
        })
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
        self.write_with_crash(cx, batch, None).await
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
        if batch.is_empty() {
            return Err(WriteError::EmptyBatch);
        }

        // Build durable rows SEQUENTIALLY, deriving every delete's
        // before-image from the fold's live state PLUS the batch prefix: a
        // create-then-delete in one batch must image the version the create
        // just minted, exactly as the oracle will re-derive it at replay.
        // Deletes take no birth ordinal — only creations spend one.
        let mut rows = Vec::with_capacity(batch.rows.len());
        let mut birth_ordinal = self.snapshot.next_birth_ordinal;
        // The batch-prefix overlay: identities this batch created or deleted
        // ahead of the row being built, with the versions the prefix minted.
        let mut prefix_versions: std::collections::BTreeMap<ElementId, ObjectId> =
            std::collections::BTreeMap::new();
        let mut prefix_edges: std::collections::BTreeMap<EId, (VId, VId)> =
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
        for pending in batch.rows {
            let row = match pending {
                PendingRow::Vertex { vid, labels, props } => {
                    let row = DeltaRow::CreateVertex {
                        vid,
                        birth_ordinal,
                        labels,
                        props,
                        valid_time: None,
                    };
                    birth_ordinal += 1;
                    prefix_versions.insert(ElementId::Vertex(vid), successor_version(None, &row)?);
                    if let DeltaRow::CreateVertex { labels, props, .. } = &row {
                        prefix_content.insert(vid, (labels.clone(), props.clone()));
                    }
                    row
                }
                PendingRow::Edge {
                    eid,
                    src,
                    dst,
                    props,
                } => {
                    let row = DeltaRow::CreateEdge {
                        eid,
                        birth_ordinal,
                        src,
                        relation: batch.relation,
                        dst,
                        canonical_key: None,
                        props,
                        valid_time: None,
                    };
                    birth_ordinal += 1;
                    prefix_versions.insert(ElementId::Edge(eid), successor_version(None, &row)?);
                    prefix_edges.insert(eid, (src, dst));
                    row
                }
                PendingRow::DeleteEdge { eid } => {
                    let live_now = !prefix_deleted_edges.contains(&eid)
                        && (prefix_edges.contains_key(&eid)
                            || self.writer.live_edge(eid).is_some());
                    if !live_now {
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
                PendingRow::DeleteVertex { vid } => {
                    let live_now = !prefix_deleted_vertices.contains(&vid)
                        && (prefix_versions.contains_key(&ElementId::Vertex(vid))
                            || self.writer.is_vertex_live(vid));
                    if !live_now {
                        return Err(WriteError::UnknownVertex { vid });
                    }
                    let before_version = prefix_versions
                        .get(&ElementId::Vertex(vid))
                        .or_else(|| self.snapshot.versions.get(&ElementId::Vertex(vid)))
                        .copied()
                        .expect("a live vertex always has a version chain head");
                    // The cascade image: every live incident edge at THIS
                    // point in the batch — the fold's live set, minus the
                    // prefix's deletions, plus the prefix's incident
                    // creations, in ascending-EId order (the reference
                    // semantics, both directions).
                    let mut cascade: std::collections::BTreeSet<EId> =
                        self.writer.live_incident_edges(vid).into_iter().collect();
                    cascade.retain(|eid| !prefix_deleted_edges.contains(eid));
                    for (eid, (src, dst)) in &prefix_edges {
                        if !prefix_deleted_edges.contains(eid) && (*src == vid || *dst == vid) {
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
                    let elem = ElementId::Vertex(vid);
                    let previous = prefix_versions
                        .get(&elem)
                        .or_else(|| self.snapshot.versions.get(&elem))
                        .copied()
                        .expect("a live vertex always has a version chain head");
                    prefix_versions.insert(elem, successor_version(Some(previous), &row)?);
                    row
                }
                PendingRow::SetProperty { vid, key, value } => {
                    let live_now = !prefix_deleted_vertices.contains(&vid)
                        && (prefix_content.contains_key(&vid)
                            || prefix_versions.contains_key(&ElementId::Vertex(vid))
                            || self.writer.is_vertex_live(vid));
                    if !live_now {
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
                    let elem = ElementId::Vertex(vid);
                    let previous = prefix_versions
                        .get(&elem)
                        .or_else(|| self.snapshot.versions.get(&elem))
                        .copied()
                        .expect("a live vertex always has a version chain head");
                    prefix_versions.insert(elem, successor_version(Some(previous), &row)?);
                    row
                }
            };
            rows.push(row);
        }

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
        self.coordinator
            .commit_with_crash(
                cx,
                &capsule.bytes,
                |seq, oid| marker_for_capsule(seq, oid, &capsule, Vec::new()),
                crash_at,
            )
            .await?;

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
        let frontier = CommitSeq(
            self.coordinator
                .chain()
                .entries()
                .last()
                .expect("the commit that just succeeded is in the chain")
                .marker
                .commit_seq,
        );
        let mut folded = self.writer.clone();
        let mut next_birth_ordinal = self.snapshot.next_birth_ordinal;
        let mut new_versions = self.snapshot.versions.clone();
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
                    .map_err(|error| RebuildError::Fold {
                        commit_seq: frontier.0,
                        error,
                    })?;
                fold_version(&mut new_versions, row)?;
            }
        }
        let (root, blocks, patches) = folded
            .clone()
            .publish(self.keys.block_keys(), frontier)
            .map_err(|error| RebuildError::Fold {
                commit_seq: frontier.0,
                error,
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
        for block in &blocks {
            self.store
                .put_verified(
                    cx,
                    &block.bytes,
                    block
                        .property_patch
                        .as_ref()
                        .map(|patch| patch.bytes.as_slice()),
                    &mut self.receipts,
                )
                .map_err(RebuildError::from)?;
        }
        for patch in &patches {
            self.store
                .put_patch_verified(cx, &patch.bytes, &mut self.receipts)
                .map_err(RebuildError::from)?;
        }
        let root_id = self
            .store
            .put_root_verified(cx, &root, &mut self.receipts)
            .map_err(RebuildError::from)?;

        // Refresh the snapshot without re-reading the partition: carry forward
        // the decoded blocks whose references are unchanged, and decode the new
        // ones from the exact bytes `put_verified` just content-addressed and
        // fsynced. The encode→address→fsync→decode round trip rebuild's doc
        // demands still happens — over the in-memory bytes the disk now holds —
        // and `incremental_snapshot.rs` pins that a from-scratch reopen derives
        // this same root and adjacency. Decode failures refuse here, before the
        // old snapshot is disturbed (fold-then-swap, as above).
        let mut fresh: std::collections::BTreeMap<ObjectId, Vec<AdjacencyEntry>> =
            std::collections::BTreeMap::new();
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
            fresh.insert(
                reference.block_id,
                fgdb_strata::decode_block(&sealed.bytes)
                    .map_err(|error| RebuildError::Store(StoreError::Malformed(error)))?,
            );
        }
        let mut carried: std::collections::BTreeMap<ObjectId, Vec<AdjacencyEntry>> = self
            .snapshot
            .refs
            .iter()
            .map(|r| r.block_id)
            .zip(std::mem::take(&mut self.snapshot.blocks))
            .collect();
        let decoded = root
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
            .collect();
        // The identical carry-forward rule for the vertex half: an unchanged
        // patch reference means an unchanged decoded patch, and new patches
        // decode from the exact bytes `put_patch_verified` just fsynced.
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
                fgdb_strata::vertex::decode_patch(&sealed.bytes)
                    .map_err(|error| RebuildError::Store(StoreError::MalformedPatch(error)))?,
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
            refs: root.blocks,
            patches: decoded_patches,
            patch_refs: root.vertex_patches,
            frontier,
            root: root_id,
            next_birth_ordinal,
            versions: new_versions,
        };
        Ok(self.snapshot.frontier)
    }

    /// The live destinations of `src` over `relation`, at the published
    /// frontier.
    pub fn neighbours(&self, src: VId, relation: RelationId) -> Result<Vec<VId>, ReadError> {
        Ok(merge_neighbours(
            &self.snapshot.blocks,
            src,
            relation,
            self.snapshot.frontier,
        )?)
    }

    /// The edge `eid` — its endpoints, relation, and lifetime — at the
    /// published frontier, or `None` when no visible version exists.
    ///
    /// Served from the durable tier-D blocks exactly as
    /// [`Database::neighbours`] is, under the same whole-history validation.
    /// Properties are deliberately absent from the answer: edge property
    /// STORAGE is `fgdb-w3-properties-gou`'s block-hosted patch shape
    /// (ruling fgdb-2t7q 3B), and answering them from an in-memory fold here
    /// would be the shortcut across the durable path this crate forbids.
    pub fn edge(&self, eid: EId) -> Result<Option<AdjacencyEntry>, ReadError> {
        Ok(merge_edge(
            &self.snapshot.blocks,
            eid,
            self.snapshot.frontier,
        )?)
    }

    /// The vertex `vid` — its labels and properties — at the published
    /// frontier, or `None` when no visible row exists (fgdb-3xoi).
    ///
    /// Served from the durable tier-D vertex patches the snapshot decoded,
    /// exactly as [`Database::neighbours`] is served from blocks: the row made
    /// the full encode → content-address → fsync → decode round trip before a
    /// reader can see it.
    pub fn vertex(&self, vid: VId) -> Option<VertexRow> {
        merge_vertex(&self.snapshot.patches, vid, self.snapshot.frontier)
    }

    /// The sequence the published partition has caught up to.
    pub fn frontier(&self) -> CommitSeq {
        self.snapshot.frontier
    }

    /// The identity of the published partition root.
    ///
    /// Exposed because the rebuild is deterministic and content-addressed, so
    /// "reopening the same stream publishes the same root" is a law a caller can
    /// assert rather than a property the crate merely claims.
    pub fn partition_root(&self) -> PartitionRootVersion {
        self.snapshot.root
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
const ELEMENT_VERSION_DOMAIN: &[u8] = b"fgdb.reference.element-version.v1";

/// Extend one element's version chain with one canonical effect — the
/// engine's independent spelling of the reference derivation: a domain, a
/// predecessor tag distinguishing creation from an all-zero prior digest, a
/// self-delimiting row length, and the row's canonical bytes. No branch
/// population, wall clock, or commit sequence enters it.
fn successor_version(
    previous: Option<ObjectId>,
    row: &DeltaRow,
) -> Result<ObjectId, CanonicalError> {
    let canonical = row.canonical_bytes()?;
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
    hasher.update(&(canonical.len() as u64).to_le_bytes());
    hasher.update(&canonical);
    Ok(ObjectId(hasher.finalize().0))
}

/// Advance the version map by one row — creation opens a chain, deletion
/// removes the element. A closed chain's head is never consulted again
/// because identities never recycle (§6.2), which is also why the map holds
/// LIVE elements only.
fn fold_version(
    versions: &mut std::collections::BTreeMap<ElementId, ObjectId>,
    row: &DeltaRow,
) -> Result<(), CanonicalError> {
    match row {
        DeltaRow::CreateVertex { vid, .. } => {
            versions.insert(ElementId::Vertex(*vid), successor_version(None, row)?);
        }
        DeltaRow::CreateEdge { eid, .. } => {
            versions.insert(ElementId::Edge(*eid), successor_version(None, row)?);
        }
        DeltaRow::DeleteVertex {
            vid,
            sorted_retired_incident_edges,
            ..
        } => {
            versions.remove(&ElementId::Vertex(*vid));
            for eid in sorted_retired_incident_edges {
                versions.remove(&ElementId::Edge(*eid));
            }
        }
        DeltaRow::DeleteEdge { eid, .. } => {
            versions.remove(&ElementId::Edge(*eid));
        }
        DeltaRow::LabelMembership { vid, .. } => {
            let elem = ElementId::Vertex(*vid);
            let previous = *versions
                .get(&elem)
                .expect("the writer proved this vertex live before the version fold ran");
            versions.insert(elem, successor_version(Some(previous), row)?);
        }
        DeltaRow::Property { elem, .. } => {
            let previous = *versions
                .get(elem)
                .expect("the writer proved this element live before the version fold ran");
            versions.insert(*elem, successor_version(Some(previous), row)?);
        }
        _ => {}
    }
    Ok(())
}

/// One vertex's mutable batch-prefix content: `(labels, props)`, both in
/// canonical order.
type VertexContent = (Vec<LabelId>, Vec<(PropertyKeyId, CanonicalScalar)>);

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
async fn rebuild(
    cx: &CommitCx,
    coordinator: &CommitCoordinator,
    store: &BlockStore,
    keys: &DatabaseKeys,
) -> Result<(Snapshot, BlockWriter), RebuildError> {
    let mut writer = BlockWriter::new(GRAPH, BRANCH, PARTITION);
    let mut frontier = CommitSeq(0);
    let mut next_birth_ordinal = 0u64;
    let mut versions = std::collections::BTreeMap::new();

    for entry in coordinator.chain().entries() {
        let commit_seq = CommitSeq(entry.marker.commit_seq);
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
                    next_birth_ordinal += 1;
                }
                writer
                    .apply(keys.block_keys(), commit_seq, row)
                    .map_err(|error| RebuildError::Fold {
                        commit_seq: commit_seq.0,
                        error,
                    })?;
                fold_version(&mut versions, row).map_err(|error| RebuildError::Decode {
                    commit_seq: commit_seq.0,
                    error,
                })?;
            }
        }
    }

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
    let (reopened_root, decoded, decoded_patches) = store.reopen(cx, root_id)?;

    Ok((
        Snapshot {
            blocks: decoded,
            refs: reopened_root.blocks,
            patches: decoded_patches,
            patch_refs: reopened_root.vertex_patches,
            frontier,
            root: root_id,
            next_birth_ordinal,
            versions,
        },
        writer,
    ))
}
