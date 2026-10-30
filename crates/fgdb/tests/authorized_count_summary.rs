//! Warm filterless counts must describe the exact live, authorized graph.
//! Compare them with cold admissions, the independent owner executor, every
//! historical cut and each resource boundary; no elapsed-time assertion.

use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryError, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::{
    GqlBudgetDimension, GqlExecutionBudget, GqlParameters, GqlQueryError, GqlQueryPolicy,
    GraphAggregateRow, GraphAggregateValue, GraphSymbol, GraphSymbolKind, PreparedGraphAggregate,
    PreparedGraphAggregateText,
};
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts,
    QueryCx, VId,
};
use fgdb_warden::{Authority, CapabilityToken, Grant, QueryLimits, Restriction, Scope};
use std::cell::Cell;
use std::sync::Arc;

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0xc1; 32]);
const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const A: LabelId = LabelId(1);
const B: LabelId = LabelId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const VERTICES: &str = "MATCH (n) RETURN COUNT(*) AS n";
const R_EDGES: &str = "MATCH (a)-[r:R]->(b) RETURN COUNT(*) AS n";
const S_EDGES: &str = "MATCH (a)-[:S]->(b) RETURN COUNT(*) AS n";
const ALL_EDGES: &str = "MATCH (a)-[]->(b) RETURN COUNT(*) AS n";
const A_VERTICES: &str = "MATCH (n:A) RETURN COUNT(*) AS n";

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Label, "A") => Some(GraphSymbol::Label(A)),
        (GraphSymbolKind::Label, "B") => Some(GraphSymbol::Label(B)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}

fn prepare(text: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 1_000_000, 100_000)
}

fn issuer() -> Authority {
    Authority::new(AuthKey::from_seed(0xc161), NS, "graph", SchemaEpoch(0), 1).unwrap()
}

fn grant() -> Grant {
    let mut grant = Grant::read_only(
        "main",
        10_000,
        QueryLimits {
            max_nodes: 100_000,
            max_work: 1_000_000,
            max_rows: 100_000,
        },
    );
    grant.labels = Scope::All;
    grant.relations = Scope::All;
    grant.properties = Scope::All;
    grant
}

/// Six vertices, parallel edges, a loop and two relations. Later commits
/// retire edges and a vertex, relabel another and add a new parallel pair.
/// Historical statement counts, distinct triples and live counts all differ.
async fn database(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0xc2; 32], NS, [0xc3; 32]))
        .await
        .unwrap();
    let mut first = WriteBatch::new(R);
    for id in 1..=6 {
        first.create_vertex(
            VId(id),
            vec![if id == 6 { B } else { A }],
            vec![(P, CanonicalScalar::Int(if id == 2 { 0 } else { id as i64 }))],
        );
    }
    for (eid, from, to) in [
        (1, 1, 2),
        (2, 1, 2),
        (3, 2, 2),
        (4, 3, 4),
        (5, 5, 6),
        (6, 6, 1),
    ] {
        first.add_edge(EId(eid), VId(from), VId(to), vec![]);
    }
    db.write(cx, first).await.unwrap();
    let mut second = WriteBatch::new(S);
    second.add_edge(EId(10), VId(4), VId(1), vec![]);
    second.add_edge(EId(11), VId(3), VId(5), vec![]);
    db.write(cx, second).await.unwrap();
    let mut third = WriteBatch::new(R);
    for eid in [2, 5, 6] {
        third.delete_edge(EId(eid));
    }
    third.delete_vertex(VId(6));
    third.set_vertex_label(VId(3), A, false);
    third.set_vertex_label(VId(3), B, true);
    third.create_vertex(VId(7), vec![A], vec![(P, CanonicalScalar::Int(7))]);
    third.add_edge(EId(7), VId(1), VId(5), vec![]);
    third.add_edge(EId(8), VId(1), VId(5), vec![]);
    db.write(cx, third).await.unwrap();
    db
}

fn count(rows: &[GraphAggregateRow]) -> u64 {
    assert_eq!(rows.len(), 1);
    let [GraphAggregateValue::Count(value)] = rows[0].values() else {
        panic!("count must keep its native Count domain: {rows:?}")
    };
    *value
}

