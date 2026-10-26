use super::*;
use fgdb_types::EId;

/// Edge deletion needs every property, but not every relation or endpoint label.
fn edge_delete_grant() -> Grant {
    Grant {
        properties: Scope::All,
        ..grant()
    }
}

/// The trusted host seeds three visible vertices and edges 10 (1->2, P=1),
/// 11 (1->1) and 12 (2->3): authorized batches address existing identities
/// only (fgdb-hxgm1).
async fn seed_triangle<V: asupersync::fs::Vfs + Clone>(
    db: &mut Database<V>,
    cx: &fgdb_types::CommitCx,
) {
    let mut seed = WriteBatch::new(R);
    for vid in [VId(1), VId(2), VId(3)] {
        seed.create_vertex(vid, vec![L], vec![]);
    }
    seed.add_edge(EId(10), VId(1), VId(2), vec![(P, CanonicalScalar::Int(1))]);
    seed.add_edge(EId(11), VId(1), VId(1), vec![]);
    seed.add_edge(EId(12), VId(2), VId(3), vec![]);
    db.write(cx, seed).await.unwrap();
}

#[test]
fn mixed_graph_batch_updates_and_deletes_edges_then_reopens() {
    under_lab(0xa911, |contexts| async move {
        let cx = contexts.commit();
        let txn = contexts.txn();
        let path = scratch("graph-reopen");
        let mut db = Database::create(&cx, &path, keys()).await.unwrap();
        seed_triangle(&mut db, &cx).await;
        let authority = issuer(NAMESPACE);
        let token = authority.issue_at(&edge_delete_grant(), NOW).unwrap();
        let baseline = txn.outstanding_obligations();
        let frontier = db.frontier().unwrap();
        let mut batch = WriteBatch::new(R);
        batch.set_edge_property(EId(10), P, Some(CanonicalScalar::Int(7)));
        batch.delete_edge(EId(12));
        let seq = db
            .write_authorized(&txn, &cx, &authority, &token, BRANCH, batch, || NOW)
            .await
            .unwrap();
        assert_eq!(seq.0, frontier.0 + 1);
        assert_eq!(txn.outstanding_obligations(), baseline);
        assert_eq!(
            db.edge_at(EId(10), seq).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(7).into())]
        );
        assert!(db.edge_at(EId(12), seq).unwrap().is_none());
        assert_eq!(db.neighbours(VId(1), R).unwrap(), vec![VId(1), VId(2)]);
        drop(db);
        let reopened = Database::open_rebuilding(&cx, &path, keys()).await.unwrap();
        assert_eq!(reopened.frontier().unwrap(), seq);
        assert_eq!(
            reopened.edge_at(EId(10), seq).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(7).into())]
        );
        assert!(reopened.edge_at(EId(12), seq).unwrap().is_none());
        assert_eq!(
            reopened.neighbours(VId(1), R).unwrap(),
            vec![VId(1), VId(2)]
        );
    });
}

