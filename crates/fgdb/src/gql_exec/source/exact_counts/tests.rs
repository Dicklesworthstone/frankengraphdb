use super::*;
use fgdb_types::{BranchId, CommitSeq, DatabaseSecurityNamespaceId, EId, GraphId, ObjectId, VId};

fn read(counts: &ExactCounts, source: GraphCountSource) -> Option<u64> {
    counts.lookup(source, &mut |_| Ok::<_, ()>(())).unwrap()
}

#[test]
fn cold_unknown_is_never_zero_and_relation_builder_preserves_multiplicity() {
    let counts = ExactCounts::default();
    assert_eq!(read(&counts, GraphCountSource::Vertices), None);
    assert_eq!(
        read(&counts, GraphCountSource::Relation(RelationId(8))),
        None
    );
    let mut builder = counts.relation_builder().unwrap();
    for relation in [8, 2, u64::MAX, 8, 2, 8] {
        builder
            .observe(RelationId(relation), &mut |_| Ok::<_, ()>(()))
            .unwrap();
    }
    counts.publish_relations(builder.finish(&mut |_| Ok::<_, ()>(())).unwrap());
    for (relation, expected) in [(8, 3), (2, 2), (u64::MAX, 1), (9, 0)] {
        assert_eq!(
            read(&counts, GraphCountSource::Relation(RelationId(relation))),
            Some(expected)
        );
    }
    assert!(counts.relation_builder().is_none());
    assert_eq!(read(&counts, GraphCountSource::Vertices), None);
}

#[test]
fn cold_builder_has_a_fixed_relation_bound_and_refuses_before_allocation() {
    let mut builder = RelationBuilder::default();
    assert_eq!(builder.observe(RelationId(1), &mut |_| Err(7)), Err(7));
    assert_eq!(builder.rows.capacity(), 0);
    assert!(builder.rows.is_empty());
    for relation in 0..=MAX_RELATIONS {
        builder
            .observe(RelationId(relation as u64), &mut |_| Ok::<_, ()>(()))
            .unwrap();
    }
    assert!(builder.rows.capacity() <= MAX_RELATIONS);
    assert!(builder.finish(&mut |_| Ok::<_, ()>(())).unwrap().is_none());
}

#[test]
fn each_cold_builder_event_is_cancellable_before_publication() {
    let run = |stop: Option<usize>| {
        let mut events = 0;
        let counts = ExactCounts::default();
        let mut control = |_| {
            events += 1;
            if stop == Some(events) {
                Err(events)
            } else {
                Ok(())
            }
        };
        let outcome = (|| {
            let mut builder = RelationBuilder::default();
            for relation in (0..35).rev().chain(0..35) {
                builder.observe(RelationId(relation), &mut control)?;
            }
            let built = builder.finish(&mut control)?;
            counts.publish_relations(built);
            Ok::<_, usize>(())
        })();
        (outcome, events, counts)
    };
    let (result, events, counts) = run(None);
    assert_eq!(result, Ok(()));
    assert_eq!(
        read(&counts, GraphCountSource::Relation(RelationId(17))),
        Some(2)
    );
    for stop in 1..=events {
        let (result, seen, counts) = run(Some(stop));
        assert_eq!(result, Err(stop));
        assert_eq!(seen, stop);
        assert!(counts.relations.get().is_none());
    }
}

