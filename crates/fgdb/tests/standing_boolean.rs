//! Boolean standing WHERE must match ordinary GLA after actual committed writes.
//! Expected results read the public snapshot, never retained standing inputs.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, MemVfs, StandingQueryError, StandingQueryFailure,
    StandingQueryHandle, WriteBatch,
};
use fgdb_delta_types::{LimbLimit, PropertyKeyId, RelationId, ZSet, ZWeight};
use fgdb_gql::algebra::{
    GlaDirection, GraphBooleanExpression, GraphBooleanOp as Op, GraphBooleanOperand as Arg,
    GraphColumn, GraphMatchClause, GraphPatternBuilder, GraphValue, IntegerComparison,
    ScalarPredicate,
};
use fgdb_gql::{
    GqlQueryPolicy, GraphAggregate, GraphAggregateRow, GraphIntegerExpression,
    GraphIntegerOp, PreparedGraphAggregate,
};
use fgdb_types::{
    CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId,
};
use std::collections::BTreeMap;

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const AMOUNT: PropertyKeyId = PropertyKeyId(1);
const P: PropertyKeyId = PropertyKeyId(2);
const FLAG: PropertyKeyId = PropertyKeyId(3);
const GATE: PropertyKeyId = PropertyKeyId(4);
const NAME: PropertyKeyId = PropertyKeyId(5);

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([1; 32], DatabaseSecurityNamespaceId([2; 32]), [3; 32])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
    Vertex,
    OneHop,
    MultiHop,
    Optional,
    Exists,
    NotExists,
}

type View = (Shape, GlaDirection, PreparedGraphAggregate, StandingQueryHandle);

fn predicate(endpoint: &str, unary: bool) -> GraphBooleanExpression {
    let scalar = GraphIntegerExpression::prepare_scalar(&[GraphIntegerOp::ScalarColumn(0)])
        .unwrap();
    let columns = [Arg::Property { variable: endpoint, key: FLAG }];
    let literal = CanonicalScalar::Int(7);
    GraphBooleanExpression::prepare(&[
        Op::Compare {
            left: Arg::Property { variable: "a", key: P },
            comparison: IntegerComparison::Equal,
            right: if unary {
                Arg::Literal(&literal)
            } else {
                Arg::Property { variable: endpoint, key: P }
            },
        },
        Op::Not,
        Op::Expression { expression: &scalar, columns: &columns },
        Op::Or,
    ])
    .unwrap()
}

fn definition(shape: Shape, direction: GlaDirection) -> PreparedGraphAggregate {
    let disabled = CanonicalScalar::Bool(false);
    let enabled = GraphBooleanExpression::prepare(&[
        Op::Compare {
            left: Arg::Property { variable: "a", key: GATE },
            comparison: IntegerComparison::Equal,
            right: Arg::Literal(&disabled),
        },
        Op::Not,
    ])
    .unwrap();
    let mut root = GraphPatternBuilder::new();
    root.vertex("a").unwrap().filter_boolean(&enabled).unwrap();
    let scoped = matches!(shape, Shape::Optional | Shape::Exists | Shape::NotExists);
    let endpoint = if shape == Shape::MultiHop { "c" } else { "b" };
    let output = if matches!(shape, Shape::Vertex | Shape::Exists | Shape::NotExists) {
        "a"
    } else {
        endpoint
    };
    let columns = [
        GraphColumn::vertex("group", "a"),
        GraphColumn::property("amount", output, AMOUNT),
        GraphColumn::vertex("target", output),
    ];
    let input = if scoped {
        let mut child = GraphPatternBuilder::new();
        child.vertex("a").unwrap().vertex("b").unwrap();
        child.edge("a", R, direction, "b").unwrap();
        child.filter_boolean(&predicate("b", false)).unwrap();
        let clause = match shape {
            Shape::Optional => GraphMatchClause::optional(&child),
            Shape::Exists => GraphMatchClause::exists(&child),
            _ => GraphMatchClause::not_exists(&child),
        };
        root.prepare_values_with_clauses(&[clause], &columns, 0, None).unwrap()
    } else {
        if shape != Shape::Vertex {
            root.vertex("b").unwrap();
            root.edge("a", R, direction, "b").unwrap();
            if shape == Shape::MultiHop {
                root.vertex("c").unwrap();
                root.edge("b", S, direction, "c").unwrap();
            }
        }
        root.filter_boolean(&predicate(output, shape == Shape::Vertex)).unwrap();
        root.prepare_values(&columns, 0, None).unwrap()
    }
    .with_duplicates();
    PreparedGraphAggregate::prepare(
        input,
        &[0],
        &[
            GraphAggregate::min("minimum", 1),
            GraphAggregate::count_rows("rows"),
            GraphAggregate::count("nonnull", 1),
            GraphAggregate::count_distinct("targets", 2),
            GraphAggregate::sum_int("sum", 1),
            GraphAggregate::average_int("average", 1),
        ],
        0,
        None,
    )
    .unwrap()
}

