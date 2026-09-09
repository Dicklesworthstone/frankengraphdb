//! cargo run -p fgdb --example graph_patterns
//!
//! A five-edge connected motif, not a sequence of separately queried hops.

use asupersync::{Budget, CancelKind, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{
    GlaDirection, GraphPatternBuilder, IntegerComparison, VertexPredicate,
};
use fgdb_gql::{GqlQueryError, GqlQueryPolicy};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

const OWNS: RelationId = RelationId(1);
const BUYS_FROM: RelationId = RelationId(2);
const SHIPS_TO: RelationId = RelationId(3);
const FINANCES: RelationId = RelationId(4);
const BACKS: RelationId = RelationId(5);
const COMPANY: LabelId = LabelId(1);
const RISK: PropertyKeyId = PropertyKeyId(1);

fn main() {
    if let Err(error) = run() {
        eprintln!("FAILED: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn core::error::Error + Send + Sync>> {
    let runtime = RuntimeBuilder::new().build()?;
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let commit_cx = contexts.commit();
    let query_cx = contexts.query();
    let txn_cx = contexts.txn();
    runtime.block_on(async move {
        // Fixture keys for this transient demonstration database only.
        let keys = DatabaseKeys::new(
            [0xe1; 32],
            DatabaseSecurityNamespaceId([0xe2; 32]),
            [0xe3; 32],
        );
        let mut db = Database::open_memory(&commit_cx, keys).await?;
        let mut entities = WriteBatch::new(RelationId(9));
        for id in 1..=4 {
            entities.ensure_vertex(VId(id), vec![COMPANY],
                vec![(RISK, CanonicalScalar::Int(if id == 3 { 90 } else { 10 }))]);
        }
        let mut batches = vec![entities];
        for (id, relation, source, destination) in [
            (10, OWNS, 1, 2),
            (11, BUYS_FROM, 2, 3),
            (12, SHIPS_TO, 3, 4),
            (13, FINANCES, 4, 1),
            (14, BACKS, 1, 3),
        ] {
            let mut batch = WriteBatch::new(relation);
            batch.ensure_edge_by_triple(EId(id), VId(source), VId(destination), vec![]);
            batches.push(batch);
        }
        let created = db.write_atomic(&commit_cx, batches).await?;
        let pinned = db.read_session()?;

        let mut builder = GraphPatternBuilder::new();
        for name in ["company", "holding", "supplier", "carrier"] {
            builder.vertex(name)?;
        }
        builder.edge("company", OWNS, GlaDirection::Forward, "holding")?;
        // Both endpoints of this atom are initially unbound. The connected
        // compiler defers it until the next atom has bound the supplier.
        builder.edge("supplier", SHIPS_TO, GlaDirection::Forward, "carrier")?;
        builder.edge("holding", BUYS_FROM, GlaDirection::Forward, "supplier")?;
        builder.edge("carrier", FINANCES, GlaDirection::Forward, "company")?;
        builder.edge("company", BACKS, GlaDirection::Forward, "supplier")?;
        builder.filter("company", VertexPredicate::HasLabel(COMPANY))?;
        builder.filter("supplier", VertexPredicate::IntegerProperty {
            key: RISK,
            comparison: IntegerComparison::GreaterOrEqual,
            value: 80,
        })?;
        builder.identity("company", "carrier", false)?;
        let pattern = builder.prepare("carrier", 0, Some(10))?;
        let policy = GqlQueryPolicy::new(100, 10, 100_000, 10_000);
        let initial = db.execute_graph_pattern_governed(&query_cx, &pattern, policy)?;
        assert_eq!(initial.value, vec![VId(4)]);
        let exact = GqlQueryPolicy::new(
            initial.rows.snapshot_records,
            initial.rows.result_rows,
            initial.evaluator.work_units,
            initial.evaluator.scratch_entries,
        );
        assert_eq!(db.execute_graph_pattern_governed(&query_cx, &pattern, exact)?, initial);
        println!("matching carriers: {:?}", initial.value);
        println!("source rows: {}; work: {}; scratch entries: {}",
            initial.rows.snapshot_records, initial.evaluator.work_units,
            initial.evaluator.scratch_entries);

        let mut txn = db.begin(&txn_cx)?;
        let mut reduction = WriteBatch::new(BUYS_FROM);
        reduction.set_vertex_property(VId(3), RISK, Some(CanonicalScalar::Int(10)));
        txn.write(&mut db, reduction)?;
        assert!(txn.execute_graph_pattern_governed(&db, &query_cx, &pattern, policy)?.value.is_empty());
        assert_eq!(db.execute_graph_pattern_governed(&query_cx, &pattern, policy)?.value, vec![VId(4)]);
        txn.commit(&mut db, &commit_cx).await?;
        assert!(db.execute_graph_pattern_governed(&query_cx, &pattern, policy)?.value.is_empty());
        assert_eq!(pinned.execute_graph_pattern_governed(&query_cx, &pattern, policy)?.value, vec![VId(4)]);
        assert_eq!(db.execute_graph_pattern_governed_at(&query_cx, &pattern, created, policy)?.value, vec![VId(4)]);

        root.cancel_with(CancelKind::User, Some("demonstration complete"));
        assert!(matches!(db.execute_graph_pattern_governed(&query_cx, &pattern, policy),
            Err(GqlQueryError::Interrupted(_))));
        println!("OK: connected motif, exact limits, canonical overlay, pinned history and cancellation");
        Ok(())
    })
}
