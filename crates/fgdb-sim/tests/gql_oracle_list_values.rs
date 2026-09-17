//! Independent oracle differential for the native list values landed by
//! `fgdb-gql-list-values-zsw0` (list literals, `list[i]`, `size()`, `UNWIND`,
//! `collect()`/`collect(DISTINCT)`, list parameters).
//!
//! SEMANTICS CHECKED, restated from the zsw0 implementation's documented
//! contract (`crates/fgdb/tests/gql_list_values.rs` header and assertions):
//! - `collect(n.p)` retains deterministic input order — the engine's canonical
//!   vertex scan order (ascending VId) — and SKIPS NULL/missing properties;
//! - `collect(DISTINCT n.p)` keeps the FIRST occurrence of each value after
//!   the same NULL exclusion;
//! - `collect` over an empty match returns one row containing the empty list;
//! - GROUP BY keys group rows first; `collect` inside a group still skips
//!   NULL members (a NULL-keyed group collects an EMPTY list);
//! - `UNWIND` of `NULL` or `[]` produces ZERO rows (openCypher rule);
//! - `UNWIND` of a list with NULL elements yields one row per element, NULLs
//!   included (they are list ELEMENTS, not collected properties);
//! - indexes are zero-based; negative indexes count from the end; out-of-range
//!   and NULL-base indexes yield NULL; `size(NULL)` is NULL, `size([])` is 0.
//!
//! The oracle side is an INDEPENDENT evaluator over
//! `fgdb_reference::ReferenceGraph` (vertices read off `vertex.props` in
//! canonical VId order via `iter_vertices`); it never calls fgdb-gql
//! evaluation code. Histories go through the real `Database` write path with
//! NULL/missing properties and a delete; temporal rows are compared against
//! `replay_through` prefix state. One lab run per graph seed; no claim about
//! maps, stored list properties, or comprehensions (the bead's No-claim).

use asupersync::fs::Vfs;
use asupersync::lab::run_async_under_lab;
use fgdb::{CAPSULE_OBJECT_KIND, Database, DatabaseKeys, QueryResult, WriteBatch};
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::CommitCoordinator;
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphAggregateValue, GraphSymbol, GraphSymbolKind,
    PreparedGraphAggregateText,
};
use fgdb_reference::ReferenceGraph;
use fgdb_sim::{replay, replay_through};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, CanonicalScalar, CommitCx, CommitSeq, GraphId, QueryCx, VId};

const GRAPH: GraphId = GraphId(1);
const BRANCH: BranchId = BranchId(1);
const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const K_OID: [u8; 32] = [0x6b; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x78; 32]);
const DEK: [u8; 32] = [0x3d; 32];

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, DEK)
}

fn capsule_keys() -> CapsuleKeys {
    CapsuleKeys::new(
        K_OID,
        NAMESPACE,
        DEK,
        CAPSULE_OBJECT_KIND,
        CapsuleProfile::balanced(),
    )
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        _ => None,
    }
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}

fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

fn null() -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Null)
}

fn list(values: Vec<GraphValue>) -> GraphValue {
    GraphValue::List(values.into_boxed_slice())
}

/// One unit per commit. VId(1)/VId(5) duplicate the value 3; VId(3) carries an
/// explicit NULL, VId(4) has no `p` at all; unit 5 deletes VId(5) so the
/// frontier's collected list loses one element the AS OF prefix still has.
fn units() -> Vec<fn(&mut WriteBatch)> {
    vec![
        |b| {
            b.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(3))]);
        },
        |b| {
            b.create_vertex(VId(2), vec![], vec![(P, CanonicalScalar::Int(1))]);
        },
        |b| {
            b.create_vertex(VId(3), vec![], vec![(P, CanonicalScalar::Null)]);
        },
        |b| {
            b.create_vertex(VId(4), vec![], vec![]);
        },
        |b| {
            b.create_vertex(VId(5), vec![], vec![(P, CanonicalScalar::Int(3))]);
        },
        |b| {
            b.delete_vertex(VId(5));
        },
    ]
}

async fn build<V: Vfs + Clone>(db: &mut Database<V>, cx: &CommitCx, seed: u64) -> Vec<CommitSeq> {
    let mut epochs = Vec::new();
    let mut random = seed;
    for (index, unit) in units().into_iter().enumerate() {
        let mut batch = WriteBatch::new(R);
        unit(&mut batch);
        if index == 0 {
            // Two equal non-NULL members survive the delete. Their value and
            // additional members vary with the seed, independently of lab scheduling.
            let duplicate = 10 + (seed % 97) as i64;
            for vid in 6..10 + seed % 4 {
                random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
                let value = if vid < 8 {
                    duplicate
                } else {
                    110 + (random % 17) as i64
                };
                batch.create_vertex(
                    VId(u128::from(vid)),
                    vec![],
                    vec![(P, CanonicalScalar::Int(value))],
                );
            }
        }
        epochs.push(db.write(cx, batch).await.expect("oracle history commit"));
    }
    epochs
}