#[test]
fn vertex_delete_authorizes_and_removes_all_incidence() {
    under_lab(0xa912, |contexts| async move {
        let cx = contexts.commit();
        let txn = contexts.txn();
        let mut db = Database::create(&cx, &scratch("prefix-cascade"), keys())
            .await
            .unwrap();
        let mut seed = WriteBatch::new(R);
        for vid in [VId(1), VId(2), VId(3)] {
            seed.create_vertex(vid, vec![L], vec![]);
        }
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        seed.add_edge(EId(11), VId(2), VId(3), vec![(P, CanonicalScalar::Int(1))]);
        seed.add_edge(EId(12), VId(2), VId(2), vec![]);
        seed.add_edge(EId(13), VId(1), VId(3), vec![]);
        db.write(&cx, seed).await.unwrap();
        let authority = issuer(NAMESPACE);
        let token = authority.issue_at(&total_grant(), NOW).unwrap();
        let baseline = txn.outstanding_obligations();
        let batch = || {
            let mut batch = WriteBatch::new(R);
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
        // Re-creating the deleted identity is a chosen-identity creation:
        // refused before observation, nothing published (fgdb-hxgm1).
        let mut recreate = WriteBatch::new(R);
        recreate.create_vertex(VId(2), vec![L], vec![]);
        assert!(matches!(
            db.write_authorized(&txn, &cx, &authority, &token, BRANCH, recreate, || NOW)
                .await,
            Err(WriteTxnError::AuthorizedClientIdentity)
        ));
        assert_eq!(db.frontier().unwrap(), seq);
        assert!(db.vertex(VId(2)).unwrap().is_none());
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
        // An allowed prefix that the refused cascade must discard with it.
        delete.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(5)));
        delete.delete_vertex(VId(1));
        assert!(matches!(
            db.write_authorized(&txn, &cx, &authority, &token, BRANCH, delete, || NOW)
                .await,
            Err(WriteTxnError::Authorization(Error::ScopeDenied))
        ));
        assert_eq!(db.frontier().unwrap(), frontier);
        assert!(db.vertex(VId(2)).unwrap().unwrap().props.is_empty());
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
fn hidden_endpoint_blocks_edge_update_and_cascade_and_create_cannot_probe_it() {
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
        for (batch, creates) in [(create, true), (update, false), (delete, false)] {
            let outcome = db
                .write_authorized(&txn, &cx, &authority, &token, BRANCH, batch, || NOW)
                .await;
            if creates {
                // A chosen-identity create refuses before reading either
                // endpoint, so the hidden one is not probed (fgdb-hxgm1).
                assert!(matches!(
                    outcome,
                    Err(WriteTxnError::AuthorizedClientIdentity)
                ));
            } else {
                assert!(matches!(
                    outcome,
                    Err(WriteTxnError::Authorization(Error::ScopeDenied))
                ));
            }
            assert_eq!(db.frontier().unwrap(), frontier);
            assert_eq!(txn.outstanding_obligations(), baseline);
            assert!(db.vertex(VId(1)).unwrap().is_some());
            assert!(db.edge_at(EId(20), frontier).unwrap().is_none());
            assert_eq!(
                db.edge_at(EId(10), frontier).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(1).into())]
            );
        }
    });
}

