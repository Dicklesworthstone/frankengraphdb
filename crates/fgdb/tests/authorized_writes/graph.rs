use super::*;
use fgdb_types::EId;

/// Edge deletion needs every property, but not every relation or endpoint label.
fn edge_delete_grant() -> Grant {
    Grant {
        properties: Scope::All,
        ..grant()
    }
}

#[test]
fn mixed_graph_batch_uses_native_ensure_aliases_and_reopens() {
    under_lab(0xa911, |contexts| async move {
        let cx = contexts.commit();
        let txn = contexts.txn();
        let path = scratch("graph-reopen");
        let mut db = Database::create(&cx, &path, keys()).await.unwrap();
        let authority = issuer(NAMESPACE);
        let token = authority.issue_at(&edge_delete_grant(), NOW).unwrap();
        let baseline = txn.outstanding_obligations();
        let frontier = db.frontier().unwrap();
        let mut batch = WriteBatch::new(R);
        for vid in [VId(1), VId(2), VId(3)] {
            batch.create_vertex(vid, vec![L], vec![]);
        }
        batch.add_edge(EId(10), VId(1), VId(2), vec![(P, CanonicalScalar::Int(1))]);
        batch.ensure_edge_by_triple(EId(99), VId(1), VId(2), vec![(P, CanonicalScalar::Int(99))]);
        batch.set_edge_property(EId(10), P, Some(CanonicalScalar::Int(7)));
        batch.add_edge(EId(11), VId(1), VId(1), vec![]);
        batch.add_edge(EId(12), VId(2), VId(3), vec![]);
        batch.delete_edge(EId(12));
        let seq = db
            .write_authorized(&txn, &cx, &authority, &token, BRANCH, batch, || NOW)
            .await
            .unwrap();
        assert_eq!(seq.0, frontier.0 + 1);
        assert_eq!(txn.outstanding_obligations(), baseline);
        assert_eq!(
            db.edge_at(EId(10), seq).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(7))]
        );
        assert!(db.edge_at(EId(99), seq).unwrap().is_none());
        assert!(db.edge_at(EId(12), seq).unwrap().is_none());
        assert_eq!(db.neighbours(VId(1), R).unwrap(), vec![VId(1), VId(2)]);
        drop(db);
        let reopened = Database::open_rebuilding(&cx, &path, keys()).await.unwrap();
        assert_eq!(reopened.frontier().unwrap(), seq);
        assert_eq!(
            reopened.edge_at(EId(10), seq).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(7))]
        );
        assert!(reopened.edge_at(EId(99), seq).unwrap().is_none());
        assert!(reopened.edge_at(EId(12), seq).unwrap().is_none());
        assert_eq!(
            reopened.neighbours(VId(1), R).unwrap(),
            vec![VId(1), VId(2)]
        );
    });
}

