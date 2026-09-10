//! cargo run -p fgdb --example scalar_parameters
//!
//! Canonical scalar arguments share one GqlParameters map with Int64/UInt64.
//! Graph text declares each nonnumeric argument's canonical kind before binding;
//! undeclared property parameters keep the original Int64 contract. Declared
//! Scalar(kind) accepts that exact kind or canonical null, never an implicit
//! numeric/text conversion. Null stays an ordinary null comparison operand,
//! not an instruction to remove the predicate or change it into IS NULL.
//!
//! Strings, Boolean, decimal, float, timestamp, bytes and scalar Int/null use
//! the existing CanonicalScalar domain and bounded ScalarPredicate authority.
//! with_text is explicitly UCS_BASIC; with_scalar accepts constructed canonical
//! values. Quotes and $name inside a parameter need no escaping and never enter
//! the lexer. One parameter can supply multiple occurrences/scopes. Operands
//! and their checked encoding are shared across immutable plans by Arc.
//!
//! Legacy PreparedGqlTemplate semantics and numeric argument transcript bytes
//! are unchanged. GqlParameterValue is now Clone rather than Copy in this
//! unreleased API; get() preserves its owned return shape with a cheap scalar
//! clone. Scalar declarations do not widen numeric HAVING or pagination. This
//! is not catalog/authorization invalidation, a lease, parameter transport,
//! pattern evidence, full GQL, or byte/lifetime/spill resource governance.
//!
//! This example and this continuation's fourteen new Rust tests are UNRUN in
//! the connector environment without Cargo/rustc. Source checks are not a build.

use asupersync::{Budget, CancelKind, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameterType, GqlParameters, GqlQueryError, GqlQueryPolicy,
    GraphPatternTextErrorKind, GraphSymbol, GraphSymbolKind,
    PreparedGraphAggregateText, PreparedGraphText};
use fgdb_types::{CanonicalScalar, CanonicalScalarKind, DatabaseSecurityNamespaceId,
    EId, PurposeContexts, VId};

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(1);
const ACTIVE: PropertyKeyId = PropertyKeyId(1);
const CATEGORY: PropertyKeyId = PropertyKeyId(2);
const FINGERPRINT: PropertyKeyId = PropertyKeyId(3);
const WANTED: &str = "O'Reilly 🦀 '$enabled'";
const HEAD: &str = "MATCH (p:Person) WHERE p.active=$enabled \
    OPTIONAL MATCH (p)-[:R]->(c) WHERE c.category=$category AND c.fingerprint=$fingerprint";
