//! Endpoint-only ANY shortest WALK through the shared snapshot admission path.

use super::*;

impl<V: Vfs + Clone> Database<V> {
    /// Return one occurrence of each endpoint admitting a shortest WALK within
    /// the bounds. Results are sorted by vertex identity. The native ANY
    /// cursor coalesces same-depth prefixes before expansion; it never computes
    /// an ALL result bag merely to apply DISTINCT afterward.
    ///
    /// Minimum hops delay settlement, not reachability: a vertex visited before
    /// that minimum may still be reached by a later admissible WALK. Zero hops
    /// retain an existing source, and a missing source returns no rows. This
    /// endpoint-only API exposes neither the chosen path nor its hop length.
    /// It is not weighted shortest path, TRAIL, SIMPLE or captured-path search.
    ///
    /// ALL and ANY share exact-cut indexed source admission, one cumulative
    /// work/scratch policy and the same result-row limit. Source records count
    /// physical matching edges, including parallel edges; ANY's result rows
    /// count endpoints, not tied routes. No quota reset, implicit mode switch
    /// or partial result follows resource exhaustion or cancellation. Retained
    /// source closure/index residency is not allocator-byte governance or spill.
    pub fn execute_any_shortest_walk_governed(
        &self,
        cx: &QueryCx,
        source: VId,
        relation: RelationId,
        direction: GlaDirection,
        bounds: GraphWalkBounds,
        policy: GqlQueryPolicy,
    ) -> ShortestResult {
        let as_of = self
            .frontier()
            .map_err(GqlError::Read)
            .map_err(GqlQueryError::Source)?;
        self.execute_any_shortest_walk_governed_at(
            cx, source, relation, direction, bounds, as_of, policy,
        )
    }

    /// ANY shortest WALK at an explicit admitted historical cut. Health and
    /// cut validation precede traversal exactly as for the ALL sibling.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_any_shortest_walk_governed_at(
        &self,
        cx: &QueryCx,
        source: VId,
        relation: RelationId,
        direction: GlaDirection,
        bounds: GraphWalkBounds,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> ShortestResult {
        self.ensure_readable()
            .and_then(|()| self.snapshot.check_frontier(as_of))
            .map_err(GqlError::Read)
            .map_err(GqlQueryError::Source)?;
        cx.with_restriction(|| {
            execute_shortest_at(
                &self.snapshot,
                source,
                relation,
                direction,
                bounds,
                as_of,
                ShortestMultiplicity::Any,
                policy,
                || cx.checkpoint(),
            )
        })
    }
}

impl EmbeddedReadView {
    /// The ANY endpoint selection over this already pinned generation. A later
    /// commit to the originating database cannot change this view's result.
    pub fn execute_any_shortest_walk_governed(
        &self,
        cx: &QueryCx,
        source: VId,
        relation: RelationId,
        direction: GlaDirection,
        bounds: GraphWalkBounds,
        policy: GqlQueryPolicy,
    ) -> ShortestResult {
        self.execute_any_shortest_walk_governed_at(
            cx,
            source,
            relation,
            direction,
            bounds,
            self.frontier(),
            policy,
        )
    }

    /// Read an earlier cut within this pinned view, without consulting a newer
    /// database handle or rebuilding a second source representation.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_any_shortest_walk_governed_at(
        &self,
        cx: &QueryCx,
        source: VId,
        relation: RelationId,
        direction: GlaDirection,
        bounds: GraphWalkBounds,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> ShortestResult {
        self.snapshot
            .check_frontier(as_of)
            .map_err(GqlError::Read)
            .map_err(GqlQueryError::Source)?;
        cx.with_restriction(|| {
            execute_shortest_at(
                &self.snapshot,
                source,
                relation,
                direction,
                bounds,
                as_of,
                ShortestMultiplicity::Any,
                policy,
                || cx.checkpoint(),
            )
        })
    }
}
