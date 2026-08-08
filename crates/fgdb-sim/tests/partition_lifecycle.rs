//! **The whole tier-D lifecycle, once, with nothing held in memory across it.**
//!
//! Every piece has laws of its own and two differentials hold the semantics to
//! `fgdb-reference`. What none of them asks is whether the pieces COMPOSE — whether
//! a partition built from a real commit stream, persisted, and reopened by a
//! process that kept nothing still answers what the oracle says.
//!
//! ```text
//!   transactions -> capsules -> D1 -> markers -> D2
//!        |                                        |
//!        |                            drop, reopen, recover
//!        |                                        |
//!        |                              tier-D writer over the recovered rows
//!        |                                        |
//!        |                          blocks + root -> BlockStore (fsynced)
//!        |                                        |
//!        |                   DROP EVERYTHING; a fresh store, a root identity
//!        |                                        |
//!   ReferenceDatabase  <----- must agree -----  reopened partition
//! ```
//!
//! **THE SCOPE BOUNDARIES ARE WHAT MAKE THIS A TEST RATHER THAN A DEMO.** Between
//! the phases the only things carried forward are what a restarted process would
//! genuinely have: a directory path, the database keys, and a root identity. No
//! writer, no block list, no in-memory partition. Anything that survives in a
//! variable is a way for the test to pass without the durable path working, so the
//! phases are written as scopes and the compiler enforces it.
//!
//! **COMPACTION IS PART OF THE LIFECYCLE, NOT AN APPENDIX.** A partition that only
//! agrees before compaction is a partition that will disagree in production, where
//! compaction is continuous. The last law compacts, republishes, reopens and demands
//! the same agreement.

use asupersync::lab::run_async_under_lab;
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::CommitCoordinator;
use fgdb_chronicle::marker::EffectSource;
use fgdb_delta_types::{
    CoordinateEntry, DeltaRow, LabelId, LogicalDeltaTemplate, PropertyKeyId, RelationId,
    SchemaEpoch,
};
use fgdb_sim::{commit_capsule, prepare_capsule, replay};
use fgdb_strata::PartitionRootVersion;
use fgdb_strata::compact::compact;
use fgdb_strata::root::{PartitionRoot, merge_neighbours};
use fgdb_strata::store::BlockStore;
use fgdb_strata::writer::BlockWriter;
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, CommitSeq, EId, GraphId, ObjectId, VId};
use std::path::{Path, PathBuf};

const GRAPH: GraphId = GraphId(1);
const BRANCH: BranchId = BranchId(1);
const REL: RelationId = RelationId(1);
const LABEL: LabelId = LabelId(10);
const PROP: PropertyKeyId = PropertyKeyId(100);
const INTENT_SEMANTICS: ObjectId = ObjectId([0x11; 32]);
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);
const KEYS: (&[u8; 32], DatabaseSecurityNamespaceId) = (&K_OID, NAMESPACE);

fn keys() -> CapsuleKeys {
    CapsuleKeys {
        k_oid: K_OID,
        namespace: NAMESPACE,
        dek: [0x3c; 32],
        object_kind: fgdb_sim::CAPSULE_OBJECT_KIND,
        profile: CapsuleProfile::balanced(),
    }
}

fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fgdb-lifecycle-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn under_lab<T, Fut>(seed: u64, test: impl FnOnce(CommitCx) -> Fut + Send + 'static) -> T
where
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (output, report) = run_async_under_lab(seed, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        test(contexts.commit()).await
    });
    assert!(
        // lab_test_passed() covers ALL THREE channels — quiescence, the full
        // 24-oracle suite, and the mirrored invariant list (fresh-eyes I3).
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
    output
}

fn vertex(vid: u128) -> DeltaRow {
    DeltaRow::CreateVertex {
        vid: VId(vid),
        birth_ordinal: vid as u64,
        labels: vec![LABEL],
        props: vec![(PROP, fgdb_types::CanonicalScalar::Int(vid as i64))],
        valid_time: None,
    }
}

fn edge(eid: u128, src: u128, dst: u128) -> DeltaRow {
    DeltaRow::CreateEdge {
        eid: EId(eid),
        birth_ordinal: eid as u64,
        src: VId(src),
        relation: REL,
        dst: VId(dst),
        canonical_key: None,
        props: vec![],
        valid_time: None,
    }
}

async fn commit_rows(coordinator: &mut CommitCoordinator, cx: &CommitCx, rows: Vec<DeltaRow>) {
    let template = LogicalDeltaTemplate::build(
        INTENT_SEMANTICS,
        [0x22; 32],
        vec![CoordinateEntry {
            graph: GRAPH,
            branch: BRANCH,
            relation: REL,
            schema_epoch: SchemaEpoch(0),
            schema_transition: None,
            rows,
        }],
    )
    .expect("template builds");
    let capsule = prepare_capsule(&K_OID, NAMESPACE, &template).expect("seals");
    commit_capsule(coordinator, cx, &capsule, vec![])
        .await
        .expect("commits");
}