const TYPES: [(&str, GqlParameterType); 3] = [
    ("enabled", GqlParameterType::Scalar(CanonicalScalarKind::Bool)),
    ("category", GqlParameterType::Scalar(CanonicalScalarKind::Text)),
    ("fingerprint", GqlParameterType::Scalar(CanonicalScalarKind::Bytes)),
];
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Property, "active") => Some(GraphSymbol::Property(ACTIVE)),
        (GraphSymbolKind::Property, "category") => Some(GraphSymbol::Property(CATEGORY)),
        (GraphSymbolKind::Property, "fingerprint") => Some(GraphSymbol::Property(FINGERPRINT)),
        _ => None,
    }
}
fn arguments(category: Option<&str>) -> Result<GqlParameters, Box<dyn core::error::Error + Send + Sync>> {
    let arguments = GqlParameters::new().with_bool("enabled", true)?
        .with_scalar("fingerprint", CanonicalScalar::bytes(vec![0, 255, 42])?)?
        .with_uint64("take", 100)?;
    Ok(match category {
        Some(category) => arguments.with_text("category", category)?,
        None => arguments.with_null("category")?,
    })
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
    let commit = contexts.commit(); let query_cx = contexts.query(); let txn_cx = contexts.txn();
    runtime.block_on(async move {
        // Fixture keys belong only to this transient demonstration database.
        let keys = DatabaseKeys::new([0xf1; 32], DatabaseSecurityNamespaceId([0xf2; 32]), [0xf3; 32]);
        let mut db = Database::open_memory(&commit, keys).await?;
        let mut seed = WriteBatch::new(R);
        for id in 0..3 {
            seed.create_vertex(VId(id), vec![PERSON], vec![(ACTIVE, CanonicalScalar::Bool(true))]);
        }
        for (id, category) in [(10, WANTED), (11, "other")] {
            seed.create_vertex(VId(id), vec![], vec![
                (CATEGORY, CanonicalScalar::ucs_basic_text(category)?),
                (FINGERPRINT, CanonicalScalar::bytes(vec![0, 255, 42])?),
            ]);
        }
        seed.add_edge(EId(1), VId(0), VId(10), vec![]);
        seed.add_edge(EId(2), VId(0), VId(10), vec![]);
        seed.add_edge(EId(3), VId(1), VId(11), vec![]);
        let basis = db.write(&commit, seed).await?;
        let view = db.read_session()?;
        let template = PreparedGraphText::prepare_with_parameter_types(
            &format!("{HEAD} RETURN p,c,c.category AS category LIMIT $take"), &TYPES, symbols,
        )?;
        let args = arguments(Some(WANTED))?;
        let pattern = template.bind_parameters(&args)?;
        let frozen = pattern.canonical_bytes();
        let policy = GqlQueryPolicy::new(100, 100, 1_000_000, 100_000);
        let before = db.execute_graph_pattern_governed(&query_cx, &pattern, policy)?;
        assert_eq!(before.value.len(), 4);
        assert_eq!(before.value[0], before.value[1]);
        assert_eq!(before.value[0].get(1).and_then(|value| value.as_vertex()), Some(VId(10)));
        assert!(before.value[2].get(1).is_some_and(|value| value.is_null()));
        assert!(before.value[3].get(1).is_some_and(|value| value.is_null()));
        let exact = GqlQueryPolicy::new(before.rows.snapshot_records, before.rows.result_rows,
            before.evaluator.work_units, before.evaluator.scratch_entries);
        assert_eq!(db.execute_graph_pattern_governed(&query_cx, &pattern, exact)?, before);

        let other = template.bind_parameters(&arguments(Some("other"))?)?;
        let other_rows = db.execute_graph_pattern_governed(&query_cx, &other, policy)?;
        assert_eq!(other_rows.value.len(), 3);
        assert_eq!(other_rows.value[1].get(1).and_then(|value| value.as_vertex()), Some(VId(11)));
        let null = template.bind_parameters(&arguments(None)?)?;
        let null_rows = db.execute_graph_pattern_governed(&query_cx, &null, policy)?;
        assert_eq!(null_rows.value.len(), 3);
        assert!(null_rows.value.iter().all(|row| row.get(1).is_some_and(|value| value.is_null())));
        assert_eq!(pattern.canonical_bytes(), frozen);
        let wrong = GqlParameters::new().with_bool("enabled", true)?.with_bool("category", true)?
            .with_scalar("fingerprint", CanonicalScalar::bytes(vec![0, 255, 42])?)?.with_uint64("take", 100)?;
        assert!(matches!(template.bind_parameters(&wrong).unwrap_err().kind,
            GraphPatternTextErrorKind::ParameterTypeMismatch { .. }));

        let summary_text = PreparedGraphAggregateText::prepare_with_parameter_types(&format!(
            "{HEAD} RETURN p,COUNT(*) AS occurrences,COUNT(c) AS present \
             GROUP BY p HAVING occurrences >= $minimum ORDER BY present DESC,p ASC LIMIT $take"
        ), &TYPES, symbols)?;
        let summary = summary_text.bind_parameters(&args.clone().with_int64("minimum", 1)?)?;
        let old_summary = db.execute_graph_aggregate_governed(&query_cx, &summary, policy)?;
        assert_eq!(old_summary.value.len(), 3);
        assert_eq!(old_summary.value[0].get(1).and_then(|value| value.as_count()), Some(2));
        assert_eq!(old_summary.value[1].get(1).and_then(|value| value.as_count()), Some(0));

        let mut txn = db.begin(&txn_cx)?;
        let mut changes = WriteBatch::new(R);
        changes.delete_edge(EId(1));
        changes.ensure_edge_by_triple(EId(999), VId(0), VId(10), vec![]);
        changes.set_vertex_property(VId(10), FINGERPRINT, None);
        changes.set_vertex_property(VId(11), CATEGORY, Some(CanonicalScalar::ucs_basic_text(WANTED)?));
        txn.write(&mut db, changes)?;
        assert!(txn.edge(&db, EId(999))?.is_none());
        let staged = txn.execute_graph_pattern_governed(&db, &query_cx, &pattern, policy)?;
        assert_eq!(staged.value.len(), 3);
        assert!(staged.value[0].get(1).is_some_and(|value| value.is_null()));
        assert_eq!(staged.value[1].get(1).and_then(|value| value.as_vertex()), Some(VId(11)));
        let staged_summary = txn.execute_graph_aggregate_governed(&db, &query_cx, &summary, policy)?;
        assert_eq!(staged_summary.value[0].keys()[0].as_vertex(), Some(VId(1)));
        assert_eq!(db.execute_graph_pattern_governed(&query_cx, &pattern, policy)?.value, before.value);
        txn.commit(&mut db, &commit).await?;
        assert_eq!(db.execute_graph_pattern_governed(&query_cx, &pattern, policy)?.value, staged.value);
        assert_eq!(db.execute_graph_pattern_governed_at(&query_cx, &pattern, basis, policy)?.value, before.value);
        assert_eq!(view.execute_graph_pattern_governed(&query_cx, &pattern, policy)?.value, before.value);
        assert_eq!(db.execute_graph_aggregate_governed(&query_cx, &summary, policy)?.value, staged_summary.value);
        assert_eq!(pattern.canonical_bytes(), frozen);
        root.cancel_with(CancelKind::User, Some("scalar-parameter demonstration complete"));
        assert!(matches!(db.execute_graph_pattern_governed(&query_cx, &pattern, policy), Err(GqlQueryError::Interrupted(_))));
        println!("OK: declared scalar arguments, safe data binding, nulls, bags, summaries, staging, history and cancellation");
        Ok(())
    })
}
