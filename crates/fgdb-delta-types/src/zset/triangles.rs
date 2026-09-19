//! Exact incremental triangles in the undirected projection of an edge bag.
//!
//! Endpoints are normalized before integration: opposite directions and
//! parallel edges add to the same unordered pair. Self-loop counts are retained
//! and validated, but a triangle always has three different vertices. DISTINCT
//! counts vertex triples; ALL counts independent edge choices on their three
//! sides. This is not directed-cycle counting or six ordered embeddings.
//!
//! Only neighborhoods incident to changed pairs are probed. A triangle belongs
//! to its first changed side, and its derivative is final product minus old
//! product, including all simultaneous-change cross terms. Preparation owns
//! every recoverable refusal; dropping the guard leaves counts, adjacency and
//! total unchanged. Memory/work admission is in logical entries, not bytes;
//! key payload/clone/comparison and standard allocation have the Z-set boundary.

pub mod committed;

use super::{ZSet, ZSetError, ZSetEvent, event};
use crate::{LimbLimit, ZWeight};
use std::collections::{BTreeMap, BTreeSet};

type Neighbors<V> = BTreeMap<V, BTreeSet<V>>;
type Weights<V> = BTreeMap<(V, V), ZWeight>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriangleQuantifier {
    /// Each unordered triple with three supported sides has multiplicity one.
    Distinct,
    /// Multiply the three exact unordered-edge multiplicities.
    All,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TriangleError<E> {
    Delta(ZSetError<E>),
    NegativeMultiplicity,
}
impl<E> From<ZSetError<E>> for TriangleError<E> {
    fn from(error: ZSetError<E>) -> Self {
        Self::Delta(error)
    }
}
impl<E: core::fmt::Display> core::fmt::Display for TriangleError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Delta(error) => error.fmt(f),
            Self::NegativeMultiplicity => f.write_str("negative integrated triangle edge count"),
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for TriangleError<E> {}

/// Exact, in-process triangle maintainer. No result-triple table is retained:
/// callers may integrate the returned derivative into a downstream Z-set, while
/// the operator retains only edge counts, symmetric adjacency and one total.
#[derive(PartialEq, Eq)]
pub struct IncrementalTriangles<V: Ord> {
    quantifier: TriangleQuantifier,
    edges: ZSet<(V, V)>,
    neighbors: Neighbors<V>,
    total: ZWeight,
}
impl<V: Ord> IncrementalTriangles<V> {
    pub fn new(quantifier: TriangleQuantifier) -> Self {
        Self {
            quantifier,
            edges: ZSet::new(),
            neighbors: BTreeMap::new(),
            total: ZWeight::ZERO,
        }
    }
    pub fn quantifier(&self) -> TriangleQuantifier {
        self.quantifier
    }
    /// Total triangle occurrences, exact even above i128::MAX.
    pub fn total(&self) -> &ZWeight {
        &self.total
    }
    /// Retained unordered edge support, including validated self loops.
    pub fn edge_rows(&self) -> impl Iterator<Item = (&(V, V), &ZWeight)> {
        self.edges.iter()
    }
}
impl<V: Ord> core::fmt::Debug for IncrementalTriangles<V> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IncrementalTriangles")
            .field("quantifier", &self.quantifier)
            .field("edge_support", &self.edges.len())
            .field("data", &"[REDACTED]")
            .finish()
    }
}

