//! cargo run -p fgdb --example graph_aggregation
//!
//! Streaming group summaries over a connected text query's compiled ALL child.

use asupersync::{Budget, CancelKind, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{
    GraphAggregate, GraphSymbol, GraphSymbolKind, GqlParameters, GqlQueryError,
    GqlQueryPolicy, PreparedGraphAggregate, PreparedGraphText,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

const LINK: RelationId = RelationId(1);
const TO: RelationId = RelationId(2);
const AMOUNT: PropertyKeyId = PropertyKeyId(1);

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
            [0xa1; 32],
            DatabaseSecurityNamespaceId([0xa2; 32]),
            [0xa3; 32],
        );
        let mut db = Database::open_memory(&commit_cx, keys).await?;
        let mut entities = WriteBatch::new(RelationId(9));
        for id in 1..=6 {
            let properties = if id == 3 {
                vec![(AMOUNT, CanonicalScalar::Int(7))]
            } else {
                vec![]
            };
            entities.create_vertex(VId(id), vec![], properties);
        }
        let mut first = WriteBatch::new(LINK);
        for (eid, owner) in [(10, 1), (11, 1), (12, 4)] {
            first.add_edge(EId(eid), VId(owner), VId(2), vec![]);
        }
        let mut second = WriteBatch::new(TO);
        for (eid, item) in [(20, 3), (21, 3), (22, 5)] {
            second.add_edge(EId(eid), VId(2), VId(item), vec![]);
        }
        let created = db.write_atomic(&commit_cx, vec![entities, first, second]).await?;
        let pinned = db.read_session()?;

        let text = PreparedGraphText::prepare(
            "MATCH (person)-[:LINK]->(bridge)-[:TO]->(item) \
             RETURN ALL person AS owner, item.amount AS amount",
            |kind, name| match (kind, name) {
                (GraphSymbolKind::Relation, "LINK") => Some(GraphSymbol::Relation(LINK)),
                (GraphSymbolKind::Relation, "TO") => Some(GraphSymbol::Relation(TO)),
                (GraphSymbolKind::Property, "amount") => Some(GraphSymbol::Property(AMOUNT)),
                _ => None,
            },
        )?;
        let input = text.bind_parameters(&GqlParameters::new())?;
        let summary = PreparedGraphAggregate::prepare(
            input,
            &[0],
            &[
                GraphAggregate::count_rows("paths"),
                GraphAggregate::count("valued_paths", 1),
                GraphAggregate::count_distinct("unique_amounts", 1),
                GraphAggregate::sum_int("total", 1),
                GraphAggregate::min("least", 1),
                GraphAggregate::max("greatest", 1),
            ],
            0,
            None,
        )?;
        let policy = GqlQueryPolicy::new(100, 10, 100_000, 10_000);
        let before = db.execute_graph_aggregate_governed(&query_cx, &summary, policy)?;
        assert_eq!(summary.key_columns(), &["owner"]);
        assert_eq!(before.rows.snapshot_records, 6);
        assert_eq!(before.value.len(), 2);
        assert_eq!(before.value[0].keys()[0].as_vertex(), Some(VId(1)));
        assert_eq!(before.value[0].get(0).and_then(|v| v.as_count()), Some(6));
        assert_eq!(before.value[0].get(1).and_then(|v| v.as_count()), Some(4));
        assert_eq!(before.value[0].get(2).and_then(|v| v.as_count()), Some(1));
        assert_eq!(before.value[0].get(3).and_then(|v| v.as_integer()), Some(28));
        assert_eq!(before.value[1].get(0).and_then(|v| v.as_count()), Some(3));
        assert_eq!(before.value[1].get(3).and_then(|v| v.as_integer()), Some(14));
        let exact = GqlQueryPolicy::new(
            before.rows.snapshot_records,
            before.rows.result_rows,
            before.evaluator.work_units,
            before.evaluator.scratch_entries,
        );
        assert_eq!(db.execute_graph_aggregate_governed(&query_cx, &summary, exact)?, before);
        for row in &before.value {
            println!(
                "owner={:?}, paths={:?}, nonnull={:?}, sum={:?}",
                row.keys()[0].as_vertex(),
                row.get(0).and_then(|v| v.as_count()),
                row.get(1).and_then(|v| v.as_count()),
                row.get(3).and_then(|v| v.as_integer()),
            );
        }

        let mut txn = db.begin(&txn_cx)?;
        let mut update = WriteBatch::new(TO);
        update.delete_edge(EId(20));
        update.set_vertex_property(VId(3), AMOUNT, Some(CanonicalScalar::Int(-2)));
        update.set_vertex_property(VId(5), AMOUNT, Some(CanonicalScalar::Int(3)));
        txn.write(&mut db, update)?;
        let staged = txn.execute_graph_aggregate_governed(&db, &query_cx, &summary, policy)?;
        assert_eq!(staged.value[0].get(0).and_then(|v| v.as_count()), Some(4));
        assert_eq!(staged.value[0].get(3).and_then(|v| v.as_integer()), Some(2));
        assert_eq!(staged.value[1].get(0).and_then(|v| v.as_count()), Some(2));
        assert_eq!(staged.value[1].get(3).and_then(|v| v.as_integer()), Some(1));
        assert_eq!(db.execute_graph_aggregate_governed(&query_cx, &summary, policy)?.value, before.value);
        txn.commit(&mut db, &commit_cx).await?;
        assert_eq!(db.execute_graph_aggregate_governed(&query_cx, &summary, policy)?.value, staged.value);
        assert_eq!(db.execute_graph_aggregate_governed_at(&query_cx, &summary, created, policy)?.value, before.value);
        assert_eq!(pinned.execute_graph_aggregate_governed(&query_cx, &summary, policy)?.value, before.value);

        root.cancel_with(CancelKind::User, Some("aggregate demonstration complete"));
        assert!(matches!(db.execute_graph_aggregate_governed(&query_cx, &summary, policy),
            Err(GqlQueryError::Interrupted(_))));
        println!("OK: grouped counts, exact integer sums, canonical staging, history and cancellation");
        Ok(())
    })
}
