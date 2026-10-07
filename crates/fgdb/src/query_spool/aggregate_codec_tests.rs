use super::*;
use asupersync::lab::run_async_under_lab;
use fgdb_types::{PurposeContexts, VId};

#[test]
fn aggregate_envelopes_preserve_all_exact_numeric_boundaries_and_reject_invalid_tags() {
    let row = GraphAggregateRow::from_group_values(
        vec![GraphValue::Vertex(VId(u128::MAX))],
        vec![
            GraphAggregateValue::Count(u64::MAX),
            GraphAggregateValue::Integer(i128::MIN),
            GraphAggregateValue::Integer(i128::MAX),
            GraphAggregateValue::Average(GraphExactAverage::new(-5, 3).unwrap()),
            GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Null)),
        ],
    );
    let encoded = envelope(&row).canonical_bytes().unwrap();
    let decoded = GraphValueRow::decode_canonical(&encoded).unwrap();
    assert_eq!(decode_envelope(&decoded, 1, 5).unwrap(), row);
    assert!(decode_envelope(&decoded, 1, 4).is_err());
    let invalid = GraphValueRow::from_owned_values(vec![GraphValue::List(
        vec![
            GraphValue::Scalar(CanonicalScalar::Int(99)),
            GraphValue::Scalar(CanonicalScalar::Null),
        ]
        .into_boxed_slice(),
    )]);
    assert!(decode_envelope(&invalid, 0, 1).is_err());
}

#[test]
fn structural_admission_counts_large_text_as_bytes_and_checks_recursive_framing() {
    let ((), report) = run_async_under_lab(0x5ba1_1001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let text = GraphValue::Scalar(CanonicalScalar::ucs_basic_text(&"x".repeat(5000)).unwrap());
        let row = GraphValueRow::from_owned_values(vec![GraphValue::Vertex(VId(1)), text]);
        let wire = row.canonical_bytes().unwrap();
        let (predicted, _) = account::encoded_shape(&row, &cx).unwrap();
        assert_eq!(predicted, wire.len());
        let resident = account::decoded(&wire, &cx).unwrap();
        assert!(resident >= 5000 + 2 * size_of::<GraphValue>());
        assert!(
            resident < 20_000,
            "text bytes must not be charged as GraphValue slots"
        );
        let pool = MemoryPool::new(65_536, 0).unwrap();
        let mut work = Work {
            cx: &cx,
            used: 0,
            limit: 100_000,
        };
        let (encoded, charge) = account::encode(&pool, &mut work, &row, 16_384).unwrap();
        assert_eq!(encoded, wire);
        assert!(pool.used() > encoded.len());
        drop(encoded);
        drop(charge);
        assert_eq!(pool.used(), 0);
        let nested = GraphValueRow::from_owned_values(vec![GraphValue::List(
            vec![
                GraphValue::Scalar(CanonicalScalar::Int(1)),
                GraphValue::List(vec![GraphValue::Vertex(VId(2))].into_boxed_slice()),
            ]
            .into_boxed_slice(),
        )]);
        let bytes = nested.canonical_bytes().unwrap();
        assert_eq!(account::encoded_shape(&nested, &cx).unwrap().0, bytes.len());
        assert!(account::decoded(&bytes, &cx).unwrap() >= 4 * size_of::<GraphValue>());
        let mut malformed = bytes.clone();
        malformed.extend_from_slice(&[0]);
        assert!(account::decoded(&malformed, &cx).is_err());
        assert!(account::decoded(&bytes[..bytes.len() - 1], &cx).is_err());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
