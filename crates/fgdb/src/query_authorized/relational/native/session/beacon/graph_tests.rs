use super::*;
use crate::{Database, DatabaseKeys, MemVfs, WriteBatch};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb_beacon::expansion::{ExpansionDirection as Direction, ExpansionLimits};
use fgdb_beacon::{
    DistanceMetric, ExactHybridQuery, ExactRrfProfile, HnswConfig, TextMatch, VectorSearch,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, SchemaEpoch};
use fgdb_gql::{GqlQueryPolicy, GraphSymbol, GraphSymbolKind};
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use fgdb_warden::{Authority, Error as WardenError, Grant, QueryLimits, Scope};
use std::sync::Arc;

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x55; 32]);
const L: LabelId = LabelId(1);
const H: LabelId = LabelId(99);
const T: PropertyKeyId = PropertyKeyId(1);
const X: PropertyKeyId = PropertyKeyId(2);
const R: RelationId = RelationId(1);
const HIDDEN_R: RelationId = RelationId(99);
const NOW: u64 = 100;
const EXPIRES: u64 = 10_000;

fn issuer() -> Authority {
    Authority::new(AuthKey::from_seed(0x55), NS, "graph", SchemaEpoch(0), 1).unwrap()
}
fn grant() -> Grant {
    let mut grant = Grant::read_only(
        "main",
        EXPIRES,
        QueryLimits {
            max_nodes: 4,
            max_work: 1_000_000,
            max_rows: 4,
        },
    );
    grant.labels = Scope::only([L]);
    grant.properties = Scope::only([T, X]);
    grant.relations = Scope::only([R]);
    grant
}
fn host() -> GqlQueryPolicy {
    GqlQueryPolicy::new(8, 4, 1_000_000, 1000)
}
fn catalog(_: GraphSymbolKind, _: &str) -> Option<GraphSymbol> {
    None
}
fn options() -> Options {
    let mut options = Options::text(T);
    options.projection.vector = vec![X];
    options.index.vector = Some(HnswConfig::new(1, DistanceMetric::SquaredEuclidean));
    options
}
fn query() -> GraphHybridQuery<'static> {
    GraphHybridQuery {
        retrieval: ExactHybridQuery {
            vector: &[0.0],
            text: "graph",
            k: 4,
            vector_candidates: 4,
            text_candidates: 4,
            vector_mode: VectorSearch::Exact,
            text_mode: TextMatch::Any,
            profile: ExactRrfProfile::default(),
        },
        graph_candidates: 4,
        graph_weight: 100,
    }
}
fn expansion(seeds: &[VId]) -> ExpansionSpec<'_, RelationId> {
    ExpansionSpec {
        seeds,
        relation: None,
        direction: Direction::Outgoing,
        max_hops: 3,
        include_seeds: false,
        limits: ExpansionLimits::default(),
    }
}
async fn database(cx: &CommitCx, hidden: bool) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x56; 32], NS, [0x57; 32]))
        .await
        .unwrap();
    let mut batch = WriteBatch::new(R);
    for id in 1..=3 {
        batch.create_vertex(
            VId(id),
            if hidden { vec![L, H] } else { vec![L] },
            vec![
                (T, CanonicalScalar::ucs_basic_text("graph").unwrap()),
                (X, CanonicalScalar::Int(id as i64)),
            ],
        );
    }
    batch.create_vertex(VId(4), vec![L], vec![]); // Graph-only result.
    for (id, src, dst) in [(1, 1, 2), (2, 1, 2), (3, 2, 3), (4, 3, 4)] {
        batch.add_edge(EId(id), VId(src), VId(dst), vec![]);
    }
    if hidden {
        for id in 100..116 {
            batch.create_vertex(
                VId(id),
                vec![H],
                vec![
                    (T, CanonicalScalar::Int(123)),
                    (
                        X,
                        CanonicalScalar::ucs_basic_text("hidden bad type").unwrap(),
                    ),
                ],
            );
            batch.add_edge(EId(id), VId(1), VId(id), vec![]);
            batch.add_edge(EId(id + 100), VId(id), VId(4), vec![]);
        }
    }
    db.write(cx, batch).await.unwrap();
    if hidden {
        let mut shortcut = WriteBatch::new(HIDDEN_R);
        shortcut.add_edge(EId(900), VId(1), VId(4), vec![]);
        db.write(cx, shortcut).await.unwrap();
    }
    db
}

