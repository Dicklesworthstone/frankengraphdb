use super::*;
use fgdb_types::{BranchId, CommitSeq, DatabaseSecurityNamespaceId, EId, GraphId, ObjectId, VId};

fn read(counts: &ExactCounts, source: GraphCountSource) -> Option<u64> {
    counts.lookup(source, &mut |_| Ok::<_, ()>(())).unwrap()
}

#[test]
fn cold_unknown_is_never_zero_and_relation_builder_preserves_multiplicity() {
    let counts = ExactCounts::default();
    assert_eq!(read(&counts, GraphCountSource::Vertices), None);
    assert_eq!(read(&counts, GraphCountSource::Edges), None);
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
    assert_eq!(read(&counts, GraphCountSource::Edges), None);
}

#[test]
fn cold_builder_has_a_fixed_relation_bound_and_refuses_before_allocation() {
    let mut builder = RelationBuilder::default();
    assert_eq!(builder.observe(RelationId(1), &mut |_| Err(7)), Err(7));
    assert_eq!(builder.rows.capacity(), 0);
    assert!(builder.rows.is_empty());
    for relation in 0..=MAX_COUNTERS {
        builder
            .observe(RelationId(relation as u64), &mut |_| Ok::<_, ()>(()))
            .unwrap();
    }
    assert!(builder.rows.capacity() <= MAX_COUNTERS);
    let relations = builder.finish(&mut |_| Ok::<_, ()>(())).unwrap();
    assert!(relations.is_none());
    let counts = ExactCounts::default();
    counts.publish_relations(relations);
    counts.publish_edges(MAX_COUNTERS as u64 + 1);
    assert_eq!(
        read(&counts, GraphCountSource::Relation(RelationId(1))),
        None
    );
    assert_eq!(
        read(&counts, GraphCountSource::Edges),
        Some(MAX_COUNTERS as u64 + 1)
    );
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
            assert_eq!(read(&counts, GraphCountSource::Edges), Some(edges.len() as u64));
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
    assert_eq!(read(&counts, GraphCountSource::Edges), Some(edges.len() as u64));
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
        assert_eq!(read(&pinned, GraphCountSource::Edges), Some(edges.len() as u64));
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
    assert_eq!(read(&counts, GraphCountSource::Edges), None);
    counts.edges = OnceLock::from(u64::MAX);
    counts.apply_before(
        &writer,
        &DeltaRow::CreateEdge {
            eid: EId(1),
            birth_ordinal: 2,
            src: VId(1),
            relation: RelationId(1),
            dst: VId(1),
            canonical_key: None,
            props: vec![],
            valid_time: None,
        },
    );
    assert_eq!(read(&counts, GraphCountSource::Edges), None);
}

#[test]
fn wildcard_lookup_is_constant_work_and_every_event_can_refuse() {
    for total in [0, 1, MAX_COUNTERS as u64 + 1, u64::MAX] {
        let counts = ExactCounts::default();
        counts.publish_edges(total);
        let mut events = Vec::new();
        assert_eq!(
            counts
                .lookup(GraphCountSource::Edges, &mut |event| {
                    events.push(event);
                    Ok::<_, ()>(())
                })
                .unwrap(),
            Some(total)
        );
        assert_eq!(events, vec![SourceEvent::Work, SourceEvent::SnapshotRecord]);
        for stop in 1..=events.len() {
            let mut seen = 0;
            assert_eq!(
                counts.lookup(GraphCountSource::Edges, &mut |_| {
                    seen += 1;
                    if seen == stop { Err(stop) } else { Ok(()) }
                }),
                Err(stop)
            );
            assert_eq!(seen, stop);
        }
    }
}

