//! **Incoming `WHERE b.k != 1`, differentially against the oracle**
//! (`fgdb-w5-parsers-nje-60-li19`).
//!
//! The C-style spelling of `gql_incoming_where_prop_oracle.rs`'s
//! inequality face: under the flipped arrow `(a)<-[:R]-(b)`, `b` is the
//! edge's ORIGIN and `a` its destination, and `!=` must alias `<>`
//! exactly — so the engine equals the SAME plain-code derivation: filter
//! `edge.src`'s `k` as an Int NOT equal to the literal, project
//! `edge.dst`, missing-`k` excluded inside the derivation (never
//! "trivially unequal"). The `k = 1` writer's dest and the keyless
//! writer's dest are both asserted absent — the latter is what a
//! complement-of-equality executor wrongly admits, and a kernel reading
//! the flipped variables as outgoing answers the wrong side entirely.

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
const IN_BANG_NE: &str = "MATCH (a)<-[:R]-(b) WHERE b.k != 1 RETURN a";

/// Destinations of `:R` edges whose ORIGIN carries `k` as an `Int`
/// different from 1 — the `<>` law verbatim, because `!=` is an alias
/// and not a new comparator: missing key out, inside the derivation.
fn reference_destinations_of_unequal_origins(graph: &ReferenceGraph) -> Vec<VId> {
    let mut rows: Vec<_> = graph
        .iter_edges()
        .filter(|(_, edge)| edge.relation == R)
        .filter(|(_, edge)| {
            graph.vertex(edge.src).is_some_and(|vertex| {
                matches!(vertex.props.get(&K), Some(CanonicalScalar::Int(v)) if *v != 1)
            })
        })
        .map(|(_, edge)| edge.dst)
        .collect();
    rows.sort_unstable();
    rows.dedup();
    rows
}

#[test]
fn incoming_origin_bang_inequality_equals_its_reference() {
    let ((), report) = run_async_under_lab(0x76_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-incoming-where-prop-bang-ne-oracle-{}",
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
            seed.create_vertex(VId(1), vec![], vec![(K, CanonicalScalar::Int(1))]);
            seed.create_vertex(VId(2), vec![], vec![]);
            seed.create_vertex(VId(3), vec![], vec![(K, CanonicalScalar::Int(9))]);
            seed.create_vertex(VId(4), vec![], vec![]);
            seed.create_vertex(VId(5), vec![], vec![]);
            seed.create_vertex(VId(6), vec![], vec![]);
            seed.add_edge(EId(10), VId(1), VId(2), vec![]);
            seed.add_edge(EId(11), VId(3), VId(4), vec![]);
            seed.add_edge(EId(12), VId(5), VId(6), vec![]);
            db.write(&commit, seed).await.expect("fixture commits");

            let bind = RelationBind::new()
                .with_relation("R", R)
                .with_property("k", K);
            rows = db
                .execute_gql(IN_BANG_NE, &bind)
                .expect("incoming WHERE b.k != 1 executes");
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
            reference_destinations_of_unequal_origins(graph),
            "the engine's != answer equals the <> derivation — an alias, \
             not a new comparator"
        );
        assert_eq!(
            rows,
            vec![VId(4)],
            "the k=9 origin's destination alone — a dest-side filter finds \
             no k anywhere and answers nothing, an origin projection \
             answers 3 instead of 4"
        );
        assert!(
            !rows.contains(&VId(2)),
            "the k=1 writer's dest fails the inequality"
        );
        assert!(
            !rows.contains(&VId(6)),
            "the keyless writer's dest is OUT — missing k is not k != 1"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
