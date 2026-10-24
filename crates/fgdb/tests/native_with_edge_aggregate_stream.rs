//! Complete WITH input semantics through native edge-aggregate pull dispatch.
use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, MemVfs, NativeAggregateCursor, PreparedNativeRead, QueryResult,
    WriteBatch,
};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::edge_stream::aggregate::EdgeAggregatePlan;
use fgdb_gql::scan_stream::ScanKind;
use fgdb_gql::stream::VertexScanState;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateError, GraphAggregateTextSlot,
    GraphAggregateValue, GraphExactAverage, PreparedGraphPipelineAggregateText, RelationBind,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use std::collections::{BTreeMap, BTreeSet};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const START: VId = VId(1_u128 << 100);
const END: VId = VId(u128::MAX);
type Edge = (u128, VId, VId, Option<i64>);
fn fixture() -> Vec<Edge> {
    vec![
        (10, START, VId(1), Some(3)),
        (11, START, VId(1), Some(3)),
        (12, START, VId(2), None),
        (13, VId(1), END, Some(4)),
        (14, VId(2), END, Some(-1)),
    ]
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x31; 32],
        DatabaseSecurityNamespaceId([0x32; 32]),
        [0x33; 32],
    )
}
fn symbols() -> RelationBind {
    RelationBind::new()
        .with_relation("R", R)
        .with_property("p", P)
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn params(floor: i64) -> GqlParameters {
    GqlParameters::new().with_int64("floor", floor).unwrap()
}
fn text(hops: usize) -> String {
    let pattern = if hops == 1 {
        "(a)-[r:R]->(b)"
    } else {
        "(a)-[r:R]->(x)-[s:R]->(b)"
    };
    format!(
        "MATCH {pattern} WITH r.p AS p,b AS owner WHERE p>=$floor \
        WITH p*2 AS value,owner AS owner RETURN COUNT(*) AS n,owner AS owner,SUM(value) AS total,\
        AVG(value) AS average,COUNT(DISTINCT value) AS support GROUP BY owner ORDER BY total DESC"
    )
}
fn seed() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    for vertex in [START, VId(1), VId(2), END] {
        batch.create_vertex(vertex, vec![], vec![]);
    }
    for (id, from, to, value) in fixture() {
        batch.add_edge(
            EId(id),
            from,
            to,
            value
                .map(|p| vec![(P, CanonicalScalar::Int(p))])
                .unwrap_or_default(),
        );
    }
    batch
}
fn drain(cursor: &mut NativeAggregateCursor<'_>) -> Vec<Vec<GraphAggregateValue>> {
    let slots = cursor.output_slots().to_vec();
    cursor
        .by_ref()
        .map(|row| {
            let row = row.unwrap();
            slots
                .iter()
                .map(|slot| match slot {
                    GraphAggregateTextSlot::GroupKey(at) => {
                        GraphAggregateValue::Value(row.keys()[*at].clone())
                    }
                    GraphAggregateTextSlot::Aggregate(at) => row.values()[*at].clone(),
                })
                .collect()
        })
        .collect()
}
// Independent stored-edge occurrence enumeration. DISTINCT below belongs to
// the aggregate argument after both WITH projections, not to source matches.
fn oracle(edges: &[Edge], hops: usize, floor: i64) -> Vec<Vec<GraphAggregateValue>> {
    let mut groups = BTreeMap::<VId, Vec<i64>>::new();
    for &(id, _, to, value) in edges {
        let Some(value) = value.filter(|p| *p >= floor) else {
            continue;
        };
        if hops == 1 {
            groups.entry(to).or_default().push(value * 2);
        } else {
            for &(next, from, end, _) in edges {
                if id != next && to == from {
                    groups.entry(end).or_default().push(value * 2);
                }
            }
        }
    }
    let mut ranked: Vec<_> = groups
        .into_iter()
        .map(|(owner, values)| {
            let total = values.iter().map(|v| i128::from(*v)).sum::<i128>();
            let count = values.len() as u64;
            let support = values.into_iter().collect::<BTreeSet<_>>().len() as u64;
            (owner, total, count, support)
        })
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    ranked
        .into_iter()
        .map(|(owner, total, count, support)| {
            vec![
                GraphAggregateValue::Count(count),
                GraphAggregateValue::Value(GraphValue::Vertex(owner)),
                GraphAggregateValue::Integer(total),
                GraphAggregateValue::Average(GraphExactAverage::new(total, count).unwrap()),
                GraphAggregateValue::Count(support),
            ]
        })
        .collect()
}