#[test]
fn vertex_delete_authorizes_and_removes_all_prefix_incidence() {
    under_lab(0xa912, |contexts| async move {
        let cx = contexts.commit();
        let txn = contexts.txn();
        let mut db = Database::create(&cx, &scratch("prefix-cascade"), keys())
            .await
            .unwrap();
        let authority = issuer(NAMESPACE);
        let token = authority.issue_at(&total_grant(), NOW).unwrap();
        let baseline = txn.outstanding_obligations();
        let batch = || {
            let mut batch = WriteBatch::new(R);
            for vid in [VId(1), VId(2), VId(3)] {
                batch.create_vertex(vid, vec![L], vec![]);
            }
            batch.add_edge(EId(10), VId(1), VId(2), vec![]);
            batch.add_edge(EId(11), VId(2), VId(3), vec![(P, CanonicalScalar::Int(1))]);
            batch.add_edge(EId(12), VId(2), VId(2), vec![]);
            batch.add_edge(EId(13), VId(1), VId(3), vec![]);
            batch.delete_vertex(VId(2));
            batch
        };
        // A scoped capability may not delete vertices at all (fgdb-4iiho),
        // even when everything the batch touches is visible to it.
        let scoped = authority.issue_at(&grant(), NOW).unwrap();
        let frontier = db.frontier().unwrap();
        assert!(matches!(
            db.write_authorized(&txn, &cx, &authority, &scoped, BRANCH, batch(), || NOW)
                .await,
            Err(WriteTxnError::Authorization(Error::ScopeDenied))
        ));
        assert_eq!(db.frontier().unwrap(), frontier);
        assert_eq!(txn.outstanding_obligations(), baseline);
        let batch = batch();
        let seq = db
            .write_authorized(&txn, &cx, &authority, &token, BRANCH, batch, || NOW)
            .await
            .unwrap();
        assert!(db.vertex(VId(2)).unwrap().is_none());
        for eid in [EId(10), EId(11), EId(12)] {
            assert!(db.edge_at(eid, seq).unwrap().is_none());
        }
        assert!(db.edge_at(EId(13), seq).unwrap().is_some());
        assert_eq!(txn.outstanding_obligations(), baseline);
        // Native normalization cancels the never-durable vertex creation.
        let mut recreate = WriteBatch::new(R);
        recreate.create_vertex(VId(2), vec![L], vec![]);
        db.write_authorized(&txn, &cx, &authority, &token, BRANCH, recreate, || NOW)
            .await
            .unwrap();
        assert!(db.vertex(VId(2)).unwrap().is_some());
        assert_eq!(txn.outstanding_obligations(), baseline);
    });
}

#[test]
fn hidden_relation_in_a_cascade_refuses_the_whole_batch() {
    under_lab(0xa913, |contexts| async move {
        let cx = contexts.commit();
        let txn = contexts.txn();
        let path = scratch("cross-relation-cascade");
        let mut db = Database::create(&cx, &path, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for vid in [VId(1), VId(2), VId(3)] {
            seed.create_vertex(vid, vec![L], vec![]);
        }
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(&cx, seed).await.unwrap();
        let mut secret = WriteBatch::new(RelationId(2));
        secret.add_edge(EId(20), VId(3), VId(1), vec![]);
        db.write(&cx, secret).await.unwrap();
        let frontier = db.frontier().unwrap();
        let baseline = txn.outstanding_obligations();
        let authority = issuer(NAMESPACE);
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let mut delete = WriteBatch::new(R);
        delete.create_vertex(VId(4), vec![L], vec![]);
        delete.delete_vertex(VId(1));
        assert!(matches!(
            db.write_authorized(&txn, &cx, &authority, &token, BRANCH, delete, || NOW)
                .await,
            Err(WriteTxnError::Authorization(Error::ScopeDenied))
        ));
        assert_eq!(db.frontier().unwrap(), frontier);
        assert!(db.vertex(VId(4)).unwrap().is_none());
        assert!(db.vertex(VId(1)).unwrap().is_some());
        for eid in [EId(10), EId(20)] {
            assert!(db.edge_at(eid, frontier).unwrap().is_some());
        }
        assert_eq!(txn.outstanding_obligations(), baseline);
        let token = authority.issue_at(&total_grant(), NOW).unwrap();
        let mut delete = WriteBatch::new(R);
        delete.delete_vertex(VId(1));
        let seq = db
            .write_authorized(&txn, &cx, &authority, &token, BRANCH, delete, || NOW)
            .await
            .unwrap();
        assert_eq!(seq.0, frontier.0 + 1);
        assert_eq!(txn.outstanding_obligations(), baseline);
        drop(db);
        let reopened = Database::open_rebuilding(&cx, &path, keys()).await.unwrap();
        assert!(reopened.vertex(VId(1)).unwrap().is_none());
        assert!(reopened.vertex(VId(2)).unwrap().is_some());
        assert!(reopened.vertex(VId(3)).unwrap().is_some());
        for eid in [EId(10), EId(20)] {
            assert!(reopened.edge_at(eid, seq).unwrap().is_none());
        }
    });
}

#[test]
fn hidden_endpoint_blocks_edge_create_update_and_cascade() {
    under_lab(0xa914, |contexts| async move {
        let cx = contexts.commit();
        let txn = contexts.txn();
        let mut db = Database::create(&cx, &scratch("hidden-endpoint"), keys())
            .await
            .unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![L], vec![]);
        seed.create_vertex(VId(2), vec![HIDDEN], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![(P, CanonicalScalar::Int(1))]);
        db.write(&cx, seed).await.unwrap();
        let frontier = db.frontier().unwrap();
        let baseline = txn.outstanding_obligations();
        let authority = issuer(NAMESPACE);
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let mut create = WriteBatch::new(R);
        create.add_edge(EId(20), VId(1), VId(2), vec![]);
        let mut update = WriteBatch::new(R);
        update.set_edge_property(EId(10), P, Some(CanonicalScalar::Int(7)));
        let mut delete = WriteBatch::new(R);
        delete.delete_vertex(VId(1));
        for batch in [create, update, delete] {
            assert!(matches!(
                db.write_authorized(&txn, &cx, &authority, &token, BRANCH, batch, || NOW)
                    .await,
                Err(WriteTxnError::Authorization(Error::ScopeDenied))
            ));
            assert_eq!(db.frontier().unwrap(), frontier);
            assert_eq!(txn.outstanding_obligations(), baseline);
            assert!(db.vertex(VId(1)).unwrap().is_some());
            assert!(db.edge_at(EId(20), frontier).unwrap().is_none());
            assert_eq!(
                db.edge_at(EId(10), frontier).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(1))]
            );
        }
    });
}

