//! Query-selected analytics run on the native snapshot/overlay and authorized
//! source, not a second graph implementation. Expected distances are derived
//! by hand; the standalone API independently supplies the projection recipe.

use asupersync::{Budget, runtime::RuntimeBuilder, security::key::AuthKey};
use fgdb::{Database, DatabaseKeys, MemVfs, QueryResult, QueryValue, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind};
use fgdb_prism::{
    Directedness, FnxParameters, FnxReadOptions, FnxSelection, FnxValue, FnxWeightSpec,
    MissingWeightPolicy, ParallelEdgePolicy, ProjectionSpec, SelfLoopPolicy,
};
use fgdb_types::{
    CanonicalF64, CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts,
    QueryCx, TxnCx, VId,
};
use fgdb_warden::{Authority, Grant, QueryLimits, Scope};

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x51; 32]);
const PERSON: LabelId = LabelId(7);
const OTHER: LabelId = LabelId(9);
const R: RelationId = RelationId(11);
const S: RelationId = RelationId(12);
const COST: PropertyKeyId = PropertyKeyId(13);
const PRIVATE: PropertyKeyId = PropertyKeyId(14);
const BRANCH: &str = "projection-test";
const WEIGHTED: &str = "CALL fnx.single_source_dijkstra_path_length(args => [1], label => 'Person', \
     relation => 'R', weight => 'cost') YIELD node, distance \
     RETURN node, distance ORDER BY node";

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Label, "Other") => Some(GraphSymbol::Label(OTHER)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Property, "cost") => Some(GraphSymbol::Property(COST)),
        (GraphSymbolKind::Property, "private") => Some(GraphSymbol::Property(PRIVATE)),
        _ => None,
    }
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000_000, 100_000, 100_000_000, 10_000_000)
}

fn run<T>(test: impl AsyncFnOnce(&CommitCx, &QueryCx, &TxnCx) -> T) -> T {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(test(&contexts.commit(), &contexts.query(), &contexts.txn()))
}

/// Person 1 --3--> 2 --4--> 3, plus isolated Person 5. The larger database
/// also has an S shortcut 1 --2--> 3 and two R edges via Other 4 with invalid
/// weights. A correct induced projection excludes 4 before reading its edges'
/// weights. The small database physically omits all of this hidden topology.
async fn database(cx: &CommitCx, extra: bool) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x52; 32], NS, [0x53; 32]))
        .await
        .unwrap();
    let mut batch = WriteBatch::new(R);
    for id in [1, 2, 3, 5] {
        batch.create_vertex(VId(id), vec![PERSON], vec![]);
    }
    for (eid, source, target, cost) in [(1, 1, 2, 3), (2, 2, 3, 4)] {
        let mut props = vec![(COST, CanonicalScalar::Int(cost))];
        if extra {
            props.push((PRIVATE, CanonicalScalar::Bool(true)));
        }
        batch.add_edge(EId(eid), VId(source), VId(target), props);
    }
    if extra {
        batch.create_vertex(VId(4), vec![OTHER], vec![]);
        for (eid, source, target) in [(3, 1, 4), (4, 4, 3)] {
            batch.add_edge(
                EId(eid),
                VId(source),
                VId(target),
                vec![(COST, CanonicalScalar::Bool(true))],
            );
        }
    }
    db.write(cx, batch).await.unwrap();
    if extra {
        let mut shortcut = WriteBatch::new(S);
        shortcut.add_edge(
            EId(5),
            VId(1),
            VId(3),
            vec![(COST, CanonicalScalar::Int(2))],
        );
        db.write(cx, shortcut).await.unwrap();
    }
    db
}

fn rows(result: QueryResult) -> Vec<Vec<GraphValue>> {
    let QueryResult::Rows { rows, .. } = result else {
        panic!("analytics must return rows, not a write receipt");
    };
    rows.into_iter()
        .map(|row| {
            row.into_iter()
                .map(|cell| match cell {
                    QueryValue::Value(value) => value,
                    _ => panic!("a direct CALL projection must retain the native value domain"),
                })
                .collect()
        })
        .collect()
}

