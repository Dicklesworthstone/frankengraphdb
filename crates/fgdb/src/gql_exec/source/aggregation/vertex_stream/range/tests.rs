use super::*;
use fgdb_gql::algebra::IntegerComparison as C;
use fgdb_strata::vertex::{VertexRow, decode_patch, encode_patch};
use fgdb_types::{CanonicalF64, CommitSeq};
use std::collections::BTreeSet;

const KEY: PropertyKeyId = PropertyKeyId(7);

fn float(value: f64) -> CanonicalScalar {
    CanonicalScalar::Float(CanonicalF64::new(value))
}

fn row(vid: VId, at: u64, value: CanonicalScalar) -> VertexRow {
    VertexRow {
        vid,
        birth_ordinal: vid.0 as u64,
        created_at: CommitSeq(at),
        retired_at: None,
        labels: vec![],
        props: vec![
            (PropertyKeyId(6), CanonicalScalar::Int(0).into()),
            (KEY, value.into()),
            (PropertyKeyId(8), CanonicalScalar::Int(0).into()),
        ],
    }
}

fn patch(mut rows: Vec<VertexRow>) -> VertexPatchRows {
    rows.sort_by_key(|row| (row.vid, row.created_at));
    decode_patch(&encode_patch(&rows).unwrap()).unwrap()
}

fn bounds(constraints: &[(CanonicalScalar, C)]) -> numeric::NumericBounds {
    let mut bounds = numeric::NumericBounds::new(KEY);
    for (value, comparison) in constraints {
        bounds.constrain(value, *comparison);
    }
    bounds
}

fn matches(value: &CanonicalScalar, constraints: &[(CanonicalScalar, C)]) -> bool {
    constraints
        .iter()
        .all(|(literal, comparison)| comparison.accepts_scalar_pair(Some(value), Some(literal)))
}

