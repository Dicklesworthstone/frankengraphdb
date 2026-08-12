//! **The spine, differentially tested against the oracle** (`fgdb-j0vu`).
//!
//! `crates/fgdb/tests/spine.rs` proves the engine agrees with ITSELF across a
//! reopen. That is necessary and it is not sufficient: an engine that folded
//! adjacency wrongly but consistently passes every law in that file. The
//! question this file asks is the other one — **does the graph the engine serves
//! equal the graph the durable history MEANS?**
//!
//! The answer comes from `fgdb-reference`, which §15.2 licenses to be simple and
//! never optimized, and from `fgdb_sim::replay`, which materializes the same
//! commit stream into it. Both already exist and are already trusted; j0vu asks
//! for the existing differential to be REUSED rather than for a new oracle, and
//! that is what this is.
//!
//! **WHY THIS FILE IS HERE AND NOT IN `crates/fgdb/tests/`.** `fgdb-reference`
//! carries a registered dependency allowlist (§15.2) naming `fgdb-chronicle` a
//! CI-rejected import, precisely so the differential cannot be gutted by code
//! sharing. The verification layer is the only place the engine and the oracle
//! may both be visible, and making `fgdb` depend on the oracle — even as a
//! dev-dependency — would erode exactly the independence that makes agreement
//! mean something.
//!
//! **THE TWO SIDES MUST SHARE NOTHING BUT BYTES ON DISK.** The engine writes
//! through `fgdb::Database`; the oracle is fed by opening a *separate*
//! `CommitCoordinator` over the same directory after the `Database` has been
//! dropped. No handle, no fold, no block list and no snapshot crosses between
//! them — only the durable stream, which is the only thing they are supposed to
//! agree about.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    BlockStoreCrashPoint, CAPSULE_OBJECT_KIND, Database, DatabaseKeys, DatabaseState,
    DerivedPublicationStage, ReadError, RebuildError, WriteBatch, WriteError,
};
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::CommitCoordinator;
use fgdb_chronicle::store::{ROOT_FILE_NAME, StoreError as SlotStoreError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_sim::{
    replay,
    vfs::{FaultKind, FaultPlan, FaultVfs, Trigger},
};
use fgdb_strata::store::StoreError as BlockStoreError;
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, CanonicalScalar, EId, GraphId, VId};
use std::future::{Future, poll_fn};
use std::path::{Path, PathBuf};
use std::task::Poll;

const GRAPH: GraphId = GraphId(1);
const BRANCH: BranchId = BranchId(1);
const KNOWS: RelationId = RelationId(1);
const WORKS_WITH: RelationId = RelationId(2);
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);
const DEK: [u8; 32] = [0x3c; 32];

fn engine_keys() -> DatabaseKeys {
    DatabaseKeys {
        k_oid: K_OID,
        namespace: NAMESPACE,
        dek: DEK,
    }
}

/// The oracle side opens the stream itself. These must be the keys the engine
/// wrote under or the capsules will not open — which is a property worth having
/// exercised rather than hidden behind a shared constructor.
fn oracle_keys() -> CapsuleKeys {
    CapsuleKeys {
        k_oid: K_OID,
        namespace: NAMESPACE,
        dek: DEK,
        object_kind: CAPSULE_OBJECT_KIND,
        profile: CapsuleProfile::balanced(),
    }
}

fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-spine-diff-{}-{name}", std::process::id()))
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
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
    output
}

fn under_lab_with_root<T, Fut>(
    seed: u64,
    test: impl FnOnce(asupersync::Cx, CommitCx) -> Fut + Send + 'static,
) -> T
where
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (output, report) = run_async_under_lab(seed, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        test(root, contexts.commit()).await
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
    output
}

fn assert_recovery_fence<T>(
    stage: DerivedPublicationStage,
    recovery: fgdb::RecoveryRequired,
    result: Result<T, ReadError>,
) {
    match result {
        Err(ReadError::RecoveryRequired(found)) => assert_eq!(
            found, recovery,
            "{stage:?}: every state-bearing read must carry the same recovery evidence"
        ),
        unexpected => assert!(
            matches!(&unexpected, Err(ReadError::RecoveryRequired(_))),
            "{stage:?}: every state-bearing read must carry recovery evidence"
        ),
    }
}

fn vfs_fault_batch() -> WriteBatch {
    let mut batch = WriteBatch::new(KNOWS);
    batch.create_vertex(
        VId(1),
        vec![LabelId(3)],
        vec![(PropertyKeyId(7), CanonicalScalar::Int(1))],
    );
    batch
}

fn block_store_fault_batch() -> WriteBatch {
    let mut batch = WriteBatch::new(KNOWS);
    batch.create_vertex(VId(1), vec![LabelId(3)], vec![]);
    batch.create_vertex(VId(2), vec![], vec![]);
    batch.add_edge(EId(1), VId(1), VId(2), vec![]);
    batch
}

async fn create_genesis(cx: &CommitCx, dir: &Path) {
    drop(
        Database::create(cx, dir, engine_keys())
            .await
            .expect("genesis database"),
    );
}

async fn assert_reopened_vertex_matches_oracle(cx: &CommitCx, dir: &Path) {
    let engine = Database::open(cx, dir, engine_keys())
        .await
        .expect("authoritative reopen repairs the root slot");
    assert_eq!(engine.frontier().expect("healthy frontier").0, 1);
    let engine_vertex = engine.vertex(VId(1)).expect("healthy read");
    drop(engine);

    let coordinator = CommitCoordinator::open(cx, dir, oracle_keys())
        .await
        .expect("oracle opens the durable stream");
    let replayed = replay(cx, &coordinator).await.expect("stream replays");
    let graph = replayed
        .database
        .graph(GRAPH, BRANCH)
        .expect("oracle materialized the coordinate");
    let oracle_vertex = graph.vertex(VId(1)).expect("durable vertex exists");
    let engine_vertex = engine_vertex.expect("engine recovered durable vertex");
    assert_eq!(engine_vertex.labels, vec![LabelId(3)]);
    assert_eq!(
        engine_vertex.props,
        vec![(PropertyKeyId(7), CanonicalScalar::Int(1))]
    );
    assert_eq!(
        engine_vertex.labels,
        oracle_vertex.labels.iter().copied().collect::<Vec<_>>()
    );
    assert_eq!(
        engine_vertex.props,
        oracle_vertex
            .props
            .iter()
            .map(|(key, value)| (*key, value.clone()))
            .collect::<Vec<_>>()
    );
}

