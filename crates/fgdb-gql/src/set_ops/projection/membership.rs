//! Borrowed, governed membership for evaluated native lists. Scalar equality
//! retains the row-predicate law (including FALSE between values of different
//! domains, fgdb-v6ku6), not the total ordering used to consolidate Z-set
//! keys. Nested comparisons use governed heap frames rather than relying on
//! process-stack depth.

use crate::GlaExecutionEvent;
use crate::algebra::{GraphValue, IntegerComparison};

pub(crate) fn evaluate<E>(
    value: &GraphValue,
    members: &[GraphValue],
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Option<bool>, E> {
    let mut result = Some(false);
    for member in members {
        // Do not stop at a match: every admitted member remains governed,
        // including a late cancellation after an earlier TRUE or UNKNOWN.
        match equal(value, member, control)? {
            Some(true) => result = Some(true),
            None if result != Some(true) => result = None,
            _ => {}
        }
    }
    Ok(result)
}

/// Three-valued `left = right` under the same law as membership. A list or
/// map compares element by element: a definite mismatch is FALSE, else an
/// UNKNOWN element makes it UNKNOWN. Maps with different key sets are FALSE
/// whatever their values (fgdb-9cawv). Two non-null values of different
/// domains are FALSE, as in openCypher (fgdb-v6ku6); a list of graph elements
/// (nodes(p), relationships(p)) is a list, equal to a list value element by
/// element.
pub(crate) fn equal<E>(
    left: &GraphValue,
    right: &GraphValue,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Option<bool>, E> {
    struct ListFrame<'a> {
        left: &'a [GraphValue],
        right: &'a [GraphValue],
        next: usize,
        result: Option<bool>,
    }

    let mut pending = Some((left, right));
    let mut frames: Vec<ListFrame<'_>> = Vec::new();
    let mut result = Some(true);
    loop {
        if let Some((left, right)) = pending.take() {
            control(GlaExecutionEvent::Work)?;
            for value in [left, right] {
                match value {
                    GraphValue::Scalar(value) => {
                        crate::algebra_exec::charge_payload(value, control)?;
                    }
                    // Comparing two maps reads every key once.
                    GraphValue::Map { keys, .. } => {
                        for _ in keys {
                            control(GlaExecutionEvent::Work)?;
                        }
                    }
                    _ => {}
                }
            }
            result = if left.is_null() || right.is_null() {
                None
            } else {
                match (left, right) {
                    (GraphValue::Scalar(left), GraphValue::Scalar(right)) => {
                        IntegerComparison::Equal.evaluate_scalar_pair(Some(left), Some(right))
                    }
                    (GraphValue::Vertex(left), GraphValue::Vertex(right)) => Some(left == right),
                    (GraphValue::Edge(left), GraphValue::Edge(right)) => Some(left == right),
                    (GraphValue::Path(left), GraphValue::Path(right)) => {
                        let steps = equal_sequence(left.steps(), right.steps(), control)?;
                        Some(left.start() == right.start() && steps)
                    }
                    (GraphValue::Vertices(left), GraphValue::Vertices(right)) => {
                        Some(equal_sequence(left, right, control)?)
                    }
                    (GraphValue::Edges(left), GraphValue::Edges(right)) => {
                        Some(equal_sequence(left, right, control)?)
                    }
                    // Keys are unique and ascending, so equal key sets are
                    // equal slices.
                    (
                        GraphValue::Map {
                            keys: left_keys, ..
                        },
                        GraphValue::Map {
                            keys: right_keys, ..
                        },
                    ) if left_keys != right_keys => Some(false),
                    (GraphValue::List(left), GraphValue::List(right))
                    | (
                        GraphValue::Map { values: left, .. },
                        GraphValue::Map { values: right, .. },
                    ) => {
                        if left.len() != right.len() {
                            Some(false)
                        } else if left.is_empty() {
                            Some(true)
                        } else {
                            // Reserve before growing storage. One frame per
                            // live nesting level; no copies of the list payload.
                            control(GlaExecutionEvent::ScratchEntry)?;
                            frames.push(ListFrame {
                                left,
                                right,
                                next: 1,
                                result: Some(true),
                            });
                            pending = Some((&left[0], &right[0]));
                            continue;
                        }
                    }
                    (GraphValue::Vertices(ids), GraphValue::List(items))
                    | (GraphValue::List(items), GraphValue::Vertices(ids)) => equal_elements(
                        ids.iter().map(|id| GraphValue::Vertex(*id)),
                        items,
                        control,
                    )?,
                    (GraphValue::Edges(ids), GraphValue::List(items))
                    | (GraphValue::List(items), GraphValue::Edges(ids)) => {
                        equal_elements(ids.iter().map(|id| GraphValue::Edge(*id)), items, control)?
                    }
                    // Only two empty lists are equal; any element pair is a
                    // vertex against a relationship.
                    (GraphValue::Vertices(vertices), GraphValue::Edges(edges))
                    | (GraphValue::Edges(edges), GraphValue::Vertices(vertices)) => {
                        Some(vertices.is_empty() && edges.is_empty())
                    }
                    _ => IntegerComparison::Equal.across_domains(),
                }
            };
        }
        let Some(mut frame) = frames.pop() else {
            return Ok(result);
        };
        // A definite mismatch dominates an unknown child, independent of
        // element order. Do not skip later governed children after a mismatch.
        match result {
            Some(false) => frame.result = Some(false),
            None if frame.result != Some(false) => frame.result = None,
            _ => {}
        }
        if frame.next < frame.left.len() {
            pending = Some((&frame.left[frame.next], &frame.right[frame.next]));
            frame.next += 1;
            // Reuse the already-reserved slot just popped above. This cannot
            // grow the vector's capacity or introduce another live frame.
            frames.push(frame);
        } else {
            result = frame.result;
        }
    }
}

/// Three-valued `left <comparison> right` for `<`, `<=`, `>` or `>=`
/// (openCypher comparability). Two scalars compare as a row predicate does.
/// Two lists compare element by element: equal elements continue, the first
/// unequal pair decides by the same comparison, and an UNKNOWN pair (NULL or
/// incomparable) makes the whole comparison UNKNOWN. When one list is a prefix
/// of the other, the shorter is the lesser. Any other pair, maps included, and
/// a NULL operand are UNKNOWN. The deciding pair is followed in a loop, not by
/// recursion, so nesting depth never reaches the process stack.
pub(crate) fn order<E>(
    comparison: IntegerComparison,
    left: &GraphValue,
    right: &GraphValue,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Option<bool>, E> {
    let (mut left, mut right) = (left, right);
    loop {
        control(GlaExecutionEvent::Work)?;
        match (left, right) {
            (GraphValue::Scalar(left), GraphValue::Scalar(right)) => {
                crate::algebra_exec::charge_payload(left, control)?;
                crate::algebra_exec::charge_payload(right, control)?;
                return Ok(comparison.evaluate_scalar_pair(Some(left), Some(right)));
            }
            (GraphValue::List(left_items), GraphValue::List(right_items)) => {
                let mut deciding = None;
                for (left_item, right_item) in left_items.iter().zip(right_items.iter()) {
                    match equal(left_item, right_item, control)? {
                        Some(true) => {}
                        Some(false) => {
                            deciding = Some((left_item, right_item));
                            break;
                        }
                        None => return Ok(None),
                    }
                }
                let Some((left_item, right_item)) = deciding else {
                    let lengths = left_items.len().cmp(&right_items.len());
                    return Ok(Some(match comparison {
                        IntegerComparison::Equal => lengths.is_eq(),
                        IntegerComparison::NotEqual => lengths.is_ne(),
                        IntegerComparison::Less => lengths.is_lt(),
                        IntegerComparison::LessOrEqual => lengths.is_le(),
                        IntegerComparison::Greater => lengths.is_gt(),
                        IntegerComparison::GreaterOrEqual => lengths.is_ge(),
                    }));
                };
                (left, right) = (left_item, right_item);
            }
            _ => return Ok(None),
        }
    }
}

/// A list of graph elements against a list value, element by element. Each
/// element is a vertex or an edge: an equal identity is TRUE, a NULL item is
/// UNKNOWN, and any other item is FALSE. A definite mismatch dominates an
/// UNKNOWN item; every item is still governed.
fn equal_elements<E>(
    elements: impl ExactSizeIterator<Item = GraphValue>,
    items: &[GraphValue],
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Option<bool>, E> {
    if elements.len() != items.len() {
        return Ok(Some(false));
    }
    let mut result = Some(true);
    for (element, item) in elements.zip(items) {
        control(GlaExecutionEvent::Work)?;
        if item.is_null() {
            if result == Some(true) {
                result = None;
            }
        } else if element != *item {
            result = Some(false);
        }
    }
    Ok(result)
}

fn equal_sequence<T: PartialEq, E>(
    left: &[T],
    right: &[T],
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<bool, E> {
    if left.len() != right.len() {
        return Ok(false);
    }
    let mut result = true;
    for (left, right) in left.iter().zip(right.iter()) {
        control(GlaExecutionEvent::Work)?;
        result &= left == right;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::super::{
        GraphSetColumnType, GraphSetProjection, GraphSetProjectionError, GraphSetValue,
        GraphValueRow, ProjectionFailure, evaluate_value,
    };
    use super::*;
    use crate::GraphIntegerErrorKind;
    use crate::algebra::GraphPath;
    use fgdb_types::{CanonicalScalar, EId, VId};

    fn int(value: i64) -> GraphValue {
        GraphValue::Scalar(CanonicalScalar::Int(value))
    }

    fn null() -> GraphValue {
        GraphValue::Scalar(CanonicalScalar::Null)
    }

    fn list(values: Vec<GraphValue>) -> GraphValue {
        GraphValue::List(values.into_boxed_slice())
    }

    fn member(value: &GraphValue, members: &[GraphValue]) -> Option<bool> {
        evaluate(value, members, &mut |_| Ok::<_, ()>(())).unwrap()
    }

    fn expression(value: GraphSetValue, list: GraphSetValue) -> GraphSetValue {
        GraphSetValue::In {
            value: Box::new(value),
            list: Box::new(list),
        }
    }

    #[test]
    fn empty_null_duplicate_and_mixed_members_keep_three_valued_logic() {
        assert_eq!(member(&null(), &[]), Some(false));
        assert_eq!(member(&null(), &[int(1)]), None);
        assert_eq!(member(&int(1), &[null(), int(2)]), None);
        assert_eq!(member(&int(1), &[int(2), int(3)]), Some(false));
        assert_eq!(member(&int(1), &[null(), int(1), int(1)]), Some(true));
        assert_eq!(member(&int(1), &[int(1), null()]), Some(true));
        // fgdb-v6ku6: a member of another domain is unequal, not UNKNOWN.
        let boolean = GraphValue::Scalar(CanonicalScalar::Bool(true));
        assert_eq!(member(&int(1), std::slice::from_ref(&boolean)), Some(false));
        assert_eq!(member(&int(1), &[boolean, null()]), None);
        assert_eq!(member(&int(1), &[list(vec![int(1)])]), Some(false));
    }

    #[test]
    fn nested_equality_is_not_set_key_equality() {
        let candidate = list(vec![null(), int(1)]);
        assert_eq!(member(&candidate, std::slice::from_ref(&candidate)), None);
        assert_eq!(
            member(&candidate, &[list(vec![null(), int(2)])]),
            Some(false)
        );
        assert_eq!(
            member(&list(vec![int(1), null()]), &[list(vec![int(2), null()])]),
            Some(false)
        );
        assert_eq!(member(&candidate, &[list(vec![null()])]), Some(false));
        assert_eq!(member(&list(vec![]), &[list(vec![])]), Some(true));
        assert_eq!(
            member(
                &list(vec![list(vec![int(3)])]),
                &[list(vec![list(vec![int(3)])])]
            ),
            Some(true)
        );
    }

    #[test]
    fn graph_identities_and_paths_retain_full_width_and_domain() {
        let vertex = GraphValue::Vertex(VId(u128::MAX));
        let edge = GraphValue::Edge(EId(u128::MAX));
        assert_eq!(member(&vertex, std::slice::from_ref(&vertex)), Some(true));
        assert_eq!(
            member(&vertex, &[GraphValue::Vertex(VId(u64::MAX.into()))]),
            Some(false)
        );
        assert_eq!(member(&vertex, std::slice::from_ref(&edge)), Some(false));
        assert_eq!(member(&edge, std::slice::from_ref(&edge)), Some(true));
        let path = GraphValue::Path(GraphPath::new(
            VId(u128::MAX),
            vec![(EId(u128::MAX), VId(0))].into_boxed_slice(),
        ));
        assert_eq!(member(&path, std::slice::from_ref(&path)), Some(true));
        let reverse = GraphValue::Path(GraphPath::new(
            VId(0),
            vec![(EId(u128::MAX), VId(u128::MAX))].into_boxed_slice(),
        ));
        assert_eq!(member(&path, &[reverse]), Some(false));
        assert_eq!(member(&path, std::slice::from_ref(&vertex)), Some(false));
    }

    /// fgdb-v6ku6: nodes(p) and relationships(p) are lists. Changing the
    /// list representation never turns an equal list into a FALSE one.
    #[test]
    fn element_lists_equal_list_values_element_by_element() {
        let both_ways = |left: &GraphValue, right: &GraphValue| {
            let forward = equal(left, right, &mut |_| Ok::<_, ()>(())).unwrap();
            let backward = equal(right, left, &mut |_| Ok::<_, ()>(())).unwrap();
            assert_eq!(forward, backward);
            forward
        };
        let vertices = GraphValue::Vertices(vec![VId(1), VId(2)].into_boxed_slice());
        let edges = GraphValue::Edges(vec![EId(1), EId(2)].into_boxed_slice());
        let vertex = |id| GraphValue::Vertex(VId(id));
        let edge = |id| GraphValue::Edge(EId(id));
        assert_eq!(
            both_ways(&vertices, &list(vec![vertex(1), vertex(2)])),
            Some(true)
        );
        assert_eq!(both_ways(&edges, &list(vec![edge(1), edge(2)])), Some(true));
        assert_eq!(both_ways(&vertices, &list(vec![vertex(1), null()])), None);
        assert_eq!(
            both_ways(&vertices, &list(vec![vertex(2), null()])),
            Some(false)
        );
        assert_eq!(
            both_ways(&vertices, &list(vec![vertex(1), int(2)])),
            Some(false)
        );
        assert_eq!(both_ways(&vertices, &list(vec![vertex(1)])), Some(false));
        assert_eq!(
            both_ways(&edges, &list(vec![vertex(1), vertex(2)])),
            Some(false)
        );
        assert_eq!(both_ways(&vertices, &edges), Some(false));
        assert_eq!(
            both_ways(
                &GraphValue::Vertices(Box::new([])),
                &GraphValue::Edges(Box::new([]))
            ),
            Some(true)
        );
        assert_eq!(
            both_ways(&GraphValue::Vertices(Box::new([])), &list(vec![])),
            Some(true)
        );
        assert_eq!(both_ways(&vertices, &int(1)), Some(false));
        assert_eq!(both_ways(&vertices, &null()), None);
    }

    #[test]
    fn projection_reads_both_original_columns_without_copying_the_list() {
        let expression = expression(GraphSetValue::Column(0), GraphSetValue::Column(1));
        assert_eq!(
            GraphSetProjection::admit_output(
                &expression,
                &[GraphSetColumnType::Any, GraphSetColumnType::List],
                0,
            ),
            Ok(GraphSetColumnType::Scalar)
        );
        let row = GraphValueRow::from_owned_values(vec![int(2), list(vec![int(1), int(2)])]);
        let mut scratch = 0;
        let result = evaluate_value(&expression, &row, 0, &mut |event| {
            if matches!(event, GlaExecutionEvent::ScratchEntry) {
                scratch += 1;
            }
            Ok::<_, ()>(())
        });
        assert!(matches!(
            result,
            Ok(GraphValue::Scalar(CanonicalScalar::Bool(true)))
        ));
        assert_eq!(scratch, 1, "only the Boolean result requires a copied cell");
    }

    #[test]
    fn null_rhs_is_unknown_but_nonlist_rhs_is_never_silently_false() {
        let expression = expression(GraphSetValue::Column(0), GraphSetValue::Column(1));
        for candidate in [int(1), null()] {
            let row = GraphValueRow::from_owned_values(vec![candidate.clone(), null()]);
            assert!(matches!(
                evaluate_value(&expression, &row, 0, &mut |_| Ok::<_, ()>(())),
                Ok(GraphValue::Scalar(CanonicalScalar::Null))
            ));
            let row = GraphValueRow::from_owned_values(vec![candidate, int(1)]);
            assert!(matches!(
                evaluate_value(&expression, &row, 3, &mut |_| Ok::<_, ()>(())),
                Err(ProjectionFailure::Arithmetic { column: 3, error })
                    if error.kind == GraphIntegerErrorKind::IncompatibleOperands
            ));
        }
    }

    #[test]
    fn preparation_checks_operand_references_literal_kinds_and_depth() {
        let invalid = expression(GraphSetValue::Column(3), GraphSetValue::List(vec![]));
        assert_eq!(
            GraphSetProjection::admit_output(&invalid, &[GraphSetColumnType::Scalar], 2),
            Err(GraphSetProjectionError::UnknownInput {
                column: 2,
                input: 3
            })
        );
        let invalid = expression(GraphSetValue::Value(null()), GraphSetValue::Value(int(1)));
        assert_eq!(
            GraphSetProjection::admit_output(&invalid, &[], 0),
            Err(GraphSetProjectionError::ListInput { column: 0 })
        );
        let mut deep = GraphSetValue::Value(int(1));
        for _ in 0..=GraphValue::MAX_LIST_DEPTH {
            deep = expression(deep, GraphSetValue::List(vec![]));
        }
        assert_eq!(
            GraphSetProjection::admit_output(&deep, &[], 0),
            Err(GraphSetProjectionError::ExpressionBounds { column: 0 })
        );
    }

    #[test]
    fn every_control_refusal_is_preserved_even_after_a_match() {
        let expression = expression(GraphSetValue::Column(0), GraphSetValue::Column(1));
        let row = GraphValueRow::from_owned_values(vec![int(0), list((0..128).map(int).collect())]);
        let mut events = 0;
        assert!(
            evaluate_value(&expression, &row, 0, &mut |_| {
                events += 1;
                Ok::<_, usize>(())
            })
            .is_ok()
        );
        assert!(events >= 128);
        for refused in 0..events {
            let mut at = 0;
            let result = evaluate_value(&expression, &row, 0, &mut |_| {
                if at == refused {
                    Err(refused)
                } else {
                    at += 1;
                    Ok(())
                }
            });
            assert!(matches!(result, Err(ProjectionFailure::Control(at)) if at == refused));
        }
    }

    #[test]
    fn membership_transcript_is_domain_separated_and_binds_operand_order() {
        let left = expression(GraphSetValue::Column(0), GraphSetValue::Column(1));
        let right = expression(GraphSetValue::Column(1), GraphSetValue::Column(0));
        let mut left_bytes = Vec::new();
        let mut right_bytes = Vec::new();
        left.append_canonical_bytes(&mut left_bytes);
        right.append_canonical_bytes(&mut right_bytes);
        assert_eq!(left_bytes[0], 7);
        assert_ne!(left_bytes, right_bytes);
        let mut column_bytes = Vec::new();
        GraphSetValue::Column(0).append_canonical_bytes(&mut column_bytes);
        assert_eq!(
            column_bytes,
            [vec![0], 0_u64.to_be_bytes().to_vec()].concat()
        );
    }

    #[test]
    fn deep_comparisons_use_governed_heap_frames_without_recursive_cleanup() {
        const DEPTH: usize = 4096;
        let mut left = int(1);
        let mut right = int(1);
        for _ in 0..DEPTH {
            left = list(vec![left]);
            right = list(vec![right]);
        }
        let mut scratch = 0;
        let result = evaluate(&left, core::slice::from_ref(&right), &mut |event| {
            if matches!(event, GlaExecutionEvent::ScratchEntry) {
                scratch += 1;
            }
            Ok::<_, usize>(())
        });
        assert_eq!(result, Ok(Some(true)));
        assert_eq!(scratch, DEPTH);
        let mut scratch = 0;
        let result = evaluate(&left, core::slice::from_ref(&right), &mut |event| {
            if matches!(event, GlaExecutionEvent::ScratchEntry) {
                if scratch == 8 {
                    return Err(8);
                }
                scratch += 1;
            }
            Ok(())
        });
        assert_eq!(result, Err(8));
        assert_eq!(scratch, 8);
        // The test owns deliberately deep public values. Release each unary
        // wrapper iteratively too, so their destructor is not the stack test.
        for mut value in [left, right] {
            while let GraphValue::List(children) = value {
                let mut children = children.into_vec();
                assert_eq!(children.len(), 1);
                value = children.pop().unwrap();
            }
        }
    }

    #[test]
    fn nested_comparisons_preserve_every_work_and_frame_refusal() {
        let candidate = list(vec![list(vec![null(), int(1)]), list(vec![int(2)])]);
        let members = [
            list(vec![list(vec![null(), int(9)]), list(vec![int(2)])]),
            list(vec![list(vec![null(), int(1)]), list(vec![int(2)])]),
        ];
        let mut events = 0;
        assert_eq!(
            evaluate(&candidate, &members, &mut |_| {
                events += 1;
                Ok::<_, usize>(())
            }),
            Ok(None)
        );
        for stop in 0..events {
            let mut seen = 0;
            let result = evaluate(&candidate, &members, &mut |_| {
                if seen == stop {
                    Err(stop)
                } else {
                    seen += 1;
                    Ok(())
                }
            });
            assert_eq!(result, Err(stop));
            assert_eq!(seen, stop);
        }
    }
}
