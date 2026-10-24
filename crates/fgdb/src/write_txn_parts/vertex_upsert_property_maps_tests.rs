use super::*;
use crate::{DatabaseKeys, MemVfs};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::PropertyKeyId;
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GlaExecutionEvent, GqlQueryError, GraphVertexUpsertError, ReverseSymbolCatalog};
use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const NEW: PropertyKeyId = PropertyKeyId(3);
const HIDDEN: PropertyKeyId = PropertyKeyId(99);
type Fault = GqlQueryError<GraphVertexUpsertError<WriteTxnError, ()>, ()>;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xb7; 32],
        DatabaseSecurityNamespaceId([0xb8; 32]),
        [0xb9; 32],
    )
}
fn catalog() -> ReverseSymbolCatalog {
    let mut catalog = ReverseSymbolCatalog::new();
    catalog.insert_property(P, "zeta");
    catalog.insert_property(Q, "alpha");
    catalog.insert_property(NEW, "new");
    catalog
}
fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn map(values: &[(&str, GraphValue)]) -> GraphValue {
    GraphValue::map(
        values
            .iter()
            .map(|(key, value)| ((*key).into(), value.clone()))
            .collect(),
    )
    .unwrap()
}
fn read(
    transaction: &WriteTxn,
    database: &Database<MemVfs>,
    vertex: VId,
    visible: impl FnMut(PropertyKeyId) -> bool,
    stop: usize,
) -> (Result<GraphValue, Fault>, Vec<GlaExecutionEvent>) {
    let mut events = Vec::new();
    let value = vertex_upsert_properties(
        transaction,
        database,
        vertex,
        Some(&catalog()),
        visible,
        || Ok(()),
        &mut |event| {
            events.push(event);
            if events.len() == stop {
                Err(GqlQueryError::Interrupted(()))
            } else {
                Ok(())
            }
        },
    );
    (value, events)
}
fn read_conflict<T>(result: Result<T, WriteTxnError>) {
    assert!(matches!(
        result,
        Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
            law: "FG-LAW-FCW-READ-01",
            ..
        }))
    ));
}

