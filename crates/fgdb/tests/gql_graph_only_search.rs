//! Topology-only native retrieval over the existing graph, transaction and
//! capability owners. Expected distances/ranks are hand-enumerated. No text
//! document, embedding, fabricated index or alternate graph engine is needed.

use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryResult, QueryValue, WriteBatch, WriteTxnError};
use fgdb_beacon::expansion::{ExpansionDirection, ExpansionLimits, ExpansionSpec};
use fgdb_beacon::read::{Projection, ReadOptions, ReadPolicy, VectorEncoding};
use fgdb_beacon::{GraphHybridQuery, IndexConfig};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphSetText};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use fgdb_warden::{Authority, Grant, QueryLimits, Scope};

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x74; 32]);
const PERSON: LabelId = LabelId(1);
const R: RelationId = RelationId(1);
const UNUSED: PropertyKeyId = PropertyKeyId(9);

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(RelationId(2))),
        (GraphSymbolKind::Property, "unused") => Some(GraphSymbol::Property(UNUSED)),
        _ => None,
    }
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 1_000, 10_000_000, 100_000)
}

fn statement(arguments: &str) -> String {
    format!(
        "CALL hybrid.search({arguments}) \
         YIELD node, graph_hops, graph_rank, vector_rank, text_rank \
         RETURN node, graph_hops, graph_rank, vector_rank, text_rank"
    )
}

fn reach(eligible: &str) -> String {
    statement(&format!(
        "seeds => [1], max_hops => 3, label => 'Person', relation => 'R', \
         eligible => {eligible}, candidates => 1, k => 1"
    ))
}

fn rows(result: QueryResult) -> Vec<Vec<QueryValue>> {
    let QueryResult::Rows { columns, rows } = result else {
        panic!("a graph-only read returned a write");
    };
    assert_eq!(
        columns,
        [
            "node",
            "graph_hops",
            "graph_rank",
            "vector_rank",
            "text_rank"
        ]
    );
    rows
}

fn expected(cells: &[(u128, i64, i64)]) -> Vec<Vec<QueryValue>> {
    cells
        .iter()
        .map(|&(node, hops, rank)| {
            [
                GraphValue::Vertex(VId(node)),
                GraphValue::Scalar(CanonicalScalar::Int(hops)),
                GraphValue::Scalar(CanonicalScalar::Int(rank)),
                GraphValue::Scalar(CanonicalScalar::Null),
                GraphValue::Scalar(CanonicalScalar::Null),
            ]
            .into_iter()
            .map(QueryValue::Value)
            .collect()
        })
        .collect()
}

/// Visible cycle 1->2->3->1, tail 3->4, isolate 5. Duplicate edges and a
/// self-loop do not change minimum hops. The hidden fixture adds 1->90->4,
/// a shorter route that a scoped reader must neither traverse nor return.
async fn database(cx: &CommitCx, hidden: bool) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x73; 32], NS, [0x75; 32]))
        .await
        .unwrap();
    let mut batch = WriteBatch::new(R);
    for id in 1..=5 {
        // A wrong-typed retrieval property is harmless to a topology-only
        // request. An accidental text/vector projection would refuse it.
        batch.create_vertex(
            VId(id),
            vec![PERSON],
            vec![(UNUSED, CanonicalScalar::Bool(true))],
        );
    }
    for (id, a, b) in [
        (1, 1, 2),
        (2, 2, 3),
        (3, 3, 1),
        (4, 3, 4),
        (5, 1, 2),
        (6, 2, 2),
    ] {
        batch.add_edge(EId(id), VId(a), VId(b), vec![]);
    }
    if hidden {
        batch.create_vertex(VId(90), vec![LabelId(9)], vec![]);
        batch.add_edge(EId(90), VId(1), VId(90), vec![]);
        batch.add_edge(EId(91), VId(90), VId(4), vec![]);
    }
    db.write(cx, batch).await.unwrap();
    db
}

