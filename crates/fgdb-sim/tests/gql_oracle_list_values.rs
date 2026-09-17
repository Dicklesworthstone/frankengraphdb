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
    GqlParameters, GqlQueryPolicy, GraphAggregateValue, GraphIntegerBinary, GraphIntegerExpression,
    GraphSymbol, GraphSymbolKind, PreparedGraphAggregateText,
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
                        GraphAggregateValue::Integer(value) => i64::try_from(value)
                            .map(int)
                            .map_err(|error| error.to_string()),
                        GraphAggregateValue::Count(value) => i64::try_from(value)
                            .map(int)
                            .map_err(|error| error.to_string()),
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

            // Both spellings must resolve to the same governed aggregation.
            let grouped = rows(
                &db,
                &cx,
                "MATCH (n) RETURN n.p AS p, collect(n.p) AS xs GROUP BY p",
                &params,
            );
            assert_eq!(
                grouped,
                rows(
                    &db,
                    &cx,
                    "MATCH (n) RETURN n.p AS p, collect(n.p) AS xs GROUP BY n.p",
                    &params,
                ),
                "GROUP BY alias and expression produce identical ordered rows"
            );
            // Alias resolution retains the same expression and public names;
            // it must not introduce a second template identity.
            let alias_template = PreparedGraphAggregateText::prepare(
                "MATCH (n) RETURN n.p AS p, collect(n.p) AS xs GROUP BY p",
                symbols,
            )
            .expect("alias template");
            let expression_template = PreparedGraphAggregateText::prepare(
                "MATCH (n) RETURN n.p AS p, collect(n.p) AS xs GROUP BY n.p",
                symbols,
            )
            .expect("expression template");
            assert_eq!(
                alias_template.canonical_template_bytes(),
                expression_template.canonical_template_bytes()
            );
            let other_key = PreparedGraphAggregateText::prepare(
                "MATCH (n) RETURN n.q AS p, collect(n.p) AS xs GROUP BY n.q",
                |kind, name| {
                    if kind == GraphSymbolKind::Property && name == "q" {
                        Some(GraphSymbol::Property(PropertyKeyId(2)))
                    } else {
                        symbols(kind, name)
                    }
                },
            )
            .expect("distinct grouping key template");
            assert_ne!(
                alias_template.canonical_template_bytes(),
                other_key.canonical_template_bytes()
            );
            assert!(
                db.query(
                    &cx,
                    "MATCH (n) RETURN n.p AS p, collect(n.p) AS xs GROUP BY absent",
                    &params,
                    symbols,
                    policy(),
                )
                .is_err(),
                "GROUP BY must reject a key outside the projected and pattern scopes"
            );
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

            // Evaluate size after collect; expected cardinality comes only
            // from the independent reference graph, never engine rows.
            let count = reference_collect(graph, false, false).len() as i64;
            assert!(count > 1, "anti-vacuity: grouping combines multiple inputs");
            assert_eq!(
                rows(
                    &db,
                    &cx,
                    "MATCH (n) RETURN size(collect(n.p)) AS len",
                    &params
                ),
                vec![vec![int(count)]],
                "size(collect) counts NULL-excluded members after grouping"
            );
            assert_eq!(
                canonical_sorted(rows(
                    &db,
                    &cx,
                    "MATCH (n) RETURN size(collect(n.p)) AS len, count(*) + 1 AS next_count, abs(sum(n.p)) AS magnitude, coalesce(max(n.p), 0) AS fallback GROUP BY n.p",
                    &params,
                )),
                canonical_sorted(expected_group_families(graph)),
                "scalar functions and arithmetic execute after grouping through Database::query"
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
                    "MATCH (n) WHERE n.p = 999 RETURN collect(n.p) AS xs, size(collect(n.p)) AS len, coalesce(max(n.p), 0) AS fallback",
                    &params
                ),
                vec![vec![list(vec![]), int(0), int(0)]],
                "empty group retains collect, size zero and coalesce fallback"
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
                        "MATCH (n) FOR SYSTEM_TIME AS OF SEQ {} RETURN collect(n.p) AS xs, size(collect(n.p)) AS len",
                        epochs[4].0
                    ),
                    &params
                ),
                vec![vec![
                    list(pre_members.clone()),
                    int(pre_members.len() as i64)
                ]],
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

/// Group reference vertices by property equality, treating missing as NULL.
/// COUNT(*) includes NULL rows; COLLECT/SUM/MAX ignore NULL arguments.
fn expected_group_families(graph: &ReferenceGraph) -> Vec<Vec<GraphValue>> {
    let mut groups = std::collections::BTreeMap::<CanonicalScalar, Vec<Option<i64>>>::new();
    for (_, vertex) in graph.iter_vertices() {
        let key = vertex
            .props
            .get(&P)
            .cloned()
            .unwrap_or(CanonicalScalar::Null);
        let value = match &key {
            CanonicalScalar::Int(value) => Some(*value),
            CanonicalScalar::Null => None,
            _ => unreachable!("integer/NULL fixture required"),
        };
        groups.entry(key).or_default().push(value);
    }
    groups
        .into_values()
        .map(|members| {
            let integers: Vec<i64> = members.iter().flatten().copied().collect();
            let sum = if integers.is_empty() {
                null()
            } else {
                int(integers.iter().sum::<i64>().abs())
            };
            vec![
                int(i64::try_from(integers.len()).expect("small fixture")),
                int(i64::try_from(members.len()).expect("small fixture") + 1),
                sum,
                int(integers.into_iter().max().unwrap_or(0)),
            ]
        })
        .collect()
}