/// Write a history through the ENGINE's public surface only.
///
/// Deliberately not a straight line: parallel edges between one pair, a
/// self-loop, a second relation, and a hub with several destinations. A fixture
/// where every vertex has one neighbour would agree under almost any folding
/// mistake.
async fn write_history(cx: &CommitCx, dir: &Path) -> Vec<fgdb_types::CommitSeq> {
    let mut db = Database::create(cx, dir, engine_keys())
        .await
        .expect("creates");

    let mut first = WriteBatch::new(KNOWS);
    // VId(1) carries labels AND properties, VId(2) a label alone, the rest
    // nothing — so the vertex differential below compares three distinct
    // shapes rather than six copies of the empty row (fgdb-3xoi).
    first.create_vertex(
        VId(1),
        vec![LabelId(3), LabelId(5)],
        vec![
            (
                PropertyKeyId(7),
                CanonicalScalar::ucs_basic_text("ada").expect("admissible"),
            ),
            (PropertyKeyId(9), CanonicalScalar::Int(1815)),
        ],
    );
    first.create_vertex(VId(2), vec![LabelId(3)], vec![]);
    for vid in 3..=5u128 {
        first.create_vertex(VId(vid), vec![], vec![]);
    }
    // EDGE PROPERTIES (fgdb-yqor): EId(10) carries two, EId(11) none — the
    // edge differential below compares distinct shapes, not copies of empty.
    first.add_edge(
        EId(10),
        VId(1),
        VId(2),
        vec![
            (PropertyKeyId(11), CanonicalScalar::Int(2019)),
            (
                PropertyKeyId(13),
                CanonicalScalar::ucs_basic_text("close").expect("admissible"),
            ),
        ],
    );
    first.add_edge(EId(11), VId(1), VId(3), vec![]);
    let mut epochs = Vec::new();
    epochs.push(db.write(cx, first).await.expect("first batch commits"));

    // PARALLEL EDGES: same (src, dst), different EId. EId is the unconditional
    // parallel-edge discriminator (§4.1), so a fold keyed on the pair alone
    // collapses these and disagrees here. EId(12) is propertied AND deleted
    // below, so its retirement exercises the tombstone-restates-props path.
    let mut second = WriteBatch::new(KNOWS);
    second.add_edge(
        EId(12),
        VId(1),
        VId(2),
        vec![(PropertyKeyId(11), CanonicalScalar::Int(2020))],
    );
    second.add_edge(EId(13), VId(4), VId(4), vec![]); // self-loop
    epochs.push(db.write(cx, second).await.expect("second batch commits"));

    // A SECOND RELATION over an overlapping vertex set, propertied on the
    // surviving edge so the cross-relation read is compared with content.
    let mut third = WriteBatch::new(WORKS_WITH);
    third.add_edge(EId(14), VId(1), VId(5), vec![]);
    third.add_edge(
        EId(15),
        VId(2),
        VId(3),
        vec![(PropertyKeyId(11), CanonicalScalar::Bool(true))],
    );
    epochs.push(db.write(cx, third).await.expect("third batch commits"));

    // DELETES, with every before-image engine-derived (fgdb-p3ok). This is
    // the differential's sharpest teeth: the oracle's replay REFUSES a wrong
    // `before_version` or an inexact cascade, so these rows are validated at
    // apply time, not merely compared afterwards. VId(6) exists-then-goes in
    // one batch; VId(5) goes with its inbound WORKS_WITH edge cascaded.
    let mut fourth = WriteBatch::new(KNOWS);
    fourth.create_vertex(VId(6), vec![], vec![]);
    fourth.add_edge(EId(16), VId(6), VId(1), vec![]);
    fourth.add_edge(EId(17), VId(2), VId(4), vec![]);
    epochs.push(db.write(cx, fourth).await.expect("fourth batch commits"));
    let mut fifth = WriteBatch::new(KNOWS);
    fifth.delete_edge(EId(12)); // ONE of the two parallel edges — its twin survives
    fifth.delete_vertex(VId(6)); // cascades EId(16)
    fifth.delete_vertex(VId(5)); // cascades EId(14), a cross-relation edge
    epochs.push(db.write(cx, fifth).await.expect("fifth batch commits"));

    // UPDATES (fgdb-stb6), every before-image engine-derived and validated by
    // the oracle at replay (LabelBeforeMismatch / PropertyBeforeMismatch):
    // change a property, unset one, add and remove labels — including a
    // same-batch chain on one vertex so the derivation walks the prefix.
    let mut sixth = WriteBatch::new(KNOWS);
    sixth.set_vertex_property(
        VId(1),
        PropertyKeyId(7),
        Some(CanonicalScalar::ucs_basic_text("lovelace").expect("admissible")),
    );
    sixth.set_vertex_property(VId(1), PropertyKeyId(9), None);
    sixth.set_vertex_label(VId(1), LabelId(9), true);
    sixth.set_vertex_label(VId(1), LabelId(3), false);
    sixth.set_vertex_label(VId(3), LabelId(7), true);
    epochs.push(db.write(cx, sixth).await.expect("sixth batch commits"));

    // EDGE PROPERTY UPDATES (fgdb-ls5b), every before-image engine-derived
    // and oracle-validated at replay: change a value, unset one, add one to a
    // previously propertyless edge — two COMMUTING fields. Same-field
    // chains now fold (fgdb-w5-effects-normal-form-819.2) and are covered
    // by the independent net-effect differential below, not this fixture.
    let mut seventh = WriteBatch::new(KNOWS);
    seventh.set_edge_property(EId(10), PropertyKeyId(11), Some(CanonicalScalar::Int(2021)));
    seventh.set_edge_property(EId(10), PropertyKeyId(13), None);
    seventh.set_edge_property(
        EId(13),
        PropertyKeyId(17),
        Some(CanonicalScalar::Bool(true)),
    );
    seventh.set_edge_property(EId(13), PropertyKeyId(19), Some(CanonicalScalar::Int(3)));
    epochs.push(db.write(cx, seventh).await.expect("seventh batch commits"));

    // Same-field chain (fgdb-w5-effects-normal-form-819.2): two writes of
    // one property on a live vertex. The fold must emit {first before,
    // last after} or the oracle replay refuses PropertyBeforeMismatch.
    let mut eighth = WriteBatch::new(KNOWS);
    eighth.set_vertex_property(VId(3), PropertyKeyId(9), Some(CanonicalScalar::Int(3)));
    eighth.set_vertex_property(VId(3), PropertyKeyId(9), Some(CanonicalScalar::Int(7)));
    epochs.push(db.write(cx, eighth).await.expect("eighth batch commits"));
    epochs
}

/// **THE DIFFERENTIAL: the engine's answer equals the oracle's, for every vertex
/// and every relation in the fixture.**
#[test]
fn the_spine_agrees_with_the_reference_oracle() {
    let dir = scratch("agreement");
    under_lab(101, move |cx| async move {
        let cx = &cx;
        let _ = write_history(cx, &dir).await;

        // ENGINE SIDE. A fresh open, so the answers come from the durable path
        // rather than from the writer that produced them.
        let engine = Database::open(cx, &dir, engine_keys())
            .await
            .expect("reopens");
        let probes: Vec<(VId, RelationId)> = (1..=6u128)
            .flat_map(|vid| [(VId(vid), KNOWS), (VId(vid), WORKS_WITH)])
            .collect();
        let engine_answers: Vec<Vec<VId>> = probes
            .iter()
            .map(|(vid, rel)| engine.neighbours(*vid, *rel).expect("engine reads"))
            .collect();
        let engine_vertices: Vec<Option<fgdb::VertexRow>> = (1..=6u128)
            .map(|vid| engine.vertex(VId(vid)).expect("engine vertex reads"))
            .collect();
        let engine_edges: Vec<Option<fgdb::EdgeRecord>> = (10..=17u128)
            .map(|eid| engine.edge(EId(eid)).expect("engine edge reads"))
            .collect();
        drop(engine); // release the single-writer lease before the oracle opens

        // ORACLE SIDE. Its own coordinator over the same directory; nothing but
        // the bytes on disk crosses from the engine.
        let coordinator = CommitCoordinator::open(cx, &dir, oracle_keys())
            .await
            .expect("oracle opens");
        let replayed = replay(cx, &coordinator).await.expect("the stream replays");
        let graph = replayed
            .database
            .graph(GRAPH, BRANCH)
            .expect("the oracle materialized the coordinate");

        let mut agreements = 0usize;
        let mut nonempty = 0usize;
        for ((vid, rel), engine_answer) in probes.iter().zip(&engine_answers) {
            let oracle_answer = graph.neighbours(*vid, *rel);
            assert_eq!(
                engine_answer, &oracle_answer,
                "engine and oracle disagree for {vid:?} over {rel:?}"
            );
            agreements += 1;
            if !oracle_answer.is_empty() {
                nonempty += 1;
            }
        }

        // THE VERTEX DIFFERENTIAL (fgdb-3xoi): the engine's durable vertex
        // rows agree with the oracle's materialized vertices — existence,
        // labels, and properties, per vid.
        let mut labeled = 0usize;
        let mut propertied = 0usize;
        for (vid, engine_row) in (1..=6u128).map(VId).zip(&engine_vertices) {
            let oracle_vertex = graph.vertex(vid);
            assert_eq!(
                engine_row.is_some(),
                oracle_vertex.is_some(),
                "engine and oracle disagree about whether {vid:?} exists"
            );
            let (Some(row), Some(vertex)) = (engine_row, oracle_vertex) else {
                continue;
            };
            let oracle_labels: Vec<LabelId> = vertex.labels.iter().copied().collect();
            let oracle_props: Vec<(PropertyKeyId, CanonicalScalar)> = vertex
                .props
                .iter()
                .map(|(key, value)| (*key, value.clone()))
                .collect();
            assert_eq!(
                row.labels, oracle_labels,
                "engine and oracle disagree about {vid:?}'s labels"
            );
            assert_eq!(
                row.props, oracle_props,
                "engine and oracle disagree about {vid:?}'s properties"
            );
            assert_eq!(
                row.birth_ordinal, vertex.birth_ordinal,
                "engine and oracle disagree about {vid:?}'s birth ordinal"
            );
            labeled += usize::from(!row.labels.is_empty());
            propertied += usize::from(!row.props.is_empty());
        }
        assert!(
            labeled >= 2 && propertied >= 1,
            "the fixture must exercise labels and properties or the vertex \
             differential is agreement about emptiness, got {labeled} labeled \
             and {propertied} propertied"
        );

        // THE EDGE-LOOKUP DIFFERENTIAL: existence, endpoints, relation, and
        // properties agree with the oracle per EId — including the deleted
        // parallel edge, its surviving twin, and the cascade-retired edges.
        let mut live_edges = 0usize;
        let mut dead_edges = 0usize;
        let mut propertied_edges = 0usize;
        for (eid, engine_edge) in (10..=17u128).map(EId).zip(&engine_edges) {
            let oracle_edge = graph.edge(eid);
            assert_eq!(
                engine_edge.is_some(),
                oracle_edge.is_some(),
                "engine and oracle disagree about whether {eid:?} exists"
            );
            let (Some(record), Some(edge)) = (engine_edge, oracle_edge) else {
                dead_edges += 1;
                continue;
            };
            assert_eq!(
                (record.entry.src, record.entry.relation, record.entry.dst),
                (edge.src, edge.relation, edge.dst),
                "engine and oracle disagree about {eid:?}'s topology"
            );
            let oracle_props: Vec<_> = edge
                .props
                .iter()
                .map(|(key, value)| (*key, value.clone()))
                .collect();
            assert_eq!(
                record.props, oracle_props,
                "engine and oracle disagree about {eid:?}'s properties"
            );
            live_edges += 1;
            if !record.props.is_empty() {
                propertied_edges += 1;
            }
        }
        assert!(
            live_edges >= 3 && dead_edges >= 3 && propertied_edges >= 2,
            "the fixture must exercise live, retired, and propertied edges, got \
             {live_edges} live, {dead_edges} dead, {propertied_edges} propertied"
        );

        // ANTI-VACUITY. Agreement over twelve empty answers is not agreement
        // about anything: two implementations that both return nothing agree
        // perfectly. Pin that the fixture actually exercises the fold.
        assert_eq!(agreements, 12, "every probe must have been compared");
        assert!(
            nonempty >= 4,
            "the fixture must produce several non-empty answers or this law is \
             agreement about emptiness, got {nonempty}"
        );
    });
}