#[test]
fn live_counts_survive_history_replay_and_generation_changes() {
    let ((), report) = run_async_under_lab(0xc161_0001, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = database(&commit).await;
        let authority = issuer();
        let token = authority.issue_at(&grant(), 100).unwrap();
        for (at, expected) in [
            (0, [0, 0, 0]),
            (1, [6, 6, 0]),
            (2, [6, 6, 2]),
            (3, [6, 5, 2]),
        ] {
            for (text, expected) in [VERTICES, R_EDGES, S_EDGES].into_iter().zip(expected) {
                let query = prepare(text);
                let owner = db
                    .execute_graph_aggregate_governed_at(&cx, &query, CommitSeq(at), policy())
                    .unwrap()
                    .value;
                assert_eq!(count(&owner), expected, "{text}, cut {at}");
                for _ in 0..3 {
                    let rows = db
                        .execute_graph_aggregate_authorized_at(
                            &cx,
                            &authority,
                            &token,
                            "main",
                            &query,
                            CommitSeq(at),
                            policy(),
                            || 100,
                        )
                        .unwrap();
                    assert_eq!(rows, owner, "{text}, cut {at}");
                }
            }
        }
        // Retain the previous snapshot allocation while publishing a change:
        // its still-live cached counts must not serve the new generation.
        let pinned = db.read_session().unwrap();
        let mut change = WriteBatch::new(R);
        change.create_vertex(VId(u128::MAX), vec![A], vec![]);
        change.add_edge(EId(u128::MAX), VId(7), VId(u128::MAX), vec![]);
        db.write(&commit, change).await.unwrap();
        for (text, expected) in [(VERTICES, 7), (R_EDGES, 6), (S_EDGES, 2)] {
            let query = prepare(text);
            for _ in 0..2 {
                let rows = db
                    .execute_graph_aggregate_authorized(
                        &cx,
                        &authority,
                        &token,
                        "main",
                        &query,
                        policy(),
                        || 100,
                    )
                    .unwrap();
                assert_eq!(count(&rows), expected, "new generation: {text}");
            }
            // The public native-text entry used by hosts returns the same
            // lossless Count cells, not a narrowed integer shortcut.
            let fgdb::QueryResult::Rows { rows, .. } = db
                .query_authorized(
                    &cx,
                    &authority,
                    &token,
                    "main",
                    text,
                    &GqlParameters::new(),
                    symbols,
                    policy(),
                    || 100,
                )
                .unwrap()
            else {
                panic!("count returned a write result")
            };
            assert_eq!(rows, vec![vec![GraphAggregateValue::Count(expected)]]);
        }
        drop(pinned);
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

type Footprint = (Result<Vec<GraphAggregateRow>, String>, usize, usize);

#[test]
fn computed_having_aliases_filter_capability_visible_counts_in_the_native_host_entry() {
    let ((), report) = run_async_under_lab(0xc161_0004, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let authority = issuer();
        let full = authority.issue_at(&grant(), 100).unwrap();
        let queries = [
            "MATCH (n) RETURN COUNT(*) + 1 AS n HAVING n > 1",
            "MATCH (a)-[r:R]->(b) RETURN COUNT(*) + 1 AS n HAVING n > 1",
            "MATCH (a)-[:S]->(b) RETURN COUNT(*) + 1 AS n HAVING n > 1",
        ];
        for (restriction, visible) in [
            (Restriction::Labels(Scope::only([A])), [5, 4, 1]),
            (Restriction::Labels(Scope::only([B])), [1, 0, 0]),
            (Restriction::Relations(Scope::only([R])), [6, 5, 0]),
            (Restriction::Properties(Scope::only([])), [6, 5, 2]),
        ] {
            let scoped = full.attenuate(restriction).unwrap();
            // Alternate full and scoped queries: an already admitted full
            // count must never select a scoped HAVING result or its value.
            for _ in 0..2 {
                for (token, counts) in [(&full, [6, 5, 2]), (&scoped, visible)] {
                    for (text, count) in queries.into_iter().zip(counts) {
                        let fgdb::QueryResult::Rows { columns, rows } = db
                            .query_authorized(
                                &cx,
                                &authority,
                                token,
                                "main",
                                text,
                                &GqlParameters::new(),
                                symbols,
                                policy(),
                                || 100,
                            )
                            .unwrap()
                        else {
                            panic!("computed count returned a write result")
                        };
                        assert_eq!(columns, ["n"]);
                        let expected = if count == 0 {
                            Vec::new()
                        } else {
                            vec![vec![GraphAggregateValue::Integer(i128::from(count) + 1)]]
                        };
                        assert_eq!(rows, expected, "{text}, visible count {count}");
                    }
                }
            }
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn footprint(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    token: &CapabilityToken,
    query: &PreparedGraphAggregate,
    policy: GqlQueryPolicy,
    stop: Option<usize>,
) -> Footprint {
    let probe = Arc::new(SimulationCheckpointProbe::new(stop));
    let observed = cx.with_checkpoint_probe(Arc::clone(&probe));
    let clocks = Cell::new(0);
    let result = db.execute_graph_aggregate_authorized(
        &observed,
        &issuer(),
        token,
        "main",
        query,
        policy,
        || {
            clocks.set(clocks.get() + 1);
            100
        },
    );
    (
        result.map_err(|error| format!("{error:?}")),
        probe.calls(),
        clocks.get(),
    )
}

#[derive(Clone, Copy, Debug)]
enum Limit {
    SignedWork,
    Nodes,
    SignedRows,
    NativeWork,
    Scratch,
    Records,
    NativeRows,
}

fn limits(kind: Limit, value: u64) -> (CapabilityToken, GqlQueryPolicy) {
    let mut grant = grant();
    let mut policy = policy();
    match kind {
        Limit::SignedWork => grant.limits.max_work = value,
        Limit::Nodes => grant.limits.max_nodes = value,
        Limit::SignedRows => grant.limits.max_rows = value,
        Limit::NativeWork => policy.evaluator.max_work_units = value,
        Limit::Scratch => policy.evaluator.max_scratch_entries = value,
        Limit::Records => policy.rows = GqlExecutionBudget::new(value, 100_000),
        Limit::NativeRows => policy.rows = GqlExecutionBudget::new(100_000, value),
    }
    (issuer().issue_at(&grant, 100).unwrap(), policy)
}

#[test]
fn warm_counts_keep_cold_refusal_thresholds_and_cancellation_prefixes() {
    let ((), report) = run_async_under_lab(0xc161_0002, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let warm = database(&commit).await;
        let token = issuer().issue_at(&grant(), 100).unwrap();
        for text in [VERTICES, R_EDGES, ALL_EDGES, A_VERTICES] {
            let query = prepare(text);
            let cold = footprint(&warm, &cx, &token, &query, policy(), None);
            assert!(cold.0.is_ok(), "{cold:?}");
            assert_eq!(footprint(&warm, &cx, &token, &query, policy(), None), cold);
            for stop in 1..=cold.1 {
                let result = footprint(&warm, &cx, &token, &query, policy(), Some(stop));
                assert!(result.0.is_err(), "{text}, stop {stop}: {result:?}");
                assert_eq!(result.1, stop, "{text}, stop {stop}");
            }
            for kind in [
                Limit::SignedWork,
                Limit::Nodes,
                Limit::SignedRows,
                Limit::NativeWork,
                Limit::Scratch,
                Limit::Records,
                Limit::NativeRows,
            ] {
                let (mut low, mut high) = (0, 1_000_000);
                while low < high {
                    let middle = low + (high - low) / 2;
                    let (token, policy) = limits(kind, middle);
                    if footprint(&warm, &cx, &token, &query, policy, None)
                        .0
                        .is_ok()
                    {
                        high = middle;
                    } else {
                        low = middle + 1;
                    }
                }
                assert!(low > 0, "nonempty count consumes {kind:?}");
                for value in [low - 1, low] {
                    let (token, policy) = limits(kind, value);
                    let expected = footprint(&warm, &cx, &token, &query, policy, None);
                    assert_eq!(expected.0.is_ok(), value == low);
                    // A new database has no admitted core, even if a refused
                    // predecessor happened to complete source admission.
                    let fresh = database(&commit).await;
                    assert_eq!(
                        footprint(&fresh, &cx, &token, &query, policy, None),
                        expected,
                        "{text}, {kind:?} at {value}"
                    );
                }
            }
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cached_full_counts_never_replace_scoped_or_fallible_inputs() {
    let ((), report) = run_async_under_lab(0xc161_0003, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let authority = issuer();
        let full = authority.issue_at(&grant(), 100).unwrap();
        for text in [VERTICES, R_EDGES, S_EDGES] {
            for _ in 0..2 {
                footprint(&db, &cx, &full, &prepare(text), policy(), None)
                    .0
                    .unwrap();
            }
        }
        for (restriction, expected) in [
            (Restriction::Labels(Scope::only([A])), [5, 4, 1]),
            (Restriction::Labels(Scope::only([B])), [1, 0, 0]),
            (Restriction::Relations(Scope::only([R])), [6, 5, 0]),
            (Restriction::Properties(Scope::only([])), [6, 5, 2]),
        ] {
            let token = full.attenuate(restriction).unwrap();
            for (text, expected) in [VERTICES, R_EDGES, S_EDGES].into_iter().zip(expected) {
                for _ in 0..2 {
                    let rows = footprint(&db, &cx, &token, &prepare(text), policy(), None)
                        .0
                        .unwrap();
                    assert_eq!(count(&rows), expected, "scoped {text}");
                }
            }
        }
        // The full-scope count cache must not hide a division exception in
        // a filtered input, even when its final result page is empty.
        let fallible = prepare("MATCH (n) WHERE 1 / n.p > 0 RETURN COUNT(*) AS n LIMIT 0");
        let result = db.execute_graph_aggregate_authorized(
            &cx,
            &authority,
            &full,
            "main",
            &fallible,
            policy(),
            || 100,
        );
        assert!(
            matches!(result, Err(QueryError::Aggregate(_))),
            "{result:?}"
        );
        let hidden = full
            .attenuate(Restriction::Properties(Scope::only([])))
            .unwrap();
        let masked = prepare("MATCH (n) WHERE 1 / n.p > 0 RETURN COUNT(*) AS n");
        let rows = db
            .execute_graph_aggregate_authorized(
                &cx,
                &authority,
                &hidden,
                "main",
                &masked,
                policy(),
                || 100,
            )
            .unwrap();
        assert_eq!(count(&rows), 0);
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn maintained_authorized_counts_admit_one_metadata_record_independent_of_graph_size() {
    let ((), report) = run_async_under_lab(0xc161_0005, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let authority = issuer();
        let full = authority.issue_at(&grant(), 100).unwrap();
        let mut baseline = None;
        for total in [1_u64, 32, 512] {
            let mut db =
                Database::open_memory(&commit, DatabaseKeys::new([0xc2; 32], NS, [0xc3; 32]))
                    .await
                    .unwrap();
            let mut batch = WriteBatch::new(R);
            for id in 1..=total {
                let id = u128::from(id);
                batch.create_vertex(VId(id), vec![A], vec![]);
                batch.add_edge(EId(id), VId(id), VId(id), vec![]);
            }
            db.write(&commit, batch).await.unwrap();
            let exact_nodes = full.attenuate(Restriction::MaxNodes(total)).unwrap();
            let short_nodes = full.attenuate(Restriction::MaxNodes(total - 1)).unwrap();
            let mut observed = Vec::new();
            for (text, expected) in [
                (VERTICES, total),
                (R_EDGES, total),
                (ALL_EDGES, total),
                (A_VERTICES, total),
                (S_EDGES, 0),
                ("MATCH (n:B) RETURN COUNT(*) AS n", 0),
            ] {
                let query = prepare(text);
                assert!(query.count_metadata_source().is_some(), "{text}");
                let owner = db
                    .execute_graph_aggregate_governed(&cx, &query, policy())
                    .unwrap();
                assert_eq!(count(&owner.value), expected, "{text}");
                assert_eq!(owner.rows.snapshot_records, 1, "{text}");
                // The same metadata input must fit the owner's exact native
                // work/scratch/record limits under current authorization.
                let exact = GqlQueryPolicy::new(
                    1,
                    owner.rows.result_rows,
                    owner.evaluator.work_units,
                    owner.evaluator.scratch_entries,
                );
                let result = footprint(&db, &cx, &exact_nodes, &query, exact, None);
                assert_eq!(result.0.as_ref().unwrap(), &owner.value, "{text}");
                observed.push((result.1, result.2));
                assert!(
                    matches!(
                        db.execute_graph_aggregate_authorized(
                            &cx,
                            &authority,
                            &short_nodes,
                            "main",
                            &query,
                            exact,
                            || 100,
                        ),
                        Err(QueryError::Authorization(fgdb_warden::Error::LimitExceeded(
                            fgdb_warden::LimitDimension::Nodes
                        )))
                    ),
                    "{text}, {total} vertices"
                );
            }
            if let Some(expected) = &baseline {
                assert_eq!(&observed, expected, "metadata work grew with {total} vertices");
            } else {
                baseline = Some(observed);
            }
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn exact_metadata_keeps_scope_history_and_live_permit_fences() {
    let ((), report) = run_async_under_lab(0xc161_0006, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let authority = issuer();
        let full = authority.issue_at(&grant(), 100).unwrap();
        let one_record = GqlQueryPolicy::new(1, 100, 1_000_000, 100_000);
        for (at, text) in [VERTICES, R_EDGES, ALL_EDGES, A_VERTICES]
            .into_iter()
            .enumerate()
        {
            let query = prepare(text);
            let current = db
                .execute_graph_aggregate_authorized(
                    &cx, &authority, &full, "main", &query, one_record, || 100,
                )
                .unwrap();
            assert_eq!(
                current,
                db.execute_graph_aggregate_governed(&cx, &query, one_record)
                    .unwrap()
                    .value
            );
            for (restriction, expected) in [
                (Restriction::Labels(Scope::only([A])), [5, 4, 5, 5]),
                (Restriction::Relations(Scope::only([R])), [6, 5, 5, 5]),
                (Restriction::Properties(Scope::only([])), [6, 5, 7, 5]),
            ] {
                let scoped = full.attenuate(restriction).unwrap();
                assert!(
                    matches!(
                        db.execute_graph_aggregate_authorized(
                            &cx, &authority, &scoped, "main", &query, one_record, || 100,
                        ),
                        Err(QueryError::Aggregate(GqlQueryError::Rows(error)))
                            if error.dimension == GqlBudgetDimension::SnapshotRecords
                    ),
                    "scoped source incorrectly used full metadata: {text}"
                );
                let rows = db
                    .execute_graph_aggregate_authorized(
                        &cx, &authority, &scoped, "main", &query, policy(), || 100,
                    )
                    .unwrap();
                assert_eq!(count(&rows), expected[at], "scoped {text}");
            }
            assert!(
                matches!(
                    db.execute_graph_aggregate_authorized_at(
                        &cx, &authority, &full, "main", &query, CommitSeq(1),
                        one_record, || 100,
                    ),
                    Err(QueryError::Aggregate(GqlQueryError::Rows(error)))
                        if error.dimension == GqlBudgetDimension::SnapshotRecords
                ),
                "historical source incorrectly used current metadata: {text}"
            );
            let history = db
                .execute_graph_aggregate_authorized_at(
                    &cx, &authority, &full, "main", &query, CommitSeq(1),
                    policy(), || 100,
                )
                .unwrap();
            assert_eq!(count(&history), [6, 6, 6, 5][at], "historical {text}");
            for (restriction, dimension) in [
                (Restriction::MaxNodes(5), fgdb_warden::LimitDimension::Nodes),
                (Restriction::MaxWork(0), fgdb_warden::LimitDimension::Work),
                (Restriction::MaxRows(0), fgdb_warden::LimitDimension::Rows),
            ] {
                let limited = full.attenuate(restriction).unwrap();
                assert!(
                    matches!(
                        db.execute_graph_aggregate_authorized(
                            &cx, &authority, &limited, "main", &query, one_record, || 100,
                        ),
                        Err(QueryError::Authorization(fgdb_warden::Error::LimitExceeded(found)))
                            if found == dimension
                    ),
                    "metadata bypassed {dimension:?}: {text}"
                );
            }
            let mut clocks = 0;
            assert!(
                matches!(
                    db.execute_graph_aggregate_authorized(
                        &cx, &authority, &full, "main", &query, one_record, || {
                            clocks += 1;
                            if clocks == 1 { 100 } else { 10_000 }
                        },
                    ),
                    Err(QueryError::Authorization(fgdb_warden::Error::Expired))
                ),
                "metadata bypassed a live expiry check: {text}"
            );
        }
        let fallible = prepare(
            "MATCH (n) RETURN 1 / (COUNT(*) - COUNT(*)) AS n LIMIT 0",
        );
        assert!(
            matches!(
                db.execute_graph_aggregate_authorized(
                    &cx, &authority, &full, "main", &fallible, one_record, || 100,
                ),
                Err(QueryError::Aggregate(_))
            ),
            "metadata must retain the native output error before LIMIT 0"
        );
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
