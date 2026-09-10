//! cargo run -p fgdb --example graph_aggregation
//!
//! Streaming summaries with HAVING, explicit group ordering and pagination.
//! HAVING in this bounded text profile accepts AND-conjoined integer tests and
//! IS [NOT] NULL on projected aliases or repeated projected expressions. Text
//! thresholds use Int64 parameters/literals; typed result clauses also accept
//! i128 thresholds without narrowing the u64 counts or i128 sums being tested.
//! ORDER BY accepts projected columns/expressions, ASC/DESC and NULLS FIRST/LAST
//! (default LAST in either direction); canonical group keys break all ties.
//! No hidden aggregate, arbitrary expression or general GQL conformance claim.
//! The ranked prefix borrows group state; only returned rows copy payloads.
//! All groups still need aggregation state: this is not spill-backed execution
//! or a peak-memory/allocator-byte bound. Source, grouping, HAVING, selection
//! and output share the same logical work/scratch and cancellation policy.

use asupersync::{Budget, CancelKind, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateTextSlot, GraphSymbol,
    GraphSymbolKind, PreparedGraphAggregateText,
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
        let created = db
            .write_atomic(&commit_cx, vec![entities, first, second])
            .await?;
        let pinned = db.read_session()?;

        let text = PreparedGraphAggregateText::prepare(
            "MATCH (person)-[:LINK]->(bridge)-[:TO]->(item) \
             RETURN person AS owner, COUNT(*) AS paths, \
             COUNT(item.amount) AS valued_paths, COUNT(DISTINCT item.amount) AS unique_amounts, \
             SUM(item.amount) AS total, MIN(item.amount) AS least, MAX(item.amount) AS greatest \
             GROUP BY person HAVING paths >= $min_paths AND total IS NOT NULL \
             ORDER BY total ASC NULLS LAST, owner ASC LIMIT $groups",
            |kind, name| match (kind, name) {
                (GraphSymbolKind::Relation, "LINK") => Some(GraphSymbol::Relation(LINK)),
                (GraphSymbolKind::Relation, "TO") => Some(GraphSymbol::Relation(TO)),
                (GraphSymbolKind::Property, "amount") => Some(GraphSymbol::Property(AMOUNT)),
                _ => None,
            },
        )?;
        let summary = text.bind_parameters(
            &GqlParameters::new()
                .with_int64("min_paths", 0)?
                .with_uint64("groups", 10)?,
        )?;
        assert_eq!(text.output_slots()[0], GraphAggregateTextSlot::GroupKey(0));
        assert_eq!(text.output_slots()[1], GraphAggregateTextSlot::Aggregate(0));
        let policy = GqlQueryPolicy::new(100, 10, 100_000, 10_000);
        let before = db.execute_graph_aggregate_governed(&query_cx, &summary, policy)?;
        assert_eq!(summary.key_columns(), &["owner"]);
        assert_eq!(before.rows.snapshot_records, 6);
        assert_eq!(before.value.len(), 2);
        // Ascending total ranks owner 4 ahead of owner 1, not by vertex ID.
        assert_eq!(before.value[0].keys()[0].as_vertex(), Some(VId(4)));
        assert_eq!(before.value[0].get(0).and_then(|v| v.as_count()), Some(3));
        assert_eq!(before.value[0].get(1).and_then(|v| v.as_count()), Some(2));
        assert_eq!(before.value[0].get(2).and_then(|v| v.as_count()), Some(1));
        assert_eq!(
            before.value[0].get(3).and_then(|v| v.as_integer()),
            Some(14)
        );
        assert_eq!(before.value[1].get(0).and_then(|v| v.as_count()), Some(6));
        assert_eq!(
            before.value[1].get(3).and_then(|v| v.as_integer()),
            Some(28)
        );
        let exact = GqlQueryPolicy::new(
            before.rows.snapshot_records,
            before.rows.result_rows,
            before.evaluator.work_units,
            before.evaluator.scratch_entries,
        );
        assert_eq!(
            db.execute_graph_aggregate_governed(&query_cx, &summary, exact)?,
            before
        );
        let page = text.bind_parameters(
            &GqlParameters::new()
                .with_int64("min_paths", 0)?
                .with_uint64("groups", 1)?,
        )?;
        assert_eq!(
            db.execute_graph_aggregate_governed(&query_cx, &page, policy)?
                .value,
            before.value[..1]
        );
        let filtered = text.bind_parameters(
            &GqlParameters::new()
                .with_int64("min_paths", 4)?
                .with_uint64("groups", 10)?,
        )?;
        assert_eq!(
            db.execute_graph_aggregate_governed(&query_cx, &filtered, policy)?
                .value,
            before.value[1..]
        );
        assert_eq!(
            db.execute_graph_aggregate_governed(&query_cx, &summary, policy)?
                .value,
            before.value
        );
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
        assert_eq!(staged.value[0].get(0).and_then(|v| v.as_count()), Some(2));
        assert_eq!(staged.value[0].get(3).and_then(|v| v.as_integer()), Some(1));
        assert_eq!(staged.value[1].get(0).and_then(|v| v.as_count()), Some(4));
        assert_eq!(staged.value[1].get(3).and_then(|v| v.as_integer()), Some(2));
        assert_eq!(
            db.execute_graph_aggregate_governed(&query_cx, &summary, policy)?
                .value,
            before.value
        );
        txn.commit(&mut db, &commit_cx).await?;
        assert_eq!(
            db.execute_graph_aggregate_governed(&query_cx, &summary, policy)?
                .value,
            staged.value
        );
        assert_eq!(
            db.execute_graph_aggregate_governed_at(&query_cx, &summary, created, policy)?
                .value,
            before.value
        );
        assert_eq!(
            pinned
                .execute_graph_aggregate_governed(&query_cx, &summary, policy)?
                .value,
            before.value
        );

        root.cancel_with(CancelKind::User, Some("aggregate demonstration complete"));
        assert!(matches!(
            db.execute_graph_aggregate_governed(&query_cx, &summary, policy),
            Err(GqlQueryError::Interrupted(_))
        ));
        println!("OK: HAVING, ranked groups, exact sums, staging, history and cancellation");
        Ok(())
    })
}