#[test]
fn ignored_ensure_alias_cannot_capture_an_unrelated_hidden_edge() {
    under_lab(0xa915, |contexts| async move {
        let cx = contexts.commit();
        let txn = contexts.txn();
        let mut db = Database::create(&cx, &scratch("alias-collision"), keys())
            .await
            .unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![L], vec![]);
        seed.create_vertex(VId(2), vec![L], vec![]);
        seed.create_vertex(VId(3), vec![HIDDEN], vec![]);
        seed.create_vertex(VId(4), vec![HIDDEN], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![(P, CanonicalScalar::Int(1))]);
        db.write(&cx, seed).await.unwrap();
        let mut hidden = WriteBatch::new(RelationId(2));
        hidden.add_edge(
            EId(99),
            VId(3),
            VId(4),
            vec![(SECRET, CanonicalScalar::Int(99))],
        );
        db.write(&cx, hidden).await.unwrap();
        let authority = issuer(NAMESPACE);
        // Vertex deletion needs a capability that hides nothing (fgdb-4iiho),
        // so edge 99 is visible here; it is still unrelated to vertex 1, and
        // its original record, not the ignored ensure spelling, decides that.
        let token = authority.issue_at(&total_grant(), NOW).unwrap();
        let mut batch = WriteBatch::new(R);
        batch.ensure_edge_by_triple(EId(99), VId(1), VId(2), vec![(P, CanonicalScalar::Int(55))]);
        batch.delete_vertex(VId(1));
        let seq = db
            .write_authorized(&txn, &cx, &authority, &token, BRANCH, batch, || NOW)
            .await
            .unwrap();
        assert!(db.vertex(VId(1)).unwrap().is_none());
        assert!(db.edge_at(EId(10), seq).unwrap().is_none());
        let untouched = db.edge_at(EId(99), seq).unwrap().unwrap();
        assert_eq!(untouched.entry.src, VId(3));
        assert_eq!(untouched.entry.dst, VId(4));
        assert_eq!(untouched.entry.relation, RelationId(2));
        assert_eq!(untouched.props, vec![(SECRET, CanonicalScalar::Int(99))]);
    });
}

