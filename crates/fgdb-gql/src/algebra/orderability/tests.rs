use super::*;
use fgdb_types::{CanonicalDecimal, EId, VId};

fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn float(value: f64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(value)))
}
fn decimal(coefficient: i128, scale: u32) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Decimal(
        CanonicalDecimal::from_scaled_half_even(coefficient, scale).unwrap(),
    ))
}
fn text(value: &str) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::ucs_basic_text(value).unwrap())
}
fn null() -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Null)
}
fn list(values: Vec<GraphValue>) -> GraphValue {
    let value = GraphValue::List(values.into_boxed_slice());
    assert!(value.validate_bounds());
    value
}
fn assert_order(values: &[GraphValue]) {
    for (a, left) in values.iter().enumerate() {
        for (b, right) in values.iter().enumerate() {
            assert_eq!(
                left.compare_orderability(right),
                a.cmp(&b),
                "{left:?} / {right:?}"
            );
        }
    }
}

#[test]
fn mixed_numeric_order_is_exact_beyond_binary64_precision() {
    assert_order(&[
        float(f64::NEG_INFINITY),
        int(i64::MIN),
        int(-9_007_199_254_740_993),
        float(-9_007_199_254_740_992.0),
        decimal(-1, 1),
        int(0),
        decimal(1, 1),
        float(0.1),
        int(1),
        float(2.0),
        float(3.2),
        int(5),
        float(9_007_199_254_740_992.0),
        int(9_007_199_254_740_993),
        int(i64::MAX),
        float(9_223_372_036_854_775_808.0),
        float(f64::INFINITY),
        float(f64::NAN),
    ]);
    for (a, b) in [
        (int(1), float(1.0)),
        (int(1), decimal(10, 1)),
        (int(0), float(-0.0)),
    ] {
        assert_eq!(a.compare_orderability(&b), Ordering::Equal);
        assert_eq!(b.compare_orderability(&a), Ordering::Equal);
        assert_ne!(
            a, b,
            "language ties must not erase canonical typed identity"
        );
        assert_ne!(a.canonical_bytes().unwrap(), b.canonical_bytes().unwrap());
    }
}

#[test]
fn heterogeneous_lists_use_recursive_language_order_with_null_last() {
    assert_order(&[
        list(vec![]),
        list(vec![text("a")]),
        list(vec![text("a"), int(1)]),
        list(vec![int(1)]),
        list(vec![int(1), text("a")]),
        list(vec![int(1), null()]),
        list(vec![null(), int(1)]),
        list(vec![null(), int(2)]),
    ]);
    assert_order(&[
        GraphValue::map(vec![]).unwrap(),
        GraphValue::Vertex(VId(u128::MAX)),
        GraphValue::Edge(EId(0)),
        list(vec![]),
        GraphValue::Path(super::super::GraphPath::new(VId(0), Box::new([]))),
        text("a"),
        GraphValue::Scalar(CanonicalScalar::Bool(false)),
        int(0),
        float(f64::NAN),
        null(),
    ]);
    let vertices = GraphValue::Vertices(vec![VId(1), VId(u128::MAX)].into_boxed_slice());
    let generic = list(vec![
        GraphValue::Vertex(VId(1)),
        GraphValue::Vertex(VId(u128::MAX)),
    ]);
    assert_eq!(vertices.compare_orderability(&generic), Ordering::Equal);
    assert_ne!(vertices, generic);
    assert_eq!(
        list(vec![int(1)]).compare_orderability(&list(vec![float(1.0)])),
        Ordering::Equal
    );
}

#[test]
fn maps_compare_size_then_key_set_then_orderable_values() {
    let map = |pairs: Vec<(&str, GraphValue)>| {
        GraphValue::map(
            pairs
                .into_iter()
                .map(|(key, value)| (key.into(), value))
                .collect(),
        )
        .unwrap()
    };
    assert_order(&[
        map(vec![]),
        map(vec![("a", text("z"))]),
        map(vec![("a", int(3))]),
        map(vec![("a", float(4.0))]),
        map(vec![("b", int(0))]),
        map(vec![("a", int(-100)), ("b", int(-100))]),
    ]);
    assert_eq!(
        map(vec![("a", int(1))]).compare_orderability(&map(vec![("a", float(1.0))])),
        Ordering::Equal,
    );
}

#[test]
fn every_nested_comparison_checkpoint_can_stop_without_mutation() {
    let value = GraphValue::map(vec![(
        "large-key".repeat(64).into(),
        list(vec![
            text(&"payload".repeat(256)),
            list(vec![int(1), float(1.0), null()]),
        ]),
    )])
    .unwrap();
    let before = value.canonical_bytes().unwrap();
    let mut calls = 0;
    assert_eq!(
        value
            .compare_orderability_with_control(&value, &mut |_| {
                calls += 1;
                Ok::<_, usize>(())
            })
            .unwrap(),
        Ordering::Equal
    );
    assert!(calls > 50);
    for stop in 1..=calls {
        let mut seen = 0;
        assert_eq!(
            value.compare_orderability_with_control(&value, &mut |_| {
                seen += 1;
                if seen == stop { Err(stop) } else { Ok(()) }
            }),
            Err(stop)
        );
        assert_eq!(value.canonical_bytes().unwrap(), before);
    }
}
