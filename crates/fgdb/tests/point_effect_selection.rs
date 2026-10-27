//! The exact production prefix selector is checked against encoded canonical
//! bytes. Real transactions below exercise its field, cascade and budget laws.

#![recursion_limit = "256"]

#[path = "../src/write_txn_parts/point_effect_selector.rs"]
mod selector;

use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch, WriteTxn};
use fgdb_delta_types::{
    DeltaRow, ElementId, EscrowDomainId, LabelId, OperationKey, PropertyKeyId, RelationId,
    SchemaEpoch,
};
use fgdb_gql::{GqlQueryError, GqlQueryPolicy};
use fgdb_types::{CanonicalScalar, EId, ObjectId, VId};
use fgdb_types::{CommitCx, DatabaseSecurityNamespaceId, PurposeContexts, QueryCx, TxnCx};
use selector::EffectKey;

const P: PropertyKeyId = PropertyKeyId(1);
const R: RelationId = RelationId(1);
const ID_BOUNDARIES: [u128; 8] = [
    0,
    1,
    255,
    256,
    (1_u128 << 64) - 1,
    1_u128 << 64,
    1_u128 << 127,
    u128::MAX,
];

fn encoded_fixture() -> Vec<(EffectKey, DeltaRow)> {
    let mut fixtures = Vec::new();
    for id in ID_BOUNDARIES {
        let (vertex, edge) = (VId(id), EId(id));
        fixtures.extend([
            (
                EffectKey::CreateVertex(vertex),
                DeltaRow::CreateVertex {
                    vid: vertex,
                    birth_ordinal: u64::MAX,
                    labels: Vec::new(),
                    props: Vec::new(),
                    valid_time: None,
                },
            ),
            (
                EffectKey::CreateVertex(vertex),
                DeltaRow::CreateVertex {
                    vid: vertex,
                    birth_ordinal: 0,
                    labels: vec![LabelId(u64::MAX)],
                    props: vec![(P, CanonicalScalar::Int(7).into())],
                    valid_time: None,
                },
            ),
            (
                EffectKey::CreateEdge(edge),
                DeltaRow::CreateEdge {
                    eid: edge,
                    birth_ordinal: 0,
                    src: VId(u128::MAX),
                    relation: RelationId(u64::MAX),
                    dst: VId(0),
                    canonical_key: None,
                    props: Vec::new(),
                    valid_time: None,
                },
            ),
            (
                EffectKey::DeleteVertex(vertex),
                DeltaRow::DeleteVertex {
                    vid: vertex,
                    before_version: ObjectId([0xff; 32]),
                    sorted_retired_incident_edges: vec![EId(0), EId(u128::MAX)],
                },
            ),
            (
                EffectKey::DeleteEdge(edge),
                DeltaRow::DeleteEdge {
                    eid: edge,
                    before_version: ObjectId([0; 32]),
                },
            ),
        ]);
        for field in [0, 1, 255, 256, (1_u64 << 32) - 1, 1_u64 << 32, u64::MAX] {
            let label = LabelId(field);
            for (before, after) in [(false, true), (true, false)] {
                fixtures.push((
                    EffectKey::Label(vertex, label),
                    DeltaRow::LabelMembership {
                        vid: vertex,
                        label,
                        before,
                        after,
                    },
                ));
            }
            let property = PropertyKeyId(field);
            for element in [ElementId::Vertex(vertex), ElementId::Edge(edge)] {
                // Distinct payload suffixes share a prefix and must both be
                // retained, in byte order, by the lower/upper-bound lookup.
                for value in [i64::MIN, i64::MAX] {
                    fixtures.push((
                        EffectKey::Property(element, property),
                        DeltaRow::Property {
                            elem: element,
                            property,
                            before: None,
                            after: Some(CanonicalScalar::Int(value).into()),
                        },
                    ));
                }
            }
        }
    }
    let oid = ObjectId([0x43; 32]);
    fixtures.extend([
        (
            EffectKey::Other,
            DeltaRow::ValidTime {
                elem: ElementId::Edge(EId(u128::MAX)),
                contract_id: oid,
                before: None,
                after: None,
            },
        ),
        (
            EffectKey::Other,
            DeltaRow::Counter {
                operation_key: OperationKey([0; 32]),
                elem: ElementId::Vertex(VId(0)),
                property: P,
                algebra_profile: oid,
                delta: 1,
                before: 0,
                after: 1,
            },
        ),
        (
            EffectKey::Other,
            DeltaRow::Escrow {
                domain_id: EscrowDomainId(u128::MAX),
                epoch: u64::MAX,
                operation_key: OperationKey([0xff; 32]),
                subject: ElementId::Edge(EId(0)),
                subject_property: None,
                delta: -1,
                before_value: 1,
                after_value: 0,
            },
        ),
        (
            EffectKey::Other,
            DeltaRow::Sketch {
                operation_key: OperationKey([0; 32]),
                sketch_profile_oid: oid,
                before_state_digest: [0; 32],
                after_state_oid: oid,
            },
        ),
        (
            EffectKey::Other,
            DeltaRow::Schema {
                transition_oid: oid,
                before_epoch: SchemaEpoch(0),
                after_epoch: SchemaEpoch(u64::MAX),
            },
        ),
        (
            EffectKey::Other,
            DeltaRow::Constraint {
                before_schema_root: oid,
                after_schema_root: ObjectId([0xff; 32]),
                before_constraint_root: ObjectId([0; 32]),
                after_constraint_root: oid,
            },
        ),
    ]);
    fixtures.sort_by_cached_key(|(_, row)| row.canonical_bytes().unwrap());
    fixtures
}