#[test]
fn whole_object_deletion_cannot_erase_untouched_hidden_properties() {
    under_lab(0xa916, |contexts| async move {
        let cx = contexts.commit();
        let txn = contexts.txn();
        let mut db = Database::create(&cx, &scratch("hidden-fields-delete"), keys())
            .await
            .unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![L], vec![(SECRET, CanonicalScalar::Int(99))]);
        seed.create_vertex(VId(2), vec![L], vec![]);
        seed.add_edge(
            EId(10),
            VId(1),
            VId(2),
            vec![(SECRET, CanonicalScalar::Int(88))],
        );
        db.write(&cx, seed).await.unwrap();
        let authority = issuer(NAMESPACE);
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let mut update = WriteBatch::new(R);
        update.set_edge_property(EId(10), P, Some(CanonicalScalar::Int(7)));
        let frontier = db
            .write_authorized(&txn, &cx, &authority, &token, BRANCH, update, || NOW)
            .await
            .unwrap();
        assert_eq!(
            db.edge_at(EId(10), frontier).unwrap().unwrap().props,
            vec![
                (P, CanonicalScalar::Int(7)),
                (SECRET, CanonicalScalar::Int(88))
            ]
        );
        let baseline = txn.outstanding_obligations();
        let mut vertex = WriteBatch::new(R);
        vertex.delete_vertex_if_present(VId(1));
        let mut edge = WriteBatch::new(R);
        edge.delete_edge_if_present(EId(10));
        for batch in [vertex, edge] {
            assert!(matches!(
                db.write_authorized(&txn, &cx, &authority, &token, BRANCH, batch, || NOW)
                    .await,
                Err(WriteTxnError::Authorization(Error::ScopeDenied))
            ));
            assert_eq!(db.frontier().unwrap(), frontier);
            assert_eq!(txn.outstanding_obligations(), baseline);
        }
        assert!(db.vertex(VId(1)).unwrap().is_some());
        assert!(db.edge_at(EId(10), frontier).unwrap().is_some());
    });
}

/// The same visible pair 1 -> 2, optionally with a hidden neighbourhood on
/// vertex 1: allowed-relation edges to hidden vertices and hidden-relation
/// edges to the visible vertex 2. `alias` adds a visible R edge 1 -> 2.
async fn ensure_fixture(
    cx: &fgdb_types::CommitCx,
    hidden: bool,
    alias: bool,
) -> Database<fgdb::MemVfs> {
    let mut db = Database::open_memory(cx, keys()).await.unwrap();
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(1), vec![L], vec![]);
    batch.create_vertex(VId(2), vec![L], vec![]);
    if alias {
        batch.add_edge(EId(10), VId(1), VId(2), vec![]);
    }
    if hidden {
        for vid in 100..130_u128 {
            batch.create_vertex(VId(vid), vec![HIDDEN], vec![]);
            batch.add_edge(EId(1000 + vid), VId(1), VId(vid), vec![]);
        }
    }
    db.write(cx, batch).await.unwrap();
    if hidden {
        let mut other = WriteBatch::new(RelationId(2));
        for eid in 0..20_u128 {
            other.add_edge(EId(3000 + eid), VId(1), VId(2), vec![]);
        }
        db.write(cx, other).await.unwrap();
    }
    db
}

/// FG-INV-20 on the write path (fgdb-4iiho). Ensuring an edge scans the
/// source vertex's live incidence for an alias. A holder can attenuate MaxWork
/// without the issuer key, so the smallest MaxWork at which the write commits
/// must not move with the hidden degree of the source vertex, whether the
/// write creates the edge or resolves an existing visible alias.
#[test]
fn ensure_edge_threshold_cannot_count_hidden_incident_edges() {
    use fgdb_warden::Restriction;
    under_lab(0xa9a1, |contexts| async move {
        let cx = contexts.commit();
        let txn = contexts.txn();
        let authority = issuer(NAMESPACE);
        let token = authority.issue_at(&grant(), NOW).unwrap();
        for alias in [false, true] {
            let mut thresholds = Vec::new();
            for hidden in [false, true] {
                let (mut low, mut high) = (0_u64, 100_000_u64);
                while low < high {
                    let middle = low + (high - low) / 2;
                    let mut db = ensure_fixture(&cx, hidden, alias).await;
                    let mut batch = WriteBatch::new(R);
                    batch.ensure_edge_by_triple(EId(50), VId(1), VId(2), vec![]);
                    let limited = token.attenuate(Restriction::MaxWork(middle)).unwrap();
                    match db
                        .write_authorized(&txn, &cx, &authority, &limited, BRANCH, batch, || NOW)
                        .await
                    {
                        Ok(_) => high = middle,
                        Err(WriteTxnError::Authorization(Error::LimitExceeded(
                            LimitDimension::Work,
                        ))) => low = middle + 1,
                        Err(other) => panic!("alias={alias} hidden={hidden} {middle}: {other:?}"),
                    }
                }
                thresholds.push(low);
            }
            assert_eq!(
                thresholds[0], thresholds[1],
                "alias={alias}: MaxWork threshold moved with hidden incident edges"
            );
        }
    });
}

