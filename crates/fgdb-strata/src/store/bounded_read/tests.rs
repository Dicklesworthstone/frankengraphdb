use super::*;
use crate::root::{BlockRef, PartitionRoot, PatchRef};
use fgdb_types::{BranchId, GraphId, ObjectId};

fn root(blocks: usize, patches: usize) -> PartitionRoot {
    PartitionRoot {
        graph: GraphId(1),
        branch: BranchId(1),
        partition: 0,
        published_at: CommitSeq(1),
        blocks: (0..blocks).map(|_| BlockRef {
            block_id: ObjectId([1; 32]),
            first_seq: CommitSeq(1),
            last_seq: CommitSeq(1),
        }).collect(),
        vertex_patches: (0..patches).map(|_| PatchRef {
            patch_id: ObjectId([2; 32]),
            first_seq: CommitSeq(1),
            last_seq: CommitSeq(1),
        }).collect(),
    }
}

#[test]
fn reference_admission_precedes_any_object_event() {
    let source = root(2, 3);
    for (limits, expected) in [
        (RootReadLimits { max_blocks: 1, ..RootReadLimits::default() }, "source blocks"),
        (RootReadLimits { max_vertex_patches: 2, ..RootReadLimits::default() }, "source vertex patches"),
    ] {
        assert!(matches!(Admission::new(limits, &source),
            Err(RootReadError::Limit { resource, .. }) if resource == expected));
    }
    let zero = RootReadLimits {
        max_root_bytes: 0,
        max_source_bytes: 0,
        max_blocks: 0,
        max_vertex_patches: 0,
        max_incidences: 0,
        max_vertex_versions: 0,
    };
    let mut admission = Admission::new(zero, &root(0, 0)).unwrap();
    assert!(matches!(admission.observe(RootReadEvent::ObjectStart),
        Err(RootReadError::Limit { resource: "source encoded bytes", requested: 1, limit: 0 })));
}

#[test]
fn exact_byte_ceiling_stops_before_opening_the_next_object() {
    let limits = RootReadLimits { max_source_bytes: 11, ..RootReadLimits::default() };
    let mut admission = Admission::new(limits, &root(0, 0)).unwrap();
    admission.observe(RootReadEvent::ObjectStart).unwrap();
    admission.observe(RootReadEvent::SourceBytes(4)).unwrap();
    admission.observe(RootReadEvent::ObjectStart).unwrap();
    admission.observe(RootReadEvent::SourceBytes(7)).unwrap();
    assert_eq!(admission.bytes, 11);
    assert!(matches!(admission.observe(RootReadEvent::ObjectStart),
        Err(RootReadError::Limit { resource: "source encoded bytes", requested: 12, limit: 11 })));
    assert_eq!(admission.bytes, 11);
}

#[test]
fn every_record_and_repeated_object_visit_consumes_its_own_allowance() {
    let limits = RootReadLimits {
        max_source_bytes: 6,
        max_incidences: 4,
        max_vertex_versions: 5,
        ..RootReadLimits::default()
    };
    let mut admission = Admission::new(limits, &root(0, 0)).unwrap();
    // Identical-sized/identical-identity visits are not de-duplicated by budget.
    for _ in 0..3 { admission.observe(RootReadEvent::SourceBytes(2)).unwrap(); }
    admission.observe(RootReadEvent::Incidences(3)).unwrap();
    admission.observe(RootReadEvent::Incidences(1)).unwrap();
    admission.observe(RootReadEvent::VertexVersions(2)).unwrap();
    admission.observe(RootReadEvent::VertexVersions(3)).unwrap();
    for (event, resource, requested, limit) in [
        (RootReadEvent::SourceBytes(2), "source encoded bytes", 8, 6),
        (RootReadEvent::Incidences(1), "source incidences", 5, 4),
        (RootReadEvent::VertexVersions(1), "source vertex versions", 6, 5),
    ] {
        assert!(matches!(admission.observe(event), Err(RootReadError::Limit {
            resource: actual, requested: count, limit: cap,
        }) if actual == resource && count == requested && cap == limit));
    }
    assert_eq!((admission.bytes, admission.incidences, admission.vertex_versions), (6, 4, 5));
}

#[test]
fn chunk_boundaries_do_not_change_record_or_byte_admission() {
    for maximum in 0..=16 {
        for first in 0..=16 {
            for second in 0..=16 {
                let mut admission = Admission::new(RootReadLimits {
                    max_source_bytes: maximum,
                    max_incidences: maximum,
                    max_vertex_versions: maximum,
                    ..RootReadLimits::default()
                }, &root(0, 0)).unwrap();
                for family in 0..3 {
                    let event = |count| match family {
                        0 => RootReadEvent::SourceBytes(count),
                        1 => RootReadEvent::Incidences(count),
                        _ => RootReadEvent::VertexVersions(count),
                    };
                    let accepted = admission.observe(event(first)).is_ok();
                    assert_eq!(accepted, first <= maximum);
                    if accepted {
                        assert_eq!(admission.observe(event(second)).is_ok(), first + second <= maximum);
                    }
                }
            }
        }
    }
}

#[test]
fn overflow_refuses_instead_of_wrapping_a_source_allowance() {
    let mut admission = Admission::new(RootReadLimits {
        max_source_bytes: usize::MAX,
        max_incidences: usize::MAX,
        max_vertex_versions: usize::MAX,
        ..RootReadLimits::default()
    }, &root(0, 0)).unwrap();
    for event in [RootReadEvent::SourceBytes(usize::MAX),
        RootReadEvent::Incidences(usize::MAX), RootReadEvent::VertexVersions(usize::MAX)] {
        admission.observe(event).unwrap();
    }
    for event in [RootReadEvent::ObjectStart, RootReadEvent::SourceBytes(1),
        RootReadEvent::Incidences(1), RootReadEvent::VertexVersions(1)] {
        assert!(matches!(admission.observe(event), Err(RootReadError::SizeOverflow)));
    }
}
