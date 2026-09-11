//! Sealed output shapes for the shared binding-row evaluator.
//!
//! A tuple is one correlated assignment projection, never a zip or product of
//! independently evaluated columns. The compiler alone constructs typed plans.

use super::{GlaOperator, GraphValueRow, MAX_PATTERN_VERTICES};
use crate::GlaExecutionEvent;
use crate::algebra_exec::ProjectedRows;
use fgdb_delta_types::PropertyKeyId;
use fgdb_types::{CanonicalScalar, VId};

/// One nonempty, ordered projection of vertex bindings. Column positions match
/// the immutable prepared pattern's `columns()`; Debug never prints identities.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GraphBindingRow {
    values: Box<[VId]>,
}

impl GraphBindingRow {
    #[must_use]
    pub fn values(&self) -> &[VId] {
        &self.values
    }

    #[must_use]
    pub fn get(&self, column: usize) -> Option<VId> {
        self.values.get(column).copied()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

impl std::borrow::Borrow<[VId]> for GraphBindingRow {
    fn borrow(&self) -> &[VId] {
        self.values()
    }
}

impl core::fmt::Debug for GraphBindingRow {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GraphBindingRow")
            .field("columns", &self.len())
            .field("values", &"[REDACTED]")
            .finish()
    }
}

/// Closed output domain. Only a private compiler pairs plans with collectors.
/// Property rows require an explicit property source; no absent-source fallback
/// can silently replace every projected value with null. Nullable scopes are
/// compiled only for GraphValueRow, never for the identity-only output types.
pub trait GlaOutput: sealed::PropertyProjection + Clone + Ord {}
impl GlaOutput for VId {}
impl GlaOutput for GraphBindingRow {}
impl GlaOutput for GraphValueRow {}

/// Outputs that need only vertex identities and predicate reads. This sealed
/// bound keeps the existing predicate-only execution APIs statically honest.
pub trait GlaIdentityOutput: GlaOutput + sealed::Projection {}
impl GlaIdentityOutput for VId {}
impl GlaIdentityOutput for GraphBindingRow {}

impl super::GlaPlan<GraphValueRow> {
    /// Internal aggregate specialization, never a public plan constructor.
    /// A terminal forest of fresh, unprojected bindings can be replaced by
    /// completion weights. The caller must attach those weights; executing
    /// this prefix alone is NOT equivalent to executing the original query.
    ///
    /// Keep every projected slot and stop at the first non-Expand operator.
    /// Earlier identities/cycles remain in the prefix. No predicate, property
    /// read, nullable scope, DISTINCT or child pagination is removable here.
    /// Return the number of edge atoms retained by the prefix, in their
    /// original order. All remaining atoms form the completion forest.
    pub(crate) fn aggregate_prefix<E>(
        &self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<(Self, usize)>, E> {
        let operators = self.operators();
        let Some(projection_at) = operators.len().checked_sub(3) else {
            return Ok(None);
        };
        let Some(GlaOperator::ProjectValues { columns }) = operators.get(projection_at) else {
            return Ok(None);
        };
        if !matches!(operators.first(), Some(GlaOperator::ScanEdges { .. }))
            || !matches!(operators.get(projection_at + 1), Some(GlaOperator::OrderByValues))
            || !matches!(operators.last(), Some(GlaOperator::Limit { offset: 0, count: None }))
        {
            return Ok(None);
        }
        let mut width = 2_usize;
        for operator in &operators[1..projection_at] {
            control(GlaExecutionEvent::Work)?;
            match operator {
                GlaOperator::Expand { source, .. } if (source.ordinal() as usize) < width => {
                    width += 1;
                }
                GlaOperator::VertexIdentity { left, right, .. }
                    if (left.ordinal() as usize) < width && (right.ordinal() as usize) < width => {}
                _ => return Ok(None),
            }
        }
        let mut last_projected = 0_usize;
        for column in columns {
            control(GlaExecutionEvent::Work)?;
            let super::ValueProjection::Vertex { slot } = column else {
                return Ok(None);
            };
            let slot = slot.ordinal() as usize;
            if slot >= width { return Ok(None); }
            last_projected = last_projected.max(slot);
        }
        let mut cut = projection_at;
        while cut > 1 && width - 1 > last_projected {
            control(GlaExecutionEvent::Work)?;
            if !matches!(operators[cut - 1], GlaOperator::Expand { .. }) { break; }
            cut -= 1;
            width -= 1;
        }
        if cut == projection_at { return Ok(None); }
        let mut prefix = Vec::new();
        for operator in operators[..cut].iter().chain(&operators[projection_at..]) {
            control(GlaExecutionEvent::ScratchEntry)?;
            if let GlaOperator::ProjectValues { columns } = operator {
                for _ in columns { control(GlaExecutionEvent::ScratchEntry)?; }
            }
            prefix.push(operator.clone());
        }
        Ok(Some((Self::from_operators(prefix), width - 1)))
    }
}

mod sealed {
    use super::*;