/// PHASE 1 — write a history through the real durable commit path.
async fn commit_a_history(dir: &Path, cx: &CommitCx) {
    let mut coordinator = CommitCoordinator::open(cx, dir, keys())
        .await
        .expect("open");
    commit_rows(&mut coordinator, cx, vec![vertex(1), vertex(2), vertex(3)]).await;
    commit_rows(&mut coordinator, cx, vec![edge(10, 1, 2), edge(11, 1, 3)]).await;

    // A deletion whose creation lives in an EARLIER capsule, so the writer's
    // live-edge map has to survive across commits to resolve it.
    let before_version = replay(cx, &coordinator)
        .await
        .expect("replays")
        .database
        .graph(GRAPH, BRANCH)
        .expect("materialized")
        .element_version(fgdb_delta_types::ElementId::Edge(EId(10)))
        .expect("the edge is live");
    commit_rows(
        &mut coordinator,
        cx,
        vec![DeltaRow::DeleteEdge {
            eid: EId(10),
            before_version,
        }],
    )
    .await;
    commit_rows(&mut coordinator, cx, vec![edge(12, 2, 3)]).await;

    // Re-create the retired TOPOLOGY under a fresh edge identity. Equal topology
    // is deliberately not a seal boundary: EId is the parallel-edge
    // discriminator, so this exercises the multigraph key through persistence.
    commit_rows(&mut coordinator, cx, vec![edge(13, 1, 2)]).await;
}

/// PHASE 2 — recover the stream, build the partition, persist it, and return ONLY
/// the root identity. Everything else goes out of scope.
async fn build_and_persist(
    dir: &Path,
    cx: &CommitCx,
    seal_after: CommitSeq,
) -> PartitionRootVersion {
    let reopened = CommitCoordinator::open(cx, dir, keys())
        .await
        .expect("reopen");
    let mut writer = BlockWriter::new(GRAPH, BRANCH, 0);
    let mut frontier = CommitSeq(0);

    for entry in reopened.chain().entries() {
        let commit_seq = CommitSeq(entry.marker.commit_seq);
        frontier = commit_seq;
        let EffectSource::Local { capsule_ref, .. } = &entry.marker.effect_source;
        let bytes = reopened
            .read_capsule(cx, *capsule_ref)
            .await
            .expect("readable");
        let template = LogicalDeltaTemplate::decode_canonical(&bytes).expect("decodes");
        for coordinate in template.coordinate_entries() {
            if (coordinate.graph, coordinate.branch) != (GRAPH, BRANCH) {
                continue;
            }
            for row in &coordinate.rows {
                writer.apply(KEYS, commit_seq, row).expect("accepted");
            }
        }
        if commit_seq == seal_after {
            writer
                .seal(KEYS)
                .expect("the fixture's explicit block cut seals");
        }
    }

    let (root, blocks, patches) = writer.publish(KEYS, frontier).expect("publishes");
    let store = BlockStore::open(cx, dir, K_OID, NAMESPACE).expect("store opens");
    for block in &blocks {
        store.put(cx, &block.bytes).expect("stores a block");
    }
    for patch in &patches {
        store
            .put_patch(cx, &patch.bytes)
            .expect("stores a vertex patch");
    }
    store.put_root(cx, &root).expect("stores the root")
}

/// What the oracle says the durable stream implies, at its frontier.
async fn oracle_answer(dir: &Path, cx: &CommitCx, source: u128) -> (Vec<VId>, CommitSeq) {
    let reopened = CommitCoordinator::open(cx, dir, keys())
        .await
        .expect("reopen");
    let database = replay(cx, &reopened).await.expect("replays").database;
    let frontier = database.applied_through(GRAPH, BRANCH).expect("a frontier");
    let graph = database.graph(GRAPH, BRANCH).expect("materialized");
    (graph.neighbours(VId(source), REL), frontier)
}