/// Visible vertices 1 and 2 joined by an R edge, plus one kind of data around
/// vertex 1 that a scoped capability cannot see. Variant 0 adds nothing; 1 a
/// hidden-relation edge; 2 an R edge to a hidden vertex; 3 a hidden property
/// on vertex 1; 4 a hidden label on vertex 1; 5 a hidden property on the
/// cascaded R edge.
async fn delete_fixture(cx: &fgdb_types::CommitCx, hidden: u8) -> Database<fgdb::MemVfs> {
    let mut db = Database::open_memory(cx, keys()).await.unwrap();
    let mut batch = WriteBatch::new(R);
    let labels = if hidden == 4 {
        vec![L, HIDDEN]
    } else {
        vec![L]
    };
    let secret = |variant| {
        if hidden == variant {
            vec![(SECRET, CanonicalScalar::Int(9))]
        } else {
            vec![]
        }
    };
    batch.create_vertex(VId(1), labels, secret(3));
    batch.create_vertex(VId(2), vec![L], vec![]);
    batch.add_edge(EId(10), VId(1), VId(2), secret(5));
    if hidden == 2 {
        batch.create_vertex(VId(3), vec![HIDDEN], vec![]);
        batch.add_edge(EId(11), VId(1), VId(3), vec![]);
    }
    db.write(cx, batch).await.unwrap();
    if hidden == 1 {
        let mut other = WriteBatch::new(RelationId(2));
        other.add_edge(EId(20), VId(2), VId(1), vec![]);
        db.write(cx, other).await.unwrap();
    }
    db
}

/// FG-INV-20 on the write path (fgdb-4iiho item 2, owner ruling 2026-09-25).
/// Deleting a visible vertex must not reveal whether it has hidden incidence
/// (a hidden-relation edge, an edge to a hidden vertex) or hidden fields (a
/// hidden label or property on it or on a cascaded edge). Each capability has
/// ONE outcome across every database variant: one that could hide anything is
/// refused in all of them, and one that hides nothing deletes the vertex and
/// its whole incidence in all of them.
#[test]
fn vertex_delete_outcome_depends_only_on_the_capability() {
    use fgdb_warden::Restriction;
    under_lab(0xa9a2, |contexts| async move {
        let cx = contexts.commit();
        let txn = contexts.txn();
        let authority = issuer(NAMESPACE);
        let total = authority.issue_at(&total_grant(), NOW).unwrap();
        let narrow = |restriction| total.attenuate(restriction).unwrap();
        let scoped = [
            (
                "relations",
                narrow(Restriction::Relations(Scope::only([R]))),
            ),
            ("labels", narrow(Restriction::Labels(Scope::only([L])))),
            (
                "properties",
                narrow(Restriction::Properties(Scope::only([P]))),
            ),
            (
                "denied",
                narrow(Restriction::DenyProperties([SECRET].into_iter().collect())),
            ),
            ("grant", authority.issue_at(&grant(), NOW).unwrap()),
        ];
        let baseline = txn.outstanding_obligations();
        for (name, token) in &scoped {
            for hidden in 0..=5 {
                let mut db = delete_fixture(&cx, hidden).await;
                let frontier = db.frontier().unwrap();
                let mut batch = WriteBatch::new(R);
                batch.delete_vertex(VId(1));
                let outcome = db
                    .write_authorized(&txn, &cx, &authority, token, BRANCH, batch, || NOW)
                    .await;
                assert!(
                    matches!(
                        outcome,
                        Err(WriteTxnError::Authorization(Error::ScopeDenied))
                    ),
                    "{name} capability, hidden variant {hidden}: {outcome:?}"
                );
                assert_eq!(db.frontier().unwrap(), frontier);
                assert!(db.vertex(VId(1)).unwrap().is_some());
                assert_eq!(txn.outstanding_obligations(), baseline);
            }
        }
        for hidden in 0..=5 {
            let mut db = delete_fixture(&cx, hidden).await;
            let mut batch = WriteBatch::new(R);
            batch.delete_vertex(VId(1));
            let outcome = db
                .write_authorized(&txn, &cx, &authority, &total, BRANCH, batch, || NOW)
                .await;
            assert!(
                outcome.is_ok(),
                "total capability, variant {hidden}: {outcome:?}"
            );
            let seq = outcome.unwrap();
            assert!(db.vertex(VId(1)).unwrap().is_none());
            for eid in [EId(10), EId(11), EId(20)] {
                assert!(
                    db.edge_at(eid, seq).unwrap().is_none(),
                    "variant {hidden}: {eid:?} survived"
                );
            }
            assert!(db.vertex(VId(2)).unwrap().is_some());
            assert_eq!(txn.outstanding_obligations(), baseline);
        }
    });
}

