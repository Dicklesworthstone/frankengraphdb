use super::*;
use crate::recovery::tests::{served, vertex};
use asupersync::lab::run_async_under_lab;
use fgdb_gql::GqlParameters;
use fgdb_protocol::body::ExecuteMode;
use fgdb_warden::Restriction;

fn request(statement: &str) -> Execute {
    Execute {
        mode: ExecuteMode::Subscribe,
        statement: statement.into(),
        parameters: vec![],
    }
}

fn success<T>(result: Result<T, Refusal>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("unexpected {:?}: {}", error.code, error.message),
    }
}

fn refused<T>(result: Result<T, Refusal>, expected: ErrorCode) {
    match result {
        Ok(_) => panic!("expected {expected:?} refusal"),
        Err(error) => assert_eq!(error.code, expected, "{}", error.message),
    }
}

#[test]
fn native_phase_reservations_share_work_and_keep_rows_at_final_delivery() {
    let policy = GqlQueryPolicy::new(100_000, 100_000, 100_000, 100_000);
    let limits = QueryLimits {
        max_nodes: 4096,
        max_work: 4096,
        max_rows: 1,
    };
    for nodes in [1, 2, 7, 20, 41] {
        for width in [0, 1, 2, 65] {
            let reservation = success(reserve(policy, limits, 4096, 8192, nodes, width));
            let maintained = reservation.setup.maintenance.evaluator;
            let delivery = reservation.setup.delivery.evaluator;
            let total = (nodes + 1) * (maintained.max_work_units + maintained.max_scratch_entries)
                + delivery.max_work_units
                + delivery.max_scratch_entries
                + reservation.wire_work;
            assert!(
                total <= 4096,
                "every circuit node, replay, baseline, and wire copy share work"
            );
            assert!(reservation.nodes <= limits.max_nodes);
            assert_eq!(
                reservation.setup.maintenance.rows.max_result_rows(),
                Some(100_000),
                "signed final rows must not truncate private maintained state"
            );
            assert_eq!(reservation.setup.delivery.rows.max_result_rows(), Some(1));
        }
    }
    refused(reserve(policy, limits, 4096, 8192, 0, 1), ErrorCode::Budget);
    refused(reserve(policy, limits, 1, 8192, 1, 1), ErrorCode::Budget);
}

