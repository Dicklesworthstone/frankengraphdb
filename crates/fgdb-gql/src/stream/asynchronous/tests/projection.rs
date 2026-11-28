use super::*;
use crate::scan_stream::ScanError;
use crate::{
    GraphElementKey, GraphSetProjection, GraphSetQuantifier, GraphSetValue, PreparedGraphSet,
};

fn input(text: &str) -> PreparedGraphSet {
    PreparedGraphText::prepare(text, |kind, name: &str| {
        (kind == GraphSymbolKind::Property)
            .then(|| {
                name.strip_prefix('p')
                    .and_then(|name| name.parse::<u64>().ok())
                    .map(|id| GraphSymbol::Property(PropertyKeyId(id)))
            })
            .flatten()
    })
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
    .into()
}

fn property(key: u64, receiver: GraphSetValue) -> GraphSetValue {
    GraphSetValue::MapGet {
        map: Box::new(receiver),
        key: format!("p{key}").into(),
        property: GraphElementKey::Property(PropertyKeyId(key)),
    }
}

fn projected(value: GraphSetValue) -> PreparedGraphSet {
    input("MATCH (n) RETURN n AS id")
        .project(
            vec![
                GraphSetProjection::new("id", GraphSetValue::Column(0)),
                GraphSetProjection::new("value", value),
            ],
            GraphSetQuantifier::All,
        )
        .unwrap()
}

#[test]
fn computed_demand_unions_native_and_lazy_root_reads_before_any_source_is_opened() {
    let fields = (1..16)
        .map(|key| property(key, GraphSetValue::Column(0)))
        .collect();
    let make = |value| {
        input("MATCH (n) WHERE n.p0 = 1 RETURN n AS id")
            .project(
                vec![
                    GraphSetProjection::new("id", GraphSetValue::Column(0)),
                    GraphSetProjection::new("values", value),
                ],
                GraphSetQuantifier::All,
            )
            .unwrap()
    };
    let accepted = make(GraphSetValue::List(fields));
    let plan = AsyncVertexProjectionPlan::compile(&accepted).unwrap();
    let VertexScanPropertyDemand::Properties(keys) = plan.property_demand() else {
        panic!("complete root demand must be selected properties");
    };
    assert_eq!(keys.keys(), &(0..16).map(PropertyKeyId).collect::<Vec<_>>());

    // A statically unselected branch still participates. Its sixteenth VM key
    // plus the native predicate exceeds the physical source's total bound.
    let refused = make(GraphSetValue::Case {
        arms: vec![(
            GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Bool(true))),
            GraphSetValue::List(
                (1..16)
                    .map(|key| property(key, GraphSetValue::Column(0)))
                    .collect(),
            ),
        )],
        otherwise: Some(Box::new(GraphSetValue::List(vec![property(
            16,
            GraphSetValue::Column(0),
        )]))),
    });
    assert!(AsyncVertexProjectionPlan::compile(&refused).is_err());

    let indirect = projected(property(
        1,
        GraphSetValue::Index {
            list: Box::new(GraphSetValue::List(vec![GraphSetValue::Column(0)])),
            index: Box::new(GraphSetValue::Value(GraphValue::Scalar(
                CanonicalScalar::Int(0),
            ))),
        },
    ));
    assert!(AsyncVertexProjectionPlan::compile(&indirect).is_err());
}

#[test]
fn computed_dynamic_root_reads_keep_the_current_record_and_one_candidate_charge() {
    // Build a real element-read VM cell, rather than a text property already
    // hoisted to a native source column. The source pattern itself needs only n.
    let query = projected(property(1, GraphSetValue::Column(0)));
    let compiled = AsyncVertexProjectionPlan::compile(&query).unwrap();
    assert_eq!(
        compiled.property_demand(),
        VertexScanPropertyDemand::Properties(PropertySelection::one(PropertyKeyId(1)))
    );
    let input = source(vec![
        sample(1, Some(CanonicalScalar::Int(99)), false),
        sample(2, Some(CanonicalScalar::Int(7)), true),
        sample(3, None, true),
    ]);
    let signals = input.signals.clone();
    let mut cursor = AsyncVertexProjectionCursor::new(input, compiled, wide(), || Ok::<_, ()>(()));
    let first = ready(cursor.next()).unwrap().unwrap();
    assert_eq!(
        first.values(),
        &[
            GraphValue::Vertex(VId(2)),
            GraphValue::Scalar(CanonicalScalar::Int(7))
        ]
    );
    drop(first);
    let second = ready(cursor.next()).unwrap().unwrap();
    assert_eq!(
        second.values(),
        &[
            GraphValue::Vertex(VId(3)),
            GraphValue::Scalar(CanonicalScalar::Null)
        ]
    );
    drop(second);
    assert!(ready(cursor.next()).is_none());
    assert_eq!(cursor.row_stats().snapshot_records, 3);
    assert_eq!(cursor.row_stats().result_rows, 2);
    assert_eq!(
        signals.reads.get(),
        3,
        "VM property access must not reopen a source history"
    );
    assert!(signals.dropped.get());
    assert_eq!(signals.live_outputs.get(), 0);
    assert_eq!(signals.live_evaluation.get(), 0);
}

#[test]
fn computed_suffix_failure_uses_matching_row_ordinal_and_pending_drop_fuses() {
    let query = crate::PreparedGraphSetText::prepare(
        "MATCH (n) RETURN n, 10 / n.p AS quotient LIMIT 1",
        |kind, name: &str| {
            (kind == GraphSymbolKind::Property && name == "p")
                .then_some(GraphSymbol::Property(PropertyKeyId(1)))
        },
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap();
    let compiled = AsyncVertexProjectionPlan::compile(&query).unwrap();
    let input = source(vec![
        sample(1, Some(CanonicalScalar::Int(0)), false),
        sample(2, Some(CanonicalScalar::Int(2)), true),
        sample(3, Some(CanonicalScalar::Int(0)), true),
    ]);
    let signals = input.signals.clone();
    let mut cursor =
        AsyncVertexProjectionCursor::new(input, compiled.clone(), wide(), || Ok::<_, ()>(()));
    drop(ready(cursor.next()).unwrap().unwrap());
    assert!(matches!(
        ready(cursor.next()),
        Some(Err(GqlQueryError::Source(ScanError::Projection {
            row: 1,
            column: 1,
            ..
        })))
    ));
    assert_eq!(cursor.row_stats().snapshot_records, 3);
    assert_eq!(cursor.row_stats().result_rows, 1);
    assert_eq!(cursor.state(), VertexScanState::Failed);
    assert!(signals.dropped.get());
    assert!(ready(cursor.next()).is_none());

    let mut input = source(vec![sample(1, Some(CanonicalScalar::Int(2)), true)]);
    input.pending_at = Some(0);
    let signals = input.signals.clone();
    let mut cursor = AsyncVertexProjectionCursor::new(input, compiled, wide(), || Ok::<_, ()>(()));
    let mut pending = Box::pin(cursor.next());
    fn assert_send<T: Send>(_: &T) {}
    assert_send(&pending);
    assert!(
        pending
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    drop(pending);
    assert_eq!(cursor.state(), VertexScanState::Failed);
    assert!(signals.dropped.get());
    assert!(ready(cursor.next()).is_none());
}
