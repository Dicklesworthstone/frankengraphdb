//! Exact, discardable cardinalities owned by one immutable snapshot generation.
//! Only unrestricted current-cut counts use them. They never replace source
//! admission, authorize a hidden population, or supply a historical estimate.

use super::{IndexMap, SourceEvent};
use fgdb_delta_types::{DeltaRow, LabelId, RelationId};
use fgdb_gql::GraphCountSource;
use fgdb_strata::writer::BlockWriter;
use std::sync::OnceLock;

// Bound retained metadata independently of both edge and vertex cardinality.
const MAX_COUNTERS: usize = 4096;
type Relations = Option<IndexMap<RelationId, u64>>;
type Labels = Option<IndexMap<LabelId, u64>>;

#[derive(Clone, Debug, Default)]
pub(crate) struct ExactCounts {
    vertices: OnceLock<u64>,
    edges: OnceLock<u64>,
    relations: OnceLock<Relations>,
    labels: OnceLock<Labels>,
}

impl ExactCounts {
    pub(crate) fn new(empty: bool) -> Self {
        if empty {
            Self {
                vertices: OnceLock::from(0),
                edges: OnceLock::from(0),
                relations: OnceLock::from(Some(IndexMap::new())),
                labels: OnceLock::from(Some(IndexMap::new())),
            }
        } else {
            Self::default()
        }
    }

    /// Collect a complete unrestricted source without retaining its records.
    /// A fully known generation needs no second builder. Oversized maps remain
    /// unavailable until a topology change permits the usual bounded rebuild.
    pub(crate) fn collector(&self, edges: bool) -> Option<CountCollector> {
        let labels = self.label_builder();
        let relations = edges.then(|| self.relation_builder()).flatten();
        if self.vertices.get().is_some()
            && (!edges || self.edges.get().is_some())
            && labels.is_none()
            && relations.is_none()
        {
            return None;
        }
        Some(CountCollector {
            vertices: 0,
            edges: edges.then_some(0),
            labels,
            relations,
        })
    }

    /// Publish only a completed, privately built summary. The authorized host
    /// retains it until final signed delivery succeeds; failures drop it.
    /// Every value remains attached to its original immutable generation.
    pub(crate) fn publish(&self, counts: Self) {
        if let Some(vertices) = counts.vertices.into_inner() {
            let _ = self.vertices.set(vertices);
        }
        if let Some(edges) = counts.edges.into_inner() {
            let _ = self.edges.set(edges);
        }
        if let Some(relations) = counts.relations.into_inner() {
            let _ = self.relations.set(relations);
        }
        if let Some(labels) = counts.labels.into_inner() {
            let _ = self.labels.set(labels);
        }
    }

    pub(super) fn lookup<E>(
        &self,
        source: GraphCountSource,
        control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
    ) -> Result<Option<u64>, E> {
        let value = match source {
            GraphCountSource::Vertices => self.vertices.get().copied(),
            GraphCountSource::Edges => self.edges.get().copied(),
            GraphCountSource::Label(label) => {
                let Some(Some(index)) = self.labels.get() else {
                    return Ok(None);
                };
                Some(lookup_key(index, label, control)?)
            }
            GraphCountSource::Relation(relation) => {
                let Some(Some(index)) = self.relations.get() else {
                    return Ok(None);
                };
                Some(lookup_key(index, relation, control)?)
            }
        };
        if value.is_some() {
            control(SourceEvent::Work)?;
            control(SourceEvent::SnapshotRecord)?;
        }
        Ok(value)
    }

    /// Read one exact metadata record while retaining the live vertex total
    /// needed by an authorized host's node budget. An unknown population must
    /// keep ordinary admission; it is never a zero-node metadata shortcut.
    pub(crate) fn lookup_with_vertices<E>(
        &self,
        source: GraphCountSource,
        control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
    ) -> Result<Option<(u64, u64)>, E> {
        let Some(vertices) = self.vertices.get().copied() else {
            return Ok(None);
        };
        self.lookup(source, control)
            .map(|matching| matching.map(|matching| (vertices, matching)))
    }

    pub(super) fn relation_builder(&self) -> Option<RelationBuilder> {
        self.relations
            .get()
            .is_none()
            .then(RelationBuilder::default)
    }

    pub(super) fn label_builder(&self) -> Option<LabelBuilder> {
        self.labels.get().is_none().then(LabelBuilder::default)
    }

    // Called only after the whole governed query, including its output, has
    // succeeded. A cancelled/refused scan never publishes partial metadata.
    pub(super) fn publish_vertices(&self, count: u64) {
        let _ = self.vertices.set(count);
    }

    pub(super) fn publish_edges(&self, count: u64) {
        let _ = self.edges.set(count);
    }

    pub(super) fn publish_relations(&self, counts: Relations) {
        let _ = self.relations.set(counts);
    }

    pub(super) fn publish_labels(&self, counts: Labels) {
        let _ = self.labels.set(counts);
    }

