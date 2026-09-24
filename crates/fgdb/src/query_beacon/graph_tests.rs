//! Real Chronicle/Strata fixtures for the three-lane embedded search adapter.

use super::*;
use crate::{DatabaseKeys, MemVfs, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_beacon::expansion::{ExpansionDirection, ExpansionLimits};
use fgdb_beacon::read::Rows;
use fgdb_beacon::{DistanceMetric, ExactHybridQuery, ExactRrfProfile, HnswConfig, TextMatch, VectorSearch};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

const LABEL: LabelId = LabelId(1);
const WORDS: PropertyKeyId = PropertyKeyId(1);
const COORD: PropertyKeyId = PropertyKeyId(2);
const REL: RelationId = RelationId(1);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x81; 32], DatabaseSecurityNamespaceId([0x82; 32]), [0x83; 32])
}

fn options() -> Options {
    let mut options = Options::text(WORDS);
    options.vertex_label = Some(LABEL);
    options.projection.vector = vec![COORD];
    options.index.vector = Some(HnswConfig::new(1, DistanceMetric::SquaredEuclidean));
    options
}

fn query() -> GraphHybridQuery<'static> {
    GraphHybridQuery {
        retrieval: ExactHybridQuery {
            vector: &[0.0], text: "needle", k: 1,
            vector_candidates: 3, text_candidates: 3,
            vector_mode: VectorSearch::Exact, text_mode: TextMatch::Any,
            profile: ExactRrfProfile::default(),
        },
        graph_candidates: 2,
        graph_weight: 4,
    }
}

fn expansion() -> ExpansionSpec<'static, RelationId> {
    ExpansionSpec {
        seeds: &[VId(0)], relation: Some(REL), direction: ExpansionDirection::Outgoing,
        max_hops: 2, include_seeds: false, limits: ExpansionLimits::default(),
    }
}

async fn fixture(contexts: &PurposeContexts) -> Database<MemVfs> {
    let commit = contexts.commit();
    let mut db = Database::open_memory(&commit, keys()).await.unwrap();
    let mut batch = WriteBatch::new(REL);
    for id in 0..=4 {
        let props = if (1..=3).contains(&id) {
            vec![(WORDS, CanonicalScalar::ucs_basic_text("needle").unwrap()),
                 (COORD, CanonicalScalar::Int(id as i64))]
        } else {
            vec![]
        };
        batch.create_vertex(VId(id), vec![LABEL], props);
    }
    batch.add_edge(EId(1), VId(0), VId(3), vec![]);
    batch.add_edge(EId(2), VId(3), VId(4), vec![]);
    db.write(&commit, batch).await.unwrap();
    db
}