/// A property-scoped delete must refuse before observing whether the edge or
/// a hidden property exists. Two work units pay execution + intent admission;
/// no node/image admission may occur, even when the edge has no properties.
#[test]
fn edge_delete_refusal_depends_on_property_authority_not_hidden_data() {
    use fgdb_warden::Restriction;
    under_lab(0xa9a3, |contexts| async move {
        let cx = contexts.commit();
        let txn = contexts.txn();
        let baseline = txn.outstanding_obligations();
        let authority = issuer(NAMESPACE);
        let root = authority.issue_at(&edge_delete_grant(), NOW).unwrap();
        let scoped = [
            (
                "allowlist",
                root.attenuate(Restriction::Properties(Scope::only([P])))
                    .unwrap(),
            ),
            (
                "empty",
                root.attenuate(Restriction::Properties(Scope::only([])))
                    .unwrap(),
            ),
            (
                "denylist",
                root.attenuate(Restriction::DenyProperties([SECRET].into_iter().collect()))
                    .unwrap(),
            ),
        ];
        for hidden in 0..=5 {
            let mut db = delete_fixture(&cx, hidden).await;
            let frontier = db.frontier().unwrap();
            let original = db.edge_at(EId(10), frontier).unwrap().unwrap().props;
            for (name, token) in &scoped {
                for eid in [EId(10), EId(999)] {
                    for if_present in [false, true] {
                        for work in [0, 1, 2, 3, 32, 4096] {
                            let limited = token
                                .attenuate(Restriction::MaxWork(work))
                                .unwrap()
                                .attenuate(Restriction::MaxNodes(0))
                                .unwrap();
                            let mut batch = WriteBatch::new(R);
                            if if_present {
                                batch.delete_edge_if_present(eid);
                            } else {
                                batch.delete_edge(eid);
                            }
                            let outcome = db
                                .write_authorized(
                                    &txn,
                                    &cx,
                                    &authority,
                                    &limited,
                                    BRANCH,
                                    batch,
                                    || NOW,
                                )
                                .await;
                            let expected = if work < 2 {
                                matches!(
                                    &outcome,
                                    Err(WriteTxnError::Authorization(Error::LimitExceeded(
                                        LimitDimension::Work
                                    )))
                                )
                            } else {
                                matches!(
                                    &outcome,
                                    Err(WriteTxnError::Authorization(Error::ScopeDenied))
                                )
                            };
                            assert!(
                                expected,
                                "{name}, hidden={hidden}, {eid:?}, if_present={if_present}, \
                                 work={work}: {outcome:?}"
                            );
                            assert_eq!(db.frontier().unwrap(), frontier);
                            assert_eq!(txn.outstanding_obligations(), baseline);
                            assert_eq!(
                                db.edge_at(EId(10), frontier).unwrap().unwrap().props,
                                original
                            );
                            assert!(db.vertex(VId(1)).unwrap().is_some());
                            assert!(db.vertex(VId(2)).unwrap().is_some());
                        }
                    }
                }
                // An allowed, native-prepared prefix must not escape when the
                // later whole-edge delete is refused by this capability gate.
                let mut batch = WriteBatch::new(R);
                batch.create_vertex(VId(50), vec![L], vec![]);
                batch.delete_edge(EId(10));
                assert!(matches!(
                    db.write_authorized(&txn, &cx, &authority, token, BRANCH, batch, || NOW)
                        .await,
                    Err(WriteTxnError::Authorization(Error::ScopeDenied))
                ));
                assert_eq!(db.frontier().unwrap(), frontier);
                assert!(db.vertex(VId(50)).unwrap().is_none());
                assert_eq!(txn.outstanding_obligations(), baseline);
            }
        }
    });
}