fn oracle(patches: &[VertexPatchRows], constraints: &[(CanonicalScalar, C)]) -> Vec<VId> {
    patches
        .iter()
        .flat_map(|patch| patch.iter())
        .filter(|row| {
            row.props
                .iter()
                .find(|(key, _)| *key == KEY)
                .is_some_and(|(_, value)| {
                    value
                        .as_scalar()
                        .is_some_and(|value| matches(value, constraints))
                })
        })
        .map(|row| row.vid)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn drain<C>(
    source: &mut RangeCandidates,
    index: &PropertyEqualityIndex,
    patches: &[VertexPatchRows],
    after: &mut Option<VId>,
    output: &mut Vec<VId>,
    control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
) -> Result<(), C> {
    loop {
        let next = source.next(index, patches, *after, control)?;
        match next {
            SourceStep::Item(vid) | SourceStep::Skipped(vid) => {
                assert!(after.is_none_or(|previous| previous < vid));
                *after = Some(vid);
                if matches!(next, SourceStep::Item(_)) {
                    output.push(vid);
                }
            }
            SourceStep::Exhausted => return Ok(()),
        }
    }
}

#[test]
fn both_range_strategies_match_an_independent_historical_statement_union() {
    let mut values = vec![
        CanonicalScalar::Int(i64::MIN),
        float(-9_223_372_036_854_775_808.0),
        CanonicalScalar::Int(i64::MAX),
        float(9_223_372_036_854_775_808.0),
        CanonicalScalar::Int(9_007_199_254_740_993),
        float(9_007_199_254_740_992.0),
        float(f64::NEG_INFINITY),
        float(f64::INFINITY),
        float(f64::NAN),
        float(-0.0),
        CanonicalScalar::Null,
        CanonicalScalar::Bool(false),
        CanonicalScalar::ucs_basic_text("7").unwrap(),
    ];
    for value in -40..=40 {
        values.push(CanonicalScalar::Int(value));
        values.push(float(value as f64 + 0.5));
    }
    let first: Vec<_> = values
        .iter()
        .enumerate()
        .map(|(at, value)| {
            let vid = match at {
                0 => VId(0),
                1 => VId(u128::MAX),
                _ => VId((at as u128 * 37) % 997 + 1),
            };
            row(vid, 1, value.clone())
        })
        .collect();
    let mut newer = Vec::new();
    for (at, original) in first.iter().enumerate().step_by(3) {
        let mut retired = original.clone();
        retired.retired_at = Some(CommitSeq(2));
        newer.push(retired);
        newer.push(row(
            original.vid,
            2,
            values[(at + 17) % values.len()].clone(),
        ));
    }
    // A future-only identity and restated old values must remain candidates;
    // MVCC admission belongs to the enclosing source, not this selector.
    newer.push(row(VId(10_000), 99, CanonicalScalar::Int(7)));
    let patches = vec![patch(first), patch(newer)];
    let index = PropertyEqualityIndex::build(&patches);
    let constraints = [
        vec![
            (CanonicalScalar::Int(-7), C::GreaterOrEqual),
            (float(7.5), C::LessOrEqual),
        ],
        vec![
            (float(f64::NEG_INFINITY), C::GreaterOrEqual),
            (float(f64::NAN), C::LessOrEqual),
        ],
        vec![(
            CanonicalScalar::Int(9_007_199_254_740_993),
            C::GreaterOrEqual,
        )],
        vec![
            (CanonicalScalar::Int(i64::MAX), C::LessOrEqual),
            (float(9_223_372_036_854_775_808.0), C::Greater),
        ],
        vec![(float(f64::NAN), C::Greater)],
        vec![
            (float(-0.0), C::Greater),
            (CanonicalScalar::Int(0), C::LessOrEqual),
        ],
    ];
    for constraints in &constraints {
        let expected = oracle(&patches, constraints);
        let query_bounds = bounds(constraints);
        for value in &values {
            assert_eq!(
                query_bounds.accepts(value),
                matches(value, constraints),
                "value={value:?}, constraints={constraints:?}",
            );
        }
        for force_histories in [false, true] {
            let mut source = RangeCandidates::new(bounds(constraints));
            if force_histories {
                source.frontier = Frontier::Histories;
            }
            let mut output = Vec::new();
            drain(
                &mut source,
                &index,
                &patches,
                &mut None,
                &mut output,
                &mut |_| Ok::<_, ()>(()),
            )
            .unwrap();
            assert_eq!(
                output, expected,
                "forced history={force_histories}, constraints={constraints:?}",
            );
        }
    }
}

#[test]
fn every_range_control_refusal_preserves_a_complete_ordered_retry() {
    for distinct in [4, MAX_RANGE_HEADS + 1] {
        let mut originals = Vec::new();
        let mut updates = Vec::new();
        for at in 0..distinct {
            for copy in 0..2 {
                let vid = VId((at * 2 + copy) as u128);
                originals.push(row(vid, 1, CanonicalScalar::Int(at as i64)));
                if copy == 0 {
                    updates.push(row(vid, 2, float(((at + 1) % distinct) as f64)));
                }
            }
        }
        originals.push(row(VId(u128::MAX), 1, CanonicalScalar::Null));
        let patches = vec![patch(originals), patch(updates)];
        let index = PropertyEqualityIndex::build(&patches);
        let constraints = [(CanonicalScalar::Int(0), C::GreaterOrEqual)];
        let expected = oracle(&patches, &constraints);
        let mut full = RangeCandidates::new(bounds(&constraints));
        let mut total = 0;
        let mut output = Vec::new();
        drain(
            &mut full,
            &index,
            &patches,
            &mut None,
            &mut output,
            &mut |_| {
                total += 1;
                Ok::<_, usize>(())
            },
        )
        .unwrap();
        assert_eq!(output, expected);
        for stop in 1..=total {
            let mut source = RangeCandidates::new(bounds(&constraints));
            let mut after = None;
            let mut output = Vec::new();
            let mut seen = 0;
            let result = drain(
                &mut source,
                &index,
                &patches,
                &mut after,
                &mut output,
                &mut |_| {
                    seen += 1;
                    if seen == stop { Err(stop) } else { Ok(()) }
                },
            );
            assert_eq!(result, Err(stop), "distinct={distinct}, stop={stop}");
            assert_eq!(seen, stop);
            assert_eq!(output, expected[..output.len()]);
            drain(
                &mut source,
                &index,
                &patches,
                &mut after,
                &mut output,
                &mut |_| Ok::<_, usize>(()),
            )
            .unwrap();
            assert_eq!(output, expected, "distinct={distinct}, stop={stop}");
        }
    }
}

#[test]
fn wide_ranges_bound_setup_and_examine_one_identity_per_cooperative_step() {
    let constraints = [(CanonicalScalar::Int(0), C::GreaterOrEqual)];
    let mut first_work = Vec::new();
    for count in [128, 4096] {
        let mut rows = vec![row(VId(0), 1, CanonicalScalar::Bool(false))];
        rows.extend((0..count).map(|at| row(VId(at + 1), 1, CanonicalScalar::Int(at as i64))));
        // One patch holds at most MAX_PATCH_ROWS rows (a format ceiling), so
        // the history spans as many patches as it needs.
        let patches: Vec<_> = rows
            .chunks(fgdb_strata::vertex::MAX_PATCH_ROWS as usize)
            .map(|chunk| patch(chunk.to_vec()))
            .collect();
        let index = PropertyEqualityIndex::build(&patches);
        let mut source = RangeCandidates::new(bounds(&constraints));
        let mut work = 0;
        let mut scratch = 0;
        let step = source
            .next(&index, &patches, None, &mut |event| {
                match event {
                    VertexScanEvent::Work => work += 1,
                    VertexScanEvent::ScratchEntry => scratch += 1,
                }
                Ok::<_, ()>(())
            })
            .unwrap();
        assert!(matches!(step, SourceStep::Skipped(VId(0))));
        assert!(matches!(source.frontier, Frontier::Histories));
        assert_eq!(scratch, 0, "wide ranges allocate no discarded frontier");
        assert!(work < 512, "a first step inspected the full range: {work}");
        first_work.push(work);
        assert!(matches!(
            source.next(&index, &patches, Some(VId(0)), &mut |_| Ok::<_, ()>(())),
            Ok(SourceStep::Item(VId(1))),
        ));
    }
    assert!(
        first_work[1] <= first_work[0] + 64,
        "setup must grow with AVL depth, not matching value count: {first_work:?}",
    );
}
