//! Binding-dependent standing filters retain hidden properties on both sides.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, StandingQueryHandle, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId, ZWeight};
use fgdb_gql::algebra::{GlaDirection, GraphColumn, GraphPatternBuilder, IntegerComparison};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphAggregate, GraphSymbol, GraphSymbolKind,
    PreparedGraphAggregate, PreparedGraphAggregateText,
};
use fgdb_types::{
    CanonicalF64, CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId,
};
use std::cmp::Ordering;
use std::collections::BTreeMap;

const L: PropertyKeyId = PropertyKeyId(11);
const R: PropertyKeyId = PropertyKeyId(12);
const REL: RelationId = RelationId(1);
const OPS: [IntegerComparison; 6] = [
    IntegerComparison::Equal,
    IntegerComparison::NotEqual,
    IntegerComparison::Less,
    IntegerComparison::LessOrEqual,
    IntegerComparison::Greater,
    IntegerComparison::GreaterOrEqual,
];
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x81; 32],
        DatabaseSecurityNamespaceId([0x82; 32]),
        [0x83; 32],
    )
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 10_000, 10_000_000, 10_000_000)
}
fn definition(edge: bool, comparison: IntegerComparison) -> PreparedGraphAggregate {
    let mut pattern = GraphPatternBuilder::new();
    pattern.vertex("a").unwrap();
    if edge {
        pattern.vertex("b").unwrap();
        pattern.edge("a", REL, GlaDirection::Forward, "b").unwrap();
    }
    pattern
        .compare_properties("a", L, comparison, if edge { "b" } else { "a" }, R)
        .unwrap();
    // Neither filter operand is a returned/input aggregate property column.
    let input = pattern
        .prepare_values(&[GraphColumn::vertex("a", "a")], 0, None)
        .unwrap()
        .with_duplicates();
    PreparedGraphAggregate::prepare(input, &[], &[GraphAggregate::count_rows("count")], 0, None)
        .unwrap()
}

fn storage_count(db: &Database<MemVfs>, edge: bool, operator: usize) -> u64 {
    let vertices = db.vertices().unwrap();
    let rows: BTreeMap<_, _> = vertices.iter().map(|v| (v.vid, v)).collect();
    let pairs = if edge {
        db.edges()
            .unwrap()
            .into_iter()
            .filter(|e| e.entry.relation == REL)
            .map(|e| (e.entry.src, e.entry.dst))
            .collect::<Vec<_>>()
    } else {
        vertices.iter().map(|v| (v.vid, v.vid)).collect()
    };
    pairs
        .into_iter()
        .filter(|(a, b)| {
            let left = rows[a]
                .props
                .iter()
                .find(|(key, _)| *key == L)
                .map(|(_, value)| value);
            let right = rows[b]
                .props
                .iter()
                .find(|(key, _)| *key == R)
                .map(|(_, value)| value);
            let (Some(a), Some(b)) = (left, right) else {
                return false;
            };
            if matches!(a, CanonicalScalar::Null)
                || matches!(b, CanonicalScalar::Null)
                || core::mem::discriminant(a) != core::mem::discriminant(b)
            {
                return false;
            }
            // Canonical value ordering is shared data semantics. This oracle does
            // not call the query predicate or the standing input/affected-set code.
            let order = a.cmp(b);
            match operator {
                0 => order == Ordering::Equal,
                1 => order != Ordering::Equal,
                2 => order == Ordering::Less,
                3 => order != Ordering::Greater,
                4 => order == Ordering::Greater,
                5 => order != Ordering::Less,
                _ => unreachable!(),
            }
        })
        .count() as u64
}

fn assert_count(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    handle: &StandingQueryHandle,
    query: &PreparedGraphAggregate,
    count: u64,
) {
    let view = db.standing_query(cx, handle).unwrap();
    assert_eq!(view.frontier(), db.frontier().unwrap());
    assert_eq!(view.rows().len(), 1);
    let (row, weight) = view.rows().iter().next().unwrap();
    assert_eq!(weight, &ZWeight::ONE);
    assert_eq!(row.get(0).unwrap().as_count(), Some(count));
    let full = db
        .execute_graph_aggregate_governed(cx, query, policy())
        .unwrap();
    assert_eq!(full.value.len(), 1);
    assert_eq!(&full.value[0], row);
}

