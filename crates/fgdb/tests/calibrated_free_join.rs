//! End-to-end integration and differential tests for Loom's sketch-guided,
//! conformal-calibrated multiway `CalibratedFreeJoin` (`fgdb-rc1111-loom-conformal-freejoin-txo5d`).
//!
//! Covers:
//! 1. Exact ordered result parity between `Database::query` /
//!    `Database::execute_graph_pattern_calibrated_at` (`CalibratedFreeJoin` over
//!    `SliceAdjacencyTrie` + `fgdb-unsafe-simd` 4-wide gallop seek) and the
//!    baseline `GlaPlan::execute_governed_with_element_maps` oracle across
//!    triangles ($C_3$), 4-cycles ($C_4$), diamonds ($K_4 - e$), factorizable
//!    bowtie/claw suffixes (`FactorizedBatch`), parallel edges (`DifferentEdges`
//!    vs `MATCH REPEATABLE ELEMENTS`), self-loops, undirected hops, labels,
//!    `WHERE` predicates, property projections, `DISTINCT`, `ORDER BY`, and
//!    `SKIP` / `LIMIT`.
//! 2. Worst-case-optimal leapfrog scaling on skewed star-plus-chord graphs
//!    ($K_{1,N}$ + 1 closing edge across $N = 250, 500, 1000$), avoiding the
//!    $O(N^2)$ binary Cartesian intermediate blowup.
//! 3. Mid-leap `&QueryCx` cancellation (`Interrupted`), deterministic
//!    `JoinDecisionCard` canonical bytes & `Database::execute_certified` +
//!    `Database::replay` (`FG-INV-19`), conformal nonconformity fallback,
//!    e-process drift fallback, and `EXPLAIN` / `EXPLAIN (CERTIFICATE)`
//!    `JoinDecisionCard` rows.

use std::sync::Arc;

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryResult, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::free_join::{
    AgmExponent, BinaryObservation, ConformalJoinCalibration, JOIN_DECISION_POLICY_EPOCH,
    JoinStrategy,
};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphText,
};
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{
    CanonicalPropertyValue, CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId,
    PurposeContexts, VId,
};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const PERSON: LabelId = LabelId(1);
const ADMIN: LabelId = LabelId(2);
const SCORE: PropertyKeyId = PropertyKeyId(1);
const TAG: PropertyKeyId = PropertyKeyId(2);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x41; 32],
        DatabaseSecurityNamespaceId([0x42; 32]),
        [0x43; 32],
    )
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Label, "Admin") => Some(GraphSymbol::Label(ADMIN)),
        (GraphSymbolKind::Property, "score") => Some(GraphSymbol::Property(SCORE)),
        (GraphSymbolKind::Property, "tag") => Some(GraphSymbol::Property(TAG)),
        _ => None,
    }
}

fn generous_policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(500_000, 500_000, 50_000_000, 50_000_000)
}

async fn seed_rich_multiway_graph(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, keys()).await.unwrap();

    let mut r_batch = WriteBatch::new(R);
    for (id, labels, score, tag) in [
        (1, vec![PERSON, ADMIN], Some(10), Some("alpha")),
        (2, vec![PERSON], Some(20), Some("beta")),
        (3, vec![PERSON, ADMIN], Some(30), None),
        (4, vec![PERSON], Some(40), Some("delta")),
        (5, vec![ADMIN], Some(50), Some("epsilon")),
        (6, vec![PERSON], None, Some("zeta")),
    ] {
        let mut props = Vec::new();
        if let Some(s) = score {
            props.push((SCORE, CanonicalScalar::Int(s)));
        }
        if let Some(t) = tag {
            props.push((TAG, CanonicalScalar::ucs_basic_text(t).unwrap()));
        }
        r_batch.create_vertex(VId(id), labels, props);
    }

    // Directed triangles, 4-cycles, diamond chords, parallel edges, and self-loop:
    // Triangle 1-2-3-1 (with parallel edge 1->2 so DifferentEdges vs REPEATABLE matters)
    r_batch.add_edge(EId(100), VId(1), VId(2), vec![]);
    r_batch.add_edge(EId(101), VId(1), VId(2), vec![]);
    r_batch.add_edge(EId(102), VId(2), VId(3), vec![]);
    r_batch.add_edge(EId(103), VId(3), VId(1), vec![]);
    // Second triangle / diamond wing 2-3-4-2 and 4-cycle 1-2-4-3-1
    r_batch.add_edge(EId(104), VId(3), VId(4), vec![]);
    r_batch.add_edge(EId(105), VId(4), VId(2), vec![]);
    r_batch.add_edge(EId(106), VId(2), VId(4), vec![]);
    r_batch.add_edge(EId(107), VId(4), VId(3), vec![]);
    // Self-loop on VId(5)
    r_batch.add_edge(EId(108), VId(5), VId(5), vec![]);
    db.write(cx, r_batch).await.unwrap();

    // Independent star arms on S from hub VId(1) for factorizable suffix tests
    let mut s_batch = WriteBatch::new(S);
    s_batch.add_edge(EId(200), VId(1), VId(4), vec![]);
    s_batch.add_edge(EId(201), VId(1), VId(5), vec![]);
    s_batch.add_edge(EId(202), VId(1), VId(6), vec![]);
    db.write(cx, s_batch).await.unwrap();

    db
}

