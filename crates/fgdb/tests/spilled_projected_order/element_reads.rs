//! Completed aggregate expressions use the resident input's exact-cut reader
//! and original meter, while real capacity-one partitions remain external.

use super::*;
use fgdb::{EmbeddedReadView, QueryResult};
use fgdb_gql::{
    GqlBudgetDimension, GraphAggregateTextSlot, GraphAggregateValue, GraphExactAverage,
    GraphIntegerErrorKind, GraphSymbolResolver, ReverseSymbolCatalog,
};
use fgdb_types::CommitSeq;

const L: LabelId = LabelId(1);
const M: LabelId = LabelId(2);
const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const WIDE_EDGE: EId = EId((1_u128 << 100) | 1);
const VERTEX_RETURN: &str = " RETURN MIN(n).p AS first,MAX(n).p AS last,COUNT(*) AS count,SUM(n.q) AS total,AVG(n.q) AS average,labels(MIN(n)) AS labels,keys(MIN(n)) AS keys,properties(MIN(n)) AS props";
const EDGE_RETURN: &str = " RETURN MIN(r).p AS first,MAX(r).p AS last,COUNT(*) AS count,SUM(r.q) AS total,AVG(r.q) AS average,type(MIN(r)) AS kind,keys(MIN(r)) AS keys,properties(MIN(r)) AS props";
const RANK: &str = "MATCH (a:L)-[r:R]->(b:L) RETURN DISTINCT COUNT(*) AS c GROUP BY a ORDER BY MIN(a).p + COUNT(*) DESC";

struct Catalog;
impl GraphSymbolResolver for Catalog {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        if kind == GraphSymbolKind::Label && name == "M" {
            Some(GraphSymbol::Label(M))
        } else {
            resolve(kind, name)
        }
    }

    fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
        let mut catalog = ReverseSymbolCatalog::new();
        catalog.insert_label(L, "L");
        catalog.insert_label(M, "M");
        catalog.insert_relation(R, "R");
        catalog.insert_property(P, "p");
        catalog.insert_property(Q, "q");
        Some(catalog)
    }
}

fn prepared(text: &str) -> PreparedNativeRead {
    PreparedNativeRead::prepare(text, &GqlParameters::new(), Catalog)
        .unwrap_or_else(|error| panic!("{text}: {error:?}"))
}

async fn fixture(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, keys()).await.unwrap();
    let mut batch = WriteBatch::new(R);
    for (id, p, q) in [(1, 2, i64::MAX), (2, 5, i64::MAX), (u128::MAX, 9, 3)] {
        batch.create_vertex(
            VId(id),
            vec![L],
            vec![(P, CanonicalScalar::Int(p)), (Q, CanonicalScalar::Int(q))],
        );
    }
    for (id, from, to, p, q) in [
        (EId(1), VId(1), VId(2), 7, Some(i64::MAX)),
        (WIDE_EDGE, VId(1), VId(2), 8, Some(i64::MAX)),
        (EId(u128::MAX), VId(2), VId(u128::MAX), 11, Some(3)),
        (EId(4), VId(u128::MAX), VId(1), 13, None),
    ] {
        batch.add_edge(
            id,
            from,
            to,
            vec![
                (P, CanonicalScalar::Int(p)),
                (Q, q.map_or(CanonicalScalar::Null, CanonicalScalar::Int)),
            ],
        );
    }
    assert_eq!(db.write(cx, batch).await.unwrap(), CommitSeq(1));
    db
}

async fn change(db: &mut Database<MemVfs>, cx: &CommitCx) {
    let mut batch = WriteBatch::new(R);
    batch.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(20)));
    batch.set_vertex_label(VId(1), M, true);
    batch.set_edge_property(EId(1), P, Some(CanonicalScalar::Int(70)));
    assert_eq!(db.write(cx, batch).await.unwrap(), CommitSeq(2));
}

