use super::*;
use crate::algebra::{GraphColumn, GraphValue, GraphValueOrder};
use crate::{GqlQueryError, GqlQueryPolicy};
use fgdb_delta_types::RelationId;
use fgdb_types::CanonicalScalar;

const KEY: PropertyKeyId = PropertyKeyId(1);

fn builder() -> GraphPatternBuilder {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("a").unwrap().vertex("b").unwrap();
    builder
}

fn columns() -> [GraphColumn<'static>; 5] {
    [
        GraphColumn::vertex("a", "a"),
        GraphColumn::vertex("b", "b"),
        GraphColumn::property("bp", "b", KEY),
        GraphColumn::property("ap", "a", KEY),
        GraphColumn::vertex("copy", "a"),
    ]
}

fn evaluate(pattern: &PreparedGraphPattern<GraphValueRow>) -> Vec<GraphValueRow> {
    let property = CanonicalScalar::Int(7);
    let execution: Result<_, GqlQueryError<(), ()>> =
        pattern.plan().execute_governed_with_properties(
            3,
            [VId(3), VId(1), VId(2)],
            std::iter::empty::<(VId, RelationId, VId)>(),
            |vid, predicates| {
                Ok(predicates.iter().all(|predicate| match predicate {
                    VertexPredicate::HasLabel(LabelId(1)) => vid != VId(2),
                    VertexPredicate::HasLabel(LabelId(2)) => vid != VId(1),
                    _ => panic!("only the proved total labels are evaluated"),
                }))
            },
            |_, _| Ok(Some(&property)),
            GqlQueryPolicy::new(3, 100, 100_000, 100_000),
            || Ok(()),
        );
    execution.unwrap().value
}

#[test]
fn factor_flatten_matches_the_original_canonical_columns_labels_and_multiplicity() {
    let mut builder = builder();
    builder
        .filter("a", VertexPredicate::HasLabel(LabelId(1)))
        .unwrap();
    builder
        .filter("b", VertexPredicate::HasLabel(LabelId(2)))
        .unwrap();
    for duplicates in [false, true] {
        let original = builder.prepare_values(&columns(), 0, None).unwrap();
        let original = if duplicates {
            original.with_duplicates()
        } else {
            original
        };
        let before = original.canonical_bytes();
        let mut charged = 0;
        let factors = original
            .independent_vertex_factors_with_control(&mut |_| {
                charged += 1;
                Ok::<_, ()>(())
            })
            .unwrap()
            .unwrap();
        assert!(charged > 0);
        assert!(core::ptr::eq(factors.original(), &original));
        assert_eq!(factors.columns(), &[(0, 0), (1, 0), (1, 1), (0, 1), (0, 2)]);
        assert_eq!(factors.factors()[0].columns(), &["a", "ap", "copy"]);
        assert_eq!(factors.factors()[1].columns(), &["b", "bp"]);
        assert_eq!(factors.factors()[0].preserves_duplicates(), duplicates);
        assert_eq!(factors.factors()[1].preserves_duplicates(), duplicates);
        let left = evaluate(&factors.factors()[0]);
        let right = evaluate(&factors.factors()[1]);
        let mut flattened = Vec::new();
        for a in &left {
            for b in &right {
                let values = factors
                    .columns()
                    .iter()
                    .map(|&(factor, at)| (if factor == 0 { a } else { b }).values()[at].clone())
                    .collect();
                flattened.push(GraphValueRow::from_owned_values(values));
            }
        }
        assert_eq!(flattened, evaluate(&original));
        assert!(
            flattened
                .iter()
                .any(|row| row.values()[0] == row.values()[1])
        );
        assert_eq!(original.canonical_bytes(), before);
        assert_eq!(flattened[0].values()[0], GraphValue::Vertex(VId(1)));
    }
}

#[test]
fn unsupported_scope_order_page_predicate_and_projection_have_no_allocation_or_control_cost() {
    let mut local = builder();
    local
        .filter(
            "b",
            VertexPredicate::PropertyNull {
                key: KEY,
                is_null: true,
            },
        )
        .unwrap();
    let mut edge = builder();
    edge.edge("a", RelationId(1), GlaDirection::Forward, "b")
        .unwrap();
    let plain = builder();
    let original = plain.prepare_values(&columns(), 0, None).unwrap();
    let mut reversed = columns();
    reversed.swap(0, 1);
    for rejected in [
        original
            .clone()
            .with_order_by(&[GraphValueOrder::descending(0)])
            .unwrap(),
        original.with_duplicates().with_visible_columns(2),
        plain.prepare_values(&columns(), 1, None).unwrap(),
        plain.prepare_values(&columns(), 0, Some(0)).unwrap(),
        plain.prepare_values(&reversed, 0, None).unwrap(),
        local.prepare_values(&columns(), 0, None).unwrap(),
        edge.prepare_values(&columns(), 0, None).unwrap(),
    ] {
        assert!(
            rejected
                .independent_vertex_factors_with_control(&mut |_| -> Result<(), ()> {
                    panic!(
                        "an unsupported physical shape must retain the old source's failure path"
                    )
                })
                .unwrap()
                .is_none()
        );
    }
}
