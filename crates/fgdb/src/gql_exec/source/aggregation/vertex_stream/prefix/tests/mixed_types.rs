use super::*;
use fgdb_types::{
    CanonicalText, CollationResolver, CollationResolverError, NonBinaryTextBinding, ObjectId,
    TzdbResolver,
};

struct ReverseSpelling;
impl CollationResolver for ReverseSpelling {
    fn artifact_available(&self, _: &ObjectId) -> bool {
        true
    }
    fn canonical_sort_key_len(
        &self,
        _: &NonBinaryTextBinding,
        text: &str,
    ) -> Result<usize, CollationResolverError> {
        Ok(text.len())
    }
    fn write_canonical_sort_key(
        &self,
        _: &NonBinaryTextBinding,
        text: &str,
        output: &mut [u8],
    ) -> Result<usize, CollationResolverError> {
        for (target, byte) in output.iter_mut().zip(text.bytes().rev()) {
            *target = byte;
        }
        Ok(text.len())
    }
    fn canonical_sort_key_matches(
        &self,
        _: &NonBinaryTextBinding,
        text: &str,
        candidate: &[u8],
    ) -> Result<bool, CollationResolverError> {
        Ok(candidate.iter().copied().eq(text.bytes().rev()))
    }
}
impl TzdbResolver for ReverseSpelling {
    fn contains_tzdb(&self, _: &ObjectId) -> bool {
        false
    }
    fn canonical_utc_offset_seconds(&self, _: &ObjectId, _: &str, _: i128) -> Option<i32> {
        None
    }
}
fn collated(value: &str) -> CanonicalScalar {
    let binding = NonBinaryTextBinding::new(
        ObjectId([1; 32]),
        ObjectId([2; 32]),
        ObjectId([3; 32]),
        ObjectId([4; 32]),
    );
    CanonicalScalar::Text(CanonicalText::new_non_binary(value, binding, &ReverseSpelling).unwrap())
}
fn resolved_patch(mut rows: Vec<VertexRow>) -> VertexPatchRows {
    rows.sort_by_key(|row| (row.vid, row.created_at));
    fgdb_strata::vertex::decode_patch_with_resolver(&encode_patch(&rows).unwrap(), &ReverseSpelling)
        .unwrap()
}

#[test]
fn numeric_history_and_other_properties_do_not_disable_selective_prefixes() {
    let mut rows: Vec<_> = (0..2048_u128)
        .map(|at| row(VId(at), 1, CanonicalScalar::Int(at as i64)))
        .collect();
    rows[0].retired_at = Some(CommitSeq(2));
    let mut unrelated = row(VId(10_000), 1, CanonicalScalar::Null);
    unrelated.props = vec![(PropertyKeyId(8), collated("Ab").into())];
    rows.push(unrelated);
    rows.push(row(
        VId(u128::MAX),
        1,
        CanonicalScalar::bytes(vec![1]).unwrap(),
    ));
    let patches = vec![
        resolved_patch(rows),
        resolved_patch(vec![
            row(VId(0), 2, text("Ab now")),
            row(VId(3000), 99, text("Ab future")),
        ]),
    ];
    let index = PropertyEqualityIndex::build(&patches);
    let expected: Vec<_> = patches
        .iter()
        .flat_map(|patch| patch.iter())
        .filter(|row| {
            row.props.iter().any(|(key, value)| {
                *key == KEY
                    && matches!(value.as_scalar(), Some(CanonicalScalar::Text(value))
                if value.as_str().starts_with("Ab"))
            })
        })
        .map(|row| row.vid)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut source = PrefixCandidates::new(KEY, "Ab").unwrap();
    let mut output = Vec::new();
    let mut work = 0;
    let mut scratch = 0;
    drain(&mut source, &index, &mut None, &mut output, &mut |event| {
        match event {
            VertexScanEvent::Work => work += 1,
            VertexScanEvent::ScratchEntry => scratch += 1,
        }
        Ok::<_, ()>(())
    })
    .unwrap();
    assert_eq!(output, expected);
    assert_eq!(output, vec![VId(0), VId(3000)]);
    assert!(matches!(source.frontier, Frontier::Buckets(_)));
    assert_eq!(scratch, 2);
    assert!(
        work < 256,
        "prefix inspected unrelated historical values: {work}"
    );
}

#[test]
fn each_mixed_or_collated_control_refusal_keeps_a_complete_retry() {
    for alternate_binding in [false, true] {
        let mut rows = vec![
            row(VId(0), 1, CanonicalScalar::Int(0)),
            row(VId(1), 1, text("Ab")),
            row(VId(2), 1, CanonicalScalar::bytes(vec![1]).unwrap()),
            row(VId(3), 1, text("outside")),
            row(VId(u128::MAX), 1, CanonicalScalar::Null),
        ];
        let changed = if alternate_binding {
            // A future alternate binding still belongs to this generation's
            // historical guard. Keep all native candidates in that fallback.
            vec![row(VId(7), 99, collated("Ab future"))]
        } else {
            rows[0].retired_at = Some(CommitSeq(2));
            vec![row(VId(0), 2, text("Ab next"))]
        };
        let patches = vec![resolved_patch(rows), resolved_patch(changed)];
        let index = PropertyEqualityIndex::build(&patches);
        let expected: Vec<_> = patches
            .iter()
            .flat_map(|patch| patch.iter())
            .filter(|row| {
                alternate_binding
                    || matches!(row.props[0].1.as_scalar(), Some(CanonicalScalar::Text(value))
                        if value.as_str().starts_with("Ab"))
            })
            .map(|row| row.vid)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut complete = PrefixCandidates::new(KEY, "Ab").unwrap();
        let mut total = 0;
        let mut output = Vec::new();
        drain(&mut complete, &index, &mut None, &mut output, &mut |_| {
            total += 1;
            Ok::<_, usize>(())
        })
        .unwrap();
        assert_eq!(output, expected);
        assert_eq!(
            matches!(complete.frontier, Frontier::Histories),
            alternate_binding
        );
        assert!(!complete.accepts(&CanonicalScalar::Int(0)));
        assert!(!complete.accepts(&CanonicalScalar::Null));
        assert!(!complete.accepts(&collated("outside")));
        assert!(complete.accepts(&collated("Ab value")));
        for stop in 1..=total {
            let mut source = PrefixCandidates::new(KEY, "Ab").unwrap();
            let mut after = None;
            let mut output = Vec::new();
            let mut seen = 0;
            assert_eq!(
                drain(&mut source, &index, &mut after, &mut output, &mut |_| {
                    seen += 1;
                    if seen == stop { Err(stop) } else { Ok(()) }
                }),
                Err(stop),
            );
            assert_eq!(seen, stop);
            assert_eq!(output, expected[..output.len()]);
            drain(&mut source, &index, &mut after, &mut output, &mut |_| {
                Ok::<_, usize>(())
            })
            .unwrap();
            assert_eq!(
                output, expected,
                "alternate={alternate_binding}, stop={stop}"
            );
        }
    }
}
