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

/// Compiler-owned partition of an aggregate's binding tree. Kept slots occupy
/// [0, retained_width); removed slots occupy a disjoint tail used only by the
/// completion forest. The permutation preserves order within both partitions.
/// The plan alone is not equivalent to the original: its weights are required.
pub(crate) struct AggregateCore {
    pub(crate) plan: super::GlaPlan<GraphValueRow>,
    pub(crate) slot_map: Vec<super::BindingSlot>,
    pub(crate) retained_width: usize,
}

impl super::GlaPlan<GraphValueRow> {
    /// Internal aggregate specialization, never a public plan constructor.
    /// A forest of fresh, unobserved bindings can be replaced by completion
    /// weights even when its branches occur before or between kept expansions.
    ///
    /// Seed liveness with the original root, all output slots and BOTH sides
    /// of every identity constraint. Close it over binding parents in reverse
    /// order. Every removed component then has exactly one kept attachment and
    /// no observable use. Keep the root and constrained/cyclic core in their
    /// original traversal order; remap every source, identity and output slot.
    /// Predicates, property reads, scopes, DISTINCT and pagination refuse this
    /// specialization entirely. No fallible read can be hidden by elimination.
    pub(crate) fn aggregate_core<E>(
        &self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<AggregateCore>, E> {
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
        if columns.is_empty() || columns.len() > MAX_PATTERN_VERTICES {
            return Ok(None);
        }
        let mut parents = [0_usize; MAX_PATTERN_VERTICES];
        let mut kept = [false; MAX_PATTERN_VERTICES];
        kept[0] = true;
        kept[1] = true;
        let mut width = 2_usize;
        for operator in &operators[1..projection_at] {
            control(GlaExecutionEvent::Work)?;
            match operator {
                GlaOperator::Expand { source, .. }
                    if (source.ordinal() as usize) < width && width < MAX_PATTERN_VERTICES => {
                    parents[width] = source.ordinal() as usize;
                    width += 1;
                }
                GlaOperator::VertexIdentity { left, right, .. }
                    if (left.ordinal() as usize) < width && (right.ordinal() as usize) < width => {
                    kept[left.ordinal() as usize] = true;
                    kept[right.ordinal() as usize] = true;
                }
                _ => return Ok(None),
            }
        }
        for column in columns {
            control(GlaExecutionEvent::Work)?;
            let super::ValueProjection::Vertex { slot } = column else {
                return Ok(None);
            };
            let slot = slot.ordinal() as usize;
            if slot >= width { return Ok(None); }
            kept[slot] = true;
        }
        // Parents always have smaller original ordinals. One reverse pass is
        // the full transitive closure, including a projected deep descendant.
        for slot in (2..width).rev() {
            control(GlaExecutionEvent::Work)?;
            if kept[slot] { kept[parents[slot]] = true; }
        }
        let retained_width = kept[..width].iter().filter(|keep| **keep).count();
        if retained_width == width { return Ok(None); }

        // Removed IDs cannot reuse compact core IDs. That would merge unrelated
        // forest roots when an early branch precedes a retained later binding.
        let mut slot_map = Vec::new();
        let mut next_kept = 0;
        let mut next_removed = retained_width;
        for keep in &kept[..width] {
            control(GlaExecutionEvent::ScratchEntry)?;
            let next = if *keep { &mut next_kept } else { &mut next_removed };
            slot_map.push(super::BindingSlot(*next as u32));
            *next += 1;
        }
        debug_assert_eq!((next_kept, next_removed), (retained_width, width));
        let mut core = Vec::new();
        let mut appended = 2;
        for operator in &operators[..projection_at] {
            control(GlaExecutionEvent::Work)?;
            let remapped = match operator {
                GlaOperator::ScanEdges { .. } => operator.clone(),
                GlaOperator::Expand { source, relation, direction } => {
                    let keep = kept[appended];
                    appended += 1;
                    if !keep { continue; }
                    GlaOperator::Expand {
                        source: slot_map[source.ordinal() as usize],
                        relation: *relation,
                        direction: *direction,
                    }
                }
                GlaOperator::VertexIdentity { left, right, equal } => GlaOperator::VertexIdentity {
                    left: slot_map[left.ordinal() as usize],
                    right: slot_map[right.ordinal() as usize],
                    equal: *equal,
                },
                _ => unreachable!("the complete topology-only shape was validated above"),
            };
            control(GlaExecutionEvent::ScratchEntry)?;
            core.push(remapped);
        }
        control(GlaExecutionEvent::ScratchEntry)?;
        let mut remapped_columns = Vec::new();
        for column in columns {
            let super::ValueProjection::Vertex { slot } = column else {
                unreachable!("the complete output shape was validated above")
            };
            control(GlaExecutionEvent::ScratchEntry)?;
            remapped_columns.push(super::ValueProjection::Vertex {
                slot: slot_map[slot.ordinal() as usize],
            });
        }
        core.push(GlaOperator::ProjectValues { columns: remapped_columns });
        for operator in &operators[projection_at + 1..] {
            control(GlaExecutionEvent::ScratchEntry)?;
            core.push(operator.clone());
        }
        Ok(Some(AggregateCore { plan: Self::from_operators(core), slot_map, retained_width }))
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

    fn prepare(text: &str) -> crate::PreparedGraphAggregate {
        PreparedGraphAggregateText::prepare(text, |kind, name| match kind {
            GraphSymbolKind::Relation => Some(GraphSymbol::Relation(RelationId(
                match name { "R" => 1, "S" => 2, _ => 3 },
            ))),
            _ => None,
        }).unwrap().bind_parameters(&GqlParameters::new()).unwrap()
    }

    #[test]
    fn interleaved_branches_have_disjoint_slots_and_remapped_cycle_and_output() {
        let aggregate = prepare("MATCH (a)-[:R]->(b), (b)-[:S]->(hidden)-[:T]->(leaf), \
            (b)-[:R]->(c), (a)-[:T]->(d), (c)-[:S]->(a), (c)-[:R]->(unused) \
            RETURN c,d,COUNT(*) AS n GROUP BY c,d");
        let plan = aggregate.input_pattern().plan();
        let original = plan.canonical_bytes();
        let core = plan.aggregate_core(&mut |_| Ok::<_, ()>(())).unwrap().unwrap();
        assert_eq!(core.retained_width, 5);
        let slots: Vec<_> = core.slot_map.iter().map(|slot| slot.ordinal()).collect();
        assert_eq!(slots, vec![0, 1, 5, 6, 2, 3, 4, 7]);
        let mut permutation = slots;
        permutation.sort_unstable();
        assert_eq!(permutation, (0..8).collect::<Vec<_>>());
        assert!(core.plan.operators().iter().any(|op| matches!(op,
            GlaOperator::VertexIdentity { left, right, equal: true }
                if left.ordinal() == 0 && right.ordinal() == 4)));
        let sources: Vec<_> = core.plan.operators().iter().filter_map(|op| match op {
            GlaOperator::Expand { source, .. } => Some(source.ordinal()), _ => None,
        }).collect();
        assert_eq!(sources, vec![1, 0, 2]);
        let output: Vec<_> = core.plan.operators().iter().find_map(|op| match op {
            GlaOperator::ProjectValues { columns } => Some(columns.iter().map(|column| match column {
                super::super::ValueProjection::Vertex { slot } => slot.ordinal(),
                _ => panic!("identity-only output"),
            }).collect()),
            _ => None,
        }).unwrap();
        assert_eq!(output, vec![2, 3]);
        assert_eq!(plan.canonical_bytes(), original);
    }

    #[test]
    fn retained_deep_descendant_keeps_all_ancestors_but_not_its_sibling() {
        let aggregate = prepare("MATCH (a)-[:R]->(b), (b)-[:S]->(x), (b)-[:S]->(y), \
            (x)-[:T]->(z) RETURN z,COUNT(*) AS n GROUP BY z");
        let core = aggregate.input_pattern().plan().aggregate_core(&mut |_| Ok::<_, ()>(()))
            .unwrap().unwrap();
        assert_eq!(core.retained_width, 4);
        assert_eq!(core.slot_map.iter().map(|slot| slot.ordinal()).collect::<Vec<_>>(),
            vec![0, 1, 2, 4, 3]);
        assert_eq!(core.plan.operators().iter().filter(|op| matches!(op, GlaOperator::Expand { .. })).count(), 2);
    }

    #[test]
    fn core_compilation_is_interruptible_and_never_mutates_the_definition() {
        let aggregate = prepare("MATCH (a)-[:R]->(b), (b)-[:S]->(x), (x)-[:T]->(y), \
            (b)-[:R]->(c), (c)-[:T]->(a) RETURN c,COUNT(*) AS n GROUP BY c");
        let plan = aggregate.input_pattern().plan();
        let original = plan.canonical_bytes();
        let mut total = 0;
        assert!(plan.aggregate_core(&mut |_| { total += 1; Ok::<_, usize>(()) }).unwrap().is_some());
        for stop in 1..=total {
            let mut calls = 0;
            let result = plan.aggregate_core(&mut |_| {
                calls += 1;
                if calls == stop { Err(stop) } else { Ok(()) }
            });
            assert!(matches!(result, Err(value) if value == stop));
            assert_eq!(calls, stop);
            assert_eq!(plan.canonical_bytes(), original);
        }
    }

    #[test]
    fn aggregate_core_preserves_columns_and_cannot_remove_observed_constraints() {
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
            let reduced = plan.aggregate_core(&mut |_| Ok::<_, ()>(())).unwrap();
            assert_eq!(reduced.as_ref().map(|core| core.retained_width - 1), expected, "{text}");
            if let Some(reduced) = reduced {
                let prefix = reduced.plan;
                assert_eq!(prefix.operators().last(), plan.operators().last());
                assert_eq!(prefix.operators().iter().find(|op| matches!(op, GlaOperator::ProjectValues { .. })),
                    plan.operators().iter().find(|op| matches!(op, GlaOperator::ProjectValues { .. })));
                assert!(prefix.operators().len() < plan.operators().len());
            }
            assert_eq!(plan.canonical_bytes(), original);
        }
    }
}