fn oracle(db: &Database<MemVfs>, definition: &PreparedGraphAggregate) -> ZSet<GraphAggregateRow> {
    let vertices = db.vertices().unwrap();
    let edges = db.edges().unwrap();
    let rows: BTreeMap<_, _> = vertices.iter().map(|row| (row.vid, row)).collect();
    let result = definition.execute_governed(
        vertices.len() as u64 + edges.len() as u64,
        vertices.iter().map(|row| row.vid),
        edges.iter().map(|row| (row.entry.src, row.entry.relation, row.entry.dst)),
        |vid, predicates| {
            let row = rows.get(&vid).unwrap();
            Ok::<_, ()>(predicates.iter().all(|predicate| {
                predicate.matches_borrowed(
                    row.labels.iter().copied(),
                    row.props.iter().map(|(key, value)| (*key, value)),
                )
            }))
        },
        |vid, key| {
            Ok::<_, ()>(rows.get(&vid).unwrap().props.iter()
                .find_map(|(actual, value)| (*actual == key).then_some(value)))
        },
        policy(),
        || Ok::<_, ()>(()),
    )
    .unwrap();
    ZSet::from_updates(
        result.value.into_iter().map(|row| (row, ZWeight::ONE)),
        LimbLimit::new(4),
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap()
}

fn verify(db: &Database<MemVfs>, cx: &QueryCx, views: &[View]) {
    for (shape, direction, definition, handle) in views {
        let view = db.standing_query(cx, handle).unwrap();
        assert_eq!(view.frontier(), db.frontier().unwrap());
        assert_eq!(view.rows(), &oracle(db, definition), "{shape:?}/{direction:?}");
    }
}

#[test]
fn boolean_views_follow_hidden_changes_across_all_admitted_topologies() {
    let ((), report) = run_async_under_lab(0x6a41, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut views = Vec::new();
        for shape in [Shape::Vertex, Shape::OneHop, Shape::MultiHop, Shape::Optional,
            Shape::Exists, Shape::NotExists]
        {
            for direction in [GlaDirection::Forward, GlaDirection::Reverse, GlaDirection::Undirected] {
                if shape == Shape::Vertex && direction != GlaDirection::Forward {
                    continue;
                }
                let definition = definition(shape, direction);
                let handle = db.register_standing_query(&cx, definition.clone(), policy()).unwrap();
                views.push((shape, direction, definition, handle));
            }
        }
        assert_eq!(views.len(), 16);
        verify(&db, &cx, &views);
        let mut seed = WriteBatch::new(R);
        for (at, value) in [None, Some(CanonicalScalar::Null), Some(CanonicalScalar::Bool(true)),
            Some(CanonicalScalar::Int(7)), Some(CanonicalScalar::Int(8))].into_iter().enumerate()
        {
            let id = at as u128 + 1;
            let mut props = vec![
                (AMOUNT, CanonicalScalar::Int(id as i64 * 10)),
                (FLAG, CanonicalScalar::Bool(id == 3)),
                (GATE, CanonicalScalar::Bool(true)),
            ];
            if let Some(value) = value { props.push((P, value)); }
            seed.create_vertex(VId(id), vec![], props);
        }
        for (eid, src, dst) in [(1,1,2), (2,1,2), (3,2,2), (4,2,3), (5,3,4), (6,4,5), (7,5,1)] {
            seed.add_edge(EId(eid), VId(src), VId(dst), vec![]);
        }
        db.write(&commit, seed).await.unwrap();
        verify(&db, &cx, &views);
        let mut second = WriteBatch::new(S);
        for (eid, src, dst) in [(11,2,4), (12,2,4), (13,3,5), (14,4,1), (15,5,2)] {
            second.add_edge(EId(eid), VId(src), VId(dst), vec![]);
        }
        db.write(&commit, second).await.unwrap();
        verify(&db, &cx, &views);
        // Exercise both snapshot registration and subsequent incremental ticks.
        let late = definition(Shape::MultiHop, GlaDirection::Forward);
        let handle = db.register_standing_query(&cx, late.clone(), policy()).unwrap();
        views.push((Shape::MultiHop, GlaDirection::Forward, late, handle));
        verify(&db, &cx, &views);

        let mut hidden = WriteBatch::new(R);
        hidden.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(8)));
        hidden.set_vertex_property(VId(1), FLAG, Some(CanonicalScalar::Bool(true)));
        hidden.set_vertex_property(VId(3), FLAG, Some(CanonicalScalar::Bool(false)));
        hidden.set_vertex_property(VId(5), GATE, Some(CanonicalScalar::Bool(false)));
        let mut unknown = WriteBatch::new(R);
        unknown.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(7)));
        unknown.set_vertex_property(VId(2), P, Some(CanonicalScalar::Bool(true)));
        unknown.set_vertex_property(VId(3), FLAG, Some(CanonicalScalar::Null));
        unknown.set_vertex_property(VId(5), GATE, Some(CanonicalScalar::Bool(true)));
        unknown.set_vertex_property(VId(4), FLAG, Some(CanonicalScalar::Int(1)));
        let mut missing = WriteBatch::new(R);
        missing.set_vertex_property(VId(2), P, None);
        missing.set_vertex_property(VId(2), FLAG, Some(CanonicalScalar::Bool(true)));
        missing.set_vertex_property(VId(3), GATE, Some(CanonicalScalar::Null));
        for write in [hidden, unknown, missing] {
            db.write(&commit, write).await.unwrap();
            verify(&db, &cx, &views);
        }
        let mut amount = WriteBatch::new(R);
        amount.set_vertex_property(VId(4), AMOUNT, Some(CanonicalScalar::Int(99)));
        db.write(&commit, amount).await.unwrap();
        verify(&db, &cx, &views);
        let mut r = WriteBatch::new(R);
        r.delete_edge(EId(1));
        r.add_edge(EId(8), VId(1), VId(5), vec![]);
        let mut s = WriteBatch::new(S);
        s.delete_edge(EId(11));
        s.add_edge(EId(16), VId(2), VId(1), vec![]);
        db.write_atomic(&commit, vec![s, r]).await.unwrap();
        verify(&db, &cx, &views);
        let mut cascade = WriteBatch::new(R);
        cascade.delete_vertex(VId(2));
        db.write(&commit, cascade).await.unwrap();
        verify(&db, &cx, &views);
        let mut irrelevant = WriteBatch::new(R);
        irrelevant.set_vertex_property(VId(4), PropertyKeyId(99), Some(CanonicalScalar::Int(100)));
        db.write(&commit, irrelevant).await.unwrap();
        verify(&db, &cx, &views);
        for (_, _, _, handle) in &views {
            let view = db.standing_query(&cx, handle).unwrap();
            assert_eq!(view.last_maintenance().affected_vertices, 0);
            assert_eq!(view.last_maintenance().affected_edges, 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn global_count(filter: &GraphBooleanExpression) -> PreparedGraphAggregate {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("a").unwrap().filter_boolean(filter).unwrap();
    let input = builder.prepare_values(&[GraphColumn::vertex("id", "a")], 0, None)
        .unwrap().with_duplicates();
    PreparedGraphAggregate::prepare(input, &[], &[GraphAggregate::count_rows("count")], 0, None)
        .unwrap()
}

fn count(db: &Database<MemVfs>, cx: &QueryCx, handle: &StandingQueryHandle) -> u64 {
    let view = db.standing_query(cx, handle).unwrap();
    assert_eq!(view.rows().len(), 1);
    let (row, weight) = view.rows().iter().next().unwrap();
    assert_eq!(weight, &ZWeight::ONE);
    row.get(0).unwrap().as_count().unwrap()
}

#[test]
fn not_keeps_unknown_and_nested_text_programs_track_hidden_properties() {
    let ((), report) = run_async_under_lab(0x6a42, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let seven = CanonicalScalar::Int(7);
        let not = global_count(&GraphBooleanExpression::prepare(&[
            Op::Compare {
                left: Arg::Property { variable: "a", key: P },
                comparison: IntegerComparison::Equal,
                right: Arg::Literal(&seven),
            },
            Op::Not,
        ]).unwrap());
        let prefix = ScalarPredicate::new(CanonicalScalar::ucs_basic_text("AL").unwrap(),
            IntegerComparison::Equal).unwrap();
        let scalar = GraphIntegerExpression::prepare_scalar(&[
            GraphIntegerOp::ScalarColumn(0), GraphIntegerOp::Upper,
            GraphIntegerOp::Scalar(prefix), GraphIntegerOp::StartsWith,
        ]).unwrap();
        let columns = [Arg::Property { variable: "a", key: NAME }];
        let text = global_count(&GraphBooleanExpression::prepare(&[
            Op::Expression { expression: &scalar, columns: &columns },
        ]).unwrap());
        let not_handle = db.register_standing_query(&cx, not.clone(), policy()).unwrap();
        let text_handle = db.register_standing_query(&cx, text.clone(), policy()).unwrap();
        let mut seed = WriteBatch::new(R);
        for (at, value) in [None, Some(CanonicalScalar::Null), Some(CanonicalScalar::Bool(true)),
            Some(CanonicalScalar::Int(7)), Some(CanonicalScalar::Int(8)), Some(CanonicalScalar::Int(9))]
            .into_iter().enumerate()
        {
            let mut props = Vec::new();
            if let Some(value) = value { props.push((P, value)); }
            match at {
                0 => props.push((NAME, CanonicalScalar::ucs_basic_text("alice").unwrap())),
                1 => props.push((NAME, CanonicalScalar::ucs_basic_text("Bob").unwrap())),
                2 => props.push((NAME, CanonicalScalar::Null)),
                4 => props.push((NAME, CanonicalScalar::Int(8))),
                5 => props.push((NAME, CanonicalScalar::ucs_basic_text("AL").unwrap())),
                _ => {}
            }
            seed.create_vertex(VId(at as u128 + 1), vec![], props);
        }
        db.write(&commit, seed).await.unwrap();
        // Missing, stored null and a unlike scalar kind remain UNKNOWN under
        // NOT; none is admitted merely because the comparison was not TRUE.
        assert_eq!(count(&db, &cx, &not_handle), 2);
        assert_eq!(count(&db, &cx, &text_handle), 2);
        let mut update = WriteBatch::new(R);
        update.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(6)));
        update.set_vertex_property(VId(2), NAME, Some(CanonicalScalar::ucs_basic_text("albert").unwrap()));
        db.write(&commit, update).await.unwrap();
        assert_eq!(count(&db, &cx, &not_handle), 3);
        assert_eq!(count(&db, &cx, &text_handle), 3);
        let mut update = WriteBatch::new(R);
        update.set_vertex_property(VId(1), NAME, Some(CanonicalScalar::ucs_basic_text("Élie").unwrap()));
        update.set_vertex_property(VId(6), NAME, Some(CanonicalScalar::Null));
        update.set_vertex_property(VId(5), P, None);
        db.write(&commit, update).await.unwrap();
        assert_eq!(count(&db, &cx, &not_handle), 2);
        assert_eq!(count(&db, &cx, &text_handle), 1);
        assert_eq!(db.standing_query(&cx, &not_handle).unwrap().rows(), &oracle(&db, &not));
        assert_eq!(db.standing_query(&cx, &text_handle).unwrap().rows(), &oracle(&db, &text));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn boolean_witness_replacement_preserves_optional_semi_and_anti_boundaries() {
    let ((), report) = run_async_under_lab(0x6a43, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut views = Vec::new();
        for shape in [Shape::Optional, Shape::Exists, Shape::NotExists] {
            let definition = definition(shape, GlaDirection::Forward);
            let handle = db.register_standing_query(&cx, definition.clone(), policy()).unwrap();
            views.push((shape, GlaDirection::Forward, definition, handle));
        }
        let mut seed = WriteBatch::new(R);
        for id in 1..=3 {
            seed.create_vertex(VId(id), vec![], vec![
                (AMOUNT, CanonicalScalar::Int(id as i64 * 10)), (P, CanonicalScalar::Int(7)),
                (FLAG, CanonicalScalar::Bool(id == 2)), (GATE, CanonicalScalar::Bool(true)),
            ]);
        }
        for (eid, dst) in [(1,2), (2,2), (3,3)] {
            seed.add_edge(EId(eid), VId(1), VId(dst), vec![]);
        }
        db.write(&commit, seed).await.unwrap();
        verify(&db, &cx, &views);
        let mut replace = WriteBatch::new(R);
        replace.set_vertex_property(VId(2), FLAG, Some(CanonicalScalar::Bool(false)));
        replace.set_vertex_property(VId(3), FLAG, Some(CanonicalScalar::Bool(true)));
        db.write(&commit, replace).await.unwrap();
        verify(&db, &cx, &views);
        let optional = db.standing_query(&cx, &views[0].3).unwrap();
        let (row, weight) = optional.rows().iter()
            .find(|(row, _)| row.keys() == [GraphValue::Vertex(VId(1))]).unwrap();
        assert_eq!(weight, &ZWeight::ONE);
        assert_eq!(row.get(1).unwrap().as_count(), Some(1));
        assert_eq!(row.get(2).unwrap().as_count(), Some(1));
        assert_eq!(row.get(4).unwrap().as_integer(), Some(30));
        let mut last_witness = WriteBatch::new(R);
        last_witness.set_vertex_property(VId(3), FLAG, Some(CanonicalScalar::Null));
        db.write(&commit, last_witness).await.unwrap();
        verify(&db, &cx, &views);
        let optional = db.standing_query(&cx, &views[0].3).unwrap();
        let (row, _) = optional.rows().iter()
            .find(|(row, _)| row.keys() == [GraphValue::Vertex(VId(1))]).unwrap();
        assert_eq!(row.get(1).unwrap().as_count(), Some(1));
        assert_eq!(row.get(2).unwrap().as_count(), Some(0));
        assert!(row.get(0).unwrap().is_null());
        assert!(row.get(4).unwrap().is_null());
        let mut root_change = WriteBatch::new(R);
        root_change.set_vertex_property(VId(1), GATE, Some(CanonicalScalar::Bool(false)));
        db.write(&commit, root_change).await.unwrap();
        verify(&db, &cx, &views);
        for (_, _, _, handle) in &views {
            assert!(db.standing_query(&cx, handle).unwrap().rows().iter()
                .all(|(row, _)| row.keys() != [GraphValue::Vertex(VId(1))]));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn boolean_budget_refusal_does_not_undo_writes_and_rebuild_resumes_maintenance() {
    let ((), report) = run_async_under_lab(0x6a44, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for id in 1..=2 {
            seed.create_vertex(VId(id), vec![], vec![
                (AMOUNT, CanonicalScalar::Int(id as i64)), (P, CanonicalScalar::Int(7)),
                (FLAG, CanonicalScalar::Bool(false)), (GATE, CanonicalScalar::Bool(true)),
            ]);
        }
        let basis = db.write(&commit, seed).await.unwrap();
        let definition = definition(Shape::Vertex, GlaDirection::Forward);
        let bounded = GqlQueryPolicy::new(100_000, 1, 10_000_000, 10_000_000);
        let handle = db.register_standing_query(&cx, definition.clone(), bounded).unwrap();
        assert!(db.standing_query(&cx, &handle).unwrap().rows().is_empty());
        let mut update = WriteBatch::new(R);
        update.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(8)));
        update.set_vertex_property(VId(2), FLAG, Some(CanonicalScalar::Bool(true)));
        let at = db.write(&commit, update).await.unwrap();
        assert_eq!(db.frontier().unwrap(), at);
        assert_eq!(oracle(&db, &definition).len(), 2);
        assert!(matches!(db.standing_query(&cx, &handle),
            Err(StandingQueryError::Unavailable { frontier, reason: StandingQueryFailure::ResultBudget })
                if frontier == basis));
        assert!(matches!(db.rebuild_standing_query(&cx, &handle, bounded),
            Err(StandingQueryError::Maintenance(StandingQueryFailure::ResultBudget))));
        assert!(matches!(db.standing_query(&cx, &handle),
            Err(StandingQueryError::Unavailable { frontier, reason: StandingQueryFailure::ResultBudget })
                if frontier == basis));
        assert_eq!(db.rebuild_standing_query(&cx, &handle, policy()).unwrap(), at);
        assert_eq!(db.standing_query(&cx, &handle).unwrap().rows(), &oracle(&db, &definition));
        let mut resumed = WriteBatch::new(R);
        resumed.set_vertex_property(VId(1), P, Some(CanonicalScalar::Null));
        let at = db.write(&commit, resumed).await.unwrap();
        let view = db.standing_query(&cx, &handle).unwrap();
        assert_eq!(view.frontier(), at);
        assert_eq!(view.rows().len(), 1);
        assert_eq!(view.rows(), &oracle(&db, &definition));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