#[test]
fn merge_property_map_masks_borrowed_history_and_hidden_staged_edits_before_charging() {
    let ((), report) = run_async_under_lab(0xb76d_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, txcx) = (contexts.commit(), contexts.txn());
        let mut observations = Vec::new();
        for hidden in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut fields = vec![(P, CanonicalScalar::Int(1)), (Q, CanonicalScalar::Null)];
            if hidden {
                fields.push((HIDDEN, CanonicalScalar::bytes(vec![0x71; 2_048]).unwrap()));
            }
            let mut seed = WriteBatch::new(R);
            seed.create_vertex(VId(1), vec![LabelId(1)], fields);
            db.write(&commit, seed).await.unwrap();
            let mut transaction = db.begin(&txcx).unwrap();
            let before = read(&transaction, &db, VId(1), |key| key != HIDDEN, usize::MAX);
            assert_eq!(
                before.0.as_ref().unwrap(),
                &map(&[
                    ("alpha", GraphValue::Scalar(CanonicalScalar::Null)),
                    ("zeta", int(1))
                ])
            );
            transaction.savepoint(&db, "before").unwrap();
            let mut edits = WriteBatch::new(R);
            edits.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(2)));
            edits.set_vertex_property(VId(1), Q, None);
            edits.set_vertex_property(VId(1), NEW, Some(CanonicalScalar::Int(3)));
            if hidden {
                edits.set_vertex_property(
                    VId(1),
                    HIDDEN,
                    Some(CanonicalScalar::bytes(vec![0x72; 4_096]).unwrap()),
                );
            }
            transaction.write(&mut db, edits).unwrap();
            let after = read(&transaction, &db, VId(1), |key| key != HIDDEN, usize::MAX);
            assert_eq!(
                after.0.as_ref().unwrap(),
                &map(&[("new", int(3)), ("zeta", int(2))])
            );
            // Repeated reads retain the same logical allowance, not a cache
            // discount. No hidden property's unknown name is consulted.
            let repeated = read(&transaction, &db, VId(1), |key| key != HIDDEN, usize::MAX);
            assert_eq!(repeated.0.unwrap(), *after.0.as_ref().unwrap());
            assert_eq!(repeated.1, after.1);
            if hidden {
                assert!(matches!(
                    read(&transaction, &db, VId(1), |_| true, usize::MAX).0,
                    Err(GqlQueryError::Source(GraphVertexUpsertError::Staging(
                        WriteTxnError::Read(crate::ReadError::UnmappedProperty(HIDDEN))
                    )))
                ));
            }
            transaction.rollback_to_savepoint(&db, "before").unwrap();
            assert_eq!(
                read(&transaction, &db, VId(1), |key| key != HIDDEN, usize::MAX)
                    .0
                    .unwrap(),
                *before.0.as_ref().unwrap()
            );
            observations.push((before.0.unwrap(), before.1, after.0.unwrap(), after.1));
            transaction.abort();
        }
        assert_eq!(observations[0], observations[1]);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn merge_empty_property_map_observes_absent_keys_across_refresh_and_savepoint_rollback() {
    let ((), report) = run_async_under_lab(0xb76d_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, txcx) = (contexts.commit(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![LabelId(1)], vec![]);
        db.write(&commit, seed).await.unwrap();
        let mut transaction = db.begin(&txcx).unwrap();
        let basis = transaction.basis();
        transaction.savepoint(&db, "before-map").unwrap();
        assert_eq!(
            read(&transaction, &db, VId(1), |_| true, usize::MAX)
                .0
                .unwrap(),
            map(&[])
        );
        assert!(
            transaction
                .read_set
                .borrow()
                .contains(&ElementId::Vertex(VId(1)))
        );
        assert!(transaction.point_reads.borrow().is_empty());
        transaction
            .rollback_to_savepoint(&db, "before-map")
            .unwrap();
        let mut winner = WriteBatch::new(R);
        winner.set_vertex_property(VId(1), NEW, Some(CanonicalScalar::Int(9)));
        db.write(&commit, winner).await.unwrap();
        // Until an explicit validated refresh, whole maps still read the old
        // historical row, even though the newest row has an additional key.
        assert_eq!(
            read(&transaction, &db, VId(1), |_| true, usize::MAX)
                .0
                .unwrap(),
            map(&[])
        );
        read_conflict(transaction.refresh_snapshot(&db, &txcx));
        assert_eq!(transaction.basis(), basis);
        read_conflict(transaction.finish(&mut db, &commit).await);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn merge_map_refusal_and_retry_keep_conservative_witnesses_and_the_staged_prefix() {
    let ((), report) = run_async_under_lab(0xb76d_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, txcx) = (contexts.commit(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for id in 1..=3 {
            seed.create_vertex(
                VId(id),
                vec![LabelId(1)],
                vec![(P, CanonicalScalar::bytes(vec![0x73; 512]).unwrap())],
            );
        }
        db.write(&commit, seed).await.unwrap();
        let mut transaction = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.set_vertex_property(VId(2), Q, Some(CanonicalScalar::Int(42)));
        transaction.write(&mut db, prefix).unwrap();
        transaction.savepoint(&db, "prefix").unwrap();
        let digest = transaction.staged_effect_digest().unwrap();
        let (expected, events) = read(&transaction, &db, VId(1), |_| true, usize::MAX);
        let expected = expected.unwrap();
        for stop in 1..=events.len() {
            let (result, observed) = read(&transaction, &db, VId(1), |_| true, stop);
            assert!(matches!(result, Err(GqlQueryError::Interrupted(()))));
            assert_eq!(observed.len(), stop);
            if stop > 1 {
                assert_eq!(
                    transaction.point_reads.borrow().1,
                    Some(ElementId::Vertex(VId(1)))
                );
            }
            assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
            assert_eq!(transaction.state(), EmbeddedTxnState::Active);
        }
        assert_eq!(
            read(&transaction, &db, VId(1), |_| true, usize::MAX)
                .0
                .unwrap(),
            expected
        );
        transaction.rollback_to_savepoint(&db, "prefix").unwrap();
        assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
        // The refused-read guard deliberately observes all changes. A retry
        // and rollback must not narrow that evidence to the returned fields.
        let mut unrelated = WriteBatch::new(R);
        unrelated.set_vertex_property(VId(3), NEW, Some(CanonicalScalar::Int(9)));
        db.write(&commit, unrelated).await.unwrap();
        read_conflict(transaction.finish(&mut db, &commit).await);
        assert!(
            db.vertex(VId(2))
                .unwrap()
                .unwrap()
                .props
                .iter()
                .all(|(key, _)| *key != Q)
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn merge_map_polls_hidden_and_unrelated_staged_effects_without_resource_charges() {
    let ((), report) = run_async_under_lab(0xb76d_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, txcx) = (contexts.commit(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for id in 1..=3 {
            seed.create_vertex(
                VId(id),
                vec![LabelId(1)],
                vec![(P, CanonicalScalar::Int(1))],
            );
        }
        db.write(&commit, seed).await.unwrap();
        let mut transaction = db.begin(&txcx).unwrap();
        let baseline = read(&transaction, &db, VId(1), |key| key != HIDDEN, usize::MAX);
        let mut prefix = WriteBatch::new(R);
        prefix.set_vertex_property(
            VId(1),
            HIDDEN,
            Some(CanonicalScalar::bytes(vec![0x74; 2_048]).unwrap()),
        );
        for id in 2..=3 {
            prefix.set_vertex_property(VId(id), P, Some(CanonicalScalar::Int(2)));
        }
        transaction.write(&mut db, prefix).unwrap();
        let digest = transaction.staged_effect_digest().unwrap();
        let run = |stop| {
            let mut polls = 0;
            let mut events = Vec::new();
            let result: Result<GraphValue, Fault> = vertex_upsert_properties(
                &transaction,
                &db,
                VId(1),
                Some(&catalog()),
                |key| key != HIDDEN,
                || {
                    polls += 1;
                    if polls == stop {
                        Err(GqlQueryError::Interrupted(()))
                    } else {
                        Ok(())
                    }
                },
                &mut |event| {
                    events.push(event);
                    Ok(())
                },
            );
            (result, polls, events)
        };
        let complete = run(usize::MAX);
        assert_eq!(complete.0.unwrap(), baseline.0.unwrap());
        assert_eq!(complete.2, baseline.1);
        assert!(complete.1 >= 3);
        for stop in [1, complete.1] {
            let (result, polls, _) = run(stop);
            assert!(matches!(result, Err(GqlQueryError::Interrupted(()))));
            assert_eq!(polls, stop);
            assert_eq!(
                transaction.point_reads.borrow().1,
                Some(ElementId::Vertex(VId(1)))
            );
            assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
        }
        transaction.abort();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

const PERSON: LabelId = LabelId(1);
const ADMIN: LabelId = LabelId(2);
const ZED: LabelId = LabelId(3);
const SECRET: LabelId = LabelId(99);

fn read_labels(
    transaction: &WriteTxn,
    database: &Database<MemVfs>,
    vertex: VId,
    visible: impl FnMut(LabelId) -> bool,
) -> (Result<GraphValue, Fault>, Vec<GlaExecutionEvent>) {
    let mut catalog = catalog();
    catalog.insert_label(PERSON, "Person");
    catalog.insert_label(ADMIN, "Admin");
    catalog.insert_label(ZED, "Zed");
    let mut events = Vec::new();
    let value = vertex_upsert_labels(
        transaction,
        database,
        vertex,
        Some(&catalog),
        visible,
        || Ok(()),
        &mut |event| {
            events.push(event);
            Ok(())
        },
    );
    (value, events)
}
fn names(values: &[&str]) -> GraphValue {
    GraphValue::List(
        values
            .iter()
            .map(|value| GraphValue::Scalar(CanonicalScalar::ucs_basic_text(value).unwrap()))
            .collect(),
    )
}

// fgdb-iufb7: MERGE ... RETURN labels(n) reads the selected vertex's history
// and staged membership edits in identity order. A hidden label is skipped
// before naming or any charge, exactly as if it were physically absent.
#[test]
fn merge_labels_mask_history_and_hidden_staged_edits_before_charging() {
    let ((), report) = run_async_under_lab(0xb76d_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, txcx) = (contexts.commit(), contexts.txn());
        let visible = |label: LabelId| label != SECRET;
        let mut observations = Vec::new();
        for hidden in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut labels = vec![ADMIN, PERSON];
            if hidden {
                labels.push(SECRET);
            }
            let mut seed = WriteBatch::new(R);
            seed.create_vertex(VId(1), labels, vec![]);
            db.write(&commit, seed).await.unwrap();
            let mut transaction = db.begin(&txcx).unwrap();
            let (value, events) = read_labels(&transaction, &db, VId(1), visible);
            let before = (value.unwrap(), events);
            // Identity order, not name order.
            assert_eq!(before.0, names(&["Person", "Admin"]));
            // The whole element is witnessed, as for a complete property map.
            assert!(
                transaction
                    .read_set
                    .borrow()
                    .contains(&ElementId::Vertex(VId(1)))
            );
            assert!(transaction.point_reads.borrow().is_empty());
            transaction.savepoint(&db, "before").unwrap();
            let mut edits = WriteBatch::new(R);
            edits.set_vertex_label(VId(1), PERSON, false);
            edits.set_vertex_label(VId(1), ZED, true);
            if hidden {
                edits.set_vertex_label(VId(1), SECRET, false);
            }
            edits.create_vertex(VId(2), vec![ZED], vec![]);
            transaction.write(&mut db, edits).unwrap();
            let mut created = WriteBatch::new(R);
            created.set_vertex_label(VId(2), PERSON, true);
            transaction.write(&mut db, created).unwrap();
            let (value, events) = read_labels(&transaction, &db, VId(1), visible);
            let after = (value.unwrap(), events);
            assert_eq!(after.0, names(&["Admin", "Zed"]));
            let (value, events) = read_labels(&transaction, &db, VId(1), visible);
            assert_eq!((value.unwrap(), events), after);
            // A vertex created in this transaction reads its created labels
            // and later edits.
            assert_eq!(
                read_labels(&transaction, &db, VId(2), visible).0.unwrap(),
                names(&["Person", "Zed"])
            );
            transaction.rollback_to_savepoint(&db, "before").unwrap();
            let (value, events) = read_labels(&transaction, &db, VId(1), visible);
            assert_eq!((value.unwrap(), events), before);
            if hidden {
                // An unnamed label refuses; it is never omitted.
                assert!(matches!(
                    read_labels(&transaction, &db, VId(1), |_| true).0,
                    Err(GqlQueryError::Source(GraphVertexUpsertError::Staging(
                        WriteTxnError::Read(crate::ReadError::UnmappedLabel(SECRET))
                    )))
                ));
            }
            observations.push((before, after));
            transaction.abort();
        }
        assert_eq!(observations[0], observations[1]);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
