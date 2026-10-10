//! `CALL fnx.*` inside a native GQL read. Prism supplies the rows at the
//! read's own snapshot; WHERE, WITH, RETURN, ORDER BY and LIMIT compose over
//! them. The oracle is the standalone `call_fnx` over an explicitly spelled
//! projection, sorted and cut by this file, not by the engine under test.

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{
    Database, DatabaseKeys, GqlError, MemVfs, ProcedureError, QueryError, QueryResult, QueryValue,
    WriteBatch,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSetExecutionError, GraphSymbol,
    GraphSymbolKind,
};
use fgdb_prism::*;
use fgdb_types::{
    CanonicalF64, CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId,
};
use std::cmp::Ordering;

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x3a; 32],
        DatabaseSecurityNamespaceId([0x3b; 32]),
        [0x3c; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000_000, 100_000, 100_000_000, 10_000_000)
}

/// 1->2->3->1 is a directed triangle, 3->4->5 a tail and 6 is isolated. No
/// pair is reciprocal, so the undirected reading is a simple graph too.
fn graph() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    for id in 1..=6_i64 {
        batch.create_vertex(
            VId(id as u128),
            vec![PERSON],
            vec![(P, CanonicalScalar::Int(id * 10))],
        );
    }
    for (edge, source, target) in [(1, 1, 2), (2, 2, 3), (3, 3, 1), (4, 3, 4), (5, 4, 5)] {
        batch.add_edge(EId(edge), VId(source), VId(target), vec![]);
    }
    batch
}

/// Every registered procedure: name, arguments, value output, and whether
/// its signature law reads the graph undirected.
const CALLS: [(&str, &str, &str, bool); 14] = [
    ("pagerank", "", "score", false),
    ("single_source_shortest_path_length", "1", "distance", false),
    ("connected_components", "", "component", true),
    ("weakly_connected_components", "", "component", false),
    ("strongly_connected_components", "", "component", false),
    ("single_source_dijkstra_path_length", "1", "distance", false),
    ("triangles", "", "triangles", true),
    ("clustering_coefficient", "", "score", true),
    // The franken_networkx catalog itself (fgdb-lq1v6).
    ("degree_centrality", "", "score", true),
    ("closeness_centrality", "", "score", true),
    ("harmonic_centrality", "", "score", true),
    ("betweenness_centrality", "", "score", true),
    ("eigenvector_centrality", "", "score", true),
    ("core_number", "", "core", true),
];

/// The whole graph spelled out field by field, independent of
/// `FnxReadOptions::whole_graph_for`.
fn explicit(undirected: bool) -> FnxReadOptions {
    FnxReadOptions {
        as_of: None,
        selection: FnxSelection {
            vertex_label: None,
            relation: None,
            weight: FnxWeightSpec::Unit,
        },
        projection: ProjectionSpec {
            directedness: if undirected {
                Directedness::Undirected
            } else {
                Directedness::Directed
            },
            parallel_edges: ParallelEdgePolicy::Reject,
            self_loops: SelfLoopPolicy::Keep,
        },
        source_limits: FnxSourceLimits {
            max_work_units: 1_000_000,
            max_scratch_entries: 1_000_000,
            max_staging_bytes: 1 << 24,
        },
        projection_limits: ProjectionLimits {
            max_vertices: 1_000,
            max_input_edges: 10_000,
            max_adjacency_entries: 20_000,
            max_workspace_bytes: 1 << 24,
        },
        execution_limits: FnxExecutionLimits {
            max_iterations: 1_000,
            max_result_rows: 1_000,
            max_estimated_work: 1 << 30,
        },
    }
}

fn value(value: FnxValue) -> GraphValue {
    match value {
        FnxValue::Vertex(vertex) => GraphValue::Vertex(vertex),
        FnxValue::Integer(count) => {
            GraphValue::Scalar(CanonicalScalar::Int(i64::try_from(count).unwrap()))
        }
        FnxValue::Score(v) | FnxValue::Float(v) => {
            GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(v)))
        }
    }
}

/// This file's own order for one column's domain: counts, finite floats and
/// vertex identities. A mixed pair has no order and fails the sort.
fn compare(left: &GraphValue, right: &GraphValue) -> Option<Ordering> {
    match (left, right) {
        (GraphValue::Vertex(a), GraphValue::Vertex(b)) => Some(a.0.cmp(&b.0)),
        (
            GraphValue::Scalar(CanonicalScalar::Int(a)),
            GraphValue::Scalar(CanonicalScalar::Int(b)),
        ) => Some(a.cmp(b)),
        (
            GraphValue::Scalar(CanonicalScalar::Float(a)),
            GraphValue::Scalar(CanonicalScalar::Float(b)),
        ) => Some(a.get().total_cmp(&b.get())),
        _ => None,
    }
}