/// Agreement must survive a reopen on BOTH sides, not just the first read.
#[test]
fn agreement_survives_a_reopen() {
    let dir = scratch("reopen-agreement");
    under_lab(102, move |cx| async move {
        let cx = &cx;
        let _ = write_history(cx, &dir).await;

        let first = {
            let engine = Database::open(cx, &dir, engine_keys())
                .await
                .expect("reopens");
            engine.neighbours(VId(1), KNOWS).expect("reads")
        };
        let second = {
            let engine = Database::open(cx, &dir, engine_keys())
                .await
                .expect("reopens again");
            engine.neighbours(VId(1), KNOWS).expect("reads")
        };
        assert_eq!(first, second, "the engine must not drift across reopens");

        let coordinator = CommitCoordinator::open(cx, &dir, oracle_keys())
            .await
            .expect("oracle opens");
        let replayed = replay(cx, &coordinator).await.expect("the stream replays");
        let oracle = replayed
            .database
            .graph(GRAPH, BRANCH)
            .expect("materialized")
            .neighbours(VId(1), KNOWS);
        assert_eq!(first, oracle, "and both must equal what the history means");
        assert!(
            !oracle.is_empty(),
            "vertex 1 has parallel KNOWS edges in the fixture; an empty answer here \
             means the fixture stopped exercising the fold"
        );
    });
}

/// Every derived-publication boundary after Chronicle D2 has the same law:
/// the retained handle is totally fenced, and a fresh open agrees with the
/// independent reference replay about the commit that triggered the failure
/// (`fgdb-l96k`).
#[test]
fn every_post_d2_failure_fences_every_read_face_and_replays_to_the_oracle() {
    const STAGES: [DerivedPublicationStage; 9] = [
        DerivedPublicationStage::FoldCommittedTemplate,
        DerivedPublicationStage::SealPartition,
        DerivedPublicationStage::PublishEdgeBlocks,
        DerivedPublicationStage::PublishVertexPatches,
        DerivedPublicationStage::PublishPartitionRoot,
        DerivedPublicationStage::PublishManifest,
        DerivedPublicationStage::PublishRootSlot,
        DerivedPublicationStage::RefreshEdgeSnapshot,
        DerivedPublicationStage::RefreshVertexSnapshot,
    ];

    for (ordinal, stage) in STAGES.into_iter().enumerate() {
        let dir = scratch(&format!("post-d2-{ordinal}-{stage:?}"));
        under_lab(1_200 + ordinal as u64, move |cx| async move {
            let cx = &cx;
            let mut db = Database::create(cx, &dir, engine_keys())
                .await
                .expect("creates");

            let mut first = WriteBatch::new(KNOWS);
            first.create_vertex(
                VId(1),
                vec![LabelId(3)],
                vec![(PropertyKeyId(7), CanonicalScalar::Int(1))],
            );
            first.create_vertex(VId(2), vec![], vec![]);
            first.add_edge(EId(10), VId(1), VId(2), vec![]);
            db.write(cx, first).await.expect("first commit publishes");

            let mut second = WriteBatch::new(KNOWS);
            second.create_vertex(
                VId(3),
                vec![LabelId(5)],
                vec![(PropertyKeyId(7), CanonicalScalar::Int(2))],
            );
            second.add_edge(
                EId(11),
                VId(1),
                VId(3),
                vec![(PropertyKeyId(11), CanonicalScalar::Bool(true))],
            );
            let error = db
                .write_with_publication_failure(cx, second, stage)
                .await
                .expect_err("the named post-D2 stage must fail");
            let (recovery, source) = match error {
                WriteError::CommittedNeedsRecovery { recovery, source } => (recovery, source),
                unexpected => {
                    assert!(
                        matches!(&unexpected, WriteError::CommittedNeedsRecovery { .. }),
                        "{stage:?}: injected failure returned the wrong error: {unexpected:?}"
                    );
                    return;
                }
            };
            assert_eq!(recovery.durable_frontier.0, 2, "{stage:?}");
            assert_eq!(recovery.published_frontier.0, 1, "{stage:?}");
            assert_eq!(recovery.failed_stage, stage);
            match *source {
                RebuildError::InjectedPublicationFailure(found) => assert_eq!(
                    found, stage,
                    "{stage:?}: the source must identify the injection boundary"
                ),
                unexpected => assert!(
                    matches!(&unexpected, RebuildError::InjectedPublicationFailure(_)),
                    "{stage:?}: the source must identify the injection boundary"
                ),
            }
            assert_eq!(
                db.state(),
                DatabaseState::NeedsAuthoritativeRecovery(recovery)
            );

            assert_recovery_fence(stage, recovery, db.frontier());
            assert_recovery_fence(stage, recovery, db.manifest());
            assert_recovery_fence(stage, recovery, db.partition_root());
            assert_recovery_fence(stage, recovery, db.neighbours(VId(1), KNOWS));
            assert_recovery_fence(
                stage,
                recovery,
                db.neighbours_at(VId(1), KNOWS, recovery.published_frontier),
            );
            assert_recovery_fence(stage, recovery, db.in_neighbours(VId(3), KNOWS));
            assert_recovery_fence(
                stage,
                recovery,
                db.in_neighbours_at(VId(3), KNOWS, recovery.published_frontier),
            );
            assert_recovery_fence(stage, recovery, db.edge(EId(11)));
            assert_recovery_fence(
                stage,
                recovery,
                db.edge_at(EId(11), recovery.published_frontier),
            );
            assert_recovery_fence(stage, recovery, db.vertex(VId(3)));
            assert_recovery_fence(
                stage,
                recovery,
                db.vertex_at(VId(3), recovery.published_frontier),
            );
            assert_recovery_fence(stage, recovery, db.vertices());
            assert_recovery_fence(stage, recovery, db.vertices_at(recovery.published_frontier));
            assert_recovery_fence(stage, recovery, db.edges());
            assert_recovery_fence(stage, recovery, db.edges_at(recovery.published_frontier));
            let compact_error = db
                .compact(cx)
                .await
                .expect_err("a fenced handle must refuse maintenance");
            let found = match compact_error {
                RebuildError::HandleNotHealthy(found) => found,
                unexpected => {
                    assert!(
                        matches!(&unexpected, RebuildError::HandleNotHealthy(_)),
                        "{stage:?}: maintenance returned the wrong fence: {unexpected:?}"
                    );
                    return;
                }
            };
            assert_eq!(found, DatabaseState::NeedsAuthoritativeRecovery(recovery));
            let mut third = WriteBatch::new(KNOWS);
            third.create_vertex(VId(4), vec![], vec![]);
            match db.write(cx, third).await {
                Err(WriteError::RecoveryRequired(found)) => assert_eq!(found, recovery),
                unexpected => assert!(
                    matches!(&unexpected, Err(WriteError::RecoveryRequired(_))),
                    "{stage:?}: fenced writer returned the wrong outcome: {unexpected:?}"
                ),
            }
            drop(db);

            // ENGINE SIDE: only the directory and keys cross the reopen.
            let engine = Database::open(cx, &dir, engine_keys())
                .await
                .expect("authoritative reopen recovers the durable commit");
            let engine_neighbours = engine.neighbours(VId(1), KNOWS).expect("reads");
            let engine_vertices = engine.vertices().expect("reads");
            let engine_edges = engine.edges().expect("reads");
            drop(engine);

            // ORACLE SIDE: independently replay the Chronicle bytes and compare
            // the whole visible universe, not just one point lookup.
            let coordinator = CommitCoordinator::open(cx, &dir, oracle_keys())
                .await
                .expect("oracle opens");
            let replayed = replay(cx, &coordinator).await.expect("stream replays");
            let graph = replayed
                .database
                .graph(GRAPH, BRANCH)
                .expect("oracle materialized the coordinate");
            assert_eq!(
                engine_neighbours,
                graph.neighbours(VId(1), KNOWS),
                "{stage:?}"
            );
            assert_eq!(engine_vertices.len(), graph.vertex_count(), "{stage:?}");
            assert_eq!(engine_edges.len(), graph.edge_count(), "{stage:?}");
            for row in &engine_vertices {
                let oracle = graph.vertex(row.vid).expect("engine-only vertex");
                assert_eq!(
                    row.labels,
                    oracle.labels.iter().copied().collect::<Vec<_>>()
                );
                assert_eq!(
                    row.props,
                    oracle
                        .props
                        .iter()
                        .map(|(key, value)| (*key, value.clone()))
                        .collect::<Vec<_>>()
                );
            }
            for record in &engine_edges {
                let oracle = graph.edge(record.entry.eid).expect("engine-only edge");
                assert_eq!(
                    (record.entry.src, record.entry.relation, record.entry.dst),
                    (oracle.src, oracle.relation, oracle.dst)
                );
                assert_eq!(
                    record.props,
                    oracle
                        .props
                        .iter()
                        .map(|(key, value)| (*key, value.clone()))
                        .collect::<Vec<_>>()
                );
            }
            assert_eq!(
                engine_vertices.len(),
                3,
                "{stage:?}: second commit must exist"
            );
            assert_eq!(engine_edges.len(), 2, "{stage:?}: second commit must exist");
        });
    }
}

