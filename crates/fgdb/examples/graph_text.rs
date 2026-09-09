//! cargo run -p fgdb --example graph_text
//!
//! Named multi-relation text, reusable parameters, properties, and ALL output.
//! This uses the bounded connected-pattern profile, not the legacy GQL artifact
//! parser. Its prepared result enters the same governed execution APIs.

use asupersync::{Budget, CancelKind, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

const KNOWS: RelationId = RelationId(1);
const WORKS_AT: RelationId = RelationId(2);
const SHIPS: RelationId = RelationId(3);
const BACKS: RelationId = RelationId(4);
const PERSON: LabelId = LabelId(1);
const SCORE: PropertyKeyId = PropertyKeyId(1);
const NAME: PropertyKeyId = PropertyKeyId(2);

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "KNOWS") => Some(GraphSymbol::Relation(KNOWS)),
        (GraphSymbolKind::Relation, "WORKS_AT") => Some(GraphSymbol::Relation(WORKS_AT)),
        (GraphSymbolKind::Relation, "SHIPS") => Some(GraphSymbol::Relation(SHIPS)),
        (GraphSymbolKind::Relation, "BACKS") => Some(GraphSymbol::Relation(BACKS)),
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Property, "score") => Some(GraphSymbol::Property(SCORE)),
        (GraphSymbolKind::Property, "name") => Some(GraphSymbol::Property(NAME)),
        _ => None,
    }
}

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
        // Demonstration keys for an ephemeral database, not production secrets.
        let keys = DatabaseKeys::new([0xc1; 32], DatabaseSecurityNamespaceId([0xc2; 32]), [0xc3; 32]);
        let mut db = Database::open_memory(&commit_cx, keys).await?;
        let mut initializer = WriteBatch::new(RelationId(9));
        for id in 1..=4 {
            let mut properties = vec![(SCORE, CanonicalScalar::Int(if id == 3 { 90 } else { 10 }))];
            if id == 3 { properties.push((NAME, CanonicalScalar::ucs_basic_text("Foundry")?)); }
            initializer.ensure_vertex(VId(id), if id == 1 { vec![PERSON] } else { vec![] }, properties);
        }
        let mut batches = vec![initializer];
        for (eid, relation, source, destination) in [
            (10, KNOWS, 1, 2), (11, KNOWS, 1, 2), (20, WORKS_AT, 2, 3),
            (30, SHIPS, 3, 4), (40, BACKS, 4, 1),
        ] {
            let mut batch = WriteBatch::new(relation);
            batch.add_edge(EId(eid), VId(source), VId(destination), vec![]);
            batches.push(batch);
        }
        let basis = db.write_atomic(&commit_cx, batches).await?;
        let pinned = db.read_session()?;
        let template = PreparedGraphText::prepare(
            "MATCH (person:Person)-[:KNOWS]->(friend)-[:WORKS_AT]->(company)-[:SHIPS]->(carrier),
                   (carrier)-[:BACKS]->(person)
             WHERE company.score >= $minimum AND person <> carrier
             RETURN ALL person AS owner,company.name AS company,carrier
             LIMIT $take",
            symbols,
        )?;
        assert_eq!(template.parameter_schema().len(), 2);
        let arguments = GqlParameters::new().with_int64("minimum", 80)?.with_uint64("take", 10)?;
        let pattern = template.bind_parameters(&arguments)?;
        let policy = GqlQueryPolicy::new(100, 10, 1_000_000, 100_000);
        let initial = db.execute_graph_pattern_governed(&query_cx, &pattern, policy)?;
        assert_eq!(initial.value.len(), 2, "the two KNOWS edges are distinct occurrences");
        assert_eq!(pattern.columns(), &["owner", "company", "carrier"]);
        assert_eq!(initial.value[0].get(0).and_then(|value| value.as_vertex()), Some(VId(1)));
        for row in &initial.value {
            println!("owner={:?}, company={:?}, carrier={:?}",
                row.get(0).and_then(|value| value.as_vertex()),
                row.get(1).and_then(|value| value.as_scalar()),
                row.get(2).and_then(|value| value.as_vertex()));
        }
        let exact = GqlQueryPolicy::new(initial.rows.snapshot_records, initial.rows.result_rows,
            initial.evaluator.work_units, initial.evaluator.scratch_entries);
        assert_eq!(db.execute_graph_pattern_governed(&query_cx, &pattern, exact)?, initial);

        // Rebinding calls neither the text lexer nor the symbol resolver.
        let stricter = template.bind_parameters(&GqlParameters::new().with_int64("minimum", 100)?.with_uint64("take", 10)?)?;
        assert!(db.execute_graph_pattern_governed(&query_cx, &stricter, policy)?.value.is_empty());
        assert_eq!(db.execute_graph_pattern_governed(&query_cx, &pattern, policy)?.value, initial.value);

        let mut txn = db.begin(&txn_cx)?;
        let mut staged = WriteBatch::new(KNOWS);
        staged.set_vertex_property(VId(3), SCORE, Some(CanonicalScalar::Int(10)));
        txn.write(&mut db, staged)?;
        assert!(txn.execute_graph_pattern_governed(&db, &query_cx, &pattern, policy)?.value.is_empty());
        assert_eq!(db.execute_graph_pattern_governed(&query_cx, &pattern, policy)?.value, initial.value);
        txn.commit(&mut db, &commit_cx).await?;
        assert!(db.execute_graph_pattern_governed(&query_cx, &pattern, policy)?.value.is_empty());
        assert_eq!(db.execute_graph_pattern_governed_at(&query_cx, &pattern, basis, policy)?.value, initial.value);
        assert_eq!(pinned.execute_graph_pattern_governed(&query_cx, &pattern, policy)?.value, initial.value);
        root.cancel_with(CancelKind::User, Some("text example complete"));
        assert!(matches!(db.execute_graph_pattern_governed(&query_cx, &pattern, policy), Err(GqlQueryError::Interrupted(_))));
        println!("OK: long text, typed rebinding, correlated properties, bags, history, staging and cancellation");
        Ok(())
    })
}
