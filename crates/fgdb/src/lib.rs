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
//! Tier D is ADJACENCY. `CreateVertex` rows are committed and durable — the
//! oracle materializes them, and they are in the stream a future tier reads —
//! but the block fold ignores them, so [`Database::neighbours`] is the whole read
//! surface. Properties, labels, and vertex-level reads arrive with
//! `fgdb-w3-properties-gou`.

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
    CanonicalError, CoordinateEntry, DeltaRow, LabelId, LogicalDeltaTemplate, PropertyKeyId,
    RelationId, SchemaEpoch,
};
use fgdb_strata::root::{RootError, merge_neighbours};
use fgdb_strata::store::{BlockStore, StoreError};
use fgdb_strata::writer::{BlockWriter, WriteError as BlockWriteError};
use fgdb_strata::{AdjacencyEntry, PartitionRootVersion};
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
/// Deletions are absent on purpose. `DeltaRow::DeleteEdge` carries a
/// `before_version` and `DeleteVertex` a complete cascade before-image, and
/// computing those is FINALIZATION's job (§9.1, `fgdb-w5-effects-normal-form-819`).
/// A caller-supplied cascade would be an assertion the caller could get wrong,
/// so this slice commits creates only and says why.
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
    frontier: CommitSeq,
    root: PartitionRootVersion,
    /// The next unspent birth ordinal, derived by counting the creations the
    /// durable stream already contains. Derived rather than stored: identity
    /// allocation is `fgdb-w2`'s, and a counter persisted here would be a second
    /// authority beside the stream.
    next_birth_ordinal: u64,
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
        let snapshot = rebuild(cx, &coordinator, &store, &keys).await?;
        Ok(Self {
            coordinator,
            store,
            keys,
            snapshot,
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

        let mut rows = Vec::with_capacity(batch.rows.len());
        for (birth_ordinal, row) in (self.snapshot.next_birth_ordinal..).zip(batch.rows) {
            rows.push(match row {
                PendingRow::Vertex { vid, labels, props } => DeltaRow::CreateVertex {
                    vid,
                    birth_ordinal,
                    labels,
                    props,
                    valid_time: None,
                },
                PendingRow::Edge {
                    eid,
                    src,
                    dst,
                    props,
                } => DeltaRow::CreateEdge {
                    eid,
                    birth_ordinal,
                    src,
                    relation: batch.relation,
                    dst,
                    canonical_key: None,
                    props,
                    valid_time: None,
                },
            });
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

        self.snapshot = rebuild(cx, &self.coordinator, &self.store, &self.keys).await?;
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
) -> Result<Snapshot, RebuildError> {
    let mut writer = BlockWriter::new(GRAPH, BRANCH, PARTITION);
    let mut frontier = CommitSeq(0);
    let mut next_birth_ordinal = 0u64;

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
            }
        }
    }

    let (root, blocks) = writer
        .publish(keys.block_keys(), frontier)
        .map_err(|error| RebuildError::Fold {
            commit_seq: frontier.0,
            error,
        })?;
    for block in &blocks {
        store.put(cx, &block.bytes)?;
    }
    let root_id = store.put_root(cx, &root)?;
    let (_, decoded) = store.reopen(cx, root_id)?;

    Ok(Snapshot {
        blocks: decoded,
        frontier,
        root: root_id,
        next_birth_ordinal,
    })
}