/// The integrated `Database` must not erase the faultable root-store seam.
/// A byte budget measured by the same public write first proves how many bytes
/// an honest write flushes; one byte less must reach Chronicle D2, fail at
/// `manifest.root`, fence the handle, and recover the committed vertex exactly
/// once from the authoritative stream.
#[test]
fn root_slot_enospc_fences_the_database_and_reopen_matches_the_oracle() {
    let control_dir = scratch("database-vfs-enospc-control");
    let faulted_dir = scratch("database-vfs-enospc-faulted");
    under_lab(1_210, move |cx| async move {
        let cx = &cx;
        create_genesis(cx, &control_dir).await;
        create_genesis(cx, &faulted_dir).await;

        let control_vfs = FaultVfs::unix(FaultPlan::faultless());
        let mut control =
            Database::open_with_vfs(cx, control_vfs.clone(), &control_dir, engine_keys())
                .await
                .expect("control opens through the VFS");
        assert_eq!(
            control
                .write(cx, vfs_fault_batch())
                .await
                .expect("control commits")
                .0,
            1
        );
        let honest_bytes = control_vfs.flushed_bytes();
        assert!(
            honest_bytes > 1,
            "a zero-byte control would make the ENOSPC placement vacuous"
        );
        assert_eq!(control_vfs.events(), Vec::new());
        drop(control);

        let faulted_vfs = FaultVfs::unix(FaultPlan {
            space_budget: Some(honest_bytes - 1),
            ..FaultPlan::faultless()
        });
        let mut db = Database::open_with_vfs(cx, faulted_vfs.clone(), &faulted_dir, engine_keys())
            .await
            .expect("faulted database opens");
        let error = db
            .write(cx, vfs_fault_batch())
            .await
            .expect_err("the root-slot barrier must exhaust the measured budget");
        let committed = match &error {
            WriteError::CommittedNeedsRecovery { recovery, source } => {
                Some((*recovery, source.as_ref()))
            }
            _ => None,
        };
        assert!(
            committed.is_some(),
            "root-slot ENOSPC returned the wrong error: {error:?}"
        );
        let Some((recovery, source)) = committed else {
            return;
        };
        assert_eq!(recovery.durable_frontier.0, 1);
        assert_eq!(recovery.published_frontier.0, 0);
        assert_eq!(
            recovery.failed_stage,
            DerivedPublicationStage::PublishRootSlot
        );
        let raw_os_error = match source {
            RebuildError::Slot(SlotStoreError::Io(error)) => error.raw_os_error(),
            _ => None,
        };
        assert_eq!(
            raw_os_error,
            Some(28),
            "root-slot ENOSPC lost its typed source: {source:?}"
        );
        assert_eq!(
            db.state(),
            DatabaseState::NeedsAuthoritativeRecovery(recovery)
        );
        assert_recovery_fence(
            DerivedPublicationStage::PublishRootSlot,
            recovery,
            db.vertex(VId(1)),
        );
        match db.write(cx, vfs_fault_batch()).await {
            Err(WriteError::RecoveryRequired(found)) => assert_eq!(found, recovery),
            unexpected => assert!(
                matches!(&unexpected, Err(WriteError::RecoveryRequired(_))),
                "fenced writer returned the wrong outcome: {unexpected:?}"
            ),
        }

        let events = faulted_vfs.events();
        assert_eq!(events.len(), 1, "exactly the planned fault must fire");
        assert!(matches!(events[0].kind, FaultKind::OutOfSpace { .. }));
        assert_eq!(events[0].path, faulted_dir.join(ROOT_FILE_NAME));

        faulted_vfs.crash().await.expect("simulate process loss");
        drop(db);
        assert_reopened_vertex_matches_oracle(cx, &faulted_dir).await;
    });
}

/// A lying root-slot fsync is harder than an ordinary I/O error: the barrier
/// returns success. `RootStore`'s post-barrier reread must detect the lie,
/// `Database` must fence rather than swap snapshots, and crash/reopen must
/// still derive the acknowledged commit from Chronicle.
#[test]
fn root_slot_fsync_lie_is_detected_fenced_and_recovered_from_chronicle() {
    let dir = scratch("database-vfs-root-slot-lie");
    under_lab(1_211, move |cx| async move {
        let cx = &cx;
        create_genesis(cx, &dir).await;

        // A database write performs D1, D2, then the root-slot barrier. The
        // event-path assertion below pins that arithmetic and fails loudly if
        // another eligible sync is introduced ahead of the slot.
        let vfs = FaultVfs::unix(FaultPlan {
            fsync_lie: Trigger::Nth(3),
            ..FaultPlan::faultless()
        });
        let mut db = Database::open_with_vfs(cx, vfs.clone(), &dir, engine_keys())
            .await
            .expect("faulted database opens");
        let error = db
            .write(cx, vfs_fault_batch())
            .await
            .expect_err("the evidence reread must expose the fsync lie");
        let committed = match &error {
            WriteError::CommittedNeedsRecovery { recovery, source } => {
                Some((*recovery, source.as_ref()))
            }
            _ => None,
        };
        assert!(
            committed.is_some(),
            "root-slot fsync lie returned the wrong error: {error:?}"
        );
        let Some((recovery, source)) = committed else {
            return;
        };
        assert_eq!(recovery.durable_frontier.0, 1);
        assert_eq!(recovery.published_frontier.0, 0);
        assert_eq!(
            recovery.failed_stage,
            DerivedPublicationStage::PublishRootSlot
        );
        assert!(
            matches!(
                source,
                RebuildError::Slot(SlotStoreError::PublicationNotObservable {
                    expected_generation: 2
                })
            ),
            "the reread must name the unobservable generation: {source:?}"
        );
        assert_eq!(
            db.state(),
            DatabaseState::NeedsAuthoritativeRecovery(recovery)
        );
        assert_recovery_fence(
            DerivedPublicationStage::PublishRootSlot,
            recovery,
            db.frontier(),
        );

        let events = vfs.events();
        assert_eq!(events.len(), 1, "exactly the planned lie must fire");
        assert!(matches!(events[0].kind, FaultKind::FsyncLie { .. }));
        assert_eq!(events[0].path, dir.join(ROOT_FILE_NAME));

        vfs.crash().await.expect("simulate process loss");
        drop(db);
        assert_reopened_vertex_matches_oracle(cx, &dir).await;
    });
}