#[test]
fn subscription_map_fields_bind_the_same_key_for_baseline_and_later_deltas() {
    let ((), report) = run_async_under_lab(0x79a0_0317, |root| async move {
        let (mut server, token, _) = served(&root, "subscription-map-fields").await;
        Arc::get_mut(server.databases.get_mut("test").unwrap())
            .unwrap()
            .symbols
            .bind(fgdb_gql::GraphSymbolKind::Property, "p", 1)
            .unwrap();
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut seed = vertex(1);
        seed.set_vertex_property(
            fgdb_types::VId(1),
            fgdb_delta_types::PropertyKeyId(1),
            Some(fgdb_types::CanonicalScalar::Int(7)),
        );
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), seed)
            .await
            .unwrap();
        let mut statement =
            request("SUBSCRIBE TO MATCH (n {p:$payload.filter.key}) RETURN n.p AS p");
        statement.parameters = vec![(
            "payload".into(),
            WireValue::Map(vec![(
                "filter".into(),
                WireValue::Map(vec![("key".into(), WireValue::Int(7))]),
            )]),
        )];
        let mut selected = success(subscribe(&root, db, &token, &statement).await);
        let baseline = success(poll(&root, db, &token, &mut selected).await).unwrap();
        assert_eq!(baseline.entries, vec![(1, vec![WireValue::Int(7)])]);
        selected
            .consumer
            .acknowledge(baseline.batch.receipt())
            .unwrap();
        statement.parameters = vec![(
            "payload".into(),
            WireValue::Map(vec![("filter".into(), WireValue::Map(vec![]))]),
        )];
        let mut missing = success(subscribe(&root, db, &token, &statement).await);
        let empty = success(poll(&root, db, &token, &mut missing).await).unwrap();
        assert!(empty.entries.is_empty());
        missing.consumer.acknowledge(empty.batch.receipt()).unwrap();
        let mut changed = fgdb::WriteBatch::new(fgdb_delta_types::RelationId(1));
        changed.set_vertex_property(
            fgdb_types::VId(1),
            fgdb_delta_types::PropertyKeyId(1),
            Some(fgdb_types::CanonicalScalar::Int(8)),
        );
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), changed)
            .await
            .unwrap();
        let delta = success(poll(&root, db, &token, &mut selected).await).unwrap();
        assert!(!delta.batch.is_snapshot());
        assert_eq!(delta.entries, vec![(-1, vec![WireValue::Int(7)])]);
        selected
            .consumer
            .acknowledge(delta.batch.receipt())
            .unwrap();
        let unseen = success(poll(&root, db, &token, &mut missing).await).unwrap();
        assert!(unseen.entries.is_empty());
        let denied = token.attenuate(Restriction::MaxWork(0)).unwrap();
        refused(
            subscribe(&root, db, &denied, &statement).await,
            ErrorCode::Budget,
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn scoped_analytic_subscriptions_match_visible_graphs_across_hidden_and_visible_commits() {
    use fgdb::WriteBatch;
    use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
    use fgdb_gql::GraphSymbolKind;
    use fgdb_types::{CanonicalScalar, CommitSeq, EId, VId};
    use fgdb_warden::Scope;
    use std::collections::BTreeMap;

    let ((), report) = run_async_under_lab(0x79a0_20cb, |root| async move {
        let (mut clean, clean_token, _) = served(&root, "analytic-visible").await;
        let (mut noisy, noisy_token, _) = served(&root, "analytic-hidden").await;
        for server in [&mut clean, &mut noisy] {
            let db = Arc::get_mut(server.databases.get_mut("test").unwrap()).unwrap();
            for (kind, name, id) in [
                (GraphSymbolKind::Label, "Visible", 1),
                (GraphSymbolKind::Label, "Secret", 2),
                (GraphSymbolKind::Relation, "R", 1),
                (GraphSymbolKind::Relation, "Hidden", 2),
                (GraphSymbolKind::Property, "secret", 2),
            ] {
                db.symbols.bind(kind, name, id).unwrap();
            }
        }
        let clean = &clean.databases["test"];
        let noisy = &noisy.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();

        // Physical history differs before registration. The extra endpoint
        // and its allowed-relation incidences must disappear before analytics.
        let mut hidden = WriteBatch::new(RelationId(1));
        hidden.create_vertex(VId(20), vec![LabelId(2)], vec![]);
        noisy
            .db
            .write(&root)
            .await
            .unwrap()
            .write(&commit, hidden)
            .await
            .unwrap();
        for db in [clean, noisy] {
            let mut seed = WriteBatch::new(RelationId(1));
            for id in 1..=4 {
                seed.create_vertex(VId(id), vec![LabelId(1)], vec![]);
            }
            for (eid, from, to) in [(1, 1, 2), (2, 2, 3), (3, 3, 1), (4, 1, 2), (5, 2, 1)] {
                seed.add_edge(EId(eid), VId(from), VId(to), vec![]);
            }
            db.db
                .write(&root)
                .await
                .unwrap()
                .write(&commit, seed)
                .await
                .unwrap();
        }
        let mut crossing = WriteBatch::new(RelationId(1));
        crossing.add_edge(EId(50), VId(1), VId(20), vec![]);
        crossing.add_edge(EId(51), VId(20), VId(4), vec![]);
        crossing.set_vertex_label(VId(1), LabelId(2), true);
        crossing.set_vertex_property(VId(1), PropertyKeyId(2), Some(CanonicalScalar::Int(9)));
        noisy
            .db
            .write(&root)
            .await
            .unwrap()
            .write(&commit, crossing)
            .await
            .unwrap();
        let mut hidden_relation = WriteBatch::new(RelationId(2));
        hidden_relation.add_edge(EId(52), VId(1), VId(4), vec![]);
        noisy
            .db
            .write(&root)
            .await
            .unwrap()
            .write(&commit, hidden_relation)
            .await
            .unwrap();

        let attenuate = |token: &CapabilityToken| {
            token
                .attenuate(Restriction::Labels(Scope::only([LabelId(1)])))
                .unwrap()
                .attenuate(Restriction::Relations(Scope::only([RelationId(1)])))
                .unwrap()
                .attenuate(Restriction::Properties(Scope::only([])))
                .unwrap()
                .attenuate(Restriction::MaxNodes(40_000))
                .unwrap()
                .attenuate(Restriction::MaxWork(200_000))
                .unwrap()
                .attenuate(Restriction::MaxRows(8))
                .unwrap()
        };
        let clean_token = attenuate(&clean_token);
        let noisy_token = attenuate(&noisy_token);
        let mut subscriptions = Vec::new();
        for (metric, field, directed) in [
            ("connected_components", "component", false),
            ("weakly_connected_components", "component", true),
            ("strongly_connected_components", "component", true),
            ("core_number", "core", false),
            ("triangles", "triangles", false),
            ("clustering_coefficient", "score", false),
        ] {
            let direction = if directed { "directed" } else { "undirected" };
            let statement = request(&format!(
                "SUBSCRIBE TO CALL fnx.{metric}({{parallel:'collapse',self_loops:'drop',direction:'{direction}'}}) \
                 YIELD vertex AS member,{field} AS value RETURN member,value"
            ));
            let a = success(subscribe(&root, clean, &clean_token, &statement).await);
            let b = success(subscribe(&root, noisy, &noisy_token, &statement).await);
            assert_eq!(a.columns, ["member", "value"]);
            assert_eq!(a.columns, b.columns);
            assert_eq!(a.registration_nodes, b.registration_nodes);
            assert_eq!(a.registration_work, b.registration_work);
            subscriptions.push((metric, a, b, BTreeMap::<(u128, i64), i128>::new()));
        }

        // These expectations are the hand-derived answers for a triangle
        // with a reciprocal/parallel side and an isolated fourth vertex.
        // Then remove the closing arc, then attach the isolate to the path.
        // Finally delete vertex 2 physically in the clean graph, but only
        // hide it by removing its allowed label in the noisy graph. Both
        // must retract its lifetime and every incident analytic edge.
        for tick in 0..4 {
            if tick != 0 {
                for (index, db) in [clean, noisy].into_iter().enumerate() {
                    let mut change = WriteBatch::new(RelationId(1));
                    if tick == 1 {
                        change.delete_edge(EId(3));
                    } else if tick == 2 {
                        change.add_edge(EId(6), VId(3), VId(4), vec![]);
                    } else if index == 0 {
                        change.delete_vertex(VId(2));
                    } else {
                        change.set_vertex_label(VId(2), LabelId(1), false);
                    }
                    db.db
                        .write(&root)
                        .await
                        .unwrap()
                        .write(&commit, change)
                        .await
                        .unwrap();
                }
            }
            for (metric, a, b, previous) in &mut subscriptions {
                let component = metric.ends_with("connected_components");
                let values: [i64; 4] = if component {
                    if tick == 3 && *metric != "strongly_connected_components" {
                        [1, 0, 3, 3]
                    } else if *metric == "strongly_connected_components" && tick != 0 {
                        [1, 1, 3, 4]
                    } else if tick == 2 {
                        [1, 1, 1, 1]
                    } else {
                        [1, 1, 1, 4]
                    }
                } else if *metric == "core_number" {
                    match tick {
                        0 => [2, 2, 2, 0],
                        1 => [1, 1, 1, 0],
                        2 => [1, 1, 1, 1],
                        _ => [0, 0, 1, 1],
                    }
                } else if tick == 0 {
                    [1, 1, 1, 0]
                } else {
                    [0; 4]
                };
                let current: BTreeMap<_, _> = values
                    .into_iter()
                    .enumerate()
                    .filter(|(index, _)| tick != 3 || *index != 1)
                    .map(|(index, value)| (((index + 1) as u128, value), 1))
                    .collect();
                let mut expected = current.clone();
                for (row, weight) in previous.iter() {
                    *expected.entry(*row).or_default() -= weight;
                }
                let expected: Vec<_> = expected
                    .into_iter()
                    .filter(|(_, weight)| *weight != 0)
                    .map(|((vertex, value), weight)| {
                        let value = if component {
                            WireValue::Vertex(value as u128)
                        } else if *metric == "clustering_coefficient" {
                            WireValue::Float(value as f64)
                        } else {
                            WireValue::Int(value)
                        };
                        (weight, vec![WireValue::Vertex(vertex), value])
                    })
                    .collect();
                let left = success(poll(&root, clean, &clean_token, a).await).unwrap();
                let right = success(poll(&root, noisy, &noisy_token, b).await).unwrap();
                assert_eq!(left.entries, expected, "{metric}, tick {tick}");
                assert_eq!(right.entries, expected, "{metric}, hidden tick {tick}");
                assert_eq!(left.batch.from(), right.batch.from());
                assert_eq!(left.batch.frontier(), CommitSeq(tick + 1));
                assert_eq!(left.batch.frontier(), right.batch.frontier());
                let repeated = success(poll(&root, noisy, &noisy_token, b).await).unwrap();
                assert!(Arc::ptr_eq(&right.batch, &repeated.batch));
                a.consumer.acknowledge(left.batch.receipt()).unwrap();
                b.consumer.acknowledge(right.batch.receipt()).unwrap();
                *previous = current;
            }

            let mut hidden = WriteBatch::new(RelationId(2));
            for index in 0..32_u128 {
                let id = 1_000 + u128::from(tick) * 32 + index;
                hidden.create_vertex(VId(id), vec![LabelId(2)], vec![]);
                hidden.add_edge(EId(id), VId(1), VId(id), vec![]);
            }
            hidden.set_vertex_property(
                VId(1),
                PropertyKeyId(2),
                Some(CanonicalScalar::Int(tick as i64)),
            );
            noisy
                .db
                .write(&root)
                .await
                .unwrap()
                .write(&commit, hidden)
                .await
                .unwrap();
            for (_, a, b, _) in &mut subscriptions {
                assert!(success(poll(&root, clean, &clean_token, a).await).is_none());
                assert!(success(poll(&root, noisy, &noisy_token, b).await).is_none());
            }
        }

        let statement = request(
            "SUBSCRIBE TO CALL fnx.connected_components({parallel:'collapse'}) YIELD vertex,component",
        );
        for (db, token) in [(clean, &clean_token), (noisy, &noisy_token)] {
            for restriction in [
                Restriction::MaxNodes(0),
                Restriction::MaxWork(0),
                Restriction::MaxRows(2),
            ] {
                let denied = token.attenuate(restriction).unwrap();
                refused(
                    subscribe(&root, db, &denied, &statement).await,
                    ErrorCode::Budget,
                );
                assert_eq!(db.subscriptions.load(Ordering::Acquire), 6);
            }
            let unsupported = request(
                "SUBSCRIBE TO CALL fnx.degree_centrality({parallel:'collapse'}) YIELD vertex,score LIMIT 0",
            );
            refused(
                subscribe(&root, db, token, &unsupported).await,
                ErrorCode::Statement,
            );
            assert_eq!(db.subscriptions.load(Ordering::Acquire), 6);
        }
        for (_, a, b, _) in &mut subscriptions {
            let changed = noisy_token
                .attenuate(Restriction::Relations(Scope::only([])))
                .unwrap();
            refused(
                poll(&root, noisy, &changed, b).await,
                ErrorCode::PermissionDenied,
            );
            let expired = noisy_token
                .attenuate(Restriction::ExpiresBefore(1))
                .unwrap();
            refused(
                poll(&root, noisy, &expired, b).await,
                ErrorCode::PermissionDenied,
            );
            a.consumer.close();
            b.consumer.close();
        }
        drop(clean.db.write(&root).await.unwrap());
        drop(noisy.db.write(&root).await.unwrap());
        assert_eq!(clean.subscriptions.load(Ordering::Acquire), 0);
        assert_eq!(noisy.subscriptions.load(Ordering::Acquire), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn zero_signed_grants_refuse_before_installation_and_source_free_needs_no_nodes() {
    let ((), report) = run_async_under_lab(0x79a0_0301, |root| async move {
        let (mut server, token, _) = served(&root, "subscription-zero-grants").await;
        Arc::get_mut(server.databases.get_mut("test").unwrap())
            .unwrap()
            .max_subscriptions = 1;
        let db = &server.databases["test"];
        let zero_work = token.attenuate(Restriction::MaxWork(0)).unwrap();
        refused(
            subscribe(
                &root,
                db,
                &zero_work,
                &request("SUBSCRIBE TO RETURN 7 AS value"),
            )
            .await,
            ErrorCode::Budget,
        );
        let zero_nodes = token.attenuate(Restriction::MaxNodes(0)).unwrap();
        refused(
            subscribe(
                &root,
                db,
                &zero_nodes,
                &request("SUBSCRIBE TO MATCH (n) RETURN n"),
            )
            .await,
            ErrorCode::Budget,
        );
        let zero_rows = zero_nodes.attenuate(Restriction::MaxRows(0)).unwrap();
        refused(
            subscribe(
                &root,
                db,
                &zero_rows,
                &request("SUBSCRIBE TO RETURN 7 AS value"),
            )
            .await,
            ErrorCode::Budget,
        );
        assert_eq!(db.subscriptions.load(Ordering::Acquire), 0);
        let mut subscription = success(
            subscribe(
                &root,
                db,
                &zero_nodes,
                &request("SUBSCRIBE TO RETURN 7 AS value"),
            )
            .await,
        );
        assert_eq!(db.subscriptions.load(Ordering::Acquire), 1);
        let delivered = success(poll(&root, db, &zero_nodes, &mut subscription).await).unwrap();
        assert_eq!(delivered.entries, vec![(1, vec![WireValue::Int(7)])]);
        subscription
            .consumer
            .acknowledge(delivered.batch.receipt())
            .unwrap();
        assert!(success(poll(&root, db, &zero_nodes, &mut subscription).await).is_none());
    });
    assert!(report.lab_test_passed());
}

#[test]
fn signed_rows_count_complete_support_and_cover_cached_pending_redelivery() {
    let ((), report) = run_async_under_lab(0x79a0_0302, |root| async move {
        let (server, token, _) = served(&root, "subscription-pending-row-budget").await;
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut seed = vertex(1);
        seed.create_vertex(fgdb_types::VId(2), vec![], vec![]);
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), seed)
            .await
            .unwrap();
        let one_row = token.attenuate(Restriction::MaxRows(1)).unwrap();
        let mut subscription = success(
            subscribe(
                &root,
                db,
                &one_row,
                &request("SUBSCRIBE TO MATCH (n) RETURN 7 AS value"),
            )
            .await,
        );
        let zero_rows = one_row.attenuate(Restriction::MaxRows(0)).unwrap();
        refused(
            poll(&root, db, &zero_rows, &mut subscription).await,
            ErrorCode::Budget,
        );
        let first = success(poll(&root, db, &one_row, &mut subscription).await).unwrap();
        assert_eq!(first.entries, vec![(2, vec![WireValue::Int(7)])]);
        // The server's initial cache is now empty, but the native consumer
        // still owns this unacknowledged Arc and ignores a smaller native policy.
        refused(
            poll(&root, db, &zero_rows, &mut subscription).await,
            ErrorCode::Budget,
        );
        assert_eq!(subscription.consumer.acknowledged_frontier(), None);
        let replay = success(poll(&root, db, &one_row, &mut subscription).await).unwrap();
        assert!(Arc::ptr_eq(&first.batch, &replay.batch));
        assert_eq!(first.entries, replay.entries);
        subscription
            .consumer
            .acknowledge(first.batch.receipt())
            .unwrap();
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), vertex(3))
            .await
            .unwrap();
        let tiny_work = one_row.attenuate(Restriction::MaxWork(1)).unwrap();
        refused(
            poll(&root, db, &tiny_work, &mut subscription).await,
            ErrorCode::Budget,
        );
        let next = success(poll(&root, db, &one_row, &mut subscription).await).unwrap();
        assert!(!next.batch.is_snapshot());
        assert_eq!(next.entries, vec![(1, vec![WireValue::Int(7)])]);
    });
    assert!(report.lab_test_passed());
}

#[test]
fn coalesced_server_pulls_bound_net_ranges_and_preserve_pending_receipts() {
    use fgdb_delta_types::{PropertyKeyId, RelationId};
    use fgdb_types::{CanonicalScalar, VId};
    use std::future::Future as _;
    use std::num::NonZeroUsize;

    let ((), report) = run_async_under_lab(0x79a0_0320, |root| async move {
        let (mut server, token, _) = served(&root, "subscription-coalesced-ranges").await;
        let configured = Arc::get_mut(server.databases.get_mut("test").unwrap()).unwrap();
        assert_eq!(configured.subscription_max_coalesced_ticks.get(), 1);
        configured.subscription_max_coalesced_ticks = NonZeroUsize::new(3).unwrap();
        configured
            .symbols
            .bind(fgdb_gql::GraphSymbolKind::Property, "p", 1)
            .unwrap();
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let edit = |value| {
            let mut batch = fgdb::WriteBatch::new(RelationId(1));
            batch.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(value)));
            batch
        };
        let mut seed = vertex(1);
        seed.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(0)));
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), seed)
            .await
            .unwrap();
        let mut subscription = success(
            subscribe(
                &root,
                db,
                &token,
                &request("SUBSCRIBE TO MATCH (n) RETURN n.p AS p"),
            )
            .await,
        );
        let baseline = success(poll(&root, db, &token, &mut subscription).await).unwrap();
        assert_eq!(baseline.entries, [(1, vec![WireValue::Int(0)])]);
        let mut cuts = Vec::new();
        for value in [1, 2, 0, 9] {
            cuts.push(
                db.db
                    .write(&root)
                    .await
                    .unwrap()
                    .write(&contexts.commit(), edit(value))
                    .await
                    .unwrap(),
            );
        }
        let repeated = success(poll(&root, db, &token, &mut subscription).await).unwrap();
        assert!(Arc::ptr_eq(&baseline.batch, &repeated.batch));
        assert_eq!(subscription.consumer.acknowledged_frontier(), None);
        subscription
            .consumer
            .acknowledge(baseline.batch.receipt())
            .unwrap();

        // The first three commits cancel exactly. The final NET row ceiling
        // may therefore be zero despite two changed rows in each atomic tick.
        let no_rows = token.attenuate(Restriction::MaxRows(0)).unwrap();
        let empty = success(poll(&root, db, &no_rows, &mut subscription).await).unwrap();
        assert!(!empty.batch.is_snapshot());
        assert_eq!(empty.batch.from(), Some(baseline.batch.frontier()));
        assert_eq!(empty.batch.frontier(), cuts[2]);
        assert!(empty.entries.is_empty());
        assert_eq!(
            subscription.consumer.acknowledged_frontier(),
            Some(baseline.batch.frontier())
        );
        let latest = db
            .db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), edit(10))
            .await
            .unwrap();
        let repeated = success(poll(&root, db, &no_rows, &mut subscription).await).unwrap();
        assert!(Arc::ptr_eq(&empty.batch, &repeated.batch));
        subscription
            .consumer
            .acknowledge(empty.batch.receipt())
            .unwrap();

        // The next range is 0 -> 10, not 0 -> 9 or 9 -> 10. Refusing its
        // complete support leaves the acknowledged cut and both ticks intact.
        let one_row = token.attenuate(Restriction::MaxRows(1)).unwrap();
        refused(
            poll(&root, db, &one_row, &mut subscription).await,
            ErrorCode::Budget,
        );
        assert_eq!(subscription.consumer.acknowledged_frontier(), Some(cuts[2]));
        let changed = success(poll(&root, db, &token, &mut subscription).await).unwrap();
        assert_eq!(changed.batch.from(), Some(cuts[2]));
        assert_eq!(changed.batch.frontier(), latest);
        assert_eq!(
            changed.entries,
            [(-1, vec![WireValue::Int(0)]), (1, vec![WireValue::Int(10)])]
        );
        let tiny = token.attenuate(Restriction::MaxWork(1)).unwrap();
        refused(
            poll(&root, db, &tiny, &mut subscription).await,
            ErrorCode::Budget,
        );
        let hidden = token
            .attenuate(Restriction::Properties(fgdb_warden::Scope::only([])))
            .unwrap();
        refused(
            poll(&root, db, &hidden, &mut subscription).await,
            ErrorCode::PermissionDenied,
        );
        // Dropping a pull while it waits behind a writer cannot acknowledge
        // or split the already selected range. The next pull shares its Arc.
        let held = db.db.write(&root).await.unwrap();
        let mut interrupted = Box::pin(poll(&root, db, &token, &mut subscription));
        assert!(
            interrupted
                .as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                .is_pending()
        );
        drop(interrupted);
        drop(held);
        assert_eq!(subscription.consumer.acknowledged_frontier(), Some(cuts[2]));
        let repeated = success(poll(&root, db, &token, &mut subscription).await).unwrap();
        assert!(Arc::ptr_eq(&changed.batch, &repeated.batch));
        assert_eq!(changed.entries, repeated.entries);
        assert_eq!(
            subscription
                .consumer
                .acknowledge(changed.batch.receipt())
                .unwrap(),
            latest
        );
        assert_eq!(
            subscription
                .consumer
                .acknowledge(changed.batch.receipt())
                .unwrap(),
            latest
        );
        assert!(success(poll(&root, db, &token, &mut subscription).await).is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn maintenance_keeps_the_original_signed_policy_across_later_writes() {
    let ((), report) = run_async_under_lab(0x79a0_0303, |root| async move {
        let (server, token, _) = served(&root, "subscription-maintenance-budget").await;
        let db = &server.databases["test"];
        let bounded = token
            .attenuate(Restriction::MaxWork(4096))
            .unwrap()
            .attenuate(Restriction::MaxNodes(64))
            .unwrap();
        let mut subscription = success(
            subscribe(
                &root,
                db,
                &bounded,
                &request("SUBSCRIBE TO MATCH (n) RETURN n"),
            )
            .await,
        );
        let initial = success(poll(&root, db, &bounded, &mut subscription).await).unwrap();
        assert!(initial.entries.is_empty());
        subscription
            .consumer
            .acknowledge(initial.batch.receipt())
            .unwrap();
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut batch = vertex(1);
        for id in 2..=200 {
            batch.create_vertex(fgdb_types::VId(id), vec![], vec![]);
        }
        // The durable write still commits. Only the derived view is fenced
        // when its retained, originally admitted maintenance budget runs out.
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), batch)
            .await
            .unwrap();
        refused(
            poll(&root, db, &token, &mut subscription).await,
            ErrorCode::Budget,
        );
    });
    assert!(report.lab_test_passed());
}

