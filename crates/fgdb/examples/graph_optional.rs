//! cargo run -p fgdb --example graph_optional
//!
//! Ordered correlated OPTIONAL/EXISTS/NOT EXISTS share the existing compiler,
//! source admission, GLA binding visitor, governed policy and result collectors.
//! An OPTIONAL child exports new vertex names to later clauses. No complete
//! child match produces one null extension; real child matches keep their bag
//! multiplicity. Child predicates run before null extension. A subsequent
//! clause rejecting a real match cannot make that match become an absent one.
//!
//! Null bindings are Option<VId>, not a reserved ID. They never reach vertex
//! predicate/property sources and cannot be rebound by later correlations.
//! A later independent optional branch may still match through a nonnull outer
//! variable. EXISTS locals remain private. Final GraphValueRow cells express
//! absent vertices/properties as canonical null, and streaming aggregates apply
//! their normal null rules: COUNT(*) retains the outer occurrence, COUNT(b)
//! excludes a null b. Identity-only result APIs remain statically nonnullable.
//!
//! Existing snapshot and canonical transaction sources admit required tables
//! once, keep isolated vertices and conflict witnesses, and share one allowance
//! with traversal, null extension, grouping and output. Definitions admit up to
//! 64 clauses/edge atoms, 65 visible variables/columns, 256 predicates and 64
//! explicit identity constraints. Copied correlations can occupy 129 private
//! binding slots. Every positive child must connect to a visible correlation.
//!
//! This is the typed API, not OPTIONAL MATCH text syntax, arbitrary nested or
//! disconnected subqueries, variable-length paths, full SSI, authorized FreeJoin
//! access, spill or byte-accurate allocator/lifetime governance. The example and
//! fifteen new Rust tests are UNRUN in this connector environment without Rust.

use asupersync::{Budget, CancelKind, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GlaDirection, GraphColumn, GraphMatchClause, GraphPatternBuilder,
    GraphValueRow, IntegerComparison, VertexPredicate};
use fgdb_gql::{GraphAggregate, GqlQueryError, GqlQueryPolicy, PreparedGraphAggregate};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

const KNOWS: RelationId = RelationId(1);
const WORKS_AT: RelationId = RelationId(2);
const PERSON: LabelId = LabelId(1);
const SCORE: PropertyKeyId = PropertyKeyId(1);