#[test]
fn full_property_authority_still_deletes_scoped_edges_and_reopens() {
    under_lab(0xa9a4, |contexts| async move {
        let cx = contexts.commit();
        let txn = contexts.txn();
        let baseline = txn.outstanding_obligations();
        let path = scratch("scoped-edge-delete-reopen");
        let mut db = Database::create(&cx, &path, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(
            VId(1),
            vec![L, HIDDEN],
            vec![(SECRET, CanonicalScalar::Int(99))],
        );
        seed.create_vertex(VId(2), vec![L], vec![]);
        seed.create_vertex(VId(3), vec![HIDDEN], vec![]);
        seed.add_edge(
            EId(10),
            VId(1),
            VId(2),
            vec![(SECRET, CanonicalScalar::Int(88))],
        );
        seed.add_edge(EId(11), VId(1), VId(1), vec![(P, CanonicalScalar::Int(7))]);
        seed.add_edge(EId(30), VId(1), VId(3), vec![]);
        db.write(&cx, seed).await.unwrap();
        let mut hidden = WriteBatch::new(RelationId(2));
        hidden.add_edge(EId(20), VId(1), VId(2), vec![]);
        db.write(&cx, hidden).await.unwrap();
        let frontier = db.frontier().unwrap();
        let authority = issuer(NAMESPACE);
        // This token cannot see label HIDDEN or relation 2. Do not replace it
        // with total_grant: that would miss an over-broad deletion gate.
        let token = authority.issue_at(&edge_delete_grant(), NOW).unwrap();
        let mut batch = WriteBatch::new(R);
        batch.delete_edge(EId(10));
        batch.delete_edge_if_present(EId(11));
        let seq = db
            .write_authorized(&txn, &cx, &authority, &token, BRANCH, batch, || NOW)
            .await
            .unwrap();
        assert_eq!(seq.0, frontier.0 + 1);
        assert_eq!(txn.outstanding_obligations(), baseline);
        for eid in [EId(10), EId(11)] {
            assert!(db.edge_at(eid, seq).unwrap().is_none());
        }
        for eid in [EId(20), EId(30)] {
            assert!(db.edge_at(eid, seq).unwrap().is_some());
        }
        drop(db);
        let reopened = Database::open_rebuilding(&cx, &path, keys()).await.unwrap();
        assert_eq!(reopened.frontier().unwrap(), seq);
        for eid in [EId(10), EId(11)] {
            assert!(reopened.edge_at(eid, seq).unwrap().is_none());
        }
        for eid in [EId(20), EId(30)] {
            assert!(reopened.edge_at(eid, seq).unwrap().is_some());
        }
        let endpoint = reopened.vertex(VId(1)).unwrap().unwrap();
        assert_eq!(endpoint.labels, vec![L, HIDDEN]);
        assert_eq!(endpoint.props, vec![(SECRET, CanonicalScalar::Int(99))]);
        assert!(reopened.vertex(VId(2)).unwrap().is_some());
        assert!(reopened.vertex(VId(3)).unwrap().is_some());
    });
}
