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
    let authority =
        Authority::new(AuthKey::from_seed(7127), NS, "graph", SchemaEpoch(1), 1).unwrap();
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
    // Hidden values stay small, so even 255 of them fit one row's 16 KiB
    // storage bound: only how many keys there are moves a physical lookup.
    for key in 0..hidden {
        properties.push((
            PropertyKeyId(10 + key as u64),
            CanonicalScalar::bytes(vec![0x63; 16]).unwrap(),
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
                assert_eq!(
                    raw_value.property.and_then(|value| value.as_scalar()),
                    Some(&CanonicalScalar::Int(7))
                );
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
                assert_eq!(
                    value,
                    fgdb_types::CanonicalPropertyValue::from(CanonicalScalar::Int(7))
                );
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
            assert_eq!(
                value,
                fgdb_types::CanonicalPropertyValue::from(CanonicalScalar::Null)
            );
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
                assert_eq!(result.unwrap(), payload.clone().into());
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
        assert_eq!(
            value,
            fgdb_types::CanonicalPropertyValue::from(CanonicalScalar::Int(19))
        );
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
        assert_eq!(
            value,
            fgdb_types::CanonicalPropertyValue::from(CanonicalScalar::Null)
        );
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

fn metadata_catalog() -> fgdb_gql::ReverseSymbolCatalog {
    let mut catalog = fgdb_gql::ReverseSymbolCatalog::new();
    catalog.insert_property(P, "p");
    catalog.insert_property(PropertyKeyId(2), "second");
    catalog.insert_label(VISIBLE, "Visible");
    catalog.insert_relation(R, "R");
    catalog
}

fn observe_metadata(
    txn: &WriteTxn,
    db: &Database<MemVfs>,
    element: ElementId,
    kind: fgdb_gql::GraphElementMetadata,
    scope: &PlannerPredicates,
) -> fgdb_gql::algebra::GraphValue {
    // This is the same original-object admission used by the signed host.
    // Its field/lifetime witnesses must remain independent of the map domain.
    match element {
        ElementId::Vertex(vertex) => {
            assert!(mutation_vertex_visible(txn, db, vertex, scope, &mut || Ok(())).unwrap());
        }
        ElementId::Edge(edge) => {
            let (source, relation, target) = mutation_edge_topology(txn, db, edge, &mut || Ok(()))
                .unwrap()
                .unwrap();
            assert!(scope.allows_relation(relation));
            for vertex in [source, target] {
                assert!(mutation_vertex_visible(txn, db, vertex, scope, &mut || Ok(())).unwrap());
            }
        }
    }
    txn.admitted_element_metadata_with_control(
        db,
        element,
        kind,
        &metadata_catalog(),
        Some(scope),
        &mut || Ok::<(), WriteTxnError>(()),
        &mut |_| Ok(()),
        &core::convert::identity,
    )
    .unwrap()
    .unwrap()
}

#[test]
fn complete_metadata_observes_visible_insertion_gaps_without_hidden_or_keys_value_conflicts() {
    use fgdb_gql::{GraphElementMetadata, algebra::GraphValue};
    let (_, report) = run_async_under_lab(0xb174, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let scope = predicates();
        for element in [ElementId::Vertex(VId(1)), ElementId::Edge(EId(1))] {
            for kind in [GraphElementMetadata::Properties, GraphElementMetadata::Keys] {
                for key in [PropertyKeyId(2), P, HIDDEN, PropertyKeyId(11)] {
                    let mut db = database(&contexts.commit(), 1, CanonicalScalar::Int(7)).await;
                    let txn_cx = contexts.txn();
                    let mut txn = db.begin(&txn_cx).unwrap();
                    let observed = observe_metadata(&txn, &db, element, kind, &scope);
                    let expected = match kind {
                        GraphElementMetadata::Properties => GraphValue::map(vec![(
                            "p".into(),
                            GraphValue::Scalar(CanonicalScalar::Int(7)),
                        )])
                        .unwrap(),
                        _ => GraphValue::List(
                            vec![GraphValue::Scalar(
                                CanonicalScalar::ucs_basic_text("p").unwrap(),
                            )]
                            .into_boxed_slice(),
                        ),
                    };
                    assert_eq!(
                        observed, expected,
                        "hidden unmapped fields are masked before naming"
                    );
                    let mut change = WriteBatch::new(R);
                    match element {
                        ElementId::Vertex(vertex) => {
                            change.set_vertex_property(vertex, key, Some(CanonicalScalar::Int(19)))
                        }
                        ElementId::Edge(edge) => {
                            change.set_edge_property(edge, key, Some(CanonicalScalar::Int(19)))
                        }
                    };
                    db.write(&contexts.commit(), change).await.unwrap();
                    let result = txn.refresh_snapshot(&db, &txn_cx);
                    let conflict = key == PropertyKeyId(2)
                        || (key == P && kind == GraphElementMetadata::Properties);
                    if conflict {
                        assert!(
                            matches!(
                                result,
                                Err(WriteTxnError::Write(
                                    crate::WriteError::FirstCommitterWins {
                                        law: "FG-LAW-FCW-READ-01",
                                        ..
                                    }
                                ))
                            ),
                            "{element:?} {kind:?} {key:?}: {result:?}"
                        );
                    } else {
                        assert!(result.is_ok(), "{element:?} {kind:?} {key:?}: {result:?}");
                    }
                    txn.abort();
                    assert_eq!(txn_cx.outstanding_obligations(), 0);
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn metadata_keeps_endpoint_visibility_observations_and_refused_reads_survive_retry() {
    use fgdb_gql::GraphElementMetadata;
    let (_, report) = run_async_under_lab(0xb175, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let scope = predicates();
        let mut db = database(&contexts.commit(), 1, CanonicalScalar::Int(7)).await;
        let txn_cx = contexts.txn();
        let mut txn = db.begin(&txn_cx).unwrap();
        let edge = ElementId::Edge(EId(1));
        observe_metadata(&txn, &db, edge, GraphElementMetadata::Keys, &scope);
        let mut change = WriteBatch::new(R);
        change.set_vertex_label(VId(2), VISIBLE, false);
        db.write(&contexts.commit(), change).await.unwrap();
        assert!(matches!(
            txn.refresh_snapshot(&db, &txn_cx),
            Err(WriteTxnError::Write(
                crate::WriteError::FirstCommitterWins {
                    law: "FG-LAW-FCW-READ-01",
                    ..
                }
            ))
        ));
        txn.abort();
        assert_eq!(txn_cx.outstanding_obligations(), 0);

        let mut db = database(&contexts.commit(), 1, CanonicalScalar::Int(7)).await;
        let txn_cx = contexts.txn();
        let mut txn = db.begin(&txn_cx).unwrap();
        let element = ElementId::Vertex(VId(1));
        let refused = txn.admitted_element_metadata_with_control(
            &db,
            element,
            GraphElementMetadata::Properties,
            &metadata_catalog(),
            Some(&scope),
            &mut || Err("physical cancellation"),
            &mut |_| Ok(()),
            &|_| "source",
        );
        assert_eq!(refused, Err("physical cancellation"));
        assert_eq!(txn.point_reads.borrow().1, Some(element));
        observe_metadata(&txn, &db, element, GraphElementMetadata::Properties, &scope);
        assert_eq!(txn.point_reads.borrow().1, Some(element));
        let mut change = WriteBatch::new(R);
        change.set_vertex_property(VId(1), HIDDEN, Some(CanonicalScalar::Int(19)));
        db.write(&contexts.commit(), change).await.unwrap();
        assert!(matches!(
            txn.refresh_snapshot(&db, &txn_cx),
            Err(WriteTxnError::Write(
                crate::WriteError::FirstCommitterWins {
                    law: "FG-LAW-FCW-READ-01",
                    ..
                }
            ))
        ));
        txn.abort();
        assert_eq!(txn_cx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn metadata_grant(max_nodes: u64) -> Grant {
    Grant {
        branch: "main".into(),
        labels: Scope::only([VISIBLE]),
        relations: Scope::only([R]),
        properties: Scope::only([P, PropertyKeyId(2)]),
        rights: Rights::ReadWrite,
        limits: QueryLimits {
            max_nodes,
            max_work: 1_000_000,
            max_rows: 100,
        },
        expires_at_ms: 10_000,
    }
}

#[test]
fn carried_metadata_charges_each_visible_node_and_unique_edge_endpoint() {
    use fgdb_gql::{GraphElementMetadata, GraphElementMetadataError};
    use fgdb_warden::LimitDimension;

    let (_, report) = run_async_under_lab(0xb176, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, query, txn_cx) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = database(&commit, 1, CanonicalScalar::Int(7)).await;
        let mut loops = WriteBatch::new(R);
        loops.add_edge(EId(2), VId(1), VId(1), vec![]);
        db.write(&commit, loops).await.unwrap();
        let authority =
            Authority::new(AuthKey::from_seed(7128), NS, "graph", SchemaEpoch(1), 1).unwrap();
        let catalog = metadata_catalog();
        for (element, nodes, kinds) in [
            (
                ElementId::Vertex(VId(1)),
                1,
                [
                    GraphElementMetadata::Labels,
                    GraphElementMetadata::Keys,
                    GraphElementMetadata::Properties,
                ],
            ),
            (
                ElementId::Edge(EId(1)),
                2,
                [
                    GraphElementMetadata::Type,
                    GraphElementMetadata::Keys,
                    GraphElementMetadata::Properties,
                ],
            ),
            (
                ElementId::Edge(EId(2)),
                1,
                [
                    GraphElementMetadata::Type,
                    GraphElementMetadata::Keys,
                    GraphElementMetadata::Properties,
                ],
            ),
        ] {
            for kind in kinds {
                for max_nodes in [nodes - 1, nodes] {
                    let token = authority.issue_at(&metadata_grant(max_nodes), 100).unwrap();
                    let verified = authority.verify_at(&token, "main", 100).unwrap();
                    let mut execution = Execution {
                        cx: &commit,
                        permit: verified.begin_write_at("main", 100).unwrap(),
                        clock: || 100,
                    };
                    let controls = RefCell::new(&mut execution);
                    let txn = db.begin(&txn_cx).unwrap();
                    let mut output_events = 0;
                    let result = returning_element_metadata(
                        &txn,
                        &db,
                        &query,
                        element,
                        kind,
                        &catalog,
                        verified.predicates(),
                        &controls,
                        &mut |_| {
                            output_events += 1;
                            Ok(())
                        },
                    );
                    if max_nodes < nodes {
                        assert!(
                            matches!(
                                result,
                                Err(GraphElementMetadataError::Source(
                                    WriteTxnError::Authorization(Error::LimitExceeded(
                                        LimitDimension::Nodes
                                    ))
                                ))
                            ),
                            "{element:?} {kind:?}: {result:?}"
                        );
                        assert_eq!(output_events, 0, "quota refuses before metadata construction");
                    } else {
                        assert!(
                            matches!(result, Ok(Some(_))),
                            "{element:?} {kind:?}: {result:?}"
                        );
                        assert_eq!(controls.borrow().permit.usage().nodes, nodes);
                        assert!(output_events > 0);
                        // Warm observations do not grant a second read for free.
                        output_events = 0;
                        let repeated = returning_element_metadata(
                            &txn,
                            &db,
                            &query,
                            element,
                            kind,
                            &catalog,
                            verified.predicates(),
                            &controls,
                            &mut |_| {
                                output_events += 1;
                                Ok(())
                            },
                        );
                        assert!(
                            matches!(
                                repeated,
                                Err(GraphElementMetadataError::Source(
                                    WriteTxnError::Authorization(Error::LimitExceeded(
                                        LimitDimension::Nodes
                                    ))
                                ))
                            ),
                            "{element:?} {kind:?}: {repeated:?}"
                        );
                        assert_eq!(output_events, 0);
                    }
                    txn.abort();
                    assert_eq!(txn_cx.outstanding_obligations(), 0);
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn hidden_and_absent_metadata_use_no_nodes_and_scalar_properties_keep_their_contract() {
    use fgdb_gql::GraphElementMetadata;

    let (_, report) = run_async_under_lab(0xb177, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, query, txn_cx) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = database(&commit, 1, CanonicalScalar::Int(7)).await;
        let mut hidden = WriteBatch::new(R);
        hidden.create_vertex(VId(3), vec![LabelId(99)], vec![]);
        hidden.add_edge(EId(3), VId(1), VId(3), vec![]);
        hidden.add_edge(EId(4), VId(3), VId(1), vec![]);
        db.write(&commit, hidden).await.unwrap();
        let mut relation = WriteBatch::new(RelationId(2));
        relation.add_edge(EId(5), VId(1), VId(2), vec![]);
        db.write(&commit, relation).await.unwrap();
        let authority =
            Authority::new(AuthKey::from_seed(7129), NS, "graph", SchemaEpoch(1), 1).unwrap();
        let catalog = metadata_catalog();
        let mut usage_by_kind = [None, None];
        for element in [
            ElementId::Vertex(VId(99)),
            ElementId::Vertex(VId(3)),
            ElementId::Edge(EId(99)),
            ElementId::Edge(EId(3)),
            ElementId::Edge(EId(4)),
            ElementId::Edge(EId(5)),
        ] {
            let token = authority.issue_at(&metadata_grant(0), 100).unwrap();
            let verified = authority.verify_at(&token, "main", 100).unwrap();
            let mut execution = Execution {
                cx: &commit,
                permit: verified.begin_write_at("main", 100).unwrap(),
                clock: || 100,
            };
            let controls = RefCell::new(&mut execution);
            let txn = db.begin(&txn_cx).unwrap();
            let mut output_events = 0;
            let result = returning_element_metadata(
                &txn,
                &db,
                &query,
                element,
                GraphElementMetadata::Properties,
                &catalog,
                verified.predicates(),
                &controls,
                &mut |_| {
                    output_events += 1;
                    Ok(())
                },
            );
            assert!(matches!(result, Ok(None)), "{element:?}: {result:?}");
            assert_eq!(
                output_events, 0,
                "hidden fields are never named or constructed"
            );
            let usage = controls.borrow().permit.usage();
            assert_eq!(usage.nodes, 0);
            let kind = usize::from(matches!(element, ElementId::Edge(_)));
            if let Some(expected) = usage_by_kind[kind] {
                assert_eq!(usage, expected, "hidden and absent {element:?}");
            } else {
                usage_by_kind[kind] = Some(usage);
            }
            txn.abort();
            assert_eq!(txn_cx.outstanding_obligations(), 0);
        }
        // The correction is metadata-specific. Scalar point reads retain
        // their existing work/payload accounting and do not acquire a node cap.
        for element in [ElementId::Vertex(VId(1)), ElementId::Edge(EId(1))] {
            let token = authority.issue_at(&metadata_grant(0), 100).unwrap();
            let verified = authority.verify_at(&token, "main", 100).unwrap();
            let mut execution = Execution {
                cx: &commit,
                permit: verified.begin_write_at("main", 100).unwrap(),
                clock: || 100,
            };
            let controls = RefCell::new(&mut execution);
            let txn = db.begin(&txn_cx).unwrap();
            let value = returning_element_property(
                &txn,
                &db,
                &query,
                element,
                P,
                verified.predicates(),
                &controls,
            )
            .unwrap();
            assert_eq!(value, Some(CanonicalScalar::Int(7).into()));
            assert_eq!(controls.borrow().permit.usage().nodes, 0);
            txn.abort();
            assert_eq!(txn_cx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
