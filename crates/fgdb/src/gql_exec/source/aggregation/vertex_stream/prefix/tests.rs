use super::*;
use fgdb_strata::vertex::{VertexPatchRows, VertexRow, decode_patch, encode_patch};
use fgdb_types::CommitSeq;
use std::collections::BTreeSet;

const KEY: PropertyKeyId = PropertyKeyId(7);

fn row(vid: VId, at: u64, value: CanonicalScalar) -> VertexRow {
    VertexRow {
        vid,
        birth_ordinal: vid.0 as u64,
        created_at: CommitSeq(at),
        retired_at: None,
        labels: vec![],
        props: vec![(KEY, value.into())],
    }
}

fn patch(mut rows: Vec<VertexRow>) -> VertexPatchRows {
    rows.sort_by_key(|row| (row.vid, row.created_at));
    decode_patch(&encode_patch(&rows).unwrap()).unwrap()
}

fn text(value: &str) -> CanonicalScalar {
    CanonicalScalar::ucs_basic_text(value).unwrap()
}

fn drain<C>(
    source: &mut PrefixCandidates,
    index: &PropertyEqualityIndex,
    after: &mut Option<VId>,
    output: &mut Vec<VId>,
    control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
) -> Result<(), C> {
    loop {
        match source.next(index, *after, control)? {
            SourceStep::Item(vid) => {
                assert!(after.is_none_or(|previous| previous < vid));
                *after = Some(vid);
                output.push(vid);
            }
            SourceStep::Skipped(_) => unreachable!("prefix histories use the ordinary residual"),
            SourceStep::Exhausted => return Ok(()),
        }
    }
}

#[test]
fn every_prefix_refusal_preserves_the_full_ordered_retry_at_the_capacity_boundary() {
    for distinct in [4, range::MAX_RANGE_HEADS, range::MAX_RANGE_HEADS + 1] {
        let mut first = Vec::new();
        let mut changed = Vec::new();
        for at in 0..distinct {
            for copy in 0..2 {
                let vid = VId((at * 2 + copy) as u128);
                first.push(row(vid, 1, text(&format!("Ad{at}"))));
                if copy == 0 {
                    changed.push(row(vid, 2, text(&format!("Ad{}", (at + 1) % distinct))));
                }
            }
        }
        first.push(row(VId(u128::MAX), 1, text("outside")));
        let patches = vec![patch(first), patch(changed)];
        let index = PropertyEqualityIndex::build(&patches);
        let indexed = distinct <= range::MAX_RANGE_HEADS;
        let expected: Vec<_> = patches
            .iter()
            .flat_map(|patch| patch.iter())
            .filter(|row| {
                !indexed
                    || matches!(row.props[0].1.as_scalar(), Some(CanonicalScalar::Text(value))
                if value.as_str().starts_with("Ad"))
            })
            .map(|row| row.vid)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut source = PrefixCandidates::new(KEY, "Ad").unwrap();
        let mut total = 0;
        let mut scratch = 0;
        let mut full = Vec::new();
        drain(&mut source, &index, &mut None, &mut full, &mut |event| {
            total += 1;
            scratch += usize::from(matches!(event, VertexScanEvent::ScratchEntry));
            Ok::<_, usize>(())
        })
        .unwrap();
        assert_eq!(full, expected);
        assert_eq!(scratch, if indexed { distinct } else { 0 });
        assert_eq!(matches!(source.frontier, Frontier::Buckets(_)), indexed);
        for stop in 1..=total {
            let mut source = PrefixCandidates::new(KEY, "Ad").unwrap();
            let mut after = None;
            let mut output = Vec::new();
            let mut seen = 0;
            let result = drain(&mut source, &index, &mut after, &mut output, &mut |_| {
                seen += 1;
                if seen == stop { Err(stop) } else { Ok(()) }
            });
            assert_eq!(result, Err(stop), "distinct={distinct}, stop={stop}");
            assert_eq!(seen, stop);
            assert_eq!(output, expected[..output.len()]);
            drain(&mut source, &index, &mut after, &mut output, &mut |_| {
                Ok::<_, usize>(())
            })
            .unwrap();
            assert_eq!(output, expected, "distinct={distinct}, stop={stop}");
        }
    }
}

#[test]
fn prefix_unknown_types_and_unicode_successors_preserve_the_storage_domain() {
    let spellings = [
        "",
        "Ad",
        "Ada",
        "B",
        "é",
        "\u{d7ff}",
        "\u{d7ff}tail",
        "\u{e000}",
        "\u{10ffff}",
        "\u{10ffff}tail",
    ];
    let first: Vec<_> = spellings
        .iter()
        .enumerate()
        .map(|(at, spelling)| row(VId(at as u128), 1, text(spelling)))
        .chain([row(VId(u128::MAX), 1, CanonicalScalar::Null)])
        .collect();
    let patches = vec![patch(first)];
    let index = PropertyEqualityIndex::build(&patches);
    for prefix in ["", "Ad", "é", "\u{d7ff}", "\u{10ffff}", "absent"] {
        let mut source = PrefixCandidates::new(KEY, prefix).unwrap();
        let expected: Vec<_> = spellings
            .iter()
            .enumerate()
            .filter(|(_, value)| value.starts_with(prefix))
            .map(|(at, _)| VId(at as u128))
            .collect();
        let mut output = Vec::new();
        drain(&mut source, &index, &mut None, &mut output, &mut |_| {
            Ok::<_, ()>(())
        })
        .unwrap();
        assert_eq!(output, expected, "prefix={prefix:?}");
    }
    for incompatible in [
        CanonicalScalar::Int(0),
        CanonicalScalar::Bool(false),
        CanonicalScalar::bytes(vec![1, 2]).unwrap(),
    ] {
        let patches = vec![patch(vec![
            row(VId(0), 1, incompatible.clone()),
            row(VId(1), 1, text("Ab")),
            row(VId(u128::MAX), 1, text("outside")),
        ])];
        let index = PropertyEqualityIndex::build(&patches);
        let mut source = PrefixCandidates::new(KEY, "A").unwrap();
        let mut output = Vec::new();
        drain(&mut source, &index, &mut None, &mut output, &mut |_| {
            Ok::<_, ()>(())
        })
        .unwrap();
        assert_eq!(output, vec![VId(1)]);
        assert!(matches!(source.frontier, Frontier::Buckets(_)));
        assert!(!source.accepts(&incompatible));
        assert!(!source.accepts(&CanonicalScalar::Null));
        assert!(!source.accepts(&text("outside")));
    }
}

mod mixed_types;
