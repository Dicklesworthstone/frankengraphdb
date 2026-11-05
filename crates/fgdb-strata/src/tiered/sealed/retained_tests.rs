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

#[test]
fn direct_retained_sealing_keeps_indivisible_property_storage_admission() {
    let blocks = vec![vec![AdjacencyEntry {
        src: VId(1),
        relation: RelationId(1),
        dst: VId(2),
        eid: EId(3),
        created_at: CommitSeq(1),
        retired_at: None,
    }]];
    let properties = vec![Some(BlockProps {
        locators: vec![1],
        rows: vec![vec![(
            PropertyKeyId(1),
            CanonicalScalar::bytes(vec![0xa5; 20_000]).unwrap().into(),
        )]],
    })];
    let ordinary =
        crate::compact::compact_with_props(&blocks, &properties, CommitSeq(1)).unwrap_err();
    assert!(matches!(
        &ordinary,
        crate::root::RootError::PropertyPatch(
            EdgePropertyPatchError::RowExceedsStorageLimit { bytes, limit }
        ) if bytes > limit
    ));
    // Removing temporary block packing cannot let the larger image's property
    // allowance override the durable format's indivisible-row storage limit.
    let limits = SealedLimits {
        max_property_bytes: usize::MAX,
        max_image_bytes: usize::MAX,
        ..SealedLimits::default()
    };
    let mut image_callbacks = 0;
    let result = crate::compact::retained_rows_for_sealing(&blocks, &properties, CommitSeq(1))
        .map_err(SealedError::History)
        .and_then(|rows| {
            image::build_retained(rows, limits, &mut || {
                image_callbacks += 1;
                Ok(())
            })
        });
    let failure = result.map(|_| ()).unwrap_err();
    assert!(matches!(&failure, SealedError::History(_)));
    if let SealedError::History(actual) = failure {
        assert_eq!(actual, ordinary);
    }
    assert_eq!(image_callbacks, 0);
}

#[test]
fn direct_retained_image_admission_precedes_callbacks_and_counts_previous_families() {
    let first = vec![(
        AdjacencyEntry {
            src: VId(1),
            relation: RelationId(1),
            dst: VId(2),
            eid: EId(3),
            created_at: CommitSeq(1),
            retired_at: None,
        },
        Vec::new(),
    )];
    let mut second = first.clone();
    second[0].0.src = VId(4);
    second[0].0.eid = EId(5);
    let limits = SealedLimits {
        max_incidences: 1,
        ..SealedLimits::default()
    };
    let mut builder = image::ImageBuilder::new(limits).unwrap();
    builder
        .push_retained(first.clone(), &mut || Ok(()))
        .unwrap();
    let mut callbacks = 0;
    let result = builder.push_retained(second.clone(), &mut || {
        callbacks += 1;
        Err(SealedError::NonCanonical)
    });
    assert!(matches!(
        result,
        Err(SealedError::Limit {
            resource: "incidences",
            requested: 2,
            limit: 1,
        })
    ));
    assert_eq!(callbacks, 0);
    // Admission refused before changing the already-built descriptor family.
    let actual = builder.finish(&mut || Ok(())).unwrap();
    let expected = image::build_retained(first.clone(), limits, &mut || Ok(())).unwrap();
    assert_eq!(
        wire::encode(&actual, limits, &mut || Ok(())).unwrap(),
        wire::encode(&expected, limits, &mut || Ok(())).unwrap()
    );

    let mut combined = first;
    combined.extend(second);
    let result = image::build_retained(combined, limits, &mut || {
        callbacks += 1;
        Err(SealedError::NonCanonical)
    });
    assert!(matches!(
        result,
        Err(SealedError::Limit {
            resource: "incidences",
            requested: 2,
            limit: 1,
        })
    ));
    assert_eq!(callbacks, 0);

    let mut empty = image::ImageBuilder::new(limits).unwrap();
    assert!(matches!(
        empty.push_retained(Vec::new(), &mut || {
            callbacks += 1;
            Err(SealedError::NonCanonical)
        }),
        Err(SealedError::NonCanonical)
    ));
    assert_eq!(callbacks, 1);
    assert!(matches!(
        image::build_retained(Vec::new(), limits, &mut || {
            callbacks += 1;
            Err(SealedError::NonCanonical)
        }),
        Err(SealedError::NonCanonical)
    ));
    assert_eq!(callbacks, 2);
}

