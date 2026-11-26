use super::*;
use crate::{DatabaseKeys, MemVfs, QueryResult, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::PropertyKeyId;
use fgdb_gql::{GraphSymbol, GraphSymbolKind};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([31; 32], DatabaseSecurityNamespaceId([32; 32]), [33; 32])
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10000, 10000, 10_000_000, 10_000_000)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        _ => None,
    }
}
fn insert(id: u128) -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    batch.create_vertex(
        VId(id),
        vec![],
        vec![(PropertyKeyId(1), CanonicalScalar::Int(id as i64))],
    );
    batch
}
fn template(text: &str) -> PreparedNativeRead {
    PreparedNativeRead::prepare(text, &GqlParameters::new(), symbols).unwrap()
}
fn setup() -> NativeSubscriptionSetup {
    NativeSubscriptionSetup {
        maintenance: policy(),
        delivery: policy(),
        retention: [8, 1000, 10000],
    }
}
fn scoped(
    db: &mut Database<MemVfs>,
    cx: &QueryCx,
    text: &str,
) -> (NativeSubscription, Arc<SubscriptionBatch>) {
    template(text)
        .subscribe_scoped(db, cx, &GqlParameters::new(), setup(), |_, batch| {
            Ok::<_, SubscriptionError>(Arc::clone(batch))
        })
        .unwrap()
}
fn read(db: &Database<MemVfs>, cx: &QueryCx, handle: &StandingQueryHandle) -> Vec<Vec<QueryValue>> {
    let (_, QueryResult::Rows { rows, .. }) =
        db.standing_native_query(cx, handle, policy()).unwrap()
    else {
        panic!("row result");
    };
    rows
}

