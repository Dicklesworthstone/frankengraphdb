//! Copy admission is tested independently of the database adapter. The value
//! copier supplies the expected event transcript; the chain owns only slots.

use super::*;
use crate::algebra::{GraphPath, GraphValue, GraphValueRow};
use crate::vertex_merge::GraphVertexMergeRowsInput;
use crate::{
    GlaExecutionEvent, GqlQueryError, GraphIntegerErrorKind, GraphSetColumnType,
    GraphSetProjection, GraphSetQuantifier, GraphSetValue, GraphVertexMergeRowsBinding,
    PreparedGraphVertexMergeRowsQuery,
};
use fgdb_delta_types::{ElementId, PropertyKeyId, RelationId};
use fgdb_types::{CanonicalScalar, EId, VId};

type Error = GqlQueryError<(), usize>;
const WIDE: VId = VId((1_u128 << 100) | 7);
const EDGE: EId = EId((1_u128 << 112) | 7);
const P: PropertyKeyId = PropertyKeyId(1);

fn payload() -> GraphValue {
    GraphValue::map(vec![(
        "long_key".repeat(16).into_boxed_str(),
        GraphValue::List(
            vec![
                GraphValue::Scalar(CanonicalScalar::ucs_basic_text(&"x".repeat(1024)).unwrap()),
                GraphValue::Path(GraphPath::new(
                    WIDE,
                    vec![(EDGE, VId(7))].into_boxed_slice(),
                )),
                GraphValue::List(vec![GraphValue::Scalar(CanonicalScalar::Null)].into()),
            ]
            .into(),
        ),
    )])
    .unwrap()
}

fn merge(keep: usize, types: Vec<GraphSetColumnType>) -> PreparedGraphVertexMergeRows {
    PreparedGraphVertexMergeRows {
        relation: RelationId(1),
        labels: Vec::new(),
        properties: Vec::new(),
        input: GraphVertexMergeRowsInput::Owned(types),
        keep,
        actions: None,
    }
}

#[test]
fn continuation_matches_native_row_and_charges_only_the_visible_prefix() {
    let visible = payload();
    let input = GraphValueRow::from_owned_values(vec![visible.clone(), payload()]);
    let merge = merge(1, vec![GraphSetColumnType::Any; 2]);
    let expected = merge.output_row(&input, WIDE);
    let step = GraphWriteChainStep::VertexMerge(Box::new(merge));
    let mut events = Vec::new();
    let actual = step
        .continuation_row_controlled(&input, ElementId::Vertex(WIDE), &mut |event| {
            events.push(event);
            Ok::<(), Error>(())
        })
        .unwrap();
    assert_eq!(actual, expected);
    assert_eq!(
        actual.values(),
        &[visible.clone(), GraphValue::Vertex(WIDE)]
    );

    let mut expected_events = vec![GlaExecutionEvent::ScratchEntry; 2];
    visible
        .copy_with_control(&mut |event| {
            expected_events.push(event);
            Ok::<(), Error>(())
        })
        .unwrap();
    expected_events.push(GlaExecutionEvent::ScratchEntry);
    assert_eq!(events, expected_events);
    assert!(!events.contains(&GlaExecutionEvent::ResultRow));
}

#[test]
fn every_nested_continuation_copy_boundary_is_interruptible() {
    let input = GraphValueRow::from_owned_values(vec![payload()]);
    let original = input.clone();
    let step = GraphWriteChainStep::VertexMerge(Box::new(merge(1, vec![GraphSetColumnType::Any])));
    let mut events = Vec::new();
    let complete = step
        .continuation_row_controlled(&input, ElementId::Vertex(WIDE), &mut |event| {
            events.push(event);
            Ok::<(), Error>(())
        })
        .unwrap();
    assert!(events.len() > 30);
    for stop in 0..events.len() {
        let mut reached = 0;
        let result =
            step.continuation_row_controlled(&input, ElementId::Vertex(WIDE), &mut |event| {
                assert_eq!(event, events[reached]);
                let current = reached;
                reached += 1;
                if current == stop {
                    Err(Error::Interrupted(stop))
                } else {
                    Ok(())
                }
            });
        assert!(matches!(result, Err(Error::Interrupted(at)) if at == stop));
        assert_eq!(reached, stop + 1);
        assert_eq!(input, original);
    }
    assert_eq!(
        step.continuation_row_controlled(
            &input,
            ElementId::Vertex(WIDE),
            &mut |_| Ok::<(), Error>(()),
        )
        .unwrap(),
        complete,
    );
}

