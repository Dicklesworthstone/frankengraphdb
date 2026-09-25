use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use std::sync::Arc;

use fgdb_types::VId;

use crate::ranking::{Ranked, retain_best};
use crate::{BeaconError, WorkControl};

const MAX_DIMENSIONS: usize = 65_536;
const MAX_LEVEL: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DistanceMetric {
    SquaredEuclidean,
    Cosine,
    /// Negative inner product, so lower is better for every metric.
    NegativeDotProduct,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HnswConfig {
    pub dimensions: usize,
    pub metric: DistanceMetric,
    /// Upper-layer degree; level zero allows twice this degree.
    pub m: usize,
    pub ef_construction: usize,
    pub seed: u64,
    pub max_vectors: usize,
}

impl HnswConfig {
    #[must_use]
    pub fn new(dimensions: usize, metric: DistanceMetric) -> Self {
        Self {
            dimensions,
            metric,
            m: 16,
            ef_construction: 200,
            seed: 0x4245_4143_4f4e_0001,
            max_vectors: 1_000_000,
        }
    }

    pub fn validate(&self) -> Result<(), BeaconError> {
        if self.dimensions == 0 || self.dimensions > MAX_DIMENSIONS {
            return Err(BeaconError::InvalidConfig(
                "dimensions must be in 1..=65536",
            ));
        }
        if !(2..=64).contains(&self.m) {
            return Err(BeaconError::InvalidConfig("HNSW m must be in 2..=64"));
        }
        if self.ef_construction < self.m {
            return Err(BeaconError::InvalidConfig(
                "ef_construction must be at least m",
            ));
        }
        if self.max_vectors == 0 {
            return Err(BeaconError::InvalidConfig("max_vectors must be positive"));
        }
        Ok(())
    }

    pub(crate) fn validate_vector(
        &self,
        vector: &[f32],
        work: &mut dyn WorkControl,
    ) -> Result<f64, BeaconError> {
        work.charge(1)?;
        if vector.len() != self.dimensions {
            return Err(BeaconError::Dimension {
                expected: self.dimensions,
                actual: vector.len(),
            });
        }
        work.charge(vector.len())?;
        let mut norm = 0.0;
        for (coordinate, value) in vector.iter().enumerate() {
            if !value.is_finite() {
                return Err(BeaconError::NonFinite { coordinate });
            }
            let value = f64::from(*value);
            norm += value * value;
        }
        if self.metric == DistanceMetric::Cosine && norm == 0.0 {
            return Err(BeaconError::ZeroVector);
        }
        Ok(norm)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VectorSearch {
    Approximate { ef_search: usize },
    Exact,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Neighbor {
    pub id: VId,
    pub distance: f64,
}

#[derive(Clone)]
struct Node {
    id: VId,
    vector: Arc<[f32]>,
    norm: f64,
    links: Vec<Vec<usize>>,
}

/// An immutable HNSW segment. Construction uses sorted VIds and ID-derived
/// geometric levels, so input iteration order and process entropy cannot
/// change its topology. It is not a durable object or a transaction authority.
#[derive(Clone)]
pub struct Hnsw {
    config: HnswConfig,
    nodes: Vec<Node>,
    entry: Option<usize>,
    top_level: usize,
}

impl core::fmt::Debug for Hnsw {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Hnsw")
            .field("config", &self.config)
            .field("vectors", &self.nodes.len())
            .field("top_level", &self.top_level)
            .finish_non_exhaustive()
    }
}

impl Hnsw {
    pub fn build(
        config: HnswConfig,
        vectors: impl IntoIterator<Item = (VId, Vec<f32>)>,
        work: &mut dyn WorkControl,
    ) -> Result<Self, BeaconError> {
        Self::build_shared(
            config,
            vectors
                .into_iter()
                .map(|(id, vector)| (id, Arc::from(vector))),
            work,
        )
    }

    pub(crate) fn build_shared(
        config: HnswConfig,
        vectors: impl IntoIterator<Item = (VId, Arc<[f32]>)>,
        work: &mut dyn WorkControl,
    ) -> Result<Self, BeaconError> {
        config.validate()?;
        let mut ordered = BTreeMap::new();
        for (id, vector) in vectors {
            let norm = config.validate_vector(&vector, work)?;
            if ordered.contains_key(&id) {
                return Err(BeaconError::DuplicateVertex(id));
            }
            if ordered.len() == config.max_vectors {
                return Err(BeaconError::ResourceLimit {
                    resource: "vectors per HNSW segment",
                    limit: config.max_vectors,
                });
            }
            ordered.insert(id, (vector, norm));
        }
        work.charge(ordered.len())?;
        let mut graph = Self {
            config,
            nodes: Vec::with_capacity(ordered.len()),
            entry: None,
            top_level: 0,
        };
        for (id, (vector, norm)) in ordered {
            graph.insert(id, vector, norm, work)?;
        }
        Ok(graph)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    #[must_use]
    pub fn config(&self) -> &HnswConfig {
        &self.config
    }

    /// `eligible` is a row-selection/visibility predicate, NOT authorization.
    /// Ineligible nodes remain traversable so a deleted version or a selective
    /// predicate cannot sever the routing graph. They never occupy result slots.
    /// Security-domain narrowing must happen before constructing this segment.
    pub fn search(
        &self,
        query: &[f32],
        k: usize,
        mode: VectorSearch,
        eligible: impl Fn(VId) -> bool,
        work: &mut dyn WorkControl,
    ) -> Result<Vec<Neighbor>, BeaconError> {
        let norm = self.config.validate_vector(query, work)?;
        if matches!(mode, VectorSearch::Approximate { ef_search: 0 }) {
            return Err(BeaconError::InvalidQuery("ef_search must be positive"));
        }
        let k = k.min(self.nodes.len());
        if k == 0 {
            return Ok(Vec::new());
        }
        let mut ranked = match mode {
            VectorSearch::Exact => {
                let mut best = BinaryHeap::new();
                for slot in 0..self.nodes.len() {
                    work.charge(1)?;
                    if eligible(self.nodes[slot].id) {
                        retain_best(&mut best, self.candidate(query, norm, slot, work)?, k);
                    }
                }
                best.into_sorted_vec()
            }
            VectorSearch::Approximate { ef_search } => {
                let Some(entry) = self.entry else {
                    return Ok(Vec::new());
                };
                let mut nearest = self.candidate(query, norm, entry, work)?;
                for layer in (1..=self.top_level).rev() {
                    nearest = self.greedy(query, norm, nearest, layer, work)?;
                }
                self.search_layer(
                    query,
                    norm,
                    nearest.slot,
                    0,
                    ef_search.max(k).min(self.nodes.len()),
                    &eligible,
                    work,
                )?
            }
        };
        ranked.truncate(k);
        Ok(ranked
            .into_iter()
            .map(|hit| Neighbor {
                id: hit.id,
                distance: hit.cost,
            })
            .collect())
    }

    fn candidate(
        &self,
        query: &[f32],
        norm: f64,
        slot: usize,
        work: &mut dyn WorkControl,
    ) -> Result<Ranked, BeaconError> {
        let node = &self.nodes[slot];
        Ok(Ranked {
            cost: self.distance(query, norm, &node.vector, node.norm, work)?,
            id: node.id,
            slot,
        })
    }

    fn distance(
        &self,
        left: &[f32],
        left_norm: f64,
        right: &[f32],
        right_norm: f64,
        work: &mut dyn WorkControl,
    ) -> Result<f64, BeaconError> {
        work.charge(self.config.dimensions)?;
        let mut sum = 0.0f64;
        // Deliberately scalar, fixed-order f64 accumulation; no FMA or parallel
        // reduction. f32 inputs and bounded dimensions cannot overflow f64.
        for (&left, &right) in left.iter().zip(right) {
            let left = f64::from(left);
            let right = f64::from(right);
            sum += match self.config.metric {
                DistanceMetric::SquaredEuclidean => (left - right) * (left - right),
                DistanceMetric::Cosine | DistanceMetric::NegativeDotProduct => left * right,
            };
        }
        let cost = match self.config.metric {
            DistanceMetric::SquaredEuclidean => sum,
            DistanceMetric::NegativeDotProduct => -sum,
            DistanceMetric::Cosine => {
                1.0 - (sum / (left_norm.sqrt() * right_norm.sqrt())).clamp(-1.0, 1.0)
            }
        };
        Ok(if cost == 0.0 { 0.0 } else { cost })
    }

    fn greedy(
        &self,
        query: &[f32],
        norm: f64,
        mut nearest: Ranked,
        layer: usize,
        work: &mut dyn WorkControl,
    ) -> Result<Ranked, BeaconError> {
        loop {
            let old = nearest.slot;
            for &slot in &self.nodes[old].links[layer] {
                work.charge(1)?;
                let candidate = self.candidate(query, norm, slot, work)?;
                if candidate < nearest {
                    nearest = candidate;
                }
            }
            if nearest.slot == old {
                return Ok(nearest);
            }
        }
    }

    fn search_layer(
        &self,
        query: &[f32],
        norm: f64,
        entry: usize,
        layer: usize,
        ef: usize,
        eligible: &impl Fn(VId) -> bool,
        work: &mut dyn WorkControl,
    ) -> Result<Vec<Ranked>, BeaconError> {
        let first = self.candidate(query, norm, entry, work)?;
        let mut visited = BTreeSet::from([entry]);
        let mut candidates = BinaryHeap::from([Reverse(first)]);
        let mut best = BinaryHeap::<Ranked>::new();
        if eligible(first.id) {
            best.push(first);
        }
        while let Some(Reverse(current)) = candidates.pop() {
            work.charge(1)?;
            if best.len() == ef && best.peek().is_some_and(|worst| current.cost > worst.cost) {
                break;
            }
            for &slot in &self.nodes[current.slot].links[layer] {
                work.charge(1)?;
                if !visited.insert(slot) {
                    continue;
                }
                let next = self.candidate(query, norm, slot, work)?;
                if best.len() < ef || best.peek().is_some_and(|worst| next.cost <= worst.cost) {
                    candidates.push(Reverse(next));
                    if eligible(next.id) {
                        retain_best(&mut best, next, ef);
                    }
                }
            }
        }
        Ok(best.into_sorted_vec())
    }

    fn insert(
        &mut self,
        id: VId,
        vector: Arc<[f32]>,
        norm: f64,
        work: &mut dyn WorkControl,
    ) -> Result<(), BeaconError> {
        work.charge(1)?;
        let level = level_for(id.0, self.config.seed, self.config.m);
        let slot = self.nodes.len();
        let Some(entry) = self.entry else {
            self.nodes.push(Node {
                id,
                vector,
                norm,
                links: vec![Vec::new(); level + 1],
            });
            self.entry = Some(slot);
            self.top_level = level;
            return Ok(());
        };
        let mut nearest = self.candidate(&vector, norm, entry, work)?;
        for layer in (level + 1..=self.top_level).rev() {
            nearest = self.greedy(&vector, norm, nearest, layer, work)?;
        }
        self.nodes.push(Node {
            id,
            vector: Arc::clone(&vector),
            norm,
            links: vec![Vec::new(); level + 1],
        });
        for layer in (0..=level.min(self.top_level)).rev() {
            let candidates = self.search_layer(
                &vector,
                norm,
                nearest.slot,
                layer,
                self.config.ef_construction.min(slot).max(1),
                &|_| true,
                work,
            )?;
            if let Some(first) = candidates.first() {
                nearest = *first;
            }
            let neighbours = self.select_diverse(candidates, self.config.m, work)?;
            self.nodes[slot].links[layer] = neighbours.clone();
            for neighbour in neighbours {
                self.nodes[neighbour].links[layer].push(slot);
                let limit = if layer == 0 {
                    2 * self.config.m
                } else {
                    self.config.m
                };
                if self.nodes[neighbour].links[layer].len() > limit {
                    self.prune(neighbour, layer, limit, work)?;
                }
            }
        }
        if level > self.top_level {
            self.entry = Some(slot);
            self.top_level = level;
        }
        Ok(())
    }

    fn select_diverse(
        &self,
        candidates: Vec<Ranked>,
        limit: usize,
        work: &mut dyn WorkControl,
    ) -> Result<Vec<usize>, BeaconError> {
        let mut selected: Vec<usize> = Vec::new();
        let mut rejected = Vec::new();
        for candidate in candidates {
            if selected.len() == limit {
                break;
            }
            let node = &self.nodes[candidate.slot];
            let mut diverse = true;
            for &slot in &selected {
                let other = &self.nodes[slot];
                if self.distance(&node.vector, node.norm, &other.vector, other.norm, work)?
                    < candidate.cost
                {
                    diverse = false;
                    break;
                }
            }
            if diverse {
                selected.push(candidate.slot);
            } else {
                rejected.push(candidate.slot);
            }
        }
        // Keep the nearest pruned candidates when the diversity test yields
        // fewer than M neighbours, rather than leaving a needlessly sparse graph.
        selected.extend(rejected.into_iter().take(limit - selected.len()));
        Ok(selected)
    }

    fn prune(
        &mut self,
        slot: usize,
        layer: usize,
        limit: usize,
        work: &mut dyn WorkControl,
    ) -> Result<(), BeaconError> {
        let vector = Arc::clone(&self.nodes[slot].vector);
        let norm = self.nodes[slot].norm;
        let mut candidates = Vec::new();
        for &other in &self.nodes[slot].links[layer] {
            candidates.push(self.candidate(&vector, norm, other, work)?);
        }
        candidates.sort_unstable();
        candidates.dedup_by_key(|candidate| candidate.slot);
        self.nodes[slot].links[layer] = self.select_diverse(candidates, limit, work)?;
        Ok(())
    }
}

fn mix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn level_for(id: u128, seed: u64, m: usize) -> usize {
    // VId is a full 128-bit logical identity. Both halves must influence the
    // level distribution; truncation makes whole issuer ranges degenerate.
    let low = id as u64;
    let high = (id >> 64) as u64;
    let mut state = mix64(mix64(low ^ seed) ^ mix64(high ^ 0x4245_4143_4f4e_4944));
    let mut level = 0;
    while level < MAX_LEVEL && state.is_multiple_of(m as u64) {
        level += 1;
        state = mix64(state);
    }
    level
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WorkBudget;

    #[test]
    fn high_identity_bits_participate_in_level_assignment() {
        let levels: BTreeSet<_> = (0..512u128)
            .map(|high| level_for((high << 64) | 7, 42, 4))
            .collect();
        assert!(levels.len() > 2);
    }

    #[test]
    fn graph_has_bounded_valid_links_and_stable_topology() {
        let rows: Vec<_> = (0..128)
            .map(|id| (VId(id), vec![id as f32, (id % 7) as f32]))
            .collect();
        let config = HnswConfig::new(2, DistanceMetric::SquaredEuclidean);
        let left = Hnsw::build(
            config.clone(),
            rows.clone(),
            &mut WorkBudget::new(20_000_000),
        )
        .unwrap();
        let right = Hnsw::build(
            config.clone(),
            rows.into_iter().rev(),
            &mut WorkBudget::new(20_000_000),
        )
        .unwrap();
        for (slot, (a, b)) in left.nodes.iter().zip(&right.nodes).enumerate() {
            assert_eq!(a.links, b.links);
            for (layer, links) in a.links.iter().enumerate() {
                assert!(links.len() <= if layer == 0 { 2 * config.m } else { config.m });
                assert_eq!(
                    links.iter().copied().collect::<BTreeSet<_>>().len(),
                    links.len()
                );
                for &other in links {
                    assert_ne!(slot, other);
                    assert!(other < left.nodes.len());
                    assert!(left.nodes[other].links.len() > layer);
                }
            }
        }
    }
}