    /// Update a private successor immediately BEFORE the same row is folded.
    /// Delete before-images come from the existing writer, not a second
    /// element map. Publication of this successor follows successful folding
    /// and durable root publication. Any inconsistency discards the cache.
    pub(crate) fn apply_before(&mut self, writer: &BlockWriter, row: &DeltaRow) {
        if let Some(previous) = self.vertices.get().copied() {
            let next = match row {
                DeltaRow::CreateVertex { .. } => previous.checked_add(1),
                DeltaRow::DeleteVertex { vid, .. } if writer.is_vertex_live(*vid) => {
                    previous.checked_sub(1)
                }
                DeltaRow::DeleteVertex { .. } => None,
                _ => Some(previous),
            };
            self.vertices = next.map_or_else(OnceLock::new, OnceLock::from);
        }
        self.apply_edges_before(writer, row);
        self.apply_labels_before(writer, row);
        let Some(Some(counts)) = self.relations.get_mut() else {
            // A topology change can bring an oversized relation population
            // back under the bound. Permit one new governed rebuild then.
            if matches!(self.relations.get(), Some(None))
                && matches!(
                    row,
                    DeltaRow::CreateEdge { .. }
                        | DeltaRow::DeleteEdge { .. }
                        | DeltaRow::DeleteVertex { .. }
                )
            {
                self.relations = OnceLock::new();
            }
            return;
        };
        let mut work = 0;
        let valid = match row {
            DeltaRow::CreateEdge { relation, .. } => increment(counts, *relation, &mut work),
            DeltaRow::DeleteEdge { eid, .. } => writer
                .live_edge(*eid)
                .is_some_and(|(_, relation, _, _)| decrement(counts, relation, &mut work)),
            DeltaRow::DeleteVertex {
                sorted_retired_incident_edges,
                ..
            } => sorted_retired_incident_edges.iter().all(|eid| {
                writer
                    .live_edge(*eid)
                    .is_some_and(|(_, relation, _, _)| decrement(counts, relation, &mut work))
            }),
            _ => true,
        };
        if !valid {
            // Unknown, never zero. A later successful source scan may rebuild.
            self.relations = OnceLock::new();
        }
    }

    fn apply_edges_before(&mut self, writer: &BlockWriter, row: &DeltaRow) {
        let Some(previous) = self.edges.get().copied() else {
            return;
        };
        let next = match row {
            DeltaRow::CreateEdge { .. } => previous.checked_add(1),
            DeltaRow::DeleteEdge { eid, .. } => {
                writer.live_edge(*eid).and_then(|_| previous.checked_sub(1))
            }
            DeltaRow::DeleteVertex {
                vid,
                sorted_retired_incident_edges,
                ..
            } if writer.is_vertex_live(*vid) => sorted_retired_incident_edges
                .iter()
                .try_fold(previous, |count, eid| {
                    writer.live_edge(*eid).and_then(|_| count.checked_sub(1))
                }),
            DeltaRow::DeleteVertex { .. } => None,
            _ => Some(previous),
        };
        // Total cardinality has no distinct-relation bound. It remains exact
        // even when the separately retained per-type counters are unavailable.
        self.edges = next.map_or_else(OnceLock::new, OnceLock::from);
    }

    fn apply_labels_before(&mut self, writer: &BlockWriter, row: &DeltaRow) {
        let Some(Some(counts)) = self.labels.get_mut() else {
            if matches!(self.labels.get(), Some(None))
                && matches!(
                    row,
                    DeltaRow::CreateVertex { .. }
                        | DeltaRow::DeleteVertex { .. }
                        | DeltaRow::LabelMembership { .. }
                )
            {
                self.labels = OnceLock::new();
            }
            return;
        };
        let mut work = 0;
        let valid = match row {
            DeltaRow::CreateVertex { labels, .. } => labels
                .iter()
                .all(|label| increment(counts, *label, &mut work)),
            DeltaRow::DeleteVertex { vid, .. } => {
                writer.live_vertex_labels(*vid).is_some_and(|labels| {
                    labels
                        .iter()
                        .all(|label| decrement(counts, *label, &mut work))
                })
            }
            DeltaRow::LabelMembership {
                vid,
                label,
                before,
                after,
            } => writer.live_vertex_labels(*vid).is_some_and(|labels| {
                let member = labels.binary_search(label).is_ok();
                if member != *before {
                    return false;
                }
                member == *after
                    || if *after {
                        increment(counts, *label, &mut work)
                    } else {
                        decrement(counts, *label, &mut work)
                    }
            }),
            _ => true,
        };
        if !valid {
            self.labels = OnceLock::new();
        }
    }
}

/// The caller must feed each live unrestricted vertex once and, for an edge
/// source, every live edge occurrence. This builder never chooses a scope.
pub(crate) struct CountCollector {
    vertices: u64,
    edges: Option<u64>,
    labels: Option<LabelBuilder>,
    relations: Option<RelationBuilder>,
}

impl CountCollector {
    pub(crate) fn vertex<E>(
        &mut self,
        labels: &[LabelId],
        control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        control(SourceEvent::Work)?;
        self.vertices += 1;
        if let Some(builder) = &mut self.labels {
            for label in labels {
                control(SourceEvent::Work)?;
                builder.observe(*label, control)?;
            }
        }
        Ok(())
    }

