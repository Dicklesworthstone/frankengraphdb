//! Entry-point differentials against the specific native text facades and governed engines.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, MemVfs, QueryError, QueryResult, QueryWriteError, WriteBatch, WriteTxn,
};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::insertion::GraphInsertRequest;
use fgdb_gql::*;
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, EmbeddedTxnCompletion,
    PurposeContexts, QueryCx, TxnCx, VId,
};
use std::collections::BTreeMap;

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x71; 32],
        DatabaseSecurityNamespaceId([0x72; 32]),
        [0x73; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn write_policy() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(policy(), 100, 100, 100)
}
async fn seeded(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, keys()).await.unwrap();
    let mut batch = WriteBatch::new(R);
    for (id, p, q) in [(1, 1, 10), (2, 2, 20), (3, 3, 30), (4, 4, 40)] {
        batch.create_vertex(
            VId(id),
            vec![PERSON],
            vec![(P, CanonicalScalar::Int(p)), (Q, CanonicalScalar::Int(q))],
        );
    }
    batch.add_edge(EId(10), VId(1), VId(2), vec![(Q, CanonicalScalar::Int(50))]);
    batch.add_edge(EId(11), VId(2), VId(3), vec![]);
    db.write(cx, batch).await.unwrap();
    db
}
fn plain(columns: &[String], rows: Vec<GraphValueRow>) -> QueryResult {
    QueryResult::Rows {
        columns: columns.to_vec(),
        rows: rows
            .iter()
            .map(|row| {
                row.values()
                    .iter()
                    .cloned()
                    .map(GraphAggregateValue::Value)
                    .collect()
            })
            .collect(),
    }
}
fn aggregate(
    columns: &[String],
    slots: &[GraphAggregateTextSlot],
    rows: Vec<GraphAggregateRow>,
) -> QueryResult {
    QueryResult::Rows {
        columns: columns.to_vec(),
        rows: rows
            .iter()
            .map(|row| {
                slots
                    .iter()
                    .map(|slot| match *slot {
                        GraphAggregateTextSlot::GroupKey(i) => {
                            GraphAggregateValue::Value(row.keys()[i].clone())
                        }
                        GraphAggregateTextSlot::Aggregate(i) => row.values()[i].clone(),
                    })
                    .collect()
            })
            .collect(),
    }
}
#[derive(Clone, Copy, Debug)]
enum ReadFacade {
    Pattern,
    Aggregate,
    Pipeline,
    Set,
    TemporalPattern,
    TemporalSet,
    TemporalAggregate,
}

// Deliberately no classifier here: each case names its SPECIFIC independent facade.
fn direct_read(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    kind: ReadFacade,
    text: &str,
    args: &GqlParameters,
    temporal_slots: &[GraphAggregateTextSlot],
) -> QueryResult {
    match kind {
        ReadFacade::Pattern => {
            let prepared = PreparedGraphText::prepare(text, symbols).unwrap();
            let bound = prepared.bind_parameters(args).unwrap();
            plain(
                bound.columns(),
                db.execute_graph_pattern_governed(cx, &bound, policy())
                    .unwrap()
                    .value,
            )
        }
        ReadFacade::Aggregate => {
            let prepared = PreparedGraphAggregateText::prepare(text, symbols).unwrap();
            let bound = prepared.bind_parameters(args).unwrap();
            aggregate(
                prepared.columns(),
                prepared.output_slots(),
                db.execute_graph_aggregate_governed(cx, &bound, policy())
                    .unwrap()
                    .value,
            )
        }
        ReadFacade::Pipeline => {
            let prepared = PreparedGraphPipelineAggregateText::prepare(text, symbols).unwrap();
            let bound = prepared.bind_parameters(args).unwrap();
            aggregate(
                prepared.columns(),
                prepared.output_slots(),
                db.execute_graph_aggregate_governed(cx, &bound, policy())
                    .unwrap()
                    .value,
            )
        }
        ReadFacade::Set => {
            let prepared = PreparedGraphSetText::prepare(text, symbols).unwrap();
            let bound = prepared.bind_parameters(args).unwrap();
            plain(
                prepared.columns(),
                db.execute_graph_set_governed(cx, &bound, policy())
                    .unwrap()
                    .value,
            )
        }
        ReadFacade::TemporalPattern => {
            let prepared = PreparedTemporalGraphText::prepare(text, symbols).unwrap();
            let bound = prepared.bind_parameters(args).unwrap();
            plain(
                bound.pattern().columns(),
                db.execute_temporal_graph_text_governed(cx, &bound, policy())
                    .unwrap()
                    .value,
            )
        }
        ReadFacade::TemporalSet => {
            let prepared = PreparedTemporalGraphSetText::prepare(text, symbols).unwrap();
            let bound = prepared.bind_parameters(args).unwrap();
            plain(
                prepared.columns(),
                db.execute_temporal_graph_set_text_governed(cx, &bound, policy())
                    .unwrap()
                    .value,
            )
        }
        ReadFacade::TemporalAggregate => {
            let prepared = PreparedTemporalGraphAggregateText::prepare(text, symbols).unwrap();
            let bound = prepared.bind_parameters(args).unwrap();
            // Fixture-specified RETURN slots, independent of entrypoint alias inference.
            aggregate(
                prepared.columns(),
                temporal_slots,
                db.execute_temporal_graph_aggregate_text_governed(cx, &bound, policy())
                    .unwrap()
                    .value,
            )
        }
    }
}