fn scalar(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn text(value: &str) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::ucs_basic_text(value).unwrap())
}
fn value(value: GraphValue) -> GraphAggregateValue {
    GraphAggregateValue::Value(value)
}
fn count(value: u64) -> Vec<GraphAggregateValue> {
    vec![GraphAggregateValue::Count(value)]
}

fn expected(changed: bool, edge: bool) -> Vec<Vec<GraphAggregateValue>> {
    let first = match (changed, edge) {
        (false, false) => 2,
        (true, false) => 20,
        (false, true) => 7,
        (true, true) => 70,
    };
    let total = 2 * i128::from(i64::MAX) + 3;
    let metadata = if edge {
        text("R")
    } else {
        GraphValue::List(
            if changed {
                vec![text("L"), text("M")]
            } else {
                vec![text("L")]
            }
            .into_boxed_slice(),
        )
    };
    vec![vec![
        value(scalar(first)),
        value(scalar(if edge { 11 } else { 9 })),
        GraphAggregateValue::Count(if edge { 4 } else { 3 }),
        GraphAggregateValue::Integer(total),
        GraphAggregateValue::Average(GraphExactAverage::new(total, 3).unwrap()),
        value(metadata),
        value(GraphValue::List(vec![text("p"), text("q")].into_boxed_slice())),
        value(
            GraphValue::map(vec![
                ("p".into(), scalar(first)),
                ("q".into(), scalar(i64::MAX)),
            ])
            .unwrap(),
        ),
    ]]
}

#[derive(Debug)]
struct Answer {
    columns: Vec<String>,
    rows: Vec<Vec<GraphAggregateValue>>,
    cut: CommitSeq,
    records: u64,
    results: u64,
    work: u64,
    scratch: u64,
}

async fn external(
    query: &PreparedNativeRead,
    view: &EmbeddedReadView,
    cx: &QueryCx,
    limits: GqlQueryPolicy,
) -> Result<Answer, NativeAggregateSpoolError> {
    let pool = MemoryPool::new(1_000_000, 0).unwrap();
    let mut a = scratch(cx, &pool).await;
    let mut b = scratch(cx, &pool).await;
    let mut c = scratch(cx, &pool).await;
    let result = query
        .spool_aggregate_in_view(
            view,
            cx,
            &GqlParameters::new(),
            limits,
            &mut a,
            &mut b,
            &mut c,
            1,
            256,
            1,
            128,
            257,
            16_384,
            1000,
            100_000_000,
            None,
        )
        .await;
    assert_eq!(pool.used(), 0, "completion or refusal releases every scratch grant");
    let (spool, _) = result?;
    let mut answer = Answer {
        columns: spool.columns().to_vec(),
        rows: Vec::new(),
        cut: spool.snapshot_seq(),
        records: spool.row_stats().snapshot_records,
        results: spool.row_stats().result_rows,
        work: spool.evaluator_stats().work_units,
        scratch: spool.evaluator_stats().scratch_entries,
    };
    {
        let mut reader = spool.reader(&mut c, None);
        while let Some(row) = reader.next_row(cx).await.unwrap() {
            answer.rows.push(
                spool
                    .output_slots()
                    .iter()
                    .map(|slot| match *slot {
                        GraphAggregateTextSlot::GroupKey(at) => {
                            value(row.keys()[at].clone())
                        }
                        GraphAggregateTextSlot::Aggregate(at) => row.values()[at].clone(),
                    })
                    .collect(),
            );
        }
        assert!(reader.next_row(cx).await.unwrap().is_none());
    }
    assert_eq!(spool.row_count(), answer.rows.len() as u64);
    assert_eq!(pool.used(), 0, "result delivery releases decoded rows");
    Ok(answer)
}

fn assert_resident(
    answer: &Answer,
    query: &PreparedNativeRead,
    view: &EmbeddedReadView,
    cx: &QueryCx,
) {
    assert_eq!(
        QueryResult::Rows {
            columns: answer.columns.clone(),
            rows: answer.rows.clone(),
        },
        query
            .execute_in_view(view, cx, &GqlParameters::new(), policy(100))
            .unwrap(),
    );
}