fn returning_step() -> GraphWriteChainStep {
    use GraphVertexMergeRowsBinding as B;
    let bindings = vec![
        B::Input(0),
        B::Vertex,
        B::Property(P),
        B::InputProperty { column: 1, key: P },
        B::InputProperty { column: 2, key: P },
        B::InputProperty { column: 3, key: P },
    ];
    let projection = (0..bindings.len())
        .map(|column| GraphSetProjection::new(format!("v{column}"), GraphSetValue::Column(column)))
        .collect();
    let query = PreparedGraphVertexMergeRowsQuery::prepare_with_grouping(
        merge(
            4,
            vec![
                GraphSetColumnType::Any,
                GraphSetColumnType::Vertex,
                GraphSetColumnType::Edge,
                GraphSetColumnType::Vertex,
            ],
        ),
        bindings,
        projection,
        GraphSetQuantifier::All,
        None,
    )
    .unwrap();
    GraphWriteChainStep::VertexMergeQuery(Box::new(query))
}

#[test]
fn returned_cells_keep_nested_values_full_identities_and_nullable_properties() {
    let step = returning_step();
    let carried = payload();
    let input = GraphValueRow::from_owned_values(vec![
        carried.clone(),
        GraphValue::Vertex(VId(7)),
        GraphValue::Edge(EDGE),
        GraphValue::Scalar(CanonicalScalar::Null),
    ]);
    let stored = GraphValue::List(
        vec![
            GraphValue::Scalar(CanonicalScalar::ucs_basic_text(&"v".repeat(2048)).unwrap()),
            GraphValue::List(vec![GraphValue::Scalar(CanonicalScalar::Int(9))].into()),
        ]
        .into(),
    );
    let property = stored.to_property_value().unwrap();
    let mut reads = Vec::new();
    let mut events = Vec::new();
    let actual = step
        .vertex_return_row_controlled(
            &input,
            WIDE,
            |element, key| {
                reads.push((element, key));
                Ok::<_, Error>((element != ElementId::Edge(EDGE)).then(|| property.clone()))
            },
            &mut |event| {
                events.push(event);
                Ok::<(), Error>(())
            },
        )
        .unwrap();
    assert_eq!(
        actual,
        vec![
            carried,
            GraphValue::Vertex(WIDE),
            stored.clone(),
            stored,
            GraphValue::Scalar(CanonicalScalar::Null),
            GraphValue::Scalar(CanonicalScalar::Null),
        ]
    );
    assert_eq!(
        reads,
        vec![
            (ElementId::Vertex(WIDE), P),
            (ElementId::Vertex(VId(7)), P),
            (ElementId::Edge(EDGE), P),
        ]
    );
    assert!(
        events.len() > 100,
        "large stored and carried values need payload admission"
    );
    assert!(!events.contains(&GlaExecutionEvent::ResultRow));

    for stop in 0..events.len() {
        let mut reached = 0;
        let result = step.vertex_return_row_controlled(
            &input,
            WIDE,
            |element, _| {
                Ok::<_, Error>((element != ElementId::Edge(EDGE)).then(|| property.clone()))
            },
            &mut |event| {
                assert_eq!(event, events[reached]);
                let current = reached;
                reached += 1;
                if current == stop {
                    Err(Error::Interrupted(stop))
                } else {
                    Ok(())
                }
            },
        );
        assert!(matches!(result, Err(Error::Interrupted(at)) if at == stop));
        assert_eq!(reached, stop + 1);
    }
}

#[test]
fn bad_continuation_domains_refuse_before_copying_and_read_errors_stay_typed() {
    let step = GraphWriteChainStep::VertexMerge(Box::new(merge(1, vec![GraphSetColumnType::Any])));
    let mut events = 0;
    for (input, element) in [
        (
            GraphValueRow::from_owned_values(Vec::new()),
            ElementId::Vertex(WIDE),
        ),
        (
            GraphValueRow::from_owned_values(vec![payload()]),
            ElementId::Edge(EDGE),
        ),
    ] {
        let result = step.continuation_row_controlled(&input, element, &mut |_| {
            events += 1;
            Ok::<(), Error>(())
        });
        assert!(matches!(result, Err(Error::Data(error))
            if error.kind == GraphIntegerErrorKind::IncompatibleOperands));
    }
    assert_eq!(events, 0);
    let input = GraphValueRow::from_owned_values(vec![
        payload(),
        GraphValue::Vertex(VId(7)),
        GraphValue::Edge(EDGE),
        GraphValue::Scalar(CanonicalScalar::Null),
    ]);
    let mut reads = 0;
    let result = returning_step().vertex_return_row_controlled(
        &input,
        WIDE,
        |_, _| {
            reads += 1;
            Err::<Option<fgdb_types::CanonicalPropertyValue>, _>(Error::Source(()))
        },
        &mut |_| Ok::<(), Error>(()),
    );
    assert!(matches!(result, Err(Error::Source(()))));
    assert_eq!(reads, 1, "a read failure cannot proceed to another field");
}