fn expected_distances(values: &[(u128, f64)]) -> Vec<Vec<GraphValue>> {
    values
        .iter()
        .map(|&(id, distance)| {
            vec![
                GraphValue::Vertex(VId(id)),
                GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(distance))),
            ]
        })
        .collect()
}

fn query(db: &Database<MemVfs>, cx: &QueryCx, text: &str) -> QueryResult {
    db.query(cx, text, &GqlParameters::new(), symbols, policy())
        .unwrap_or_else(|error| panic!("{text}: {error}"))
}

#[test]
fn named_projection_selects_labels_relations_weights_and_direction() {
    run(async |commit, cx, _| {
        let db = database(commit, true).await;
        let expected = expected_distances(&[(1, 0.0), (2, 3.0), (3, 7.0)]);
        assert_eq!(rows(query(&db, cx, WEIGHTED)), expected);

        // Independent projection construction: no query binder or defaults
        // choose any of the input/reduction fields in this control.
        let mut options = FnxReadOptions::whole_graph(None);
        options.selection = FnxSelection {
            vertex_label: Some(PERSON),
            relation: Some(R),
            weight: FnxWeightSpec::Property {
                key: COST,
                missing: MissingWeightPolicy::Reject,
            },
        };
        options.projection = ProjectionSpec {
            directedness: Directedness::Directed,
            parallel_edges: ParallelEdgePolicy::Reject,
            self_loops: SelfLoopPolicy::Keep,
        };
        let standalone = db
            .call_fnx(
                cx,
                "CALL fnx.single_source_dijkstra_path_length(1) YIELD node, distance",
                &FnxParameters::new(),
                options,
            )
            .unwrap();
        let converted: Vec<Vec<GraphValue>> = standalone
            .analytics
            .rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|value| match value {
                        FnxValue::Vertex(vertex) => GraphValue::Vertex(vertex),
                        FnxValue::Float(value) => {
                            GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(value)))
                        }
                        _ => panic!("Dijkstra result domain changed"),
                    })
                    .collect()
            })
            .collect();
        assert_eq!(converted, expected);
        assert_eq!(
            rows(query(
                &db,
                cx,
                "CALL fnx.single_source_dijkstra_path_length(args => [1], label => 'Person', \
                 relation => 'S', weight => 'cost') YIELD node, distance \
                 RETURN node, distance ORDER BY node"
            )),
            expected_distances(&[(1, 0.0), (3, 2.0)]),
        );
        assert_eq!(
            rows(query(
                &db,
                cx,
                "CALL fnx.single_source_dijkstra_path_length(args => [3], label => 'Person', \
                 relation => 'R', weight => 'cost', direction => 'reversed') YIELD node, distance \
                 RETURN node, distance ORDER BY node"
            )),
            expected_distances(&[(1, 7.0), (2, 4.0), (3, 0.0)]),
        );
        // Omitting the label really does admit the malformed Other edges.
        assert!(
            db.query(
                cx,
                "CALL fnx.single_source_dijkstra_path_length(args => [1], relation => 'R', \
             weight => 'cost') YIELD node RETURN node",
                &GqlParameters::new(),
                symbols,
                policy()
            )
            .is_err()
        );
        // Isolated selected vertices must not be lost by selecting endpoints.
        let components = rows(query(
            &db,
            cx,
            "CALL fnx.weakly_connected_components(label => 'Person', relation => 'R') \
             YIELD node, component RETURN node, component ORDER BY node",
        ));
        assert_eq!(
            components,
            vec![
                vec![GraphValue::Vertex(VId(1)), GraphValue::Vertex(VId(1))],
                vec![GraphValue::Vertex(VId(2)), GraphValue::Vertex(VId(1))],
                vec![GraphValue::Vertex(VId(3)), GraphValue::Vertex(VId(1))],
                vec![GraphValue::Vertex(VId(5)), GraphValue::Vertex(VId(5))],
            ]
        );
    });
}

