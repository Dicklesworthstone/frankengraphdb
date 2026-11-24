use super::*;
use crate::{DatabaseKeys, MemVfs, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_gql::{GqlParameters, PreparedGraphSetText, PreparedGraphText};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts};

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}

/// The rebuild establishes a baseline. Each fixture selects labeled owners,
/// so adding an unlabeled vertex advances the frontier with an empty result delta.
pub(super) async fn check_first_tick_after_rebuild<'a>(
    db: &mut Database<MemVfs>,
    cx: &QueryCx,
    commit: &fgdb_types::CommitCx,
    handles: impl IntoIterator<Item = &'a StandingQueryHandle>,
) {
    let from = db.frontier().unwrap();
    let handles: Vec<_> = handles.into_iter().collect();
    assert!(!handles.is_empty());
    for handle in &handles {
        for requested in [CommitSeq(from.0.checked_sub(1).unwrap()), from] {
            assert!(matches!(
                db.standing_native_delta(cx, handle, requested, policy()),
                Err(StandingQueryError::DeltaUnavailable { from: refused, frontier })
                    if refused == requested && frontier == from
            ));
        }
    }
    let mut change = WriteBatch::new(RelationId(1));
    change.create_vertex(VId(0xf00d_fa11), vec![], vec![]);
    let next = db.write(commit, change).await.unwrap();
    assert_eq!(next, CommitSeq(from.0 + 1));
    for handle in handles {
        for _ in 0..2 {
            let (at, delta) = db
                .standing_native_delta(cx, handle, from, policy())
                .unwrap();
            assert_eq!(at, next);
            assert!(delta.is_empty());
        }
    }
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x61; 32],
        DatabaseSecurityNamespaceId([0x62; 32]),
        [0x63; 32],
    )
}
fn none(_: fgdb_gql::GraphSymbolKind, _: &str) -> Option<fgdb_gql::GraphSymbol> {
    None
}
fn bound() -> PreparedGraphSet {
    PreparedGraphSetText::prepare("(MATCH (n) RETURN n AS id UNION ALL MATCH (n) RETURN n AS id) EXCEPT ALL MATCH (n) RETURN n AS id", none)
        .unwrap().bind_parameters(&GqlParameters::new()).unwrap()
}
fn addresses(db: &Database<MemVfs>, first: usize, root: usize) -> Vec<usize> {
    db.standing_queries[first..=root]
        .iter()
        .map(|query| match query {
            StandingQuery::Rows { source, .. } => source.as_ref() as *const _ as usize,
            StandingQuery::Set(query) => query.as_ref() as *const _ as usize,
            StandingQuery::Closure(query) => query.as_ref() as *const _ as usize,
            StandingQuery::Projection(query) => query.as_ref() as *const _ as usize,
            StandingQuery::Join(query) => query.as_ref() as *const _ as usize,
            _ => panic!("not a native set node"),
        })
        .collect()
}