/// Engine rows, every cell a typed GraphValue.
fn rows<V: Vfs + Clone>(
    db: &Database<V>,
    cx: &QueryCx,
    text: &str,
    params: &GqlParameters,
) -> Vec<Vec<GraphValue>> {
    let result = db.query(cx, text, params, symbols, policy());
    assert!(result.is_ok(), "query={text}: {result:?}");
    let result = result.expect("query succeeded");
    let converted: Result<Vec<Vec<GraphValue>>, String> = match result {
        QueryResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|cell| match cell {
                        GraphAggregateValue::Value(value) => Ok(value),
                        other => Err(format!("expected value cell, got {other:?}")),
                    })
                    .collect()
            })
            .collect(),
        other => Err(format!("expected rows, got {other:?}")),
    };
    assert!(converted.is_ok(), "query={text}: {converted:?}");
    converted.expect("row conversion succeeded")
}

/// INDEPENDENT evaluation over the reference graph: collected `p` values of
/// live vertices in canonical VId order, skipping NULL/missing.
/// `distinct` keeps first occurrences. `keep_nulls` is the planted-negative
/// lever only: it retains NULL/missing members as NULL cells.
fn reference_collect(graph: &ReferenceGraph, distinct: bool, keep_nulls: bool) -> Vec<GraphValue> {
    let mut out: Vec<GraphValue> = Vec::new();
    for (_vid, vertex) in graph.iter_vertices() {
        let value = vertex
            .props
            .get(&P)
            .cloned()
            .unwrap_or(CanonicalScalar::Null);
        if value == CanonicalScalar::Null && !keep_nulls {
            continue;
        }
        let cell = GraphValue::Scalar(value);
        if !distinct || !out.contains(&cell) {
            out.push(cell);
        }
    }
    out
}

fn reference_vertices(graph: &ReferenceGraph) -> Vec<VId> {
    graph.iter_vertices().map(|(vid, _)| vid).collect()
}

/// Independent UNWIND + MATCH {p: x} join: for each parameter element in
/// order, the live vertices whose `p` equals it (canonical VId order within
/// one element; duplicates per element occurrence).
fn reference_unwind_match(graph: &ReferenceGraph, elements: &[i64]) -> Vec<VId> {
    let mut out = Vec::new();
    for element in elements {
        for vid in reference_vertices(graph) {
            let vertex = graph.vertex(vid).expect("iterated vertex");
            if matches!(vertex.props.get(&P), Some(CanonicalScalar::Int(value)) if value == element)
            {
                out.push(vid);
            }
        }
    }
    out
}

fn reference_rows_vids(values: &[VId]) -> Vec<Vec<GraphValue>> {
    values
        .iter()
        .map(|vid| vec![GraphValue::Vertex(*vid)])
        .collect()
}

/// The oracle's expected `collect`/`collect(DISTINCT)` row pair.
/// `keep_nulls: true` is the planted-negative lever ONLY — it makes the
/// evaluator retain NULL/missing members, which must break the differential.
fn expected_collect_rows(graph: &ReferenceGraph, keep_nulls: bool) -> Vec<Vec<GraphValue>> {
    vec![vec![
        list(reference_collect(graph, false, keep_nulls)),
        list(reference_collect(graph, true, keep_nulls)),
    ]]
}

/// Sort rows by canonical bytes — the test-side normalizer used where the
/// engine's group ordering is not itself under test.
fn canonical_sorted(mut rows: Vec<Vec<GraphValue>>) -> Vec<Vec<u8>> {
    let mut encoded: Vec<Vec<u8>> = rows
        .iter_mut()
        .map(|row| {
            row.iter()
                .map(|cell| cell.canonical_bytes().expect("oracle cells encode"))
                .collect::<Vec<_>>()
                .concat()
        })
        .collect();
    encoded.sort();
    encoded
}