fn canonical<V: Ord + Clone>(a: &V, b: &V) -> (V, V) {
    if a <= b {
        (a.clone(), b.clone())
    } else {
        (b.clone(), a.clone())
    }
}
fn adjacent<V: Ord>(old: &Neighbors<V>, inserted: &Neighbors<V>, a: &V, b: &V) -> bool {
    old.get(a).is_some_and(|row| row.contains(b))
        || inserted.get(a).is_some_and(|row| row.contains(b))
}
fn candidates<'a, V: Ord>(
    old: &'a Neighbors<V>,
    inserted: &'a Neighbors<V>,
    a: &V,
) -> impl Iterator<Item = &'a V> {
    old.get(a)
        .into_iter()
        .flatten()
        .chain(inserted.get(a).into_iter().flatten())
}
fn degree<V: Ord>(old: &Neighbors<V>, inserted: &Neighbors<V>, a: &V) -> u128 {
    old.get(a).map_or(0, |row| row.len() as u128)
        + inserted.get(a).map_or(0, |row| row.len() as u128)
}
fn stage_neighbor<V: Ord + Clone, E>(
    inserted: &mut Neighbors<V>,
    a: &V,
    b: &V,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<(), ZSetError<E>> {
    event(control, ZSetEvent::Work)?;
    // Conservative staging and eventual retained group/member reservations.
    for _ in 0..4 {
        event(control, ZSetEvent::ScratchEntry)?;
    }
    inserted.entry(a.clone()).or_default().insert(b.clone());
    Ok(())
}
fn remove_neighbor<V: Ord>(neighbors: &mut Neighbors<V>, a: &V, b: &V) {
    if let Some(row) = neighbors.get_mut(a) {
        row.remove(b);
        if row.is_empty() {
            neighbors.remove(a);
        }
    }
}

impl<V: Ord + Clone> IncrementalTriangles<V> {
    pub fn edge_weight(&self, a: &V, b: &V) -> Option<&ZWeight> {
        self.edges.weight(&canonical(a, b))
    }

    /// Integrate a whole signed delta, refusing a negative final unordered-pair
    /// count. Insertions and retractions of opposite orientations consolidate
    /// before that check. This kernel does not authenticate individual EIds;
    /// identity/lifetime validation belongs to the committed-input adapter.
    pub fn prepare<E>(
        &mut self,
        delta: &ZSet<(V, V)>,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<TriangleUpdate<'_, V>, TriangleError<E>> {
        event(control, ZSetEvent::Work)?;
        let normalized = delta.map(|(a, b)| Ok(canonical(a, b)), limbs, control)?;
        let weights = self
            .edges
            .prepare_integration(&normalized, limbs, control)?;
        let mut effective = BTreeSet::new();
        let mut inserted = Neighbors::new();
        for (edge, next) in &weights {
            event(control, ZSetEvent::Work)?;
            if next < &ZWeight::ZERO {
                return Err(TriangleError::NegativeMultiplicity);
            }
            let old_present = self.edges.weight(edge).is_some();
            let new_present = !next.is_zero();
            if !old_present && new_present {
                event(control, ZSetEvent::ScratchEntry)?;
            }
            if edge.0 == edge.1 {
                continue;
            }
            if !old_present && new_present {
                stage_neighbor(&mut inserted, &edge.0, &edge.1, control)?;
                stage_neighbor(&mut inserted, &edge.1, &edge.0, control)?;
            }
            if self.quantifier == TriangleQuantifier::All || old_present != new_present {
                event(control, ZSetEvent::ScratchEntry)?;
                effective.insert(edge.clone());
            }
        }
        let mut output = ZSet::new();
        for edge in &effective {
            event(control, ZSetEvent::Work)?;
            let (a, b) = edge;
            let (scan, probe) =
                if degree(&self.neighbors, &inserted, a) <= degree(&self.neighbors, &inserted, b) {
                    (a, b)
                } else {
                    (b, a)
                };
            // Old and inserted neighbor sets are disjoint. Removed sides stay
            // in the candidate union, so disappearing triangles are visited too.
            for third in candidates(&self.neighbors, &inserted, scan) {
                event(control, ZSetEvent::Work)?;
                if third == a || third == b || !adjacent(&self.neighbors, &inserted, probe, third) {
                    continue;
                }
                let (x, y, z) = if third < a {
                    (third, a, b)
                } else if third < b {
                    (a, third, b)
                } else {
                    (a, b, third)
                };
                for _ in 0..3 {
                    event(control, ZSetEvent::ScratchEntry)?;
                }
                let sides = [
                    (x.clone(), y.clone()),
                    (x.clone(), z.clone()),
                    (y.clone(), z.clone()),
                ];
                let mut first = None;
                for side in &sides {
                    event(control, ZSetEvent::Work)?;
                    if effective.contains(side) {
                        first = Some(side);
                        break;
                    }
                }
                if first != Some(edge) {
                    continue;
                }
                let old = self.triangle_weight(&sides, None, limbs, control)?;
                let next = self.triangle_weight(&sides, Some(&weights), limbs, control)?;
                event(control, ZSetEvent::Work)?;
                let inverse = old.checked_neg(limbs).map_err(ZSetError::Arithmetic)?;
                event(control, ZSetEvent::Work)?;
                let change = next
                    .checked_add(&inverse, limbs)
                    .map_err(ZSetError::Arithmetic)?;
                if !change.is_zero() {
                    event(control, ZSetEvent::ScratchEntry)?;
                    output.accumulate((x.clone(), y.clone(), z.clone()), change, limbs, control)?;
                }
            }
        }
        let change = output.total_weight(limbs, control)?;
        event(control, ZSetEvent::Work)?;
        let total = self
            .total
            .checked_add(&change, limbs)
            .map_err(ZSetError::Arithmetic)?;
        debug_assert!(total >= ZWeight::ZERO);
        event(control, ZSetEvent::Work)?;
        Ok(TriangleUpdate {
            owner: self,
            weights,
            inserted,
            total,
            delta: output,
        })
    }

    pub fn apply<E>(
        &mut self,
        delta: &ZSet<(V, V)>,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<ZSet<(V, V, V)>, TriangleError<E>> {
        Ok(self.prepare(delta, limbs, control)?.commit())
    }

    fn triangle_weight<E>(
        &self,
        sides: &[(V, V); 3],
        replacements: Option<&Weights<V>>,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<ZWeight, ZSetError<E>> {
        let mut weights = [None; 3];
        for (at, edge) in sides.iter().enumerate() {
            event(control, ZSetEvent::Work)?;
            weights[at] = replacements
                .and_then(|rows| rows.get(edge))
                .or_else(|| self.edges.weight(edge));
        }
        if weights
            .iter()
            .any(|weight| weight.is_none_or(ZWeight::is_zero))
        {
            return Ok(ZWeight::ZERO);
        }
        if self.quantifier == TriangleQuantifier::Distinct {
            return Ok(ZWeight::ONE);
        }
        let mut product = ZWeight::ONE;
        for weight in weights.into_iter().flatten() {
            event(control, ZSetEvent::Work)?;
            product = product
                .checked_mul(weight, limbs)
                .map_err(ZSetError::Arithmetic)?;
        }
        Ok(product)
    }

    /// Explicit recomputation/export from retained input for auditing or a new
    /// sink. Ordinary updates never call this or scan unrelated neighborhoods.
    pub fn snapshot<E>(
        &self,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<ZSet<(V, V, V)>, TriangleError<E>> {
        let mut output = ZSet::new();
        let empty = Neighbors::new();
        for (edge, _) in self.edges.iter() {
            event(control, ZSetEvent::Work)?;
            let (a, b) = edge;
            if a == b {
                continue;
            }
            let (scan, probe) =
                if degree(&self.neighbors, &empty, a) <= degree(&self.neighbors, &empty, b) {
                    (a, b)
                } else {
                    (b, a)
                };
            for third in candidates(&self.neighbors, &empty, scan) {
                event(control, ZSetEvent::Work)?;
                if third <= b || !adjacent(&self.neighbors, &empty, probe, third) {
                    continue;
                }
                for _ in 0..3 {
                    event(control, ZSetEvent::ScratchEntry)?;
                }
                let sides = [
                    (a.clone(), b.clone()),
                    (a.clone(), third.clone()),
                    (b.clone(), third.clone()),
                ];
                let weight = self.triangle_weight(&sides, None, limbs, control)?;
                event(control, ZSetEvent::ScratchEntry)?;
                output.accumulate(
                    (a.clone(), b.clone(), third.clone()),
                    weight,
                    limbs,
                    control,
                )?;
            }
        }
        Ok(output)
    }
}

#[must_use = "dropping a triangle update aborts it"]
pub struct TriangleUpdate<'a, V: Ord> {
    owner: &'a mut IncrementalTriangles<V>,
    weights: Weights<V>,
    inserted: Neighbors<V>,
    total: ZWeight,
    delta: ZSet<(V, V, V)>,
}
impl<V: Ord> TriangleUpdate<'_, V> {
    pub fn delta(&self) -> &ZSet<(V, V, V)> {
        &self.delta
    }
    pub fn total(&self) -> &ZWeight {
        &self.total
    }
}
impl<V: Ord + Clone> TriangleUpdate<'_, V> {
    pub fn commit(self) -> ZSet<(V, V, V)> {
        let Self {
            owner,
            weights,
            inserted,
            total,
            delta,
        } = self;
        for ((a, b), weight) in &weights {
            if weight.is_zero() && a != b {
                remove_neighbor(&mut owner.neighbors, a, b);
                remove_neighbor(&mut owner.neighbors, b, a);
            }
        }
        owner.edges.publish(weights);
        for (vertex, row) in inserted {
            owner.neighbors.entry(vertex).or_default().extend(row);
        }
        owner.total = total;
        delta
    }
}
impl<V: Ord> core::fmt::Debug for TriangleUpdate<'_, V> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TriangleUpdate")
            .field("delta_support", &self.delta.len())
            .field("data", &"[REDACTED]")
            .finish()
    }
}

#[cfg(test)]
mod tests;