/// Columns and plain cells. A non-row result reads as one impossible column
/// name, so the caller's column assertion reports it.
fn rows(result: QueryResult) -> (Vec<String>, Vec<Vec<GraphValue>>) {
    let QueryResult::Rows { columns, rows } = result else {
        return (vec!["<not a row result>".to_owned()], Vec::new());
    };
    let rows = rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|cell| match cell {
                    QueryValue::Value(value) => Some(value),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()
                .expect("every cell is a plain value")
        })
        .collect();
    (columns, rows)
}

fn procedure_error(error: QueryError) -> Result<ProcedureError, QueryError> {
    match error {
        QueryError::Set(GqlQueryError::Source(GraphSetExecutionError::Source(
            GqlError::Procedure(error),
        ))) => Ok(error),
        other => Err(other),
    }
}
async fn open(commit: &fgdb_types::CommitCx) -> Database<MemVfs> {
    let mut db = Database::<MemVfs>::open_memory(commit, keys())
        .await
        .unwrap();
    db.write(commit, graph()).await.unwrap();
    db
}

fn run<T>(test: impl AsyncFnOnce(&fgdb_types::CommitCx, &QueryCx) -> T) -> T {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let commit = contexts.commit();
    let cx = contexts.query();
    runtime.block_on(test(&commit, &cx))
}