/// Post-group scalar families through the real facade + with_output_projection:
/// size(collect), abs(sum), coalesce(max, 0), and arithmetic over count.
/// Proves the governed projection VM end to end, including the NULL-keyed
/// group (coalesce fallback 0, size 0).
#[test]
fn scalar_families_over_aggregates_match_independent_reference() {
    let graph_seed = 0x1A49_u64;
    let ((), report) = run_async_under_lab(graph_seed, move |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-senc-projection-{}-{graph_seed}",
            std::process::id()
        ));
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("oracle database");
        build(&mut db, &commit, graph_seed).await;
        drop(db);
        let coordinator = CommitCoordinator::open(&commit, &dir, capsule_keys())
            .await
            .expect("oracle coordinator");
        let full = replay(&commit, &coordinator)
            .await
            .expect("full stream replay")
            .database;
        let graph = full.graph(GRAPH, BRANCH).expect("oracle graph");
        drop(coordinator);
        let db = Database::open(&commit, &dir, keys())
            .await
            .expect("reopen query database after independent replay");
        let params = GqlParameters::new();

        // Facade template with every summary the projections reference; GROUP
        // BY n.p keeps the grouping key in evaluation column 0.
        let template = PreparedGraphAggregateText::prepare(
            "MATCH (n) RETURN n.p AS p, count(*) AS n, sum(n.p) AS s, max(n.p) AS m, collect(n.p) AS xs GROUP BY n.p",
            symbols,
        )
        .expect("grouped template");
        // Evaluation columns: key n.p = 0, count = 1, sum = 2, max = 3, collect = 4.
        let count_plus_one = GraphIntegerExpression::prepare_scalar(&[
            fgdb_gql::GraphIntegerOp::Column(1),
            fgdb_gql::GraphIntegerOp::Literal(Some(1)),
            fgdb_gql::GraphIntegerOp::Binary(GraphIntegerBinary::Add),
        ])
        .expect("count+1 program");
        let abs_sum = GraphIntegerExpression::prepare_scalar(&[
            fgdb_gql::GraphIntegerOp::Column(2),
            fgdb_gql::GraphIntegerOp::Unary(fgdb_gql::GraphIntegerUnary::Abs),
        ])
        .expect("abs(sum) program");
        let coalesce_max = GraphIntegerExpression::prepare_scalar(&[
            fgdb_gql::GraphIntegerOp::Column(3),
            fgdb_gql::GraphIntegerOp::Literal(Some(0)),
            fgdb_gql::GraphIntegerOp::Coalesce,
        ])
        .expect("coalesce(max,0) program");
        let projections = vec![
            fgdb_gql::GraphSetProjection::new(
                "len",
                fgdb_gql::GraphSetValue::Size(Box::new(fgdb_gql::GraphSetValue::Column(4))),
            ),
            fgdb_gql::GraphSetProjection::new(
                "count_plus_one",
                fgdb_gql::GraphSetValue::Integer(count_plus_one),
            ),
            fgdb_gql::GraphSetProjection::new(
                "absolute_sum",
                fgdb_gql::GraphSetValue::Integer(abs_sum),
            ),
            fgdb_gql::GraphSetProjection::new(
                "max_fallback",
                fgdb_gql::GraphSetValue::Integer(coalesce_max),
            ),
        ];
        let prepared = template
            .bind_parameters(&params)
            .expect("bind")
            .with_output_projection(projections)
            .expect("projection attaches");
        let projected: Vec<Vec<GraphValue>> = db
            .execute_graph_aggregate_governed(&cx, &prepared, policy())
            .expect("projected aggregate executes")
            .value
            .iter()
            .map(|row| {
                assert!(row.keys().is_empty(), "projected rows are keyless");
                assert_eq!(row.values().len(), 4, "four projected columns");
                let cell = |at: usize| match row.get(at).expect("projected cell") {
                    GraphAggregateValue::Integer(value) => {
                        int(i64::try_from(*value).expect("smoke values stay i64"))
                    }
                    GraphAggregateValue::Count(value) => {
                        int(i64::try_from(*value).expect("small fixture count"))
                    }
                    GraphAggregateValue::Value(GraphValue::Scalar(scalar)) => {
                        GraphValue::Scalar(scalar.clone())
                    }
                    other => unreachable!("expected integer projection, got {other:?}"),
                };
                vec![cell(0), cell(1), cell(2), cell(3)]
            })
            .collect();

        // INDEPENDENT oracle: group members from the reference graph only.
        let expected = expected_group_families(graph);
        assert!(
            expected.iter().any(|row| row[0] == int(0)),
            "anti-vacuity: NULL-keyed group exists (coalesce fallback exercised)"
        );
        assert!(
            expected.iter().any(|row| row[0] > int(1)),
            "anti-vacuity: a multi-member group exercises real aggregation"
        );
        assert_eq!(
            canonical_sorted(projected),
            canonical_sorted(expected),
            "size/count+1/abs(sum)/coalesce(max,0) per group vs independent oracle"
        );

        drop(db);
    });
    assert!(
        report.lab_test_passed(),
        "seed={graph_seed} report={report:?}"
    );
}