#[test]
fn prefix_order_and_exact_ranges_follow_real_canonical_bytes() {
    let fixture = encoded_fixture();
    // Expected prefixes are assigned beside each independent typed fixture,
    // while the sorting oracle is the actual durable encoding.
    assert!(fixture.windows(2).all(|pair| pair[0].0 <= pair[1].0));
    for (expected, row) in &fixture {
        assert_eq!(EffectKey::of(row), *expected);
    }
    let rows: Vec<_> = fixture.iter().map(|(_, row)| row.clone()).collect();
    let mut keys: Vec<_> = fixture.iter().map(|(key, _)| *key).collect();
    keys.dedup();
    keys.extend([
        EffectKey::CreateVertex(VId(2)),
        EffectKey::DeleteEdge(EId(u128::MAX - 1)),
        EffectKey::Property(ElementId::Vertex(VId(2)), P),
        EffectKey::Property(ElementId::Edge(EId(u128::MAX)), PropertyKeyId(2)),
    ]);
    for key in keys {
        let actual = selector::select(&rows, key, &mut || Ok::<_, ()>(())).unwrap();
        let expected: Vec<_> = fixture
            .iter()
            .filter(|(expected, _)| *expected == key)
            .map(|(_, row)| row.canonical_bytes().unwrap())
            .collect();
        assert_eq!(
            actual
                .iter()
                .map(|row| row.canonical_bytes().unwrap())
                .collect::<Vec<_>>(),
            expected,
            "{key:?}"
        );
    }
}

#[test]
fn every_range_search_checkpoint_interrupts_hits_misses_and_empty_inputs() {
    let rows: Vec<_> = encoded_fixture().into_iter().map(|(_, row)| row).collect();
    for (rows, key) in [
        (rows.as_slice(), EffectKey::CreateVertex(VId(1_u128 << 127))),
        (
            rows.as_slice(),
            EffectKey::Property(ElementId::Edge(EId(u128::MAX)), P),
        ),
        (
            rows.as_slice(),
            EffectKey::Property(ElementId::Vertex(VId(99)), P),
        ),
        (&[][..], EffectKey::DeleteEdge(EId(0))),
    ] {
        let mut calls = 0;
        selector::select(rows, key, &mut || {
            calls += 1;
            Ok::<_, usize>(())
        })
        .unwrap();
        assert!(calls > 0);
        for cut in 1..=calls {
            let mut seen = 0;
            let result = selector::select(rows, key, &mut || {
                seen += 1;
                if seen == cut { Err(cut) } else { Ok(()) }
            });
            assert!(matches!(result, Err(at) if at == cut));
            assert_eq!(seen, cut);
        }
    }
}