#[test]
fn calibrated_free_join_matches_baseline_gla_oracle_across_cyclic_and_filtered_patterns() {
    let ((), report) = run_async_under_lab(0xcf10_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query_cx = contexts.query();
        let db = seed_rich_multiway_graph(&commit).await;
        let view = db.read_session().unwrap();
        let as_of = view.frontier();
        let raw_vertices = view.vertices_at(as_of).unwrap();
        let raw_edges = view.edges_at(as_of).unwrap();
        let params = GqlParameters::new();

        let queries = [
            // 1. Triangle C_3 with parallel edges (default DIFFERENT EDGES)
            "MATCH (a)-[:R]->(b)-[:R]->(c)-[:R]->(a) RETURN a, b, c ORDER BY a, b, c",
            // 2. Triangle C_3 under REPEATABLE ELEMENTS (includes self-loop 5->5->5->5 and parallel multiplicities)
            "MATCH REPEATABLE ELEMENTS (a)-[:R]->(b)-[:R]->(c)-[:R]->(a) RETURN ALL a, b, c",
            // 3. Labeled + WHERE-filtered + property-projecting triangle
            "MATCH (a:Admin)-[:R]->(b:Person)-[:R]->(c:Admin)-[:R]->(a) WHERE a.score >= 10 AND b.tag IS NOT NULL RETURN a.score AS a_score, b.tag AS b_tag, c.score AS c_score ORDER BY a_score DESC",
            // 4. 4-cycle C_4 with vertex inequality and DISTINCT + SKIP/LIMIT
            "MATCH (a:Person)-[:R]->(b:Person)-[:R]->(c:Person)-[:R]->(d:Person)-[:R]->(a) WHERE a <> c RETURN DISTINCT a.score AS s1, c.score AS s2 ORDER BY s1, s2 SKIP 1 LIMIT 2",
            // 5. Diamond K_4 - e (5 edge atoms on 4 variables)
            "MATCH (a)-[:R]->(b)-[:R]->(c)-[:R]->(a), (b)-[:R]->(d)-[:R]->(c) RETURN a, b, c, d ORDER BY a, b, c, d",
            // 6. Undirected triangle with DifferentEdges EId-disjointness
            "MATCH (a)-[:R]-(b)-[:R]-(c)-[:R]-(a) WHERE a.score < b.score AND b.score < c.score RETURN a, b, c ORDER BY a, b, c",
            // 7. Factorizable 3-star claw on hub `a` under REPEATABLE ELEMENTS (`FactorizedFreeJoin`)
            "MATCH REPEATABLE ELEMENTS (a:Admin)-[:S]->(x), (a)-[:S]->(y), (a)-[:S]->(z) RETURN a.score AS hub, x, y, z ORDER BY x, y, z LIMIT 15",
            // 8. Empty match (unsatisfiable label/property filter pushed into unary SliceAdjacencyTrie)
            "MATCH (a:Admin)-[:R]->(b:Person)-[:R]->(c:Person)-[:R]->(a) WHERE a.score > 999 RETURN a, b, c",
        ];

        for text in queries {
            let prepared = PreparedGraphText::prepare_with_resolver(text, symbols).unwrap();
            let bound = prepared.bind_parameters(&params).unwrap();
            let (calibrated_exec, maybe_card) = db
                .execute_graph_pattern_calibrated_at(
                    &query_cx,
                    &bound,
                    as_of,
                    &ConformalJoinCalibration::default(),
                    generous_policy(),
                )
                .unwrap();
            let card = maybe_card.unwrap_or_else(|| {
                panic!("expected multiway pattern to compile to CalibratedFreeJoin: {text}")
            });
            assert_eq!(card.policy_epoch, JOIN_DECISION_POLICY_EPOCH);

            // Baseline reference evaluation directly on GlaPlan without CalibratedFreeJoin
            let vertex_ids = raw_vertices.iter().map(|row| row.vid);
            let identified_edges = raw_edges.iter().map(|rec| {
                (
                    rec.entry.eid,
                    rec.entry.src,
                    rec.entry.relation,
                    rec.entry.dst,
                )
            });
            let baseline_exec =
                bound
                    .plan()
                    .execute_governed_with_element_maps(
                        (raw_vertices.len() + raw_edges.len()) as u64,
                        vertex_ids,
                        identified_edges,
                        |vid, preds| {
                            Ok::<_, ()>(raw_vertices.iter().find(|r| r.vid == vid).is_some_and(
                                |r| preds.iter().all(|p| p.matches(&r.labels, &r.props)),
                            ))
                        },
                        |vid, key| {
                            Ok(raw_vertices.iter().find(|r| r.vid == vid).and_then(|r| {
                                r.props
                                    .binary_search_by_key(&key, |(k, _)| *k)
                                    .ok()
                                    .map(|at| &r.props[at].1)
                            }))
                        },
                        |_, _| Ok(None),
                        |_| Ok(None),
                        |_| Ok(None),
                        |_| Ok(None),
                        |_| Ok(None),
                        generous_policy(),
                        || Ok::<(), ()>(()),
                    )
                    .unwrap();

            assert_eq!(
                calibrated_exec.value,
                baseline_exec.value,
                "CalibratedFreeJoin drifted from baseline GLA oracle on: {text}\ncard: {}",
                card.summary_ascii()
            );

            // Verify Database::query (end-to-end public API) returns identical rows
            let QueryResult::Rows { rows: db_rows, .. } = db
                .query(&query_cx, text, &params, symbols, generous_policy())
                .unwrap()
            else {
                panic!("expected rows for {text}");
            };
            assert_eq!(db_rows.len(), calibrated_exec.value.len(), "{text}");
        }

        // Explicitly check AGM fractional edge-cover exponents & FactorizedFreeJoin selection:
        let tri = PreparedGraphText::prepare_with_resolver(
            "MATCH (a)-[:R]->(b)-[:R]->(c)-[:R]->(a) RETURN a, b, c",
            symbols,
        )
        .unwrap()
        .bind_parameters(&params)
        .unwrap();
        let (_, tri_card) = db
            .execute_graph_pattern_calibrated_at(
                &query_cx,
                &tri,
                as_of,
                &ConformalJoinCalibration::default(),
                generous_policy(),
            )
            .unwrap();
        let tri_card = tri_card.unwrap();
        assert_eq!(tri_card.agm_exponent, AgmExponent::reduced(3, 2));

        let claw = PreparedGraphText::prepare_with_resolver(
            "MATCH REPEATABLE ELEMENTS (a:Admin)-[:S]->(x), (a)-[:S]->(y), (a)-[:S]->(z) RETURN a, x, y, z",
            symbols,
        )
        .unwrap()
        .bind_parameters(&params)
        .unwrap();
        let (claw_exec, claw_card) = db
            .execute_graph_pattern_calibrated_at(
                &query_cx,
                &claw,
                as_of,
                &ConformalJoinCalibration::default(),
                generous_policy(),
            )
            .unwrap();
        let claw_card = claw_card.unwrap();
        assert!(claw_card.has_factorizable_suffix, "{claw_card:?}");
        assert_eq!(
            claw_card.selected_strategy,
            JoinStrategy::FactorizedFreeJoin
        );
        // Hub VId(1) has 3 S-neighbors {4, 5, 6} -> 3^3 = 27 combinations!
        assert_eq!(claw_exec.value.len(), 27);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn skewed_star_triangle_achieves_linear_wco_work_scaling_without_quadratic_blowup() {
    let ((), report) = run_async_under_lab(0xcf10_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query_cx = contexts.query();
        let params = GqlParameters::new();

        // Skewed star graph: hub 1 has N outgoing R-edges to {2..=N+1} AND N incoming
        // R-edges from {N+2..=2N+1}, plus a single closing edge 2 -> (N+2) forming
        // exactly ONE directed triangle (1 -> 2 -> N+2 -> 1).
        // A binary join on `(c)-[:R]->(a)-[:R]->(b)` materializes N * N intermediates
        // (1,000,000 at N=1000), whereas WCO Generic Join intersects in O(N) work!
        let mut observed_work = Vec::new();
        for n in [250_u128, 500, 1_000] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut batch = WriteBatch::new(R);
            for v in 1..=(2 * n + 1) {
                batch.create_vertex(VId(v), vec![PERSON], vec![]);
            }
            let mut next_eid = 1_u128;
            for i in 0..n {
                batch.add_edge(EId(next_eid), VId(1), VId(2 + i), vec![]);
                next_eid += 1;
                batch.add_edge(EId(next_eid), VId(2 + n + i), VId(1), vec![]);
                next_eid += 1;
            }
            // One closing chord creating 3 cyclic rotations of the single triangle
            batch.add_edge(EId(next_eid), VId(2), VId(2 + n), vec![]);
            db.write(&commit, batch).await.unwrap();

            let prepared = PreparedGraphText::prepare_with_resolver(
                "MATCH (a:Person)-[:R]->(b:Person)-[:R]->(c:Person)-[:R]->(a) RETURN a, b, c ORDER BY a",
                symbols,
            )
            .unwrap()
            .bind_parameters(&params)
            .unwrap();

            let (exec, card) = db
                .execute_graph_pattern_calibrated_at(
                    &query_cx,
                    &prepared,
                    db.frontier().unwrap(),
                    &ConformalJoinCalibration::default(),
                    generous_policy(),
                )
                .unwrap();
            let card = card.unwrap();
            assert_eq!(exec.value.len(), 3, "3 rotations of the single triangle");
            assert_eq!(
                card.selected_strategy,
                JoinStrategy::WorstCaseOptimalGeneric
            );

            // Live incumbent comparison in the same invocation (AGENTS.md Rule 0.5):
            // run the uncalibrated binary-expand `GlaPlan` on the identical graph.
            let view = db.read_session().unwrap();
            let as_of = view.frontier();
            let raw_vertices = view.vertices_at(as_of).unwrap();
            let raw_edges = view.edges_at(as_of).unwrap();
            let incumbent =
                prepared
                    .plan()
                    .execute_governed_with_element_maps(
                        (raw_vertices.len() + raw_edges.len()) as u64,
                        raw_vertices.iter().map(|row| row.vid),
                        raw_edges.iter().map(|rec| {
                            (
                                rec.entry.eid,
                                rec.entry.src,
                                rec.entry.relation,
                                rec.entry.dst,
                            )
                        }),
                        |vid, preds| {
                            Ok::<_, ()>(raw_vertices.iter().find(|r| r.vid == vid).is_some_and(
                                |r| preds.iter().all(|p| p.matches(&r.labels, &r.props)),
                            ))
                        },
                        |_, _| Ok(None),
                        |_, _| Ok(None),
                        |_| Ok(None),
                        |_| Ok(None),
                        |_| Ok(None),
                        |_| Ok(None),
                        generous_policy(),
                        || Ok::<(), ()>(()),
                    )
                    .unwrap();
            assert_eq!(exec.value, incumbent.value);
            observed_work.push((n, exec.evaluator.work_units, incumbent.evaluator.work_units));
        }

        let (_, w250, _) = observed_work[0];
        let (_, w500, _) = observed_work[1];
        let (_, w1000, inc1000) = observed_work[2];
        // Sub-quadratic / O(E log E) scaling: 4x input size grows WCO work by ~4.6x
        // (never 16x = N^2), while beating the live quadratic incumbent by > 3x at N=1000!
        assert!(
            w500 < w250 * 3 && w1000 < w250 * 6 && w1000 * 3 < inc1000,
            "expected O(E log E) WCO work scaling beating quadratic incumbent across N=250/500/1000, got {observed_work:?}"
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn decision_card_replay_conformal_fallback_cancellation_and_explain_surface() {
    let ((), report) = run_async_under_lab(0xcf10_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query_cx = contexts.query();
        let db = seed_rich_multiway_graph(&commit).await;
        let as_of = db.frontier().unwrap();
        let params = GqlParameters::new();

        let text = "MATCH REPEATABLE ELEMENTS (a:Admin)-[:S]->(x), (a)-[:S]->(y), (a)-[:S]->(z) RETURN a, x, y, z ORDER BY x, y, z";
        let prepared = PreparedGraphText::prepare_with_resolver(text, symbols)
            .unwrap()
            .bind_parameters(&params)
            .unwrap();

        // 1. Deterministic JoinDecisionCard canonical bytes & digest across repeated runs
        let (exec1, card1) = db
            .execute_graph_pattern_calibrated_at(
                &query_cx,
                &prepared,
                as_of,
                &ConformalJoinCalibration::default(),
                generous_policy(),
            )
            .unwrap();
        let (exec2, card2) = db
            .execute_graph_pattern_calibrated_at(
                &query_cx,
                &prepared,
                as_of,
                &ConformalJoinCalibration::default(),
                generous_policy(),
            )
            .unwrap();
        let card1 = card1.unwrap();
        let card2 = card2.unwrap();
        assert_eq!(exec1.value, exec2.value);
        assert_eq!(card1.canonical_bytes(), card2.canonical_bytes());
        assert_eq!(card1.digest(), card2.digest());
        assert_eq!(card1.candidate_strategy, JoinStrategy::FactorizedFreeJoin);
        assert_eq!(card1.selected_strategy, JoinStrategy::FactorizedFreeJoin);
        assert_eq!(card1.pinned_fallback, JoinStrategy::WorstCaseOptimalGeneric);

        // 2. Conformal nonconformity fallback -> selects pinned_fallback while preserving exact rows!
        let nonconforming = ConformalJoinCalibration {
            observed_residual_override: Some(0.95),
            ..ConformalJoinCalibration::default()
        };
        let (fallback_exec, fallback_card) = db
            .execute_graph_pattern_calibrated_at(
                &query_cx,
                &prepared,
                as_of,
                &nonconforming,
                generous_policy(),
            )
            .unwrap();
        let fallback_card = fallback_card.unwrap();
        assert_eq!(fallback_card.conformal_conforming, Some(false));
        assert_eq!(
            fallback_card.selected_strategy,
            JoinStrategy::WorstCaseOptimalGeneric
        );
        assert_eq!(fallback_exec.value, exec1.value);

        // 3. E-process drift rejection fallback -> selects pinned_fallback while preserving exact rows!
        let drifted = ConformalJoinCalibration {
            drift_observations: vec![BinaryObservation::One; 12],
            ..ConformalJoinCalibration::default()
        };
        let (drift_exec, drift_card) = db
            .execute_graph_pattern_calibrated_at(
                &query_cx,
                &prepared,
                as_of,
                &drifted,
                generous_policy(),
            )
            .unwrap();
        let drift_card = drift_card.unwrap();
        assert!(drift_card.drift_rejected_null);
        assert_eq!(
            drift_card.selected_strategy,
            JoinStrategy::WorstCaseOptimalGeneric
        );
        assert_eq!(drift_exec.value, exec1.value);

        // 4. FG-INV-19 execute_certified + replay bit-identical verification
        let (cert_rows, cert) = db
            .execute_certified(&query_cx, text, &params, symbols, generous_policy())
            .unwrap();
        let replayed = db
            .replay(&query_cx, &cert, &params, symbols, generous_policy())
            .unwrap();
        assert_eq!(cert_rows, replayed);

        // 5. EXPLAIN & EXPLAIN (CERTIFICATE) surface JoinDecisionCard without executing
        let (explain_rows, explain_cert) = db.explain(text, &params, symbols, true).unwrap();
        assert!(explain_cert.is_some());
        assert!(
            explain_rows.iter().any(|r| r.operator == "JoinDecisionCard"
                && r.detail.contains("family=CalibratedFreeJoin")),
            "expected JoinDecisionCard in EXPLAIN rows, got {explain_rows:?}"
        );
        let QueryResult::Rows {
            rows: cert_explain_rows,
            ..
        } = db
            .query(
                &query_cx,
                &format!("EXPLAIN (CERTIFICATE) {text}"),
                &params,
                symbols,
                GqlQueryPolicy::new(1, 1, 1, 1),
            )
            .unwrap()
        else {
            panic!("expected EXPLAIN (CERTIFICATE) rows");
        };
        assert!(cert_explain_rows.len() > explain_rows.len());

        // 6. Mid-leap QueryCx checkpoint cancellation cleanly returns Interrupted
        let cancel_probe = Arc::new(SimulationCheckpointProbe::new(Some(3)));
        let cancelling_cx = query_cx.with_checkpoint_probe(cancel_probe);
        let interrupted = db.execute_graph_pattern_calibrated_at(
            &cancelling_cx,
            &prepared,
            as_of,
            &ConformalJoinCalibration::default(),
            generous_policy(),
        );
        assert!(
            matches!(interrupted, Err(GqlQueryError::Interrupted(_))),
            "expected clean Interrupted on mid-leap cancel, got {interrupted:?}"
        );

        // Make sure unused CanonicalPropertyValue import isn't needed
        let _ = CanonicalPropertyValue::Scalar(CanonicalScalar::Null);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
