//! A host's admitted graph with each edge's endpoints given by position
//! (fgdb-61p1n).
//!
//! The vertex and relation tables are strictly ascending by identity, so a
//! position orders, equates and distinguishes exactly as the identity it
//! stands for: any step that only compares, equates or sorts identities takes
//! the same branches on positions. An evaluator can then keep per-vertex state in arrays indexed
//! by position instead of identity-keyed hash maps (most of a 5.5M-edge count
//! was its two verdict probes per edge into a 2M-entry map).

use fgdb_delta_types::RelationId;
use fgdb_types::VId;

/// An edge by its endpoints' positions in its graph's vertex table and its
/// relation's position in the graph's relation table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PositionedEdge {
    pub source: u32,
    pub destination: u32,
    pub relation: u32,
}

/// Admitted vertices, strictly ascending by identity beside a host value the
/// evaluator never reads, relations strictly ascending, and admitted edges in
/// arrival order, each naming its endpoints and relation by position.
#[derive(Clone, Copy, Debug)]
pub struct PositionedGraph<'g, T> {
    vertices: &'g [(VId, T)],
    relations: &'g [RelationId],
    edges: &'g [PositionedEdge],
}

impl<'g, T> PositionedGraph<'g, T> {
    /// `None` unless both tables are strictly ascending by identity and every
    /// edge's positions are inside them (one pass over each), so no position
    /// the evaluator follows can name another element or none.
    #[must_use]
    pub fn new(
        vertices: &'g [(VId, T)],
        relations: &'g [RelationId],
        edges: &'g [PositionedEdge],
    ) -> Option<Self> {
        let len = u32::try_from(vertices.len()).ok()?;
        let relation_len = u32::try_from(relations.len()).ok()?;
        let ascending = vertices.windows(2).all(|pair| pair[0].0 < pair[1].0)
            && relations.windows(2).all(|pair| pair[0] < pair[1]);
        let inside = edges.iter().all(|edge| {
            edge.source < len && edge.destination < len && edge.relation < relation_len
        });
        (ascending && inside).then_some(Self {
            vertices,
            relations,
            edges,
        })
    }

    pub(crate) fn len(&self) -> usize {
        self.vertices.len()
    }

    /// The identity at `at`, a position validated with the graph.
    pub(crate) fn vertex(&self, at: u32) -> VId {
        self.vertices[at as usize].0
    }

    pub(crate) fn vertices(&self) -> impl Iterator<Item = VId> + 'g {
        self.vertices.iter().map(|&(vid, _)| vid)
    }

    pub(crate) fn edges(&self) -> &'g [PositionedEdge] {
        self.edges
    }

    pub(crate) fn relations(&self) -> &'g [RelationId] {
        self.relations
    }

    /// The position of `relation` in the relation table, if it is there.
    pub(crate) fn relation(&self, relation: RelationId) -> Option<u32> {
        self.relations
            .iter()
            .position(|&known| known == relation)
            .map(|at| at as u32)
    }

    /// Every edge as its identity triple, in arrival order.
    pub(crate) fn triples(&self) -> impl Iterator<Item = (VId, RelationId, VId)> + 'g {
        let (vertices, relations) = (self.vertices, self.relations);
        self.edges.iter().map(move |edge| {
            (
                vertices[edge.source as usize].0,
                relations[edge.relation as usize],
                vertices[edge.destination as usize].0,
            )
        })
    }
}
