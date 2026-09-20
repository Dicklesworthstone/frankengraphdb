//! Native set syntax compiles to ordinary maintained row/set nodes, not reruns.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, PreparedNativeRead, QueryResult, QueryValue,
    StandingQueryError, StandingQueryFailure, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind};
use fgdb_gql::algebra::GraphValue;
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts, VId};
use std::collections::{BTreeMap, BTreeSet};

const P: PropertyKeyId = PropertyKeyId(1);
const OPS: [&str; 6] = ["UNION ALL", "UNION", "INTERSECT ALL", "INTERSECT", "EXCEPT ALL", "EXCEPT"];
fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000) }
fn keys() -> DatabaseKeys { DatabaseKeys::new([0x51; 32], DatabaseSecurityNamespaceId([0x52; 32]), [0x53; 32]) }
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Label, "R") => Some(GraphSymbol::Label(LabelId(2))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn query(op: &str) -> String {
    format!("MATCH (n:L) RETURN n.p AS p {op} MATCH (n:R) RETURN n.p AS other")
}
fn oracle(db: &Database<MemVfs>, op: usize) -> QueryResult {
    let mut left = BTreeMap::new(); let mut right = BTreeMap::new();
    for row in db.vertices_at(db.frontier().unwrap()).unwrap() {
        let value = GraphValue::Scalar(row.props.iter().find(|(key, _)| *key == P)
            .map_or(CanonicalScalar::Null, |(_, value)| value.clone()));
        for (label, bag) in [(LabelId(1), &mut left), (LabelId(2), &mut right)] {
            if row.labels.contains(&label) { *bag.entry(value.clone()).or_insert(0_u64) += 1; }
        }
    }
    let keys = left.keys().chain(right.keys()).collect::<BTreeSet<_>>();
    let mut rows = Vec::new();
    for key in keys {
        let l = left.get(key).copied().unwrap_or(0); let r = right.get(key).copied().unwrap_or(0);
        let count = match op {
            0 => l + r, 1 => u64::from(l > 0 || r > 0), 2 => l.min(r),
            3 => u64::from(l > 0 && r > 0), 4 => l.saturating_sub(r),
            5 => u64::from(l > 0 && r == 0), _ => unreachable!(),
        };
        for _ in 0..count { rows.push(vec![QueryValue::Value(key.clone())]); }
    }
    QueryResult::Rows { columns: vec!["p".into()], rows }
}

#[test]
fn all_six_native_set_laws_and_nested_grouping_follow_whole_commits() {
    let ((), report) = run_async_under_lab(0x6e73_7201, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let params = GqlParameters::new();
        let handles: Vec<_> = OPS.iter().map(|op|
            db.register_standing_native(&cx, &query(op), &params, symbols, policy()).unwrap()).collect();
        let nested = format!("({}) EXCEPT ALL ({})", query(OPS[0]), query(OPS[2]));
        let nested_handle = db.register_standing_native(&cx, &nested, &params, symbols, policy()).unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for (id, label, value) in [(1, 1, Some(4)), (2, 1, Some(4)), (3, 2, Some(4)),
            (4, 2, Some(7)), (5, 1, None), (6, 2, None)] {
            seed.create_vertex(VId(id), vec![LabelId(label)], value.map(|v| (P, CanonicalScalar::Int(v))).into_iter().collect());
        }
        let mut edit = WriteBatch::new(RelationId(1));
        edit.delete_vertex(VId(1)); edit.set_vertex_property(VId(4), P, Some(CanonicalScalar::Int(4)));
        let mut labels = WriteBatch::new(RelationId(1));
        labels.set_vertex_label(VId(2), LabelId(1), false); labels.set_vertex_label(VId(2), LabelId(2), true);
        for batch in [seed, edit, labels] {
            db.write(&commit, batch).await.unwrap();
            for (at, handle) in handles.iter().enumerate() {
                let (frontier, actual) = db.standing_native_query(&cx, handle, policy()).unwrap();
                assert_eq!(frontier, db.frontier().unwrap());
                assert_eq!(actual, oracle(&db, at), "{}", OPS[at]);
                assert_eq!(actual, db.query(&cx, &query(OPS[at]), &params, symbols, policy()).unwrap());
                assert_eq!(db.standing_native_columns(&cx, handle).unwrap(), &["p"]);
            }
            assert_eq!(db.standing_native_query(&cx, &nested_handle, policy()).unwrap().1,
                db.query(&cx, &nested, &params, symbols, policy()).unwrap());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn one_root_handle_repairs_failed_hidden_operands_and_preserves_independent_circuits() {
    let ((), report) = run_async_under_lab(0x6e73_7202, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let params = GqlParameters::new(); let text = query("INTERSECT ALL");
        let small = GqlQueryPolicy::new(100_000, 1, 10_000_000, 10_000_000);
        let bounded = db.register_standing_native(&cx, &text, &params, symbols, small).unwrap();
        let healthy = db.register_standing_native(&cx, &text, &params, symbols, policy()).unwrap();
        let mut first = WriteBatch::new(RelationId(1));
        first.create_vertex(VId(1), vec![LabelId(1), LabelId(2)], vec![(P, CanonicalScalar::Int(4))]);
        db.write(&commit, first).await.unwrap();
        let mut second = WriteBatch::new(RelationId(1));
        second.create_vertex(VId(2), vec![LabelId(1)], vec![(P, CanonicalScalar::Int(7))]);
        db.write(&commit, second).await.unwrap();
        assert!(matches!(db.standing_native_query(&cx, &bounded, policy()),
            Err(StandingQueryError::Unavailable { reason: StandingQueryFailure::DependencyUnavailable, .. })));
        let expected = db.standing_native_query(&cx, &healthy, policy()).unwrap();
        assert!(db.rebuild_standing_query(&cx, &bounded, small).is_err());
        assert!(matches!(db.standing_native_query(&cx, &bounded, policy()), Err(StandingQueryError::Unavailable { .. })));
        assert_eq!(db.standing_native_query(&cx, &healthy, policy()).unwrap(), expected);
        db.rebuild_standing_query(&cx, &bounded, policy()).unwrap();
        assert_eq!(db.standing_native_query(&cx, &bounded, policy()).unwrap(), expected);
        let mut tick = WriteBatch::new(RelationId(1));
        tick.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(4)));
        db.write(&commit, tick).await.unwrap();
        assert_eq!(db.standing_native_query(&cx, &bounded, policy()).unwrap(),
            db.standing_native_query(&cx, &healthy, policy()).unwrap());
        let other = Database::open_memory(&commit, keys()).await.unwrap();
        assert!(matches!(other.standing_native_query(&cx, &bounded, policy()), Err(StandingQueryError::ForeignHandle)));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn parameters_freeze_per_circuit_and_unsupported_final_pages_refuse_without_fallback() {
    let ((), report) = run_async_under_lab(0x6e73_7203, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let text = "MATCH (n:L) WHERE n.p > $floor RETURN n.p AS p UNION ALL MATCH (n:R) RETURN n.p AS p";
        let low = GqlParameters::new().with_int64("floor", 3).unwrap();
        let high = GqlParameters::new().with_int64("floor", 8).unwrap();
        let template = PreparedNativeRead::prepare(text, &low, symbols).unwrap();
        let a = template.register_standing(&mut db, &cx, &low, policy()).unwrap();
        let b = template.register_standing(&mut db, &cx, &high, policy()).unwrap();
        assert!(matches!(template.register_standing(&mut db, &cx, &GqlParameters::new(), policy()),
            Err(StandingQueryError::NativePrepare(_))));
        drop(template);
        let mut seed = WriteBatch::new(RelationId(1));
        seed.create_vertex(VId(1), vec![LabelId(1)], vec![(P, CanonicalScalar::Int(4))]);
        db.write(&commit, seed).await.unwrap();
        assert_eq!(db.standing_native_query(&cx, &a, policy()).unwrap().1,
            db.query(&cx, text, &low, symbols, policy()).unwrap());
        assert_eq!(db.standing_native_query(&cx, &b, policy()).unwrap().1,
            db.query(&cx, text, &high, symbols, policy()).unwrap());
        assert_ne!(db.standing_native_query(&cx, &a, policy()).unwrap().1,
            db.standing_native_query(&cx, &b, policy()).unwrap().1);
        let paged = format!("({}) ORDER BY p DESC LIMIT 0", query("UNION ALL"));
        db.query(&cx, &paged, &GqlParameters::new(), symbols, policy()).unwrap();
        assert!(matches!(db.register_standing_native(&cx, &paged, &GqlParameters::new(), symbols, policy()),
            Err(StandingQueryError::Unsupported)));
        assert!(db.standing_native_query(&cx, &a, policy()).is_ok());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
