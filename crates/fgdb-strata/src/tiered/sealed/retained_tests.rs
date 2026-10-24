//! Retained memory is capacity ownership, not encoded size or build peak.
use super::*;
use crate::edge_props::BlockProps;
use fgdb_delta_types::PropertyKeyId;
use fgdb_types::{CanonicalList, CanonicalPropertyValue, CanonicalScalar, EId, ObjectId};

fn source() -> (SealedScope, Image) {
    let mut entries: Vec<_> = (0..33)
        .map(|at| AdjacencyEntry {
            src: VId(1),
            relation: RelationId(1),
            dst: VId(at + 10),
            eid: EId(at + 100),
            created_at: CommitSeq(1),
            retired_at: None,
        })
        .collect();
    entries.push(AdjacencyEntry {
        src: VId(2),
        relation: RelationId(1),
        dst: VId(3),
        eid: EId(500),
        created_at: CommitSeq(1),
        retired_at: None,
    });
    let props = BlockProps {
        // Property patches are positional: each propertied incidence owns the
        // next row exactly once, even when the row values happen to be equal.
        locators: (1..=entries.len())
            .map(|at| u8::try_from(at).unwrap())
            .collect(),
        rows: vec![vec![(PropertyKeyId(1), CanonicalScalar::Int(1).into())]; entries.len()],
    };
    let image = image::build(
        crate::compact::Compaction {
            blocks: vec![entries],
            block_props: vec![Some(props)],
            dropped: 0,
            superseded: 0,
        },
        SealedLimits::default(),
        &mut || Ok(()),
    )
    .unwrap();
    (
        SealedScope {
            source_root: PartitionRootVersion(ObjectId([0x73; 32])),
            graph: GraphId(1),
            branch: BranchId(1),
            partition: 0,
            floor: CommitSeq(1),
            publication: CommitSeq(1),
        },
        image,
    )
}

fn extra_capacity<T>(values: &mut Vec<T>) -> usize {
    let before = values.capacity();
    values.reserve_exact(before + 17);
    let added = values.capacity() - before;
    assert!(added > 0);
    added * size_of::<T>()
}

#[test]
fn retained_image_counts_every_arena_and_nested_property_spare_capacity() {
    let (scope, mut image) = source();
    macro_rules! grow {
        ($values:expr) => {{
            let before = image.retained_bytes(&mut || Ok(())).unwrap();
            let added = extra_capacity(&mut $values);
            assert_eq!(
                image.retained_bytes(&mut || Ok(())).unwrap(),
                before + added
            );
        }};
    }
    grow!(image.rows);
    grow!(image.inlines);
    grow!(image.runs);
    grow!(image.properties);
    grow!(image.properties[0]);
    grow!(image.runs[0].spans);
    grow!(image.runs[0].locators);
    let before = image.retained_bytes(&mut || Ok(())).unwrap();
    let mut payload = Vec::with_capacity(8192);
    payload.extend_from_slice(b"payload");
    let payload_bytes = payload.capacity();
    let mut children = Vec::with_capacity(29);
    children.push(CanonicalScalar::bytes(payload).unwrap().into());
    let child_bytes = children.capacity() * size_of::<CanonicalPropertyValue>();
    let mut outer = Vec::with_capacity(31);
    outer.push(CanonicalPropertyValue::List(
        CanonicalList::try_new(children).unwrap(),
    ));
    let outer_bytes = outer.capacity() * size_of::<CanonicalPropertyValue>();
    image.properties[0][0].1 = CanonicalPropertyValue::List(CanonicalList::try_new(outer).unwrap());
    let expected = before + payload_bytes + child_bytes + outer_bytes;
    assert_eq!(image.retained_bytes(&mut || Ok(())).unwrap(), expected);
    let partition =
        SealedPartition::finish(scope, image, SealedLimits::default(), &mut || Ok(())).unwrap();
    assert_eq!(partition.charged_resident_bytes(), expected);
    assert_eq!(partition.clone().charged_resident_bytes(), expected);
    assert!(partition.charged_resident_bytes() > partition.anchor().encoded_bytes());
    let encoded = wire::encode(&partition.image, SealedLimits::default(), &mut || Ok(())).unwrap();
    let reloaded = SealedPartition::reload_inner(
        partition.anchor(),
        &encoded,
        SealedLimits::default(),
        None,
        &mut || Ok(()),
    )
    .unwrap();
    assert_eq!(
        reloaded.charged_resident_bytes(),
        reloaded.image.retained_bytes(&mut || Ok(())).unwrap()
    );
    assert!(
        reloaded.charged_resident_bytes() < expected,
        "reload does not recreate spare allocation capacity"
    );
}

#[test]
fn retained_image_measurement_reaches_live_control_at_every_owned_property_node() {
    let (_, mut image) = source();
    image.properties[0][0].1 = CanonicalPropertyValue::List(
        CanonicalList::try_new(vec![
            CanonicalScalar::Int(1).into(),
            CanonicalPropertyValue::List(
                CanonicalList::try_new(vec![CanonicalScalar::Int(2).into()]).unwrap(),
            ),
        ])
        .unwrap(),
    );
    let mut calls = 0;
    image
        .retained_bytes(&mut || {
            calls += 1;
            Ok(())
        })
        .unwrap();
    assert!(calls >= 7);
    for stop in 1..=calls {
        let mut reached = 0;
        assert!(matches!(
            image.retained_bytes(&mut || {
                reached += 1;
                if reached == stop {
                    Err(SealedError::InvalidFloor)
                } else {
                    Ok(())
                }
            }),
            Err(SealedError::InvalidFloor)
        ));
        assert_eq!(reached, stop);
    }
}
