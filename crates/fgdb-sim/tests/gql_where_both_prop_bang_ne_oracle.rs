//! **`WHERE a.k != 1 AND b.m <> 9`, differentially against the oracle**
//! (`fgdb-w5-parsers-nje-64-0avu`).
//!
//! The mixed-spelling twin of `gql_where_both_prop_ne_oracle.rs`: the
//! source conjunct wears the C-style `!=` while the dest conjunct keeps
//! the diamond `<>`, and both must mean the SAME comparator — so the
//! engine equals the verbatim `<>`/`<>` derivation: a row survives only
//! when the SOURCE carries `k` as an `Int` other than 1 AND the DEST
//! carries `m` as an `Int` other than 9, a missing key on EITHER end
//! excluded inside the derivation. Every failure mode keeps its own
//! edge, leaving exactly one surviving row.

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
const FILTERED: &str = "MATCH (a)-[:R]->(b) WHERE a.k != 1 AND b.m <> 9 RETURN b";

/// Dests of `:R` edges whose source carries `k != 1` AND whose dest
/// carries `m != 9`, both as `Int`s — the `<>`/`<>` law verbatim,
/// because `!=` is an alias and not a new comparator: missing either
/// key excludes the row, inside the derivation.
fn reference_destinations(graph: &ReferenceGraph) -> Vec<VId> {
    let mut rows: Vec<_> = graph
        .iter_edges()
        .filter(|(_, edge)| edge.relation == R)
        .filter(|(_, edge)| {
            graph.vertex(edge.src).is_some_and(|vertex| {
                matches!(vertex.props.get(&K), Some(CanonicalScalar::Int(v)) if *v != 1)
            }) && graph.vertex(edge.dst).is_some_and(|vertex| {
                matches!(vertex.props.get(&M), Some(CanonicalScalar::Int(v)) if *v != 9)
            })
        })
        .map(|(_, edge)| edge.dst)
        .collect();
    rows.sort_unstable();
    rows.dedup();
    rows
}

#[test]
fn both_end_mixed_bang_inequality_equals_reference_filter() {
    let ((), report) = run_async_under_lab(0x7a_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-where-both-prop-bang-ne-oracle-{}",
            std::process::id()
        ));
        let namespace = DatabaseSecurityNamespaceId([0x77; 32]);
        let rows;
        {
            let mut db = Database::create(
                &commit,
                &dir,
                DatabaseKeys::new([0x5a; 32], namespace, [0x3c; 32]),
            )
            .await
            .expect("database creates");
            let mut seed = WriteBatch::new(R);
            // Fails the k conjunct: source carries k = 1.
            seed.create_vertex(VId(1), vec![], vec![(K, CanonicalScalar::Int(1))]);
            seed.create_vertex(VId(2), vec![], vec![(M, CanonicalScalar::Int(0))]);
            // Fails the m conjunct: dest carries m = 9.
            seed.create_vertex(VId(3), vec![], vec![(K, CanonicalScalar::Int(5))]);
            seed.create_vertex(VId(4), vec![], vec![(M, CanonicalScalar::Int(9))]);
            // Missing k on the source: satisfies neither predicate family.
            seed.create_vertex(VId(5), vec![], vec![]);
            seed.create_vertex(VId(6), vec![], vec![(M, CanonicalScalar::Int(0))]);
            // Survives both conjuncts.
            seed.create_vertex(VId(7), vec![], vec![(K, CanonicalScalar::Int(5))]);
            seed.create_vertex(VId(8), vec![], vec![(M, CanonicalScalar::Int(0))]);
            // Missing m on the dest: out on the other end.
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
            rows = db
                .execute_gql(FILTERED, &bind)
                .expect("mixed-spelling conjunction MATCH executes");
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
            rows,
            reference_destinations(graph),
            "the mixed != / <> conjunction equals the <>/<> derivation — \
             one comparator in two spellings"
        );
        assert_eq!(rows, vec![VId(8)], "one edge survives both conjuncts");
        assert!(
            !rows.contains(&VId(2)),
            "the k=1 source's dest fails the != conjunct"
        );
        assert!(
            !rows.contains(&VId(4)),
            "the m=9 dest fails the <> conjunct"
        );
        assert!(
            !rows.contains(&VId(6)),
            "the keyless SOURCE excludes its row — missing k is not k != 1"
        );
        assert!(
            !rows.contains(&VId(10)),
            "the keyless DEST excludes its row — missing m is not m <> 9"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