fn metadata_ranges(id: u128) -> [(EffectKey, EffectKey); 3] {
    [
        (
            EffectKey::Label(VId(id), LabelId(0)),
            EffectKey::Label(VId(id), LabelId(u64::MAX)),
        ),
        (
            EffectKey::Property(ElementId::Vertex(VId(id)), PropertyKeyId(0)),
            EffectKey::Property(ElementId::Vertex(VId(id)), PropertyKeyId(u64::MAX)),
        ),
        (
            EffectKey::Property(ElementId::Edge(EId(id)), PropertyKeyId(0)),
            EffectKey::Property(ElementId::Edge(EId(id)), PropertyKeyId(u64::MAX)),
        ),
    ]
}

#[test]
fn inclusive_metadata_ranges_follow_real_canonical_bytes() {
    let fixture = encoded_fixture();
    let rows: Vec<_> = fixture.iter().map(|(_, row)| row.clone()).collect();
    let mut ranges: Vec<_> = ID_BOUNDARIES
        .into_iter()
        .chain([2, 99, u128::MAX - 1])
        .flat_map(metadata_ranges)
        .collect();
    ranges.extend([
        (
            EffectKey::Label(VId(0), LabelId(0)),
            EffectKey::Label(VId(0), LabelId(1)),
        ),
        (
            EffectKey::Label(VId(u128::MAX), LabelId(u64::MAX)),
            EffectKey::Label(VId(u128::MAX), LabelId(u64::MAX)),
        ),
        (
            EffectKey::Property(ElementId::Vertex(VId(256)), PropertyKeyId(2)),
            EffectKey::Property(ElementId::Vertex(VId(256)), PropertyKeyId(254)),
        ),
        (
            EffectKey::Property(ElementId::Edge(EId(1_u128 << 127)), PropertyKeyId(256)),
            EffectKey::Property(ElementId::Edge(EId(1_u128 << 127)), PropertyKeyId(u64::MAX)),
        ),
        (
            EffectKey::CreateVertex(VId(0)),
            EffectKey::Property(ElementId::Edge(EId(u128::MAX)), PropertyKeyId(u64::MAX)),
        ),
        (EffectKey::Other, EffectKey::Other),
        (
            EffectKey::Property(ElementId::Vertex(VId(0)), PropertyKeyId(u64::MAX)),
            EffectKey::Property(ElementId::Vertex(VId(0)), PropertyKeyId(0)),
        ),
    ]);
    for (first, last) in ranges {
        let actual = selector::select_range(&rows, first, last, &mut || Ok::<_, ()>(())).unwrap();
        // Prefixes were assigned independently beside each typed fixture;
        // actual durable bytes determine ordering, including all suffixes.
        let expected: Vec<_> = fixture
            .iter()
            .filter(|(key, _)| first <= *key && *key <= last)
            .map(|(_, row)| row.canonical_bytes().unwrap())
            .collect();
        assert_eq!(
            actual
                .iter()
                .map(|row| row.canonical_bytes().unwrap())
                .collect::<Vec<_>>(),
            expected,
            "{first:?}..={last:?}"
        );
    }
}

#[test]
fn every_metadata_range_checkpoint_preserves_the_exact_interruption() {
    let rows: Vec<_> = encoded_fixture().into_iter().map(|(_, row)| row).collect();
    let ranges: Vec<_> = ID_BOUNDARIES
        .into_iter()
        .chain([2, 99, u128::MAX - 1])
        .flat_map(metadata_ranges)
        .collect();
    for input in [rows.as_slice(), &[][..]] {
        for &(first, last) in &ranges {
            // Reversed bounds are empty but must still be cancellable.
            for (first, last) in [(first, last), (last, first)] {
                let mut calls = 0;
                let selected = selector::select_range(input, first, last, &mut || {
                    calls += 1;
                    Ok::<_, usize>(())
                })
                .unwrap();
                assert!(calls > 0);
                if input.is_empty() || first > last {
                    assert!(selected.is_empty());
                    assert_eq!(calls, 1);
                }
                for cut in 1..=calls {
                    let mut seen = 0;
                    let result = selector::select_range(input, first, last, &mut || {
                        seen += 1;
                        if seen == cut { Err(cut) } else { Ok(()) }
                    });
                    assert!(matches!(result, Err(at) if at == cut));
                    assert_eq!(seen, cut, "{first:?}..={last:?}");
                }
            }
        }
    }
}