#[test]
fn replay_retention_gap_uses_one_bounded_replacement_baseline() {
    let ((), report) = run_async_under_lab(0x79a0_0304, |root| async move {
        for max_ticks in [1, 3] {
            let (mut server, token, _) = served(
                &root,
                &format!("subscription-replay-gap-budget-{max_ticks}"),
            )
            .await;
            Arc::get_mut(server.databases.get_mut("test").unwrap())
                .unwrap()
                .subscription_max_coalesced_ticks = std::num::NonZeroUsize::new(max_ticks).unwrap();
            let db = &server.databases["test"];
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let query = contexts.query();
            let params = GqlParameters::new();
            let prepared = PreparedNativeRead::prepare_subscription(
                &query,
                "SUBSCRIBE TO MATCH (n) RETURN n",
                &params,
                db.symbols.clone(),
            )
            .unwrap();
            let mut guard = db.db.write(&root).await.unwrap();
            let generation = guard.generation();
            let (mut consumer, baseline) = prepared
                .subscribe_scoped(
                    &mut guard,
                    &query,
                    &params,
                    NativeSubscriptionSetup {
                        maintenance: db.query_policy,
                        delivery: db.query_policy,
                        retention: [1, 1000, 100_000],
                    },
                    |_, batch| Ok::<_, SubscriptionError>(Arc::clone(batch)),
                )
                .unwrap();
            consumer.acknowledge(baseline.receipt()).unwrap();
            for id in 1..=3 {
                guard.write(&contexts.commit(), vertex(id)).await.unwrap();
            }
            drop(guard);
            let scope = db
                .authority
                .verify_at(&token, TRUNK, unix_millis())
                .unwrap()
                .predicates()
                .clone();
            let admitted = scope.limits();
            db.subscriptions.fetch_add(1, Ordering::AcqRel);
            let consumer = generation.own_subscription(consumer);
            let mut subscription = Subscription {
                consumer,
                columns: vec!["n".into()],
                generation,
                admitted,
                scope,
                initial: None,
                registration_work: 0,
                registration_nodes: 0,
            };
            let tiny = token.attenuate(Restriction::MaxWork(6)).unwrap();
            refused(
                poll(&root, db, &tiny, &mut subscription).await,
                ErrorCode::Budget,
            );
            assert_eq!(subscription.consumer.acknowledged_frontier(), None);
            let replacement = success(poll(&root, db, &token, &mut subscription).await).unwrap();
            assert!(replacement.batch.is_snapshot());
            assert_eq!(replacement.entries.len(), 3);
            subscription
                .consumer
                .acknowledge(replacement.batch.receipt())
                .unwrap();
            assert!(success(poll(&root, db, &token, &mut subscription).await).is_none());
        }
    });
    assert!(report.lab_test_passed());
}

