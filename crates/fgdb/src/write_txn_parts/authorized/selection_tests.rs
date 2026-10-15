//! Physical point lookups vary with private storage. Observable admission must
//! not: the same visible field pays for its lookup, payload, witness and value.
use super::*;
use crate::{DatabaseKeys, MemVfs, WriteBatch};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb_delta_types::{LabelId, RelationId, SchemaEpoch};
use fgdb_types::{CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use fgdb_warden::{Authority, Grant, QueryLimits, Rights, Scope};
use std::cell::Cell;

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x47; 32]);
const P: PropertyKeyId = PropertyKeyId(1);
const HIDDEN: PropertyKeyId = PropertyKeyId(10);
const R: RelationId = RelationId(1);
const VISIBLE: LabelId = LabelId(1);

fn predicates() -> PlannerPredicates {
    let authority = Authority::new(AuthKey::from_seed(7127), NS, "graph", SchemaEpoch(1), 1)
        .unwrap();
    let grant = Grant {
        branch: "main".into(),
        labels: Scope::only([VISIBLE]),
        relations: Scope::only([R]),
        properties: Scope::only([P, PropertyKeyId(2)]),
        rights: Rights::ReadWrite,
        limits: QueryLimits {
            max_nodes: 1000,
            max_work: 1_000_000,
            max_rows: 100,
        },
        expires_at_ms: 10_000,
    };
    let token = authority.issue_at(&grant, 100).unwrap();
    authority
        .verify_at(&token, "main", 100)
        .unwrap()
        .predicates()
        .clone()
}

async fn database(cx: &CommitCx, hidden: usize, value: CanonicalScalar) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x46; 32], NS, [0x48; 32]))
        .await
        .unwrap();
    let mut properties = vec![(P, value)];
    for key in 0..hidden {
        properties.push((
            PropertyKeyId(10 + key as u64),
            CanonicalScalar::bytes(vec![0x63; 1024]).unwrap(),
        ));
    }
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(1), vec![VISIBLE], properties.clone());
    batch.create_vertex(VId(2), vec![VISIBLE], vec![]);
    batch.add_edge(EId(1), VId(1), VId(2), properties);
    db.write(cx, batch).await.unwrap();
    db
}