#[test]
fn metadata_range_search_work_is_logarithmic_in_unrelated_canonical_rows() {
    let base = 1_u128 << 100;
    let mut observed = Vec::new();
    for count in [64_u128, 4096] {
        let mut rows = Vec::new();
        for index in 0..count {
            let id = base + 2 * index;
            for field in [0, 1, u64::MAX] {
                rows.push(DeltaRow::LabelMembership {
                    vid: VId(id),
                    label: LabelId(field),
                    before: false,
                    after: true,
                });
                for elem in [ElementId::Vertex(VId(id)), ElementId::Edge(EId(id))] {
                    rows.push(DeltaRow::Property {
                        elem,
                        property: PropertyKeyId(field),
                        before: None,
                        after: Some(CanonicalScalar::Int(index as i64).into()),
                    });
                }
            }
        }
        rows.sort_by_cached_key(|row| row.canonical_bytes().unwrap());
        let bound = 1 + 2 * (usize::BITS - rows.len().leading_zeros()) as usize;
        let mut maximum = 0;
        for (id, expected) in [
            (base, 3),
            (base + count, 3),
            (base + 2 * (count - 1), 3),
            (base + 1, 0),
            (0, 0),
            (u128::MAX, 0),
        ] {
            for (first, last) in metadata_ranges(id) {
                let mut calls = 0;
                let selected = selector::select_range(&rows, first, last, &mut || {
                    calls += 1;
                    Ok::<_, ()>(())
                })
                .unwrap();
                assert_eq!(selected.len(), expected, "{first:?}..={last:?}");
                assert!(
                    calls <= bound,
                    "{} canonical rows, {first:?}..={last:?}: {calls} > {bound}",
                    rows.len()
                );
                maximum = maximum.max(calls);
            }
        }
        observed.push((rows.len(), maximum));
    }
    eprintln!("canonical metadata range rows/maximum search polls: {observed:?}");
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xa7; 32],
        DatabaseSecurityNamespaceId([0xa8; 32]),
        [0xa9; 32],
    )
}

fn run<T>(test: impl AsyncFnOnce(&CommitCx, &QueryCx, &TxnCx) -> T) -> T {
    let runtime = fgdb::runtime_builder().build().unwrap();
    let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
    runtime.block_on(test(&commit, &query, &txn))
}

fn compare_full_overlay(transaction: &WriteTxn, database: &Database<MemVfs>, cx: &QueryCx) {
    let policy = GqlQueryPolicy::new(1, 1, 512, 32);
    for id in [0, 255, 256, 1024, 2048, 1_u128 << 127, u128::MAX] {
        let vertex = transaction.vertex(database, VId(id)).unwrap();
        let edge = transaction.edge(database, EId(id)).unwrap();
        for key in [
            PropertyKeyId(0),
            P,
            PropertyKeyId(255),
            PropertyKeyId(256),
            PropertyKeyId(u64::MAX),
        ] {
            let expected_vertex = vertex.as_ref().and_then(|row| {
                row.props
                    .iter()
                    .find_map(|(found, value)| (*found == key).then(|| value.clone()))
            });
            let expected_edge = edge.as_ref().and_then(|row| {
                row.props
                    .iter()
                    .find_map(|(found, value)| (*found == key).then(|| value.clone()))
            });
            assert_eq!(
                transaction
                    .vertex_property_governed(database, cx, VId(id), key, policy)
                    .unwrap()
                    .value,
                vec![expected_vertex],
                "vertex {id}:{key:?}"
            );
            assert_eq!(
                transaction
                    .edge_property_governed(database, cx, EId(id), key, policy)
                    .unwrap()
                    .value,
                vec![expected_edge],
                "edge {id}:{key:?}"
            );
        }
        for label in [LabelId(0), LabelId(255), LabelId(256), LabelId(u64::MAX)] {
            assert_eq!(
                transaction
                    .vertex_has_label_governed(database, cx, VId(id), label, policy)
                    .unwrap()
                    .value,
                vec![vertex.as_ref().map(|row| row.labels.contains(&label))],
                "label {id}:{label:?}",
            );
        }
    }
}