#[test]
fn list_value_families_match_independent_reference_across_seeds() {
    for graph_seed in [0x1A49_u64, 0x1A4A, 0x1A4B, 0x1A4C] {
        let ((), report) = run_async_under_lab(graph_seed, move |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let dir = std::env::temp_dir().join(format!(
                "fgdb-oracle-list-values-{}-{graph_seed}",
                std::process::id()
            ));
            let mut db = Database::create(&commit, &dir, keys())
                .await
                .expect("oracle database");
            let epochs = build(&mut db, &commit, graph_seed).await;
            drop(db);

            // Independent state: full stream, and the prefix through the last
            // pre-delete epoch (the delete is the sixth and final unit).
            let coordinator = CommitCoordinator::open(&commit, &dir, capsule_keys())
                .await
                .expect("oracle coordinator");
            let full = replay(&commit, &coordinator)
                .await
                .expect("full stream replay")
                .database;
            let graph = full.graph(GRAPH, BRANCH).expect("oracle graph");
            let prefix = replay_through(&commit, &coordinator, epochs[4])
                .await
                .expect("prefix replay")
                .database;
            let pre_graph = prefix.graph(GRAPH, BRANCH).expect("prefix graph");
            drop(coordinator);
            let db = Database::open(&commit, &dir, keys())
                .await
                .expect("reopen query database after independent replay");

            let params = GqlParameters::new();
            let live = reference_vertices(graph);

            let expected = expected_collect_rows(graph, false);
            assert_eq!(
                rows(
                    &db,
                    &cx,
                    "MATCH (n) RETURN collect(n.p) AS xs, collect(DISTINCT n.p) AS dx",
                    &params
                ),
                expected,
                "collect at frontier retains VId input order and excludes NULL"
            );
            assert_ne!(
                expected[0][0], expected[0][1],
                "anti-vacuity: surviving duplicates distinguish collect from DISTINCT"
            );

            // Grouped collect, compared as canonical multisets (group ROW
            // order is an engine canonical-order question, not a list one).
            // The `db.query` facades reject GROUP BY (their aggregate pipeline
            // template demands WITH/UNWIND), so grouped aggregation goes
            // through the dedicated PreparedGraphAggregateText facade; its
            // GROUP BY binds pattern expressions (n.p), not RETURN aliases.
            let grouped_template = PreparedGraphAggregateText::prepare(
                "MATCH (n) RETURN n.p AS p, collect(n.p) AS xs GROUP BY n.p",
                symbols,
            )
            .expect("grouped aggregate template");
            let grouped_query = grouped_template
                .bind_parameters(&params)
                .expect("grouped aggregate bind");
            let grouped: Vec<Vec<GraphValue>> = db
                .execute_graph_aggregate_governed(&cx, &grouped_query, policy())
                .expect("grouped aggregate executes")
                .value
                .iter()
                .map(|row| {
                    vec![
                        row.keys()[0].clone(),
                        match row.get(0).expect("collect cell") {
                            GraphAggregateValue::Value(value) => value.clone(),
                            other => panic!("collect yields values, got {other:?}"),
                        },
                    ]
                })
                .collect();
            // Distinct group keys over live vertices: NULL for missing/explicit
            // NULL, plus each distinct int value.
            let mut keys: Vec<CanonicalScalar> = live
                .iter()
                .map(|vid| {
                    graph
                        .vertex(*vid)
                        .expect("live vertex")
                        .props
                        .get(&P)
                        .cloned()
                        .unwrap_or(CanonicalScalar::Null)
                })
                .collect();
            keys.sort_by(|a, b| {
                a.encode()
                    .expect("keys encode")
                    .cmp(&b.encode().expect("keys encode"))
            });
            keys.dedup();
            let mut expected_grouped: Vec<Vec<GraphValue>> = Vec::new();
            for key in &keys {
                let members: Vec<GraphValue> = live
                    .iter()
                    .filter(|vid| {
                        graph
                            .vertex(**vid)
                            .expect("vertex")
                            .props
                            .get(&P)
                            .cloned()
                            .unwrap_or(CanonicalScalar::Null)
                            == *key
                    })
                    .filter_map(|vid| {
                        match graph.vertex(*vid).expect("vertex").props.get(&P) {
                            Some(CanonicalScalar::Int(value)) => Some(int(*value)),
                            _ => None, // collect skips NULL/missing members
                        }
                    })
                    .collect();
                expected_grouped.push(vec![GraphValue::Scalar(key.clone()), list(members)]);
            }
            assert!(
                expected_grouped
                    .iter()
                    .any(|row| matches!(&row[1], GraphValue::List(values) if values.len() > 1)),
                "anti-vacuity: a non-NULL group has multiple collected members"
            );
            assert!(
                expected_grouped
                    .iter()
                    .any(|row| row[0] == null() && row[1] == list(vec![])),
                "anti-vacuity: NULL-keyed group exists and collects no NULL members"
            );
            assert_eq!(
                canonical_sorted(grouped),
                canonical_sorted(expected_grouped),
                "GROUP BY collect vs independent grouping (NULL group collects empty)"
            );

            // UNWIND $list AS x MATCH (n {p: x}): duplicates preserved.
            let elements = [3, 1, 3];
            let list_params = GqlParameters::new()
                .with_list("xs", elements.iter().copied().map(int).collect())
                .expect("list parameter");
            assert_eq!(
                rows(
                    &db,
                    &cx,
                    "UNWIND $xs AS x MATCH (n {p: x}) RETURN n",
                    &list_params
                ),
                reference_rows_vids(&reference_unwind_match(graph, &elements)),
                "UNWIND parameter join preserves element multiplicity and order"
            );

            // UNWIND literal lists: nested, NULL elements, empty, NULL base.
            assert_eq!(
                rows(&db, &cx, "UNWIND [[1,2],[3],NULL] AS x RETURN x", &params),
                vec![
                    vec![list(vec![int(1), int(2)])],
                    vec![list(vec![int(3)])],
                    vec![null()],
                ],
                "UNWIND over nested literals yields each sub-list, NULL included"
            );
            assert_eq!(
                rows(&db, &cx, "UNWIND [1,NULL,1] AS x RETURN x", &params),
                vec![vec![int(1)], vec![null()], vec![int(1)]],
                "UNWIND yields NULL elements as rows"
            );
            assert!(
                rows(&db, &cx, "UNWIND [] AS x RETURN x", &params).is_empty(),
                "anti-vacuity: empty UNWIND produces zero rows"
            );
            assert!(
                rows(&db, &cx, "UNWIND NULL AS x RETURN x", &params).is_empty(),
                "NULL UNWIND produces zero rows"
            );

            // `size` over LISTS (literals and WITH-bound lists) through the
            // db.query facade; `size(collect(...))` is NOT a supported
            // composition (the aggregate facades reject a scalar function
            // wrapping an aggregate), so the collect-length family is verified
            // by taking the length of the returned list itself. The literal
            // cases are the bead's size(NULL)/size([])/NULL-element contract.
            let collected = rows(&db, &cx, "MATCH (n) RETURN collect(n.p) AS xs", &params);
            assert_eq!(
                collected,
                vec![vec![list(reference_collect(graph, false, false))]],
                "collect returns the NULL-excluded list the oracle counts"
            );
            let count = reference_collect(graph, false, false).len() as i64;
            assert!(
                count >= 1,
                "anti-vacuity: collected list is non-empty before size checks"
            );
            assert_eq!(
                rows(
                    &db,
                    &cx,
                    "MATCH (n) WHERE n.p = 1 WITH [7,NULL,[2,3]] AS xs RETURN size(xs) AS length, size([1,2,3]) AS a, size([]) AS b, size(NULL) AS c, size([1,NULL]) AS d",
                    &params
                ),
                vec![vec![int(3), int(3), int(0), null(), int(2)]],
                "size of WITH-bound list and literals: NULL base NULL, empty 0"
            );
            assert_eq!(
                rows(
                    &db,
                    &cx,
                    "MATCH (n) WHERE n.p = 999 RETURN collect(n.p) AS xs",
                    &params
                ),
                vec![vec![list(vec![])]],
                "empty match collects empty list (aggregate RETURN stays alone)"
            );
            assert_eq!(
                rows(
                    &db,
                    &cx,
                    "MATCH (n) WHERE n.p = 1 RETURN size([1,2,3]) AS a, size([]) AS b, size(NULL) AS c, size([1,NULL]) AS d",
                    &params
                ),
                vec![vec![int(3), int(0), null(), int(2)]],
                "size literals: empty 0, NULL base NULL, NULL element counts"
            );

            // Indexing: in-range, negative, out-of-range, NULL base.
            assert_eq!(
                rows(
                    &db,
                    &cx,
                    "MATCH (n) WHERE n.p = 1 RETURN [10,20,30][0] AS a, [10,20,30][2] AS b, [10,20,30][-1] AS c, [10,20,30][-3] AS d, [10,20,30][3] AS e, [10,20,30][-4] AS f, NULL[0] AS g",
                    &params
                ),
                vec![vec![
                    int(10),
                    int(30),
                    int(30),
                    int(10),
                    null(),
                    null(),
                    null()
                ]],
                "zero-based, negative-from-end, out-of-range NULL, NULL-base NULL"
            );

            // The prefix still has VId(5); deleting it removes one occurrence
            // of 3 while retaining the other equal member and generated rows.
            let pre_members = reference_collect(pre_graph, false, false);
            assert_eq!(
                pre_members.iter().filter(|value| **value == int(3)).count(),
                2,
                "anti-vacuity: prefix contains two copies of the deleted value"
            );
            assert_eq!(
                rows(
                    &db,
                    &cx,
                    &format!(
                        "MATCH (n) FOR SYSTEM_TIME AS OF SEQ {} RETURN collect(n.p) AS xs",
                        epochs[4].0
                    ),
                    &params
                ),
                vec![vec![list(pre_members.clone())]],
                "AS OF prefix collect retains the to-be-deleted member"
            );
            assert_ne!(
                pre_members.len(),
                reference_collect(graph, false, false).len(),
                "anti-vacuity: the delete must actually change the collected list"
            );
        });
        assert!(
            report.lab_test_passed(),
            "seed={graph_seed} report={report:?}"
        );
    }
}
