//! cargo run -p fgdb --example graph_existence
//!
//! Correlated EXISTS/NOT EXISTS compiled into the shared GLA visitor. Inner
//! names also declared outside are correlations; other names stay local to
//! that clause and cannot be projected or correlate a subsequent clause.
//! Each positive connected inner clause must have at least one correlation.
//! Multiple clauses are AND-conjoined before output DISTINCT/order/pagination.
//!
//! The first complete inner witness resolves existence and does not multiply
//! the outer bag. Source, budget and cancellation errors never become absence.
//! A node-root query admits vertices AND topology once, preserving isolated
//! vertices and charging both tables to the same source/evaluator allowance.
//! Transactions retain the corresponding insertion and observed-row witnesses
//! even after later output refusal. They remain conservative, not full SSI.
//!
//! Caps apply across the definition: 64 edge atoms, 256 predicates, 64 explicit
//! identities and 64 existential clauses; the existing per-builder/name/column
//! bounds also apply. This is not OPTIONAL null extension, nested existential
//! syntax, disconnected subqueries, EXISTS text grammar, an index-only plan,
//! registered FreeJoin, spill or byte-accurate whole-operation governance.
//! The source and evaluator are implemented; this example and the new Rust
//! tests are UNRUN in the connector environment without Cargo/rustc.

use asupersync::{Budget, CancelKind, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GlaDirection, GraphColumn, GraphExistence, GraphPatternBuilder,
    GraphValueRow, IntegerComparison, VertexPredicate};
use fgdb_gql::{GraphAggregate, GqlQueryError, GqlQueryPolicy, PreparedGraphAggregate};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

const KNOWS: RelationId = RelationId(1);
const WORKS_AT: RelationId = RelationId(2);
const PERSON: LabelId = LabelId(1);
const SCORE: PropertyKeyId = PropertyKeyId(1);
const NAME: PropertyKeyId = PropertyKeyId(2);

fn main() {
    if let Err(error) = run() {
        eprintln!("FAILED: {error}");
        std::process::exit(1);
    }
}
fn ids(rows: &[GraphValueRow]) -> Vec<VId> {
    rows.iter().map(|row| row.get(0).and_then(|value| value.as_vertex())
        .expect("the prepared first column is an outer vertex")).collect()
}
fn run() -> Result<(), Box<dyn core::error::Error + Send + Sync>> {
    let runtime = RuntimeBuilder::new().build()?;
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let commit_cx = contexts.commit();
    let query_cx = contexts.query();
    let txn_cx = contexts.txn();
    runtime.block_on(async move {
        let keys = DatabaseKeys::new([0xb1; 32], DatabaseSecurityNamespaceId([0xb2; 32]), [0xb3; 32]);
        let mut db = Database::open_memory(&commit_cx, keys).await?;
        let mut vertices = WriteBatch::new(RelationId(9));
        for (id, name) in [(1, "Alex"), (2, "Blair"), (3, "Casey")] {
            vertices.create_vertex(VId(id), vec![PERSON], vec![(NAME, CanonicalScalar::ucs_basic_text(name)?)]);
        }
        vertices.create_vertex(VId(10), vec![], vec![]);
        vertices.create_vertex(VId(20), vec![], vec![(SCORE, CanonicalScalar::Int(7))]);
        let mut knows = WriteBatch::new(KNOWS);
        knows.add_edge(EId(1), VId(1), VId(10), vec![]);
        knows.add_edge(EId(2), VId(1), VId(10), vec![]);
        let mut works = WriteBatch::new(WORKS_AT);
        works.add_edge(EId(3), VId(10), VId(20), vec![]);
        works.add_edge(EId(4), VId(10), VId(20), vec![]);
        let created = db.write_atomic(&commit_cx, vec![vertices, knows, works]).await?;
        let pinned = db.read_session()?;

        let mut people = GraphPatternBuilder::new();
        people.vertex("person")?;
        people.filter("person", VertexPredicate::HasLabel(PERSON))?;
        let mut qualifying_connection = GraphPatternBuilder::new();
        for name in ["person", "friend", "company"] { qualifying_connection.vertex(name)?; }
        qualifying_connection.edge("person", KNOWS, GlaDirection::Forward, "friend")?;
        qualifying_connection.edge("friend", WORKS_AT, GlaDirection::Forward, "company")?;
        qualifying_connection.filter("company", VertexPredicate::IntegerProperty {
            key: SCORE, comparison: IntegerComparison::GreaterOrEqual, value: 5,
        })?;
        let columns = [GraphColumn::vertex("owner", "person"), GraphColumn::property("name", "person", NAME)];
        let missing = people.prepare_values_with_existence(
            &[GraphExistence::not_exists(&qualifying_connection)], &columns, 0, None,
        )?.with_duplicates();
        let present = people.prepare_values_with_existence(
            &[GraphExistence::exists(&qualifying_connection)], &columns, 0, None,
        )?.with_duplicates();
        let policy = GqlQueryPolicy::new(100, 10, 100_000, 10_000);
        let before = db.execute_graph_pattern_governed(&query_cx, &missing, policy)?;
        assert_eq!(ids(&before.value), vec![VId(2), VId(3)]);
        assert_eq!(before.rows.snapshot_records, 9);
        // Four concrete inner paths produce one outer occurrence, not four.
        assert_eq!(ids(&db.execute_graph_pattern_governed(&query_cx, &present, policy)?.value), vec![VId(1)]);
        let exact = GqlQueryPolicy::new(before.rows.snapshot_records, before.rows.result_rows,
            before.evaluator.work_units, before.evaluator.scratch_entries);
        assert_eq!(db.execute_graph_pattern_governed(&query_cx, &missing, exact)?, before);
        let count = PreparedGraphAggregate::prepare(missing.clone(), &[],
            &[GraphAggregate::count_rows("people_without_connection")], 0, None)?;
        assert_eq!(db.execute_graph_aggregate_governed(&query_cx, &count, policy)?.value[0]
            .get(0).and_then(|value| value.as_count()), Some(2));

        let mut txn = db.begin(&txn_cx)?;
        let mut changes = WriteBatch::new(KNOWS);
        changes.delete_edge(EId(1));
        changes.delete_edge(EId(2));
        changes.add_edge(EId(5), VId(2), VId(10), vec![]);
        txn.write(&mut db, changes)?;
        let staged = txn.execute_graph_pattern_governed(&db, &query_cx, &missing, policy)?;
        assert_eq!(ids(&staged.value), vec![VId(1), VId(3)]);
        assert_eq!(db.execute_graph_pattern_governed(&query_cx, &missing, policy)?.value, before.value);
        txn.commit(&mut db, &commit_cx).await?;
        assert_eq!(db.execute_graph_pattern_governed(&query_cx, &missing, policy)?.value, staged.value);
        assert_eq!(db.execute_graph_pattern_governed_at(&query_cx, &missing, created, policy)?.value, before.value);
        assert_eq!(pinned.execute_graph_pattern_governed(&query_cx, &missing, policy)?.value, before.value);
        for row in &staged.value {
            println!("owner={:?}, name={:?}", row.get(0).and_then(|value| value.as_vertex()),
                row.get(1).and_then(|value| value.as_scalar()));
        }
        root.cancel_with(CancelKind::User, Some("existential demonstration complete"));
        assert!(matches!(db.execute_graph_pattern_governed(&query_cx, &missing, policy), Err(GqlQueryError::Interrupted(_))));
        println!("OK: correlated absence, no witness multiplication, canonical staging, history and cancellation");
        Ok(())
    })
}