#[test]
fn source_free_grouping_keeps_private_input_rows_with_zero_signed_nodes() {
    let ((), report) = run_async_under_lab(0x79a0_0305, |root| async move {
        let (server, token, _) = served(&root, "subscription-source-free-group").await;
        let db = &server.databases["test"];
        let bounded = token
            .attenuate(Restriction::MaxNodes(0))
            .unwrap()
            .attenuate(Restriction::MaxRows(1))
            .unwrap();
        let mut subscription = success(
            subscribe(
                &root,
                db,
                &bounded,
                &request("SUBSCRIBE TO UNWIND [1,2] AS x RETURN SUM(x) AS total"),
            )
            .await,
        );
        let initial = success(poll(&root, db, &bounded, &mut subscription).await).unwrap();
        assert_eq!(initial.entries, vec![(1, vec![WireValue::WideInt(3)])]);
    });
    assert!(report.lab_test_passed());
}

#[test]
fn component_calls_subscribe_over_native_replay_with_full_width_vertex_outputs() {
    use fgdb_delta_types::RelationId;
    use fgdb_types::{EId, VId};

    let ((), report) = run_async_under_lab(0x79a0_03c1, |root| async move {
        let (mut server, token, _) = served(&root, "subscription-components").await;
        let served = Arc::get_mut(server.databases.get_mut("test").unwrap()).unwrap();
        served
            .symbols
            .bind(fgdb_gql::GraphSymbolKind::Relation, "R", 1)
            .unwrap();
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let wide = 1_u128 << 100;
        let mut seed = vertex(1);
        for id in [2, 3, wide] {
            seed.create_vertex(VId(id), vec![], vec![]);
        }
        for (eid, from, to) in [(1, 1, 2), (2, 2, 3), (3, 1, 2), (4, wide, wide)] {
            seed.add_edge(EId(eid), VId(from), VId(to), vec![]);
        }
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), seed)
            .await
            .unwrap();
        let statement = request(
            "SUBSCRIBE TO CALL fnx.strongly_connected_components(\
             {relations:['R'],parallel:'collapse'}) \
             YIELD component AS root,vertex AS member \
             RETURN member,root SKIP 1 LIMIT 3",
        );
        let mut subscription = success(subscribe(&root, db, &token, &statement).await);
        assert_eq!(subscription.columns, ["member", "root"]);
        let initial = success(poll(&root, db, &token, &mut subscription).await).unwrap();
        assert!(initial.batch.is_snapshot());
        assert_eq!(
            initial.entries,
            [2, 3, wide]
                .map(|id| (1, vec![WireValue::Vertex(id), WireValue::Vertex(id)]))
                .to_vec()
        );

        // Keep the baseline pending while two committed SCC changes accumulate.
        // The ordinary CALL's canonical page still includes the wide isolate.
        let mut cycle = fgdb::WriteBatch::new(RelationId(1));
        cycle.add_edge(EId(5), VId(3), VId(1), vec![]);
        let merged = db
            .db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), cycle)
            .await
            .unwrap();
        let mut split = fgdb::WriteBatch::new(RelationId(1));
        split.delete_edge(EId(2));
        let split = db
            .db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), split)
            .await
            .unwrap();
        let repeat = success(poll(&root, db, &token, &mut subscription).await).unwrap();
        assert!(Arc::ptr_eq(&initial.batch, &repeat.batch));
        assert_eq!(initial.entries, repeat.entries);
        subscription
            .consumer
            .acknowledge(initial.batch.receipt())
            .unwrap();
        let row = |member, component| vec![WireValue::Vertex(member), WireValue::Vertex(component)];
        let first = success(poll(&root, db, &token, &mut subscription).await).unwrap();
        assert_eq!(
            (first.batch.from(), first.batch.frontier()),
            (Some(initial.batch.frontier()), merged)
        );
        assert_eq!(
            first.entries,
            vec![
                (1, row(2, 1)),
                (-1, row(2, 2)),
                (1, row(3, 1)),
                (-1, row(3, 3)),
            ]
        );
        subscription
            .consumer
            .acknowledge(first.batch.receipt())
            .unwrap();
        let second = success(poll(&root, db, &token, &mut subscription).await).unwrap();
        assert_eq!(
            (second.batch.from(), second.batch.frontier()),
            (Some(first.batch.frontier()), split)
        );
        assert_eq!(
            second.entries,
            vec![
                (-1, row(2, 1)),
                (1, row(2, 2)),
                (-1, row(3, 1)),
                (1, row(3, 3)),
            ]
        );
        subscription
            .consumer
            .acknowledge(second.batch.receipt())
            .unwrap();
        assert!(success(poll(&root, db, &token, &mut subscription).await).is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn component_subscription_scopes_mask_topology_and_unsupported_profiles_refuse() {
    use fgdb_delta_types::{LabelId, RelationId};
    use fgdb_types::{EId, VId};
    use fgdb_warden::Scope;

    let ((), report) = run_async_under_lab(0x79a0_03c2, |root| async move {
        let (mut server, token, _) = served(&root, "subscription-component-scope").await;
        let served = Arc::get_mut(server.databases.get_mut("test").unwrap()).unwrap();
        served
            .symbols
            .bind(fgdb_gql::GraphSymbolKind::Relation, "R", 1)
            .unwrap();
        served
            .symbols
            .bind(fgdb_gql::GraphSymbolKind::Label, "L", 1)
            .unwrap();
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let statement = request(
            "SUBSCRIBE TO CALL fnx.connected_components(\
             {relations:['R'],parallel:'collapse'}) YIELD node,component",
        );
        let scoped = [
            token
                .attenuate(Restriction::Labels(Scope::only([LabelId(1)])))
                .unwrap(),
            token
                .attenuate(Restriction::Relations(Scope::only([RelationId(1)])))
                .unwrap(),
            token
                .attenuate(Restriction::Properties(Scope::only([])))
                .unwrap(),
        ];
        for hidden in [false, true] {
            if hidden {
                let mut seed = vertex(1);
                seed.create_vertex(VId(2), vec![], vec![]);
                seed.add_edge(EId(1), VId(1), VId(2), vec![]);
                db.db
                    .write(&root)
                    .await
                    .unwrap()
                    .write(&contexts.commit(), seed)
                    .await
                    .unwrap();
            }
            for (index, capability) in scoped.iter().enumerate() {
                let mut projected = success(subscribe(&root, db, capability, &statement).await);
                let initial = success(poll(&root, db, capability, &mut projected).await).unwrap();
                let expected = if hidden && index != 0 {
                    vec![
                        (1, vec![WireValue::Vertex(1), WireValue::Vertex(1)]),
                        (1, vec![WireValue::Vertex(1), WireValue::Vertex(2)]),
                    ]
                } else {
                    vec![]
                };
                assert_eq!(initial.entries, expected);
                projected.consumer.close();
                drop(db.db.write(&root).await.unwrap());
                assert_eq!(db.subscriptions.load(Ordering::Acquire), 0);
            }
        }
        for text in [
            "SUBSCRIBE TO CALL fnx.connected_components(\
             {relations:['R']}) YIELD node,component",
            "SUBSCRIBE TO CALL fnx.connected_components(\
             {relations:['R'],parallel:'collapse',direction:'directed'}) YIELD node,component",
            "SUBSCRIBE TO CALL fnx.connected_components(\
             {relations:['R'],parallel:'collapse',self_loops:'reject'}) YIELD node,component",
            "SUBSCRIBE TO CALL fnx.degree_centrality(\
             {relations:['R'],parallel:'collapse'}) YIELD node,score LIMIT 0",
        ] {
            refused(
                subscribe(&root, db, &token, &request(text)).await,
                ErrorCode::Statement,
            );
            assert_eq!(db.subscriptions.load(Ordering::Acquire), 0);
        }
        let tiny = token.attenuate(Restriction::MaxRows(1)).unwrap();
        refused(
            subscribe(&root, db, &tiny, &statement).await,
            ErrorCode::Budget,
        );
        assert_eq!(db.subscriptions.load(Ordering::Acquire), 0);
        let mut subscription = success(subscribe(&root, db, &token, &statement).await);
        assert_eq!(db.subscriptions.load(Ordering::Acquire), 1);
        // Without RETURN, the CALL exposes its canonical named schema.
        assert_eq!(subscription.columns, ["component", "node"]);
        // A later narrowed capability cannot read or consume a pending result.
        refused(
            poll(&root, db, &scoped[0], &mut subscription).await,
            ErrorCode::PermissionDenied,
        );
        let delivered = success(poll(&root, db, &token, &mut subscription).await).unwrap();
        assert_eq!(
            delivered.entries,
            vec![
                (1, vec![WireValue::Vertex(1), WireValue::Vertex(1)]),
                (1, vec![WireValue::Vertex(1), WireValue::Vertex(2)]),
            ]
        );
        // Induced-label component circuits now have a native derivative.
        // Keep the original statement as a positive: the original unlabelled
        // graph contributes no rows, then labelling only V1 contributes its
        // isolated component. V2 and its crossing edge remain excluded.
        let selected = request(
            "SUBSCRIBE TO CALL fnx.connected_components(\
             {relations:['R'],parallel:'collapse',labels:['L']}) YIELD node,component",
        );
        let mut labelled = success(subscribe(&root, db, &token, &selected).await);
        assert_eq!(db.subscriptions.load(Ordering::Acquire), 2);
        let empty = success(poll(&root, db, &token, &mut labelled).await).unwrap();
        assert!(empty.entries.is_empty());
        labelled
            .consumer
            .acknowledge(empty.batch.receipt())
            .unwrap();
        let mut update = fgdb::WriteBatch::new(RelationId(1));
        update.set_vertex_label(VId(1), LabelId(1), true);
        let changed = db
            .db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), update)
            .await
            .unwrap();
        let delta = success(poll(&root, db, &token, &mut labelled).await).unwrap();
        assert_eq!(delta.batch.frontier(), changed);
        assert_eq!(
            delta.entries,
            vec![(1, vec![WireValue::Vertex(1), WireValue::Vertex(1)])]
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn clustering_subscription_replays_degree_only_scores_empty_ticks_and_canonical_pages() {
    use fgdb_delta_types::RelationId;
    use fgdb_types::{EId, VId};

    let ((), report) = run_async_under_lab(0x79a0_03d1, |root| async move {
        let (mut server, token, _) = served(&root, "subscription-clustering").await;
        Arc::get_mut(server.databases.get_mut("test").unwrap())
            .unwrap()
            .symbols
            .bind(fgdb_gql::GraphSymbolKind::Relation, "R", 1)
            .unwrap();
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let wide = 1_u128 << 100;
        let mut seed = vertex(1);
        for id in [2, 3, wide] {
            seed.create_vertex(VId(id), vec![], vec![]);
        }
        for (eid, a, b) in [(1, 1, 2), (2, 2, 3), (3, 3, 1), (4, wide, wide)] {
            seed.add_edge(EId(eid), VId(a), VId(b), vec![]);
        }
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), seed)
            .await
            .unwrap();
        let statement = request(
            "SUBSCRIBE TO CALL fnx.clustering_coefficient(\
             {relations:['R'],parallel:'collapse',self_loops:'keep'}) \
             YIELD score AS local,vertex AS member RETURN local,member SKIP 1 LIMIT 3",
        );
        let mut subscription = success(subscribe(&root, db, &token, &statement).await);
        assert_eq!(subscription.columns, ["local", "member"]);
        let row = |vertex, score| vec![WireValue::Float(score), WireValue::Vertex(vertex)];
        let initial = success(poll(&root, db, &token, &mut subscription).await).unwrap();
        assert!(initial.batch.is_snapshot());
        // The CALL is canonically sorted in score,member order before RETURN.
        // Its zero-score wide isolate is skipped; all three triangle vertices remain.
        assert_eq!(
            initial.entries,
            vec![(1, row(1, 1.0)), (1, row(2, 1.0)), (1, row(3, 1.0))]
        );

        let mut expected = Vec::new();
        for tick in 0..4 {
            let mut change = fgdb::WriteBatch::new(RelationId(1));
            match tick {
                0 => {
                    change.add_edge(EId(5), VId(1), VId(wide), vec![]);
                }
                1 => {
                    change.add_edge(EId(6), VId(wide), VId(1), vec![]);
                }
                2 => {
                    change.delete_edge(EId(5));
                }
                _ => {
                    change.delete_vertex(VId(wide));
                }
            }
            let at = db
                .db
                .write(&root)
                .await
                .unwrap()
                .write(&contexts.commit(), change)
                .await
                .unwrap();
            expected.push(at);
        }
        let repeat = success(poll(&root, db, &token, &mut subscription).await).unwrap();
        assert!(Arc::ptr_eq(&repeat.batch, &initial.batch));
        assert_eq!(repeat.entries, initial.entries);
        subscription
            .consumer
            .acknowledge(initial.batch.receipt())
            .unwrap();
        let mut after = initial.batch.frontier();
        for (tick, at) in expected.into_iter().enumerate() {
            let frame = success(poll(&root, db, &token, &mut subscription).await).unwrap();
            assert_eq!(
                (frame.batch.from(), frame.batch.frontier()),
                (Some(after), at)
            );
            match tick {
                0 => {
                    assert_eq!(
                        frame.entries,
                        vec![(1, row(1, 1.0 / 3.0)), (-1, row(1, 1.0))]
                    );
                    let WireValue::Float(score) = frame.entries[0].1[0] else {
                        panic!("wire score");
                    };
                    assert_eq!(score.to_bits(), (1.0_f64 / 3.0).to_bits());
                }
                1 | 2 => assert!(
                    frame.entries.is_empty(),
                    "remaining physical support preserves the degree"
                ),
                _ => {
                    // Removing the wide isolate's lifetime also removes the
                    // first canonical row; vertex 1 now falls outside the page.
                    assert_eq!(frame.entries, vec![(-1, row(1, 1.0 / 3.0))]);
                }
            }
            subscription
                .consumer
                .acknowledge(frame.batch.receipt())
                .unwrap();
            after = at;
        }
        assert!(success(poll(&root, db, &token, &mut subscription).await).is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn clustering_subscription_masks_hidden_topology_before_registration_or_delivery() {
    use fgdb_delta_types::{LabelId, RelationId};
    use fgdb_types::{EId, VId};
    use fgdb_warden::Scope;

    let ((), report) = run_async_under_lab(0x79a0_03d2, |root| async move {
        let (mut server, token, _) = served(&root, "subscription-clustering-scope").await;
        let served = Arc::get_mut(server.databases.get_mut("test").unwrap()).unwrap();
        served
            .symbols
            .bind(fgdb_gql::GraphSymbolKind::Relation, "R", 1)
            .unwrap();
        served
            .symbols
            .bind(fgdb_gql::GraphSymbolKind::Label, "L", 1)
            .unwrap();
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let statement = request(
            "SUBSCRIBE TO CALL fnx.clustering_coefficient(\
             {relations:['R'],parallel:'collapse',self_loops:'drop'}) YIELD vertex,score",
        );
        let scoped = [
            token
                .attenuate(Restriction::Labels(Scope::only([LabelId(1)])))
                .unwrap(),
            token
                .attenuate(Restriction::Relations(Scope::only([RelationId(1)])))
                .unwrap(),
            token
                .attenuate(Restriction::Properties(Scope::only([])))
                .unwrap(),
        ];
        for hidden in [false, true] {
            if hidden {
                let mut seed = vertex(1);
                for id in [2, 3] {
                    seed.create_vertex(VId(id), vec![], vec![]);
                }
                for (id, a, b) in [(1, 1, 2), (2, 2, 3), (3, 3, 1)] {
                    seed.add_edge(EId(id), VId(a), VId(b), vec![]);
                }
                db.db
                    .write(&root)
                    .await
                    .unwrap()
                    .write(&contexts.commit(), seed)
                    .await
                    .unwrap();
            }
            for (index, token) in scoped.iter().enumerate() {
                let mut projected = success(subscribe(&root, db, token, &statement).await);
                let initial = success(poll(&root, db, token, &mut projected).await).unwrap();
                let expected = if hidden && index != 0 {
                    [1, 2, 3]
                        .map(|vertex| (1, vec![WireValue::Float(1.0), WireValue::Vertex(vertex)]))
                        .to_vec()
                } else {
                    vec![]
                };
                assert_eq!(initial.entries, expected);
                projected.consumer.close();
                drop(db.db.write(&root).await.unwrap());
                assert_eq!(db.subscriptions.load(Ordering::Acquire), 0);
            }
        }
        for text in [
            "SUBSCRIBE TO CALL fnx.clustering_coefficient({relations:['R'],self_loops:'drop'}) YIELD vertex,score",
            "SUBSCRIBE TO CALL fnx.clustering_coefficient({relations:['R'],parallel:'collapse',self_loops:'reject'}) YIELD vertex,score",
            "SUBSCRIBE TO CALL fnx.clustering_coefficient({relations:['R'],parallel:'collapse',direction:'directed',self_loops:'drop'}) YIELD vertex,score LIMIT 0",
            "SUBSCRIBE TO CALL fnx.directed_clustering_coefficient({relations:['R'],parallel:'collapse',self_loops:'drop'}) YIELD vertex,score LIMIT 0",
        ] {
            refused(
                subscribe(&root, db, &token, &request(text)).await,
                ErrorCode::Statement,
            );
            assert_eq!(db.subscriptions.load(Ordering::Acquire), 0);
        }
        let mut subscription = success(subscribe(&root, db, &token, &statement).await);
        assert_eq!(subscription.columns, ["score", "vertex"]);
        for token in &scoped {
            refused(
                poll(&root, db, token, &mut subscription).await,
                ErrorCode::PermissionDenied,
            );
        }
        let initial = success(poll(&root, db, &token, &mut subscription).await).unwrap();
        assert!(initial.batch.is_snapshot());
        assert_eq!(
            initial.entries,
            [1, 2, 3]
                .map(|vertex| (1, vec![WireValue::Float(1.0), WireValue::Vertex(vertex)]))
                .to_vec()
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn selected_union_analytics_subscriptions_replay_exact_changes_without_exposing_excluded_vertices()
{
    use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
    use fgdb_types::{CanonicalScalar, EId, VId};
    use fgdb_warden::Scope;
    use std::collections::BTreeMap;

    let ((), report) = run_async_under_lab(0x79a0_03d3, |root| async move {
        let (mut server, token, _) = served(&root, "subscription-selected-analytics").await;
        let served = Arc::get_mut(server.databases.get_mut("test").unwrap()).unwrap();
        served
            .symbols
            .bind(fgdb_gql::GraphSymbolKind::Relation, "R", 1)
            .unwrap();
        served
            .symbols
            .bind(fgdb_gql::GraphSymbolKind::Label, "L", 1)
            .unwrap();
        served
            .symbols
            .bind(fgdb_gql::GraphSymbolKind::Label, "M", 2)
            .unwrap();
        served
            .symbols
            .bind(fgdb_gql::GraphSymbolKind::Relation, "S", 2)
            .unwrap();
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let wide = u128::MAX;
        let mut seed = vertex(1);
        for id in [2, 3, 4, wide] {
            seed.create_vertex(VId(id), vec![], vec![]);
        }
        for (eid, a, b) in [
            (1, 1, 2),
            (2, 2, 3),
            (4, 1, 2),
            (5, 2, 1),
            (6, wide, wide),
            (10, 1, 4),
            (11, 4, 2),
        ] {
            seed.add_edge(EId(eid), VId(a), VId(b), vec![]);
        }
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), seed)
            .await
            .unwrap();
        let mut second = fgdb::WriteBatch::new(RelationId(2));
        second.add_edge(EId(3), VId(3), VId(1), vec![]);
        second.add_edge(EId(7), VId(1), VId(2), vec![]);
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), second)
            .await
            .unwrap();
        let mut excluded = fgdb::WriteBatch::new(RelationId(3));
        excluded.add_edge(EId(8), VId(1), VId(wide), vec![]);
        excluded.add_edge(EId(9), VId(wide), VId(2), vec![]);
        let mut after = db
            .db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), excluded)
            .await
            .unwrap();
        let scoped = token
            .attenuate(Restriction::Labels(Scope::only([LabelId(1)])))
            .unwrap();
        let mut subscriptions = Vec::new();
        for (metric, field) in [
            ("triangles", "triangles"),
            ("clustering_coefficient", "score"),
            ("core_number", "core"),
        ] {
            let statement = request(&format!(
                "SUBSCRIBE TO CALL fnx.{metric}(\
                 {{relations:['S','R','S'],labels:['M','L','M'],parallel:'collapse',self_loops:'drop'}}) \
                 YIELD vertex AS node,{field} AS value RETURN node,value"
            ));
            // Explicit selectors intersect the capability-projected topology.
            let mut projected = success(subscribe(&root, db, &scoped, &statement).await);
            let empty = success(poll(&root, db, &scoped, &mut projected).await).unwrap();
            assert!(empty.entries.is_empty());
            projected.consumer.close();
            drop(db.db.write(&root).await.unwrap());
            let mut subscription = success(subscribe(&root, db, &token, &statement).await);
            assert_eq!(subscription.columns, ["node", "value"]);
            let initial = success(poll(&root, db, &token, &mut subscription).await).unwrap();
            assert!(initial.batch.is_snapshot());
            assert_eq!(initial.batch.frontier(), after);
            assert!(initial.entries.is_empty());
            subscriptions.push((metric, subscription, initial, BTreeMap::new()));
        }
        assert_eq!(db.subscriptions.load(Ordering::Acquire), 3);

        for tick in 0..7 {
            let relation = if tick == 3 {
                RelationId(2)
            } else {
                RelationId(1)
            };
            let mut change = fgdb::WriteBatch::new(relation);
            match tick {
                0 => {
                    change.set_vertex_label(VId(1), LabelId(1), true);
                    change.set_vertex_label(VId(2), LabelId(2), true);
                }
                1 => {
                    change.set_vertex_label(VId(3), LabelId(1), true);
                    change.set_vertex_label(VId(3), LabelId(2), true);
                    change.set_vertex_label(VId(wide), LabelId(2), true);
                }
                2 => {
                    // The second label and the second relation retain support.
                    change.set_vertex_label(VId(3), LabelId(1), false);
                    for id in [1, 4, 5] {
                        change.delete_edge(EId(id));
                    }
                }
                3 => {
                    change.delete_edge(EId(7));
                }
                4 => {
                    change.set_vertex_label(VId(3), LabelId(2), false);
                }
                5 => {
                    change.set_vertex_property(
                        VId(3),
                        PropertyKeyId(1),
                        Some(CanonicalScalar::Int(7)),
                    );
                }
                _ => {
                    change.set_vertex_label(VId(1), LabelId(1), false);
                    change.set_vertex_label(VId(2), LabelId(2), false);
                    change.set_vertex_label(VId(wide), LabelId(2), false);
                }
            }
            let at = db
                .db
                .write(&root)
                .await
                .unwrap()
                .write(&contexts.commit(), change)
                .await
                .unwrap();
            for (metric, subscription, initial, previous) in &mut subscriptions {
                if tick == 0 {
                    // A new topology cannot overwrite the unacknowledged baseline.
                    let repeated = success(poll(&root, db, &token, subscription).await).unwrap();
                    assert!(Arc::ptr_eq(&repeated.batch, &initial.batch));
                    assert!(repeated.entries.is_empty());
                    subscription
                        .consumer
                        .acknowledge(initial.batch.receipt())
                        .unwrap();
                }
                refused(
                    poll(&root, db, &scoped, subscription).await,
                    ErrorCode::PermissionDenied,
                );
                let core = *metric == "core_number";
                let values: BTreeMap<u128, i64> = match tick {
                    0 => [(1, i64::from(core)), (2, i64::from(core))]
                        .into_iter()
                        .collect(),
                    1 | 2 => [
                        (1, if core { 2 } else { 1 }),
                        (2, if core { 2 } else { 1 }),
                        (3, if core { 2 } else { 1 }),
                        (wide, 0),
                    ]
                    .into_iter()
                    .collect(),
                    3 => [
                        (1, i64::from(core)),
                        (2, i64::from(core)),
                        (3, i64::from(core)),
                        (wide, 0),
                    ]
                    .into_iter()
                    .collect(),
                    4 | 5 => [(1, 0), (2, 0), (wide, 0)].into_iter().collect(),
                    _ => BTreeMap::new(),
                };
                let expected: BTreeMap<(u128, i64), i128> = values
                    .into_iter()
                    .map(|(vertex, value)| ((vertex, value), 1))
                    .collect();
                let mut derivative = expected.clone();
                for (row, weight) in previous.iter() {
                    *derivative.entry(*row).or_default() -= weight;
                }
                let frame = success(poll(&root, db, &token, subscription).await).unwrap();
                assert_eq!(
                    (frame.batch.from(), frame.batch.frontier()),
                    (Some(after), at)
                );
                let expected_entries: Vec<_> = derivative
                    .into_iter()
                    .filter(|(_, weight)| *weight != 0)
                    .map(|((vertex, value), weight)| {
                        let value = if *metric == "clustering_coefficient" {
                            WireValue::Float(value as f64)
                        } else {
                            WireValue::Int(value)
                        };
                        (weight, vec![WireValue::Vertex(vertex), value])
                    })
                    .collect();
                assert_eq!(frame.entries, expected_entries, "{metric}, tick {tick}");
                subscription
                    .consumer
                    .acknowledge(frame.batch.receipt())
                    .unwrap();
                *previous = expected;
            }
            after = at;
        }
        for (_, subscription, _, _) in &mut subscriptions {
            assert!(success(poll(&root, db, &token, subscription).await).is_none());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn capability_quotient_noninterference_10000_hidden_mutations_and_revocation() {
    use fgdb::WriteBatch;
    use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
    use fgdb_gql::GraphSymbolKind;
    use fgdb_types::{CanonicalScalar, CommitSeq, EId, VId};
    use fgdb_warden::Scope;

    let ((), report) = run_async_under_lab(0x79a0_20ca, |root| async move {
        let (mut server1, root_tok1, _) = served(&root, "cap-sub-d1").await;
        let (mut server2, root_tok2, _) = served(&root, "cap-sub-d2").await;
        for server in [&mut server1, &mut server2] {
            let db = Arc::get_mut(server.databases.get_mut("test").unwrap()).unwrap();
            for (kind, name, id) in [
                (GraphSymbolKind::Label, "Person", 1),
                (GraphSymbolKind::Label, "Secret", 2),
                (GraphSymbolKind::Relation, "KNOWS", 1),
                (GraphSymbolKind::Relation, "SECRET_EDGE", 2),
                (GraphSymbolKind::Property, "name", 1),
                (GraphSymbolKind::Property, "ssn", 2),
            ] {
                db.symbols.bind(kind, name, id).unwrap();
            }
        }
        let db1 = &server1.databases["test"];
        let db2 = &server2.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit_cx = contexts.commit();

        // Pre-populate D2 with hidden vertices/edges before visible seed so physical
        // CommitSeqs and physical snapshot record counts diverge before registration.
        {
            let mut pre = WriteBatch::new(RelationId(2));
            for i in 0..50_u128 {
                let vid = VId(10_000 + i);
                pre.create_vertex(
                    vid,
                    vec![LabelId(2)],
                    vec![(PropertyKeyId(2), CanonicalScalar::Int(i as i64))],
                );
                if i > 0 {
                    pre.add_edge(
                        EId(20_000 + i),
                        VId(10_000 + i - 1),
                        vid,
                        vec![(PropertyKeyId(2), CanonicalScalar::Int(999))],
                    );
                }
            }
            db2.db
                .write(&root)
                .await
                .unwrap()
                .write(&commit_cx, pre)
                .await
                .unwrap();
        }

        // Seed identical visible projection Pi_C(D1) == Pi_C(D2), while D2 also
        // attaches hidden label Secret and hidden property ssn to visible vertices.
        {
            let mut seed1 = WriteBatch::new(RelationId(1));
            seed1.create_vertex(
                VId(1),
                vec![LabelId(1)],
                vec![(
                    PropertyKeyId(1),
                    CanonicalScalar::ucs_basic_text("Ada").unwrap(),
                )],
            );
            seed1.create_vertex(
                VId(2),
                vec![LabelId(1)],
                vec![(
                    PropertyKeyId(1),
                    CanonicalScalar::ucs_basic_text("Grace").unwrap(),
                )],
            );
            db1.db
                .write(&root)
                .await
                .unwrap()
                .write(&commit_cx, seed1)
                .await
                .unwrap();

            let mut seed2 = WriteBatch::new(RelationId(1));
            seed2.create_vertex(
                VId(1),
                vec![LabelId(1), LabelId(2)],
                vec![
                    (
                        PropertyKeyId(1),
                        CanonicalScalar::ucs_basic_text("Ada").unwrap(),
                    ),
                    (
                        PropertyKeyId(2),
                        CanonicalScalar::ucs_basic_text("000-01-0001").unwrap(),
                    ),
                ],
            );
            seed2.create_vertex(
                VId(2),
                vec![LabelId(1)],
                vec![
                    (
                        PropertyKeyId(1),
                        CanonicalScalar::ucs_basic_text("Grace").unwrap(),
                    ),
                    (
                        PropertyKeyId(2),
                        CanonicalScalar::ucs_basic_text("000-02-0002").unwrap(),
                    ),
                ],
            );
            db2.db
                .write(&root)
                .await
                .unwrap()
                .write(&commit_cx, seed2)
                .await
                .unwrap();

            let mut seed2_secret_edge = WriteBatch::new(RelationId(2));
            seed2_secret_edge.add_edge(EId(9001), VId(1), VId(2), vec![]);
            db2.db
                .write(&root)
                .await
                .unwrap()
                .write(&commit_cx, seed2_secret_edge)
                .await
                .unwrap();
        }

        let attenuate = |tok: &CapabilityToken| {
            tok.attenuate(Restriction::Rights(fgdb_warden::Rights::Read))
                .unwrap()
                .attenuate(Restriction::Labels(Scope::only([LabelId(1)])))
                .unwrap()
                .attenuate(Restriction::Relations(Scope::only([RelationId(1)])))
                .unwrap()
                .attenuate(Restriction::Properties(Scope::only([PropertyKeyId(1)])))
                .unwrap()
                // Tight node budget (~113 work units per circuit stage): plenty for
                // 2 visible vertices, far below 200 hidden ops/commit (10,000 total)!
                .attenuate(Restriction::MaxNodes(4_096))
                .unwrap()
                .attenuate(Restriction::MaxWork(200_000))
                .unwrap()
        };
        let scoped1 = attenuate(&root_tok1);
        let scoped2 = attenuate(&root_tok2);

        let stmt = request(
            "SUBSCRIBE TO MATCH (p:Person) \
             RETURN p.name AS name, COUNT { MATCH (p)-[:KNOWS]->(:Person) } AS friends, p.ssn AS ssn",
        );
        let mut sub1 = success(subscribe(&root, db1, &scoped1, &stmt).await);
        let mut sub2 = success(subscribe(&root, db2, &scoped2, &stmt).await);

        let base1 = success(poll(&root, db1, &scoped1, &mut sub1).await).unwrap();
        let base2 = success(poll(&root, db2, &scoped2, &mut sub2).await).unwrap();
        assert!(base1.batch.is_snapshot() && base2.batch.is_snapshot());
        assert_eq!(base1.batch.frontier(), CommitSeq(1));
        assert_eq!(base2.batch.frontier(), CommitSeq(1));
        assert_eq!(base1.entries, base2.entries);
        assert_eq!(
            base2.entries,
            vec![
                (
                    1,
                    vec![
                        WireValue::Text("Ada".into()),
                        WireValue::Int(0),
                        WireValue::Null
                    ]
                ),
                (
                    1,
                    vec![
                        WireValue::Text("Grace".into()),
                        WireValue::Int(0),
                        WireValue::Null
                    ]
                ),
            ]
        );
        sub1.consumer.acknowledge(base1.batch.receipt()).unwrap();
        sub2.consumer.acknowledge(base2.batch.receipt()).unwrap();

        // Apply 10,000 hidden mutations on D2 across 50 commits (200 hidden ops each):
        // hidden SECRET_EDGE creations, hidden Secret vertices, and hidden ssn updates
        // on visible Person(1). Every poll must return None with zero frontier leaks!
        let mut hidden_ops = 0_usize;
        for commit_idx in 0..50_u128 {
            let mut hidden = WriteBatch::new(RelationId(2));
            for slot in 0..99_u128 {
                let hvid = VId(30_000 + commit_idx * 100 + slot);
                let heid = EId(60_000 + commit_idx * 100 + slot);
                hidden.create_vertex(
                    hvid,
                    vec![LabelId(2)],
                    vec![(PropertyKeyId(2), CanonicalScalar::Int(slot as i64))],
                );
                hidden.add_edge(heid, VId(1), hvid, vec![]);
                hidden_ops += 2;
            }
            hidden.set_vertex_property(
                VId(1),
                PropertyKeyId(2),
                Some(CanonicalScalar::Int(commit_idx as i64)),
            );
            hidden.add_edge(EId(90_000 + commit_idx), VId(1), VId(2), vec![]);
            hidden_ops += 2;
            db2.db
                .write(&root)
                .await
                .unwrap()
                .write(&commit_cx, hidden)
                .await
                .unwrap();
            assert!(
                success(poll(&root, db2, &scoped2, &mut sub2).await).is_none(),
                "hidden commit {commit_idx} leaked a visible delta or exhausted budget"
            );
        }
        assert_eq!(hidden_ops, 10_000);

        // Visible transition 1: Ada -[:KNOWS]-> Grace (with extra hidden noise in the SAME commit on D2).
        {
            let mut vis1 = WriteBatch::new(RelationId(1));
            vis1.add_edge(EId(501), VId(1), VId(2), vec![]);
            db1.db
                .write(&root)
                .await
                .unwrap()
                .write(&commit_cx, vis1)
                .await
                .unwrap();

            let mut vis2 = WriteBatch::new(RelationId(1));
            vis2.add_edge(EId(501), VId(1), VId(2), vec![]);
            vis2.set_vertex_property(VId(2), PropertyKeyId(2), Some(CanonicalScalar::Int(777)));
            db2.db
                .write(&root)
                .await
                .unwrap()
                .write(&commit_cx, vis2)
                .await
                .unwrap();
        }
        let d1 = success(poll(&root, db1, &scoped1, &mut sub1).await).unwrap();
        let d2 = success(poll(&root, db2, &scoped2, &mut sub2).await).unwrap();
        assert_eq!(
            (d1.batch.from(), d1.batch.frontier()),
            (Some(CommitSeq(1)), CommitSeq(2))
        );
        assert_eq!(
            (d2.batch.from(), d2.batch.frontier()),
            (Some(CommitSeq(1)), CommitSeq(2))
        );
        assert_eq!(d1.entries, d2.entries);
        assert_eq!(
            d2.entries,
            vec![
                (
                    -1,
                    vec![
                        WireValue::Text("Ada".into()),
                        WireValue::Int(0),
                        WireValue::Null
                    ]
                ),
                (
                    1,
                    vec![
                        WireValue::Text("Ada".into()),
                        WireValue::Int(1),
                        WireValue::Null
                    ]
                ),
            ]
        );
        sub1.consumer.acknowledge(d1.batch.receipt()).unwrap();
        sub2.consumer.acknowledge(d2.batch.receipt()).unwrap();

        // Mid-stream capability expiry immediately refuses polling.
        let expired = scoped2.attenuate(Restriction::ExpiresBefore(1)).unwrap();
        refused(
            poll(&root, db2, &expired, &mut sub2).await,
            ErrorCode::PermissionDenied,
        );

        // Mid-stream issuer retirement / revocation epoch bump immediately refuses polling.
        assert!(db2.authority.retire());
        refused(
            poll(&root, db2, &scoped2, &mut sub2).await,
            ErrorCode::PermissionDenied,
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