#[test]
fn selected_effects_match_full_overlay_through_multirelation_cascades_and_savepoints() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for (at, id) in [0, 255, 256, 1_u128 << 127, u128::MAX]
            .into_iter()
            .enumerate()
        {
            seed.create_vertex(
                VId(id),
                vec![LabelId(0), LabelId(255)],
                vec![
                    (P, CanonicalScalar::Int(at as i64)),
                    (PropertyKeyId(255), CanonicalScalar::Null),
                    (PropertyKeyId(u64::MAX), CanonicalScalar::Int(-1)),
                ],
            );
        }
        // Reversed endpoint order, a self-loop, and two incident relations.
        for (id, source, target) in [(0, u128::MAX, 0), (255, 255, 255), (256, 256, 256)] {
            seed.add_edge(
                EId(id),
                VId(source),
                VId(target),
                vec![(P, CanonicalScalar::Int(10))],
            );
        }
        db.write(commit, seed).await.unwrap();
        let mut other = WriteBatch::new(RelationId(2));
        other.add_edge(
            EId(u128::MAX),
            VId(u128::MAX),
            VId(256),
            vec![(P, CanonicalScalar::Int(20))],
        );
        other.add_edge(
            EId(1_u128 << 127),
            VId(0),
            VId(0),
            vec![(P, CanonicalScalar::Int(30))],
        );
        db.write(commit, other).await.unwrap();
        let mut transaction = db.begin(txcx).unwrap();
        transaction.savepoint(&db, "initial").unwrap();
        compare_full_overlay(&transaction, &db, cx);
        let mut changes = WriteBatch::new(R);
        changes.set_vertex_property(VId(0), P, Some(CanonicalScalar::Int(99)));
        changes.set_vertex_property(VId(0), PropertyKeyId(255), None);
        changes.set_vertex_label(VId(0), LabelId(0), false);
        changes.set_vertex_label(VId(0), LabelId(u64::MAX), true);
        changes.set_edge_property(EId(0), P, Some(CanonicalScalar::Int(42)));
        changes.create_vertex(
            VId(1024),
            vec![LabelId(256)],
            vec![(P, CanonicalScalar::Int(77))],
        );
        changes.add_edge(
            EId(1024),
            VId(1024),
            VId(u128::MAX),
            vec![(P, CanonicalScalar::Int(78))],
        );
        let mut other = WriteBatch::new(RelationId(2));
        other.set_edge_property(EId(u128::MAX), P, Some(CanonicalScalar::Int(43)));
        transaction
            .write_ordered(&mut db, vec![changes, other])
            .unwrap();
        transaction.savepoint(&db, "created").unwrap();
        compare_full_overlay(&transaction, &db, cx);
        let mut retire = WriteBatch::new(R);
        // The shared edge's cascade can be assigned to either endpoint's NET
        // row; numeric endpoint order and source orientation are independent.
        retire.delete_vertex(VId(u128::MAX));
        retire.delete_vertex(VId(0));
        retire.delete_vertex(VId(255));
        retire.delete_vertex(VId(1024));
        retire.delete_edge(EId(256));
        transaction.write_ordered(&mut db, vec![retire]).unwrap();
        compare_full_overlay(&transaction, &db, cx);
        for id in [0, 255, 256, 1024, 1_u128 << 127, u128::MAX] {
            assert!(transaction.edge(&db, EId(id)).unwrap().is_none());
        }
        transaction.rollback_to_savepoint(&db, "created").unwrap();
        compare_full_overlay(&transaction, &db, cx);
        assert!(transaction.edge(&db, EId(1024)).unwrap().is_some());
        transaction.rollback_to_savepoint(&db, "initial").unwrap();
        compare_full_overlay(&transaction, &db, cx);
        assert!(transaction.vertex(&db, VId(1024)).unwrap().is_none());
        let mut replacement = WriteBatch::new(R);
        replacement.create_vertex(
            VId(2048),
            vec![LabelId(u64::MAX)],
            vec![(P, CanonicalScalar::Int(88))],
        );
        transaction.write(&mut db, replacement).unwrap();
        compare_full_overlay(&transaction, &db, cx);
        transaction.abort();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn unrelated_staged_rows_do_not_spend_linear_point_work() {
    run(async |commit, cx, txcx| {
        let mut maximum = Vec::new();
        for count in [64_u128, 4096] {
            let mut db = Database::open_memory(commit, keys()).await.unwrap();
            let mut transaction = db.begin(txcx).unwrap();
            let base = 1_u128 << 100;
            let mut batch = WriteBatch::new(R);
            for index in 0..count {
                batch.create_vertex(
                    VId(base + index),
                    vec![LabelId(1)],
                    vec![(P, CanonicalScalar::Int(index as i64))],
                );
            }
            for index in 0..count {
                batch.add_edge(
                    EId(base + index),
                    VId(base + index),
                    VId(base + (index + 1) % count),
                    vec![(P, CanonicalScalar::Int(index as i64 + 1))],
                );
            }
            transaction.write(&mut db, batch).unwrap();
            // The old traversal of 8192 NET rows cannot fit this allowance.
            // Both present and absent identities must use targeted searches.
            let policy = GqlQueryPolicy::new(0, 1, 256, 16);
            let mut work = 0;
            for index in [0, count / 2, count - 1, count] {
                let vertex = transaction
                    .vertex_property_governed(&db, cx, VId(base + index), P, policy)
                    .unwrap();
                let edge = transaction
                    .edge_property_governed(&db, cx, EId(base + index), P, policy)
                    .unwrap();
                assert_eq!(
                    vertex.value,
                    vec![(index < count).then(|| CanonicalScalar::Int(index as i64).into())]
                );
                assert_eq!(
                    edge.value,
                    vec![(index < count).then(|| CanonicalScalar::Int(index as i64 + 1).into())]
                );
                work = work
                    .max(vertex.evaluator.work_units)
                    .max(edge.evaluator.work_units);
                assert_eq!(vertex.rows.snapshot_records, 0);
                assert_eq!(edge.rows.snapshot_records, 0);
            }
            maximum.push(work);
            transaction.abort();
        }
        assert!(
            maximum[1] < maximum[0] * 3,
            "64/4096 point work: {maximum:?}"
        );
        eprintln!("maximum point work for 64/4096 staged vertices+edges: {maximum:?}");
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn controlled_point_searches_preserve_staging_at_every_interruption() {
    use fgdb_types::context::SimulationCheckpointProbe;
    use std::sync::Arc;

    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        let mut transaction = db.begin(txcx).unwrap();
        let mut batch = WriteBatch::new(R);
        for id in 0..64 {
            batch.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(id as i64))]);
        }
        transaction.write(&mut db, batch).unwrap();
        let digest = transaction.staged_effect_digest().unwrap();
        let policy = GqlQueryPolicy::new(0, 1, 256, 16);
        for target in [VId(63), VId(u128::MAX)] {
            let probe = Arc::new(SimulationCheckpointProbe::new(None));
            transaction
                .vertex_property_governed(
                    &db,
                    &cx.with_checkpoint_probe(probe.clone()),
                    target,
                    P,
                    policy,
                )
                .unwrap();
            let calls = probe.calls();
            for cut in 1..=calls {
                let probe = Arc::new(SimulationCheckpointProbe::new(Some(cut)));
                let result = transaction.vertex_property_governed(
                    &db,
                    &cx.with_checkpoint_probe(probe.clone()),
                    target,
                    P,
                    policy,
                );
                assert!(
                    matches!(result, Err(GqlQueryError::Interrupted(_))),
                    "target {target:?} checkpoint {cut}: {result:?}"
                );
                assert_eq!(probe.calls(), cut);
                assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
            }
        }
        assert!(db.vertices().unwrap().is_empty());
        transaction.abort();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}
