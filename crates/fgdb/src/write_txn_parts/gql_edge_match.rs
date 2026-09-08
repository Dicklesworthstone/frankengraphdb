impl WriteTxn {
    fn execute_prepared_edge_match<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        plan: &BoundPlan,
        edge_relation: RelationId,
    ) -> Result<Vec<VId>, WriteTxnError> {
        let graph = self.overlay_graph(database)?;
        let logical = fgdb_gql::algebra::GlaPlan::lower(plan);
        // Admission and ordered staged-effect folding remain transaction-owned.
        // Expansion, predicates and result semantics have one typed GLA owner;
        // no expansion scans graph.edges, and a complete predicate conjunction
        // reads one overlay vertex version rather than once per comparator.
        let edges = graph
            .edges
            .values()
            .filter(|edge| {
                graph.vertices.contains(&edge.0) && graph.vertices.contains(&edge.2)
            })
            .copied();
        let destinations = logical.execute([], edges, |vid, predicates| {
            Ok::<_, WriteTxnError>(self.vertex(database, vid)?.is_some_and(|row| {
                predicates
                    .iter()
                    .all(|predicate| predicate.matches(&row.labels, &row.props))
            }))
        })?;
        // Keep the conservative admitted-graph dependencies, including vertices
        // that predicates rejected. LIMIT must never narrow the read footprint.
        let mut read_set = self.read_set.borrow_mut();
        read_set.extend(graph.observed);
        read_set.extend(destinations.iter().copied().map(ElementId::Vertex));
        drop(read_set);
        self.match_expansions
            .borrow_mut()
            .extend(graph.vertices.iter().copied().map(|src| (src, edge_relation)));
        Ok(destinations)
    }
}