#[test]
fn lookup_work_depends_on_relations_not_edges_and_all_events_can_refuse() {
    let counts = ExactCounts::new(true);
    let mut work = 0;
    let mut index = IndexMap::new();
    for relation in 0..MAX_COUNTERS {
        index = index.insert(RelationId(relation as u64), u64::MAX, &mut work);
    }
    let counts = ExactCounts {
        relations: OnceLock::from(Some(index)),
        ..counts
    };
    for relation in [0, 2071, MAX_COUNTERS as u64 - 1, u64::MAX] {
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

#[test]
fn label_changes_noops_retirements_and_pins_match_independent_membership_sets() {
    use std::collections::{BTreeMap, BTreeSet};

    let mut writer = BlockWriter::new(GraphId(1), BranchId(1), 0);
    let mut counts = ExactCounts::new(true);
    let mut visible: BTreeMap<VId, BTreeSet<LabelId>> = BTreeMap::new();
    let mut pinned = Vec::new();
    let labels: Vec<_> = (1..=9).map(LabelId).chain([LabelId(u64::MAX)]).collect();
    let mut apply = |row: DeltaRow, seq| {
        pinned.push((counts.clone(), visible.clone()));
        counts.apply_before(&writer, &row);
        writer
            .apply(
                (&[3; 32], DatabaseSecurityNamespaceId([7; 32])),
                CommitSeq(seq),
                &row,
            )
            .unwrap();
        match row {
            DeltaRow::CreateVertex { vid, labels, .. } => {
                assert!(visible.insert(vid, labels.into_iter().collect()).is_none());
            }
            DeltaRow::LabelMembership {
                vid, label, after, ..
            } => {
                let labels = visible.get_mut(&vid).unwrap();
                if after {
                    labels.insert(label);
                } else {
                    labels.remove(&label);
                }
            }
            DeltaRow::DeleteVertex { vid, .. } => {
                assert!(visible.remove(&vid).is_some());
            }
            _ => panic!("membership fixture"),
        }
        assert_eq!(
            read(&counts, GraphCountSource::Vertices),
            Some(visible.len() as u64)
        );
        for label in &labels {
            let expected = visible.values().filter(|set| set.contains(label)).count() as u64;
            assert_eq!(
                read(&counts, GraphCountSource::Label(*label)),
                Some(expected)
            );
        }
    };
    for id in 1..=24 {
        apply(
            DeltaRow::CreateVertex {
                vid: VId(u128::MAX - id),
                birth_ordinal: id as u64,
                labels: vec![LabelId(1), LabelId(id as u64 % 5 + 2), LabelId(u64::MAX)],
                props: vec![],
                valid_time: None,
            },
            1,
        );
    }
    for id in 1..=24 {
        for (label, before, after) in [
            (1, true, false),
            (1, false, false),
            (9, false, true),
            (9, true, true),
        ] {
            apply(
                DeltaRow::LabelMembership {
                    vid: VId(u128::MAX - id),
                    label: LabelId(label),
                    before,
                    after,
                },
                2,
            );
        }
    }
    for id in (1..=24).step_by(2) {
        apply(
            DeltaRow::DeleteVertex {
                vid: VId(u128::MAX - id),
                before_version: ObjectId([0; 32]),
                sorted_retired_incident_edges: vec![],
            },
            3,
        );
    }
    apply(
        DeltaRow::CreateVertex {
            vid: VId(1),
            birth_ordinal: 25,
            labels: vec![LabelId(8)],
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
    for (old, visible) in pinned {
        for label in &labels {
            let expected = visible.values().filter(|set| set.contains(label)).count() as u64;
            assert_eq!(read(&old, GraphCountSource::Label(*label)), Some(expected));
        }
    }
}

#[test]
fn label_build_and_lookup_are_bounded_and_every_event_can_refuse() {
    let counts = ExactCounts::default();
    assert_eq!(read(&counts, GraphCountSource::Label(LabelId(1))), None);
    let run = |stop| {
        let counts = ExactCounts::default();
        let mut events = 0;
        let mut control = |_| {
            events += 1;
            if stop == Some(events) {
                Err(events)
            } else {
                Ok(())
            }
        };
        let result = (|| {
            let mut builder = counts.label_builder().unwrap();
            for id in (1..=20).rev().chain(1..=20) {
                builder.observe(LabelId(id), &mut control)?;
            }
            counts.publish_labels(builder.finish(&mut control)?);
            Ok::<_, usize>(())
        })();
        (result, events, counts)
    };
    let (result, events, counts) = run(None);
    assert_eq!(result, Ok(()));
    for id in [0, 1, 13, 20, u64::MAX] {
        let source = GraphCountSource::Label(LabelId(id));
        let mut calls = 0;
        assert_eq!(
            counts
                .lookup(source, &mut |_| {
                    calls += 1;
                    Ok::<_, ()>(())
                })
                .unwrap(),
            Some(if (1..=20).contains(&id) { 2 } else { 0 })
        );
        assert!(calls <= 8);
        for stop in 1..=calls {
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
    for stop in 1..=events {
        let (result, seen, counts) = run(Some(stop));
        assert_eq!(result, Err(stop));
        assert_eq!(seen, stop);
        assert_eq!(read(&counts, GraphCountSource::Label(LabelId(1))), None);
    }
    let mut builder = LabelBuilder::default();
    assert_eq!(builder.observe(LabelId(1), &mut |_| Err(7)), Err(7));
    assert_eq!(builder.rows.capacity(), 0);
    for id in 0..=MAX_COUNTERS {
        builder
            .observe(LabelId(id as u64), &mut |_| Ok::<_, ()>(()))
            .unwrap();
    }
    assert!(builder.rows.capacity() <= MAX_COUNTERS);
    assert!(builder.finish(&mut |_| Ok::<_, ()>(())).unwrap().is_none());
}

#[test]
fn inconsistent_label_before_images_and_overflow_discard_only_label_metadata() {
    let mut writer = BlockWriter::new(GraphId(1), BranchId(1), 0);
    let mut counts = ExactCounts::new(true);
    let create = DeltaRow::CreateVertex {
        vid: VId(1),
        birth_ordinal: 1,
        labels: vec![LabelId(1)],
        props: vec![],
        valid_time: None,
    };
    counts.apply_before(&writer, &create);
    writer
        .apply(
            (&[3; 32], DatabaseSecurityNamespaceId([7; 32])),
            CommitSeq(1),
            &create,
        )
        .unwrap();
    let pinned = counts.clone();
    counts.apply_before(
        &writer,
        &DeltaRow::LabelMembership {
            vid: VId(1),
            label: LabelId(1),
            before: false,
            after: true,
        },
    );
    assert_eq!(read(&counts, GraphCountSource::Label(LabelId(1))), None);
    assert_eq!(read(&counts, GraphCountSource::Vertices), Some(1));
    assert_eq!(
        read(&counts, GraphCountSource::Relation(RelationId(1))),
        Some(0)
    );
    assert_eq!(read(&pinned, GraphCountSource::Label(LabelId(1))), Some(1));

    let mut counts = pinned.clone();
    counts.labels = OnceLock::from(Some(IndexMap::new().insert(LabelId(1), u64::MAX, &mut 0)));
    counts.apply_before(&writer, &create);
    assert_eq!(read(&counts, GraphCountSource::Label(LabelId(1))), None);

    let mut counts = pinned;
    counts.apply_before(
        &writer,
        &DeltaRow::DeleteVertex {
            vid: VId(2),
            before_version: ObjectId([0; 32]),
            sorted_retired_incident_edges: vec![],
        },
    );
    assert_eq!(read(&counts, GraphCountSource::Label(LabelId(1))), None);
}