fn main() {
    if let Err(error) = run() {
        eprintln!("FAILED: {error}");
        std::process::exit(1);
    }
}
fn ids(rows: &[GraphValueRow]) -> Vec<VId> {
    rows.iter().map(|row| row.get(0).and_then(|value| value.as_vertex())
        .expect("the prepared first column is a nonnullable outer identity")).collect()
}
fn run() -> Result<(), Box<dyn core::error::Error + Send + Sync>> {
    let runtime = RuntimeBuilder::new().build()?;
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let commit = contexts.commit(); let query_cx = contexts.query(); let txn_cx = contexts.txn();
    runtime.block_on(async move {
        // Fixture keys belong only to this transient demonstration database.
        let keys = DatabaseKeys::new([0xd1; 32], DatabaseSecurityNamespaceId([0xd2; 32]), [0xd3; 32]);
        let mut db = Database::open_memory(&commit, keys).await?;
        let mut vertices = WriteBatch::new(RelationId(9));
        for id in 0..3 { vertices.create_vertex(VId(id), vec![PERSON], vec![]); }
        for id in [10, 11] { vertices.create_vertex(VId(id), vec![], vec![]); }
        vertices.create_vertex(VId(20), vec![], vec![(SCORE, CanonicalScalar::Int(7))]);
        let mut first = WriteBatch::new(KNOWS);
        first.add_edge(EId(1), VId(0), VId(10), vec![]);
        first.add_edge(EId(2), VId(0), VId(10), vec![]);
        first.add_edge(EId(3), VId(1), VId(11), vec![]);
        let mut second = WriteBatch::new(WORKS_AT);
        second.add_edge(EId(4), VId(10), VId(20), vec![]);
        second.add_edge(EId(5), VId(10), VId(20), vec![]);
        let created = db.write_atomic(&commit, vec![vertices, first, second]).await?;
        let pinned = db.read_session()?;

        let mut people = GraphPatternBuilder::new();
        people.vertex("person")?;
        people.filter("person", VertexPredicate::HasLabel(PERSON))?;
        let mut friendship = GraphPatternBuilder::new();
        for name in ["person", "friend"] { friendship.vertex(name)?; }
        friendship.edge("person", KNOWS, GlaDirection::Forward, "friend")?;
        let mut employment = GraphPatternBuilder::new();
        for name in ["friend", "company"] { employment.vertex(name)?; }
        employment.edge("friend", WORKS_AT, GlaDirection::Forward, "company")?;
        employment.filter("company", VertexPredicate::IntegerProperty {
            key: SCORE, comparison: IntegerComparison::GreaterOrEqual, value: 5,
        })?;
        let columns = [GraphColumn::vertex("owner", "person"), GraphColumn::vertex("friend", "friend"),
            GraphColumn::vertex("company", "company"), GraphColumn::property("score", "company", SCORE)];
        let clauses = [GraphMatchClause::optional(&friendship), GraphMatchClause::optional(&employment)];
        let pattern = people.prepare_values_with_clauses(&clauses, &columns, 0, None)?.with_duplicates();
        let policy = GqlQueryPolicy::new(100, 100, 1_000_000, 100_000);
        let before = db.execute_graph_pattern_governed(&query_cx, &pattern, policy)?;
        assert_eq!(before.rows.snapshot_records, 11);
        assert_eq!(ids(&before.value), vec![VId(0), VId(0), VId(0), VId(0), VId(1), VId(2)]);
        assert_eq!(before.value[4].get(1).and_then(|value| value.as_vertex()), Some(VId(11)));
        assert!(before.value[4].get(2).is_some_and(|value| value.is_null()));
        assert!(before.value[5].get(1).is_some_and(|value| value.is_null()));
        assert!(before.value[5].get(3).is_some_and(|value| value.is_null()));
        let exact = GqlQueryPolicy::new(11, 6, before.evaluator.work_units, before.evaluator.scratch_entries);
        assert_eq!(db.execute_graph_pattern_governed(&query_cx, &pattern, exact)?, before);

        let summary = PreparedGraphAggregate::prepare(pattern.clone(), &[0], &[
            GraphAggregate::count_rows("rows"), GraphAggregate::count("companies", 2),
            GraphAggregate::count_distinct("unique_companies", 2), GraphAggregate::sum_int("total", 3),
        ], 0, None)?;
        let totals = db.execute_graph_aggregate_governed(&query_cx, &summary, policy)?;
        assert_eq!(totals.value.len(), 3);
        assert_eq!(totals.value[0].get(0).and_then(|value| value.as_count()), Some(4));
        assert_eq!(totals.value[0].get(3).and_then(|value| value.as_integer()), Some(28));
        for row in &totals.value[1..] {
            assert_eq!(row.get(0).and_then(|value| value.as_count()), Some(1));
            assert_eq!(row.get(1).and_then(|value| value.as_count()), Some(0));
            assert!(row.get(3).is_some_and(|value| value.is_null()));
        }
        let mut bound_company = GraphPatternBuilder::new(); bound_company.vertex("company")?;
        let missing = people.prepare_values_with_clauses(&[
            clauses[0], clauses[1], GraphMatchClause::not_exists(&bound_company),
        ], &columns, 0, None)?.with_duplicates();
        assert_eq!(ids(&db.execute_graph_pattern_governed(&query_cx, &missing, policy)?.value), vec![VId(1), VId(2)]);

        let mut txn = db.begin(&txn_cx)?;
        let mut changes = WriteBatch::new(KNOWS);
        changes.delete_edge(EId(1));
        changes.ensure_edge_by_triple(EId(999), VId(0), VId(10), vec![]);
        changes.add_edge(EId(6), VId(2), VId(10), vec![]);
        changes.set_vertex_property(VId(20), SCORE, Some(CanonicalScalar::Int(9)));
        txn.write(&mut db, changes)?;
        assert!(txn.edge(&db, EId(999))?.is_none());
        let staged = txn.execute_graph_pattern_governed(&db, &query_cx, &pattern, policy)?;
        assert_eq!(ids(&staged.value), vec![VId(0), VId(0), VId(1), VId(2), VId(2)]);
        assert_eq!(ids(&txn.execute_graph_pattern_governed(&db, &query_cx, &missing, policy)?.value), vec![VId(1)]);
        let staged_totals = txn.execute_graph_aggregate_governed(&db, &query_cx, &summary, policy)?;
        for at in [0, 2] {
            assert_eq!(staged_totals.value[at].get(0).and_then(|value| value.as_count()), Some(2));
            assert_eq!(staged_totals.value[at].get(3).and_then(|value| value.as_integer()), Some(18));
        }
        assert_eq!(db.execute_graph_pattern_governed(&query_cx, &pattern, policy)?.value, before.value);
        txn.commit(&mut db, &commit).await?;
        assert_eq!(db.execute_graph_pattern_governed(&query_cx, &pattern, policy)?.value, staged.value);
        assert_eq!(db.execute_graph_pattern_governed_at(&query_cx, &pattern, created, policy)?.value, before.value);
        assert_eq!(pinned.execute_graph_pattern_governed(&query_cx, &pattern, policy)?.value, before.value);
        assert_eq!(db.execute_graph_aggregate_governed(&query_cx, &summary, policy)?.value, staged_totals.value);
        for row in &staged.value {
            println!("owner={:?}, friend={:?}, company={:?}, score={:?}",
                row.get(0).and_then(|value| value.as_vertex()),
                row.get(1).and_then(|value| value.as_vertex()),
                row.get(2).and_then(|value| value.as_vertex()),
                row.get(3).and_then(|value| value.as_scalar()));
        }
        root.cancel_with(CancelKind::User, Some("optional demonstration complete"));
        assert!(matches!(db.execute_graph_pattern_governed(&query_cx, &pattern, policy), Err(GqlQueryError::Interrupted(_))));
        println!("OK: nullable chained matches, bags, zero-preserving summaries, staging, history and cancellation");
        Ok(())
    })
}
