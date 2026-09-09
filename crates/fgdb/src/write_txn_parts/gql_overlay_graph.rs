mod query_source {
    use super::{BoundPlan, Database, OverlayEdgeMap, PendingRow, WriteTxn, WriteTxnError};
    use crate::Snapshot;
    use crate::gql_exec::source::{self, SourceEvent};
    use asupersync::fs::Vfs;
    use fgdb_delta_types::{DeltaRow, ElementId, LabelId, PropertyKeyId, RelationId};
    use fgdb_gql::algebra::{GlaIdentityOutput, GlaOperator, GlaOutput, GlaPlan, PreparedGraphPattern, VertexPredicate};
    use fgdb_types::{CanonicalScalar, VId};
    use std::collections::{BTreeMap, BTreeSet};

    type EdgeTriple = (VId, RelationId, VId);
    struct VertexView<'a> {
        labels: &'a [LabelId],
        props: &'a [(PropertyKeyId, CanonicalScalar)],
        label_edits: BTreeMap<LabelId, bool>,
        property_edits: BTreeMap<PropertyKeyId, Option<&'a CanonicalScalar>>,
    }
    impl<'a> VertexView<'a> {
        fn new(labels: &'a [LabelId], props: &'a [(PropertyKeyId, CanonicalScalar)]) -> Self {
            Self { labels, props, label_edits: BTreeMap::new(), property_edits: BTreeMap::new() }
        }
        fn property(&self, key: PropertyKeyId) -> Option<&CanonicalScalar> {
            if let Some(value) = self.property_edits.get(&key) { *value } else {
                self.props.binary_search_by_key(&key, |(key, _)| *key).ok().map(|at| &self.props[at].1)
            }
        }
        fn matches(&self, predicate: &VertexPredicate) -> bool {
            let (label, property) = match predicate {
                VertexPredicate::HasLabel(label) => (self.label_edits.get(label).copied()
                    .unwrap_or_else(|| self.labels.binary_search(label).is_ok()).then_some(*label), None),
                VertexPredicate::IntegerProperty { key, .. } => (None, self.property(*key).map(|value| (*key, value))),
            };
            predicate.matches_borrowed(label, property)
        }
    }

    /// One basis/template pair, with output shape carried by the checked plan.
    /// The borrowed source is the same for scalar and tuple query projections.
    pub(super) struct OverlayQuerySource<'a, Row = VId> {
        pub(super) logical: GlaPlan<Row>,
        vertices: Vec<(VId, VertexView<'a>)>,
        edges: Vec<EdgeTriple>,
        pub(super) snapshot_records: usize,
    }
    impl<Row: GlaOutput> OverlayQuerySource<'_, Row> {
        pub(super) fn vertex_ids(&self) -> impl Iterator<Item = VId> + '_ { self.vertices.iter().map(|(vid, _)| *vid) }
        pub(super) fn edge_triples(&self) -> impl Iterator<Item = EdgeTriple> + '_ { self.edges.iter().copied() }
        pub(super) fn matches(&self, vid: VId, predicates: &[VertexPredicate]) -> bool {
            self.vertices.binary_search_by_key(&vid, |(vid, _)| *vid).ok()
                .is_some_and(|at| predicates.iter().all(|predicate| self.vertices[at].1.matches(predicate)))
        }
        fn property(&self, vid: VId, key: PropertyKeyId) -> Option<&CanonicalScalar> {
            let at = self.vertices.binary_search_by_key(&vid, |(vid, _)| *vid).ok()?;
            self.vertices[at].1.property(key)
        }
        pub(super) fn execute(self) -> Result<Vec<Row>, WriteTxnError>
        where Row: GlaIdentityOutput {
            self.logical.execute(self.vertex_ids(), self.edge_triples(), |vid, predicates| Ok(self.matches(vid, predicates)))
        }
    }
    fn is_vertex_effect(row: &DeltaRow) -> bool {
        matches!(row, DeltaRow::CreateVertex { .. } | DeltaRow::DeleteVertex { .. }
            | DeltaRow::LabelMembership { .. } | DeltaRow::Property { elem: ElementId::Vertex(_), .. })
    }
    fn apply_vertex<'a, E>(rows: &mut BTreeMap<VId, VertexView<'a>>, effect: &'a DeltaRow,
        candidates: Option<&BTreeSet<VId>>, control: &mut impl FnMut(SourceEvent) -> Result<(), E>) -> Result<(), E> {
        match effect {
            DeltaRow::CreateVertex { vid, labels, props, .. } if candidates.is_none_or(|candidates| candidates.contains(vid)) => {
                if !rows.contains_key(vid) { control(SourceEvent::ScratchEntry)?; }
                rows.insert(*vid, VertexView::new(labels, props));
            }
            DeltaRow::DeleteVertex { vid, .. } => { rows.remove(vid); }
            DeltaRow::LabelMembership { vid, label, after, .. } => {
                if let Some(row) = rows.get_mut(vid) {
                    if !row.label_edits.contains_key(label) { control(SourceEvent::ScratchEntry)?; }
                    row.label_edits.insert(*label, *after);
                }
            }
            DeltaRow::Property { elem: ElementId::Vertex(vid), property, after, .. } => {
                if let Some(row) = rows.get_mut(vid) {
                    if !row.property_edits.contains_key(property) { control(SourceEvent::ScratchEntry)?; }
                    row.property_edits.insert(*property, after.as_ref());
                }
            }
            _ => {}
        }
        Ok(())
    }

    impl WriteTxn {
        pub(super) fn query_snapshot<'a, V: Vfs + Clone>(&self, database: &'a Database<V>) -> Result<&'a Snapshot, WriteTxnError> {
            self.ensure_database(database)?; database.ensure_readable()?;
            database.snapshot.check_frontier(self.basis)?; Ok(&database.snapshot)
        }
        pub(super) fn query_source<'a, V: Vfs + Clone>(&'a self, database: &'a Database<V>, plan: &BoundPlan)
            -> Result<OverlayQuerySource<'a>, WriteTxnError> {
            let snapshot = self.query_snapshot(database)?; self.query_source_over(snapshot, plan, &mut |_| Ok(()))
        }

        /// Run one scalar or correlated binding projection over the original
        /// basis plus canonical staged effects. All shapes retain the same
        /// source, witnesses and one shared source/evaluator allowance.
        pub fn execute_graph_pattern_governed<V: Vfs + Clone, Row: GlaOutput>(
            &self, database: &Database<V>, cx: &fgdb_types::QueryCx,
            pattern: &PreparedGraphPattern<Row>, policy: fgdb_gql::GqlQueryPolicy,
        ) -> Result<fgdb_gql::GqlQueryExecution<Row>, fgdb_gql::GqlQueryError<WriteTxnError, Box<asupersync::error::Error>>> {
            cx.with_restriction(|| {
                let snapshot = self.query_snapshot(database).map_err(fgdb_gql::GqlQueryError::Source)?;
                cx.checkpoint().map_err(fgdb_gql::GqlQueryError::Interrupted)?;
                let mut usage = crate::gql_exec::AdmissionUsage::default();
                let source = self.query_source_over_logical(snapshot, pattern.plan().clone(), pattern.required_vertex_label(), &mut |event| {
                    cx.checkpoint().map_err(fgdb_gql::GqlQueryError::Interrupted)?; usage.observe(policy, event)
                })?;
                let result = source.logical.execute_governed_with_properties(source.snapshot_records as u64, source.vertex_ids(), source.edge_triples(),
                    |vid, predicates| Ok::<_, WriteTxnError>(source.matches(vid, predicates)),
                    |vid, key| Ok(source.property(vid, key)), usage.remaining(policy), || cx.checkpoint());
                usage.finish(policy, result)
            })
        }

        fn note_query_read<E>(&self, observed: &mut BTreeSet<ElementId>, element: ElementId,
            control: &mut impl FnMut(SourceEvent) -> Result<(), E>) -> Result<(), E> {
            control(SourceEvent::Work)?;
            if !observed.contains(&element) {
                control(SourceEvent::ScratchEntry)?; control(SourceEvent::ScratchEntry)?;
                observed.insert(element); self.read_set.borrow_mut().insert(element);
            }
            Ok(())
        }
        pub(super) fn query_source_over<'a, E>(&'a self, snapshot: &'a Snapshot, plan: &BoundPlan,
            control: &mut impl FnMut(SourceEvent) -> Result<(), E>) -> Result<OverlayQuerySource<'a>, E> {
            self.query_source_over_logical(snapshot, GlaPlan::lower(plan), plan.src_label, control)
        }

        /// Row limits count the final canonical overlay, not removed basis rows.
        /// Once an observation is retained, later refusal does not discard it.
        fn query_source_over_logical<'a, E, Row: GlaOutput>(
            &'a self, snapshot: &'a Snapshot, logical: GlaPlan<Row>, required_vertex_label: Option<LabelId>,
            control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
        ) -> Result<OverlayQuerySource<'a, Row>, E> {
            let edge_scan = logical.scans_edges(); control(SourceEvent::Work)?;
            if edge_scan { self.scanned_edges.set(true); }
            else if let Some(label) = required_vertex_label {
                control(SourceEvent::ScratchEntry)?; self.scanned_vertex_labels.borrow_mut().insert(label);
            } else { self.scanned_vertices.set(true); }
            let mut observed = BTreeSet::new();
            // Raw intentions contribute only negative-read identities. The
            // canonical template, not these intentions, creates query rows.
            for batch in &self.staged {
                control(SourceEvent::Work)?;
                for pending in &batch.rows {
                    control(SourceEvent::Work)?;
                    match pending {
                        PendingRow::Vertex { vid, .. } | PendingRow::DeleteVertex { vid, .. } => {
                            self.note_query_read(&mut observed, ElementId::Vertex(*vid), control)?;
                        }
                        PendingRow::Edge { eid, src, dst, .. } if edge_scan => {
                            for element in [ElementId::Edge(*eid), ElementId::Vertex(*src), ElementId::Vertex(*dst)] {
                                self.note_query_read(&mut observed, element, control)?;
                            }
                        }
                        PendingRow::DeleteEdge { eid, .. } if edge_scan => {
                            self.note_query_read(&mut observed, ElementId::Edge(*eid), control)?;
                        }
                        _ => {}
                    }
                }
            }
            let mut vertices = BTreeMap::new(); let mut edges = OverlayEdgeMap::new();
            if edge_scan {
                source::visit_edges(&snapshot.blocks, self.basis, control, |entry, control| {
                    for element in [ElementId::Edge(entry.eid), ElementId::Vertex(entry.src), ElementId::Vertex(entry.dst)] {
                        self.note_query_read(&mut observed, element, control)?;
                    }
                    control(SourceEvent::ScratchEntry)?;
                    edges.insert(entry.eid, (entry.src, entry.relation, entry.dst)); Ok(())
                })?;
            } else {
                source::visit_vertices(&snapshot.patches, self.basis, control, |row, control| {
                    self.note_query_read(&mut observed, ElementId::Vertex(row.vid), control)?;
                    control(SourceEvent::ScratchEntry)?; vertices.insert(row.vid, VertexView::new(&row.labels, &row.props)); Ok(())
                })?;
            }
            let mut vertex_effects = Vec::new();
            if let Some(prepared) = &self.prepared {
                for coordinate in prepared.template.coordinate_entries() {
                    control(SourceEvent::Work)?;
                    for effect in &coordinate.rows {
                        control(SourceEvent::Work)?;
                        if !edge_scan { apply_vertex(&mut vertices, effect, None, control)?; continue; }
                        match effect {
                            DeltaRow::CreateEdge { eid, src, relation, dst, .. } => {
                                if !edges.contains_key(eid) { control(SourceEvent::ScratchEntry)?; }
                                edges.insert(*eid, (*src, *relation, *dst));
                            }
                            DeltaRow::DeleteEdge { eid, .. } => { edges.remove(eid); }
                            DeltaRow::DeleteVertex { sorted_retired_incident_edges, .. } => {
                                for eid in sorted_retired_incident_edges {
                                    control(SourceEvent::Work)?; self.note_query_read(&mut observed, ElementId::Edge(*eid), control)?;
                                    edges.remove(eid);
                                }
                            }
                            _ => {}
                        }
                        if is_vertex_effect(effect) { control(SourceEvent::ScratchEntry)?; vertex_effects.push(effect); }
                    }
                }
            }
            if edge_scan && logical.needs_vertex_values() {
                let mut candidates = BTreeSet::new();
                for &(src, relation, dst) in edges.values() {
                    control(SourceEvent::Work)?;
                    let requested = logical.operators().iter().any(|op| match op {
                        GlaOperator::ScanEdges { relation: required, .. } | GlaOperator::Expand { relation: required, .. } => *required == relation,
                        _ => false,
                    });
                    if requested {
                        for vid in [src, dst] {
                            if !candidates.contains(&vid) { control(SourceEvent::ScratchEntry)?; candidates.insert(vid); }
                        }
                    }
                }
                for &vid in &candidates {
                    self.note_query_read(&mut observed, ElementId::Vertex(vid), control)?;
                    if let Some(row) = source::find_vertex(&snapshot.patches, vid, self.basis, control)? {
                        control(SourceEvent::ScratchEntry)?; vertices.insert(vid, VertexView::new(&row.labels, &row.props));
                    }
                }
                for effect in vertex_effects { control(SourceEvent::Work)?; apply_vertex(&mut vertices, effect, Some(&candidates), control)?; }
            }
            let mut vertex_rows = Vec::new();
            for (vid, row) in vertices {
                control(SourceEvent::Work)?;
                if !edge_scan { control(SourceEvent::SnapshotRecord)?; }
                control(SourceEvent::ScratchEntry)?; vertex_rows.push((vid, row));
            }
            let mut edge_rows = Vec::new();
            for triple in edges.into_values() {
                control(SourceEvent::Work)?; control(SourceEvent::SnapshotRecord)?; control(SourceEvent::ScratchEntry)?; edge_rows.push(triple);
            }
            let snapshot_records = if edge_scan { edge_rows.len() } else { vertex_rows.len() };
            Ok(OverlayQuerySource { logical, vertices: vertex_rows, edges: edge_rows, snapshot_records })
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use fgdb_gql::algebra::IntegerComparison;
        #[test]
        fn borrowed_property_overrides_shadow_without_cloning_or_coercion() {
            let key = PropertyKeyId(1); let properties = [(key, CanonicalScalar::Int(7))]; let changed = CanonicalScalar::Int(9);
            let mut row = VertexView::new(&[LabelId(2)], &properties);
            assert!(std::ptr::eq(row.property(key).unwrap(), &properties[0].1));
            row.property_edits.insert(key, Some(&changed)); assert!(std::ptr::eq(row.property(key).unwrap(), &changed));
            assert!(row.matches(&VertexPredicate::IntegerProperty { key, comparison: IntegerComparison::Equal, value: 9 }));
            row.property_edits.insert(key, None); assert!(row.property(key).is_none());
            for comparison in [IntegerComparison::Equal, IntegerComparison::NotEqual, IntegerComparison::Greater,
                IntegerComparison::Less, IntegerComparison::GreaterOrEqual, IntegerComparison::LessOrEqual] {
                assert!(!row.matches(&VertexPredicate::IntegerProperty { key, comparison, value: 9 }));
            }
        }
        #[test]
        fn label_edits_override_membership_without_mutating_the_borrowed_row() {
            let labels = [LabelId(2), LabelId(4)]; let mut row = VertexView::new(&labels, &[]);
            row.label_edits.insert(LabelId(2), false); row.label_edits.insert(LabelId(3), true);
            assert!(!row.matches(&VertexPredicate::HasLabel(LabelId(2))));
            assert!(row.matches(&VertexPredicate::HasLabel(LabelId(3)))); assert!(row.matches(&VertexPredicate::HasLabel(LabelId(4))));
            assert_eq!(labels, [LabelId(2), LabelId(4)]);
        }
    }
}