/// **THE LIFECYCLE: commit, build, persist, drop everything, reopen, agree.**
#[test]
fn a_persisted_partition_reopens_and_agrees_with_the_oracle() {
    let dir = scratch_dir("lifecycle");
    under_lab(51, move |cx| async move {
        let cx = &cx;
        commit_a_history(&dir, cx).await;
        let root_id = build_and_persist(&dir, cx, CommitSeq(2)).await;

        // NOTHING survives from the phases above except the path and this id —
        // exactly what a restarted process would hold.
        let store = BlockStore::open(cx, &dir, K_OID, NAMESPACE).expect("store opens");
        let (root, blocks, _block_props, _patches) =
            store.reopen(cx, root_id).expect("the partition reopens");

        for source in [1u128, 2, 3] {
            let (expected, frontier) = oracle_answer(&dir, cx, source).await;
            assert_eq!(
                merge_neighbours(&blocks, VId(source), REL, frontier).expect("merges"),
                expected,
                "reopened partition disagrees with the oracle for vertex {source}"
            );
        }

        // The fixture is non-trivial in both directions, or agreement is cheap:
        // 1->2 was created, deleted, and re-created under a new edge identity, so
        // the adjacency is live again by a DIFFERENT version than the one that died.
        let (v1, _) = oracle_answer(&dir, cx, 1).await;
        assert_eq!(v1, vec![VId(2), VId(3)]);
        let (v2, _) = oracle_answer(&dir, cx, 2).await;
        assert_eq!(v2, vec![VId(3)]);
        assert!(
            root.blocks.len() >= 2,
            "the explicit cut must exercise the multi-block path, or the root and merge are \
             never exercised: {} block(s)",
            root.blocks.len()
        );
    });
}

/// **COMPACTION IS PART OF THE LIFECYCLE.** Compact, republish, reopen, and demand
/// the same agreement.
///
/// A partition that only agrees BEFORE compaction is one that will disagree in
/// production, where compaction is continuous. The floor here is the stream's
/// frontier — the strongest floor any reader could license — so this also exercises
/// the case where compaction has the most licence to drop.
#[test]
fn a_compacted_partition_still_agrees_after_reopening() {
    let dir = scratch_dir("compacted");
    under_lab(52, move |cx| async move {
        let cx = &cx;
        commit_a_history(&dir, cx).await;
        let root_id = build_and_persist(&dir, cx, CommitSeq(2)).await;
        let (_, frontier) = oracle_answer(&dir, cx, 1).await;

        let compacted_root_id = {
            let store = BlockStore::open(cx, &dir, K_OID, NAMESPACE).expect("store opens");
            let (root, blocks, _block_props, _patches) =
                store.reopen(cx, root_id).expect("reopens");
            let result = compact(&blocks, frontier).expect("the persisted history compacts");
            assert!(
                result.blocks.len() <= root.blocks.len(),
                "compaction must not grow the partition"
            );
            assert!(
                result.dropped > 0,
                "the deleted edge ended below the frontier, so something must drop \
                 — otherwise this law is about a compaction that did nothing"
            );

            // Republish: new blocks, a new root, the old one untouched.
            let mut references = Vec::new();
            for entries in &result.blocks {
                let bytes = fgdb_strata::encode_block(entries).expect("lawful");
                let id = store.put(cx, &bytes).expect("stores");
                let span = fgdb_strata::root::span_of(entries).expect("non-empty");
                references.push(fgdb_strata::root::BlockRef {
                    block_id: id.0,
                    first_seq: span.0,
                    last_seq: span.1,
                });
            }
            let compacted = PartitionRoot {
                graph: GRAPH,
                branch: BRANCH,
                partition: 0,
                published_at: frontier,
                blocks: references,
                // Compaction here rewrites BLOCKS only; the vertex patches are
                // untouched state and the compacted root must keep naming
                // them, or republishing would silently lose every vertex row.
                vertex_patches: root.vertex_patches.clone(),
            };
            store
                .put_root(cx, &compacted)
                .expect("stores the compacted root")
        };

        // A fresh handle, the compacted root identity, and nothing else.
        let store = BlockStore::open(cx, &dir, K_OID, NAMESPACE).expect("store opens");
        let (_, blocks, _block_props, _patches) =
            store.reopen(cx, compacted_root_id).expect("reopens");
        for source in [1u128, 2, 3] {
            let (expected, _) = oracle_answer(&dir, cx, source).await;
            assert_eq!(
                merge_neighbours(&blocks, VId(source), REL, frontier).expect("merges"),
                expected,
                "compacted partition disagrees for vertex {source}"
            );
        }

        // PUBLISHING A NEW ROOT DID NOT DESTROY THE OLD ONE. Roots are immutable and
        // content-addressed, so the pre-compaction partition is still reachable and
        // still correct — which is what makes republishing safe to do while readers
        // hold the previous identity.
        assert_ne!(compacted_root_id, root_id);
        let (_, old_blocks, _old_block_props, _old_patches) = store
            .reopen(cx, root_id)
            .expect("the old root still reopens");
        assert_eq!(
            merge_neighbours(&old_blocks, VId(1), REL, frontier).expect("merges"),
            merge_neighbours(&blocks, VId(1), REL, frontier).expect("merges"),
            "and both roots answer the same at the frontier"
        );
    });
}
