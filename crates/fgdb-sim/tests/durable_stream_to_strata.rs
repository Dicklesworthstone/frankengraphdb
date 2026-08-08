//! **The whole arc, both halves.**
//!
//! `strata_oracle_differential.rs` drives the tier-D writer from synthetic
//! histories. This drives it from the DURABLE COMMIT STREAM: real transactions
//! sealed into erasure-coded capsules, committed under the two-fsync protocol,
//! recovered from disk after the coordinator is dropped, and only then folded into
//! Strata blocks. The merged read across those blocks must equal what the
//! recovered oracle says the same stream implies.
//!
//! ```text
//!   intents -> effects -> template -> capsule -> D1 -> marker -> D2
//!                                                        |
//!                                             crash, reopen, recover
//!                                                        |
//!                              +-------------------------+------------------+
//!                              |                                            |
//!                    ReferenceDatabase                             tier-D BlockWriter
//!                    (the oracle's answer)                         (Strata's answer)
//!                              |                                            |
//!                              +------------------ must agree --------------+
//! ```
//!
//! **WHY THIS IS NOT THE OTHER DIFFERENTIAL AGAIN.** There, both sides were fed a
//! `Vec` of rows I wrote. Here the rows come back off disk, through the capsule
//! codec and the canonical delta decoder, in the order the recovered marker chain
//! gives them — so it also proves the writer can be driven by RECOVERY, which is
//! the only way a real partition is ever rebuilt. A tier that agrees on synthetic
//! input and cannot be fed from the stream is not part of the database.
//!
//! **THE ONE SEQUENCE THE ORACLE CAN SPEAK ABOUT IS THE PRESENT.**
//! `ReferenceDatabase` holds materialized state, not history, so it can only be
//! asked "what is true now". Strata is asked the same question at the stream's
//! frontier. The historical answers Strata alone can give are checked against the
//! transitions the test itself commits, which is the honest limit of what a
//! present-state oracle can witness.

use asupersync::lab::run_async_under_lab;
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::CommitCoordinator;
use fgdb_chronicle::marker::EffectSource;
use fgdb_delta_types::{
    CoordinateEntry, DeltaRow, LabelId, LogicalDeltaTemplate, PropertyKeyId, RelationId,
    SchemaEpoch,
};
use fgdb_sim::{commit_capsule, prepare_capsule, replay};
use fgdb_strata::root::merge_neighbours;
use fgdb_strata::writer::BlockWriter;
use fgdb_strata::{AdjacencyEntry, decode_block};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, CommitSeq, EId, GraphId, ObjectId, VId};
use std::path::PathBuf;

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
    let dir =
        std::env::temp_dir().join(format!("fgdb-strata-stream-{}-{name}", std::process::id()));
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

/// Commit one template's worth of rows through the real durable path.
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

/// Rebuild a partition from the RECOVERED stream.
///
/// Walks the recovered marker chain, reads each capsule back off disk, decodes the
/// template, and folds every row into the writer at the sequence its marker
/// carries. This is how a real partition is rebuilt after a restart — there is no
/// other source of truth (doctrine 5: derived structures are never more
/// authoritative than the commit stream, and recovery discards and rebuilds them).
async fn rebuild_from_stream(
    cx: &CommitCx,
    coordinator: &CommitCoordinator,
) -> Vec<Vec<AdjacencyEntry>> {
    let mut writer = BlockWriter::new(GRAPH, BRANCH, 0);
    for entry in coordinator.chain().entries() {
        let commit_seq = CommitSeq(entry.marker.commit_seq);
        let EffectSource::Local { capsule_ref, .. } = &entry.marker.effect_source;
        let bytes = coordinator
            .read_capsule(cx, *capsule_ref)
            .await
            .expect("a committed capsule is readable");
        let template =
            LogicalDeltaTemplate::decode_canonical(&bytes).expect("a committed template decodes");
        for coordinate in template.coordinate_entries() {
            if (coordinate.graph, coordinate.branch) != (GRAPH, BRANCH) {
                continue;
            }
            for row in &coordinate.rows {
                writer
                    .apply(KEYS, commit_seq, row)
                    .expect("the writer accepts a committed row");
            }
        }
    }
    let (_, sealed, _patches) = writer
        .publish(KEYS, CommitSeq(u64::MAX / 2))
        .expect("publishes");
    sealed
        .iter()
        .map(|block| decode_block(&block.bytes).expect("a sealed block decodes"))
        .collect()
}

