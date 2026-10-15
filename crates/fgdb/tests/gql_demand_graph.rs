//! Native graph-only demand execution: actual source selection, coherent pins,
//! capability admission and explicit refusal rather than committed-only reads.
#![recursion_limit = "256"]

use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryResult, QueryValue, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphSetText};
use fgdb_types::{CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use fgdb_warden::{Authority, Grant, QueryLimits, Scope};
use std::cell::Cell;

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x59; 32]);
const R: RelationId = RelationId(1);
const BODY: PropertyKeyId = PropertyKeyId(9);

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Node") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "body") => Some(GraphSymbol::Property(BODY)),
        _ => None,
    }
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100, 10_000_000, 100_000)
}

async fn open(cx: &CommitCx, hidden: bool) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x58; 32], NS, [0x5a; 32]))
        .await.unwrap();
    let mut batch = WriteBatch::new(R);
    for id in [1, 2, 3, 9] {
        batch.create_vertex(VId(id), vec![LabelId(1)], vec![(BODY, CanonicalScalar::Bool(true))]);
    }
    batch.add_edge(EId(1), VId(1), VId(2), vec![]);
    batch.add_edge(EId(2), VId(2), VId(3), vec![]);
    for id in 1_000..2_024 {
        batch.create_vertex(VId(id), vec![LabelId(1)], vec![]);
        batch.add_edge(EId(id), VId(id), VId(id), vec![]);
    }
    if hidden {
        batch.create_vertex(VId(90), vec![LabelId(99)], vec![]);
        batch.add_edge(EId(90), VId(1), VId(90), vec![]);
        batch.add_edge(EId(91), VId(90), VId(9), vec![]);
    }
    db.write(cx, batch).await.unwrap();
    if hidden {
        let mut shortcut = WriteBatch::new(RelationId(99));
        shortcut.add_edge(EId(92), VId(1), VId(3), vec![]);
        db.write(cx, shortcut).await.unwrap();
    }
    db
}

fn statement(access: &str) -> String {
    format!(
        "CALL hybrid.search(seeds => [$source], max_hops => 2, graph_access => '{access}', \
         eligible => $eligible, candidates => 1, k => 1) \
         YIELD node, graph_hops, graph_rank, text_rank, vector_rank \
         RETURN node, graph_hops, graph_rank, text_rank, vector_rank"
    )
}

fn params(source: i64, eligible: &[i64]) -> GqlParameters {
    GqlParameters::new().with_int64("source", source).unwrap()
        .with_list("eligible", eligible.iter().map(|&id| {
            GraphValue::Scalar(CanonicalScalar::Int(id))
        }).collect()).unwrap()
}

fn rows(result: QueryResult) -> Vec<Vec<QueryValue>> {
    match result {
        QueryResult::Rows { rows, .. } => rows,
        QueryResult::Write { .. } => panic!("graph retrieval performed a write"),
    }
}

fn int(n: i64) -> QueryValue {
    QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(n)))
}

fn expected(id: u128, hops: i64) -> Vec<Vec<QueryValue>> {
    vec![vec![
        QueryValue::Value(GraphValue::Vertex(VId(id))), int(hops), int(1),
        QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Null)),
        QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Null)),
    ]]
}