#[test]
fn kernel_parameters_and_positional_defaults_are_preserved() {
    run(async |commit, cx, _| {
        let db = database(commit, false).await;
        let params = GqlParameters::new()
            .with_int64("source", 1)
            .unwrap()
            .with_int64("cutoff", 1)
            .unwrap();
        let actual = db
            .query(
                cx,
                "CALL fnx.single_source_shortest_path_length(args => [$source, $cutoff], \
             label => 'Person', relation => 'R') YIELD node RETURN node ORDER BY node",
                &params,
                symbols,
                policy(),
            )
            .unwrap();
        assert_eq!(
            rows(actual),
            vec![
                vec![GraphValue::Vertex(VId(1))],
                vec![GraphValue::Vertex(VId(2))],
            ]
        );
        for algorithm in [
            "pagerank",
            "connected_components",
            "weakly_connected_components",
        ] {
            let positional = format!("CALL fnx.{algorithm}() YIELD node RETURN node ORDER BY node");
            let named =
                format!("CALL fnx.{algorithm}(args => []) YIELD node RETURN node ORDER BY node");
            assert_eq!(query(&db, cx, &positional), query(&db, cx, &named));
        }
    });
}

#[test]
fn reduction_and_loop_policies_apply_before_discarded_weight_observation() {
    run(async |commit, cx, _| {
        let mut db = database(commit, true).await;
        let mut extra = WriteBatch::new(R);
        extra.add_edge(
            EId(20),
            VId(1),
            VId(2),
            vec![(COST, CanonicalScalar::Int(8))],
        );
        extra.add_edge(
            EId(21),
            VId(2),
            VId(2),
            vec![(COST, CanonicalScalar::Bool(true))],
        );
        db.write(commit, extra).await.unwrap();
        for (parallel, to_two, to_three) in [
            ("min", 3.0, 7.0),
            ("max", 8.0, 12.0),
            ("sum", 11.0, 15.0),
            ("collapse", 1.0, 2.0),
        ] {
            let text = format!(
                "CALL fnx.single_source_dijkstra_path_length(args => [1], label => 'Person', \
                 relation => 'R', weight => 'cost', parallel => '{parallel}', \
                 self_loops => 'drop') YIELD node, distance RETURN node, distance ORDER BY node"
            );
            assert_eq!(
                rows(query(&db, cx, &text)),
                expected_distances(&[(1, 0.0), (2, to_two), (3, to_three)]),
                "{parallel}"
            );
        }
        for policies in [
            "parallel => 'reject', self_loops => 'drop'",
            "parallel => 'min', self_loops => 'keep'",
            "parallel => 'min', self_loops => 'reject'",
        ] {
            let text = format!(
                "CALL fnx.single_source_dijkstra_path_length(args => [1], label => 'Person', \
                 relation => 'R', weight => 'cost', {policies}) YIELD node RETURN node LIMIT 0"
            );
            assert!(
                db.query(cx, &text, &GqlParameters::new(), symbols, policy())
                    .is_err(),
                "{text}"
            );
        }
    });
}

#[test]
fn missing_weights_require_an_explicit_policy() {
    run(async |commit, cx, _| {
        let mut db = database(commit, false).await;
        let mut remove = WriteBatch::new(R);
        remove.set_edge_property(EId(2), COST, None);
        db.write(commit, remove).await.unwrap();
        assert!(
            db.query(cx, WEIGHTED, &GqlParameters::new(), symbols, policy())
                .is_err()
        );
        for (missing, distance) in [("unit", 4.0), ("zero", 3.0)] {
            let text = format!(
                "CALL fnx.single_source_dijkstra_path_length(args => [1], relation => 'R', \
                 weight => 'cost', missing => '{missing}') YIELD node, distance \
                 RETURN node, distance ORDER BY node"
            );
            assert_eq!(
                rows(query(&db, cx, &text)),
                expected_distances(&[(1, 0.0), (2, 3.0), (3, distance)])
            );
        }
    });
}

