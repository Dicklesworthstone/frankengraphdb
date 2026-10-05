//! The source index and a post-WITH filter must retain exactly the same rows.
//! Uses real native preparation, snapshot admission, MemVfs commit and reopen.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphSetText};
use fgdb_types::{CanonicalF64, CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts, VId};

const P: PropertyKeyId = PropertyKeyId(1);
const ID: PropertyKeyId = PropertyKeyId(2);
const LABEL: LabelId = LabelId(1);
const R: RelationId = RelationId(1);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x91; 32], DatabaseSecurityNamespaceId([0x92; 32]), [0x93; 32])
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LABEL)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "id") => Some(GraphSymbol::Property(ID)),
        _ => None,
    }
}
fn float(value: f64) -> CanonicalScalar {
    CanonicalScalar::Float(CanonicalF64::new(value))
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(20_000, 20_000, 2_000_000, 2_000_000)
}
fn batch(values: Vec<CanonicalScalar>) -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    for (at, value) in values.into_iter().enumerate() {
        let id = at as i64 + 1;
        batch.create_vertex(VId(id as u128), vec![LABEL],
            vec![(P, value), (ID, CanonicalScalar::Int(id))]);
    }
    batch
}
fn prepare(text: &str) -> fgdb_gql::PreparedGraphSet {
    PreparedGraphSetText::prepare(text, symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap()
}

#[test]
fn source_and_with_numeric_filters_agree_for_all_six_operators_after_reopen() {
    let ((), report) = run_async_under_lab(0x514e_5152_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        db.write(&commit, batch(vec![CanonicalScalar::Int(99), float(99.5),
            CanonicalScalar::Int(100), float(100.0), float(100.5),
            CanonicalScalar::Int(101), CanonicalScalar::Null])).await.unwrap();
        let before = db.frontier().unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        for literal in ["100", "100.0"] {
            for (operator, expected) in [("=", 2), ("<>", 4), ("<", 2), ("<=", 4), (">", 2), (">=", 4)] {
                let source_text = format!(
                    "MATCH (n:L) WHERE n.p {operator} {literal} RETURN n.id AS id ORDER BY id");
                let with_text = format!(
                    "MATCH (n:L) WITH n.id AS id, n.p AS p WHERE p {operator} {literal} RETURN id ORDER BY id");
                let source = db.execute_graph_set_governed(&query, &prepare(&source_text), policy()).unwrap();
                let later = db.execute_graph_set_governed(&query, &prepare(&with_text), policy()).unwrap();
                assert_eq!(source.value.len(), expected, "{source_text}");
                assert_eq!(source.value, later.value, "{source_text} versus {with_text}");
            }
        }
        for (source_filter, later_filter, expected) in [
            ("n.p >= 100 AND n.p < 101.0", "p >= 100 AND p < 101.0", 3),
            ("n.p > 100.0 AND n.p < 100", "p > 100.0 AND p < 100", 0),
        ] {
            let source = prepare(&format!(
                "MATCH (n:L) WHERE {source_filter} RETURN n.id AS id ORDER BY id"));
            let later = prepare(&format!(
                "MATCH (n:L) WITH n.id AS id, n.p AS p WHERE {later_filter} RETURN id ORDER BY id"));
            let source = db.execute_graph_set_governed(&query, &source, policy()).unwrap();
            let later = db.execute_graph_set_governed(&query, &later, policy()).unwrap();
            assert_eq!(source.value.len(), expected);
            assert_eq!(source.value, later.value);
        }
        assert_eq!(db.frontier().unwrap(), before, "reads must not publish commits");
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn indexed_numeric_equality_does_not_round_and_pinned_reads_keep_their_generation() {
    let ((), report) = run_async_under_lab(0x514e_5152_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, batch(vec![CanonicalScalar::Int(9_007_199_254_740_993),
            float(9_007_199_254_740_992.0), float(9_007_199_254_740_994.0),
            CanonicalScalar::Int(i64::MAX), float(9_223_372_036_854_775_808.0)])).await.unwrap();
        let old = db.frontier().unwrap();
        let pinned = db.read_session().unwrap();
        for (literal, expected) in [("9007199254740993", 1), ("9007199254740992.0", 1),
            ("9223372036854775807", 1), ("9223372036854775808.0", 1)] {
            let source = prepare(&format!(
                "MATCH (n:L) WHERE n.p = {literal} RETURN n.id AS id ORDER BY id"));
            let later = prepare(&format!(
                "MATCH (n:L) WITH n.id AS id, n.p AS p WHERE p = {literal} RETURN id ORDER BY id"));
            let source = db.execute_graph_set_governed(&query, &source, policy()).unwrap();
            let later = db.execute_graph_set_governed(&query, &later, policy()).unwrap();
            assert_eq!(source.value.len(), expected, "literal={literal}");
            assert_eq!(source.value, later.value);
        }
        let mut extra = WriteBatch::new(R);
        extra.create_vertex(VId(6), vec![LABEL], vec![(P, CanonicalScalar::Int(9_007_199_254_740_992)),
            (ID, CanonicalScalar::Int(6))]);
        db.write(&commit, extra).await.unwrap();
        let lookup = prepare("MATCH (n:L) WHERE n.p = 9007199254740992.0 RETURN n.id AS id ORDER BY id");
        let current = db.execute_graph_set_governed(&query, &lookup, policy()).unwrap();
        let historical = db.execute_graph_set_governed_at(&query, &lookup, old, policy()).unwrap();
        let pinned_result = pinned.execute_graph_set_governed(&query, &lookup, policy()).unwrap();
        assert_eq!(current.value.len(), 2, "both integer and float matches survive the index");
        assert_eq!(historical.value.len(), 1);
        assert_eq!(pinned_result.value, historical.value);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