#[test]
fn every_scalar_pair_and_comparison_matches_public_storage_for_one_and_two_slots() {
    let ((), report) = run_async_under_lab(0x7e11, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(REL);
        seed.create_vertex(VId(1), vec![], vec![]);
        seed.create_vertex(VId(2), vec![], vec![]);
        for (id, src, dst) in [(1, 1, 2), (2, 1, 2), (3, 2, 1), (4, 1, 1)] {
            seed.add_edge(EId(id), VId(src), VId(dst), vec![]);
        }
        db.write(&commit, seed).await.unwrap();
        let mut queries = Vec::new();
        for edge in [false, true] {
            for (op, comparison) in OPS.into_iter().enumerate() {
                let query = definition(edge, comparison);
                let handle = db
                    .register_standing_query(&cx, query.clone(), policy())
                    .unwrap();
                queries.push((query, handle, edge, op));
            }
        }
        let values = [
            None,
            Some(CanonicalScalar::Null),
            Some(CanonicalScalar::Bool(false)),
            Some(CanonicalScalar::Bool(true)),
            Some(CanonicalScalar::Int(i64::MIN)),
            Some(CanonicalScalar::Int(0)),
            Some(CanonicalScalar::Int(i64::MAX)),
            Some(CanonicalScalar::Float(CanonicalF64::new(-0.0))),
            Some(CanonicalScalar::Float(CanonicalF64::new(f64::NAN))),
            Some(CanonicalScalar::ucs_basic_text("O'Brien 🦀").unwrap()),
            Some(CanonicalScalar::ucs_basic_text("different").unwrap()),
            Some(CanonicalScalar::bytes(vec![0, 255]).unwrap()),
        ];
        let mut ordinal = 0;
        for left in &values {
            for right in &values {
                ordinal += 1;
                let mut change = WriteBatch::new(REL);
                change.set_vertex_property(VId(1), L, left.clone());
                change.set_vertex_property(VId(1), R, right.clone());
                change.set_vertex_property(VId(2), R, right.clone());
                // Guarantee a real committed tick even when both are absent.
                change.set_vertex_property(
                    VId(1),
                    PropertyKeyId(99),
                    Some(CanonicalScalar::Int(ordinal)),
                );
                db.write(&commit, change).await.unwrap();
                for (query, handle, edge, op) in &queries {
                    assert_count(&db, &cx, handle, query, storage_count(&db, *edge, *op));
                }
            }
        }
        db.compact(&commit).await.unwrap();
        for (query, handle, edge, op) in &queries {
            assert_count(&db, &cx, handle, query, storage_count(&db, *edge, *op));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn hidden_text_operands_advance_even_when_the_result_delta_is_empty() {
    let ((), report) = run_async_under_lab(0x7e12, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let old = CanonicalScalar::ucs_basic_text(&"old-value".repeat(512)).unwrap();
        let new = CanonicalScalar::ucs_basic_text(&"secret-new".repeat(512)).unwrap();
        let mut batch = WriteBatch::new(REL);
        batch.create_vertex(VId(1), vec![], vec![(L, old.clone())]);
        batch.create_vertex(VId(2), vec![], vec![(R, old.clone())]);
        batch.add_edge(EId(1), VId(1), VId(2), vec![]);
        batch.add_edge(EId(2), VId(1), VId(2), vec![]);
        db.write(&commit, batch).await.unwrap();
        let text = "MATCH (a)-[:R]->(b) WHERE a.left=b.right RETURN COUNT(*) AS hits";
        let query = PreparedGraphAggregateText::prepare(text, |kind, name| match (kind, name) {
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(REL)),
            (GraphSymbolKind::Property, "left") => Some(GraphSymbol::Property(L)),
            (GraphSymbolKind::Property, "right") => Some(GraphSymbol::Property(R)),
            _ => None,
        })
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let handle = db
            .register_standing_query(&cx, query.clone(), policy())
            .unwrap();
        assert_count(&db, &cx, &handle, &query, 2);
        let mut same_result = WriteBatch::new(REL);
        same_result.set_vertex_property(VId(1), L, Some(new.clone()));
        same_result.set_vertex_property(VId(2), R, Some(new.clone()));
        db.write(&commit, same_result).await.unwrap();
        assert_count(&db, &cx, &handle, &query, 2);
        let view = db.standing_query(&cx, &handle).unwrap();
        assert_eq!(view.last_maintenance().affected_vertices, 2);
        assert_eq!(view.last_maintenance().affected_edges, 2);
        // Two parallel edges compare both old and new borrowed text payloads.
        // Pin the payload work without depending on incidental map overhead.
        assert!(view.last_maintenance().work_units >= 2 * (4608_u64 / 64 + 5120 / 64));
        assert!(!format!("{view:?}").contains("secret-new"));
        for (right, expected) in [(Some(old), 0), (Some(new), 2), (None, 0)] {
            let mut change = WriteBatch::new(REL);
            change.set_vertex_property(VId(2), R, right);
            db.write(&commit, change).await.unwrap();
            assert_count(&db, &cx, &handle, &query, expected);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
