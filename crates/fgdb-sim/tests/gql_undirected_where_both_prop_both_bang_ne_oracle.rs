//! **Undirected `WHERE a.k != 1 AND b.m != 9`, differentially against the
//! oracle** (`fgdb-wur5`).
//!
//! The undirected twin of `gql_where_both_prop_both_bang_ne_oracle.rs`:
//! an undirected match binds each edge twice, once per orientation, and
//! the conjunction rides the binding — the anchor side must carry `k` as
//! an `Int` other than 1 AND the other side must carry `m` as an `Int`
//! other than 9, with `RETURN b` answering the other side. Both
//! conjuncts wear the C-style `!=`, which must alias `<>` on each end
//! at once, and the derivation walks BOTH orientations in plain code
//! with a missing key on either end excluded inside it. The
//! reversed-role edge is the flipped-orientation witness an
//! outgoing-only kernel misses; every failure mode keeps its own edge.

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
const FILTERED: &str = "MATCH (a)-[:R]-(b) WHERE a.k != 1 AND b.m != 9 RETURN b";

/// Other endpoints of every `:R` edge binding whose anchor side carries
/// `k != 1` AND whose other side carries `m != 9`, both as `Int`s —
/// BOTH orientations walked, missing either key out, inside the
/// derivation: the `<>`/`<>` law verbatim, because `!=` is an alias.
fn reference_other_endpoints(graph: &ReferenceGraph) -> Vec<VId> {
    let holds = |vid: VId, key: PropertyKeyId, bad: i64| {
        graph.vertex(vid).is_some_and(|vertex| {
            matches!(vertex.props.get(&key), Some(CanonicalScalar::Int(v)) if *v != bad)
        })
    };
    let mut rows = Vec::new();
    for (_, edge) in graph.iter_edges().filter(|(_, edge)| edge.relation == R) {
        if holds(edge.src, K, 1) && holds(edge.dst, M, 9) {
            rows.push(edge.dst);
        }
        if holds(edge.dst, K, 1) && holds(edge.src, M, 9) {
            rows.push(edge.src);
        }
    }
    rows.sort_unstable();
    rows.dedup();
    rows
}

#[test]
fn undirected_both_end_double_bang_inequality_equals_reference_filter() {
    let ((), report) = run_async_under_lab(0x7e_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-undirected-where-both-prop-both-bang-ne-oracle-{}",
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
            // Fails the k conjunct: the anchor-side carrier has k = 1.
            seed.create_vertex(VId(1), vec![], vec![(K, CanonicalScalar::Int(1))]);
            seed.create_vertex(VId(2), vec![], vec![(M, CanonicalScalar::Int(0))]);
            // Fails the m conjunct: the other side carries m = 9.
            seed.create_vertex(VId(3), vec![], vec![(K, CanonicalScalar::Int(5))]);
            seed.create_vertex(VId(4), vec![], vec![(M, CanonicalScalar::Int(9))]);
            // Missing k on the anchor side: satisfies neither family.
            seed.create_vertex(VId(5), vec![], vec![]);
            seed.create_vertex(VId(6), vec![], vec![(M, CanonicalScalar::Int(0))]);
            // Survives via the src-anchored orientation.
            seed.create_vertex(VId(7), vec![], vec![(K, CanonicalScalar::Int(5))]);
            seed.create_vertex(VId(8), vec![], vec![(M, CanonicalScalar::Int(0))]);
            // Missing m on the other side: out on the other end.
            seed.create_vertex(VId(9), vec![], vec![(K, CanonicalScalar::Int(5))]);
            seed.create_vertex(VId(10), vec![], vec![]);
            // The reversed-role edge: the k carrier sits on the stored
            // DEST side, so only the flipped orientation keeps it — the
            // witness an outgoing-only kernel misses.
            seed.create_vertex(VId(11), vec![], vec![(K, CanonicalScalar::Int(5))]);
            seed.create_vertex(VId(12), vec![], vec![(M, CanonicalScalar::Int(0))]);
            seed.add_edge(EId(10), VId(1), VId(2), vec![]);
            seed.add_edge(EId(11), VId(3), VId(4), vec![]);
            seed.add_edge(EId(12), VId(5), VId(6), vec![]);
            seed.add_edge(EId(13), VId(7), VId(8), vec![]);
            seed.add_edge(EId(14), VId(9), VId(10), vec![]);
            seed.add_edge(EId(15), VId(12), VId(11), vec![]);
            db.write(&commit, seed).await.expect("fixture commits");

            let bind = RelationBind::new()
                .with_relation("R", R)
                .with_property("k", K)
                .with_property("m", M);
            rows = db
                .execute_gql(FILTERED, &bind)
                .expect("undirected double-bang conjunction MATCH executes");
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
            reference_other_endpoints(graph),
            "the undirected double != conjunction equals the <>/<> \
             both-orientation derivation"
        );
        assert_eq!(
            rows,
            vec![VId(8), VId(12)],
            "the src-anchored survivor answers 8 AND the reversed-role \
             edge answers 12 via the flipped orientation — an \
             outgoing-only kernel misses 12"
        );
        assert!(
            !rows.contains(&VId(2)),
            "the k=1 carrier's row fails the anchor != conjunct"
        );
        assert!(
            !rows.contains(&VId(4)),
            "the m=9 other side fails the other != conjunct"
        );
        assert!(
            !rows.contains(&VId(6)),
            "the keyless anchor excludes its row — missing k is not k != 1"
        );
        assert!(
            !rows.contains(&VId(10)),
            "the keyless other side excludes its row — missing m is not \
             m != 9"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