#[test]
fn every_registered_procedure_composes_and_equals_the_standalone_call() {
    // The table is the whole registry: a new row must join this law.
    let registered: Vec<&str> = FnxSignatureRegistry::signatures()
        .iter()
        .map(|signature| signature.name)
        .collect();
    let listed: Vec<String> = CALLS
        .iter()
        .map(|(name, ..)| format!("fnx.{name}"))
        .collect();
    assert_eq!(listed, registered);
    run_transaction(async |commit, cx, txcx| {
        let db = open(commit).await;
        let txn = db.begin(txcx).unwrap();
        for (name, arguments, field, undirected) in CALLS {
            let standalone = db
                .call_fnx(
                    cx,
                    &format!("CALL fnx.{name}({arguments}) YIELD vertex, {field}"),
                    &FnxParameters::new(),
                    explicit(undirected),
                )
                .map_err(|error| format!("{name}: {error}"))
                .unwrap();
            let mut expected: Vec<Vec<GraphValue>> = standalone
                .analytics
                .rows
                .into_iter()
                .map(|row| row.into_iter().map(value).collect())
                .collect();
            assert!(expected.len() >= 4, "{name}: {} rows", expected.len());
            expected.sort_by(|a, b| {
                let by_value = compare(&b[1], &a[1]).expect("one value domain");
                by_value.then_with(|| compare(&a[0], &b[0]).expect("vertices"))
            });
            expected.truncate(4);

            let text = format!(
                "CALL fnx.{name}({arguments}) YIELD vertex, {field} AS value \
                 RETURN vertex, value ORDER BY value DESC, vertex LIMIT 4"
            );
            let (columns, actual) = rows(
                db.query(cx, &text, &GqlParameters::new(), symbols, policy())
                    .map_err(|error| format!("{name}: {error}"))
                    .unwrap(),
            );
            assert_eq!(columns, ["vertex", "value"], "{name}");
            assert_eq!(actual, expected, "{name}");
            let (columns, actual) = rows(
                txn.query(&db, cx, &text, &GqlParameters::new(), symbols, policy())
                    .map_err(|error| format!("transaction {name}: {error}"))
                    .unwrap(),
            );
            assert_eq!(columns, ["vertex", "value"], "transaction {name}");
            assert_eq!(actual, expected, "transaction {name}");
        }
        txn.abort();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

/// fgdb-luq0b: the README's analytics shape, `YIELD node, score RETURN
/// node.p, score ORDER BY score DESC LIMIT k`, runs as written. `node` names
/// the vertex output, and `node.p` reads through the implied identity match.
/// A scalar output read or matched as a vertex refuses at its YIELD site.
#[test]
fn a_readme_shaped_call_reads_vertex_properties_with_no_match() {
    run(async |commit, cx| {
        let db = open(commit).await;
        let query = |text: &str| db.query(cx, text, &GqlParameters::new(), symbols, policy());
        // Independent: the standalone call's (vertex, score), with p = 10 * id.
        let standalone = db
            .call_fnx(
                cx,
                "CALL fnx.pagerank() YIELD vertex, score",
                &FnxParameters::new(),
                explicit(false),
            )
            .unwrap();
        let mut expected: Vec<Vec<GraphValue>> = standalone
            .analytics
            .rows
            .into_iter()
            .map(|row| match row.as_slice() {
                [FnxValue::Vertex(vertex), score] => vec![
                    GraphValue::Scalar(CanonicalScalar::Int(vertex.0 as i64 * 10)),
                    value(*score),
                ],
                _ => Vec::new(),
            })
            .collect();
        expected.sort_by(|a, b| compare(&a[0], &b[0]).expect("integer p"));
        assert_eq!(expected.len(), 6);
        let (columns, actual) = rows(
            query("CALL fnx.pagerank() YIELD node, score RETURN node.p AS p, score ORDER BY p")
                .unwrap(),
        );
        assert_eq!(columns, ["p", "score"]);
        assert_eq!(actual, expected);
        // The README spelling equals the MATCH it implies.
        assert_eq!(
            query("CALL fnx.pagerank() YIELD node, score RETURN node.p, score ORDER BY score DESC LIMIT 3")
                .unwrap(),
            query(
                "CALL fnx.pagerank() YIELD vertex AS node, score MATCH (node) \
                 RETURN node.p, score ORDER BY score DESC LIMIT 3"
            )
            .unwrap()
        );
        for (text, site) in [
            ("CALL fnx.pagerank() YIELD vertex, score RETURN score.p", 1),
            (
                "CALL fnx.pagerank() YIELD score AS s, vertex MATCH (s) RETURN s",
                0,
            ),
        ] {
            let error = procedure_error(query(text).unwrap_err()).unwrap();
            assert!(
                matches!(
                    error,
                    ProcedureError::Bind(FnxCallError {
                        site: FnxCallSite::Yield(at),
                        kind: FnxBindErrorKind::Expected(_),
                    }) if at == site
                ),
                "{text}: {error:?}"
            );
        }
    });
}

#[test]
fn a_filter_after_the_call_sees_every_yielded_row() {
    run(async |commit, cx| {
        let db = open(commit).await;
        let (_, actual) = rows(
            db.query(
                cx,
                "CALL fnx.single_source_shortest_path_length(1) YIELD vertex, distance \
                 WHERE distance >= 2 RETURN vertex, distance ORDER BY vertex",
                &GqlParameters::new(),
                symbols,
                policy(),
            )
            .unwrap(),
        );
        let int = |v: i64| GraphValue::Scalar(CanonicalScalar::Int(v));
        assert_eq!(
            actual,
            vec![
                vec![GraphValue::Vertex(VId(3)), int(2)],
                vec![GraphValue::Vertex(VId(4)), int(3)],
                vec![GraphValue::Vertex(VId(5)), int(4)],
            ]
        );
    });
}

#[test]
fn a_parameter_argument_binds_through_the_same_typed_resolution() {
    run(async |commit, cx| {
        let db = open(commit).await;
        let text = "CALL fnx.single_source_shortest_path_length($source, $cutoff) \
                    YIELD vertex RETURN vertex ORDER BY vertex";
        let params = GqlParameters::new()
            .with_int64("source", 3)
            .unwrap()
            .with_int64("cutoff", 1)
            .unwrap();
        let (_, actual) = rows(db.query(cx, text, &params, symbols, policy()).unwrap());
        assert_eq!(
            actual,
            [1, 3, 4]
                .map(|id| vec![GraphValue::Vertex(VId(id))])
                .to_vec()
        );
    });
}

#[test]
fn a_read_session_runs_the_call_at_its_own_snapshot_not_the_writer_head() {
    run(async |commit, cx| {
        let mut db = open(commit).await;
        let old = db.read_session().unwrap();
        let text = "CALL fnx.single_source_shortest_path_length(1) YIELD vertex \
                    RETURN vertex ORDER BY vertex";
        let before = old
            .query(cx, text, &GqlParameters::new(), symbols, policy())
            .unwrap();
        let mut more = WriteBatch::new(R);
        more.add_edge(EId(6), VId(5), VId(6), vec![]);
        db.write(commit, more).await.unwrap();
        let (_, head) = rows(
            db.query(cx, text, &GqlParameters::new(), symbols, policy())
                .unwrap(),
        );
        // Control: the writer head reaches the newly linked vertex 6.
        assert_eq!(head.len(), 6);
        let (_, pinned) = rows(
            old.query(cx, text, &GqlParameters::new(), symbols, policy())
                .unwrap(),
        );
        assert_eq!(pinned.len(), 5);
        assert_eq!(rows(before).1, pinned);
    });
}

#[test]
fn a_certified_call_replays_byte_identically_after_later_commits() {
    run(async |commit, cx| {
        let mut db = open(commit).await;
        let text = "CALL fnx.pagerank() YIELD vertex, score \
                    RETURN vertex, score ORDER BY score DESC, vertex";
        let params = GqlParameters::new();
        let (result, certificate) = db
            .execute_certified(cx, text, &params, symbols, policy())
            .unwrap();
        let mut more = WriteBatch::new(R);
        more.add_edge(EId(6), VId(6), VId(1), vec![]);
        db.write(commit, more).await.unwrap();
        // Control: the new edge moves the live ranking.
        assert_ne!(
            db.query(cx, text, &params, symbols, policy()).unwrap(),
            result
        );
        assert_eq!(
            db.replay(cx, &certificate, &params, symbols, policy())
                .unwrap(),
            result
        );
    });
}

/// fgdb-3b1v7: the query certificate carries Prism's own certificate for
/// each CALL, so a replay under a different kernel refuses even if its rows
/// happen to match. A read without CALL keeps its v1 certificate bytes.
#[test]
fn a_call_certificate_folds_in_prism_evidence_and_replay_checks_it() {
    run(async |commit, cx| {
        let db = open(commit).await;
        let params = GqlParameters::new();
        let (_, certificate) = db
            .execute_certified(
                cx,
                "CALL fnx.pagerank() YIELD vertex, score RETURN vertex, score ORDER BY vertex",
                &params,
                symbols,
                policy(),
            )
            .unwrap();
        // Independent: the standalone call over the same projection at the
        // same sequence certifies the same call, kernel, result and witness.
        let standalone = db
            .call_fnx(
                cx,
                "CALL fnx.pagerank() YIELD vertex, score",
                &FnxParameters::new(),
                explicit(false),
            )
            .unwrap()
            .analytics
            .certificate;
        assert_eq!(certificate.procedures.len(), 1);
        let evidence = certificate.procedures[0];
        assert_eq!(
            evidence,
            fgdb::NativeProcedureEvidence::from_fnx(&standalone)
        );
        assert_eq!(evidence.call_digest, standalone.call_digest);
        let bytes = certificate.canonical_bytes();
        assert_eq!(
            bytes[33], 2,
            "a read that ran a procedure is the v2 envelope"
        );
        assert_eq!(
            fgdb::NativeResultCertificate::decode(&bytes).unwrap(),
            certificate
        );
        // The kernel digest names the executing kernel: pagerank's in-house
        // kernel and betweenness's foundation kernel certify differently.
        let (_, foundation) = db
            .execute_certified(
                cx,
                "CALL fnx.betweenness_centrality() YIELD vertex, score RETURN vertex, score",
                &params,
                symbols,
                policy(),
            )
            .unwrap();
        assert_ne!(
            foundation.procedures[0].kernel_digest,
            evidence.kernel_digest
        );
        assert!(
            db.replay(cx, &certificate, &params, symbols, policy())
                .is_ok()
        );
        // A different kernel identity refuses typed, before the rows count.
        let mut kernel = certificate.clone();
        kernel.procedures[0].kernel_digest.0[0] ^= 1;
        assert_eq!(
            db.replay(cx, &kernel, &params, symbols, policy()),
            Err(fgdb::ReplayRefusal::ProcedureKernelMismatch { index: 0 })
        );
        // Any other evidence drift, or a missing procedure, refuses too.
        let mut witness = certificate.clone();
        witness.procedures[0].evidence_digest.0[0] ^= 1;
        assert_eq!(
            db.replay(cx, &witness, &params, symbols, policy()),
            Err(fgdb::ReplayRefusal::ProcedureMismatch { index: 0 })
        );
        let mut missing = certificate.clone();
        missing.procedures.clear();
        assert_eq!(
            db.replay(cx, &missing, &params, symbols, policy()),
            Err(fgdb::ReplayRefusal::ProcedureMismatch { index: 0 })
        );
        // Control: a read without CALL certifies no procedure (v1 bytes).
        let (_, plain) = db
            .execute_certified(cx, "MATCH (n:Person) RETURN n", &params, symbols, policy())
            .unwrap();
        assert!(plain.procedures.is_empty());
        assert_eq!(plain.canonical_bytes()[33], 1);
    });
}

#[test]
fn refusals_are_typed_and_name_the_offending_site() {
    run(async |commit, cx| {
        let db = open(commit).await;
        let query = |text: &str| {
            db.query(cx, text, &GqlParameters::new(), symbols, policy())
                .unwrap_err()
        };
        let unknown = procedure_error(query("CALL fnx.nope() YIELD vertex RETURN vertex")).unwrap();
        assert!(
            matches!(
                unknown,
                ProcedureError::Bind(FnxCallError {
                    site: FnxCallSite::Procedure,
                    kind: FnxBindErrorKind::UnknownProcedure,
                })
            ),
            "{unknown:?}"
        );
        let other =
            procedure_error(query("CALL graph.pagerank() YIELD vertex RETURN vertex")).unwrap();
        assert!(
            matches!(
                other,
                ProcedureError::Bind(FnxCallError {
                    kind: FnxBindErrorKind::UnknownProcedure,
                    ..
                })
            ),
            "{other:?}"
        );
        let text = procedure_error(query(
            "CALL fnx.single_source_shortest_path_length('one') YIELD vertex RETURN vertex",
        ))
        .unwrap();
        assert!(
            matches!(
                text,
                ProcedureError::Bind(FnxCallError {
                    site: FnxCallSite::Argument(0),
                    ..
                })
            ),
            "{text:?}"
        );
        let missing = procedure_error(query(
            "CALL fnx.single_source_shortest_path_length() YIELD vertex RETURN vertex",
        ))
        .unwrap();
        assert!(
            matches!(
                missing,
                ProcedureError::Bind(FnxCallError {
                    site: FnxCallSite::Arguments,
                    kind: FnxBindErrorKind::MissingArgument("source"),
                })
            ),
            "{missing:?}"
        );
        let field =
            procedure_error(query("CALL fnx.pagerank() YIELD vertex, rank RETURN rank")).unwrap();
        assert!(
            matches!(
                field,
                ProcedureError::Bind(FnxCallError {
                    site: FnxCallSite::Yield(1),
                    kind: FnxBindErrorKind::UnknownYield,
                })
            ),
            "{field:?}"
        );
    });
}

#[test]
fn a_multigraph_is_refused_rather_than_collapsed() {
    run(async |commit, cx| {
        let mut db = open(commit).await;
        let mut parallel = WriteBatch::new(R);
        parallel.add_edge(EId(7), VId(1), VId(2), vec![]);
        db.write(commit, parallel).await.unwrap();
        let error = db
            .query(
                cx,
                "CALL fnx.pagerank() YIELD vertex RETURN vertex",
                &GqlParameters::new(),
                symbols,
                policy(),
            )
            .unwrap_err();
        assert!(
            matches!(procedure_error(error).unwrap(), ProcedureError::Read(_)),
            "a parallel edge must refuse"
        );
    });
}

#[test]
fn the_read_budget_caps_the_projection() {
    run(async |commit, cx| {
        let db = open(commit).await;
        let tight = GqlQueryPolicy::new(3, 100_000, 100_000_000, 10_000_000);
        let text = "CALL fnx.pagerank() YIELD vertex RETURN vertex";
        assert!(
            db.query(cx, text, &GqlParameters::new(), symbols, tight)
                .is_err(),
            "a 3-record budget cannot project 6 vertices and 5 edges"
        );
        assert!(
            db.query(cx, text, &GqlParameters::new(), symbols, policy())
                .is_ok()
        );
    });
}

#[test]
fn a_yielded_vertex_joins_its_own_graph_properties() {
    run(async |commit, cx| {
        let db = open(commit).await;
        let standalone = db
            .call_fnx(
                cx,
                "CALL fnx.pagerank() YIELD vertex, score",
                &FnxParameters::new(),
                explicit(false),
            )
            .unwrap();
        // Oracle: vertex v carries p = 10 * v, so each row is (p, score).
        let mut expected: Vec<Vec<GraphValue>> = standalone
            .analytics
            .rows
            .into_iter()
            .map(|row| match (row[0], row[1]) {
                (FnxValue::Vertex(vertex), score) => vec![
                    GraphValue::Scalar(CanonicalScalar::Int(vertex.0 as i64 * 10)),
                    value(score),
                ],
                _ => Vec::new(),
            })
            .collect();
        expected.sort_by(|a, b| {
            let by_score = compare(&b[1], &a[1]).expect("scores");
            by_score.then_with(|| compare(&a[0], &b[0]).expect("properties"))
        });
        let (columns, actual) = rows(
            db.query(
                cx,
                "CALL fnx.pagerank() YIELD vertex AS n, score MATCH (n:Person) \
                 WITH n.p AS p, score RETURN p, score ORDER BY score DESC, p",
                &GqlParameters::new(),
                symbols,
                policy(),
            )
            .unwrap(),
        );
        assert_eq!(columns, ["p", "score"]);
        // Six vertices, six rows: the reused name is that vertex, not a
        // second binding crossing all 36 pairs.
        assert_eq!(actual.len(), 6);
        assert_eq!(actual, expected);
    });
}

/// The single row of an aggregate read; anything else reads as no cells.
fn only_row(result: QueryResult) -> Vec<QueryValue> {
    let QueryResult::Rows { rows, .. } = result else {
        return Vec::new();
    };
    if rows.len() == 1 {
        rows.into_iter().next().unwrap_or_default()
    } else {
        Vec::new()
    }
}

#[test]
fn aggregate_analytics_run_at_the_read_snapshot() {
    run(async |commit, cx| {
        let mut db = open(commit).await;
        let old = db.read_session().unwrap();
        let params = GqlParameters::new();
        let int = |v: i64| QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(v)));
        // {1..5} and {6}: two weak components.
        let components = "CALL fnx.weakly_connected_components() YIELD component \
                          RETURN COUNT(DISTINCT component) AS c";
        let reach = "CALL fnx.single_source_shortest_path_length(1) YIELD distance \
                     RETURN MAX(distance) AS far, COUNT(*) AS reached";
        let head = |db: &Database<MemVfs>, text| {
            only_row(db.query(cx, text, &params, symbols, policy()).unwrap())
        };
        assert_eq!(head(&db, components), [QueryValue::Count(2)]);
        assert_eq!(head(&db, reach), [int(4), QueryValue::Count(5)]);
        // Linking 6 merges the components at the head, never in the old view.
        let mut more = WriteBatch::new(R);
        more.add_edge(EId(6), VId(5), VId(6), vec![]);
        db.write(commit, more).await.unwrap();
        assert_eq!(head(&db, components), [QueryValue::Count(1)]);
        let pinned = |text| only_row(old.query(cx, text, &params, symbols, policy()).unwrap());
        assert_eq!(pinned(components), [QueryValue::Count(2)]);
        assert_eq!(pinned(reach), [int(4), QueryValue::Count(5)]);
    });
}

/// fgdb-qnqrj: a Prism score thresholds with a decimal literal. Degree
/// centrality here is deg / 5, and only vertex 3 (degree 3) exceeds 0.5.
#[test]
fn a_procedure_score_filters_against_a_decimal_literal() {
    run(async |commit, cx| {
        let db = open(commit).await;
        let standalone = db
            .call_fnx(
                cx,
                "CALL fnx.degree_centrality() YIELD vertex, score",
                &FnxParameters::new(),
                explicit(true),
            )
            .unwrap();
        let expected: Vec<Vec<GraphValue>> = standalone
            .analytics
            .rows
            .into_iter()
            .filter(|row| matches!(row[1], FnxValue::Score(score) if score > 0.5))
            .map(|row| vec![value(row[0])])
            .collect();
        assert_eq!(expected, [vec![GraphValue::Vertex(VId(3))]]);
        let (_, actual) = rows(
            db.query(
                cx,
                "CALL fnx.degree_centrality() YIELD vertex, score WHERE score > 0.5 \
                 RETURN vertex ORDER BY vertex",
                &GqlParameters::new(),
                symbols,
                policy(),
            )
            .unwrap(),
        );
        assert_eq!(actual, expected);
    });
}

fn run_transaction<T>(
    test: impl AsyncFnOnce(&fgdb_types::CommitCx, &QueryCx, &fgdb_types::TxnCx) -> T,
) -> T {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(test(&contexts.commit(), &contexts.query(), &contexts.txn()))
}

/// fgdb-7qznp: the CALL and every later MATCH see the same canonical staged
/// graph, including deletions, their cascades and a dependent relation group.
#[test]
fn transaction_calls_compose_over_staged_topology_properties_and_isolates() {
    run_transaction(async |commit, cx, txcx| {
        let mut db = open(commit).await;
        let before = db.frontier().unwrap();
        let params = GqlParameters::new();
        let mut txn = db.begin(txcx).unwrap();
        let mut changes = WriteBatch::new(R);
        changes.delete_vertex(VId(3)); // retires edges 2, 3 and 4
        changes.delete_edge(EId(5));
        changes.create_vertex(VId(7), vec![PERSON], vec![(P, CanonicalScalar::Int(70))]);
        changes.set_vertex_property(VId(6), P, Some(CanonicalScalar::Int(600)));
        let mut second_relation = WriteBatch::new(RelationId(2));
        second_relation.add_edge(EId(6), VId(2), VId(6), vec![]);
        let mut third_relation = WriteBatch::new(RelationId(3));
        third_relation.add_edge(EId(7), VId(6), VId(7), vec![]);
        txn.write_ordered(&mut db, vec![changes, second_relation, third_relation])
            .unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        let reach = "CALL fnx.single_source_shortest_path_length(1) YIELD vertex AS n, distance \
                     MATCH (n) RETURN n.p AS p, distance ORDER BY distance";
        let (_, actual) = rows(
            txn.query(&db, cx, reach, &params, symbols, policy())
                .unwrap(),
        );
        let int = |v| GraphValue::Scalar(CanonicalScalar::Int(v));
        assert_eq!(
            actual,
            vec![
                vec![int(10), int(0)],
                vec![int(20), int(1)],
                vec![int(600), int(2)],
                vec![int(70), int(3)]
            ],
        );
        // {1,2,6,7}, {4}, {5}. A source built only from edge endpoints loses
        // two isolated components; a source reading the basis yields two.
        let components = "CALL fnx.weakly_connected_components() YIELD component \
                          RETURN COUNT(DISTINCT component) AS c";
        assert_eq!(
            only_row(
                txn.query(&db, cx, components, &params, symbols, policy())
                    .unwrap()
            ),
            [QueryValue::Count(3)],
        );
        assert_eq!(
            only_row(
                db.query(cx, components, &params, symbols, policy())
                    .unwrap()
            ),
            [QueryValue::Count(2)],
        );
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        assert!(matches!(
            txn.finish(&mut db, commit).await.unwrap(),
            fgdb_types::EmbeddedTxnCompletion::WriteCommitted { .. },
        ));
        assert_eq!(
            rows(db.query(cx, reach, &params, symbols, policy()).unwrap()).1,
            actual
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

/// Source mutations after the pinned basis do not change CALL results. The
/// same observations must instead reject transaction completion, even when
/// the query delivered no rows or the changed relation did not exist before.
#[test]
fn transaction_calls_retain_empty_table_phantoms_and_hidden_output_dependencies() {
    run_transaction(async |commit, cx, txcx| {
        let params = GqlParameters::new();
        for (change, hidden) in
            (0..4).flat_map(|change| [false, true].map(|hidden| (change, hidden)))
        {
            let mut db = Database::open_memory(commit, keys()).await.unwrap();
            if change != 0 {
                let mut seed = WriteBatch::new(R);
                seed.create_vertex(VId(1), vec![PERSON], vec![(P, CanonicalScalar::Int(10))]);
                seed.create_vertex(VId(2), vec![PERSON], vec![]);
                db.write(commit, seed).await.unwrap();
            }
            let mut txn = db.begin(txcx).unwrap();
            let text = if hidden {
                "CALL fnx.weakly_connected_components() YIELD vertex, component \
                 RETURN vertex, component LIMIT 0"
            } else {
                "CALL fnx.weakly_connected_components() YIELD vertex AS n, component \
                 MATCH (n) RETURN n, component, n.p ORDER BY n"
            };
            let answer = txn
                .query(&db, cx, text, &params, symbols, policy())
                .unwrap();
            if hidden {
                assert!(rows(answer.clone()).1.is_empty());
            }
            let mut winner = WriteBatch::new(RelationId(99));
            match change {
                0 => {
                    winner.create_vertex(VId(9), vec![], vec![]);
                }
                1 => {
                    winner.add_edge(EId(99), VId(1), VId(2), vec![]);
                }
                2 => {
                    winner.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(20)));
                }
                _ => {
                    winner.delete_vertex(VId(2));
                }
            }
            db.write(commit, winner).await.unwrap();
            if !hidden {
                assert_ne!(
                    db.query(cx, text, &params, symbols, policy()).unwrap(),
                    answer,
                    "the fixture must distinguish the live head for change {change}",
                );
            }
            // Re-executing never reacquires the live head.
            assert_eq!(
                txn.query(&db, cx, text, &params, symbols, policy())
                    .unwrap(),
                answer,
            );
            let frontier = db.frontier().unwrap();
            assert!(
                matches!(
                    txn.finish(&mut db, commit).await,
                    Err(fgdb::WriteTxnError::Write(
                        fgdb::WriteError::FirstCommitterWins { .. }
                    )),
                ),
                "missing dependency for change {change}"
            );
            assert_eq!(db.frontier().unwrap(), frontier);
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn transaction_call_refusal_and_savepoint_rollback_preserve_graph_dependencies() {
    run_transaction(async |commit, cx, txcx| {
        let mut db = open(commit).await;
        let mut txn = db.begin(txcx).unwrap();
        txn.savepoint(&db, "before_parallel").unwrap();
        let mut parallel = WriteBatch::new(R);
        parallel.add_edge(EId(7), VId(1), VId(2), vec![]);
        txn.write(&mut db, parallel).unwrap();
        let text = "CALL fnx.pagerank() YIELD vertex RETURN vertex";
        let error = txn
            .query(&db, cx, text, &GqlParameters::new(), symbols, policy())
            .unwrap_err();
        assert!(matches!(
            error,
            QueryError::TransactionSet(ref error) if matches!(error.as_ref(),
                GqlQueryError::Source(GraphSetExecutionError::Source(
                    fgdb::WriteTxnError::Gql(GqlError::Procedure(ProcedureError::Read(_)))
                )))
        ));
        txn.rollback_to_savepoint(&db, "before_parallel").unwrap();
        assert!(txn.edge(&db, EId(7)).unwrap().is_none());
        // Do not execute another graph query after rollback: completion
        // must retain the failed CALL's table witness, not a later read's.
        let mut winner = WriteBatch::new(RelationId(88));
        winner.add_edge(EId(88), VId(5), VId(6), vec![]);
        db.write(commit, winner).await.unwrap();
        let frontier = db.frontier().unwrap();
        assert!(matches!(
            txn.finish(&mut db, commit).await,
            Err(fgdb::WriteTxnError::Write(
                fgdb::WriteError::FirstCommitterWins { .. }
            )),
        ));
        assert_eq!(db.frontier().unwrap(), frontier);
        assert!(db.edge(EId(7)).unwrap().is_none());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn transaction_call_sources_share_one_allowance_across_union_and_aggregate() {
    run_transaction(async |commit, cx, txcx| {
        let db = open(commit).await;
        let params = GqlParameters::new();
        let txn = db.begin(txcx).unwrap();
        let text = "CALL fnx.weakly_connected_components() YIELD component RETURN component";
        let prepare = |text: &str| {
            fgdb_gql::PreparedGraphSetText::prepare(text, symbols)
                .unwrap()
                .bind_parameters(&params)
                .unwrap()
        };
        let single = txn
            .execute_graph_set_governed(&db, cx, &prepare(text), policy())
            .unwrap();
        assert_eq!(single.rows.snapshot_records, 11);
        let combined = prepare(&format!("{text} UNION ALL {text}"));
        let tight = GqlQueryPolicy::new(
            2 * single.rows.snapshot_records - 1,
            100,
            policy().evaluator.max_work_units,
            policy().evaluator.max_scratch_entries,
        );
        assert!(matches!(
            txn.execute_graph_set_governed(&db, cx, &combined, tight),
            Err(GqlQueryError::Rows(error))
                if error.dimension == fgdb_gql::GqlBudgetDimension::SnapshotRecords,
        ));
        let result = txn
            .execute_graph_set_governed(&db, cx, &combined, policy())
            .unwrap();
        assert_eq!(result.rows.snapshot_records, 22);
        assert_eq!(result.value.len(), 12);
        assert!(result.evaluator.work_units >= 2 * single.evaluator.work_units);
        let exact = GqlQueryPolicy::new(
            result.rows.snapshot_records,
            result.rows.result_rows,
            result.evaluator.work_units,
            result.evaluator.scratch_entries,
        );
        assert_eq!(
            txn.execute_graph_set_governed(&db, cx, &combined, exact)
                .unwrap(),
            result,
        );
        for work in [false, true] {
            let mut short = exact;
            if work {
                short.evaluator.max_work_units -= 1;
            } else {
                short.evaluator.max_scratch_entries -= 1;
            }
            assert!(
                txn.execute_graph_set_governed(&db, cx, &combined, short)
                    .is_err(),
                "the complete query must account for every work/scratch unit",
            );
        }
        // The kernel alone fits this allowance. Its source and projection
        // already spent some, so preflight must refuse before running it.
        let pagerank = "CALL fnx.pagerank() YIELD vertex";
        let kernel_work = db
            .call_fnx(cx, pagerank, &FnxParameters::new(), explicit(false))
            .unwrap()
            .analytics
            .certificate
            .estimated_work;
        let kernel_only = GqlQueryPolicy::new(
            1_000_000,
            100,
            kernel_work as u64,
            policy().evaluator.max_scratch_entries,
        );
        let error = txn
            .execute_graph_set_governed(
                &db,
                cx,
                &prepare(&format!("{pagerank} RETURN vertex")),
                kernel_only,
            )
            .unwrap_err();
        assert!(
            matches!(&error,
                GqlQueryError::Source(GraphSetExecutionError::Source(
                    fgdb::WriteTxnError::Gql(GqlError::Procedure(ProcedureError::Read(cause)))
                )) if matches!(cause.as_ref(),
                    FnxReadError::Execution(FnxExecutionError::LimitExceeded {
                        resource: "estimated work", limit, requested,
                    }) if *requested == kernel_work && limit < requested
                )
            ),
            "{error:?}",
        );
        let count = "CALL fnx.weakly_connected_components() YIELD component \
                     RETURN COUNT(DISTINCT component) AS c";
        // Procedure input rows spend scratch, not the one final result row.
        let one_row = GqlQueryPolicy::new(1_000_000, 1, 100_000_000, 10_000_000);
        assert_eq!(
            only_row(
                txn.query(&db, cx, count, &params, symbols, one_row)
                    .unwrap()
            ),
            [QueryValue::Count(2)],
        );
        txn.abort();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}