    pub trait Projection: Sized + Ord {
        fn collect<E>(
            operator: &GlaOperator,
            bindings: &[Option<VId>],
            projected: &mut ProjectedRows<Self>,
            control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
        ) -> Result<(), E>;
    }

    pub trait PropertyProjection: Sized + Ord {
        fn collect_properties<'a, E>(
            operator: &GlaOperator,
            bindings: &[Option<VId>],
            projected: &mut ProjectedRows<Self>,
            property: &mut impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
            control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
        ) -> Result<(), E>;
    }

    impl Projection for VId {
        fn collect<E>(
            operator: &GlaOperator,
            bindings: &[Option<VId>],
            projected: &mut ProjectedRows<Self>,
            control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
        ) -> Result<(), E> {
            let GlaOperator::Project { slot } = operator else {
                unreachable!("the private scalar-plan constructor owns its projection shape")
            };
            let value = bindings[slot.ordinal() as usize]
                .expect("identity-only plans cannot contain nullable bindings");
            if projected.should_retain(&value, control)? {
                control(GlaExecutionEvent::ScratchEntry)?;
                projected.insert(value);
            }
            Ok(())
        }
    }

    impl Projection for GraphBindingRow {
        fn collect<E>(
            operator: &GlaOperator,
            bindings: &[Option<VId>],
            projected: &mut ProjectedRows<Self>,
            control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
        ) -> Result<(), E> {
            let GlaOperator::ProjectBindings { slots } = operator else {
                unreachable!("the private tuple-plan constructor owns its projection shape")
            };
            // The compiler caps columns at MAX_PATTERN_VERTICES. Form the
            // lookup key on the stack. DISTINCT elides repeated tuples; ALL
            // reserves each occurrence. Every selected cell is a checkpoint.
            let mut key = [VId(0); MAX_PATTERN_VERTICES];
            for (column, slot) in slots.iter().enumerate() {
                control(GlaExecutionEvent::Work)?;
                key[column] = bindings[slot.ordinal() as usize]
                    .expect("identity-only plans cannot contain nullable bindings");
            }
            let key = &key[..slots.len()];
            if !projected.should_retain(key, control)? {
                return Ok(());
            }
            // One set entry plus every owned cell is charged before growth.
            // A refusal drops the private prefix; it never releases a short row.
            control(GlaExecutionEvent::ScratchEntry)?;
            let mut values = Vec::new();
            for value in key {
                control(GlaExecutionEvent::ScratchEntry)?;
                values.push(*value);
            }
            projected.insert(GraphBindingRow {
                values: values.into_boxed_slice(),
            });
            Ok(())
        }
    }

    impl PropertyProjection for VId {
        fn collect_properties<'a, E>(
            operator: &GlaOperator,
            bindings: &[Option<VId>],
            projected: &mut ProjectedRows<Self>,
            _property: &mut impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
            control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
        ) -> Result<(), E> {
            Self::collect(operator, bindings, projected, control)
        }
    }

