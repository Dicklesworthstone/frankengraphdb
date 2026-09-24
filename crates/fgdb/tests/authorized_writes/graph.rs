use super::*;
use fgdb_types::EId;

#[test]
fn mixed_graph_batch_uses_native_ensure_aliases_and_reopens() {
    under_lab(0xa911, |contexts| async move {
        let cx = contexts.commit();
        let txn = contexts.txn();
        let path = scratch("graph-reopen");
        let mut db = Database::create(&cx, &path, keys()).await.unwrap();
        let authority = issuer(NAMESPACE);
        let token = authority.issue_at(&grant(), NOW).unwrap();
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
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let baseline = txn.outstanding_obligations();
        let mut batch = WriteBatch::new(R);
        for vid in [VId(1), VId(2), VId(3)] {
            batch.create_vertex(vid, vec![L], vec![]);
        }
        batch.add_edge(EId(10), VId(1), VId(2), vec![]);
        batch.add_edge(EId(11), VId(2), VId(3), vec![(P, CanonicalScalar::Int(1))]);
        batch.add_edge(EId(12), VId(2), VId(2), vec![]);
        batch.add_edge(EId(13), VId(1), VId(3), vec![]);
        batch.delete_vertex(VId(2));
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
        let mut all = grant();
        all.relations = Scope::All;
        let token = authority.issue_at(&all, NOW).unwrap();
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
        let token = authority.issue_at(&grant(), NOW).unwrap();
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