    pub(crate) fn edges<E>(
        &mut self,
        relation: RelationId,
        count: u64,
        control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        control(SourceEvent::Work)?;
        let total = self.edges.as_mut().expect("the source admitted edges");
        *total = total
            .checked_add(count)
            .expect("the complete in-memory edge population fits u64");
        if let Some(builder) = &mut self.relations {
            builder.observe_many(relation, count, control)?;
        }
        Ok(())
    }

    pub(crate) fn finish<E>(
        self,
        control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
    ) -> Result<ExactCounts, E> {
        let counts = ExactCounts::default();
        counts.publish_vertices(self.vertices);
        if let Some(edges) = self.edges {
            counts.publish_edges(edges);
        }
        if let Some(builder) = self.relations {
            counts.publish_relations(builder.finish(control)?);
        }
        if let Some(builder) = self.labels {
            counts.publish_labels(builder.finish(control)?);
        }
        Ok(counts)
    }
}

fn lookup_key<K: Ord, E>(
    index: &IndexMap<K, u64>,
    key: K,
    control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
) -> Result<u64, E> {
    let mut node = index.0.as_deref();
    while let Some(current) = node {
        control(SourceEvent::Work)?;
        match key.cmp(&current.key) {
            std::cmp::Ordering::Less => node = current.left.0.as_deref(),
            std::cmp::Ordering::Greater => node = current.right.0.as_deref(),
            std::cmp::Ordering::Equal => return Ok(current.value),
        }
    }
    Ok(0)
}

fn increment<K: Ord + Clone>(counts: &mut IndexMap<K, u64>, key: K, work: &mut u64) -> bool {
    let old = counts.get(&key).copied().unwrap_or(0);
    old.checked_add(1).is_some_and(|next| {
        if old == 0 && counts.len() == MAX_COUNTERS {
            return false;
        }
        *counts = counts.insert(key, next, work);
        true
    })
}

fn decrement<K: Ord + Clone>(counts: &mut IndexMap<K, u64>, key: K, work: &mut u64) -> bool {
    let Some(next) = counts.get(&key).and_then(|value| value.checked_sub(1)) else {
        return false;
    };
    *counts = if next == 0 {
        counts.remove(&key, work)
    } else {
        counts.insert(key, next, work)
    };
    true
}

pub(super) type RelationBuilder = CountBuilder<RelationId>;
pub(super) type LabelBuilder = CountBuilder<LabelId>;

/// Cold scans collect at most MAX_COUNTERS sorted counters, then build one
/// persistent tree. Charge replacement capacity before allocation and every
/// comparison/move; geometric growth avoids quadratic reallocation.
pub(super) struct CountBuilder<K> {
    rows: Vec<(K, u64)>,
    unavailable: bool,
}

impl<K> Default for CountBuilder<K> {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            unavailable: false,
        }
    }
}

impl<K: Ord + Clone> CountBuilder<K> {
    pub(super) fn observe<E>(
        &mut self,
        key: K,
        control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        self.observe_many(key, 1, control)
    }

    fn observe_many<E>(
        &mut self,
        key: K,
        count: u64,
        control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        if self.unavailable {
            return Ok(());
        }
        if count == 0 {
            return Ok(());
        }
        let (mut low, mut high) = (0, self.rows.len());
        while low < high {
            let mid = low + (high - low) / 2;
            control(SourceEvent::Work)?;
            match self.rows[mid].0.cmp(&key) {
                std::cmp::Ordering::Less => low = mid + 1,
                std::cmp::Ordering::Greater => high = mid,
                std::cmp::Ordering::Equal => {
                    if let Some(next) = self.rows[mid].1.checked_add(count) {
                        self.rows[mid].1 = next;
                    } else {
                        self.unavailable = true;
                    }
                    return Ok(());
                }
            }
        }
        if self.rows.len() == MAX_COUNTERS {
            self.unavailable = true;
            return Ok(());
        }
        if self.rows.len() == self.rows.capacity() {
            let capacity = (self.rows.capacity().max(4) * 2).min(MAX_COUNTERS);
            // Charge the entire replacement, including overlap with the old
            // allocation during reserve, rather than just the length delta.
            for _ in 0..capacity {
                control(SourceEvent::ScratchEntry)?;
            }
            for _ in 0..self.rows.len() {
                control(SourceEvent::Work)?;
            }
            self.rows.reserve_exact(capacity - self.rows.len());
        }
        for _ in low..self.rows.len() {
            control(SourceEvent::Work)?;
        }
        control(SourceEvent::Work)?;
        self.rows.insert(low, (key, count));
        Ok(())
    }

    pub(super) fn finish<E>(
        self,
        control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
    ) -> Result<Option<IndexMap<K, u64>>, E> {
        if self.unavailable {
            return Ok(None);
        }
        for _ in 0..self.rows.len() {
            control(SourceEvent::ScratchEntry)?;
            control(SourceEvent::Work)?;
        }
        Ok(Some(IndexMap::from_sorted(self.rows, &mut 0)))
    }
}

#[cfg(test)]
mod tests;
