//! Committed grouped maintenance versus independent public storage scans.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, StandingQueryError, StandingQueryFailure, StandingQueryHandle, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, ZWeight};
use fgdb_gql::algebra::{GraphColumn, GraphPatternBuilder, GraphValue, IntegerComparison, VertexPredicate};
use fgdb_gql::{GraphAggregate, GraphAggregateRow, GraphExactAverage, GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphAggregate, PreparedGraphAggregateText};
use fgdb_types::{CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId};
use std::collections::BTreeMap;

const PERSON: LabelId = LabelId(1);
const SELECTED: PropertyKeyId = PropertyKeyId(1);
const TEAM: PropertyKeyId = PropertyKeyId(2);
const SCORE: PropertyKeyId = PropertyKeyId(3);
const UNUSED: PropertyKeyId = PropertyKeyId(4);
const R: RelationId = RelationId(1);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x81; 32], DatabaseSecurityNamespaceId([0x82; 32]), [0x83; 32])
}
fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000) }
fn text(value: &str) -> CanonicalScalar { CanonicalScalar::ucs_basic_text(value).unwrap() }
fn definition(grouped: bool, identity: bool) -> PreparedGraphAggregate {
    let mut input = GraphPatternBuilder::new();
    input.vertex("n").unwrap();
    input.filter("n", VertexPredicate::HasLabel(PERSON)).unwrap();
    input.filter("n", VertexPredicate::IntegerProperty {
        key: SELECTED, comparison: IntegerComparison::GreaterOrEqual, value: 1,
    }).unwrap();
    let key = if identity { GraphColumn::vertex("team", "n") }
        else { GraphColumn::property("team", "n", TEAM) };
    let input = input.prepare_values(&[key, GraphColumn::property("score", "n", SCORE)], 0, None)
        .unwrap().with_duplicates();
    PreparedGraphAggregate::prepare(input, if grouped { &[0] } else { &[] }, &[
        GraphAggregate::count_rows("rows"), GraphAggregate::count("present", 1),
        GraphAggregate::sum_int("total", 1), GraphAggregate::average_int("mean", 1),
    ], 0, None).unwrap()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) {
    let mut batch = WriteBatch::new(R);
    for (id, member, selected, team, score) in [
        (1, true, 1, Some(text("blue")), Some(CanonicalScalar::Int(5))),
        (2, true, 1, Some(text("blue")), None),
        (3, true, 1, Some(text("green")), Some(CanonicalScalar::Int(-3))),
        (4, true, 0, Some(text("green")), Some(CanonicalScalar::Int(99))),
        (5, false, 1, Some(CanonicalScalar::Null), Some(CanonicalScalar::Int(8))),
        (6, true, 1, None, Some(CanonicalScalar::Null)),
    ] {
        let mut props = vec![(SELECTED, CanonicalScalar::Int(selected))];
        if let Some(team) = team { props.push((TEAM, team)); }
        if let Some(score) = score { props.push((SCORE, score)); }
        batch.create_vertex(VId(id), if member { vec![PERSON] } else { vec![] }, props);
    }
    db.write(cx, batch).await.unwrap();
}

type Summary = (Vec<GraphValue>, u64, u64, Option<i128>, Option<(i128, u64)>);
fn summary(row: &GraphAggregateRow) -> Summary {
    (row.keys().to_vec(), row.get(0).unwrap().as_count().unwrap(),
     row.get(1).unwrap().as_count().unwrap(), row.get(2).unwrap().as_integer(),
     row.get(3).unwrap().as_average().map(|v| (v.numerator(), v.denominator())))
}
fn oracle(db: &Database<MemVfs>, grouped: bool, identity: bool) -> Vec<Summary> {
    let mut groups = BTreeMap::<Vec<GraphValue>, Vec<Option<i128>>>::new();
    if !grouped { groups.insert(Vec::new(), Vec::new()); }
    for row in db.vertices().unwrap() {
        let get = |key| row.props.iter().find(|(k, _)| *k == key).map(|(_, v)| v);
        if !row.labels.contains(&PERSON)
            || !matches!(get(SELECTED), Some(CanonicalScalar::Int(value)) if *value >= 1) { continue; }
        let key = if !grouped { Vec::new() } else if identity { vec![GraphValue::Vertex(row.vid)] }
            else { vec![GraphValue::Scalar(get(TEAM).cloned().unwrap_or(CanonicalScalar::Null))] };
        let value = match get(SCORE) {
            None | Some(CanonicalScalar::Null) => None,
            Some(CanonicalScalar::Int(value)) => Some(i128::from(*value)),
            _ => panic!("fixture score domain"),
        };
        groups.entry(key).or_default().push(value);
    }
    groups.into_iter().map(|(key, rows)| {
        let values: Vec<_> = rows.iter().copied().flatten().collect();
        let sum = (!values.is_empty()).then(|| values.iter().sum::<i128>());
        let average = sum.map(|sum| {
            let value = GraphExactAverage::new(sum, values.len() as u64).unwrap();
            (value.numerator(), value.denominator())
        });
        (key, rows.len() as u64, values.len() as u64, sum, average)
    }).collect()
}
fn verify(db: &Database<MemVfs>, cx: &QueryCx, handle: &StandingQueryHandle,
          query: &PreparedGraphAggregate, grouped: bool, identity: bool) {
    let view = db.standing_query(cx, handle).unwrap();
    assert_eq!(view.frontier(), db.frontier().unwrap());
    let maintained: Vec<_> = view.rows().iter().map(|(row, weight)| {
        assert_eq!(weight, &ZWeight::ONE);
        summary(row)
    }).collect();
    assert_eq!(maintained, oracle(db, grouped, identity));
    let mut full = db.execute_graph_aggregate_governed(cx, query, policy()).unwrap().value;
    full.sort();
    assert_eq!(maintained, full.iter().map(summary).collect::<Vec<_>>());
}

