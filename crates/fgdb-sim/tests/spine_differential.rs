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
async fn write_history(cx: &CommitCx, dir: &Path) {
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
    first.add_edge(EId(10), VId(1), VId(2), vec![]);
    first.add_edge(EId(11), VId(1), VId(3), vec![]);
    db.write(cx, first).await.expect("first batch commits");

    // PARALLEL EDGES: same (src, dst), different EId. EId is the unconditional
    // parallel-edge discriminator (§4.1), so a fold keyed on the pair alone
    // collapses these and disagrees here.
    let mut second = WriteBatch::new(KNOWS);
    second.add_edge(EId(12), VId(1), VId(2), vec![]);
    second.add_edge(EId(13), VId(4), VId(4), vec![]); // self-loop
    db.write(cx, second).await.expect("second batch commits");

    // A SECOND RELATION over an overlapping vertex set.
    let mut third = WriteBatch::new(WORKS_WITH);
    third.add_edge(EId(14), VId(1), VId(5), vec![]);
    third.add_edge(EId(15), VId(2), VId(3), vec![]);
    db.write(cx, third).await.expect("third batch commits");

    // DELETES, with every before-image engine-derived (fgdb-p3ok). This is
    // the differential's sharpest teeth: the oracle's replay REFUSES a wrong
    // `before_version` or an inexact cascade, so these rows are validated at
    // apply time, not merely compared afterwards. VId(6) exists-then-goes in
    // one batch; VId(5) goes with its inbound WORKS_WITH edge cascaded.
    let mut fourth = WriteBatch::new(KNOWS);
    fourth.create_vertex(VId(6), vec![], vec![]);
    fourth.add_edge(EId(16), VId(6), VId(1), vec![]);
    fourth.add_edge(EId(17), VId(2), VId(4), vec![]);
    db.write(cx, fourth).await.expect("fourth batch commits");
    let mut fifth = WriteBatch::new(KNOWS);
    fifth.delete_edge(EId(12)); // ONE of the two parallel edges — its twin survives
    fifth.delete_vertex(VId(6)); // cascades EId(16)
    fifth.delete_vertex(VId(5)); // cascades EId(14), a cross-relation edge
    db.write(cx, fifth).await.expect("fifth batch commits");
}

/// **THE DIFFERENTIAL: the engine's answer equals the oracle's, for every vertex
/// and every relation in the fixture.**
#[test]
fn the_spine_agrees_with_the_reference_oracle() {
    let dir = scratch("agreement");
    under_lab(101, move |cx| async move {
        let cx = &cx;
        write_history(cx, &dir).await;

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
        write_history(cx, &dir).await;

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