#[test]
fn an_ensure_refuses_before_it_could_capture_an_unrelated_hidden_edge() {
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
        // Even a capability that hides nothing cannot stage an ensure: it
        // names a chosen identity (EId 99, an unrelated edge here), so the
        // whole batch refuses before any read (fgdb-hxgm1).
        let token = authority.issue_at(&total_grant(), NOW).unwrap();
        let seq = db.frontier().unwrap();
        let mut batch = WriteBatch::new(R);
        batch.ensure_edge_by_triple(EId(99), VId(1), VId(2), vec![(P, CanonicalScalar::Int(55))]);
        batch.delete_vertex(VId(1));
        assert!(matches!(
            db.write_authorized(&txn, &cx, &authority, &token, BRANCH, batch, || NOW)
                .await,
            Err(WriteTxnError::AuthorizedClientIdentity)
        ));
        assert_eq!(db.frontier().unwrap(), seq);
        assert!(db.vertex(VId(1)).unwrap().is_some());
        assert!(db.edge_at(EId(10), seq).unwrap().is_some());
        let untouched = db.edge_at(EId(99), seq).unwrap().unwrap();
        assert_eq!(untouched.entry.src, VId(3));
        assert_eq!(untouched.entry.dst, VId(4));
        assert_eq!(untouched.entry.relation, RelationId(2));
        assert_eq!(
            untouched.props,
            vec![(SECRET, CanonicalScalar::Int(99).into())]
        );
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
                (P, CanonicalScalar::Int(7).into()),
                (SECRET, CanonicalScalar::Int(88).into())
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

/// FG-INV-20 on the write path (fgdb-4iiho). Ensuring an edge used to scan
/// the source vertex's live incidence for an alias, so its MaxWork threshold
/// could count hidden incident edges. An ensure names a chosen identity and
/// now refuses before any read (fgdb-hxgm1), so its outcome at every budget
/// must be the same whatever the hidden degree and whether an alias exists:
/// below the entry charge, exactly at it, and at the full grant.
#[test]
fn ensure_edge_outcome_cannot_count_hidden_incident_edges() {
    use fgdb_warden::Restriction;
    under_lab(0xa9a1, |contexts| async move {
        let cx = contexts.commit();
        let txn = contexts.txn();
        let authority = issuer(NAMESPACE);
        let token = authority.issue_at(&grant(), NOW).unwrap();
        for budget in [Some(0_u64), Some(1), None] {
            let mut outcomes = Vec::new();
            for alias in [false, true] {
                for hidden in [false, true] {
                    let mut db = ensure_fixture(&cx, hidden, alias).await;
                    let frontier = db.frontier().unwrap();
                    let mut batch = WriteBatch::new(R);
                    batch.ensure_edge_by_triple(EId(50), VId(1), VId(2), vec![]);
                    let attenuated;
                    let limited = match budget {
                        Some(work) => {
                            attenuated = token.attenuate(Restriction::MaxWork(work)).unwrap();
                            &attenuated
                        }
                        None => &token,
                    };
                    let outcome = db
                        .write_authorized(&txn, &cx, &authority, limited, BRANCH, batch, || NOW)
                        .await;
                    assert_eq!(db.frontier().unwrap(), frontier);
                    outcomes.push(format!("{outcome:?}"));
                }
            }
            assert!(
                outcomes.iter().all(|outcome| outcome == &outcomes[0]),
                "budget {budget:?}: the outcome moved with hidden incidence: {outcomes:?}"
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

/// FG-INV-20 accounting on the create and update write paths (fgdb-4iiho
/// item c). A scoped writer touching a VISIBLE element must not be able to
/// learn, from its outcome or its MaxWork threshold, what it cannot see
/// around that element: a hidden-relation edge, an edge to a hidden vertex, a
/// hidden label, or a hidden property on the vertex or on its edge (the six
/// delete_fixture variants). Each operation has ONE outcome across all six,
/// and when admitted, ONE exact work threshold.
#[test]
fn update_thresholds_and_create_refusals_ignore_hidden_data() {
    use fgdb_warden::Restriction;
    const CEILING: u64 = 100_000;
    /// A named single-write batch over the fixture's visible elements.
    type Operation = (&'static str, fn() -> WriteBatch);
    under_lab(0xa9a2, |contexts| async move {
        let cx = contexts.commit();
        let txn = contexts.txn();
        let authority = issuer(NAMESPACE);
        let token = authority.issue_at(&grant(), NOW).unwrap();
        // Creation's threshold law now lives with the allocating insertion
        // surface (insert_tests::matched_creation_work_threshold_ignores_
        // hidden_data); a chosen-identity create refuses uniformly, below.
        let operations: [Operation; 2] = [
            ("vertex property update", || {
                let mut batch = WriteBatch::new(R);
                batch.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(5)));
                batch
            }),
            ("edge property update", || {
                let mut batch = WriteBatch::new(R);
                batch.set_edge_property(EId(10), P, Some(CanonicalScalar::Int(5)));
                batch
            }),
        ];
        let mut creates = Vec::new();
        for variant in 0..=5_u8 {
            let mut db = delete_fixture(&cx, variant).await;
            let mut batch = WriteBatch::new(R);
            batch.add_edge(EId(50), VId(1), VId(2), vec![]);
            let outcome = db
                .write_authorized(&txn, &cx, &authority, &token, BRANCH, batch, || NOW)
                .await;
            assert!(
                matches!(outcome, Err(WriteTxnError::AuthorizedClientIdentity)),
                "variant {variant}: {outcome:?}"
            );
            creates.push(format!("{outcome:?}"));
        }
        assert!(creates.iter().all(|outcome| outcome == &creates[0]));
        for (name, operation) in operations {
            let mut outcomes = Vec::new();
            for variant in 0..=5_u8 {
                // The outcome at the full grant budget must not move either.
                let mut db = delete_fixture(&cx, variant).await;
                let admitted = db
                    .write_authorized(&txn, &cx, &authority, &token, BRANCH, operation(), || NOW)
                    .await;
                let outcome = match admitted {
                    Ok(_) => {
                        let (mut low, mut high) = (0_u64, CEILING);
                        while low < high {
                            let middle = low + (high - low) / 2;
                            let mut db = delete_fixture(&cx, variant).await;
                            let limited = token.attenuate(Restriction::MaxWork(middle)).unwrap();
                            match db
                                .write_authorized(
                                    &txn,
                                    &cx,
                                    &authority,
                                    &limited,
                                    BRANCH,
                                    operation(),
                                    || NOW,
                                )
                                .await
                            {
                                Ok(_) => high = middle,
                                Err(WriteTxnError::Authorization(Error::LimitExceeded(
                                    LimitDimension::Work,
                                ))) => low = middle + 1,
                                Err(other) => {
                                    panic!("{name} variant {variant} at {middle}: {other:?}")
                                }
                            }
                        }
                        assert!(
                            low < CEILING,
                            "{name}: no threshold below the search ceiling"
                        );
                        format!("admitted at work {low}")
                    }
                    Err(error) => format!("refused: {error:?}"),
                };
                outcomes.push(outcome);
            }
            assert!(
                outcomes.iter().all(|outcome| outcome == &outcomes[0]),
                "{name}: outcome moved with hidden data: {outcomes:?}"
            );
            // Every target is visible to this grant, so a uniform refusal would
            // satisfy the equality vacuously: require admission.
            assert!(outcomes[0].starts_with("admitted"), "{name}: {outcomes:?}");
        }
    });
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
                // (A property write, except under the empty property scope,
                // where only a label write is allowed.)
                let mut batch = WriteBatch::new(R);
                if *name == "empty" {
                    batch.set_vertex_label(VId(2), L, true);
                } else {
                    batch.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(5)));
                }
                batch.delete_edge(EId(10));
                assert!(matches!(
                    db.write_authorized(&txn, &cx, &authority, token, BRANCH, batch, || NOW)
                        .await,
                    Err(WriteTxnError::Authorization(Error::ScopeDenied))
                ));
                assert_eq!(db.frontier().unwrap(), frontier);
                assert!(db.vertex(VId(2)).unwrap().unwrap().props.is_empty());
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
        assert_eq!(
            endpoint.props,
            vec![(SECRET, CanonicalScalar::Int(99).into())]
        );
        assert!(reopened.vertex(VId(2)).unwrap().is_some());
        assert!(reopened.vertex(VId(3)).unwrap().is_some());
    });
}

mod query_deletion {
    use super::*;
    use fgdb_gql::{
        GlaLimitDimension, GqlParameters, GqlQueryError, GqlQueryPolicy, GraphDeleteError,
        GraphDeletePolicy, GraphDeleteStats, GraphMutationPolicy, GraphSymbol, GraphSymbolKind,
        PreparedGraphDeleteText, PreparedGraphMutationQueryText,
    };

    type DeleteFault = GqlQueryError<GraphDeleteError<WriteTxnError>, WriteTxnError>;

    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Label, "Visible") => Some(GraphSymbol::Label(L)),
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
            _ => None,
        }
    }

    fn read_grant() -> Grant {
        Grant {
            rights: Rights::ReadWrite,
            ..total_grant()
        }
    }

    fn authorization(error: &(dyn std::error::Error + 'static)) -> Option<Error> {
        let mut cause = Some(error);
        while let Some(error) = cause {
            if let Some(WriteTxnError::Authorization(error)) = error.downcast_ref::<WriteTxnError>()
            {
                return Some(*error);
            }
            cause = error.source();
        }
        None
    }

    async fn fixture(
        cx: &fgdb_types::CommitCx,
        unrelated: u128,
        incident: bool,
    ) -> Database<fgdb::MemVfs> {
        let mut db = Database::open_memory(cx, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for id in 1..=3 {
            seed.create_vertex(VId(id), vec![L], vec![(P, CanonicalScalar::Int(id as i64))]);
        }
        if incident {
            for id in [10, 11] {
                seed.add_edge(
                    EId(id),
                    VId(1),
                    VId(2),
                    vec![(P, CanonicalScalar::Int(id as i64))],
                );
            }
        }
        for id in 0..unrelated {
            seed.add_edge(
                EId(1_000 + id),
                VId(2),
                VId(3),
                vec![(P, CanonicalScalar::Int(id as i64))],
            );
        }
        db.write(cx, seed).await.unwrap();
        db
    }

    async fn isolated(
        contexts: &PurposeContexts,
        unrelated: u128,
        mut capability: Grant,
        records: u64,
    ) -> Result<GraphDeleteStats, DeleteFault> {
        let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = fixture(&commit, unrelated, false).await;
        let before = (
            db.frontier().unwrap(),
            db.vertices().unwrap(),
            db.edges().unwrap(),
        );
        capability.limits.max_rows = 0;
        let authority = issuer(NAMESPACE);
        let token = authority.issue_at(&capability, NOW).unwrap();
        let statement =
            PreparedGraphDeleteText::prepare("MATCH (a:Visible {p:1}) DELETE a", R, symbols)
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap();
        let result = db
            .execute_graph_delete_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                BRANCH,
                &statement,
                GraphDeletePolicy::new(GqlQueryPolicy::new(records, 1, 1_000_000, 100_000), 1),
                || NOW,
            )
            .await;
        let result = match result {
            Ok((stats, _)) => {
                assert_eq!((stats.target_vertices, stats.target_edges), (1, 0));
                assert!(db.vertex(VId(1)).unwrap().is_none());
                assert_eq!(db.vertices().unwrap(), before.1[1..]);
                assert_eq!(
                    db.edges().unwrap(),
                    before.2,
                    "unrelated payloads must survive"
                );
                assert_eq!(db.frontier().unwrap().0, before.0.0 + 1);
                Ok(stats)
            }
            Err(error) => {
                assert_eq!(
                    (
                        db.frontier().unwrap(),
                        db.vertices().unwrap(),
                        db.edges().unwrap()
                    ),
                    before
                );
                Err(error)
            }
        };
        assert_eq!(txn.outstanding_obligations(), 0);
        result
    }

    #[test]
    fn isolated_delete_reads_no_unrelated_edge_rows_and_keeps_one_signed_allowance() {
        under_lab(0xa9b1, |contexts| async move {
            let empty = isolated(&contexts, 0, read_grant(), 10_000).await.unwrap();
            assert!(empty.selection.snapshot_records > 0);
            // Exact matching input allowance from the edge-free control. Any
            // whole-graph incidence scan would exceed it on these two graphs.
            let records = empty.selection.snapshot_records;
            let mut expected = None;
            for unrelated in [8, 128] {
                let stats = isolated(&contexts, unrelated, read_grant(), records)
                    .await
                    .unwrap();
                assert_eq!(stats.selection.snapshot_records, records);
                expected = Some(stats);
            }
            let expected = expected.unwrap();
            let (mut refused, mut admitted) = (0, read_grant().limits.max_work);
            while admitted - refused > 1 {
                let trial = refused + (admitted - refused) / 2;
                let mut capability = read_grant();
                capability.limits.max_work = trial;
                match isolated(&contexts, 128, capability, records).await {
                    Ok(stats) => {
                        assert_eq!(stats, expected);
                        admitted = trial;
                    }
                    Err(error) => {
                        assert_eq!(
                            authorization(&error),
                            Some(Error::LimitExceeded(LimitDimension::Work)),
                            "{error:?}"
                        );
                        refused = trial;
                    }
                }
            }
            for (limit, succeeds) in [(admitted - 1, false), (admitted, true)] {
                let mut capability = read_grant();
                capability.limits.max_work = limit;
                let result = isolated(&contexts, 128, capability, records).await;
                if succeeds {
                    assert_eq!(result.unwrap(), expected);
                } else {
                    assert_eq!(
                        authorization(&result.unwrap_err()),
                        Some(Error::LimitExceeded(LimitDimension::Work))
                    );
                }
            }
        });
    }

    #[test]
    fn standalone_delete_projection_and_incidence_share_native_limits_before_commit() {
        under_lab(0xa9b2, |contexts| async move {
            let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
            let authority = issuer(NAMESPACE);
            let token = authority.issue_at(&read_grant(), NOW).unwrap();
            let statement = PreparedGraphMutationQueryText::prepare(
                "MATCH (a:Visible {p:1})-[r:R]->(b:Visible) DELETE a,r \
                 RETURN size(range(1,32)) AS size LIMIT 0",
                R,
                symbols,
            )
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
            let mut measured: Option<[u64; 3]> = None;
            let mut expected = None;
            for attempt in 0..5 {
                let mut db = fixture(&commit, 8, true).await;
                let before = (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap(),
                );
                let mut policy =
                    GraphMutationPolicy::new(GqlQueryPolicy::new(10_000, 0, 1_000_000, 100_000), 3);
                if let Some([records, work, scratch]) = measured {
                    policy.query = GqlQueryPolicy::new(
                        records - u64::from(attempt == 4),
                        0,
                        work - u64::from(attempt == 2),
                        scratch - u64::from(attempt == 3),
                    );
                }
                let result = db
                    .execute_graph_mutation_query_authorized(
                        &txn,
                        &query,
                        &commit,
                        &authority,
                        &token,
                        BRANCH,
                        &statement,
                        policy,
                        || NOW,
                    )
                    .await;
                if attempt < 2 {
                    let (stats, rows, _) = result.unwrap();
                    assert_eq!(stats.effects, 3);
                    assert!(rows.value.is_empty());
                    assert!(rows.evaluator.work_units > stats.evaluator.work_units);
                    let usage = [
                        rows.rows.snapshot_records,
                        rows.evaluator.work_units,
                        rows.evaluator.scratch_entries,
                    ];
                    let graph = (db.vertices().unwrap(), db.edges().unwrap());
                    assert_eq!(graph.0, before.1[1..]);
                    assert_eq!(graph.1, before.2[2..]);
                    assert_eq!(db.frontier().unwrap().0, before.0.0 + 1);
                    if let Some(prior) = measured {
                        assert_eq!(usage, prior);
                        assert_eq!(Some(&(stats, rows, graph)), expected.as_ref());
                    } else {
                        measured = Some(usage);
                        expected = Some((stats, rows, graph));
                    }
                } else {
                    let error = result.unwrap_err();
                    match attempt {
                        2 => assert!(
                            matches!(error, GqlQueryError::Evaluator(limit)
                            if limit.dimension == GlaLimitDimension::WorkUnits),
                            "{error:?}"
                        ),
                        3 => assert!(
                            matches!(error, GqlQueryError::Evaluator(limit)
                            if limit.dimension == GlaLimitDimension::ScratchEntries),
                            "{error:?}"
                        ),
                        4 => assert!(matches!(error, GqlQueryError::Rows(_)), "{error:?}"),
                        _ => unreachable!(),
                    }
                    assert_eq!(
                        (
                            db.frontier().unwrap(),
                            db.vertices().unwrap(),
                            db.edges().unwrap()
                        ),
                        before
                    );
                }
                assert_eq!(txn.outstanding_obligations(), 0);
            }
        });
    }

    #[test]
    fn indexed_proof_keeps_parallel_incoming_self_loop_and_cross_relation_incidence() {
        under_lab(0xa9b3, |contexts| async move {
            let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
            let mut db = fixture(&commit, 1, true).await;
            let mut loops = WriteBatch::new(R);
            loops.add_edge(EId(12), VId(1), VId(1), vec![]);
            db.write(&commit, loops).await.unwrap();
            let mut incoming = WriteBatch::new(RelationId(2));
            incoming.add_edge(EId(13), VId(3), VId(1), vec![]);
            db.write(&commit, incoming).await.unwrap();
            let before = (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap(),
            );
            let authority = issuer(NAMESPACE);
            let token = authority.issue_at(&read_grant(), NOW).unwrap();
            for (pattern, succeeds) in [("[r:R]", false), ("[r]", true)] {
                let deletion = PreparedGraphDeleteText::prepare(
                    &format!("MATCH (a:Visible {{p:1}})-{pattern}-(b) DELETE a,r"),
                    R,
                    symbols,
                )
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap();
                let result = db
                    .execute_graph_delete_authorized(
                        &txn,
                        &query,
                        &commit,
                        &authority,
                        &token,
                        BRANCH,
                        &deletion,
                        GraphDeletePolicy::new(GqlQueryPolicy::new(100, 100, 100_000, 10_000), 5),
                        || NOW,
                    )
                    .await;
                if succeeds {
                    let (stats, _) = result.unwrap();
                    assert_eq!((stats.target_vertices, stats.target_edges), (1, 4));
                    assert!(db.vertex(VId(1)).unwrap().is_none());
                    assert_eq!(db.vertices().unwrap(), before.1[1..]);
                    assert_eq!(db.edges().unwrap(), before.2[4..]);
                } else {
                    assert!(matches!(
                        result.unwrap_err(),
                        GqlQueryError::Source(GraphDeleteError::IncidentRelationships)
                    ));
                    assert_eq!(
                        (
                            db.frontier().unwrap(),
                            db.vertices().unwrap(),
                            db.edges().unwrap()
                        ),
                        before
                    );
                }
                assert_eq!(txn.outstanding_obligations(), 0);
            }
        });
    }

    #[test]
    fn restricted_delete_still_refuses_before_observing_hidden_incidence() {
        under_lab(0xa9b4, |contexts| async move {
            let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
            let authority = issuer(NAMESPACE);
            let capability = Grant {
                relations: Scope::only([R]),
                ..read_grant()
            };
            let token = authority.issue_at(&capability, NOW).unwrap();
            let statement =
                PreparedGraphDeleteText::prepare("MATCH (a:Visible {p:1}) DELETE a", R, symbols)
                    .unwrap()
                    .bind_parameters(&GqlParameters::new())
                    .unwrap();
            for hidden in [false, true] {
                let mut db = fixture(&commit, 8, false).await;
                if hidden {
                    let mut extra = WriteBatch::new(RelationId(2));
                    extra.add_edge(
                        EId(10),
                        VId(3),
                        VId(1),
                        vec![(SECRET, CanonicalScalar::Int(99))],
                    );
                    db.write(&commit, extra).await.unwrap();
                }
                let before = (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap(),
                );
                let error = db
                    .execute_graph_delete_authorized(
                        &txn,
                        &query,
                        &commit,
                        &authority,
                        &token,
                        BRANCH,
                        &statement,
                        GraphDeletePolicy::new(GqlQueryPolicy::new(3, 1, 100_000, 10_000), 1),
                        || NOW,
                    )
                    .await
                    .unwrap_err();
                assert_eq!(authorization(&error), Some(Error::ScopeDenied));
                assert_eq!(
                    (
                        db.frontier().unwrap(),
                        db.vertices().unwrap(),
                        db.edges().unwrap()
                    ),
                    before
                );
                assert_eq!(txn.outstanding_obligations(), 0);
            }
        });
    }
}