#[test]
fn grouped_commits_move_keys_retract_groups_and_keep_null_and_average_semantics() {
    let ((), report) = run_async_under_lab(0x6a01, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root);
        let commit = cx.commit(); let query_cx = cx.query();
        let vfs = MemVfs::new().unwrap(); let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let query = definition(true, false);
        let global = definition(false, false);
        let identity = definition(true, true);
        let grouped_handle = db.register_standing_query(&query_cx, query.clone(), policy()).unwrap();
        let global_handle = db.register_standing_query(&query_cx, global.clone(), policy()).unwrap();
        let identity_handle = db.register_standing_query(&query_cx, identity.clone(), policy()).unwrap();
        assert!(db.standing_query(&query_cx, &grouped_handle).unwrap().rows().is_empty());
        verify(&db, &query_cx, &global_handle, &global, false, false);
        seed(&mut db, &commit).await;
        for step in 0..9 {
            for (handle, plan, grouped, id) in [
                (&grouped_handle, &query, true, false), (&global_handle, &global, false, false),
                (&identity_handle, &identity, true, true),
            ] { verify(&db, &query_cx, handle, plan, grouped, id); }
            let mut batch = WriteBatch::new(R);
            match step {
                0 => { batch.set_vertex_property(VId(1), TEAM, Some(text("green"))); }
                1 => { batch.set_vertex_label(VId(3), PERSON, false); }
                2 => { batch.set_vertex_property(VId(2), SCORE, Some(CanonicalScalar::Int(7))); }
                3 => { batch.set_vertex_property(VId(4), SELECTED, Some(CanonicalScalar::Int(1))); }
                4 => { batch.set_vertex_property(VId(6), TEAM, Some(CanonicalScalar::Null)); }
                5 => { batch.delete_vertex(VId(2)); }
                6 => { batch.set_vertex_label(VId(3), PERSON, true); }
                7 => { batch.set_vertex_property(VId(6), TEAM, None); }
                8 => { for id in [1, 3, 4, 5, 6] { batch.delete_vertex(VId(id)); } }
                _ => unreachable!(),
            }
            db.write(&commit, batch).await.unwrap();
            db.compact(&commit).await.unwrap();
        }
        verify(&db, &query_cx, &grouped_handle, &query, true, false);
        verify(&db, &query_cx, &global_handle, &global, false, false);
        assert!(db.standing_query(&query_cx, &grouped_handle).unwrap().rows().is_empty());
        drop(db);
        let mut db = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert!(matches!(db.standing_query(&query_cx, &grouped_handle), Err(StandingQueryError::ForeignHandle)));
        let handle = db.register_standing_query(&query_cx, query.clone(), policy()).unwrap();
        verify(&db, &query_cx, &handle, &query, true, false);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn parsed_multikey_label_query_and_unfiltered_global_query_are_maintainable() {
    let ((), report) = run_async_under_lab(0x6a02, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let symbols = |kind, name: &str| match (kind, name) {
            (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
            (GraphSymbolKind::Property, "team") => Some(GraphSymbol::Property(TEAM)),
            (GraphSymbolKind::Property, "selected") => Some(GraphSymbol::Property(SELECTED)),
            (GraphSymbolKind::Property, "score") => Some(GraphSymbol::Property(SCORE)),
            _ => None,
        };
        let plans = [
            "MATCH (n:Person) RETURN n.team,n.selected,COUNT(*) AS tally,AVG(n.score) AS mean GROUP BY n.team,n.selected",
            "MATCH (n) RETURN COUNT(*) AS tally,COUNT(n.team) AS named",
            "MATCH (n:Person) WHERE n.team IS NULL RETURN COUNT(*) AS tally,SUM(n.score) AS total",
        ].map(|text| PreparedGraphAggregateText::prepare(text, symbols).unwrap()
            .bind_parameters(&GqlParameters::new()).unwrap());
        let handles = plans.iter().map(|plan| db.register_standing_query(&cx, plan.clone(), policy()).unwrap())
            .collect::<Vec<_>>();
        seed(&mut db, &commit).await;
        for step in 0..3 {
            for (plan, handle) in plans.iter().zip(&handles) {
                let mut full = db.execute_graph_aggregate_governed(&cx, plan, policy()).unwrap().value;
                full.sort();
                let view = db.standing_query(&cx, handle).unwrap();
                assert_eq!(view.rows().iter().map(|(row, _)| row.clone()).collect::<Vec<_>>(), full);
            }
            let mut batch = WriteBatch::new(R);
            batch.set_vertex_property(VId(1), TEAM, if step == 0 { None } else { Some(text("blue")) });
            batch.set_vertex_property(VId(1), SELECTED, Some(CanonicalScalar::Int(step)));
            db.write(&commit, batch).await.unwrap();
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn grouped_result_limit_is_final_state_and_does_not_abort_durable_writes() {
    let ((), report) = run_async_under_lab(0x6a03, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let query = definition(true, false);
        let narrow = GqlQueryPolicy::new(100_000, 1, 10_000_000, 10_000_000);
        let handle = db.register_standing_query(&cx, query.clone(), narrow).unwrap();
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(1), vec![PERSON], vec![
            (SELECTED, CanonicalScalar::Int(1)), (TEAM, text("only")), (SCORE, CanonicalScalar::Int(1)),
        ]);
        db.write(&commit, batch).await.unwrap();
        let mut replacement = WriteBatch::new(R);
        replacement.set_vertex_property(VId(1), TEAM, Some(text("replacement")));
        let before = db.write(&commit, replacement).await.unwrap();
        verify(&db, &cx, &handle, &query, true, false);
        let healthy = db.register_standing_query(&cx, query.clone(), policy()).unwrap();
        let mut growth = WriteBatch::new(R);
        growth.create_vertex(VId(2), vec![PERSON], vec![
            (SELECTED, CanonicalScalar::Int(1)), (TEAM, text("new-group")), (SCORE, CanonicalScalar::Int(2)),
        ]);
        let after = db.write(&commit, growth).await.unwrap();
        assert!(after > before);
        assert!(db.vertex(VId(2)).unwrap().is_some());
        assert!(matches!(db.standing_query(&cx, &handle), Err(StandingQueryError::Unavailable {
            frontier, reason: StandingQueryFailure::ResultBudget,
        }) if frontier == before));
        verify(&db, &cx, &healthy, &query, true, false);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn irrelevant_ticks_do_not_revisit_vertices_or_leak_staged_values() {
    let ((), report) = run_async_under_lab(0x6a04, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let query = definition(true, false);
        let handle = db.register_standing_query(&cx, query.clone(), policy()).unwrap();
        let old = db.standing_query(&cx, &handle).unwrap().rows().iter()
            .map(|(row, _)| row.clone()).collect::<Vec<_>>();
        let mut txn = db.begin(&contexts.txn()).unwrap();
        let mut staged = WriteBatch::new(R);
        staged.set_vertex_property(VId(1), TEAM, Some(text("uncommitted")));
        txn.write(&mut db, staged).unwrap();
        verify(&db, &cx, &handle, &query, true, false);
        assert_eq!(db.standing_query(&cx, &handle).unwrap().rows().iter()
            .map(|(row, _)| row.clone()).collect::<Vec<_>>(), old);
        txn.commit(&mut db, &commit).await.unwrap();
        verify(&db, &cx, &handle, &query, true, false);
        for step in 0..3 {
            let mut batch = WriteBatch::new(R);
            match step {
                0 => { batch.set_vertex_property(VId(1), UNUSED, Some(text("private-payload-8723"))); }
                1 => { batch.add_edge(EId(1), VId(1), VId(2), vec![]); }
                2 => { batch.delete_edge(EId(1)); }
                _ => unreachable!(),
            }
            let at = db.write(&commit, batch).await.unwrap();
            let view = db.standing_query(&cx, &handle).unwrap();
            assert_eq!(view.frontier(), at);
            assert_eq!(view.last_maintenance().affected_vertices, 0);
            assert!(!format!("{view:?}").contains("uncommitted"));
            verify(&db, &cx, &handle, &query, true, false);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn changed_group_work_does_not_scale_with_unrelated_materialized_groups() {
    let ((), report) = run_async_under_lab(0x6a05, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let cx = contexts.query();
        let mut stats = Vec::new();
        for size in [8_u128, 400] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(R);
            for id in 1..=size {
                seed.create_vertex(VId(id), vec![PERSON], vec![
                    (SELECTED, CanonicalScalar::Int(1)), (TEAM, CanonicalScalar::Int(id as i64)),
                    (SCORE, CanonicalScalar::Int(10)),
                ]);
            }
            db.write(&commit, seed).await.unwrap();
            let query = definition(true, false);
            let handle = db.register_standing_query(&cx, query.clone(), policy()).unwrap();
            let mut change = WriteBatch::new(R);
            change.set_vertex_property(VId(1), SCORE, Some(CanonicalScalar::Int(11)));
            db.write(&commit, change).await.unwrap();
            verify(&db, &cx, &handle, &query, true, false);
            stats.push(*db.standing_query(&cx, &handle).unwrap().last_maintenance());
        }
        assert_eq!(stats[0], stats[1]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
