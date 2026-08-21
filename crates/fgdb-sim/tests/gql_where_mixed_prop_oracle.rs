//! **Mixed `=`/`<>` conjunctions, differentially against the oracle**
//! (`fgdb-w5-parsers-nje.18`).
//!
//! The two mixed spellings on ONE fixture, each against its own plain-code
//! derivation: `a.k = 1 AND b.m <> 9` keeps the equality on the source and
//! the inequality on the dest, its twin swaps them — and their answers are
//! DISJOINT (`[4]` vs `[6]`), so a kernel that confuses which comparator
//! landed on which end, or which end carries which key, answers the wrong
//! singleton and fails both statements at once. Missing keys are excluded
//! inside both derivations (a keyless source and a keyless dest each get
//! their own edge), so neither inequality conjunct is the equality's
//! complement.

use asupersync::lab::run_async_under_lab;
use fgdb::{CAPSULE_OBJECT_KIND, Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::CommitCoordinator;
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_reference::ReferenceGraph;
use fgdb_sim::replay;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, CanonicalScalar, EId, GraphId, VId};

const R: RelationId = RelationId(1);
const K: PropertyKeyId = PropertyKeyId(7);
const M: PropertyKeyId = PropertyKeyId(9);
const EQ_NE: &str = "MATCH (a)-[:R]->(b) WHERE a.k = 1 AND b.m <> 9 RETURN b";
const NE_EQ: &str = "MATCH (a)-[:R]->(b) WHERE a.k <> 1 AND b.m = 9 RETURN b";

/// One derivation for both spellings: dests of `:R` edges whose source
/// carries `k` and whose dest carries `m` — both as `Int`s, missing keys
/// out — with the two comparators supplied per statement.
fn reference_destinations(
    graph: &ReferenceGraph,
    src_keeps: impl Fn(i64) -> bool,
    dst_keeps: impl Fn(i64) -> bool,
) -> Vec<VId> {
    let mut rows: Vec<_> = graph
        .iter_edges()
        .filter(|(_, edge)| edge.relation == R)
        .filter(|(_, edge)| {
            graph.vertex(edge.src).is_some_and(|vertex| {
                matches!(vertex.props.get(&K), Some(CanonicalScalar::Int(v)) if src_keeps(*v))
            }) && graph.vertex(edge.dst).is_some_and(|vertex| {
                matches!(vertex.props.get(&M), Some(CanonicalScalar::Int(v)) if dst_keeps(*v))
            })
        })
        .map(|(_, edge)| edge.dst)
        .collect();
    rows.sort_unstable();
    rows.dedup();
    rows
}

#[test]
fn mixed_comparator_conjunctions_equal_their_reference_filters() {
    let ((), report) = run_async_under_lab(0x45_18, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-where-mixed-prop-oracle-{}",
            std::process::id()
        ));
        let namespace = DatabaseSecurityNamespaceId([0x77; 32]);
        let eq_ne_rows;
        let ne_eq_rows;
        {
            let mut db = Database::create(
                &commit,
                &dir,
                DatabaseKeys::new([0x5a; 32], namespace, [0x3c; 32]),
            )
            .await
            .expect("database creates");
            let mut seed = WriteBatch::new(R);
            // Fails both statements: k=1 kills NE_EQ, m=9 kills EQ_NE.
            seed.create_vertex(VId(1), vec![], vec![(K, CanonicalScalar::Int(1))]);
            seed.create_vertex(VId(2), vec![], vec![(M, CanonicalScalar::Int(9))]);
            // EQ_NE's survivor: k=1, m=0.
            seed.create_vertex(VId(3), vec![], vec![(K, CanonicalScalar::Int(1))]);
            seed.create_vertex(VId(4), vec![], vec![(M, CanonicalScalar::Int(0))]);
            // NE_EQ's survivor: k=5, m=9.
            seed.create_vertex(VId(5), vec![], vec![(K, CanonicalScalar::Int(5))]);
            seed.create_vertex(VId(6), vec![], vec![(M, CanonicalScalar::Int(9))]);
            // Keyless source: out of both, whatever the comparators.
            seed.create_vertex(VId(7), vec![], vec![]);
            seed.create_vertex(VId(8), vec![], vec![(M, CanonicalScalar::Int(0))]);
            // Keyless dest: out of both on the other end.
            seed.create_vertex(VId(9), vec![], vec![(K, CanonicalScalar::Int(5))]);
            seed.create_vertex(VId(10), vec![], vec![]);
            seed.add_edge(EId(10), VId(1), VId(2), vec![]);
            seed.add_edge(EId(11), VId(3), VId(4), vec![]);
            seed.add_edge(EId(12), VId(5), VId(6), vec![]);
            seed.add_edge(EId(13), VId(7), VId(8), vec![]);
            seed.add_edge(EId(14), VId(9), VId(10), vec![]);
            db.write(&commit, seed).await.expect("fixture commits");

            let bind = RelationBind::new()
                .with_relation("R", R)
                .with_property("k", K)
                .with_property("m", M);
            eq_ne_rows = db
                .execute_gql(EQ_NE, &bind)
                .expect("a.k = 1 AND b.m <> 9 executes");
            ne_eq_rows = db
                .execute_gql(NE_EQ, &bind)
                .expect("a.k <> 1 AND b.m = 9 executes");
        }

        let keys = CapsuleKeys::new(
            [0x5a; 32],
            namespace,
            [0x3c; 32],
            CAPSULE_OBJECT_KIND,
            CapsuleProfile::balanced(),
        );
        let coordinator = CommitCoordinator::open(&commit, &dir, keys)
            .await
            .expect("independent coordinator opens");
        let reference = replay(&commit, &coordinator)
            .await
            .expect("durable stream replays")
            .database;
        let graph = reference
            .graph(GraphId(1), BranchId(1))
            .expect("reference graph exists");

        assert_eq!(
            eq_ne_rows,
            reference_destinations(graph, |k| k == 1, |m| m != 9),
            "a.k = 1 AND b.m <> 9 equals its derivation"
        );
        assert_eq!(eq_ne_rows, vec![VId(4)], "the equality-source survivor");
        assert_eq!(
            ne_eq_rows,
            reference_destinations(graph, |k| k != 1, |m| m == 9),
            "a.k <> 1 AND b.m = 9 equals its derivation"
        );
        assert_eq!(ne_eq_rows, vec![VId(6)], "the inequality-source survivor");
        // Disjointness is the point: a comparator-confused kernel answers
        // the wrong singleton and fails BOTH statements above.
        assert!(
            !eq_ne_rows.contains(&VId(6)) && !ne_eq_rows.contains(&VId(4)),
            "the two mixed spellings answer disjoint rows"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