#[test]
fn projection_certificates_and_pinned_views_retain_the_exact_source() {
    run(async |commit, cx, _| {
        let mut db = database(commit, true).await;
        let view = db.read_session().unwrap();
        let params = GqlParameters::new();
        let (before, certificate) = db
            .execute_certified(cx, WEIGHTED, &params, symbols, policy())
            .unwrap();
        assert_eq!(
            rows(before.clone()),
            expected_distances(&[(1, 0.0), (2, 3.0), (3, 7.0)])
        );
        let (same_rows, different_recipe) = db
            .execute_certified(
                cx,
                "CALL fnx.single_source_dijkstra_path_length(args => [1], label => 'Person', \
             relation => 'R', weight => 'cost', missing => 'unit') YIELD node, distance \
             RETURN node, distance ORDER BY node",
                &params,
                symbols,
                policy(),
            )
            .unwrap();
        // All selected edges have weights: these policies give the SAME rows.
        // Procedure evidence binds the result and witness, not the projection
        // recipe (9ab35ec5), so it is equal. The certificate binds the
        // statement, so it still distinguishes the input recipes.
        assert_eq!(same_rows, before);
        assert_eq!(certificate.procedures.len(), 1);
        assert_eq!(different_recipe.procedures.len(), 1);
        assert_eq!(
            certificate.procedures[0].evidence_digest,
            different_recipe.procedures[0].evidence_digest
        );
        assert_ne!(
            certificate.canonical_bytes(),
            different_recipe.canonical_bytes()
        );
        let (_, unweighted) = db
            .execute_certified(
                cx,
                "CALL fnx.single_source_dijkstra_path_length(args => [1], label => 'Person', \
             relation => 'R') YIELD node, distance RETURN node, distance ORDER BY node",
                &params,
                symbols,
                policy(),
            )
            .unwrap();
        assert_eq!(certificate.procedures.len(), 1);
        assert_ne!(certificate.canonical_bytes(), unweighted.canonical_bytes());
        let mut update = WriteBatch::new(R);
        update.set_edge_property(EId(2), COST, Some(CanonicalScalar::Int(20)));
        db.write(commit, update).await.unwrap();
        assert_eq!(
            rows(query(&db, cx, WEIGHTED)),
            expected_distances(&[(1, 0.0), (2, 3.0), (3, 23.0)])
        );
        assert_eq!(
            view.query(cx, WEIGHTED, &params, symbols, policy())
                .unwrap(),
            before
        );
        assert_eq!(
            db.replay(cx, &certificate, &params, symbols, policy())
                .unwrap(),
            before
        );
    });
}