fn validation_fixture(degree: usize, hosted: bool) -> (SealedScope, Image) {
    let scope = SealedScope {
        source_root: PartitionRootVersion(ObjectId([0x46; 32])),
        graph: GraphId(1),
        branch: BranchId(2),
        partition: 0,
        floor: CommitSeq(1),
        publication: CommitSeq(10),
    };
    let mut retained = Vec::new();
    for at in 0..degree {
        retained.push((
            AdjacencyEntry {
                src: VId((1u128 << 127) | 3),
                relation: RelationId(7),
                dst: VId((1u128 << 100) | ((at / 3) as u128 * 257)),
                eid: EId((1u128 << 126) | at as u128),
                created_at: CommitSeq(1 + (at / 3 % 3) as u64),
                retired_at: (at / 7 % 2 == 0).then_some(CommitSeq(8)),
            },
            if hosted && at % 2 != 0 {
                vec![(PropertyKeyId(1), CanonicalScalar::Int(at as i64).into())]
            } else {
                Vec::new()
            },
        ));
    }
    if degree != 0 {
        // The same validation must still prove inline dictionary membership.
        retained.push((
            AdjacencyEntry {
                src: VId((1u128 << 127) | 3),
                relation: RelationId(8),
                dst: VId(u128::MAX - 1),
                eid: EId(u128::MAX),
                created_at: CommitSeq(1),
                retired_at: None,
            },
            Vec::new(),
        ));
    }
    let image = image::build_retained(retained, SealedLimits::default(), &mut || Ok(())).unwrap();
    (scope, image)
}

#[derive(Debug, PartialEq, Eq)]
struct ValidationTrace {
    // Debug preserves the concrete refusal variant and its fields; none of
    // these local admission cases contains a non-comparable external error.
    result: Result<(), String>,
    checkpoints: usize,
}

fn validation_trace(
    image: &Image,
    scope: SealedScope,
    scalar: bool,
    stop: usize,
) -> ValidationTrace {
    let mut checkpoints = 0;
    let mut guard = || {
        checkpoints += 1;
        if checkpoints == stop {
            Err(SealedError::InvalidFloor)
        } else {
            Ok(())
        }
    };
    let limits = SealedLimits::default();
    let result = if scalar {
        image.validate_scalar(scope, limits, &mut guard)
    } else {
        image.validate(scope, limits, &mut guard)
    };
    ValidationTrace {
        result: result.map_err(|error| format!("{error:?}")),
        checkpoints,
    }
}

fn assert_validation_refusal_parity(image: &Image, scope: SealedScope) {
    let reference = validation_trace(image, scope, true, usize::MAX);
    assert_eq!(validation_trace(image, scope, false, usize::MAX), reference);
    for stop in 1..=reference.checkpoints {
        let expected = ValidationTrace {
            result: Err("InvalidFloor".to_owned()),
            checkpoints: stop,
        };
        assert_eq!(validation_trace(image, scope, true, stop), expected);
        assert_eq!(validation_trace(image, scope, false, stop), expected);
    }
}

#[test]
fn sequential_image_validation_matches_scalar_for_tiers_spans_and_property_rows() {
    for degree in [0, 1, 8, 9, 33, 257, 1025] {
        for hosted in [false, true] {
            let (scope, image) = validation_fixture(degree, hosted);
            let reference = validation_trace(&image, scope, true, usize::MAX);
            assert!(reference.result.is_ok(), "degree={degree}, hosted={hosted}");
            assert_eq!(
                validation_trace(&image, scope, false, usize::MAX),
                reference
            );
            if degree <= 257 {
                assert_validation_refusal_parity(&image, scope);
            }
        }
    }
}