/// Dropping an async write is the cancellation boundary callers actually own.
/// This drives the ordinary write until the root-slot fsync is observably
/// suspended, drops that future, and then proves the borrowed `Database` was
/// already fenced before the await. Chronicle D2 must survive the simulated
/// process loss and ordinary reopen must recover it exactly once.
#[test]
fn root_slot_cancellation_leaves_the_borrowed_handle_fenced_and_recoverable() {
    let dir = scratch("database-vfs-root-slot-cancel");
    under_lab_with_root(1_212, move |root, cx| async move {
        let cx = &cx;
        create_genesis(cx, &dir).await;

        // Four Chronicle durability boundaries precede derived publication
        // for this reopened database; the root-slot barrier is fifth. The
        // pending-path observation below independently pins that ordinal to
        // manifest.root before the write future is dropped, so protocol drift
        // cannot silently cancel a different operation.
        let vfs = FaultVfs::unix_with_clock(
            FaultPlan {
                latency: Trigger::Nth(5),
                latency_micros: 60_000_000,
                ..FaultPlan::faultless()
            },
            root,
        );
        let mut db = Database::open_with_vfs(cx, vfs.clone(), &dir, engine_keys())
            .await
            .expect("faultable database opens");

        let mut write = Box::pin(db.write(cx, vfs_fault_batch()));
        let pending = poll_fn(|task_cx| {
            if let Poll::Ready(result) = write.as_mut().poll(task_cx) {
                return Poll::Ready(Err(format!(
                    "write completed before cancellation reached root publication: {result:?}"
                )));
            }
            let pending = vfs.pending_latency_paths();
            if pending.is_empty() {
                Poll::Pending
            } else {
                Poll::Ready(Ok(pending))
            }
        })
        .await
        .expect("the write must suspend at an injected durability boundary");
        assert_eq!(
            pending,
            vec![dir.join(ROOT_FILE_NAME)],
            "cancellation must target the post-D2 root-slot sync"
        );

        // This is the cancellation itself. It releases the exclusive mutable
        // borrow and must leave `db` in the recovery state installed before
        // RootStore's await.
        drop(write);
        assert!(
            vfs.pending_latency_paths().is_empty(),
            "dropping the write must retire its pending latency waiter"
        );
        assert_eq!(
            vfs.events(),
            Vec::new(),
            "a cancelled delay must not be reported as fully awaited"
        );

        let state = db.state();
        assert!(
            matches!(state, DatabaseState::NeedsAuthoritativeRecovery(_)),
            "cancelled post-D2 handle remained callable: {state:?}"
        );
        let DatabaseState::NeedsAuthoritativeRecovery(recovery) = state else {
            return;
        };
        assert_eq!(recovery.durable_frontier.0, 1);
        assert_eq!(recovery.published_frontier.0, 0);
        assert_eq!(
            recovery.failed_stage,
            DerivedPublicationStage::PublishRootSlot
        );
        assert_recovery_fence(
            DerivedPublicationStage::PublishRootSlot,
            recovery,
            db.frontier(),
        );
        let refused = db.write(cx, vfs_fault_batch()).await;
        assert!(
            matches!(refused, Err(WriteError::RecoveryRequired(_))),
            "fenced database accepted another write: {refused:?}"
        );
        let Err(WriteError::RecoveryRequired(found)) = refused else {
            return;
        };
        assert_eq!(found, recovery);

        vfs.crash().await.expect("simulate process loss");
        drop(db);
        assert_reopened_vertex_matches_oracle(cx, &dir).await;
    });
}

/// Strata's production publisher already models the two instants around
/// canonical-name publication. This proves the integrated database does not
/// erase that seam: D2 remains authoritative, the borrowed handle is fenced at
/// `PublishEdgeBlocks`, and reopen repairs both sides of the rename boundary.
#[test]
fn strata_block_publication_crashes_fence_and_recover_the_integrated_spine() {
    let scenarios = [
        (
            "staging-durable",
            BlockStoreCrashPoint::AfterStagingFileSyncBeforePublication,
            "complete staging inode before canonical publication",
        ),
        (
            "canonical-inode-durable",
            BlockStoreCrashPoint::AfterBlockFileSyncBeforeStoreDirectorySync,
            "strata block inode durable before directory entry",
        ),
    ];

    under_lab(1_213, move |cx| async move {
        let cx = &cx;
        for (name, crash_at, expected_source) in scenarios {
            let dir = scratch(&format!("database-strata-{name}"));
            create_genesis(cx, &dir).await;
            let mut db = Database::open(cx, &dir, engine_keys())
                .await
                .expect("database opens");

            let error = db
                .write_with_block_store_crash(cx, block_store_fault_batch(), crash_at)
                .await
                .expect_err("the real Strata publication must stop at its crash point");
            let committed = match &error {
                WriteError::CommittedNeedsRecovery { recovery, source } => {
                    Some((*recovery, source.as_ref()))
                }
                _ => None,
            };
            assert!(
                committed.is_some(),
                "{name}: Strata crash returned the wrong error: {error:?}"
            );
            let Some((recovery, source)) = committed else {
                continue;
            };
            assert_eq!(recovery.durable_frontier.0, 1, "{name}");
            assert_eq!(recovery.published_frontier.0, 0, "{name}");
            assert_eq!(
                recovery.failed_stage,
                DerivedPublicationStage::PublishEdgeBlocks,
                "{name}"
            );
            let RebuildError::Store(BlockStoreError::Io(io_error)) = source else {
                assert!(
                    matches!(source, RebuildError::Store(BlockStoreError::Io(_))),
                    "{name}: crash lost its typed Strata source: {source:?}"
                );
                continue;
            };
            assert!(
                io_error.to_string().contains(expected_source),
                "{name}: wrong Strata crash instant: {io_error}"
            );
            assert_eq!(
                db.state(),
                DatabaseState::NeedsAuthoritativeRecovery(recovery),
                "{name}"
            );
            assert_recovery_fence(
                DerivedPublicationStage::PublishEdgeBlocks,
                recovery,
                db.neighbours(VId(1), KNOWS),
            );
            let refused = db.write(cx, block_store_fault_batch()).await;
            assert!(
                matches!(refused, Err(WriteError::RecoveryRequired(_))),
                "{name}: fenced handle accepted a second write: {refused:?}"
            );
            let Err(WriteError::RecoveryRequired(found)) = refused else {
                continue;
            };
            assert_eq!(found, recovery, "{name}");
            drop(db);

            let engine = Database::open(cx, &dir, engine_keys())
                .await
                .expect("authoritative reopen repairs Strata publication");
            assert_eq!(engine.frontier().expect("healthy frontier").0, 1, "{name}");
            let engine_neighbours = engine.neighbours(VId(1), KNOWS).expect("healthy read");
            let engine_vertices = engine.vertices().expect("healthy read");
            let engine_edges = engine.edges().expect("healthy read");
            drop(engine);

            let coordinator = CommitCoordinator::open(cx, &dir, oracle_keys())
                .await
                .expect("oracle opens the durable stream");
            let replayed = replay(cx, &coordinator).await.expect("stream replays");
            let graph = replayed
                .database
                .graph(GRAPH, BRANCH)
                .expect("oracle materialized the coordinate");
            assert_eq!(engine_neighbours, graph.neighbours(VId(1), KNOWS), "{name}");
            assert_eq!(engine_vertices.len(), graph.vertex_count(), "{name}");
            assert_eq!(engine_edges.len(), graph.edge_count(), "{name}");
            assert_eq!(engine_neighbours, vec![VId(2)], "{name}");
        }
    });
}