#[test]
fn third_lane_promotes_a_low_base_rank_and_retains_graph_only_vertices() {
    let ((), report) = run_async_under_lab(0xbeac_2001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let db = fixture(&contexts).await;
        let cx = contexts.query();
        let opts = options();
        let q = query();
        let base = db.beacon_search(&cx, &opts, Search::Hybrid(q.retrieval)).unwrap();
        assert!(matches!(base, Rows::Hybrid(rows) if rows[0].id == VId(1)));
        let hits = db.beacon_search_graph(&cx, &opts, q, expansion()).unwrap();
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert_eq!(hit.id, VId(3));
        assert_eq!(hit.vector_rank.unwrap().get(), 3);
        assert_eq!(hit.text_rank.unwrap().get(), 3);
        assert_eq!(hit.graph_rank.unwrap().get(), 1);
        assert_eq!(hit.graph_hops, Some(1));
        // Independently calculated: 1/63 + 1/63 + 4/61 = 374/3843.
        assert_eq!(hit.score.numerator(), 374);
        assert_eq!(hit.score.denominator(), 3843);

        let mut all = q;
        all.retrieval.k = 7; // Larger than the old two-lane ceiling (3 + 3).
        let hits = db.beacon_search_graph(&cx, &opts, all, expansion()).unwrap();
        assert_eq!(hits.iter().map(|hit| hit.id).collect::<Vec<_>>(),
            vec![VId(3), VId(4), VId(1), VId(2)]);
        assert_eq!(hits[1].graph_hops, Some(2));
        assert_eq!(hits[1].score.numerator(), 2);
        assert_eq!(hits[1].score.denominator(), 31);
        assert!(hits[1].text_rank.is_none() && hits[1].vector_rank.is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn directions_relations_cycles_and_excluded_transit_follow_the_declared_graph_law() {
    let ((), report) = run_async_under_lab(0xbeac_2002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(REL);
        for id in [0, 1, 2, 3, 9, u128::MAX] {
            batch.create_vertex(VId(id), vec![if id == 9 { LabelId(2) } else { LABEL }], vec![]);
        }
        for (eid, from, to) in [(1, 0, 1), (2, 1, 2), (3, 2, 0), (4, 0, 1),
            (5, 1, 1), (6, 3, 0), (7, 0, 9), (8, 9, u128::MAX)] {
            batch.add_edge(EId(eid), VId(from), VId(to), vec![]);
        }
        db.write(&commit, batch).await.unwrap();
        let mut other = WriteBatch::new(RelationId(2));
        other.add_edge(EId(9), VId(0), VId(u128::MAX), vec![]);
        db.write(&commit, other).await.unwrap();
        let mut opts = Options::text(WORDS);
        opts.vertex_label = Some(LABEL);
        let mut q = query();
        q.retrieval.profile = ExactRrfProfile::new(60, 0, 1).unwrap();
        q.retrieval.text_candidates = 0;
        q.retrieval.k = 10;
        q.graph_candidates = 10;
        let mut spec = expansion();
        spec.seeds = &[VId(0), VId(0), VId(9), VId(88)];
        for (direction, expected) in [
            (ExpansionDirection::Outgoing, vec![(1, 1), (2, 2)]),
            (ExpansionDirection::Incoming, vec![(2, 1), (3, 1), (1, 2)]),
            (ExpansionDirection::Undirected, vec![(1, 1), (2, 1), (3, 1)]),
        ] {
            spec.direction = direction;
            let mut hits = db.beacon_search_graph(&cx, &opts, q, spec).unwrap();
            hits.sort_by_key(|hit| hit.graph_rank);
            assert_eq!(hits.iter().map(|hit| (hit.id.0, hit.graph_hops.unwrap())).collect::<Vec<_>>(), expected);
        }
        spec.direction = ExpansionDirection::Outgoing;
        spec.relation = None;
        let hits = db.beacon_search_graph(&cx, &opts, q, spec).unwrap();
        assert!(hits.iter().any(|hit| hit.id == VId(u128::MAX) && hit.graph_hops == Some(1)));
        spec.max_hops = 0;
        spec.include_seeds = true;
        spec.limits.max_input_edges = 0;
        spec.limits.max_source_scratch = 0;
        let hits = db.beacon_search_graph(&cx, &opts, q, spec).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!((hits[0].id, hits[0].graph_hops), (VId(0), Some(0)));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn all_three_populations_share_the_pinned_historical_cut() {
    let ((), report) = run_async_under_lab(0xbeac_2003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let mut db = fixture(&contexts).await;
        let view = db.read_session().unwrap();
        let before = view.beacon_search_graph(&cx, &options(), query(), expansion()).unwrap();
        let mut change = WriteBatch::new(REL);
        change.delete_vertex(VId(3));
        change.add_edge(EId(3), VId(0), VId(4), vec![]);
        db.write(&contexts.commit(), change).await.unwrap();
        assert_eq!(db.beacon_search_graph(&cx, &options(), query(), expansion()).unwrap()[0].id, VId(4));
        let mut history = options();
        history.as_of = Some(view.frontier());
        assert_eq!(db.beacon_search_graph(&cx, &history, query(), expansion()).unwrap(), before);
        assert_eq!(view.beacon_search_graph(&cx, &options(), query(), expansion()).unwrap(), before);
        history.as_of = Some(db.frontier().unwrap());
        assert!(matches!(view.beacon_search_graph(&cx, &history, query(), expansion()),
            Err(Error::Read(ReadError::BeyondFrontier { .. }))));
        drop(db);
        assert_eq!(view.beacon_search_graph(&cx, &options(), query(), expansion()).unwrap(), before);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn disabled_graph_lane_is_not_admitted_and_enabled_limits_fail_without_partial_hits() {
    let ((), report) = run_async_under_lab(0xbeac_2004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let db = fixture(&contexts).await;
        let cx = contexts.query();
        let mut spec = expansion();
        for case in 0..5 {
            spec.limits = ExpansionLimits::default();
            match case {
                0 => spec.limits.max_vertices = 0,
                1 => spec.limits.max_input_edges = 0,
                2 => spec.limits.max_visited_vertices = 1,
                3 => spec.limits.max_seed_ids = 0,
                _ => spec.limits.max_source_scratch = 0,
            }
            assert!(matches!(db.beacon_search_graph(&cx, &options(), query(), spec),
                Err(Error::Index(BeaconError::ResourceLimit { .. }))));
            let mut disabled = query();
            disabled.graph_weight = 0;
            let hits = db.beacon_search_graph(&cx, &options(), disabled, spec).unwrap();
            assert_eq!(hits[0].id, VId(1));
            assert!(hits[0].graph_rank.is_none());
            disabled.graph_weight = 4;
            disabled.graph_candidates = 0;
            assert_eq!(db.beacon_search_graph(&cx, &options(), disabled, spec).unwrap(), hits);
        }
        let mut no_rows = options();
        no_rows.policy.max_result_rows = 0;
        assert!(matches!(db.beacon_search_graph(&cx, &no_rows, query(), expansion()),
            Err(Error::Index(BeaconError::ResourceLimit { resource: "result rows", .. }))));
        let mut no_work = options();
        no_work.policy.max_work_units = 0;
        let mut zero = query();
        zero.retrieval.k = 0;
        assert!(matches!(db.beacon_search_graph(&cx, &no_work, zero, expansion()),
            Err(Error::Index(BeaconError::WorkBudgetExceeded))));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_public_graph_search_checkpoint_refuses_once_and_allows_retry() {
    use fgdb_types::context::SimulationCheckpointProbe;
    use std::sync::Arc;
    let ((), report) = run_async_under_lab(0xbeac_2005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let db = fixture(&contexts).await;
        let cx = contexts.query();
        let trace = Arc::new(SimulationCheckpointProbe::new(None));
        let observed = cx.with_checkpoint_probe(Arc::clone(&trace));
        let expected = db.beacon_search_graph(&observed, &options(), query(), expansion()).unwrap();
        let frontier = db.frontier().unwrap();
        assert!(trace.calls() > 2);
        for stop in 1..=trace.calls() {
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let interrupted = cx.with_checkpoint_probe(Arc::clone(&probe));
            assert!(matches!(db.beacon_search_graph(&interrupted, &options(), query(), expansion()),
                Err(Error::Interrupted(_))), "checkpoint {stop}");
            assert_eq!(probe.calls(), stop);
            assert_eq!(db.frontier().unwrap(), frontier);
            assert_eq!(db.beacon_search_graph(&interrupted, &options(), query(), expansion()).unwrap(), expected);
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

mod authorized {
    use super::*;
    use asupersync::security::key::AuthKey;
    use fgdb_delta_types::SchemaEpoch;
    use fgdb_warden::{Authority, Error as WardenError, Grant, QueryLimits, Restriction, Scope};

    const BRANCH: &str = "host-graph-search";
    const HIDDEN: LabelId = LabelId(99);
    const SECRET: PropertyKeyId = PropertyKeyId(99);
    const NOW: u64 = 100;
    const EXPIRES: u64 = 1000;

    fn issuer() -> Authority {
        Authority::new(AuthKey::from_seed(9201), DatabaseSecurityNamespaceId([0x82; 32]),
            "host-graph", SchemaEpoch(0), 1).unwrap()
    }

    fn grant() -> Grant {
        let mut grant = Grant::read_only(BRANCH, EXPIRES, QueryLimits {
            max_nodes: 1_000_000, max_work: 1_000_000, max_rows: 1_000_000,
        });
        grant.labels = Scope::only([LABEL]);
        grant.properties = Scope::only([WORDS, COORD]);
        grant.relations = Scope::only([REL]);
        grant
    }

    async fn scoped_fixture(contexts: &PurposeContexts, hidden: bool) -> Database<MemVfs> {
        let mut db = fixture(contexts).await;
        let mut additions = WriteBatch::new(REL);
        additions.create_vertex(VId(5), vec![LABEL], vec![]);
        if hidden {
            additions.create_vertex(VId(99), vec![HIDDEN], vec![
                (WORDS, CanonicalScalar::Int(5)),
                (COORD, CanonicalScalar::ucs_basic_text("not a vector coordinate").unwrap()),
            ]);
            additions.add_edge(EId(100), VId(0), VId(99), vec![]);
            additions.add_edge(EId(101), VId(99), VId(5), vec![]);
            additions.add_edge(EId(102), VId(99), VId(3), vec![]);
            additions.set_vertex_label(VId(3), HIDDEN, true);
            additions.set_vertex_property(VId(3), SECRET,
                Some(CanonicalScalar::ucs_basic_text("private payload").unwrap()));
        }
        db.write(&contexts.commit(), additions).await.unwrap();
        if hidden {
            let mut forbidden = WriteBatch::new(RelationId(2));
            forbidden.add_edge(EId(103), VId(0), VId(5), vec![]);
            db.write(&contexts.commit(), forbidden).await.unwrap();
            let mut history = WriteBatch::new(REL);
            history.set_vertex_property(VId(99), WORDS, Some(CanonicalScalar::Bool(true)));
            db.write(&contexts.commit(), history).await.unwrap();
        }
        db
    }

    fn accepted(result: Result<Vec<GraphHybridHit>, Error<ReadError, crate::QueryError>>) -> bool {
        match result {
            Ok(_) => true,
            Err(Error::Index(BeaconError::ResourceLimit { .. } | BeaconError::WorkBudgetExceeded)) => false,
            Err(Error::Interrupted(crate::QueryError::Authorization(WardenError::LimitExceeded(_)))) => false,
            other => panic!("unexpected refusal: {other:?}"),
        }
    }

    fn minimum(mut accepts: impl FnMut(usize) -> bool) -> usize {
        let (mut low, mut high) = (0, 1_000_000);
        assert!(accepts(high));
        while low < high {
            let mid = low + (high - low) / 2;
            if accepts(mid) { high = mid; } else { low = mid + 1; }
        }
        low
    }

    #[test]
    fn hidden_bridges_fields_and_history_change_neither_hits_nor_limit_thresholds() {
        let ((), report) = run_async_under_lab(0xbeac_2101, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let clean = scoped_fixture(&contexts, false).await;
            let full = scoped_fixture(&contexts, true).await;
            let cx = contexts.query();
            let authority = issuer();
            let token = authority.issue_at(&grant(), NOW).unwrap();
            let mut spec = expansion();
            spec.relation = None; // The token, not user selection, hides relation 2.
            spec.seeds = &[VId(0), VId(99)];
            for mode in [VectorSearch::Exact, VectorSearch::Approximate { ef_search: 32 }] {
                let mut q = query();
                q.retrieval.vector_mode = mode;
                q.retrieval.k = 7;
                let expected = clean.beacon_search_graph_authorized(
                    &cx, &authority, &token, BRANCH, &options(), q, spec, || NOW,
                ).unwrap();
                assert_eq!(full.beacon_search_graph_authorized(
                    &cx, &authority, &token, BRANCH, &options(), q, spec, || NOW,
                ).unwrap(), expected);
                assert_eq!(expected.iter().map(|h| h.id).collect::<Vec<_>>(),
                    vec![VId(3), VId(4), VId(1), VId(2)]);
                // Three signed allowances; every check gets its own permit.
                for dimension in 0..3 {
                    let threshold = |db: &Database<MemVfs>| minimum(|limit| {
                        let restriction = match dimension {
                            0 => Restriction::MaxWork(limit as u64),
                            1 => Restriction::MaxNodes(limit as u64),
                            _ => Restriction::MaxRows(limit as u64),
                        };
                        let narrowed = token.attenuate(restriction).unwrap();
                        accepted(db.beacon_search_graph_authorized(
                            &cx, &authority, &narrowed, BRANCH, &options(), q, spec, || NOW,
                        ))
                    });
                    let before = threshold(&clean);
                    assert_eq!(threshold(&full), before, "signed dimension {dimension}");
                    if dimension == 1 { assert_eq!(before, 6); }
                    if dimension == 2 { assert_eq!(before, 7); }
                }
                // Native work plus graph admission, traversal and scratch.
                for dimension in 0..5 {
                    let threshold = |db: &Database<MemVfs>| minimum(|limit| {
                        let mut opts = options();
                        let mut bounded = spec;
                        match dimension {
                            0 => opts.policy.max_work_units = limit,
                            1 => bounded.limits.max_vertices = limit,
                            2 => bounded.limits.max_input_edges = limit,
                            3 => bounded.limits.max_visited_vertices = limit,
                            _ => bounded.limits.max_source_scratch = limit,
                        }
                        accepted(db.beacon_search_graph_authorized(
                            &cx, &authority, &token, BRANCH, &opts, q, bounded, || NOW,
                        ))
                    });
                    let before = threshold(&clean);
                    assert_eq!(threshold(&full), before, "native dimension {dimension}");
                    match dimension {
                        1 => assert_eq!(before, 6),
                        2 | 4 => assert_eq!(before, 2),
                        3 => assert_eq!(before, 3),
                        _ => assert!(before > 0),
                    }
                }
                let mut masked = options();
                masked.projection.text = Some(SECRET);
                masked.projection.vector = vec![SECRET];
                let expected = clean.beacon_search_graph_authorized(
                    &cx, &authority, &token, BRANCH, &masked, q, spec, || NOW,
                ).unwrap();
                assert_eq!(full.beacon_search_graph_authorized(
                    &cx, &authority, &token, BRANCH, &masked, q, spec, || NOW,
                ).unwrap(), expected);
                assert_eq!(expected.iter().map(|h| h.id).collect::<Vec<_>>(), vec![VId(3), VId(4)]);
                assert!(expected.iter().all(|h| h.text_rank.is_none() && h.vector_rank.is_none()));
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn historical_authorization_precedes_projection_and_graph_transit() {
        let ((), report) = run_async_under_lab(0xbeac_2102, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let mut db = scoped_fixture(&contexts, true).await;
            let cx = contexts.query();
            let authority = issuer();
            let token = authority.issue_at(&grant(), NOW).unwrap();
            let mut q = query();
            q.retrieval.k = 7;
            let before = db.beacon_search_graph_authorized(
                &cx, &authority, &token, BRANCH, &options(), q, expansion(), || NOW,
            ).unwrap();
            let mut history = options();
            history.as_of = Some(db.frontier().unwrap());
            let mut change = WriteBatch::new(REL);
            change.set_vertex_label(VId(3), LABEL, false);
            change.set_vertex_property(VId(3), WORDS, Some(CanonicalScalar::Int(5)));
            change.set_vertex_property(VId(3), COORD, Some(CanonicalScalar::Bool(false)));
            db.write(&contexts.commit(), change).await.unwrap();
            let current = db.beacon_search_graph_authorized(
                &cx, &authority, &token, BRANCH, &options(), q, expansion(), || NOW,
            ).unwrap();
            assert!(current.iter().all(|h| h.id != VId(3) && h.id != VId(4) && h.graph_rank.is_none()));
            assert_eq!(db.beacon_search_graph_authorized(
                &cx, &authority, &token, BRANCH, &history, q, expansion(), || NOW,
            ).unwrap(), before);
            let mut denied = expansion();
            denied.relation = Some(RelationId(2));
            denied.limits.max_input_edges = 0;
            denied.limits.max_source_scratch = 0;
            let current = db.beacon_search_graph_authorized(
                &cx, &authority, &token, BRANCH, &options(), q, denied, || NOW,
            ).unwrap();
            assert!(current.iter().all(|h| h.graph_rank.is_none()));
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn expiry_at_every_clock_boundary_refuses_even_empty_final_delivery() {
        let ((), report) = run_async_under_lab(0xbeac_2103, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let db = fixture(&contexts).await;
            let cx = contexts.query();
            let authority = issuer();
            let token = authority.issue_at(&grant(), NOW).unwrap();
            for k in [0, 1] {
                let mut q = query();
                q.retrieval.k = k;
                let count = Cell::new(0usize);
                db.beacon_search_graph_authorized(
                    &cx, &authority, &token, BRANCH, &options(), q, expansion(), || {
                        count.set(count.get() + 1);
                        NOW
                    },
                ).unwrap();
                assert!(count.get() > 10);
                for cut in 0..count.get() {
                    let seen = Cell::new(0usize);
                    assert!(matches!(db.beacon_search_graph_authorized(
                        &cx, &authority, &token, BRANCH, &options(), q, expansion(), || {
                            let at = seen.get();
                            seen.set(at + 1);
                            if at >= cut { EXPIRES } else { NOW }
                        },
                    ), Err(Error::Interrupted(crate::QueryError::Authorization(WardenError::Expired)))));
                    assert_eq!(seen.get(), cut + 1);
                }
            }
            let mut invalid = options();
            invalid.index.vector.as_mut().unwrap().dimensions = 0;
            assert!(matches!(db.beacon_search_graph_authorized(
                &cx, &authority, &token, "wrong-branch", &invalid, query(), expansion(), || NOW,
            ), Err(Error::Interrupted(crate::QueryError::Authorization(_)))));
            assert_eq!(contexts.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