#[test]
fn native_with_aliases_and_filters_match_occurrences_and_keep_the_opened_snapshot() {
    let ((), report) = run_async_under_lab(0x5749_5448, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let basis = db.write(&commit, seed()).await.unwrap();
        let pin = db.read_session().unwrap();
        let mut opened = Vec::new();
        for hops in [1, 2] {
            for floor in [-2, 0, 4] {
                let statement = text(hops);
                let args = params(floor);
                let prepared = PreparedNativeRead::prepare(&statement, &args, symbols()).unwrap();
                let QueryResult::Rows { columns, rows } =
                    prepared.execute(&db, &cx, &args, wide()).unwrap()
                else {
                    panic!("read");
                };
                assert_eq!(rows, oracle(&fixture(), hops, floor));
                let stream = prepared.stream_aggregate(&db, &cx, &args, wide()).unwrap();
                fn send(_: &impl Send) {}
                send(&stream);
                assert_eq!(stream.kind(), ScanKind::Edge);
                assert_eq!(stream.columns(), columns);
                assert_eq!(stream.row_stats().snapshot_records, 0);
                opened.push((hops, floor, rows, stream));
            }
        }
        let mut edit = WriteBatch::new(R);
        edit.delete_edge(EId(10));
        edit.set_edge_property(EId(11), P, Some(CanonicalScalar::Int(9)));
        db.write(&commit, edit).await.unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        let changed: Vec<_> = fixture()
            .into_iter()
            .filter(|edge| edge.0 != 10)
            .map(|(id, a, b, p)| (id, a, b, if id == 11 { Some(9) } else { p }))
            .collect();
        for (hops, floor, expected, mut old) in opened {
            assert_eq!(drain(&mut old), expected);
            assert_eq!(old.snapshot_seq(), basis);
            let statement = text(hops);
            let args = params(floor);
            let prepared = PreparedNativeRead::prepare(&statement, &args, symbols()).unwrap();
            let mut current = prepared.stream_aggregate(&db, &cx, &args, wide()).unwrap();
            assert_eq!(drain(&mut current), oracle(&changed, hops, floor));
            let mut pinned = prepared
                .stream_aggregate_in_view(&pin, &cx, &args, wide())
                .unwrap();
            assert_eq!(drain(&mut pinned), expected);
            let mut names = symbols();
            let q = PreparedGraphPipelineAggregateText::prepare(
                &statement,
                |kind: fgdb_gql::GraphSymbolKind, name: &str| {
                    fgdb_gql::GraphSymbolResolver::resolve_symbol(&mut names, kind, name)
                },
            )
            .unwrap()
            .bind_parameters(&args)
            .unwrap();
            let plan = EdgeAggregatePlan::compile(&q).unwrap();
            let mut historical = db
                .stream_global_edge_aggregate_governed_at(&cx, &plan, basis, wide())
                .unwrap();
            let old_rows: Vec<_> = historical.by_ref().collect::<Result<_, _>>().unwrap();
            assert_eq!(old_rows.len(), expected.len());
            for (row, want) in old_rows.iter().zip(&expected) {
                assert_eq!(
                    row.keys(),
                    std::slice::from_ref(match &want[1] {
                        GraphAggregateValue::Value(v) => v,
                        _ => unreachable!(),
                    })
                );
                assert_eq!(
                    row.values(),
                    [
                        want[0].clone(),
                        want[2].clone(),
                        want[3].clone(),
                        want[4].clone()
                    ]
                );
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn pipeline_quotas_late_errors_and_rebinding_never_release_partial_groups() {
    let ((), report) = run_async_under_lab(0x5749_5449, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let statement = text(1);
        let args = params(0);
        let prepared = PreparedNativeRead::prepare(&statement, &args, symbols()).unwrap();
        let mut full = prepared.stream_aggregate(&db, &cx, &args, wide()).unwrap();
        let expected = drain(&mut full);
        let r = full.row_stats();
        let e = full.evaluator_stats();
        let exact = GqlQueryPolicy::new(
            r.snapshot_records,
            r.result_rows,
            e.work_units,
            e.scratch_entries,
        );
        assert_eq!(
            drain(&mut prepared.stream_aggregate(&db, &cx, &args, exact).unwrap()),
            expected
        );
        for limited in [
            GqlQueryPolicy::new(r.snapshot_records - 1, u64::MAX, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, r.result_rows - 1, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, u64::MAX, e.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, e.scratch_entries - 1),
        ] {
            let mut stream = prepared.stream_aggregate(&db, &cx, &args, limited).unwrap();
            assert!(stream.by_ref().collect::<Result<Vec<_>, _>>().is_err());
            assert_eq!(stream.state(), VertexScanState::Failed);
            assert!(stream.next().is_none());
        }
        assert!(
            prepared
                .stream_aggregate(&db, &cx, &GqlParameters::new(), wide())
                .is_err()
        );
        let mut closed = prepared.stream_aggregate(&db, &cx, &args, wide()).unwrap();
        closed.close();
        assert!(closed.next().is_none());
        assert_eq!(closed.row_stats().snapshot_records, 0);
        let mut edit = WriteBatch::new(R);
        edit.add_edge(EId(99), START, END, vec![(P, CanonicalScalar::Int(0))]);
        db.write(&commit, edit).await.unwrap();
        for page in ["", " LIMIT 0"] {
            let bad = format!(
                "MATCH (a)-[r:R]->(b) WITH r.p AS p WHERE p>=0 WITH 12/p AS unused RETURN COUNT(*) AS n{page}"
            );
            let mut stream = db
                .query_aggregate_stream(&cx, &bad, &GqlParameters::new(), symbols(), wide())
                .unwrap();
            assert!(matches!(
                stream.next(),
                Some(Err(GqlQueryError::Source(
                    GraphAggregateError::InputRelation(_)
                )))
            ));
            assert_eq!(stream.row_stats().result_rows, 0);
            assert!(stream.next().is_none());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn thousands_of_with_occurrences_share_one_summary_and_intermediate_pages_still_refuse() {
    let ((), report) = run_async_under_lab(0x5749_544a, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(START, vec![], vec![]);
        batch.create_vertex(END, vec![], vec![]);
        for id in 0..4096 {
            batch.add_edge(
                EId(id),
                START,
                END,
                vec![(P, CanonicalScalar::Int(i64::MAX / 2))],
            );
        }
        db.write(&commit, batch).await.unwrap();
        let statement = "MATCH (a)-[r:R]->(b) WITH r.p AS p WHERE p>0 WITH p*2 AS v RETURN COUNT(*) AS n,SUM(v) AS total,COUNT(DISTINCT v) AS support";
        let mut stream = db
            .query_aggregate_stream(
                &cx,
                statement,
                &GqlParameters::new(),
                symbols(),
                GqlQueryPolicy::new(4096, 1, 10_000_000, 10_000_000),
            )
            .unwrap();
        let row = stream.next().unwrap().unwrap();
        assert_eq!(
            row.values(),
            &[
                GraphAggregateValue::Count(4096),
                GraphAggregateValue::Integer(4096 * i128::from(i64::MAX - 1)),
                GraphAggregateValue::Count(1)
            ]
        );
        assert!(stream.next().is_none());
        assert_eq!(stream.row_stats().result_rows, 1);
        for clause in [
            "DISTINCT r.p AS p",
            "r.p AS p LIMIT 0",
            "r.p AS p ORDER BY p",
        ] {
            let statement = format!("MATCH (a)-[r:R]->(b) WITH {clause} RETURN COUNT(*) AS n");
            assert!(
                db.query_aggregate_stream(
                    &cx,
                    &statement,
                    &GqlParameters::new(),
                    symbols(),
                    wide()
                )
                .is_err()
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn vertex_with_dispatch_rebinds_and_keeps_its_pinned_generation() {
    let ((), report) = run_async_under_lab(0x5749_544b, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let mut batch = WriteBatch::new(R);
        for (id, value) in [
            (START, Some(3)),
            (VId(1), Some(1)),
            (VId(2), None),
            (END, Some(3)),
        ] {
            batch.create_vertex(
                id,
                vec![],
                value
                    .map(|value| vec![(P, CanonicalScalar::Int(value))])
                    .unwrap_or_default(),
            );
        }
        let basis = db.write(&commit, batch).await.unwrap();
        let pin = db.read_session().unwrap();
        let statement = "MATCH (n) WITH n.p AS p WHERE p >= $floor WITH p*2 AS value \
            RETURN value AS bucket,COUNT(*) AS n,SUM(value) AS total GROUP BY value ORDER BY total DESC";
        let args = params(0);
        let prepared = PreparedNativeRead::prepare(statement, &args, symbols()).unwrap();
        let expected_row = |key, count, total| {
            vec![
                GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Int(key))),
                GraphAggregateValue::Count(count),
                GraphAggregateValue::Integer(total),
            ]
        };
        let expected = vec![expected_row(6, 2, 12), expected_row(2, 1, 2)];
        let mut old = prepared.stream_aggregate(&db, &cx, &args, wide()).unwrap();
        assert_eq!(old.kind(), ScanKind::Vertex);
        assert_eq!(old.row_stats().snapshot_records, 0);
        let mut deletion = WriteBatch::new(R);
        deletion.delete_vertex(START);
        db.write(&commit, deletion).await.unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(drain(&mut old), expected);
        assert_eq!(old.snapshot_seq(), basis);
        assert!(old.next().is_none());
        let mut pinned = prepared
            .stream_aggregate_in_view(&pin, &cx, &args, wide())
            .unwrap();
        assert_eq!(drain(&mut pinned), expected);
        let current = vec![expected_row(6, 1, 6), expected_row(2, 1, 2)];
        let mut stream = prepared.stream_aggregate(&db, &cx, &args, wide()).unwrap();
        assert_eq!(drain(&mut stream), current);
        let QueryResult::Rows { columns, rows } =
            prepared.execute(&db, &cx, &args, wide()).unwrap()
        else {
            panic!("read");
        };
        assert_eq!(rows, current);
        assert_eq!(stream.columns(), columns);
        let mut rebound = prepared
            .stream_aggregate(&db, &cx, &params(2), wide())
            .unwrap();
        assert_eq!(drain(&mut rebound), vec![expected_row(6, 1, 6)]);
        let mut empty = prepared
            .stream_aggregate(&db, &cx, &params(4), wide())
            .unwrap();
        assert!(empty.next().is_none());
        assert!(
            prepared
                .stream_aggregate(&db, &cx, &GqlParameters::new(), wide())
                .is_err()
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