#[test]
fn every_circuit_stage_refusal_and_unwind_drops_only_the_unpublished_suffix() {
    let ((), report) = run_async_under_lab(0x6e73_7210, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        seed.create_vertex(VId(1), vec![], vec![]);
        db.write(&commit, seed).await.unwrap();
        let prior = db
            .register_standing_native(
                &cx,
                "MATCH (n) RETURN n AS id",
                &GqlParameters::new(),
                none,
                policy(),
            )
            .unwrap();
        let before = db.standing_queries.len();
        let answer = db.standing_native_query(&cx, &prior, policy()).unwrap();
        let query = bound();
        let mut calls = 0;
        {
            let mut staged = Staging::new(&mut db);
            staged
                .compile(&cx, &query, policy(), &mut || {
                    calls += 1;
                    Ok(())
                })
                .unwrap();
            assert_eq!(staged.database.standing_queries.len(), before + 5);
            // Deliberate downstream drop even after every node prepared.
        }
        assert_eq!(db.standing_queries.len(), before);
        for stop in 1..=calls {
            let mut visited = 0;
            {
                let mut staged = Staging::new(&mut db);
                assert!(
                    staged
                        .compile(&cx, &query, policy(), &mut || {
                            visited += 1;
                            if visited == stop {
                                Err(StandingQueryError::Maintenance(
                                    StandingQueryFailure::Interrupted,
                                ))
                            } else {
                                Ok(())
                            }
                        })
                        .is_err()
                );
            }
            assert_eq!(visited, stop);
            assert_eq!(db.standing_queries.len(), before);
            assert_eq!(
                db.standing_native_query(&cx, &prior, policy()).unwrap(),
                answer
            );
        }
        let leaf = PreparedGraphSet::from(
            PreparedGraphText::prepare("MATCH (n) RETURN n AS id", none)
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap(),
        );
        let invalid = leaf
            .clone()
            .combine(
                GraphSetOperation::Union,
                GraphSetQuantifier::All,
                leaf.with_page(1, None),
            )
            .unwrap();
        assert!(register(&mut db, &cx, &invalid, policy()).is_err());
        assert_eq!(
            db.standing_queries.len(),
            before,
            "valid left operand must be reclaimed"
        );
        // A panic before acceptance uses the same Drop guard, not a special
        // cancellation mode. No expected panic escapes the lab supervisor.
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut staged = Staging::new(&mut db);
            staged
                .compile(&cx, &query, policy(), &mut || Ok(()))
                .unwrap();
            panic!("injected downstream unwind");
        }));
        assert!(panic.is_err());
        assert_eq!(db.standing_queries.len(), before);
        assert_eq!(
            db.standing_native_query(&cx, &prior, policy()).unwrap(),
            answer
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn complete_native_rebuild_replaces_no_old_node_until_every_stage_is_accepted() {
    let ((), report) = run_async_under_lab(0x6e73_7211, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        seed.create_vertex(VId(1), vec![], vec![]);
        db.write(&commit, seed).await.unwrap();
        let handle = register(&mut db, &cx, &bound(), policy()).unwrap();
        let Layout::Circuit { first, .. } = handle.native.as_deref().unwrap() else {
            panic!("circuit layout");
        };
        let first = *first;
        let root = handle.index;
        let size = db.standing_queries.len();
        let before = db.standing_native_query(&cx, &handle, policy()).unwrap();
        let mut calls = 0;
        let private_inputs = handle.native.as_deref().unwrap().private_inputs().unwrap();
        rebuild_checked(
            &mut db,
            &cx,
            first,
            root,
            private_inputs,
            policy(),
            &mut || {
                calls += 1;
                Ok(())
            },
        )
        .unwrap();
        assert!(calls > 0);
        for stop in 1..=calls {
            let identity = addresses(&db, first, root);
            let mut visited = 0;
            assert!(
                rebuild_checked(
                    &mut db,
                    &cx,
                    first,
                    root,
                    private_inputs,
                    policy(),
                    &mut || {
                        visited += 1;
                        if visited == stop {
                            Err(StandingQueryError::Maintenance(
                                StandingQueryFailure::Interrupted,
                            ))
                        } else {
                            Ok(())
                        }
                    }
                )
                .is_err()
            );
            assert_eq!(visited, stop);
            assert_eq!(addresses(&db, first, root), identity);
            assert_eq!(db.standing_queries.len(), size);
            assert_eq!(
                db.standing_native_query(&cx, &handle, policy()).unwrap(),
                before
            );
        }
        db.rebuild_standing_query(&cx, &handle, policy()).unwrap();
        assert_eq!(db.standing_queries.len(), size);
        let mut next = WriteBatch::new(RelationId(1));
        next.create_vertex(VId(2), vec![], vec![]);
        db.write(&commit, next).await.unwrap();
        let (_, QueryResult::Rows { rows, .. }) =
            db.standing_native_query(&cx, &handle, policy()).unwrap()
        else {
            panic!("native rows");
        };
        assert_eq!(
            rows.len(),
            2,
            "rebased dependencies must read original public indexes"
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn reachability_symbols(
    kind: fgdb_gql::GraphSymbolKind,
    name: &str,
) -> Option<fgdb_gql::GraphSymbol> {
    match (kind, name) {
        (fgdb_gql::GraphSymbolKind::Relation, "R") => {
            Some(fgdb_gql::GraphSymbol::Relation(RelationId(1)))
        }
        (fgdb_gql::GraphSymbolKind::Relation, "S") => {
            Some(fgdb_gql::GraphSymbol::Relation(RelationId(2)))
        }
        (fgdb_gql::GraphSymbolKind::Label, "L") => Some(fgdb_gql::GraphSymbol::Label(LabelId(1))),
        (fgdb_gql::GraphSymbolKind::Property, "p") => Some(fgdb_gql::GraphSymbol::Property(
            fgdb_delta_types::PropertyKeyId(1),
        )),
        _ => None,
    }
}

#[derive(Clone, Copy)]
enum ReachabilityProfile {
    Plain,
    Bounded,
    Properties,
    BoundedProperties,
}

#[test]
fn native_reachability_registration_and_rebuild_are_atomic_at_every_stage_and_row_limit() {
    reachability_registration_and_rebuild(false, false, ReachabilityProfile::Plain);
}

#[test]
fn native_reflexive_reachability_registration_and_rebuild_preserve_private_lifetime_support() {
    reachability_registration_and_rebuild(true, false, ReachabilityProfile::Plain);
}

#[test]
fn selected_reachability_registration_and_rebuild_keep_endpoint_owners_atomic() {
    reachability_registration_and_rebuild(true, true, ReachabilityProfile::Plain);
}

#[test]
fn bounded_walk_registration_and_rebuild_preserve_shared_hop_dependencies_atomically() {
    reachability_registration_and_rebuild(false, false, ReachabilityProfile::Bounded);
}

#[test]
fn selected_reflexive_bounded_walk_keeps_private_lifetimes_and_endpoint_owners_atomic() {
    reachability_registration_and_rebuild(true, true, ReachabilityProfile::Bounded);
}

#[test]
fn property_reachability_registration_and_rebuild_keep_payload_joins_atomic() {
    reachability_registration_and_rebuild(true, true, ReachabilityProfile::Properties);
}

#[test]
fn bounded_property_walk_registration_and_rebuild_keep_payload_joins_atomic() {
    reachability_registration_and_rebuild(true, true, ReachabilityProfile::BoundedProperties);
}

fn reachability_registration_and_rebuild(
    reflexive: bool,
    selected: bool,
    profile: ReachabilityProfile,
) {
    let bounded = matches!(
        profile,
        ReachabilityProfile::Bounded | ReachabilityProfile::BoundedProperties
    );
    let property_payloads = matches!(
        profile,
        ReachabilityProfile::Properties | ReachabilityProfile::BoundedProperties
    );
    let ((), report) = run_async_under_lab(0x6e73_7c11, move |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for id in 1..=3 {
            seed.create_vertex(
                VId(id),
                if selected { vec![LabelId(1)] } else { vec![] },
                if property_payloads {
                    vec![(
                        fgdb_delta_types::PropertyKeyId(1),
                        CanonicalScalar::Int(i64::try_from(id).unwrap()),
                    )]
                } else {
                    vec![]
                },
            );
        }
        for id in 1..=8 {
            seed.add_edge(EId(id), VId(1), VId(2), vec![]);
        }
        seed.add_edge(EId(9), VId(2), VId(3), vec![]);
        db.write(&commit, seed).await.unwrap();
        let params = GqlParameters::new();
        let prior = db
            .register_standing_native(&cx, "MATCH (n) RETURN n", &params, none, policy())
            .unwrap();
        let before = db.standing_queries.len();
        let prior_answer = db.standing_native_query(&cx, &prior, policy()).unwrap();
        let text = if bounded && property_payloads {
            "MATCH REPEATABLE ELEMENTS WALK (a:L)-[:R*0..3]->(b:L) WHERE a.p>=1 AND b.p>=1 RETURN DISTINCT a.p AS origin,b.p AS target"
        } else if property_payloads {
            "MATCH (a:L)-[:R*0..]->(b:L) WHERE a.p>=1 AND b.p>=1 RETURN DISTINCT a.p AS origin,b.p AS target"
        } else if bounded && selected {
            "MATCH REPEATABLE ELEMENTS WALK (a:L)-[:R*0..3]->(b:L) RETURN DISTINCT a,b"
        } else if bounded && reflexive {
            "MATCH REPEATABLE ELEMENTS WALK (a)-[:R*0..3]->(b) RETURN DISTINCT a,b"
        } else if bounded {
            "MATCH REPEATABLE ELEMENTS WALK (a)-[:R*1..3]->(b) RETURN DISTINCT a,b"
        } else if selected {
            "MATCH (a:L)-[:R*0..]->(b:L) RETURN DISTINCT a,b"
        } else if reflexive {
            "MATCH (a)-[:R*0..]->(b) RETURN DISTINCT a,b"
        } else {
            "MATCH (a)-[:R*]->(b) RETURN DISTINCT a,b"
        };
        let nodes = match (bounded, reflexive) {
            (true, true) => 11,
            (true, false) => 9,
            (false, true) => 5,
            (false, false) => 3,
        } + if selected || property_payloads { 4 } else { 0 };
        let rows = if reflexive { 6 } else { 3 };
        let prepared = PreparedNativeRead::prepare(text, &params, reachability_symbols).unwrap();
        assert_eq!(
            prepared.subscription_footprint(&cx, &params).unwrap().0,
            nodes as u64,
            "shared sources are charged once, before any registry mutation"
        );
        let query: PreparedGraphSet = PreparedGraphText::prepare(text, reachability_symbols)
            .unwrap()
            .bind_parameters(&params)
            .unwrap()
            .into();
        let exact = GqlQueryPolicy::new(1000, rows, 10_000_000, 10_000_000);
        let too_few = GqlQueryPolicy::new(1000, rows - 1, 10_000_000, 10_000_000);
        assert!(matches!(
            register(&mut db, &cx, &query, too_few),
            Err(StandingQueryError::Maintenance(
                StandingQueryFailure::ResultBudget
            ))
        ));
        assert_eq!(db.standing_queries.len(), before);
        let mut calls = 0;
        {
            let mut staged = Staging::new(&mut db);
            staged
                .compile(&cx, &query, exact, &mut || {
                    calls += 1;
                    Ok(())
                })
                .unwrap();
            assert_eq!(staged.database.standing_queries.len(), before + nodes);
        }
        assert!(calls >= nodes);
        assert_eq!(db.standing_queries.len(), before);
        for stop in 1..=calls {
            let mut visited = 0;
            {
                let mut staged = Staging::new(&mut db);
                let error = staged
                    .compile(&cx, &query, exact, &mut || {
                        visited += 1;
                        if visited == stop {
                            Err(StandingQueryError::Maintenance(
                                StandingQueryFailure::Interrupted,
                            ))
                        } else {
                            Ok(())
                        }
                    })
                    .unwrap_err();
                assert!(matches!(
                    error,
                    StandingQueryError::Maintenance(StandingQueryFailure::Interrupted)
                ));
            }
            assert_eq!(visited, stop);
            assert_eq!(db.standing_queries.len(), before);
            assert_eq!(
                db.standing_native_query(&cx, &prior, policy()).unwrap(),
                prior_answer
            );
        }
        let handle = register(&mut db, &cx, &query, exact).unwrap();
        assert_eq!(db.standing_queries.len(), before + nodes);
        let expected = db.standing_native_bag(&cx, &handle, exact).unwrap().1;
        assert_eq!(expected.len() as u64, rows);
        assert_eq!(
            db.standing_native_bag(&cx, &handle, exact).unwrap().1,
            expected
        );
        let Layout::Circuit {
            first,
            private_inputs,
            ..
        } = handle.native.as_deref().unwrap()
        else {
            panic!("owned reachability circuit");
        };
        let mut expected_private = vec![true; nodes];
        expected_private[nodes - 1] = false;
        assert_eq!(*private_inputs, expected_private);
        let identities = addresses(&db, *first, handle.index);
        assert!(db.rebuild_standing_query(&cx, &handle, too_few).is_err());
        assert_eq!(addresses(&db, *first, handle.index), identities);
        assert_eq!(
            db.standing_native_bag(&cx, &handle, exact).unwrap().1,
            expected
        );
        let mut rebuild_calls = 0;
        rebuild_checked(
            &mut db,
            &cx,
            *first,
            handle.index,
            private_inputs,
            exact,
            &mut || {
                rebuild_calls += 1;
                Ok(())
            },
        )
        .unwrap();
        for stop in 1..=rebuild_calls {
            let identities = addresses(&db, *first, handle.index);
            let mut visited = 0;
            assert!(
                rebuild_checked(
                    &mut db,
                    &cx,
                    *first,
                    handle.index,
                    private_inputs,
                    exact,
                    &mut || {
                        visited += 1;
                        if visited == stop {
                            Err(StandingQueryError::Maintenance(
                                StandingQueryFailure::Interrupted,
                            ))
                        } else {
                            Ok(())
                        }
                    }
                )
                .is_err()
            );
            assert_eq!(visited, stop);
            assert_eq!(addresses(&db, *first, handle.index), identities);
            assert_eq!(db.standing_queries.len(), before + nodes);
            assert_eq!(
                db.standing_native_bag(&cx, &handle, exact).unwrap().1,
                expected
            );
        }
        for unsupported in [
            "MATCH (a)-[:R*]->(b) RETURN a,b",
            "MATCH (a)-[:R*0..2]->(b) RETURN DISTINCT a,b",
            "MATCH (a)-[:R*2..]->(b) RETURN DISTINCT a,b",
            "MATCH (a)-[:R*1..2]->(b) RETURN DISTINCT a,b",
            "MATCH (a)-[:R*]-(b) RETURN DISTINCT a,b",
            "MATCH (a)-[:R*]->(b) RETURN DISTINCT a,b LIMIT 0",
        ] {
            let prepared =
                PreparedNativeRead::prepare(unsupported, &params, reachability_symbols).unwrap();
            assert!(
                prepared
                    .register_standing(&mut db, &cx, &params, policy())
                    .is_err(),
                "{unsupported}"
            );
            assert_eq!(db.standing_queries.len(), before + nodes, "{unsupported}");
        }
        let mut change = WriteBatch::new(RelationId(1));
        change.delete_edge(EId(9));
        db.write(&commit, change).await.unwrap();
        assert_eq!(
            db.standing_native_bag(&cx, &handle, exact).unwrap().1.len(),
            if reflexive { 4 } else { 1 }
        );
        // A durable write may outgrow this view's allowance. It must fence the
        // dependent result, preserve an unrelated view, and permit an explicit
        // complete rebuild under a sufficient budget.
        let mut cycle = WriteBatch::new(RelationId(1));
        cycle.add_edge(EId(10), VId(2), VId(3), vec![]);
        cycle.add_edge(EId(11), VId(3), VId(1), vec![]);
        db.write(&commit, cycle).await.unwrap();
        assert!(db.standing_native_bag(&cx, &handle, policy()).is_err());
        assert_eq!(
            db.standing_native_query(&cx, &prior, policy()).unwrap().1,
            prior_answer.1
        );
        db.rebuild_standing_query(&cx, &handle, policy()).unwrap();
        assert_eq!(db.standing_queries.len(), before + nodes);
        assert_eq!(
            db.standing_native_bag(&cx, &handle, policy())
                .unwrap()
                .1
                .len(),
            9
        );
        assert_eq!(
            db.standing_native_query(&cx, &handle, policy()).unwrap().1,
            db.query(&cx, text, &params, reachability_symbols, policy())
                .unwrap()
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