#[test]
fn scoped_session_uses_the_native_three_lane_result_in_every_direction() {
    let ((), report) = run_async_under_lab(0xbeac_4201, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit(), true).await;
        let clean = database(&c.commit(), false).await;
        let issuer = issuer();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut session = db
            .authorized_read_session(&c.query(), &issuer, &token, "main", catalog, host(), || NOW)
            .unwrap();
        for direction in [
            Direction::Outgoing,
            Direction::Incoming,
            Direction::Undirected,
        ] {
            for mode in [
                VectorSearch::Exact,
                VectorSearch::Approximate { ef_search: 16 },
            ] {
                let mut q = query();
                q.retrieval.vector_mode = mode;
                for seeds in [vec![VId(1)], vec![VId(4), VId(4)], vec![VId(100)], vec![]] {
                    let mut e = expansion(&seeds);
                    e.direction = direction;
                    let actual = session
                        .beacon_search_graph(&c.query(), &options(), q, e)
                        .unwrap();
                    assert_eq!(
                        actual,
                        clean
                            .beacon_search_graph(&c.query(), &options(), q, e)
                            .unwrap()
                    );
                    assert_eq!(
                        actual,
                        db.beacon_search_graph_authorized(
                            &c.query(),
                            &issuer,
                            &token,
                            "main",
                            &options(),
                            q,
                            e,
                            || NOW,
                        )
                        .unwrap()
                    );
                }
            }
        }
        let rows = session
            .beacon_search_graph(&c.query(), &options(), query(), expansion(&[VId(1)]))
            .unwrap();
        // ubs:ignore -- test lookup by vertex id, not secret material.
        let graph_only = rows.iter().find(|hit| hit.id == VId(4)).unwrap();
        assert_eq!(graph_only.graph_hops, Some(3));
        assert_eq!(graph_only.vector_rank, None);
        assert_eq!(graph_only.text_rank, None);
        let mut hidden_relation = expansion(&[VId(1)]);
        hidden_relation.relation = Some(HIDDEN_R);
        assert!(
            session
                .beacon_search_graph(&c.query(), &options(), query(), hidden_relation)
                .unwrap()
                .iter()
                .all(|hit| hit.graph_hops.is_none())
        );
        session.close();
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn writer_changes_and_drop_cannot_mix_new_topology_into_the_session_pin() {
    let ((), report) = run_async_under_lab(0xbeac_4202, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = database(&c.commit(), true).await;
        let at = db.frontier().unwrap();
        let issuer = issuer();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut session = db
            .authorized_read_session(&c.query(), &issuer, &token, "main", catalog, host(), || NOW)
            .unwrap();
        let expected = session
            .beacon_search_graph(&c.query(), &options(), query(), expansion(&[VId(1)]))
            .unwrap();
        let mut update = WriteBatch::new(R);
        update.set_vertex_label(VId(2), L, false);
        update.set_vertex_property(
            VId(2),
            X,
            Some(CanonicalScalar::ucs_basic_text("now hidden").unwrap()),
        );
        update.delete_edge(EId(4));
        db.write(&c.commit(), update).await.unwrap();
        let mut current = db
            .authorized_read_session(&c.query(), &issuer, &token, "main", catalog, host(), || NOW)
            .unwrap();
        assert_ne!(
            current
                .beacon_search_graph(&c.query(), &options(), query(), expansion(&[VId(1)]))
                .unwrap(),
            expected
        );
        let mut historical = options();
        historical.as_of = Some(at);
        assert_eq!(
            current
                .beacon_search_graph(&c.query(), &historical, query(), expansion(&[VId(1)]))
                .unwrap(),
            expected
        );
        historical.as_of = Some(db.frontier().unwrap());
        assert!(matches!(
            session.beacon_search_graph(&c.query(), &historical, query(), expansion(&[VId(1)])),
            Err(super::super::SearchError::Read(_))
        ));
        drop(db);
        assert_eq!(
            session
                .beacon_search_graph(&c.query(), &options(), query(), expansion(&[VId(1)]))
                .unwrap(),
            expected
        );
        current.close();
        session.close();
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn vertices_and_parallel_edges_share_record_and_scratch_allowances() {
    let ((), report) = run_async_under_lab(0xbeac_4203, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let issuer = issuer();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        for hidden in [false, true] {
            let db = database(&c.commit(), hidden).await;
            let mut session = db
                .authorized_read_session(
                    &c.query(),
                    &issuer,
                    &token,
                    "main",
                    catalog,
                    GqlQueryPolicy::new(7, 4, 1_000_000, 1000),
                    || NOW,
                )
                .unwrap();
            let result =
                session.beacon_search_graph(&c.query(), &options(), query(), expansion(&[VId(1)]));
            assert!(matches!(result,
                Err(super::super::SearchError::Interrupted(QueryError::Pattern(GqlQueryError::Rows(error))))
                    if error.dimension == GqlBudgetDimension::SnapshotRecords && error.limit == 7 && error.observed == 8
            ));
            assert!(!session.is_closed());
            session.close();
            let mut session = db
                .authorized_read_session(
                    &c.query(),
                    &issuer,
                    &token,
                    "main",
                    catalog,
                    host(),
                    || NOW,
                )
                .unwrap();
            let mut o = options();
            o.policy.max_source_scratch = 7;
            assert!(matches!(
                session.beacon_search_graph(&c.query(), &o, query(), expansion(&[VId(1)])),
                Err(super::super::SearchError::Index(
                    BeaconError::ResourceLimit {
                        resource: "admitted graph scratch",
                        limit: 7,
                    }
                ))
            ));
            o.policy.max_source_scratch = 8;
            assert_eq!(
                session
                    .beacon_search_graph(&c.query(), &o, query(), expansion(&[VId(1)]))
                    .unwrap()
                    .len(),
                4
            );
            let mut e = expansion(&[VId(1)]);
            e.limits.max_input_edges = 3;
            assert!(matches!(
                session.beacon_search_graph(&c.query(), &options(), query(), e),
                Err(super::super::SearchError::Index(
                    BeaconError::ResourceLimit {
                        resource: "expansion input edges",
                        limit: 3,
                    }
                ))
            ));
            session.close();
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn final_k_and_graph_structure_limits_cannot_escape_host_policy() {
    let ((), report) = run_async_under_lab(0xbeac_4204, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit(), false).await;
        let issuer = issuer();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut session = db
            .authorized_read_session(&c.query(), &issuer, &token, "main", catalog, host(), || NOW)
            .unwrap();
        let mut too_many = query();
        too_many.retrieval.k = 5;
        assert!(matches!(
            session.beacon_search_graph(&c.query(), &options(), too_many, expansion(&[VId(1)])),
            Err(super::super::SearchError::Interrupted(
                QueryError::Authorization(WardenError::LimitExceeded(
                    fgdb_warden::LimitDimension::Rows
                ),)
            ))
        ));
        let mut e = expansion(&[VId(1)]);
        e.limits = ExpansionLimits {
            max_vertices: 0,
            max_input_edges: 0,
            max_visited_vertices: 0,
            max_seed_ids: 0,
            max_source_scratch: 0,
        };
        for (weight, candidates) in [(0, 4), (100, 0)] {
            let mut q = query();
            q.graph_weight = weight;
            q.graph_candidates = candidates;
            let rows = session
                .beacon_search_graph(&c.query(), &options(), q, e)
                .unwrap();
            assert_eq!(rows.len(), 3);
            assert!(rows.iter().all(|hit| hit.graph_hops.is_none()));
        }
        session.close();
        // A caller's huge graph seed/structure allowances cannot widen the
        // host scratch ceiling (four records fit, five requested seeds do not).
        let mut session = db
            .authorized_read_session(
                &c.query(),
                &issuer,
                &token,
                "main",
                catalog,
                GqlQueryPolicy::new(100, 4, 1_000_000, 4),
                || NOW,
            )
            .unwrap();
        assert!(matches!(
            session.beacon_search_graph(
                &c.query(),
                &options(),
                query(),
                expansion(&[VId(1), VId(1), VId(1), VId(1), VId(1)])
            ),
            Err(super::super::SearchError::Index(
                BeaconError::ResourceLimit {
                    resource: "expansion seed IDs",
                    limit: 4,
                }
            ))
        ));
        session.close();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_graph_checkpoint_refuses_without_rows_and_retry_uses_the_same_pin() {
    let ((), report) = run_async_under_lab(0xbeac_4205, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit(), true).await;
        let issuer = issuer();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut session = db
            .authorized_read_session(&c.query(), &issuer, &token, "main", catalog, host(), || NOW)
            .unwrap();
        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        let expected = session
            .beacon_search_graph(
                &c.query().with_checkpoint_probe(probe.clone()),
                &options(),
                query(),
                expansion(&[VId(1)]),
            )
            .unwrap();
        assert!(probe.calls() > 20);
        for cut in 1..=probe.calls() {
            let cut_probe = Arc::new(SimulationCheckpointProbe::new(Some(cut)));
            let result = session.beacon_search_graph(
                &c.query().with_checkpoint_probe(cut_probe),
                &options(),
                query(),
                expansion(&[VId(1)]),
            );
            assert!(
                matches!(result, Err(super::super::SearchError::Interrupted(_))),
                "cut={cut}: {result:?}"
            );
            assert!(!session.is_closed());
            assert_eq!(
                session
                    .beacon_search_graph(&c.query(), &options(), query(), expansion(&[VId(1)]))
                    .unwrap(),
                expected
            );
        }
        session.close();
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn expiry_retires_the_session_even_at_the_final_empty_delivery_boundary() {
    let ((), report) = run_async_under_lab(0xbeac_4206, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit(), false).await;
        let issuer = issuer();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        for empty in [false, true] {
            let mut o = options();
            if empty {
                o.vertex_label = Some(H);
            }
            let seen = Cell::new(0);
            let mut baseline = db
                .authorized_read_session(
                    &c.query(),
                    &issuer,
                    &token,
                    "main",
                    catalog,
                    host(),
                    || {
                        seen.set(seen.get() + 1);
                        NOW
                    },
                )
                .unwrap();
            seen.set(0);
            let rows = baseline
                .beacon_search_graph(&c.query(), &o, query(), expansion(&[VId(1)]))
                .unwrap();
            assert_eq!(rows.is_empty(), empty);
            let calls = seen.get();
            baseline.close();
            for cut in [0, 1, calls / 2, calls - 2, calls - 1] {
                let seen = Cell::new(0);
                let stop = Cell::new(false);
                let mut session = db
                    .authorized_read_session(
                        &c.query(),
                        &issuer,
                        &token,
                        "main",
                        catalog,
                        host(),
                        || {
                            let at = seen.get();
                            seen.set(at + 1);
                            if stop.get() && at >= cut {
                                EXPIRES
                            } else {
                                NOW
                            }
                        },
                    )
                    .unwrap();
                seen.set(0);
                stop.set(true);
                assert!(matches!(
                    session.beacon_search_graph(&c.query(), &o, query(), expansion(&[VId(1)])),
                    Err(super::super::SearchError::Interrupted(
                        QueryError::Authorization(WardenError::Expired)
                    ))
                ));
                assert!(session.is_closed());
                assert_eq!(seen.get(), cut + 1);
            }
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