/// **THE TIME-TRAVEL DIFFERENTIAL (fgdb-90jx): at EVERY epoch frontier, the
/// engine's as-of answers equal the oracle replayed through that prefix.**
///
/// The frontier differential above cannot see a fold that reaches the right
/// final state through wrong intermediate ones — a delete applied one commit
/// early, an update folded into its predecessor's span. Here the oracle is
/// rebuilt six times, once per prefix, so every intermediate graph the stream
/// ever meant is compared, not just the last.
#[test]
fn the_spine_agrees_with_the_oracle_at_every_epoch() {
    let dir = scratch("epoch-agreement");
    under_lab(107, move |cx| async move {
        let cx = &cx;
        let epochs = write_history(cx, &dir).await;
        assert_eq!(epochs.len(), 7, "the fixture is seven epochs");

        // ENGINE SIDE: every epoch's answers gathered from one fresh open,
        // before the single-writer lease drops.
        let engine = Database::open(cx, &dir, engine_keys())
            .await
            .expect("reopens");
        let probes: Vec<(VId, RelationId)> = (1..=6u128)
            .flat_map(|vid| [(VId(vid), KNOWS), (VId(vid), WORKS_WITH)])
            .collect();
        type EpochAnswers = (
            Vec<Vec<VId>>,
            Vec<Option<fgdb::VertexRow>>,
            Vec<Option<fgdb::EdgeRecord>>,
            Vec<fgdb::VertexRow>,
            Vec<fgdb::EdgeRecord>,
        );
        let engine_epochs: Vec<EpochAnswers> = epochs
            .iter()
            .map(|as_of| {
                (
                    probes
                        .iter()
                        .map(|(vid, rel)| {
                            engine
                                .neighbours_at(*vid, *rel, *as_of)
                                .expect("engine reads")
                        })
                        .collect(),
                    (1..=6u128)
                        .map(|vid| engine.vertex_at(VId(vid), *as_of).expect("engine reads"))
                        .collect(),
                    (10..=17u128)
                        .map(|eid| engine.edge_at(EId(eid), *as_of).expect("engine reads"))
                        .collect(),
                    engine.vertices_at(*as_of).expect("engine scans"),
                    engine.edges_at(*as_of).expect("engine scans"),
                )
            })
            .collect();
        drop(engine);

        // ORACLE SIDE: one prefix replay per epoch, over nothing but the bytes.
        let coordinator = CommitCoordinator::open(cx, &dir, oracle_keys())
            .await
            .expect("oracle opens");
        for (as_of, (hoods, vertices, edges, vertex_scan, edge_scan)) in
            epochs.iter().zip(&engine_epochs)
        {
            let replayed = fgdb_sim::replay_through(cx, &coordinator, *as_of)
                .await
                .expect("the prefix replays");
            let graph = replayed
                .database
                .graph(GRAPH, BRANCH)
                .expect("the oracle materialized the coordinate");

            for ((vid, rel), engine_answer) in probes.iter().zip(hoods) {
                assert_eq!(
                    engine_answer,
                    &graph.neighbours(*vid, *rel),
                    "epoch {as_of:?}: {vid:?} over {rel:?}"
                );
            }
            for (vid, engine_row) in (1..=6u128).map(VId).zip(vertices) {
                let oracle_vertex = graph.vertex(vid);
                assert_eq!(
                    engine_row.is_some(),
                    oracle_vertex.is_some(),
                    "epoch {as_of:?}: {vid:?} existence"
                );
                let (Some(row), Some(vertex)) = (engine_row, oracle_vertex) else {
                    continue;
                };
                assert_eq!(
                    row.labels,
                    vertex.labels.iter().copied().collect::<Vec<_>>(),
                    "epoch {as_of:?}: {vid:?} labels"
                );
                assert_eq!(
                    row.props,
                    vertex
                        .props
                        .iter()
                        .map(|(key, value)| (*key, value.clone()))
                        .collect::<Vec<_>>(),
                    "epoch {as_of:?}: {vid:?} properties"
                );
            }
            for (eid, engine_edge) in (10..=17u128).map(EId).zip(edges) {
                let oracle_edge = graph.edge(eid);
                assert_eq!(
                    engine_edge.is_some(),
                    oracle_edge.is_some(),
                    "epoch {as_of:?}: {eid:?} existence"
                );
                let (Some(record), Some(edge)) = (engine_edge, oracle_edge) else {
                    continue;
                };
                assert_eq!(
                    (record.entry.src, record.entry.relation, record.entry.dst),
                    (edge.src, edge.relation, edge.dst),
                    "epoch {as_of:?}: {eid:?} topology"
                );
                assert_eq!(
                    record.props,
                    edge.props
                        .iter()
                        .map(|(key, value)| (*key, value.clone()))
                        .collect::<Vec<_>>(),
                    "epoch {as_of:?}: {eid:?} properties"
                );
            }

            // THE ENUMERATION DIFFERENTIAL (fgdb-9k5w): the scan agrees with
            // the oracle element-for-element, and the COUNTS close the
            // universe — the engine cannot answer a vertex the oracle lacks,
            // and equal cardinality forbids missing one the oracle has.
            assert_eq!(
                vertex_scan.len(),
                graph.vertex_count(),
                "epoch {as_of:?}: vertex scan cardinality"
            );
            for row in vertex_scan {
                let vertex = graph.vertex(row.vid);
                assert!(
                    vertex.is_some(),
                    "epoch {as_of:?}: scanned {:?} unknown to the oracle",
                    row.vid
                );
                let vertex = vertex.expect("existence just asserted");
                assert_eq!(
                    row.labels,
                    vertex.labels.iter().copied().collect::<Vec<_>>()
                );
                assert_eq!(
                    row.props,
                    vertex
                        .props
                        .iter()
                        .map(|(key, value)| (*key, value.clone()))
                        .collect::<Vec<_>>()
                );
            }
            assert_eq!(
                edge_scan.len(),
                graph.edge_count(),
                "epoch {as_of:?}: edge scan cardinality"
            );
            for record in edge_scan {
                let edge = graph.edge(record.entry.eid);
                assert!(
                    edge.is_some(),
                    "epoch {as_of:?}: scanned {:?} unknown to the oracle",
                    record.entry.eid
                );
                let edge = edge.expect("existence just asserted");
                assert_eq!(
                    (record.entry.src, record.entry.relation, record.entry.dst),
                    (edge.src, edge.relation, edge.dst)
                );
                assert_eq!(
                    record.props,
                    edge.props
                        .iter()
                        .map(|(key, value)| (*key, value.clone()))
                        .collect::<Vec<_>>()
                );
            }
        }

        // ANTI-VACUITY: six agreements about one unchanging graph would prove
        // nothing about time. Every consecutive epoch pair must differ in at
        // least one gathered answer — each commit changed the observable graph.
        assert!(
            engine_epochs.windows(2).all(|pair| pair[0] != pair[1]),
            "every epoch must observe a different graph from its predecessor"
        );
    });
}

// ---------------------------------------------------------------------------
// Model-based generated histories over the ENGINE (§15 storage oracle)
// ---------------------------------------------------------------------------

/// Deterministic generator state — SplitMix64, so a seed IS the history and a
/// failure report is a repro command, never a coincidence.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// The generator's model: enough state to propose ONLY lawful operations.
/// Validity is maintained, not filtered — a refused batch is a generator
/// defect, and the driver treats it as one.
#[derive(Default)]
struct GenModel {
    next_vid: u128,
    next_eid: u128,
    live_vertices: Vec<u128>,
    /// eid -> (src, dst); endpoints so a vertex delete cascades in the model
    /// exactly as the engine's writer cascades it.
    live_edges: Vec<(u128, u128, u128)>,
}