#[test]
fn transaction_projection_reads_staged_weights_and_savepoint_rollback() {
    run(async |commit, cx, txcx| {
        let mut db = database(commit, true).await;
        let before = query(&db, cx, WEIGHTED);
        let mut txn = db.begin(txcx).unwrap();
        txn.savepoint(&db, "before_weight").unwrap();
        let mut change = WriteBatch::new(R);
        change.set_edge_property(EId(2), COST, Some(CanonicalScalar::Int(1)));
        txn.write(&mut db, change).unwrap();
        let actual = txn
            .query(&db, cx, WEIGHTED, &GqlParameters::new(), symbols, policy())
            .unwrap();
        assert_eq!(
            rows(actual),
            expected_distances(&[(1, 0.0), (2, 3.0), (3, 4.0)])
        );
        assert_eq!(query(&db, cx, WEIGHTED), before);
        txn.rollback_to_savepoint(&db, "before_weight").unwrap();
        assert_eq!(
            txn.query(&db, cx, WEIGHTED, &GqlParameters::new(), symbols, policy())
                .unwrap(),
            before
        );
        txn.abort();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn query_projection_cannot_widen_capability_scope_or_observe_hidden_weights() {
    run(async |commit, cx, _| {
        let db = database(commit, true).await;
        let control = database(commit, false).await;
        let issuer = Authority::new(
            AuthKey::from_seed(701),
            NS,
            "projection-graph",
            SchemaEpoch(0),
            1,
        )
        .unwrap();
        let mut grant = Grant::read_only(
            BRANCH,
            1000,
            QueryLimits {
                max_nodes: 1000,
                max_work: 1_000_000,
                max_rows: 1000,
            },
        );
        grant.labels = Scope::only([PERSON]);
        grant.relations = Scope::only([R]);
        grant.properties = Scope::only([COST]);
        let token = issuer.issue_at(&grant, 100).unwrap();
        for text in [
            WEIGHTED,
            "CALL fnx.single_source_dijkstra_path_length(args => [1], weight => 'cost') \
             YIELD node, distance RETURN node, distance ORDER BY node",
            "CALL fnx.single_source_dijkstra_path_length(args => [1], weight => 'private', \
             missing => 'unit') YIELD node, distance RETURN node, distance ORDER BY node",
            "CALL fnx.weakly_connected_components(relation => 'S') \
             YIELD node, component RETURN node, component ORDER BY node",
            "CALL fnx.in_degree_centrality() YIELD node, score RETURN node, score ORDER BY node",
            "CALL fnx.out_degree_centrality() YIELD node, score RETURN node, score ORDER BY node",
        ] {
            let read = |database: &Database<MemVfs>| {
                database
                    .query_authorized(
                        cx,
                        &issuer,
                        &token,
                        BRANCH,
                        text,
                        &GqlParameters::new(),
                        symbols,
                        policy(),
                        || 100,
                    )
                    .unwrap()
            };
            let actual = read(&db);
            assert_eq!(actual, read(&control), "{text}");
            let actual = rows(actual);
            assert!(!actual.is_empty(), "non-vacuous visible projection: {text}");
            if text.contains("in_degree_centrality") {
                assert_eq!(
                    actual,
                    expected_distances(&[(1, 0.0), (2, 1.0 / 3.0), (3, 1.0 / 3.0), (5, 0.0)])
                );
            } else if text.contains("out_degree_centrality") {
                assert_eq!(
                    actual,
                    expected_distances(&[(1, 1.0 / 3.0), (2, 1.0 / 3.0), (3, 0.0), (5, 0.0)])
                );
            }
        }
        let actual = db
            .query_authorized(
                cx,
                &issuer,
                &token,
                BRANCH,
                WEIGHTED,
                &GqlParameters::new(),
                symbols,
                policy(),
                || 100,
            )
            .unwrap();
        assert_eq!(
            rows(actual),
            expected_distances(&[(1, 0.0), (2, 3.0), (3, 7.0)])
        );
    });
}

#[test]
fn invalid_projection_options_and_enclosing_limits_refuse_without_side_effects() {
    run(async |commit, cx, _| {
        let db = database(commit, false).await;
        let frontier = db.frontier().unwrap();
        for arguments in [
            "direction => 'sideways'",
            "direction => 1",
            "parallel => 'ignore'",
            "self_loops => 'ignore'",
            "missing => 'unit'",
            "args => 1",
            "args => ['not a number']",
            "snapshot => 1",
            "max_work => 999999",
            "weight => 'cost', missing => 'ignore'",
            "label => ['Person']",
        ] {
            let text = format!("CALL fnx.pagerank({arguments}) YIELD node RETURN node LIMIT 0");
            assert!(
                db.query(cx, &text, &GqlParameters::new(), symbols, policy())
                    .is_err(),
                "{text}"
            );
        }
        for tight in [
            GqlQueryPolicy::new(0, 100, 1_000_000, 100_000),
            GqlQueryPolicy::new(100_000, 2, 1_000_000, 100_000),
            GqlQueryPolicy::new(100_000, 100, 0, 100_000),
            GqlQueryPolicy::new(100_000, 100, 1_000_000, 0),
        ] {
            assert!(
                db.query(cx, WEIGHTED, &GqlParameters::new(), symbols, tight)
                    .is_err()
            );
        }
        assert_eq!(db.frontier().unwrap(), frontier);
        assert_eq!(
            rows(query(&db, cx, WEIGHTED)),
            expected_distances(&[(1, 0.0), (2, 3.0), (3, 7.0)])
        );
    });
}