#[test]
fn native_demand_reads_only_reached_vertices_and_rebinds_one_template() {
    let ((), report) = run_async_under_lab(0xbeac_5201, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = open(&c.commit(), false).await;
        // Three vertices in the reached component, versus 1,028 in the source.
        let tight = GqlQueryPolicy::new(3, 10, 10_000_000, 100_000);
        let text = statement("demand");
        let template = PreparedGraphSetText::prepare(&text, symbols).unwrap();
        for source in [1, 2] {
            let parameters = params(source, &[3]);
            let bound = template.bind_parameters(&parameters).unwrap();
            let result = db.execute_graph_set_governed(&c.query(), &bound, tight).unwrap();
            assert!(result.rows.snapshot_records <= 3);
            assert_eq!(rows(db.query(&c.query(), &text, &parameters, symbols, tight).unwrap()),
                expected(3, 3 - source));
            for mode in ["indexed", "materialized"] {
                let reference = db.query(&c.query(), &statement(mode), &parameters, symbols, policy()).unwrap();
                assert_eq!(rows(reference), expected(3, 3 - source));
                assert!(db.query(&c.query(), &statement(mode), &parameters, symbols, tight).is_err(),
                    "{mode} still prepares the full vertex directory");
            }
        }
        let text = "CALL hybrid.search(seeds => [9,9,999], max_hops => 0, \
            include_seeds => true, graph_access => 'demand', k => 1) \
            YIELD node, graph_hops, graph_rank, text_rank, vector_rank \
            RETURN node, graph_hops, graph_rank, text_rank, vector_rank";
        assert_eq!(rows(db.query(&c.query(), text, &GqlParameters::new(), symbols, tight).unwrap()),
            expected(9, 0));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn reported_source_scratch_and_output_capacity_replay_at_exact_limits() {
    let ((), report) = run_async_under_lab(0xbeac_5202, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = open(&c.commit(), false).await;
        for eligible in [vec![3], vec![]] {
            // Capacity exceeds the actual result even for the nonempty case.
            // The report must retain that reservation on exact-budget replay.
            let text = statement("demand").replace("candidates => 1, k => 1", "candidates => 4, k => 4");
            let parameters = params(1, &eligible);
            let bound = PreparedGraphSetText::prepare(&text, symbols).unwrap()
                .bind_parameters(&parameters).unwrap();
            let baseline = db.execute_graph_set_governed(&c.query(), &bound, policy()).unwrap();
            assert_eq!(baseline.value.len(), usize::from(!eligible.is_empty()));
            assert!(baseline.evaluator.scratch_entries >= 9, "source and output reservation are reported");
            let exact = GqlQueryPolicy::new(
                baseline.rows.snapshot_records, baseline.rows.result_rows,
                baseline.evaluator.work_units, baseline.evaluator.scratch_entries,
            );
            let replay = db.execute_graph_set_governed(&c.query(), &bound, exact).unwrap();
            assert_eq!(replay.value, baseline.value);
            assert_eq!(replay.evaluator, baseline.evaluator);
            let mut short = exact;
            short.evaluator.max_work_units -= 1;
            assert!(db.execute_graph_set_governed(&c.query(), &bound, short).is_err());
            let mut short = exact;
            short.evaluator.max_scratch_entries -= 1;
            assert!(db.execute_graph_set_governed(&c.query(), &bound, short).is_err());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn retained_native_demand_reads_do_not_follow_a_writers_newer_topology() {
    let ((), report) = run_async_under_lab(0xbeac_5203, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = open(&c.commit(), false).await;
        let pin = db.read_session().unwrap();
        let text = statement("demand");
        let parameters = params(1, &[3]);
        let before = pin.query(&c.query(), &text, &parameters, symbols, policy()).unwrap();
        assert_eq!(rows(before), expected(3, 2));
        let mut edit = WriteBatch::new(R);
        edit.delete_vertex(VId(2));
        db.write(&c.commit(), edit).await.unwrap();
        db.compact(&c.commit()).await.unwrap();
        assert!(rows(db.query(&c.query(), &text, &parameters, symbols, policy()).unwrap()).is_empty());
        drop(db);
        assert_eq!(rows(pin.query(&c.query(), &text, &parameters, symbols, policy()).unwrap()), expected(3, 2));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn authorized_prepared_demand_masks_before_expansion_and_retains_live_expiry() {
    let ((), report) = run_async_under_lab(0xbeac_5204, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let full = open(&c.commit(), true).await;
        let clean = open(&c.commit(), false).await;
        let authority = Authority::new(AuthKey::from_seed(5204), NS, "demand-host", SchemaEpoch(0), 1).unwrap();
        let mut grant = Grant::read_only("main", 1_000, QueryLimits {
            max_nodes: 3, max_work: 1_000_000, max_rows: 10,
        });
        grant.labels = Scope::only([LabelId(1)]);
        grant.relations = Scope::only([R]);
        grant.properties = Scope::only([]);
        let token = authority.issue_at(&grant, 100).unwrap();
        let text = statement("demand");
        let parameters = params(1, &[3,90]);
        let reference = rows(clean.query(&c.query(), &text, &parameters, symbols, policy()).unwrap());
        assert_eq!(reference, expected(3, 2));
        assert_ne!(rows(full.query(&c.query(), &text, &parameters, symbols, policy()).unwrap()), reference,
            "the forbidden direct edge must be an active negative control");
        assert_eq!(rows(full.query_authorized(
            &c.query(), &authority, &token, "main", &text, &parameters, symbols, policy(), || 100,
        ).unwrap()), reference);
        let now = Cell::new(100_u64);
        let mut session = full.authorized_read_session(
            &c.query(), &authority, &token, "main", symbols, policy(), || now.get(),
        ).unwrap();
        let prepared = session.prepare(&c.query(), &text, &parameters).unwrap();
        assert_eq!(rows(session.execute(&c.query(), &prepared, &parameters).unwrap()), reference);
        assert!(rows(session.execute(&c.query(), &prepared, &params(1, &[])).unwrap()).is_empty());
        now.set(1_001);
        assert!(session.execute(&c.query(), &prepared, &parameters).is_err());
        assert!(session.is_closed());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unsupported_demand_profiles_cannot_ignore_staged_writes_or_drop_modalities() {
    let ((), report) = run_async_under_lab(0xbeac_5205, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = open(&c.commit(), false).await;
        let basis = db.frontier().unwrap();
        let mut txn = db.begin(&c.txn()).unwrap();
        let mut edit = WriteBatch::new(R);
        edit.create_vertex(VId(11), vec![LabelId(1)], vec![]);
        edit.add_edge(EId(11), VId(2), VId(11), vec![]);
        txn.write(&mut db, edit).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        for tail in ["", " LIMIT 0"] {
            let text = format!("{}{tail}", statement("demand"));
            let error = txn.query(&db, &c.query(), &text, &params(1, &[11]), symbols, policy()).unwrap_err();
            assert!(error.to_string().contains("demand graph access requires an immutable graph-only read"));
            assert_eq!(txn.staged_effect_digest().unwrap(), digest);
            assert_eq!(db.frontier().unwrap(), basis);
        }
        for access in ["indexed", "materialized"] {
            assert_eq!(rows(txn.query(&db, &c.query(), &statement(access), &params(1, &[11]), symbols, policy()).unwrap()),
                expected(11, 2));
        }
        txn.abort();
        for modality in [
            "text => 'graph', text_property => 'body'",
            "vector => [1], vector_properties => ['body']",
        ] {
            let text = format!(
                "CALL hybrid.search(seeds => [1], max_hops => 2, \
                 graph_access => 'demand', {modality}) YIELD node RETURN node LIMIT 0"
            );
            let error = db.query(&c.query(), &text, &GqlParameters::new(), symbols, policy()).unwrap_err();
            assert!(error.to_string().contains("demand graph access requires an immutable graph-only read"));
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn malformed_eligibility_and_late_expressions_refuse_even_under_limit_zero() {
    let ((), report) = run_async_under_lab(0xbeac_5206, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = open(&c.commit(), false).await;
        for text in [
            "CALL hybrid.search(graph_access => 'demand') YIELD node RETURN node LIMIT 0",
            "CALL hybrid.search(seeds => [1], graph_access => 'demand') YIELD node RETURN node LIMIT 0",
            "CALL hybrid.search(seeds => [1], max_hops => 2, graph_access => 'demand', eligible => [-1]) YIELD node RETURN node LIMIT 0",
            "CALL hybrid.search(seeds => [1], max_hops => 2, graph_access => 'demand') YIELD node RETURN 1 / 0 AS fail LIMIT 0",
        ] {
            assert!(db.query(&c.query(), text, &GqlParameters::new(), symbols, policy()).is_err(), "{text}");
        }
        assert_eq!(rows(db.query(&c.query(), &statement("demand"), &params(1, &[3]), symbols, policy()).unwrap()),
            expected(3, 2));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
