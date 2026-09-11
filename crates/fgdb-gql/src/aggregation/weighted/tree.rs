//! Exact marginalization of a compiler-proved terminal binding forest.
//!
//! Every removed vertex has one incoming pattern edge and no retained use.
//! Sum over the child choices; multiply independent child subtrees. These are
//! finite nonnegative bag weights, not cardinality estimates or vertex lists.
//! An absent map entry is ZERO; an empty child map is never mistaken for a leaf.
//! Overflow is positive support until a completed COUNT requires its exact u64.
//!
//! Only weighted.rs's topology-only aggregate profile reaches this code. The
//! prefix still runs in the original GLA visitor. No new matcher, relation
//! ordering, public result semantics, source reads or durable format are added.
//! Message maps are metered private preprocessing, not spill-backed storage.

use super::*;

type Message = BTreeMap<VId, Multiplicity>;

/// For each retained binding slot, a product of its removed child messages.
/// Slots are pattern positions, not vertex identities: distinct variables may
/// bind the same VId and still make independent edge/vertex choices.
pub(super) struct Forest {
    roots: BTreeMap<usize, Message>,
}

impl Forest {
    /// Attachment bindings remain live if a later path contraction remaps the
    /// retained core. Empty messages are protected too: they denote ZERO.
    pub(super) fn anchors(&self) -> impl Iterator<Item = usize> + '_ {
        self.roots.keys().copied()
    }

    pub(super) fn build<E>(
        suffix: &[EdgeAccess],
        topology: &BTreeMap<TopologyKey, Multiplicity>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Self, E> {
        let mut roots: BTreeMap<usize, Message> = BTreeMap::new();
        // Appended slots strictly increase, so reverse edge order is a valid
        // leaf-to-root elimination order without a graph sort or recursion.
        for edge in suffix.iter().rev() {
            control(GlaExecutionEvent::Work)?;
            let descendants = roots.remove(&edge.destination);
            let mut message = Message::new();
            for (&(source, relation, destination), &weight) in topology {
                control(GlaExecutionEvent::Work)?;
                if relation != edge.relation { continue; }
                match edge.direction {
                    GlaDirection::Forward => contribute(
                        &mut message, source, destination, weight, descendants.as_ref(), control,
                    )?,
                    GlaDirection::Reverse => contribute(
                        &mut message, destination, source, weight, descendants.as_ref(), control,
                    )?,
                    GlaDirection::Undirected => {
                        contribute(&mut message, source, destination, weight, descendants.as_ref(), control)?;
                        if source != destination {
                            contribute(&mut message, destination, source, weight, descendants.as_ref(), control)?;
                        }
                    }
                }
            }
            if let Some(siblings) = roots.remove(&edge.source) {
                // A parent needs ALL children. Intersection implements zero
                // annihilation, including zero times an overflowing weight.
                let mut product = Message::new();
                for (vertex, weight) in siblings {
                    control(GlaExecutionEvent::Work)?;
                    if let Some(other) = message.get(&vertex) {
                        control(GlaExecutionEvent::ScratchEntry)?;
                        product.insert(vertex, weight.product(*other));
                    }
                }
                message = product;
            }
            // Keep even an empty map: it denotes an impossible subtree, not
            // the multiplicative identity used for a vertex without children.
            control(GlaExecutionEvent::ScratchEntry)?;
            roots.insert(edge.source, message);
        }
        Ok(Self { roots })
    }

    pub(super) fn completion<E, C>(
        &self,
        bindings: &[Option<VId>],
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), VisitError<E, C>>,
    ) -> Result<Option<Multiplicity>, VisitError<E, C>> {
        let mut weight = Multiplicity::ONE;
        for (slot, message) in &self.roots {
            control(GlaExecutionEvent::Work)?;
            let vertex = bindings.get(*slot).copied().flatten().ok_or_else(||
                GqlQueryError::Source(GraphAggregateError::MultiplicityUnavailable))?;
            let Some(factor) = message.get(&vertex) else { return Ok(None); };
            weight = weight.product(*factor);
        }
        Ok(Some(weight))
    }
}

fn contribute<E>(
    message: &mut Message,
    parent: VId,
    child: VId,
    weight: Multiplicity,
    descendants: Option<&Message>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<(), E> {
    control(GlaExecutionEvent::Work)?;
    let weight = if let Some(descendants) = descendants {
        let Some(children) = descendants.get(&child) else { return Ok(()); };
        weight.product(*children)
    } else {
        weight
    };
    if let Some(current) = message.get_mut(&parent) {
        *current = current.sum(weight);
    } else {
        control(GlaExecutionEvent::ScratchEntry)?;
        message.insert(parent, weight);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positive_sums_overflow_without_erasing_support_or_wrapping() {
        let max = Multiplicity(NonZeroU64::new(u64::MAX));
        assert_eq!(Multiplicity::ONE.sum(Multiplicity::ONE).exact_count(), Some(2));
        assert_eq!(max.sum(Multiplicity::ONE).exact_count(), None);
        assert_eq!(max.sum(Multiplicity::ONE).sum(Multiplicity::ONE).exact_count(), None);
    }

    #[test]
    fn empty_subtrees_annihilate_overflow_and_remain_distinct_from_leaves() {
        let topology = BTreeMap::from([
            ((VId(1), RelationId(1), VId(2)), Multiplicity(NonZeroU64::new(u64::MAX))),
            ((VId(2), RelationId(1), VId(3)), Multiplicity::ONE.sum(Multiplicity::ONE)),
        ]);
        let edges = [
            EdgeAccess { source: 0, destination: 2, relation: RelationId(1), direction: GlaDirection::Forward },
            EdgeAccess { source: 2, destination: 3, relation: RelationId(1), direction: GlaDirection::Forward },
            EdgeAccess { source: 2, destination: 4, relation: RelationId(9), direction: GlaDirection::Forward },
        ];
        let forest = Forest::build(&edges, &topology, &mut |_| Ok::<_, ()>(())).unwrap();
        assert!(forest.roots.contains_key(&0));
        assert!(forest.roots[&0].is_empty());
        let result = forest.completion::<(), ()>(&[Some(VId(1)), Some(VId(5))], &mut |_| Ok(())).unwrap();
        assert!(result.is_none());
        let nonempty = Forest::build(&edges[..2], &topology, &mut |_| Ok::<_, ()>(())).unwrap();
        assert_eq!(nonempty.roots[&0][&VId(1)].exact_count(), None);
        assert!(matches!(nonempty.completion::<(), ()>(&[], &mut |_| Ok(())),
            Err(GqlQueryError::Source(GraphAggregateError::MultiplicityUnavailable))));
    }
}
