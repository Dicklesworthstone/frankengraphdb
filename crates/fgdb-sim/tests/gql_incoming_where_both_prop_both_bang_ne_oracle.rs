//! **Incoming `WHERE a.k != 1 AND b.m != 9`, differentially against the
//! oracle** (`fgdb-ysm0`).
//!
//! The incoming twin of `gql_where_both_prop_both_bang_ne_oracle.rs`:
//! under the flipped arrow `(a)<-[:R]-(b)` the pattern's `a` is the
//! stored edge's DEST and `b` its SOURCE, so the conjunction filters the
//! dest's `k` AND the source's `m` while `RETURN b` projects sources —
//! and both conjuncts wear the C-style `!=`, which must alias `<>` on
//! each end at once. The derivation composes exactly that in plain
//! code, with a missing key on EITHER end excluded inside it. Every
//! failure mode keeps its own edge, leaving exactly one surviving row;
//! a kernel that reads the flipped variables as outgoing filters the
//! wrong ends and disagrees on this fixture.

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
const FILTERED: &str = "MATCH (a)<-[:R]-(b) WHERE a.k != 1 AND b.m != 9 RETURN b";

/// Sources of `:R` edges whose DEST carries `k != 1` AND whose SOURCE
/// carries `m != 9`, both as `Int`s — the incoming `<>`/`<>` law
/// verbatim, because `!=` is an alias and not a new comparator: missing
/// either key excludes the row, inside the derivation.
fn reference_sources(graph: &ReferenceGraph) -> Vec<VId> {
    let mut rows: Vec<_> = graph
        .iter_edges()
        .filter(|(_, edge)| edge.relation == R)
        .filter(|(_, edge)| {
            graph.vertex(edge.dst).is_some_and(
                |vertex| matches!(vertex.props.get(&K), Some(CanonicalScalar::Int(v)) if *v != 1),
            ) && graph.vertex(edge.src).is_some_and(
                |vertex| matches!(vertex.props.get(&M), Some(CanonicalScalar::Int(v)) if *v != 9),
            )
        })
        .map(|(_, edge)| edge.src)
        .collect();
    rows.sort_unstable();
    rows.dedup();
    rows
}

#[test]
fn incoming_both_end_double_bang_inequality_equals_reference_filter() {
    let ((), report) = run_async_under_lab(0x7d_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-incoming-where-both-prop-both-bang-ne-oracle-{}",
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
            // Fails the k conjunct: the pattern's a (stored dest) has k=1.
            seed.create_vertex(VId(1), vec![], vec![(K, CanonicalScalar::Int(1))]);
            seed.create_vertex(VId(2), vec![], vec![(M, CanonicalScalar::Int(0))]);
            // Fails the m conjunct: the pattern's b (stored source) has m=9.
            seed.create_vertex(VId(3), vec![], vec![(K, CanonicalScalar::Int(5))]);
            seed.create_vertex(VId(4), vec![], vec![(M, CanonicalScalar::Int(9))]);
            // Missing k on the dest end: satisfies neither predicate family.
            seed.create_vertex(VId(5), vec![], vec![]);
            seed.create_vertex(VId(6), vec![], vec![(M, CanonicalScalar::Int(0))]);
            // Survives both conjuncts.
            seed.create_vertex(VId(7), vec![], vec![(K, CanonicalScalar::Int(5))]);
            seed.create_vertex(VId(8), vec![], vec![(M, CanonicalScalar::Int(0))]);
            // Missing m on the source end: out on the other end.
            seed.create_vertex(VId(9), vec![], vec![(K, CanonicalScalar::Int(5))]);
            seed.create_vertex(VId(10), vec![], vec![]);
            // Stored reversed: the m-carrying source points at the
            // k-carrying dest.
            seed.add_edge(EId(10), VId(2), VId(1), vec![]);
            seed.add_edge(EId(11), VId(4), VId(3), vec![]);
            seed.add_edge(EId(12), VId(6), VId(5), vec![]);
            seed.add_edge(EId(13), VId(8), VId(7), vec![]);
            seed.add_edge(EId(14), VId(10), VId(9), vec![]);
            db.write(&commit, seed).await.expect("fixture commits");

            let bind = RelationBind::new()
                .with_relation("R", R)
                .with_property("k", K)
                .with_property("m", M);
            rows = db
                .execute_gql(FILTERED, &bind)
                .expect("incoming double-bang conjunction MATCH executes");
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
            reference_sources(graph),
            "the incoming double != conjunction equals the <>/<> \
             derivation — one comparator in two spellings on both ends"
        );
        assert_eq!(
            rows,
            vec![VId(8)],
            "one edge survives both conjuncts, answering by its source"
        );
        assert!(
            !rows.contains(&VId(2)),
            "the k=1 dest's source fails the a-side != conjunct"
        );
        assert!(
            !rows.contains(&VId(4)),
            "the m=9 source fails the b-side != conjunct"
        );
        assert!(
            !rows.contains(&VId(6)),
            "the keyless DEST excludes its row — missing k is not k != 1"
        );
        assert!(
            !rows.contains(&VId(10)),
            "the keyless SOURCE excludes its row — missing m is not m != 9"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