#[test]
fn pattern_aggregate_pipeline_and_set_each_match_two_specific_facade_queries() {
    let ((), report) = run_async_under_lab(0x7d14_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let db = seeded(&contexts.commit()).await;
        let cx = contexts.query();
        let args = GqlParameters::new();
        for (kind, text) in [
            (
                ReadFacade::Pattern,
                "MATCH (a)-[:R]->(b) RETURN b.p AS score ORDER BY score DESC",
            ),
            (
                ReadFacade::Pattern,
                "MATCH (a)-[:R]->(b)-[:R]->(c) RETURN ALL a,c.p AS score",
            ),
            (
                ReadFacade::Aggregate,
                "MATCH (n) RETURN COUNT(*) AS count,SUM(n.p) AS total,AVG(n.p) AS mean",
            ),
            (
                ReadFacade::Aggregate,
                "MATCH (n) RETURN ABS(n.p) AS bucket,SUM(n.p*n.q) AS total,COUNT(DISTINCT n.p) AS different GROUP BY ABS(n.p) ORDER BY bucket DESC",
            ),
            (
                ReadFacade::Pipeline,
                "MATCH (n) WITH n.p AS x RETURN SUM_INT(x) AS total,AVG_INT(x) AS mean",
            ),
            (
                ReadFacade::Pipeline,
                "MATCH (n) WITH n.p AS x ORDER BY x DESC LIMIT 2 RETURN COUNT(*) AS count,SUM(x) AS total,AVG(x) AS mean",
            ),
            (
                ReadFacade::Set,
                "MATCH (a) WHERE a.p >= 2 RETURN a.p AS p UNION MATCH (b) WHERE b.p <= 3 RETURN b.p AS p ORDER BY p DESC",
            ),
            (
                ReadFacade::Set,
                "MATCH (a) RETURN a.p AS p EXCEPT ALL MATCH (b) WHERE b.p = 2 RETURN b.p AS p ORDER BY p",
            ),
        ] {
            let expected = direct_read(&db, &cx, kind, text, &args, &[]);
            assert_eq!(
                db.query(&cx, text, &args, symbols, policy()).unwrap(),
                expected,
                "{kind:?}: {text}"
            );
        }
        // This assertion kills routing an aggregate to the pattern engine even
        // if the latter happens to accept some overlapping grammar.
        let result = db
            .query(
                &cx,
                "MATCH (n) RETURN COUNT(*) AS count,SUM(n.p) AS total,AVG(n.p) AS mean",
                &args,
                symbols,
                policy(),
            )
            .unwrap();
        let QueryResult::Rows { columns, rows } = result else {
            panic!("aggregate must return rows")
        };
        assert_eq!(columns, ["count", "total", "mean"]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], GraphAggregateValue::Count(4));
        assert_eq!(rows[0][1], GraphAggregateValue::Integer(10));
        let average = rows[0][2].as_average().unwrap();
        assert_eq!((average.numerator(), average.denominator()), (5, 2));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn temporal_pattern_set_and_aggregate_each_match_two_historical_facade_queries() {
    let ((), report) = run_async_under_lab(0x7d14_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = seeded(&commit).await;
        let historical = db.frontier().unwrap();
        let mut change = WriteBatch::new(R);
        change.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(100)));
        change.create_vertex(VId(5), vec![PERSON], vec![(P, CanonicalScalar::Int(5))]);
        db.write(&commit, change).await.unwrap();
        let args = GqlParameters::new()
            .with_uint64("at", historical.0)
            .unwrap();
        use GraphAggregateTextSlot::{Aggregate as A, GroupKey as K};
        let cases: &[(ReadFacade, &str, &[GraphAggregateTextSlot])] = &[
            (
                ReadFacade::TemporalPattern,
                "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at RETURN n.p AS p ORDER BY p",
                &[],
            ),
            (
                ReadFacade::TemporalPattern,
                "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at WHERE n.p >= 2 RETURN n.p AS p ORDER BY p DESC",
                &[],
            ),
            (
                ReadFacade::TemporalSet,
                "MATCH (a) FOR SYSTEM_TIME AS OF SEQ $at WHERE a.p >= 2 RETURN a.p*2 AS p UNION DISTINCT MATCH (b) WHERE b.p <= 3 RETURN b.p*2 AS p ORDER BY p",
                &[],
            ),
            (
                ReadFacade::TemporalSet,
                "MATCH (a) FOR SYSTEM_TIME AS OF SEQ $at WHERE a.p >= 2 RETURN a.p AS p INTERSECT MATCH (b) WHERE b.p <= 3 RETURN b.p AS p EXCEPT MATCH (c) WHERE c.p = 2 RETURN c.p AS p ORDER BY p",
                &[],
            ),
            (
                ReadFacade::TemporalAggregate,
                "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at RETURN COUNT(*) AS c,SUM(n.p*2) AS s",
                &[A(0), A(1)],
            ),
            (
                ReadFacade::TemporalAggregate,
                "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at RETURN ABS(n.p) AS bucket,COUNT(*) AS c GROUP BY ABS(n.p) ORDER BY bucket DESC",
                &[K(0), A(0)],
            ),
            (
                ReadFacade::TemporalAggregate,
                "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at RETURN n.p AS x,n.p AS y,COUNT(*) AS c GROUP BY n.p ORDER BY x",
                &[K(0), K(0), A(0)],
            ),
        ];
        for &(kind, text, slots) in cases {
            assert_eq!(
                db.query(&cx, text, &args, symbols, policy()).unwrap(),
                direct_read(&db, &cx, kind, text, &args, slots),
                "{kind:?}: {text}"
            );
        }
        let historical_count = db.query(&cx, cases[4].1, &args, symbols, policy()).unwrap();
        assert_eq!(
            historical_count,
            QueryResult::Rows {
                columns: vec!["c".into(), "s".into()],
                rows: vec![vec![
                    GraphAggregateValue::Count(4),
                    GraphAggregateValue::Integer(20)
                ]],
            }
        );
        let current = GqlParameters::new()
            .with_uint64("at", db.frontier().unwrap().0)
            .unwrap();
        assert_ne!(
            historical_count,
            db.query(&cx, cases[4].1, &current, symbols, policy())
                .unwrap()
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn budget_refusal_and_future_as_of_keep_native_typed_errors() {
    let ((), report) = run_async_under_lab(0x7d14_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let db = seeded(&contexts.commit()).await;
        let cx = contexts.query();
        let text = "MATCH (n) RETURN n.p AS p";
        let args = GqlParameters::new();
        let bound = PreparedGraphText::prepare(text, symbols)
            .unwrap()
            .bind_parameters(&args)
            .unwrap();
        let budget = GqlQueryPolicy::new(0, 100, 100_000, 100_000);
        let direct = db
            .execute_graph_pattern_governed(&cx, &bound, budget)
            .unwrap_err();
        let entry = db.query(&cx, text, &args, symbols, budget).unwrap_err();
        match (direct, entry) {
            (GqlQueryError::Rows(a), QueryError::Pattern(GqlQueryError::Rows(b))) => {
                assert_eq!(a, b)
            }
            pair => panic!("lost native source-budget refusal: {pair:?}"),
        }
        let text = "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at RETURN n.p AS p";
        let args = GqlParameters::new()
            .with_uint64("at", db.frontier().unwrap().0 + 1)
            .unwrap();
        let bound = PreparedTemporalGraphText::prepare(text, symbols)
            .unwrap()
            .bind_parameters(&args)
            .unwrap();
        let direct = db
            .execute_temporal_graph_text_governed(&cx, &bound, policy())
            .unwrap_err();
        let entry = db.query(&cx, text, &args, symbols, policy()).unwrap_err();
        match (direct, entry) {
            (GqlQueryError::Source(a), QueryError::Pattern(GqlQueryError::Source(b))) => {
                assert_eq!(format!("{a:?}"), format!("{b:?}"))
            }
            pair => panic!("AS OF must refuse its future rather than read live data: {pair:?}"),
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unsupported_diagnostics_are_deterministic_and_resolver_is_cached_across_probes() {
    let ((), report) = run_async_under_lab(0x7d14_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let db = seeded(&contexts.commit()).await;
        let cx = contexts.query();
        for (text, expected_facade) in [
            // The refusal names the facade whose parser got furthest. Since the
            // source-free pipeline aggregates (df505256, 60d2041e), the pipeline
            // facade reads further into `RETURN 1` than the set facade does. The
            // laws below (typed refusal, in-text offset, deterministic diagnostics,
            // cached misses) are unchanged. That the winner moves at all is the
            // parser race tracked by fgdb-crate-layer-drift-phjcs.
            ("EXPLAIN RETURN 1", fgdb::NativeReadClass::PipelineAggregate),
            (
                "MATCH (n:Missing) RETURN n",
                fgdb::NativeReadClass::Aggregate,
            ),
        ] {
            let mut runs = Vec::new();
            for _ in 0..2 {
                let mut calls = BTreeMap::new();
                let result = db.query(
                    &cx,
                    text,
                    &GqlParameters::new(),
                    |kind: GraphSymbolKind, name: &str| {
                        *calls.entry((kind, name.to_owned())).or_insert(0) += 1;
                        symbols(kind, name)
                    },
                    policy(),
                );
                let error = result.unwrap_err();
                let QueryError::Refused { facade, source } = &error else {
                    panic!("{text}: expected typed refusal, got {error:?}")
                };
                assert_eq!(*facade, expected_facade, "{text}");
                let offset = match source.as_ref() {
                    QueryError::SetText(error) => error.offset,
                    QueryError::PatternText(error) => error.offset,
                    QueryError::PipelineText(error) => error.offset,
                    other => panic!("{text}: wrong typed source: {other:?}"),
                };
                assert!(offset > 0 && offset <= text.len(), "{text}: {offset}");
                let diagnostics = error.to_string();
                assert!(
                    calls.values().all(|count| *count == 1),
                    "misses must be cached too"
                );
                runs.push((diagnostics, calls));
            }
            assert_eq!(runs[0], runs[1]);
        }
        let mut calls = BTreeMap::new();
        let text = "MATCH (a:Person) RETURN a.p AS p UNION MATCH (b:Person) RETURN b.p AS p";
        let actual = db
            .query(
                &cx,
                text,
                &GqlParameters::new(),
                |kind: GraphSymbolKind, name: &str| {
                    *calls.entry((kind, name.to_owned())).or_insert(0) += 1;
                    symbols(kind, name)
                },
                policy(),
            )
            .unwrap();
        assert_eq!(
            actual,
            direct_read(&db, &cx, ReadFacade::Set, text, &GqlParameters::new(), &[])
        );
        assert_eq!(
            calls.get(&(GraphSymbolKind::Label, "Person".into())),
            Some(&1)
        );
        assert_eq!(
            calls.get(&(GraphSymbolKind::Property, "p".into())),
            Some(&1)
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[derive(Clone, Copy, Debug)]
enum WriteFacade {
    Mutation,
    Insert,
    Delete,
    VertexMerge,
    VertexUpsert,
    EdgeMerge,
    EdgeUpsert,
}
fn identity(request: GraphInsertRequest) -> Result<ElementId, ()> {
    Ok(match request {
        GraphInsertRequest::Vertex { row, vertex } => {
            ElementId::Vertex(VId(100 + row as u128 * 10 + vertex as u128))
        }
        GraphInsertRequest::Edge { row, edge } => {
            ElementId::Edge(EId(200 + row as u128 * 10 + edge as u128))
        }
    })
}
fn direct_write(
    txn: &mut WriteTxn,
    db: &mut Database<MemVfs>,
    cx: &QueryCx,
    kind: WriteFacade,
    text: &str,
    args: &GqlParameters,
) -> GraphWriteStepReceipt {
    let policy = write_policy();
    match kind {
        WriteFacade::Mutation => {
            let bound = PreparedGraphMutationText::prepare(text, R, symbols)
                .unwrap()
                .bind_parameters(args)
                .unwrap();
            let (_, targets, edges) = txn
                .execute_graph_mutation_returning_governed(db, cx, &bound, policy.mutations)
                .unwrap();
            assert!(edges.is_empty());
            GraphWriteStepReceipt::Mutation { targets, edges }
        }
        WriteFacade::Insert => {
            let bound = PreparedGraphInsertText::prepare(text, R, symbols)
                .unwrap()
                .bind_parameters(args)
                .unwrap();
            let (_, vertices, edges) = txn
                .execute_graph_insert_returning_governed(
                    db,
                    cx,
                    &bound,
                    policy.insertion_policy(),
                    identity,
                )
                .unwrap();
            GraphWriteStepReceipt::Insert { vertices, edges }
        }
        WriteFacade::Delete => {
            let bound = PreparedGraphDeleteText::prepare(text, R, symbols)
                .unwrap()
                .bind_parameters(args)
                .unwrap();
            let (_, targets, edges) = txn
                .execute_graph_delete_elements_returning_governed(
                    db,
                    cx,
                    &bound,
                    policy.deletion_policy(),
                )
                .unwrap();
            GraphWriteStepReceipt::Delete { targets, edges }
        }
        WriteFacade::VertexMerge => {
            let bound = PreparedGraphVertexMergeText::prepare(text, R, symbols)
                .unwrap()
                .bind_parameters(args)
                .unwrap();
            let (_, outcome) = txn
                .execute_graph_vertex_merge_governed(
                    db,
                    cx,
                    &bound,
                    policy.vertex_merge_policy(),
                    identity,
                )
                .unwrap();
            GraphWriteStepReceipt::VertexMerge { outcome }
        }
        WriteFacade::VertexUpsert => {
            let bound = PreparedGraphVertexUpsertText::prepare(text, R, symbols)
                .unwrap()
                .bind_parameters(args)
                .unwrap();
            let (_, outcome) = txn
                .execute_graph_vertex_upsert_governed(
                    db,
                    cx,
                    &bound,
                    policy.vertex_upsert_policy(),
                    identity,
                )
                .unwrap();
            GraphWriteStepReceipt::VertexUpsert { outcome }
        }
        WriteFacade::EdgeMerge => {
            let bound = PreparedGraphEdgeMergeText::prepare(text, R, symbols)
                .unwrap()
                .bind_parameters(args)
                .unwrap();
            let (_, outcome) = txn
                .execute_graph_edge_merge_governed(
                    db,
                    cx,
                    &bound,
                    policy.edge_merge_policy(),
                    |_| Ok::<_, ()>(ElementId::Edge(EId(200))),
                )
                .unwrap();
            GraphWriteStepReceipt::EdgeMerge { outcome }
        }
        WriteFacade::EdgeUpsert => {
            let bound = PreparedGraphEdgeUpsertText::prepare(text, R, symbols)
                .unwrap()
                .bind_parameters(args)
                .unwrap();
            let (_, outcome) = txn
                .execute_graph_edge_upsert_governed(
                    db,
                    cx,
                    &bound,
                    policy.edge_upsert_policy(),
                    |_| Ok::<_, ()>(ElementId::Edge(EId(200))),
                )
                .unwrap();
            GraphWriteStepReceipt::EdgeUpsert { outcome }
        }
    }
}

async fn write_case(
    commit: &CommitCx,
    cx: &QueryCx,
    txcx: &TxnCx,
    kind: WriteFacade,
    text: &str,
    autocommit: bool,
) {
    let mut direct = seeded(commit).await;
    let mut entry = seeded(commit).await;
    let args = GqlParameters::new();
    let before_vertices = entry.vertices().unwrap();
    let before_edges = entry.edges().unwrap();
    let before = entry.frontier().unwrap();
    let mut direct_txn = direct.begin(txcx).unwrap();
    let expected = direct_write(&mut direct_txn, &mut direct, cx, kind, text, &args);
    let expected_digest = direct_txn.staged_effect_digest().unwrap();
    let (receipt, completion) = if autocommit {
        let QueryResult::Write {
            receipt,
            completion,
        } = entry
            .query_write(
                txcx,
                cx,
                commit,
                text,
                &args,
                symbols,
                R,
                write_policy(),
                |r| identity(r.request),
            )
            .await
            .unwrap()
        else {
            panic!("write returned read rows")
        };
        (
            receipt,
            completion.expect("autocommit must include completion"),
        )
    } else {
        let mut txn = entry.begin(txcx).unwrap();
        let QueryResult::Write {
            receipt,
            completion,
        } = txn
            .query_write(
                &mut entry,
                cx,
                text,
                &args,
                symbols,
                R,
                write_policy(),
                |r| identity(r.request),
            )
            .unwrap()
        else {
            panic!("write returned read rows")
        };
        assert_eq!(completion, None, "staging is not commit");
        assert_eq!(entry.frontier().unwrap(), before);
        assert_eq!(entry.vertices().unwrap(), before_vertices);
        assert_eq!(entry.edges().unwrap(), before_edges);
        assert_eq!(
            txn.staged_effect_digest().unwrap(),
            expected_digest,
            "{kind:?}: {text}"
        );
        (receipt, txn.finish(&mut entry, commit).await.unwrap())
    };
    assert_eq!(receipt.stats().completed_statements, 1);
    assert_eq!(
        receipt.steps(),
        &[expected],
        "{kind:?}, autocommit={autocommit}: {text}"
    );
    let direct_completion = direct_txn.finish(&mut direct, commit).await.unwrap();
    assert_eq!(completion, direct_completion);
    assert_eq!(entry.frontier().unwrap(), direct.frontier().unwrap());
    assert_eq!(
        entry.vertices().unwrap(),
        direct.vertices().unwrap(),
        "vertex effects: {text}"
    );
    assert_eq!(
        entry.edges().unwrap(),
        direct.edges().unwrap(),
        "edge effects: {text}"
    );
    assert_eq!(txcx.outstanding_obligations(), 0);
}

#[test]
fn every_write_facade_matches_two_specific_queries_in_autocommit_and_outer_transaction() {
    let ((), report) = run_async_under_lab(0x7d14_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        for (kind, text) in [
            (
                WriteFacade::Mutation,
                "MATCH (n:Person) WHERE n.p=1 SET n.q=99",
            ),
            (
                WriteFacade::Mutation,
                "MATCH (n:Person) WHERE n.p=4 REMOVE n.q",
            ),
            (
                WriteFacade::Mutation,
                "MATCH (n) WHERE n.p=1 DETACH DELETE n",
            ),
            (WriteFacade::Insert, "CREATE (n:Person {p:7,q:70})"),
            (
                WriteFacade::Insert,
                "MATCH (a:Person) WHERE a.p=1 CREATE (a)-[:R]->(n:Person {p:8})",
            ),
            (WriteFacade::Delete, "MATCH (n:Person) WHERE n.p=4 DELETE n"),
            (
                WriteFacade::Delete,
                "MATCH (n:Person) WHERE n.p=99 DELETE n",
            ),
            (WriteFacade::VertexMerge, "MERGE (n:Person {p:1})"),
            (WriteFacade::VertexMerge, "MERGE (n:Person {p:7})"),
            (
                WriteFacade::VertexUpsert,
                "MERGE (n:Person {p:1}) ON MATCH SET n.q=91 ON CREATE SET n.q=92",
            ),
            (
                WriteFacade::VertexUpsert,
                "MERGE (n:Person {p:7}) ON MATCH SET n.q=91 ON CREATE SET n.q=92",
            ),
            (
                WriteFacade::EdgeMerge,
                "MATCH (a:Person),(b:Person) WHERE a.p=1 AND b.p=2 MERGE (a)-[:R]->(b)",
            ),
            (
                WriteFacade::EdgeMerge,
                "MATCH (a:Person),(b:Person) WHERE a.p=1 AND b.p=3 MERGE (a)-[:R]->(b)",
            ),
            (
                WriteFacade::EdgeMerge,
                "MATCH (a:Person),(b:Person) WHERE a.p=99 AND b.p=3 MERGE (a)-[:R]->(b)",
            ),
            (
                WriteFacade::EdgeUpsert,
                "MATCH (a:Person),(b:Person) WHERE a.p=1 AND b.p=2 MERGE (a)-[e:R]->(b) ON MATCH SET e.q=91 ON CREATE SET e.q=92",
            ),
            (
                WriteFacade::EdgeUpsert,
                "MATCH (a:Person),(b:Person) WHERE a.p=1 AND b.p=3 MERGE (a)-[e:R]->(b) ON MATCH SET e.q=91 ON CREATE SET e.q=92",
            ),
        ] {
            for autocommit in [false, true] {
                write_case(&commit, &cx, &txcx, kind, text, autocommit).await;
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn script_identity(request: GraphWriteIdentityRequest) -> Result<ElementId, ()> {
    Ok(match identity(request.request)? {
        ElementId::Vertex(VId(id)) => ElementId::Vertex(VId(id + request.statement as u128 * 1000)),
        ElementId::Edge(EId(id)) => ElementId::Edge(EId(id + request.statement as u128 * 1000)),
    })
}

#[test]
fn two_write_scripts_match_native_receipts_overlay_effects_and_single_commit() {
    let ((), report) = run_async_under_lab(0x7d14_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        for text in [
            "MERGE (n:Person {p:7}); MERGE (n:Person {p:8}); MATCH (a:Person),(b:Person) WHERE a.p=7 AND b.p=8 MERGE (a)-[:R]->(b)",
            "MATCH (n) WHERE n.p=4 DELETE n; CREATE (n:Person {p:4}); MATCH (n:Person) WHERE n.p=4 SET n.q=44",
        ] {
            for autocommit in [false, true] {
                let mut direct = seeded(&commit).await;
                let mut entry = seeded(&commit).await;
                let before = entry.frontier().unwrap();
                let args = GqlParameters::new();
                let prepared = PreparedGraphWriteScript::prepare(text, R, symbols).unwrap();
                let (expected, expected_completion, actual, actual_completion) = if autocommit {
                    let (expected, completion) = direct
                        .execute_graph_write_script_autocommit_governed(
                            &txcx,
                            &cx,
                            &commit,
                            &prepared,
                            &args,
                            write_policy(),
                            script_identity,
                        )
                        .await
                        .unwrap();
                    let QueryResult::Write {
                        receipt,
                        completion: actual_completion,
                    } = entry
                        .query_write(
                            &txcx,
                            &cx,
                            &commit,
                            text,
                            &args,
                            symbols,
                            R,
                            write_policy(),
                            script_identity,
                        )
                        .await
                        .unwrap()
                    else {
                        panic!("script returned rows")
                    };
                    (expected, completion, receipt, actual_completion.unwrap())
                } else {
                    let mut direct_txn = direct.begin(&txcx).unwrap();
                    let mut txn = entry.begin(&txcx).unwrap();
                    let expected = direct_txn
                        .execute_graph_write_script_governed(
                            &mut direct,
                            &cx,
                            &prepared,
                            &args,
                            write_policy(),
                            script_identity,
                        )
                        .unwrap();
                    let QueryResult::Write {
                        receipt,
                        completion,
                    } = txn
                        .query_write(
                            &mut entry,
                            &cx,
                            text,
                            &args,
                            symbols,
                            R,
                            write_policy(),
                            script_identity,
                        )
                        .unwrap()
                    else {
                        panic!("script returned rows")
                    };
                    assert_eq!(completion, None);
                    assert_eq!(entry.frontier().unwrap(), before);
                    assert_eq!(
                        txn.staged_effect_digest().unwrap(),
                        direct_txn.staged_effect_digest().unwrap()
                    );
                    (
                        expected,
                        direct_txn.finish(&mut direct, &commit).await.unwrap(),
                        receipt,
                        txn.finish(&mut entry, &commit).await.unwrap(),
                    )
                };
                assert_eq!(actual, expected, "{text}");
                assert_eq!(actual_completion, expected_completion);
                assert_eq!(actual.stats().completed_statements, 3);
                assert!(
                    matches!(actual_completion, EmbeddedTxnCompletion::WriteCommitted { commit_seq }
                    if commit_seq == CommitSeq(before.0 + 1))
                );
                assert_eq!(entry.vertices().unwrap(), direct.vertices().unwrap());
                assert_eq!(entry.edges().unwrap(), direct.edges().unwrap());
                assert_eq!(txcx.outstanding_obligations(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn scalar_payloads_match_declared_read_and_write_facades_without_interpolation() {
    let ((), report) = run_async_under_lab(0x7d14_0007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let payload = "'); MERGE (escape); -- COUNT(*) FOR SYSTEM_TIME";
        let declarations = [(
            "name",
            GqlParameterType::Scalar(fgdb_types::CanonicalScalarKind::Text),
        )];
        let args = GqlParameters::new().with_text("name", payload).unwrap();
        let text = "CREATE (n:Person {p:$name})";
        let prepared =
            PreparedGraphInsertText::prepare_with_parameter_types(text, R, &declarations, symbols)
                .unwrap()
                .bind_parameters(&args)
                .unwrap();
        for autocommit in [false, true] {
            let mut direct = seeded(&commit).await;
            let mut entry = seeded(&commit).await;
            let mut direct_txn = direct.begin(&txcx).unwrap();
            let (_, vertices, edges) = direct_txn
                .execute_graph_insert_returning_governed(
                    &mut direct,
                    &cx,
                    &prepared,
                    write_policy().insertion_policy(),
                    identity,
                )
                .unwrap();
            let expected_step = GraphWriteStepReceipt::Insert { vertices, edges };
            let (receipt, completion) = if autocommit {
                let QueryResult::Write {
                    receipt,
                    completion,
                } = entry
                    .query_write(
                        &txcx,
                        &cx,
                        &commit,
                        text,
                        &args,
                        symbols,
                        R,
                        write_policy(),
                        |r| identity(r.request),
                    )
                    .await
                    .unwrap()
                else {
                    panic!("scalar insertion returned rows")
                };
                (receipt, completion.unwrap())
            } else {
                let mut txn = entry.begin(&txcx).unwrap();
                let QueryResult::Write {
                    receipt,
                    completion,
                } = txn
                    .query_write(
                        &mut entry,
                        &cx,
                        text,
                        &args,
                        symbols,
                        R,
                        write_policy(),
                        |r| identity(r.request),
                    )
                    .unwrap()
                else {
                    panic!("scalar insertion returned rows")
                };
                assert_eq!(completion, None);
                assert_eq!(
                    txn.staged_effect_digest().unwrap(),
                    direct_txn.staged_effect_digest().unwrap()
                );
                (receipt, txn.finish(&mut entry, &commit).await.unwrap())
            };
            assert_eq!(receipt.steps(), &[expected_step]);
            assert_eq!(
                completion,
                direct_txn.finish(&mut direct, &commit).await.unwrap()
            );
            assert_eq!(entry.vertices().unwrap(), direct.vertices().unwrap());
            let read = "MATCH (n:Person) WHERE n.p=$name RETURN n.p AS payload";
            let bound =
                PreparedGraphText::prepare_with_parameter_types(read, &declarations, symbols)
                    .unwrap()
                    .bind_parameters(&args)
                    .unwrap();
            let expected = plain(
                bound.columns(),
                direct
                    .execute_graph_pattern_governed(&cx, &bound, policy())
                    .unwrap()
                    .value,
            );
            assert_eq!(
                entry.query(&cx, read, &args, symbols, policy()).unwrap(),
                expected
            );
            assert_eq!(
                expected,
                QueryResult::Rows {
                    columns: vec!["payload".into()],
                    rows: vec![vec![GraphAggregateValue::Value(GraphValue::Scalar(
                        CanonicalScalar::ucs_basic_text(payload).unwrap(),
                    ))]],
                }
            );
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_script_budget_refusal_matches_native_error_and_preserves_outer_prefix() {
    let ((), report) = run_async_under_lab(0x7d14_0008, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let text = "MERGE (n:Person {p:7}); MERGE (n:Person {p:8})";
        let prepared = PreparedGraphWriteScript::prepare(text, R, symbols).unwrap();
        let args = GqlParameters::new();
        let budget = GraphWriteProgramPolicy::new(policy(), 0, 1, 0);
        let mut direct = seeded(&commit).await;
        let mut entry = seeded(&commit).await;
        let mut direct_txn = direct.begin(&txcx).unwrap();
        let mut txn = entry.begin(&txcx).unwrap();
        for (transaction, database) in [(&mut direct_txn, &mut direct), (&mut txn, &mut entry)] {
            let mut prefix = WriteBatch::new(R);
            prefix.create_vertex(VId(99), vec![], vec![(P, CanonicalScalar::Int(99))]);
            transaction.write(database, prefix).unwrap();
        }
        let before = txn.staged_effect_digest().unwrap();
        let mut direct_requests = Vec::new();
        let mut entry_requests = Vec::new();
        let direct_error = direct_txn
            .execute_graph_write_script_governed(
                &mut direct,
                &cx,
                &prepared,
                &args,
                budget,
                |request| {
                    direct_requests.push(request);
                    script_identity(request)
                },
            )
            .unwrap_err();
        let entry_error = txn
            .query_write(
                &mut entry,
                &cx,
                text,
                &args,
                symbols,
                R,
                budget,
                |request| {
                    entry_requests.push(request);
                    script_identity(request)
                },
            )
            .unwrap_err();
        let QueryWriteError::Execute(entry_error) = entry_error else {
            panic!("budget became preparation error")
        };
        for error in [&direct_error, &entry_error] {
            assert!(matches!(
                error,
                GraphWriteScriptExecutionError::Program(GraphWriteProgramError::CreationBudget {
                    statement: 1,
                    dimension: fgdb_gql::insertion::GraphInsertLimitDimension::Vertices,
                    limit: 1,
                    observed: 2,
                })
            ));
        }
        assert_eq!(entry_requests, direct_requests);
        assert_eq!(
            entry_requests.len(),
            1,
            "second statement must refuse before allocation"
        );
        assert_eq!(txn.staged_effect_digest().unwrap(), before);
        assert_eq!(direct_txn.staged_effect_digest().unwrap(), before);
        assert!(txn.vertex(&entry, VId(100)).unwrap().is_none());
        assert!(txn.vertex(&entry, VId(99)).unwrap().is_some());
        assert_eq!(
            txn.finish(&mut entry, &commit).await.unwrap(),
            direct_txn.finish(&mut direct, &commit).await.unwrap()
        );
        assert_eq!(entry.vertices().unwrap(), direct.vertices().unwrap());
        assert!(entry.vertex(VId(100)).unwrap().is_none());
        assert!(entry.vertex(VId(99)).unwrap().is_some());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