#[test]
fn committed_deltas_and_pinned_generations_agree_with_independent_maps() {
    use std::collections::{BTreeMap, BTreeSet};
    let mut writer = BlockWriter::new(GraphId(1), BranchId(1), 0);
    let mut counts = ExactCounts::new(true);
    let mut vertices = BTreeSet::new();
    let mut edges = BTreeMap::new();
    let mut retained = Vec::new();
    {
        let mut apply = |row: DeltaRow, seq: u64| {
            retained.push((counts.clone(), vertices.len() as u64, edges.clone()));
            counts.apply_before(&writer, &row);
            writer
                .apply(
                    (&[3; 32], DatabaseSecurityNamespaceId([7; 32])),
                    CommitSeq(seq),
                    &row,
                )
                .unwrap();
            match row {
                DeltaRow::CreateVertex { vid, .. } => {
                    assert!(vertices.insert(vid));
                }
                DeltaRow::DeleteVertex { vid, .. } => {
                    assert!(vertices.remove(&vid));
                    edges.retain(|_, (source, _, target)| *source != vid && *target != vid);
                }
                DeltaRow::CreateEdge {
                    eid,
                    src,
                    relation,
                    dst,
                    ..
                } => {
                    assert!(edges.insert(eid, (src, relation, dst)).is_none());
                }
                DeltaRow::DeleteEdge { eid, .. } => {
                    assert!(edges.remove(&eid).is_some());
                }
                _ => {}
            }
            assert_eq!(
                read(&counts, GraphCountSource::Vertices),
                Some(vertices.len() as u64)
            );
            for relation in 1..=3 {
                let relation = RelationId(relation);
                let expected = edges.values().filter(|&&(_, r, _)| r == relation).count() as u64;
                assert_eq!(
                    read(&counts, GraphCountSource::Relation(relation)),
                    Some(expected)
                );
            }
        };
        for slot in 1..=20 {
            apply(
                DeltaRow::CreateVertex {
                    vid: VId(u128::MAX - slot),
                    birth_ordinal: slot as u64,
                    labels: vec![],
                    props: vec![],
                    valid_time: None,
                },
                1,
            );
        }
        for slot in 1..=180 {
            apply(
                DeltaRow::CreateEdge {
                    eid: EId(u128::MAX - slot),
                    birth_ordinal: slot as u64 + 20,
                    src: VId(u128::MAX - (slot % 20 + 1)),
                    relation: RelationId((slot % 3 + 1) as u64),
                    dst: VId(u128::MAX - ((slot * 7) % 20 + 1)),
                    canonical_key: None,
                    props: vec![],
                    valid_time: None,
                },
                2,
            );
        }
        for slot in (1..=180).step_by(3) {
            apply(
                DeltaRow::DeleteEdge {
                    eid: EId(u128::MAX - slot),
                    before_version: ObjectId([0; 32]),
                },
                3,
            );
        }
        // A creation retired in the same commit contributes no live row.
        apply(
            DeltaRow::CreateVertex {
                vid: VId(1),
                birth_ordinal: 201,
                labels: vec![],
                props: vec![],
                valid_time: None,
            },
            4,
        );
        apply(
            DeltaRow::DeleteVertex {
                vid: VId(1),
                before_version: ObjectId([0; 32]),
                sorted_retired_incident_edges: vec![],
            },
            4,
        );
    }
    let vertex = VId(u128::MAX - 1);
    let row = DeltaRow::DeleteVertex {
        vid: vertex,
        before_version: ObjectId([0; 32]),
        sorted_retired_incident_edges: writer.live_incident_edges(vertex),
    };
    counts.apply_before(&writer, &row);
    writer
        .apply(
            (&[3; 32], DatabaseSecurityNamespaceId([7; 32])),
            CommitSeq(5),
            &row,
        )
        .unwrap();
    edges.retain(|_, (source, _, target)| *source != vertex && *target != vertex);
    assert_eq!(read(&counts, GraphCountSource::Vertices), Some(19));
    for relation in 1..=3 {
        let relation = RelationId(relation);
        assert_eq!(
            read(&counts, GraphCountSource::Relation(relation)),
            Some(edges.values().filter(|&&(_, r, _)| r == relation).count() as u64)
        );
    }
    for (pinned, vertex_count, edges) in retained {
        assert_eq!(
            read(&pinned, GraphCountSource::Vertices),
            Some(vertex_count)
        );
        for relation in 1..=3 {
            let relation = RelationId(relation);
            assert_eq!(
                read(&pinned, GraphCountSource::Relation(relation)),
                Some(edges.values().filter(|&&(_, r, _)| r == relation).count() as u64)
            );
        }
    }
}

#[test]
fn overflow_or_missing_delete_image_discards_counts_instead_of_manufacturing_zero() {
    let writer = BlockWriter::new(GraphId(1), BranchId(1), 0);
    let mut counts = ExactCounts::new(true);
    counts.vertices = OnceLock::from(u64::MAX);
    counts.apply_before(
        &writer,
        &DeltaRow::CreateVertex {
            vid: VId(1),
            birth_ordinal: 1,
            labels: vec![],
            props: vec![],
            valid_time: None,
        },
    );
    assert_eq!(read(&counts, GraphCountSource::Vertices), None);
    counts.apply_before(
        &writer,
        &DeltaRow::DeleteEdge {
            eid: EId(1),
            before_version: ObjectId([0; 32]),
        },
    );
    assert_eq!(
        read(&counts, GraphCountSource::Relation(RelationId(1))),
        None
    );
}

#[test]
fn lookup_work_depends_on_relations_not_edges_and_all_events_can_refuse() {
    let counts = ExactCounts::new(true);
    let mut work = 0;
    let mut index = IndexMap::new();
    for relation in 0..MAX_RELATIONS {
        index = index.insert(RelationId(relation as u64), u64::MAX, &mut work);
    }
    let counts = ExactCounts {
        relations: OnceLock::from(Some(index)),
        ..counts
    };
    for relation in [0, 2071, MAX_RELATIONS as u64 - 1, u64::MAX] {
        let mut events = Vec::new();
        let source = GraphCountSource::Relation(RelationId(relation));
        let value = counts
            .lookup(source, &mut |event| {
                events.push(event);
                Ok::<_, ()>(())
            })
            .unwrap();
        assert_eq!(value, Some(if relation == u64::MAX { 0 } else { u64::MAX }));
        assert!(
            events.len() <= 16,
            "lookup is bounded by AVL height, not cardinality"
        );
        assert_eq!(events.last(), Some(&SourceEvent::SnapshotRecord));
        for stop in 1..=events.len() {
            let mut seen = 0;
            assert_eq!(
                counts.lookup(source, &mut |_| {
                    seen += 1;
                    if seen == stop { Err(stop) } else { Ok(()) }
                }),
                Err(stop)
            );
            assert_eq!(seen, stop);
        }
    }
}
