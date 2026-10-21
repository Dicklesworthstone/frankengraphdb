//! Structural group admission and the shared checked row conversion contract.
use fgdb_delta_types::PropertyKeyId;
use fgdb_gql::algebra::{GraphColumn, GraphPatternBuilder, GraphValue, GraphValueOrder};
use fgdb_gql::spill_set::SpillSetAggregateRowError;
use fgdb_gql::{
    GraphAggregate, GraphAggregateError, GraphAggregateRow, GraphAggregateValue, GraphExactAverage,
    GraphIntegerErrorKind, PreparedGraphSet,
};
use fgdb_types::CanonicalScalar;

fn input() -> PreparedGraphSet {
    let mut graph = GraphPatternBuilder::new();
    graph.vertex("n").unwrap();
    graph
        .prepare_values(
            &[GraphColumn::property("key", "n", PropertyKeyId(1))],
            0,
            None,
        )
        .unwrap()
        .with_duplicates()
        .into()
}

#[test]
fn group_accessor_preserves_complete_children_and_does_not_invent_a_finite_bound() {
    let child = input();
    let group = child
        .clone()
        .group_by(&[0], &[GraphAggregate::count_rows("count")])
        .unwrap();
    assert_eq!(
        group.incremental_group().unwrap().input().canonical_bytes(),
        child.canonical_bytes()
    );
    assert_eq!(group.incremental_result_order(), Some((&[][..], None)));
    assert!(group.incremental_window_sequence_compatible());
    assert!(group.incremental_pattern().is_none());
    let frozen = group.canonical_bytes();
    for count in [0, 1, 5] {
        let paged = group.clone().with_page(1, Some(count));
        assert!(paged.incremental_group().is_none());
        let (complete, window) = paged.incremental_window().unwrap().unwrap();
        assert_eq!(complete.canonical_bytes(), frozen);
        assert_eq!((window.offset(), window.count()), (1, count));
        assert!(complete.incremental_group().is_some());
    }
    let ordered = group
        .with_order_by(&[GraphValueOrder::descending(1)])
        .unwrap();
    assert!(ordered.incremental_group().is_none());
    assert!(
        ordered.incremental_ordered_window().is_none(),
        "unbounded ranking still refuses"
    );
    let global = child
        .clone()
        .with_page(0, Some(0))
        .group_by(&[], &[GraphAggregate::count_rows("count")])
        .unwrap();
    let keyed = child
        .with_page(0, Some(0))
        .group_by(&[0], &[GraphAggregate::count_rows("count")])
        .unwrap();
    assert_eq!(global.incremental_result_order(), Some((&[][..], Some(1))));
    assert_eq!(keyed.incremental_result_order(), Some((&[][..], Some(0))));
    let global = global
        .with_order_by(&[GraphValueOrder::descending(0)])
        .unwrap();
    assert_eq!(global.incremental_ordered_window().unwrap().3, 1);
}

#[test]
fn shared_row_conversion_preserves_domains_and_every_interruption() {
    let row = GraphAggregateRow::from_group_values(
        vec![GraphValue::Scalar(
            CanonicalScalar::ucs_basic_text(&"x".repeat(257)).unwrap(),
        )],
        vec![
            GraphAggregateValue::Count(2),
            GraphAggregateValue::Integer(-3),
        ],
    );
    let mut calls = 0;
    let expected = row
        .to_value_row_with_control(&mut |_| {
            calls += 1;
            Ok::<(), usize>(())
        })
        .unwrap();
    assert_eq!(
        expected.values()[1],
        GraphValue::Scalar(CanonicalScalar::Int(2))
    );
    assert_eq!(
        expected.values()[2],
        GraphValue::Scalar(CanonicalScalar::Int(-3))
    );
    for stop in 1..=calls {
        let mut seen = 0;
        assert!(matches!(
            row.to_value_row_with_control(&mut |_| {
                seen += 1;
                if seen == stop { Err(stop) } else { Ok(()) }
            }),
            Err(SpillSetAggregateRowError::Control(at)) if at == stop
        ));
        assert_eq!(seen, stop);
    }
    for (value, kind) in [
        (
            GraphAggregateValue::Count(u64::MAX),
            GraphIntegerErrorKind::Overflow,
        ),
        (
            GraphAggregateValue::Integer(i128::MIN),
            GraphIntegerErrorKind::Overflow,
        ),
    ] {
        let row = GraphAggregateRow::from_group_values(
            vec![GraphValue::Scalar(CanonicalScalar::Int(1))],
            vec![value],
        );
        assert!(matches!(
            row.to_value_row_with_control(&mut |_| Ok::<(), ()>(())),
            Err(SpillSetAggregateRowError::Value(
                GraphAggregateError::OutputExpression { column: 1, error }
            )) if error.kind == kind
        ));
    }
    // A row has no exact rational: an average converts to the binary64
    // nearest it (fgdb-y255t), 2/3 here.
    let row = GraphAggregateRow::from_group_values(
        vec![GraphValue::Scalar(CanonicalScalar::Int(1))],
        vec![GraphAggregateValue::Average(
            GraphExactAverage::new(2, 3).unwrap(),
        )],
    );
    assert_eq!(
        row.to_value_row_with_control(&mut |_| Ok::<(), ()>(()))
            .unwrap()
            .values()[1],
        GraphValue::Scalar(CanonicalScalar::Float(fgdb_types::CanonicalF64::new(
            2.0 / 3.0
        )))
    );
}