/// **THE GENERATED DIFFERENTIAL: N seeded random histories of creates,
/// deletes, cascades, label flips, and BOTH property-update families, each
/// compared against the oracle at EVERY epoch with counts closing the
/// universe.** The hand-built seven-epoch fixture above proves the shapes it
/// thought of; this proves the interactions nobody thought of, and a seed
/// reproduces any disagreement exactly.
#[test]
fn generated_histories_agree_with_the_oracle_at_every_epoch() {
    for seed in [11u64, 47, 203] {
        let dir = scratch(&format!("generated-{seed}"));
        under_lab(300 + seed, move |cx| async move {
            let cx = &cx;
            let mut rng = SplitMix64(seed);
            let mut model = GenModel {
                next_vid: 1,
                next_eid: 1000,
                ..GenModel::default()
            };

            // ENGINE SIDE: generate and commit 8 batches of 1..=5 lawful ops,
            // dropping and REOPENING the database halfway — the retained
            // writer's incremental fold and the from-scratch rebuild must be
            // indistinguishable under every random shape, not only under the
            // fixtures that were written knowing the answer.
            let mut db = Database::create(cx, &dir, engine_keys())
                .await
                .expect("creates");
            let mut epochs = Vec::new();
            for round in 0..8 {
                if round == 4 {
                    drop(db);
                    db = Database::open(cx, &dir, engine_keys())
                        .await
                        .expect("a mid-history reopen rebuilds and continues");
                    // The v3 head witness (GoldBarn, thread fgdb-l96k): the
                    // checkpoint-derived element-version heads must equal the
                    // full fold's on every generated shape — graph answers
                    // cannot see a head that chained through the wrong
                    // statements, so the maps are compared directly.
                    let derived = db.element_versions().expect("reads").clone();
                    drop(db);
                    let control = Database::open_rebuilding(cx, &dir, engine_keys())
                        .await
                        .expect("the rebuild control reopens");
                    assert_eq!(
                        control.element_versions().expect("reads"),
                        &derived,
                        "seed {seed}: checkpoint-derived v3 heads diverged from the fold's"
                    );
                    drop(control);
                    db = Database::open(cx, &dir, engine_keys())
                        .await
                        .expect("the checkpoint-selected session resumes");
                }
                if round == 6 {
                    // Consolidate mid-history: every epoch comparison below
                    // must still hold over the compacted generation — the
                    // answer-preservation law under shapes nobody hand-wrote.
                    db.compact(cx).await.expect("consolidates");
                }
                let mut batch = WriteBatch::new(KNOWS);
                let ops = 1 + rng.below(5);
                // Per-batch order-sensitivity bookkeeping (fgdb-kokz): the
                // model refuses to PROPOSE what the engine refuses to commit,
                // so a refusal stays a generator defect rather than noise.
                let mut touched: std::collections::BTreeSet<(u8, u128)> =
                    std::collections::BTreeSet::new();
                for _ in 0..ops {
                    match rng.below(8) {
                        0 | 1 => {
                            let vid = model.next_vid;
                            model.next_vid += 1;
                            let labels = if rng.below(2) == 0 {
                                vec![LabelId(3)]
                            } else {
                                vec![]
                            };
                            let props = if rng.below(2) == 0 {
                                vec![(PropertyKeyId(7), CanonicalScalar::Int(vid as i64))]
                            } else {
                                vec![]
                            };
                            batch.create_vertex(VId(vid), labels, props);
                            model.live_vertices.push(vid);
                        }
                        2 if model.live_vertices.len() >= 2 => {
                            let eid = model.next_eid;
                            model.next_eid += 1;
                            let src = model.live_vertices[rng.below(model.live_vertices.len())];
                            let dst = model.live_vertices[rng.below(model.live_vertices.len())];
                            let props = if rng.below(2) == 0 {
                                vec![(PropertyKeyId(11), CanonicalScalar::Int(eid as i64))]
                            } else {
                                vec![]
                            };
                            batch.add_edge(EId(eid), VId(src), VId(dst), props);
                            model.live_edges.push((eid, src, dst));
                        }
                        3 if !model.live_edges.is_empty() => {
                            let at = rng.below(model.live_edges.len());
                            if touched.contains(&(1, model.live_edges[at].0)) {
                                continue; // updated this batch — deletion is order-sensitive
                            }
                            let (eid, _, _) = model.live_edges.remove(at);
                            batch.delete_edge(EId(eid));
                        }
                        4 if !model.live_vertices.is_empty() => {
                            let at = rng.below(model.live_vertices.len());
                            let vid = model.live_vertices[at];
                            let cascade_touched = touched.contains(&(0, vid))
                                || model.live_edges.iter().any(|(eid, s, d)| {
                                    (*s == vid || *d == vid) && touched.contains(&(1, *eid))
                                });
                            if cascade_touched {
                                continue; // this batch updated it or a cascade member
                            }
                            model.live_vertices.remove(at);
                            batch.delete_vertex(VId(vid));
                            model.live_edges.retain(|(_, s, d)| *s != vid && *d != vid);
                        }
                        5 if !model.live_vertices.is_empty() => {
                            let vid = model.live_vertices[rng.below(model.live_vertices.len())];
                            if !touched.insert((0, vid)) {
                                continue; // one exact field per element per batch
                            }
                            let value = (rng.below(2) == 0)
                                .then(|| CanonicalScalar::Int(rng.next() as i64 % 1000));
                            batch.set_vertex_property(VId(vid), PropertyKeyId(7), value);
                        }
                        6 if !model.live_edges.is_empty() => {
                            let (eid, _, _) = model.live_edges[rng.below(model.live_edges.len())];
                            if !touched.insert((1, eid)) {
                                continue; // one exact field per element per batch
                            }
                            let value = (rng.below(2) == 0)
                                .then(|| CanonicalScalar::Int(rng.next() as i64 % 1000));
                            batch.set_edge_property(EId(eid), PropertyKeyId(11), value);
                        }
                        7 if !model.live_vertices.is_empty() => {
                            let vid = model.live_vertices[rng.below(model.live_vertices.len())];
                            if !touched.insert((0, vid)) {
                                continue; // coarser than the engine's per-field law: safe
                            }
                            let member = rng.below(2) == 0;
                            batch.set_vertex_label(VId(vid), LabelId(3), member);
                        }
                        _ => {
                            // The preferred family had no lawful target yet;
                            // create a vertex instead so the batch stays
                            // non-empty and the mix self-heals from empty
                            // models.
                            let vid = model.next_vid;
                            model.next_vid += 1;
                            batch.create_vertex(VId(vid), vec![], vec![]);
                            model.live_vertices.push(vid);
                        }
                    }
                }
                let frontier = db
                    .write(cx, batch)
                    .await
                    .expect("every generated batch is lawful — a refusal is a generator defect");
                epochs.push(frontier);
            }

            // Gather every epoch's engine scans AND every vertex's
            // neighbours before the lease drops — the neighbour merge is its
            // own read path (a contiguous in-place scan), so agreement on
            // edge scans alone would leave it unwitnessed.
            let probe_vids = model.next_vid;
            type GenEpoch = (
                Vec<fgdb::VertexRow>,
                Vec<fgdb::EdgeRecord>,
                Vec<(Vec<VId>, Vec<VId>)>,
            );
            let engine_epochs: Vec<GenEpoch> = epochs
                .iter()
                .map(|as_of| {
                    (
                        db.vertices_at(*as_of).expect("engine scans"),
                        db.edges_at(*as_of).expect("engine scans"),
                        (1..probe_vids)
                            .map(|vid| {
                                (
                                    db.neighbours_at(VId(vid), KNOWS, *as_of)
                                        .expect("engine reads"),
                                    db.in_neighbours_at(VId(vid), KNOWS, *as_of)
                                        .expect("engine reads"),
                                )
                            })
                            .collect(),
                    )
                })
                .collect();
            // The POST-COMPACTION head witness: the loop compacted at round 6,
            // so the checkpoint-selected open below lands on the compacted
            // generation — the one place statement collapse could hand the
            // derivation a shorter chain than the fold's. The round-4 witness
            // above never sees a compacted partition.
            let retained = db.element_versions().expect("reads").clone();
            drop(db);
            let reopened = Database::open(cx, &dir, engine_keys())
                .await
                .expect("reopens on the compacted generation");
            assert_eq!(
                reopened.element_versions().expect("reads"),
                &retained,
                "seed {seed}: post-compaction checkpoint-derived v3 heads \
                 diverged from the retained session's"
            );
            drop(reopened);
            let control = Database::open_rebuilding(cx, &dir, engine_keys())
                .await
                .expect("the rebuild control reopens");
            assert_eq!(
                control.element_versions().expect("reads"),
                &retained,
                "seed {seed}: the full fold's v3 heads diverged from the \
                 retained session's"
            );
            drop(control);

            // ORACLE SIDE: one prefix replay per epoch, over nothing but the
            // bytes; counts close the universe in both directions.
            let coordinator = CommitCoordinator::open(cx, &dir, oracle_keys())
                .await
                .expect("oracle opens");
            for (as_of, (vertex_scan, edge_scan, hoods)) in epochs.iter().zip(&engine_epochs) {
                let replayed = fgdb_sim::replay_through(cx, &coordinator, *as_of)
                    .await
                    .expect("the prefix replays");
                let graph = replayed
                    .database
                    .graph(GRAPH, BRANCH)
                    .expect("the oracle materialized the coordinate");
                assert_eq!(
                    vertex_scan.len(),
                    graph.vertex_count(),
                    "seed {seed} epoch {as_of:?}: vertex cardinality"
                );
                for row in vertex_scan {
                    let vertex = graph.vertex(row.vid);
                    assert!(
                        vertex.is_some(),
                        "seed {seed} epoch {as_of:?}: scanned {:?} unknown to the oracle",
                        row.vid
                    );
                    let vertex = vertex.expect("existence just asserted");
                    assert_eq!(
                        row.labels,
                        vertex.labels.iter().copied().collect::<Vec<_>>(),
                        "seed {seed} epoch {as_of:?}: {:?} labels",
                        row.vid
                    );
                    assert_eq!(
                        row.props,
                        vertex
                            .props
                            .iter()
                            .map(|(key, value)| (*key, value.clone()))
                            .collect::<Vec<_>>(),
                        "seed {seed} epoch {as_of:?}: {:?} properties",
                        row.vid
                    );
                }
                assert_eq!(
                    edge_scan.len(),
                    graph.edge_count(),
                    "seed {seed} epoch {as_of:?}: edge cardinality"
                );
                for record in edge_scan {
                    let edge = graph.edge(record.entry.eid);
                    assert!(
                        edge.is_some(),
                        "seed {seed} epoch {as_of:?}: scanned {:?} unknown to the oracle",
                        record.entry.eid
                    );
                    let edge = edge.expect("existence just asserted");
                    assert_eq!(
                        (record.entry.src, record.entry.dst),
                        (edge.src, edge.dst),
                        "seed {seed} epoch {as_of:?}: {:?} topology",
                        record.entry.eid
                    );
                    assert_eq!(
                        record.props,
                        edge.props
                            .iter()
                            .map(|(key, value)| (*key, value.clone()))
                            .collect::<Vec<_>>(),
                        "seed {seed} epoch {as_of:?}: {:?} properties",
                        record.entry.eid
                    );
                }
                for (vid, (hood, arrivals)) in (1..probe_vids).map(VId).zip(hoods) {
                    assert_eq!(
                        hood,
                        &graph.neighbours(vid, KNOWS),
                        "seed {seed} epoch {as_of:?}: {vid:?} neighbours"
                    );
                    // The oracle has no reverse face; derive arrivals from the
                    // already-agreed edge scan — an independent construction,
                    // which is the point (fgdb-x164).
                    let mut expected: Vec<VId> = edge_scan
                        .iter()
                        .filter(|record| record.entry.dst == vid)
                        .map(|record| record.entry.src)
                        .collect();
                    expected.sort();
                    expected.dedup();
                    assert_eq!(
                        arrivals, &expected,
                        "seed {seed} epoch {as_of:?}: {vid:?} in-neighbours"
                    );
                }
            }
        });
    }
}