#[test]
fn seeds_only_native_calls_rank_directions_multiple_seeds_and_isolates() {
    let ((), report) = run_async_under_lab(0xbeac_3701, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = database(&contexts.commit(), false).await;
        let params = GqlParameters::new();
        for (args, answer) in [
            (
                "seeds => [1], max_hops => 3",
                vec![(2, 1, 1), (3, 2, 2), (4, 3, 3)],
            ),
            (
                "seeds => [1], max_hops => 1, direction => 'in'",
                vec![(3, 1, 1)],
            ),
            (
                "seeds => [1], max_hops => 1, direction => 'both'",
                vec![(2, 1, 1), (3, 1, 2)],
            ),
            (
                "seeds => [3, 1, 3], max_hops => 1",
                vec![(2, 1, 1), (4, 1, 2)],
            ),
            (
                "seeds => [5, 3, 3, 999], max_hops => 0, include_seeds => true",
                vec![(3, 0, 1), (5, 0, 2)],
            ),
            ("seeds => [5, 3], max_hops => 0", vec![]),
            ("seeds => [], max_hops => 3", vec![]),
            ("seeds => [1], max_hops => 3, relation => 'S'", vec![]),
        ] {
            let text = statement(args);
            assert_eq!(
                rows(db.query(&cx, &text, &params, symbols, policy()).unwrap()),
                expected(&answer),
                "{text}"
            );
        }
        // Both 2 and 3 remain transit vertices although neither can be a hit.
        assert_eq!(
            rows(
                db.query(&cx, &reach("[4, 4, 999]"), &params, symbols, policy())
                    .unwrap()
            ),
            expected(&[(4, 3, 1)])
        );
        assert!(
            rows(
                db.query(&cx, &reach("[]"), &params, symbols, policy())
                    .unwrap()
            )
            .is_empty()
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn zero_hop_parameters_and_pinned_reads_do_not_reacquire_the_writer_head() {
    let ((), report) = run_async_under_lab(0xbeac_3702, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = database(&commit, false).await;
        let pinned = db.read_session().unwrap();
        let text = statement("seeds => [1], max_hops => $hops, include_seeds => true");
        let zero = GqlParameters::new().with_int64("hops", 0).unwrap();
        let one = GqlParameters::new().with_int64("hops", 1).unwrap();
        assert_eq!(
            rows(pinned.query(&cx, &text, &zero, symbols, policy()).unwrap()),
            expected(&[(1, 0, 1)])
        );
        let before = pinned.query(&cx, &text, &one, symbols, policy()).unwrap();
        assert_eq!(rows(before.clone()), expected(&[(1, 0, 1), (2, 1, 2)]));
        let mut change = WriteBatch::new(R);
        change.delete_vertex(VId(2));
        change.add_edge(EId(10), VId(1), VId(4), vec![]);
        db.write(&commit, change).await.unwrap();
        assert_eq!(
            rows(db.query(&cx, &text, &one, symbols, policy()).unwrap()),
            expected(&[(1, 0, 1), (4, 1, 2)])
        );
        assert_eq!(
            pinned.query(&cx, &text, &one, symbols, policy()).unwrap(),
            before
        );
        drop(db);
        assert_eq!(
            pinned.query(&cx, &text, &one, symbols, policy()).unwrap(),
            before
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn transaction_typed_graph_only_reads_need_no_index_or_projected_properties() {
    let ((), report) = run_async_under_lab(0xbeac_3703, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = database(&commit, false).await;
        let txn = db.begin(&txcx).unwrap();
        let mut options = ReadOptions {
            as_of: None,
            vertex_label: Some(PERSON),
            projection: Projection {
                text: Some(UNUSED),
                vector: vec![UNUSED],
                encoding: VectorEncoding::Coordinates,
            },
            index: IndexConfig {
                vector: None,
                text: None,
                ..IndexConfig::default()
            },
            policy: ReadPolicy::default(),
        };
        assert!(options.index.validate().is_err());
        let query = GraphHybridQuery::graph_only(1, 1, 100, 60).unwrap();
        let mut expansion = ExpansionSpec {
            seeds: &[VId(1)],
            relation: Some(R),
            direction: ExpansionDirection::Outgoing,
            max_hops: 3,
            include_seeds: false,
            limits: ExpansionLimits::default(),
        };
        let result = txn
            .beacon_search_graph_filtered(&db, &cx, &options, query, expansion, |id| id == VId(4))
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, VId(4));
        assert_eq!(result[0].graph_hops, Some(3));
        assert_eq!(
            (result[0].score.numerator(), result[0].score.denominator()),
            (100, 61)
        );
        assert_eq!(result[0].vector_distance, None);
        assert_eq!(result[0].text_score, None);
        assert_eq!(
            db.beacon_search_graph_filtered(&cx, &options, query, expansion, |id| id == VId(4))
                .unwrap(),
            result
        );
        expansion.seeds = &[VId(5)];
        expansion.max_hops = 0;
        expansion.include_seeds = true;
        expansion.limits.max_input_edges = 0;
        expansion.limits.max_source_scratch = 0;
        let result = txn
            .beacon_search_graph(&db, &cx, &options, query, expansion)
            .unwrap();
        assert_eq!((result[0].id, result[0].graph_hops), (VId(5), Some(0)));
        options.policy.max_staging_rows = 0;
        assert!(
            txn.beacon_search_graph_filtered(&db, &cx, &options, query, expansion, |_| false)
                .is_err()
        );
        txn.abort();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_graph_only_calls_read_staged_topology_labels_and_savepoint_rollback() {
    let ((), report) = run_async_under_lab(0xbeac_3704, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = database(&commit, false).await;
        let basis = db.frontier().unwrap();
        let text = reach("[4]");
        let params = GqlParameters::new();
        let mut txn = db.begin(&txcx).unwrap();
        let original = txn
            .query(&db, &cx, &text, &params, symbols, policy())
            .unwrap();
        assert_eq!(rows(original.clone()), expected(&[(4, 3, 1)]));
        txn.savepoint(&db, "base").unwrap();
        let mut short = WriteBatch::new(R);
        short.add_edge(EId(10), VId(1), VId(4), vec![]);
        short.create_vertex(VId(6), vec![PERSON], vec![]);
        txn.write(&mut db, short).unwrap();
        let shortened = txn
            .query(&db, &cx, &text, &params, symbols, policy())
            .unwrap();
        assert_eq!(rows(shortened.clone()), expected(&[(4, 1, 1)]));
        let seed = statement("seeds => [6], max_hops => 0, include_seeds => true");
        assert_eq!(
            rows(
                txn.query(&db, &cx, &seed, &params, symbols, policy())
                    .unwrap()
            ),
            expected(&[(6, 0, 1)])
        );
        assert!(rows(db.query(&cx, &seed, &params, symbols, policy()).unwrap()).is_empty());
        let mut label = WriteBatch::new(R);
        label.set_vertex_label(VId(4), PERSON, false);
        txn.write(&mut db, label).unwrap();
        assert!(
            rows(
                txn.query(&db, &cx, &text, &params, symbols, policy())
                    .unwrap()
            )
            .is_empty()
        );
        txn.rollback_to_savepoint(&db, "base").unwrap();
        assert_eq!(
            txn.query(&db, &cx, &text, &params, symbols, policy())
                .unwrap(),
            original
        );
        assert!(txn.vertex(&db, VId(6)).unwrap().is_none());
        let mut short = WriteBatch::new(R);
        short.add_edge(EId(10), VId(1), VId(4), vec![]);
        txn.write(&mut db, short).unwrap();
        assert_eq!(db.frontier().unwrap(), basis);
        txn.commit(&mut db, &commit).await.unwrap();
        assert_eq!(
            db.query(&cx, &text, &params, symbols, policy()).unwrap(),
            shortened
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn empty_or_hidden_output_retains_transaction_edge_dependencies_after_rollback() {
    let ((), report) = run_async_under_lab(0xbeac_3705, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let params = GqlParameters::new();
        for hidden_output in [false, true] {
            let mut db = database(&commit, false).await;
            let mut txn = db.begin(&txcx).unwrap();
            txn.savepoint(&db, "before-read").unwrap();
            let text = if hidden_output {
                format!("{} LIMIT 0", reach("[4]"))
            } else {
                reach("[]")
            };
            assert!(
                rows(
                    txn.query(&db, &cx, &text, &params, symbols, policy())
                        .unwrap()
                )
                .is_empty()
            );
            txn.rollback_to_savepoint(&db, "before-read").unwrap();
            let mut winner = WriteBatch::new(R);
            winner.add_edge(EId(10), VId(1), VId(5), vec![]);
            db.write(&commit, winner).await.unwrap();
            let frontier = db.frontier().unwrap();
            // Do not re-read: rollback must retain THIS read's dependency.
            assert!(matches!(
                txn.finish(&mut db, &commit).await,
                Err(WriteTxnError::Write(
                    fgdb::WriteError::FirstCommitterWins { .. }
                ))
            ));
            assert_eq!(db.frontier().unwrap(), frontier);
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn authorized_graph_only_prepared_calls_cannot_use_hidden_shortcuts_or_seeds() {
    let ((), report) = run_async_under_lab(0xbeac_3706, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let db = database(&commit, true).await;
        let visible = database(&commit, false).await;
        let authority = Authority::new(
            AuthKey::from_seed(3706),
            NS,
            "graph-only-host",
            SchemaEpoch(0),
            1,
        )
        .unwrap();
        let mut grant = Grant::read_only(
            "main",
            1000,
            QueryLimits {
                max_nodes: 1000,
                max_work: 10_000_000,
                max_rows: 1000,
            },
        );
        grant.labels = Scope::only([PERSON]);
        grant.relations = Scope::only([R]);
        grant.properties = Scope::only([]);
        let token = authority.issue_at(&grant, 100).unwrap();
        let params = GqlParameters::new();
        // No label selector here: only the capability removes the shortcut.
        let text =
            statement("seeds => [1], max_hops => 3, eligible => [90, 4], candidates => 1, k => 1");
        let privileged = db.query(&cx, &text, &params, symbols, policy()).unwrap();
        assert_eq!(rows(privileged), expected(&[(90, 1, 1)]));
        let control = visible
            .query(&cx, &text, &params, symbols, policy())
            .unwrap();
        assert_eq!(rows(control.clone()), expected(&[(4, 3, 1)]));
        let mut session = db
            .authorized_read_session(&cx, &authority, &token, "main", symbols, policy(), || 100)
            .unwrap();
        let prepared = session.prepare(&cx, &text, &params).unwrap();
        assert_eq!(session.execute(&cx, &prepared, &params).unwrap(), control);
        let seed = statement("seeds => [90, 5], max_hops => 0, include_seeds => true");
        let prepared = session.prepare(&cx, &seed, &params).unwrap();
        assert_eq!(
            rows(session.execute(&cx, &prepared, &params).unwrap()),
            expected(&[(5, 0, 1)])
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn invalid_graph_only_requests_refuse_even_with_limit_zero_and_preserve_staged_writes() {
    let ((), report) = run_async_under_lab(0xbeac_3707, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = database(&commit, false).await;
        let params = GqlParameters::new();
        let mut txn = db.begin(&txcx).unwrap();
        let mut write = WriteBatch::new(R);
        write.create_vertex(VId(6), vec![PERSON], vec![]);
        txn.write(&mut db, write).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        for args in [
            "k => 1",
            "seeds => null, max_hops => 1",
            "seeds => [1]",
            "seeds => [1], max_hops => -1",
            "seeds => [1], max_hops => 4294967296",
            "seeds => [1], max_hops => 1.5",
            "seeds => [1], max_hops => 1, graph_candidates => 0",
            "seeds => [1], max_hops => 1, graph_weight => 0",
            "seeds => [1], max_hops => 1, candidates => 1, k => 2",
            "seeds => [1], max_hops => 1, text => 'missing schema'",
            "seeds => [1], max_hops => 1, metric => 'cosine'",
            "seeds => [1], max_hops => 1, eligible => [-1]",
        ] {
            let text = format!("{} LIMIT 0", statement(args));
            assert!(
                db.query(&cx, &text, &params, symbols, policy()).is_err(),
                "{text}"
            );
            assert!(
                txn.query(&db, &cx, &text, &params, symbols, policy())
                    .is_err(),
                "{text}"
            );
            assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        }
        assert!(txn.vertex(&db, VId(6)).unwrap().is_some());
        assert!(db.vertex(VId(6)).unwrap().is_none());
        txn.abort();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn graph_only_transaction_queries_share_work_scratch_and_record_allowances() {
    let ((), report) = run_async_under_lab(0xbeac_3708, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = database(&commit, false).await;
        let txn = db.begin(&txcx).unwrap();
        let params = GqlParameters::new();
        let text = reach("[4]");
        let prepare = |text: &str| {
            PreparedGraphSetText::prepare(text, symbols)
                .unwrap()
                .bind_parameters(&params)
                .unwrap()
        };
        let query = prepare(&text);
        // Populate retained read witnesses, then compare repeatable per-read
        // accounting rather than charging their first insertion twice.
        txn.execute_graph_set_governed(&db, &cx, &query, policy())
            .unwrap();
        let baseline = txn
            .execute_graph_set_governed(&db, &cx, &query, policy())
            .unwrap();
        assert_eq!(baseline.value.len(), 1);
        assert!(baseline.rows.snapshot_records >= 5);
        assert!(baseline.evaluator.work_units > 1);
        assert!(baseline.evaluator.scratch_entries > 1);
        let exact = GqlQueryPolicy::new(
            baseline.rows.snapshot_records,
            baseline.rows.result_rows,
            baseline.evaluator.work_units,
            baseline.evaluator.scratch_entries,
        );
        assert_eq!(
            txn.execute_graph_set_governed(&db, &cx, &query, exact)
                .unwrap(),
            baseline
        );
        for dimension in 0..3 {
            let short = GqlQueryPolicy::new(
                baseline.rows.snapshot_records - u64::from(dimension == 0),
                baseline.rows.result_rows,
                baseline.evaluator.work_units - u64::from(dimension == 1),
                baseline.evaluator.scratch_entries - u64::from(dimension == 2),
            );
            assert!(
                txn.execute_graph_set_governed(&db, &cx, &query, short)
                    .is_err(),
                "dimension {dimension}"
            );
        }
        let combined = prepare(&format!("{text} UNION ALL {text}"));
        let one_source =
            GqlQueryPolicy::new(baseline.rows.snapshot_records, 100, 10_000_000, 100_000);
        assert!(
            txn.execute_graph_set_governed(&db, &cx, &combined, one_source)
                .is_err()
        );
        assert_eq!(
            txn.execute_graph_set_governed(&db, &cx, &combined, policy())
                .unwrap()
                .value
                .len(),
            2
        );
        let empty = format!("{} LIMIT 0", reach("[]"));
        assert!(
            txn.query(
                &db,
                &cx,
                &empty,
                &params,
                symbols,
                GqlQueryPolicy::new(0, 0, 0, 0)
            )
            .is_err()
        );
        txn.abort();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