#[test]
fn vertex_and_edge_field_charges_ignore_hidden_keys_but_physical_lookups_do_not() {
    let (_, report) = run_async_under_lab(0xb171, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let scope = predicates();
        for element in [ElementId::Vertex(VId(1)), ElementId::Edge(EId(1))] {
            let mut physical = Vec::new();
            let mut traces = Vec::new();
            for hidden in [0, 31, 255] {
                let mut db = database(&contexts.commit(), hidden, CanonicalScalar::Int(7)).await;
                let txn_cx = contexts.txn();
                let txn = db.begin(&txn_cx).unwrap();
                let mut raw = 0;
                let raw_value = txn
                    .point_projection_with_control(
                        &db,
                        element,
                        PointReadField::Property(P),
                        &mut |_| {
                            raw += 1;
                            Ok::<(), &'static str>(())
                        },
                        &|_| "source",
                    )
                    .unwrap();
                assert_eq!(raw_value.property, Some(&CanonicalScalar::Int(7)));
                physical.push(raw);
                let mut trace = Vec::new();
                let polls = Cell::new(0);
                let value = property(
                    &txn,
                    &db,
                    element,
                    P,
                    &scope,
                    || {
                        polls.set(polls.get() + 1);
                        Ok::<(), &'static str>(())
                    },
                    &mut |event| {
                        trace.push(event);
                        Ok(())
                    },
                    |_| "source",
                )
                .unwrap();
                assert_eq!(value, CanonicalScalar::Int(7));
                assert_eq!(polls.get(), raw, "every native traversal event still polls");
                assert!(
                    txn.point_reads
                        .borrow()
                        .contains(element, PointReadField::Property(P))
                );
                traces.push(trace);
                txn.abort();
                assert_eq!(txn_cx.outstanding_obligations(), 0);
            }
            assert_ne!(
                physical[0], physical[2],
                "the hidden-key fixture must affect raw lookup"
            );
            assert_eq!(traces[0], traces[1]);
            assert_eq!(traces[0], traces[2]);
            assert_eq!(
                traces[0],
                [
                    GlaExecutionEvent::Work,
                    GlaExecutionEvent::ScratchEntry,
                    GlaExecutionEvent::ScratchEntry,
                    GlaExecutionEvent::ScratchEntry,
                ]
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn masked_fields_never_lookup_or_witness_and_visible_payload_reserves_before_copy() {
    let (_, report) = run_async_under_lab(0xb172, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let scope = predicates();
        let size = 2 * fgdb_gql::algebra::GRAPH_VALUE_PAYLOAD_UNIT_BYTES + 1;
        let payload = CanonicalScalar::bytes(vec![0x37; size]).unwrap();
        let mut db = database(&contexts.commit(), 31, payload.clone()).await;
        let txn_cx = contexts.txn();
        let element = ElementId::Vertex(VId(1));
        for key in [HIDDEN, PropertyKeyId(9999)] {
            let txn = db.begin(&txn_cx).unwrap();
            let mut trace = Vec::new();
            let value = property(
                &txn,
                &db,
                element,
                key,
                &scope,
                || Err("masked property must not touch storage"),
                &mut |event| {
                    trace.push(event);
                    Ok(())
                },
                |_| "source",
            )
            .unwrap();
            assert_eq!(value, CanonicalScalar::Null);
            assert_eq!(trace, [GlaExecutionEvent::ScratchEntry]);
            assert!(txn.point_reads.borrow().is_empty());
            txn.abort();
        }
        // Three native payload units plus the fixed lookup/witness/value work.
        for allowance in 0..=7 {
            let txn = db.begin(&txn_cx).unwrap();
            let mut charged = 0;
            let result = property(
                &txn,
                &db,
                element,
                P,
                &scope,
                || Ok::<(), &'static str>(()),
                &mut |_| {
                    if charged == allowance {
                        return Err("admission");
                    }
                    charged += 1;
                    Ok(())
                },
                |_| "source",
            );
            if allowance < 7 {
                assert_eq!(result, Err("admission"));
                if allowance > 0 {
                    assert_eq!(txn.point_reads.borrow().1, Some(element));
                }
            } else {
                assert_eq!(result.unwrap(), payload);
                assert!(
                    txn.point_reads
                        .borrow()
                        .contains(element, PointReadField::Property(P))
                );
            }
            txn.abort();
        }
        assert_eq!(txn_cx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn physical_interruption_retains_refusal_witness_and_native_overlay_still_wins() {
    let (_, report) = run_async_under_lab(0xb173, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let scope = predicates();
        let mut db = database(&contexts.commit(), 31, CanonicalScalar::Int(7)).await;
        let txn_cx = contexts.txn();
        let mut txn = db.begin(&txn_cx).unwrap();
        let element = ElementId::Vertex(VId(1));
        let result = property(
            &txn,
            &db,
            element,
            P,
            &scope,
            || Err("interrupted"),
            &mut |_| Ok(()),
            |_| "source",
        );
        assert_eq!(result, Err("interrupted"));
        assert_eq!(txn.point_reads.borrow().1, Some(element));
        let mut change = WriteBatch::new(R);
        change.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(19)));
        txn.write(&mut db, change).unwrap();
        let value = property(
            &txn,
            &db,
            element,
            P,
            &scope,
            || Ok::<(), &'static str>(()),
            &mut |_| Ok(()),
            |_| "source",
        )
        .unwrap();
        assert_eq!(value, CanonicalScalar::Int(19));
        assert_eq!(
            txn.point_reads.borrow().1,
            Some(element),
            "retry cannot erase an observation"
        );
        // An admitted but absent field returns NULL with a negative witness.
        let missing = PropertyKeyId(2);
        let value = property(
            &txn,
            &db,
            element,
            missing,
            &scope,
            || Ok::<(), &'static str>(()),
            &mut |_| Ok(()),
            |_| "source",
        )
        .unwrap();
        assert_eq!(value, CanonicalScalar::Null);
        assert!(
            txn.point_reads
                .borrow()
                .contains(element, PointReadField::Property(missing))
        );
        txn.abort();
        assert_eq!(txn_cx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