/// Independent application of the same fold scenarios through the engine
/// WriteBatch path and a standalone reference Transaction — not a replay of
/// the engine stream. Agreement here is what 819.2's differential asked
/// for: same intents, two implementations, same live graph.
#[test]
fn net_effect_fold_agrees_independently_with_reference_transactions() {
    let dir = scratch("nenf-independent");
    under_lab(119, move |cx| async move {
        let cx = &cx;
        let rank = PropertyKeyId(100);

        let mut engine = Database::create(cx, &dir, engine_keys())
            .await
            .expect("creates");
        let mut seed = WriteBatch::new(KNOWS);
        seed.create_vertex(VId(1), vec![], vec![(rank, CanonicalScalar::Int(5))]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.create_vertex(VId(8), vec![], vec![(rank, CanonicalScalar::Int(1))]);
        engine.write(cx, seed).await.expect("seeds");

        let mut two_sets = WriteBatch::new(KNOWS);
        two_sets.set_vertex_property(VId(1), rank, Some(CanonicalScalar::Int(3)));
        two_sets.set_vertex_property(VId(1), rank, Some(CanonicalScalar::Int(7)));
        engine.write(cx, two_sets).await.expect("two sets fold");

        let mut create_set_delete = WriteBatch::new(KNOWS);
        create_set_delete.create_vertex(VId(9), vec![], vec![(rank, CanonicalScalar::Int(1))]);
        create_set_delete.set_vertex_property(VId(9), rank, Some(CanonicalScalar::Int(4)));
        create_set_delete.delete_vertex(VId(9));
        engine
            .write(cx, create_set_delete)
            .await
            .expect("create+set+delete cancels");

        let mut set_delete = WriteBatch::new(KNOWS);
        set_delete.set_vertex_property(VId(8), rank, Some(CanonicalScalar::Int(3)));
        set_delete.delete_vertex(VId(8));
        engine
            .write(cx, set_delete)
            .await
            .expect("set+delete of an existing vertex");
        drop(engine);

        let engine = Database::open(cx, &dir, engine_keys())
            .await
            .expect("reopens");
        let engine_v1 = engine.vertex(VId(1)).expect("reads").expect("v1 live");
        assert_eq!(engine_v1.props, vec![(rank, CanonicalScalar::Int(7))]);
        assert!(engine.vertex(VId(8)).expect("reads").is_none());
        assert!(engine.vertex(VId(9)).expect("reads").is_none());
        assert!(engine.vertex(VId(2)).expect("reads").is_some());
        drop(engine);

        let mut oracle = fgdb_reference::ReferenceDatabase::new();
        let semantics = fgdb_types::ObjectId([0x11; 32]);
        let mut txn = fgdb_reference::txn::Transaction::begin_genesis(&oracle, GRAPH, BRANCH)
            .expect("genesis");
        txn.execute(&[
            fgdb_reference::intents::Statement::new(vec![
                fgdb_reference::intents::Intent::CreateVertex {
                    vid: VId(1),
                    labels: vec![],
                    props: vec![(rank, CanonicalScalar::Int(5))],
                },
            ]),
            fgdb_reference::intents::Statement::new(vec![
                fgdb_reference::intents::Intent::CreateVertex {
                    vid: VId(2),
                    labels: vec![],
                    props: vec![],
                },
            ]),
            fgdb_reference::intents::Statement::new(vec![
                fgdb_reference::intents::Intent::CreateVertex {
                    vid: VId(8),
                    labels: vec![],
                    props: vec![(rank, CanonicalScalar::Int(1))],
                },
            ]),
        ])
        .expect("oracle seeds");
        txn.commit(
            &mut oracle,
            KNOWS,
            semantics,
            fgdb_types::CommitSeq(1),
            fgdb_types::LogicalCommandSeq(10),
        )
        .expect("oracle seed commits")
        .committed_parts()
        .expect("oracle seed wrote");

        let mut txn =
            fgdb_reference::txn::Transaction::begin(&oracle, GRAPH, BRANCH).expect("oracle begin");
        txn.execute(&[
            fgdb_reference::intents::Statement::new(vec![
                fgdb_reference::intents::Intent::SetProp {
                    elem: fgdb_delta_types::ElementId::Vertex(VId(1)),
                    name: rank,
                    value: CanonicalScalar::Int(3),
                },
            ]),
            fgdb_reference::intents::Statement::new(vec![
                fgdb_reference::intents::Intent::SetProp {
                    elem: fgdb_delta_types::ElementId::Vertex(VId(1)),
                    name: rank,
                    value: CanonicalScalar::Int(7),
                },
            ]),
        ])
        .expect("oracle two sets");
        txn.commit(
            &mut oracle,
            KNOWS,
            semantics,
            fgdb_types::CommitSeq(2),
            fgdb_types::LogicalCommandSeq(20),
        )
        .expect("oracle two sets commit")
        .committed_parts()
        .expect("oracle two sets wrote");

        let mut txn =
            fgdb_reference::txn::Transaction::begin(&oracle, GRAPH, BRANCH).expect("oracle begin");
        txn.execute(&[
            fgdb_reference::intents::Statement::new(vec![
                fgdb_reference::intents::Intent::CreateVertex {
                    vid: VId(9),
                    labels: vec![],
                    props: vec![(rank, CanonicalScalar::Int(1))],
                },
            ]),
            fgdb_reference::intents::Statement::new(vec![
                fgdb_reference::intents::Intent::SetProp {
                    elem: fgdb_delta_types::ElementId::Vertex(VId(9)),
                    name: rank,
                    value: CanonicalScalar::Int(4),
                },
            ]),
            fgdb_reference::intents::Statement::new(vec![
                fgdb_reference::intents::Intent::DeleteVertex { vid: VId(9) },
            ]),
        ])
        .expect("oracle create+set+delete");
        txn.commit(
            &mut oracle,
            KNOWS,
            semantics,
            fgdb_types::CommitSeq(3),
            fgdb_types::LogicalCommandSeq(30),
        )
        .expect("oracle cancel commits")
        .committed_parts()
        .expect("oracle cancel wrote");

        let mut txn =
            fgdb_reference::txn::Transaction::begin(&oracle, GRAPH, BRANCH).expect("oracle begin");
        txn.execute(&[
            fgdb_reference::intents::Statement::new(vec![
                fgdb_reference::intents::Intent::SetProp {
                    elem: fgdb_delta_types::ElementId::Vertex(VId(8)),
                    name: rank,
                    value: CanonicalScalar::Int(3),
                },
            ]),
            fgdb_reference::intents::Statement::new(vec![
                fgdb_reference::intents::Intent::DeleteVertex { vid: VId(8) },
            ]),
        ])
        .expect("oracle set+delete");
        txn.commit(
            &mut oracle,
            KNOWS,
            semantics,
            fgdb_types::CommitSeq(4),
            fgdb_types::LogicalCommandSeq(40),
        )
        .expect("oracle set+delete commits")
        .committed_parts()
        .expect("oracle set+delete wrote");

        let graph = oracle.graph(GRAPH, BRANCH).expect("oracle coordinate");
        assert_eq!(
            graph.vertex(VId(1)).expect("v1").props.get(&rank),
            Some(&CanonicalScalar::Int(7))
        );
        assert!(graph.vertex(VId(8)).is_none());
        assert!(graph.vertex(VId(9)).is_none());
        assert!(graph.vertex(VId(2)).is_some());
    });
}