fn assert_record_limit(error: NativeAggregateSpoolError, limit: u64) {
    assert!(
        matches!(&error, NativeAggregateSpoolError::Execute(inner)
            if matches!(inner.as_ref(), GqlQueryError::Rows(exceeded)
                if exceeded.dimension == GqlBudgetDimension::SnapshotRecords
                    && exceeded.limit == limit && exceeded.observed == limit + 1)),
        "{error:?}",
    );
}

#[test]
fn completed_element_reads_keep_exact_cells_metadata_and_the_pinned_historical_cut() {
    let ((), report) = run_async_under_lab(0x5ba1_1030, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = fixture(&commit).await;
        let old = db.read_session().unwrap();
        change(&mut db, &commit).await;
        let current = db.read_session().unwrap();
        drop(db);
        for (view, historical, changed) in [
            (&old, false, false),
            (&current, false, true),
            (&current, true, false),
            (&old, false, false),
        ] {
            for edge in [false, true] {
                let pattern = if edge {
                    "MATCH (a)-[r:R]->(b)"
                } else {
                    "MATCH (n:L)"
                };
                let temporal = if historical {
                    " FOR SYSTEM_TIME AS OF SEQ 1"
                } else {
                    ""
                };
                let suffix = if edge { EDGE_RETURN } else { VERTEX_RETURN };
                let query = prepared(&format!("{pattern}{temporal}{suffix}"));
                let answer = external(&query, view, &cx, policy(1)).await.unwrap();
                assert_eq!(answer.rows, expected(changed, edge));
                assert_eq!(answer.cut, CommitSeq(if changed { 2 } else { 1 }));
                assert_resident(&answer, &query, view, &cx);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn private_property_rank_runs_once_per_group_before_distinct_and_pagination() {
    let ((), report) = run_async_under_lab(0x5ba1_1031, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = fixture(&commit).await;
        let old = db.read_session().unwrap();
        let plain = prepared(
            "MATCH (a:L)-[r:R]->(b:L) RETURN DISTINCT COUNT(*) AS c GROUP BY a ORDER BY MIN(a) DESC",
        );
        let baseline = external(&plain, &old, &cx, policy(2)).await.unwrap();
        let query = prepared(RANK);
        // Computed element ranking still needs an explicit resident reader;
        // the reader-free async compiler keeps its existing static refusal.
        assert!(query.prepare_buffered_aggregate(&GqlParameters::new()).is_err());
        let before = external(&query, &old, &cx, policy(2)).await.unwrap();
        assert_eq!(before.columns, vec!["c"]);
        assert_eq!(before.rows, vec![count(1), count(2)]);
        assert_eq!(before.records, baseline.records + 3);
        assert_resident(&before, &query, &old, &cx);
        let page = prepared(&format!("{RANK} SKIP 1 LIMIT 1"));
        let paged = external(&page, &old, &cx, policy(1)).await.unwrap();
        assert_eq!(paged.rows, vec![count(2)]);
        assert_eq!(paged.records, before.records);
        assert_resident(&paged, &page, &old, &cx);

        change(&mut db, &commit).await;
        let current = db.read_session().unwrap();
        let after = external(&query, &current, &cx, policy(2)).await.unwrap();
        assert_eq!(after.rows, vec![count(2), count(1)]);
        assert_resident(&after, &query, &current, &cx);
        let repeated = external(&query, &old, &cx, policy(2)).await.unwrap();
        assert_eq!(repeated.rows, before.rows);
        assert_eq!(repeated.records, before.records);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn completed_reads_share_all_counters_and_preserve_null_lazy_and_late_error_semantics() {
    let ((), report) = run_async_under_lab(0x5ba1_1032, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let db = fixture(&commit).await;
        let view = db.read_session().unwrap();
        let plain = prepared(
            "MATCH (n:L) RETURN MIN(n) AS first,MAX(n) AS last,COUNT(*) AS count,SUM(n.q) AS total,AVG(n.q) AS average",
        );
        let baseline = external(&plain, &view, &cx, policy(1)).await.unwrap();
        let query = prepared(&format!("MATCH (n:L){VERTEX_RETURN}"));
        let complete = external(&query, &view, &cx, policy(1)).await.unwrap();
        assert_eq!(complete.rows, expected(false, false));
        assert_eq!(complete.records, baseline.records + 5);
        let exact = GqlQueryPolicy::new(
            complete.records,
            complete.results,
            complete.work,
            complete.scratch,
        );
        let repeated = external(&query, &view, &cx, exact).await.unwrap();
        assert_eq!(repeated.rows, complete.rows);
        assert_eq!(
            (repeated.records, repeated.results, repeated.work, repeated.scratch),
            (complete.records, complete.results, complete.work, complete.scratch),
        );
        for limits in [
            GqlQueryPolicy::new(complete.records - 1, u64::MAX, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, complete.results - 1, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, u64::MAX, complete.work - 1, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, complete.scratch - 1),
        ] {
            assert!(external(&query, &view, &cx, limits).await.is_err());
        }
        let hidden = prepared(&format!("MATCH (n:L){VERTEX_RETURN} LIMIT 0"));
        let hidden_answer = external(&hidden, &view, &cx, policy(0)).await.unwrap();
        assert!(hidden_answer.rows.is_empty());
        assert_eq!(hidden_answer.records, complete.records);
        assert_record_limit(
            external(
                &hidden,
                &view,
                &cx,
                GqlQueryPolicy::new(complete.records - 1, 0, u64::MAX, u64::MAX),
            )
            .await
            .unwrap_err(),
            complete.records - 1,
        );

        let lazy = prepared(
            "MATCH (n:L) RETURN CASE WHEN COUNT(*)=3 THEN [7] ELSE [MIN(n).p,1/0] END AS value,COUNT(*) AS count",
        );
        let skipped = external(&lazy, &view, &cx, policy(1)).await.unwrap();
        assert_eq!(
            skipped.rows,
            vec![vec![
                value(GraphValue::List(vec![scalar(7)].into_boxed_slice())),
                GraphAggregateValue::Count(3),
            ]],
        );
        assert_eq!(skipped.records, baseline.records);
        assert_resident(&skipped, &lazy, &view, &cx);
        for window in ["", " LIMIT 0"] {
            let failing = prepared(&format!(
                "MATCH (n:L) RETURN 1/(MIN(n).p-2) AS broken{window}"
            ));
            assert_record_limit(
                external(
                    &failing,
                    &view,
                    &cx,
                    GqlQueryPolicy::new(baseline.records, 100, u64::MAX, u64::MAX),
                )
                .await
                .unwrap_err(),
                baseline.records,
            );
            let error = external(&failing, &view, &cx, policy(100))
                .await
                .unwrap_err();
            assert!(
                matches!(&error, NativeAggregateSpoolError::Execute(inner)
                    if matches!(inner.as_ref(), GqlQueryError::Source(
                        GraphAggregateError::OutputExpression { error, .. }
                    ) if error.kind == GraphIntegerErrorKind::DivisionByZero)),
                "{error:?}",
            );
        }

        // MIN/MAX over an empty source yield NULL. Even metadata receivers
        // then short-circuit without spending a logical snapshot-record grant.
        let empty = Database::open_memory(&commit, keys()).await.unwrap();
        let empty_view = empty.read_session().unwrap();
        let answer = external(
            &query,
            &empty_view,
            &cx,
            GqlQueryPolicy::new(0, 1, u64::MAX, u64::MAX),
        )
        .await
        .unwrap();
        let null = || value(GraphValue::Scalar(CanonicalScalar::Null));
        assert_eq!(
            answer.rows,
            vec![vec![
                null(), null(), GraphAggregateValue::Count(0), null(),
                null(), null(), null(), null(),
            ]],
        );
        assert_eq!(answer.records, 0);
        assert_resident(&answer, &query, &empty_view, &cx);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