    impl PropertyProjection for GraphBindingRow {
        fn collect_properties<'a, E>(
            operator: &GlaOperator,
            bindings: &[Option<VId>],
            projected: &mut ProjectedRows<Self>,
            _property: &mut impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
            control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
        ) -> Result<(), E> {
            Self::collect(operator, bindings, projected, control)
        }
    }

    impl PropertyProjection for GraphValueRow {
        fn collect_properties<'a, E>(
            operator: &GlaOperator,
            bindings: &[Option<VId>],
            projected: &mut ProjectedRows<Self>,
            property: &mut impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
            control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
        ) -> Result<(), E> {
            let GlaOperator::ProjectValues { columns } = operator else {
                unreachable!("the private value-plan constructor owns its projection shape")
            };
            super::super::values::collect_values(columns, bindings, projected, property, control)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::sealed::Projection;
    use super::*;
    use crate::algebra::BindingSlot;

    #[test]
    fn tuple_collection_keeps_correlations_and_deduplicates_complete_rows() {
        let op = GlaOperator::ProjectBindings {
            slots: vec![BindingSlot(1), BindingSlot(0)],
        };
        let mut rows = ProjectedRows::new(true);
        let mut scratch = 0;
        for bindings in [[VId(1), VId(3)], [VId(2), VId(3)], [VId(1), VId(3)]] {
            GraphBindingRow::collect(&op, &bindings.map(Some), &mut rows, &mut |event| {
                scratch += usize::from(event == GlaExecutionEvent::ScratchEntry);
                Ok::<_, ()>(())
            })
            .unwrap();
        }
        assert_eq!(
            rows.iter().map(|r| r.values()).collect::<Vec<_>>(),
            vec![&[VId(3), VId(1)][..], &[VId(3), VId(2)][..]]
        );
        assert_eq!(scratch, 6, "two rows, each one entry plus two owned cells");
        assert!(!format!("{rows:?}").contains("VId"));
        let first = rows.first().unwrap();
        assert_eq!(first.get(0), Some(VId(3)));
        assert_eq!(first.get(2), None);
        assert_eq!(first.len(), 2);
        assert!(!first.is_empty());
    }

    #[test]
    fn every_tuple_checkpoint_refuses_before_publishing_an_incomplete_row() {
        let op = GlaOperator::ProjectBindings {
            slots: vec![BindingSlot(0), BindingSlot(1)],
        };
        for stop in 1..=5 {
            let mut rows = ProjectedRows::new(true);
            let mut calls = 0;
            let result = GraphBindingRow::collect(
                &op,
                &[Some(VId(1)), Some(VId(2))],
                &mut rows,
                &mut |_| {
                    calls += 1;
                    if calls == stop { Err(stop) } else { Ok(()) }
                },
            );
            assert_eq!(result, Err(stop));
            assert_eq!(calls, stop);
            assert!(rows.is_empty());
        }
    }
}

#[cfg(test)]
mod factorization_tests {
    use super::*;
    use crate::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphAggregateText};
    use fgdb_delta_types::RelationId;

    #[test]
    fn aggregate_prefix_preserves_columns_and_cannot_remove_observed_constraints() {
        for (text, expected) in [
            ("MATCH (a)-[:R]->(b)-[:S]->(c) RETURN COUNT(*) AS n", Some(1)),
            ("MATCH (a)-[:R]->(b)-[:S]->(c) RETURN COUNT(c) AS n", None),
            ("MATCH (a)-[:R]->(b)-[:S]->(c) WHERE a=c RETURN COUNT(*) AS n", None),
            ("MATCH (a)-[:R]->(b)-[:S]->(a),(b)-[:R]->(c) RETURN COUNT(*) AS n", Some(2)),
            ("MATCH (a)-[:R]->(b)-[:S]->(c:L) RETURN COUNT(*) AS n", None),
            ("MATCH (a)-[:R]->(b)-[:S]->(c) RETURN COUNT(c.n) AS n", None),
            ("MATCH (a)-[:R]->(b) OPTIONAL MATCH (b)-[:S]->(c) RETURN COUNT(*) AS n", None),
        ] {
            let aggregate = PreparedGraphAggregateText::prepare(text, |kind, name| match kind {
                GraphSymbolKind::Relation => Some(GraphSymbol::Relation(RelationId(if name == "R" { 1 } else { 2 }))),
                GraphSymbolKind::Property => Some(GraphSymbol::Property(PropertyKeyId(1))),
                GraphSymbolKind::Label => Some(GraphSymbol::Label(fgdb_delta_types::LabelId(1))),
            }).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
            let plan = aggregate.input_pattern().plan();
            let original = plan.canonical_bytes();
            let prefix = plan.aggregate_prefix(&mut |_| Ok::<_, ()>(())).unwrap();
            assert_eq!(prefix.as_ref().map(|(_, retained)| *retained), expected, "{text}");
            if let Some((prefix, _)) = prefix {
                assert_eq!(prefix.operators().last(), plan.operators().last());
                assert_eq!(prefix.operators().iter().find(|op| matches!(op, GlaOperator::ProjectValues { .. })),
                    plan.operators().iter().find(|op| matches!(op, GlaOperator::ProjectValues { .. })));
                assert!(prefix.operators().len() < plan.operators().len());
            }
            assert_eq!(plan.canonical_bytes(), original);
        }
    }
}