#[test]
fn releasing_private_circuits_out_of_order_preserves_siblings_and_reuses_only_vacant_suffixes() {
    let ((), report) = run_async_under_lab(0x5c14_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        for order in [[0, 1, 2], [2, 1, 0], [1, 0, 2], [1, 2, 0]] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            db.write(&commit, insert(1)).await.unwrap();
            let shared = db
                .register_standing_native(
                    &cx,
                    "MATCH (n) RETURN n",
                    &GqlParameters::new(),
                    symbols,
                    policy(),
                )
                .unwrap();
            let prefix = db.standing_queries.len();
            let texts = [
                "MATCH (n) RETURN n",
                "MATCH (n) RETURN n AS id UNION ALL MATCH (m) RETURN m AS id",
                "MATCH (n) WITH n.p AS p RETURN AVG(p) AS a,COUNT(*) AS c",
            ];
            let mut subscriptions = texts.map(|text| scoped(&mut db, &cx, text));
            let ranges = subscriptions
                .each_ref()
                .map(|(sub, _)| sub.ownership.as_ref().unwrap().range.clone());
            let mut closed = [false; 3];
            for (step, index) in order.into_iter().enumerate() {
                let (sub, initial) = &mut subscriptions[index];
                assert!(sub.can_release_registration());
                let expected = ranges[index].end - ranges[index].start;
                assert_eq!(
                    sub.close_and_release(&mut db, &cx, policy()).unwrap(),
                    expected
                );
                assert!(sub.is_closed());
                assert!(!sub.can_release_registration());
                assert_eq!(sub.close_and_release(&mut db, &cx, policy()).unwrap(), 0);
                assert!(matches!(
                    sub.acknowledge(initial.receipt()),
                    Err(SubscriptionError::Closed)
                ));
                assert!(
                    !initial.rows().is_empty(),
                    "caller-owned baseline survives reclamation"
                );
                assert!(matches!(
                    db.standing_native_bag(&cx, sub.handle(), policy()),
                    Err(StandingQueryError::UnknownHandle)
                ));
                closed[index] = true;
                for slot in ranges[index].clone() {
                    assert!(
                        db.standing_queries
                            .get(slot)
                            .is_none_or(|q| matches!(q, StandingQuery::Released))
                    );
                }
                let at = db.write(&commit, insert(2 + step as u128)).await.unwrap();
                assert_eq!(read(&db, &cx, &shared).len(), step + 2);
                for (other, (sub, baseline)) in subscriptions.iter_mut().enumerate() {
                    if !closed[other] {
                        // Internal test access does not escape a public handle.
                        let (_, fresh) =
                            db.standing_native_bag(&cx, &sub.handle, policy()).unwrap();
                        sub.restart_from_current().unwrap();
                        let next = sub.poll(&db, &cx, policy()).unwrap().unwrap();
                        assert_eq!(next.frontier(), at);
                        assert_eq!(next.rows(), &fresh);
                        *baseline = next;
                    }
                }
            }
            assert_eq!(db.standing_queries.len(), prefix);
            // Repeated sessions retain neither old operator state nor tail slots.
            for _ in 0..12 {
                let (mut sub, _) = scoped(&mut db, &cx, texts[1]);
                sub.close_and_release(&mut db, &cx, policy()).unwrap();
                assert_eq!(db.standing_queries.len(), prefix);
            }
            assert_eq!(read(&db, &cx, &shared).len(), 4);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn source_replay_and_fork_escapes_permanently_preserve_shared_registrations() {
    let ((), report) = run_async_under_lab(0x5c14_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        for escape in 0..3 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            db.write(&commit, insert(1)).await.unwrap();
            let (mut sub, baseline) = scoped(&mut db, &cx, "MATCH (n) RETURN n");
            let count = db.standing_queries.len();
            let mut fork = None;
            match escape {
                0 => {
                    let _ = sub.handle();
                }
                1 => {
                    let _ = sub.replay_handle().unwrap();
                }
                _ => {
                    fork = Some(sub.fork(&db, &cx).unwrap());
                }
            }
            assert!(!sub.can_release_registration());
            assert!(matches!(
                sub.close_and_release(&mut db, &cx, policy()),
                Err(SubscriptionError::SharedRegistration)
            ));
            assert_eq!(db.standing_queries.len(), count);
            assert!(Arc::ptr_eq(sub.pending.as_ref().unwrap(), &baseline));
            sub.acknowledge(baseline.receipt()).unwrap();
            db.write(&commit, insert(2)).await.unwrap();
            assert!(sub.poll(&db, &cx, policy()).unwrap().is_some());
            if let Some(mut fork) = fork {
                sub.close();
                assert_eq!(
                    fork.poll(&db, &cx, policy()).unwrap().unwrap().rows().len(),
                    2
                );
                assert!(matches!(
                    fork.close_and_release(&mut db, &cx, policy()),
                    Err(SubscriptionError::NotPrivate)
                ));
            }
        }
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let (mut sub, _) = scoped(&mut db, &cx, "RETURN 1 AS value");
        let mut other = Database::open_memory(&commit, keys()).await.unwrap();
        assert!(sub.fork(&other, &cx).is_err());
        assert!(
            sub.can_release_registration(),
            "a refused fork did not share anything"
        );
        assert!(matches!(
            sub.close_and_release(&mut other, &cx, policy()),
            Err(SubscriptionError::Query(StandingQueryError::ForeignHandle))
        ));
        sub.close_and_release(&mut db, &cx, policy()).unwrap();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn admission_errors_and_panics_rollback_the_private_suffix_not_existing_subscriptions() {
    let ((), report) = run_async_under_lab(0x5c14_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, insert(1)).await.unwrap();
        let existing = db
            .register_standing_native(
                &cx,
                "MATCH (n) RETURN n",
                &GqlParameters::new(),
                symbols,
                policy(),
            )
            .unwrap();
        let first = db.standing_queries.len();
        let before = read(&db, &cx, &existing);
        let prepared = template("MATCH (n) RETURN n AS id UNION ALL MATCH (m) RETURN m AS id");
        assert!(matches!(
            prepared.subscribe_scoped(&mut db, &cx, &GqlParameters::new(), setup(), |_, _| {
                Err::<(), _>(SubscriptionError::Unacknowledged)
            }),
            Err(SubscriptionError::Unacknowledged)
        ));
        assert_eq!(db.standing_queries.len(), first);
        assert_eq!(read(&db, &cx, &existing), before);
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = prepared.subscribe_scoped(
                    &mut db,
                    &cx,
                    &GqlParameters::new(),
                    setup(),
                    |_, _| -> Result<(), SubscriptionError> {
                        panic!("host refuses by unwinding");
                    },
                );
            }))
            .is_err()
        );
        assert_eq!(db.standing_queries.len(), first);
        assert_eq!(read(&db, &cx, &existing), before);
        db.write(&commit, insert(2)).await.unwrap();
        assert_eq!(read(&db, &cx, &existing).len(), 2);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_release_checkpoint_budget_and_unwind_preserves_the_retryable_owner() {
    let ((), report) = run_async_under_lab(0x5c14_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, insert(1)).await.unwrap();
        let (mut sub, initial) = scoped(
            &mut db,
            &cx,
            "MATCH (n) RETURN n AS id UNION ALL MATCH (m) RETURN m AS id",
        );
        let range = sub.ownership.as_ref().unwrap().range.clone();
        let count = db.standing_queries.len();
        let before = read(&db, &cx, &sub.handle);
        let mut calls = 0;
        let stats = {
            let mut checkpoint = || {
                calls += 1;
                Ok(())
            };
            let mut meter = Meter {
                policy: policy(),
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            drop(Release::prepare(&mut db.standing_queries, range.clone(), &mut meter).unwrap());
            meter.stats
        };
        for stop in 1..=calls {
            let mut visited = 0;
            {
                let mut checkpoint = || {
                    visited += 1;
                    if visited == stop {
                        Err(StandingQueryFailure::Interrupted)
                    } else {
                        Ok(())
                    }
                };
                let mut meter = Meter {
                    policy: policy(),
                    stats: StandingQueryStats::default(),
                    checkpoint: &mut checkpoint,
                };
                assert!(matches!(
                    Release::prepare(&mut db.standing_queries, range.clone(), &mut meter),
                    Err(StandingQueryFailure::Interrupted)
                ));
            }
            assert_eq!(visited, stop);
            assert_eq!(db.standing_queries.len(), count);
            assert_eq!(read(&db, &cx, &sub.handle), before);
            assert!(sub.can_release_registration());
            assert!(Arc::ptr_eq(sub.pending.as_ref().unwrap(), &initial));
        }
        for (work, scratch) in [
            (stats.work_units - 1, stats.scratch_entries),
            (stats.work_units, stats.scratch_entries - 1),
        ] {
            assert!(
                sub.close_and_release(&mut db, &cx, GqlQueryPolicy::new(0, 0, work, scratch))
                    .is_err()
            );
            assert_eq!(db.standing_queries.len(), count);
            assert_eq!(read(&db, &cx, &sub.handle), before);
            assert!(Arc::ptr_eq(sub.pending.as_ref().unwrap(), &initial));
        }
        let mut visited = 0;
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut checkpoint = || {
                    visited += 1;
                    assert_ne!(visited, calls, "final release checkpoint unwind");
                    Ok(())
                };
                let mut meter = Meter {
                    policy: policy(),
                    stats: StandingQueryStats::default(),
                    checkpoint: &mut checkpoint,
                };
                let _ = Release::prepare(&mut db.standing_queries, range.clone(), &mut meter);
            }))
            .is_err()
        );
        assert_eq!(read(&db, &cx, &sub.handle), before);
        let exact = GqlQueryPolicy::new(0, 0, stats.work_units, stats.scratch_entries);
        assert_eq!(sub.close_and_release(&mut db, &cx, exact).unwrap(), count);
        assert!(db.standing_queries.is_empty());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn failed_sources_can_be_reclaimed_without_rebuild_and_ordinary_close_never_reclaims() {
    let ((), report) = run_async_under_lab(0x5c14_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, insert(1)).await.unwrap();
        let mut bounded = setup();
        bounded.maintenance.rows = fgdb_gql::GqlExecutionBudget::new(10000, 1);
        let (mut sub, initial) = template("MATCH (n) RETURN n")
            .subscribe_scoped(&mut db, &cx, &GqlParameters::new(), bounded, |_, batch| {
                Ok::<_, SubscriptionError>(Arc::clone(batch))
            })
            .unwrap();
        let count = db.standing_queries.len();
        let at = db.write(&commit, insert(2)).await.unwrap();
        assert!(matches!(
            sub.poll(&db, &cx, policy()),
            Err(SubscriptionError::Query(
                StandingQueryError::Unavailable { .. }
            ))
        ));
        sub.close();
        assert_eq!(db.standing_queries.len(), count);
        assert!(sub.can_release_registration());
        sub.close_and_release(&mut db, &cx, policy()).unwrap();
        assert!(db.standing_queries.is_empty());
        assert_eq!(db.frontier().unwrap(), at);
        assert_eq!(initial.rows().len(), 1);
        let (mut unowned, _) = template("RETURN 1 AS value")
            .subscribe_replaying(&mut db, &cx, &GqlParameters::new(), setup(), |_, _| {
                Ok::<_, SubscriptionError>(())
            })
            .unwrap();
        assert!(matches!(
            unowned.close_and_release(&mut db, &cx, policy()),
            Err(SubscriptionError::NotPrivate)
        ));
        let count = db.standing_queries.len();
        unowned.close();
        drop(unowned);
        assert_eq!(db.standing_queries.len(), count);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn authorized_subscriptions_hide_catalog_ticks_and_preserve_pending_deliveries() {
    use asupersync::security::key::AuthKey;
    use fgdb_delta_types::catalog::{CatalogSymbol, SymbolCatalog};
    use fgdb_delta_types::{LabelId, SchemaEpoch};
    use fgdb_types::EId;
    use fgdb_warden::{Authority, Grant, QueryLimits, Scope};

    let ((), report) = run_async_under_lab(0x5c14_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let params = GqlParameters::new();
        let issuer = Authority::new(
            AuthKey::from_seed(0x5c14_0006),
            DatabaseSecurityNamespaceId([32; 32]),
            "graph",
            SchemaEpoch(0),
            1,
        )
        .unwrap();
        let mut grant = Grant::read_only(
            "main",
            1000,
            QueryLimits {
                max_nodes: 100_000,
                max_work: 10_000_000,
                max_rows: 100_000,
            },
        );
        grant.labels = Scope::only([LabelId(1)]);
        grant.relations = Scope::only([RelationId(1)]);
        grant.properties = Scope::only([PropertyKeyId(1)]);
        let token = issuer.issue_at(&grant, 0).unwrap();
        let authority = issuer.verify_at(&token, "main", 1).unwrap();

        for text in [
            "MATCH (n:L) RETURN n.p AS p",
            "MATCH (n:L) RETURN COUNT(*) AS amount, SUM(n.p) AS total",
            "MATCH (a:L)-[:R]->(b:L) RETURN b.p AS p",
            "MATCH (a:L)-[:R*]->(b:L) RETURN DISTINCT a,b",
        ] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut visible = Database::open_memory(&commit, keys()).await.unwrap();
            for graph in [&mut db, &mut visible] {
                let mut seed = WriteBatch::new(RelationId(1));
                for (id, value) in [(1, 3), (2, 7)] {
                    seed.create_vertex(
                        VId(id),
                        vec![LabelId(1)],
                        vec![(PropertyKeyId(1), CanonicalScalar::Int(value))],
                    );
                }
                seed.add_edge(EId(1), VId(1), VId(2), vec![]);
                graph.write(&commit, seed).await.unwrap();
            }
            // Only one world contains these hidden graph facts or the later
            // catalog registrations. Their public deliveries must still agree.
            let mut hidden = WriteBatch::new(RelationId(2));
            hidden.create_vertex(
                VId(10),
                vec![LabelId(2)],
                vec![(PropertyKeyId(1), CanonicalScalar::Int(99))],
            );
            hidden.add_edge(EId(10), VId(2), VId(1), vec![]);
            db.write(&commit, hidden).await.unwrap();
            let resolver = |kind, name: &str| match (kind, name) {
                (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
                (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
                _ => symbols(kind, name),
            };
            let prepared = PreparedNativeRead::prepare(text, &params, resolver).unwrap();
            let (mut sub, initial) = prepared
                .subscribe_authorized(
                    &mut db,
                    &cx,
                    &params,
                    authority.predicates(),
                    setup(),
                    |_, batch| Ok::<_, SubscriptionError>(Arc::clone(batch)),
                )
                .unwrap();
            let (mut control, expected_initial) = prepared
                .subscribe_authorized(
                    &mut visible,
                    &cx,
                    &params,
                    authority.predicates(),
                    setup(),
                    |_, batch| Ok::<_, SubscriptionError>(Arc::clone(batch)),
                )
                .unwrap();
            assert_eq!(initial.frontier(), CommitSeq(1), "{text}");
            assert_eq!(initial.frontier(), expected_initial.frontier());
            assert_eq!(initial.rows(), expected_initial.rows(), "{text}");
            let initial_stats = db
                .standing_query_node_stats(&cx, &sub.handle)
                .unwrap()
                .copied()
                .collect::<Vec<_>>();

            let mut names = SymbolCatalog::default();
            names
                .bind_all([
                    ("Hidden", CatalogSymbol::Label(LabelId(2))),
                    ("SECRET", CatalogSymbol::Relation(RelationId(2))),
                    ("secret", CatalogSymbol::Property(PropertyKeyId(2))),
                ])
                .unwrap();
            let mut floors = SymbolCatalog::default();
            floors.reserve_through(CatalogSymbol::Label(LabelId(99)));
            floors.reserve_through(CatalogSymbol::Relation(RelationId(99)));
            floors.reserve_through(CatalogSymbol::Property(PropertyKeyId(99)));
            for catalog in [&names, &floors] {
                let before = db.frontier().unwrap();
                let at = db.register_symbols(&commit, catalog).await.unwrap();
                assert_eq!(at, before.checked_successor().unwrap());
                assert!(Arc::ptr_eq(
                    &initial,
                    &sub.poll(&db, &cx, policy()).unwrap().unwrap()
                ));
                assert_eq!(
                    db.standing_query_node_stats(&cx, &sub.handle)
                        .unwrap()
                        .copied()
                        .collect::<Vec<_>>(),
                    initial_stats,
                    "catalog-only work never reaches the masked circuit: {text}"
                );
                let first = sub.ownership.as_ref().unwrap().range.start;
                let StandingQuery::CapabilityScope(scope) = &db.standing_queries[first] else {
                    panic!("restricted registration owns a projection");
                };
                assert_eq!(scope.physical_frontier, at);
                assert_eq!(scope.visible_frontier, initial.frontier());
                assert_eq!(scope.stats, StandingQueryStats::default());
            }
            sub.acknowledge(initial.receipt()).unwrap();
            control.acknowledge(expected_initial.receipt()).unwrap();
            assert!(sub.poll(&db, &cx, policy()).unwrap().is_none());
            assert!(control.poll(&visible, &cx, policy()).unwrap().is_none());

            let mut hidden_property = WriteBatch::new(RelationId(1));
            hidden_property.set_vertex_property(
                VId(1),
                PropertyKeyId(2),
                Some(CanonicalScalar::Int(123)),
            );
            db.write(&commit, hidden_property).await.unwrap();
            assert!(sub.poll(&db, &cx, policy()).unwrap().is_none());
            assert_eq!(sub.acknowledged_frontier(), Some(CommitSeq(1)));

            let mut update = WriteBatch::new(RelationId(1));
            update.set_vertex_property(VId(2), PropertyKeyId(1), Some(CanonicalScalar::Int(8)));
            update.create_vertex(
                VId(3),
                vec![LabelId(1)],
                vec![(PropertyKeyId(1), CanonicalScalar::Int(11))],
            );
            update.add_edge(EId(2), VId(2), VId(3), vec![]);
            for graph in [&mut db, &mut visible] {
                graph.write(&commit, update.clone()).await.unwrap();
            }
            let next = sub.poll(&db, &cx, policy()).unwrap().unwrap();
            let expected = control.poll(&visible, &cx, policy()).unwrap().unwrap();
            assert!(!next.is_snapshot());
            assert_eq!((next.from(), next.frontier()), (Some(CommitSeq(1)), CommitSeq(2)));
            assert_eq!((next.from(), next.frontier()), (expected.from(), expected.frontier()));
            assert_eq!(next.rows(), expected.rows(), "{text}");
            assert_eq!(
                db.standing_native_query(&cx, &sub.handle, policy()).unwrap().1,
                visible.query(&cx, text, &params, resolver, policy()).unwrap(),
                "maintained masked output agrees with a fresh visible-only query: {text}"
            );

            let mut later = SymbolCatalog::default();
            later.bind("LATER", CatalogSymbol::Relation(RelationId(100))).unwrap();
            db.register_symbols(&commit, &later).await.unwrap();
            assert!(Arc::ptr_eq(
                &next,
                &sub.poll(&db, &cx, policy()).unwrap().unwrap()
            ));
            sub.acknowledge(next.receipt()).unwrap();
            control.acknowledge(expected.receipt()).unwrap();
            assert!(sub.poll(&db, &cx, policy()).unwrap().is_none());

            let mut delete = WriteBatch::new(RelationId(1));
            delete.delete_vertex(VId(2));
            for graph in [&mut db, &mut visible] {
                graph.write(&commit, delete.clone()).await.unwrap();
            }
            let last = sub.poll(&db, &cx, policy()).unwrap().unwrap();
            let expected_last = control.poll(&visible, &cx, policy()).unwrap().unwrap();
            assert_eq!((last.from(), last.frontier()), (Some(CommitSeq(2)), CommitSeq(3)));
            assert_eq!(
                (last.from(), last.frontier()),
                (expected_last.from(), expected_last.frontier())
            );
            assert_eq!(last.rows(), expected_last.rows(), "{text}");
            assert_eq!(
                db.standing_native_query(&cx, &sub.handle, policy()).unwrap().1,
                visible.query(&cx, text, &params, resolver, policy()).unwrap(),
                "{text}"
            );
            sub.acknowledge(last.receipt()).unwrap();
            control.acknowledge(expected_last.receipt()).unwrap();
            sub.close_and_release(&mut db, &cx, policy()).unwrap();
            control.close_and_release(&mut visible, &cx, policy()).unwrap();
            assert!(db.standing_queries.is_empty());
            assert!(visible.standing_queries.is_empty());
            assert_eq!(initial.rows(), expected_initial.rows());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