#[test]
fn sequential_image_validation_preserves_malformed_refusals_before_every_checkpoint() {
    use fgdb_codec::elias_fano::{EliasFano, EntryLimit};
    use fgdb_codec::identity::IdentityColumn;

    type Mutation = fn(&mut Image, &mut SealedScope);
    let cases: &[(&str, Mutation)] = &[
        ("neighbor outside dictionary", |image, _| {
            let values = vec![image.dictionary.len() as u64; image.runs[0].neighbors.len()];
            image.runs[0].neighbors =
                EliasFano::try_new(&values, EntryLimit::new(values.len())).unwrap();
        }),
        ("unused dictionary member", |image, _| {
            let mut values: Vec<_> = image.dictionary.iter().collect();
            values.push(VId(u128::MAX));
            image.dictionary = IdentityColumn::try_new(
                &values,
                image::identity_limits(values.len(), SealedLimits::default()),
            )
            .unwrap();
        }),
        ("unordered dictionary", |image, _| {
            let mut values: Vec<_> = image.dictionary.iter().collect();
            values.swap(0, 1);
            image.dictionary = IdentityColumn::try_new(
                &values,
                image::identity_limits(values.len(), SealedLimits::default()),
            )
            .unwrap();
        }),
        ("inline neighbor absent from dictionary", |image, _| {
            let mut values: Vec<_> = image.dictionary.iter().collect();
            *values.last_mut().unwrap() = VId(u128::MAX);
            image.dictionary = IdentityColumn::try_new(
                &values,
                image::identity_limits(values.len(), SealedLimits::default()),
            )
            .unwrap();
        }),
        ("unordered identities within one neighbor", |image, _| {
            let mut values: Vec<_> = image.runs[0].edge_ids.iter().collect();
            values.swap(0, 1);
            image.runs[0].edge_ids = IdentityColumn::try_new(
                &values,
                image::identity_limits(values.len(), SealedLimits::default()),
            )
            .unwrap();
        }),
        ("duplicate content statement", |image, _| {
            let mut values: Vec<_> = image.runs[0].edge_ids.iter().collect();
            values[1] = values[0];
            image.runs[0].edge_ids = IdentityColumn::try_new(
                &values,
                image::identity_limits(values.len(), SealedLimits::default()),
            )
            .unwrap();
        }),
        ("missing identity", |image, _| {
            let mut values: Vec<_> = image.runs[0].edge_ids.iter().collect();
            values.pop();
            image.runs[0].edge_ids = IdentityColumn::try_new(
                &values,
                image::identity_limits(values.len(), SealedLimits::default()),
            )
            .unwrap();
        }),
        ("missing locator", |image, _| {
            image.runs[0].locators.pop();
        }),
        ("locator sequence", |image, _| {
            image.runs[0].locators[0] = 2;
        }),
        ("span gap", |image, _| {
            image.runs[0].spans[1].start_row += 1;
        }),
        ("redundant adjacent span", |image, _| {
            let first = image.runs[0].spans[0];
            image.runs[0].spans[1].created_at = first.created_at;
            image.runs[0].spans[1].retired_at = first.retired_at;
        }),
        ("zero birth", |image, _| {
            image.runs[0].spans[0].created_at = CommitSeq(0);
        }),
        ("future birth", |image, _| {
            image.runs[0].spans[0].created_at = CommitSeq(11);
            image.runs[0].spans[0].retired_at = None;
        }),
        ("future retirement", |image, _| {
            image.runs[0].spans[0].retired_at = Some(CommitSeq(11));
        }),
        ("already dropped history", |_, scope| {
            scope.floor = CommitSeq(8);
        }),
        ("missing run", |image, _| {
            image.rows[0].storage = image::Storage::Run(image.runs.len());
        }),
        ("row count mismatch", |image, _| {
            image.rows[0].count += 1;
        }),
        ("unreferenced property row", |image, _| {
            image.properties.push(image.properties[0].clone());
        }),
    ];
    for &(name, mutate) in cases {
        let (mut scope, mut image) = validation_fixture(257, true);
        mutate(&mut image, &mut scope);
        assert!(
            validation_trace(&image, scope, true, usize::MAX)
                .result
                .is_err(),
            "ineffective mutation: {name}"
        );
        assert_validation_refusal_parity(&image, scope);
    }
}

#[test]
#[ignore = "manual paired complete image validation measurement; no timing gate or competitor claim"]
fn benchmark_sequential_complete_image_validation() {
    use std::hint::black_box;
    use std::time::Instant;

    for degree in [4096, 65_536] {
        for hosted in [false, true] {
            let (scope, image) = validation_fixture(degree, hosted);
            let reference = validation_trace(&image, scope, true, usize::MAX);
            assert!(reference.result.is_ok());
            assert_eq!(
                validation_trace(&image, scope, false, usize::MAX),
                reference
            );
            let mut samples = [Vec::new(), Vec::new()];
            for trial in 0..9 {
                for which in [trial % 2, 1 - trial % 2] {
                    // Includes allocation of the used-dictionary bitmap and
                    // all validation, including hosted property admission.
                    // Source construction, fingerprinting, queries and I/O
                    // are outside this explicitly bounded measurement.
                    let started = Instant::now();
                    if which == 0 {
                        black_box(&image)
                            .validate_scalar(scope, SealedLimits::default(), &mut || Ok(()))
                            .unwrap();
                    } else {
                        black_box(&image)
                            .validate(scope, SealedLimits::default(), &mut || Ok(()))
                            .unwrap();
                    }
                    samples[which].push(started.elapsed().as_nanos());
                }
            }
            eprintln!(
                "complete_image_validation degree={degree} hosted={hosted} scalar_ns={:?} sequential_ns={:?}",
                samples[0], samples[1]
            );
        }
    }
}
