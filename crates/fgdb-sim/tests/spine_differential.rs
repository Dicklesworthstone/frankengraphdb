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
use fgdb::{CAPSULE_OBJECT_KIND, Database, DatabaseKeys, WriteBatch};
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::CommitCoordinator;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_sim::replay;
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, CanonicalScalar, EId, GraphId, VId};
use std::path::{Path, PathBuf};

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
        let engine_vertices: Vec<Option<fgdb::VertexRow>> =
            (1..=6u128).map(|vid| engine.vertex(VId(vid))).collect();
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
        assert_eq!(epochs.len(), 6, "the fixture is six epochs");

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
                )
            })
            .collect();
        drop(engine);

        // ORACLE SIDE: one prefix replay per epoch, over nothing but the bytes.
        let coordinator = CommitCoordinator::open(cx, &dir, oracle_keys())
            .await
            .expect("oracle opens");
        for (as_of, (hoods, vertices, edges)) in epochs.iter().zip(&engine_epochs) {
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