/// THE ARC: commit, drop the coordinator, recover, rebuild the partition from the
/// recovered stream, and agree with the oracle.
#[test]
fn a_partition_rebuilt_from_the_recovered_stream_agrees_with_the_oracle() {
    let dir = scratch_dir("arc");
    under_lab(21, move |cx| async move {
        let cx = &cx;
        {
            let mut coordinator = CommitCoordinator::open(cx, &dir, keys())
                .await
                .expect("open");
            commit_rows(&mut coordinator, cx, vec![vertex(1), vertex(2), vertex(3)]).await;
            commit_rows(&mut coordinator, cx, vec![edge(10, 1, 2)]).await;
            commit_rows(&mut coordinator, cx, vec![edge(11, 1, 3), edge(12, 2, 3)]).await;
        }
        // The coordinator is GONE. Everything below comes off disk.
        let reopened = CommitCoordinator::open(cx, &dir, keys())
            .await
            .expect("reopen");
        let database = replay(cx, &reopened)
            .await
            .expect("the stream replays")
            .database;
        let graph = database
            .graph(GRAPH, BRANCH)
            .expect("the coordinate materialized");
        let frontier = database.applied_through(GRAPH, BRANCH).expect("a frontier");

        let blocks = rebuild_from_stream(cx, &reopened).await;
        assert!(!blocks.is_empty(), "the stream produced blocks");

        for source in [1u128, 2, 3] {
            assert_eq!(
                merge_neighbours(&blocks, VId(source), REL, frontier).expect("merges"),
                graph.neighbours(VId(source), REL),
                "vertex {source} disagrees at the frontier"
            );
        }
        assert_eq!(
            graph.neighbours(VId(1), REL),
            vec![VId(2), VId(3)],
            "and the fixture is non-trivial, or agreement proves nothing"
        );
    });
}

/// A DELETION THAT CROSSES A COMMIT: the edge is created in one commit and retired
/// in a later one, so the tombstone reaches Strata through two separate capsules.
///
/// This is the case that needs the writer's live-edge map to survive across
/// commits — a `DeleteEdge` row names only an `EId`, and the creation that carries
/// its endpoints is in a different capsule entirely.
#[test]
fn a_deletion_in_a_later_commit_agrees_with_the_oracle() {
    let dir = scratch_dir("delete");
    under_lab(22, move |cx| async move {
        let cx = &cx;
        {
            let mut coordinator = CommitCoordinator::open(cx, &dir, keys())
                .await
                .expect("open");
            commit_rows(&mut coordinator, cx, vec![vertex(1), vertex(2), vertex(3)]).await;
            commit_rows(&mut coordinator, cx, vec![edge(10, 1, 2), edge(11, 1, 3)]).await;
            // A later commit retires one of them. The before-image is READ from
            // the replayed stream rather than invented: the materializer refuses a
            // version that disagrees with materialized state, and that check is
            // the delta stream's self-verification.
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
        }
        let reopened = CommitCoordinator::open(cx, &dir, keys())
            .await
            .expect("reopen");
        let database = replay(cx, &reopened).await.expect("replays").database;
        let graph = database.graph(GRAPH, BRANCH).expect("materialized");
        let frontier = database.applied_through(GRAPH, BRANCH).expect("frontier");

        let blocks = rebuild_from_stream(cx, &reopened).await;
        assert_eq!(
            graph.neighbours(VId(1), REL),
            vec![VId(3)],
            "the oracle dropped the retired edge, or this proves nothing"
        );
        assert_eq!(
            merge_neighbours(&blocks, VId(1), REL, frontier).expect("merges"),
            vec![VId(3)],
            "and Strata agrees at the frontier"
        );

        // THE HISTORY STRATA CAN ANSWER AND THE ORACLE CANNOT: before the
        // retirement's commit sequence the edge is still there. Checked against the
        // transition this test itself committed, which is the honest limit of what
        // a present-state oracle can witness.
        assert_eq!(
            merge_neighbours(&blocks, VId(1), REL, CommitSeq(2)).expect("merges"),
            vec![VId(2), VId(3)],
            "at the sequence the edges were created, both are visible"
        );
    });
}

/// The rebuild is DETERMINISTIC: recovering and rebuilding twice from the same
/// durable stream produces byte-identical blocks.
///
/// Doctrine 4 applied to a storage tier. Without it a partition could differ
/// between two recoveries of one database, which would make a content-addressed
/// block identity meaningless — the same history would name different objects.
#[test]
fn rebuilding_twice_produces_identical_blocks() {
    let dir = scratch_dir("deterministic");
    under_lab(23, move |cx| async move {
        let cx = &cx;
        {
            let mut coordinator = CommitCoordinator::open(cx, &dir, keys())
                .await
                .expect("open");
            commit_rows(&mut coordinator, cx, vec![vertex(1), vertex(2)]).await;
            commit_rows(&mut coordinator, cx, vec![edge(10, 1, 2)]).await;
        }
        let first = {
            let reopened = CommitCoordinator::open(cx, &dir, keys())
                .await
                .expect("reopen");
            rebuild_from_stream(cx, &reopened).await
        };
        let second = {
            let reopened = CommitCoordinator::open(cx, &dir, keys())
                .await
                .expect("reopen");
            rebuild_from_stream(cx, &reopened).await
        };
        assert_eq!(
            first, second,
            "two recoveries produced different partitions"
        );
        assert!(!first.is_empty());
    });
}
