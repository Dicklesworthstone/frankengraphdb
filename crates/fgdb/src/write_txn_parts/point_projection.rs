// A projection of the admitted historical winner and the prepared NET effects.
// This does not interpret PendingRow, expose metadata, or own a second overlay.

mod point_effect_selector {
    include!("point_effect_selector.rs");
}

impl WriteTxn {
    /// Run a point read against an earlier immutable prepared prefix of this
    /// transaction. Point projection and scoped point-visibility readers use
    /// only `basis` plus `prepared`; they do not inspect `staged` or a cached
    /// materialized overlay. The caller captures the prefix before its first
    /// effect, so computed values share the input row's frozen preimage.
    ///
    /// The callback cannot stage an effect or retain a borrowed projection.
    /// Observation sets stay live on the transaction. The current write prefix
    /// is restored on success, refusal and unwinding, without copying graph
    /// state, replacing a transaction or granting a new allowance.
    fn with_prepared_point_read<T>(
        &mut self,
        prepared: &Option<std::sync::Arc<PreparedWrite>>,
        read: impl FnOnce(&WriteTxn) -> T,
    ) -> T {
        struct Restore<'a> {
            txn: &'a mut WriteTxn,
            prepared: Option<std::sync::Arc<PreparedWrite>>,
        }
        impl Drop for Restore<'_> {
            fn drop(&mut self) {
                self.txn.prepared = self.prepared.take();
            }
        }
        let current = core::mem::replace(&mut self.prepared, prepared.clone());
        let restored = Restore {
            txn: self,
            prepared: current,
        };
        read(restored.txn)
    }
}

#[derive(Default)]
struct PointProjection<'a> {
    exists: bool,
    property: Option<&'a CanonicalPropertyValue>,
    label: bool,
}

/// A whole-property sibling of the scalar point projection. It borrows the
/// selected historical row and canonical staged values; it never clones a
/// vertex, inspects a hidden payload, or enumerates the host's known keys.
struct PointProperties<'a> {
    base: &'a [(fgdb_delta_types::PropertyKeyId, CanonicalPropertyValue)],
    edits: std::collections::BTreeMap<
        fgdb_delta_types::PropertyKeyId,
        Option<&'a CanonicalPropertyValue>,
    >,
}

struct PointEdgeMetadata<'a> {
    relation: RelationId,
    properties: PointProperties<'a>,
}

impl<'a> PointProperties<'a> {
    fn new(base: &'a [(fgdb_delta_types::PropertyKeyId, CanonicalPropertyValue)]) -> Self {
        Self {
            base,
            edits: std::collections::BTreeMap::new(),
        }
    }

    fn effective_properties(
        &self,
    ) -> impl Iterator<Item = (fgdb_delta_types::PropertyKeyId, &CanonicalPropertyValue)> {
        self.base
            .iter()
            .filter(|(key, _)| !self.edits.contains_key(key))
            .map(|(key, value)| (*key, value))
            .chain(
                self.edits
                    .iter()
                    .filter_map(|(key, value)| value.map(|value| (*key, value))),
            )
    }
}

/// The whole-label sibling of `PointProperties` (fgdb-iufb7): the selected
/// historical row's labels and the prepared NET membership edits, borrowed.
struct PointLabels<'a> {
    base: &'a [LabelId],
    edits: std::collections::BTreeMap<LabelId, bool>,
}

impl<'a> PointLabels<'a> {
    fn new(base: &'a [LabelId]) -> Self {
        Self {
            base,
            edits: std::collections::BTreeMap::new(),
        }
    }

    /// Effective labels in identity order: every unedited historical label
    /// and every label a staged edit sets, each once.
    fn effective_labels(&self) -> Vec<LabelId> {
        self.effective_labels_iter().collect()
    }

    /// Merge the canonical historical labels and admitted membership edits
    /// without allocating another identity vector before output admission.
    fn effective_labels_iter(&self) -> impl Iterator<Item = LabelId> + '_ {
        let mut base = self.base.iter().copied().peekable();
        let mut edits = self.edits.iter().peekable();
        std::iter::from_fn(move || {
            loop {
                match (base.peek().copied(), edits.peek().copied()) {
                    (Some(label), Some((&edited, &member))) if edited <= label => {
                        edits.next();
                        if edited == label {
                            base.next();
                        }
                        if member {
                            return Some(edited);
                        }
                    }
                    (Some(_), _) => return base.next(),
                    (None, Some((&edited, &member))) => {
                        edits.next();
                        if member {
                            return Some(edited);
                        }
                    }
                    (None, None) => return None,
                }
            }
        })
    }
}

// A controlled binary search over canonical sorted fields. No property value
// is inspected or copied to locate its key; each comparison is cancellable.
fn point_find<'a, T, K: Ord + Copy, E>(
    values: &'a [T],
    requested: K,
    key: impl Fn(&T) -> K,
    control: &mut impl FnMut(crate::gql_exec::source::SourceEvent) -> Result<(), E>,
) -> Result<Option<&'a T>, E> {
    use crate::gql_exec::source::SourceEvent::Work;
    let (mut low, mut high) = (0, values.len());
    while low < high {
        control(Work)?;
        let middle = low + (high - low) / 2;
        match key(&values[middle]).cmp(&requested) {
            core::cmp::Ordering::Less => low = middle + 1,
            core::cmp::Ordering::Greater => high = middle,
            core::cmp::Ordering::Equal => return Ok(Some(&values[middle])),
        }
    }
    Ok(None)
}

impl<'a> PointProjection<'a> {
    fn apply_selected<E>(
        &mut self,
        rows: &'a [fgdb_delta_types::DeltaRow],
        key: point_effect_selector::EffectKey,
        element: ElementId,
        field: PointReadField,
        control: &mut impl FnMut(crate::gql_exec::source::SourceEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        use crate::gql_exec::source::SourceEvent::Work;
        for effect in point_effect_selector::select(rows, key, &mut || control(Work))? {
            control(Work)?;
            self.apply(element, field, effect, control)?;
        }
        Ok(())
    }

    fn from_fields<E>(
        field: PointReadField,
        labels: &[LabelId],
        properties: &'a [(fgdb_delta_types::PropertyKeyId, CanonicalPropertyValue)],
        control: &mut impl FnMut(crate::gql_exec::source::SourceEvent) -> Result<(), E>,
    ) -> Result<Self, E> {
        let mut projected = Self {
            exists: true,
            ..Self::default()
        };
        match field {
            // Authorization of a supplied element needs its lifetime without
            // reading any property or label payload. Creation already set
            // `exists`; canonical retirement resets the projection below.
            PointReadField::VertexExistence => {}
            PointReadField::Property(requested) => {
                projected.property = point_find(properties, requested, |(key, _)| *key, control)?
                    .map(|(_, value)| value);
            }
            PointReadField::Label(requested) => {
                projected.label = point_find(labels, requested, |label| *label, control)?.is_some();
            }
            _ => unreachable!("only existence, property and label accessors project a point"),
        }
        Ok(projected)
    }

    fn apply<E>(
        &mut self,
        element: ElementId,
        field: PointReadField,
        effect: &'a fgdb_delta_types::DeltaRow,
        control: &mut impl FnMut(crate::gql_exec::source::SourceEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        use fgdb_delta_types::DeltaRow;
        match effect {
            DeltaRow::CreateVertex {
                vid, labels, props, ..
            } if element == ElementId::Vertex(*vid) => {
                *self = Self::from_fields(field, labels, props, control)?;
            }
            DeltaRow::CreateEdge { eid, props, .. } if element == ElementId::Edge(*eid) => {
                *self = Self::from_fields(field, &[], props, control)?;
            }
            DeltaRow::DeleteVertex {
                vid,
                sorted_retired_incident_edges,
                ..
            } => {
                let deleted = match element {
                    ElementId::Vertex(target) => target == *vid,
                    ElementId::Edge(target) => {
                        point_find(sorted_retired_incident_edges, target, |eid| *eid, control)?
                            .is_some()
                    }
                };
                if deleted {
                    *self = Self::default();
                }
            }
            DeltaRow::DeleteEdge { eid, .. } if element == ElementId::Edge(*eid) => {
                *self = Self::default();
            }
            DeltaRow::Property {
                elem,
                property,
                after,
                ..
            } if self.exists
                && *elem == element
                && field == PointReadField::Property(*property) =>
            {
                self.property = after.as_ref();
            }
            DeltaRow::LabelMembership {
                vid, label, after, ..
            } if self.exists
                && element == ElementId::Vertex(*vid)
                && field == PointReadField::Label(*label) =>
            {
                self.label = *after;
            }
            _ => {}
        }
        Ok(())
    }
}

impl WriteTxn {
    /// Borrow the selected vertex's effective fields from the same admitted
    /// basis and NET effects as individual point projections. Visibility must
    /// also be supplied to the final property-map collector: the historical
    /// slice is borrowed intact, and only visible edits are retained here.
    fn vertex_properties_with_control<'a, V: Vfs + Clone, E>(
        &'a self,
        database: &'a Database<V>,
        vertex: VId,
        visible: &mut impl FnMut(fgdb_delta_types::PropertyKeyId) -> bool,
        poll: &mut impl FnMut() -> Result<(), E>,
        control: &mut impl FnMut(crate::gql_exec::source::SourceEvent) -> Result<(), E>,
        source_error: &impl Fn(WriteTxnError) -> E,
    ) -> Result<Option<PointProperties<'a>>, E> {
        use crate::gql_exec::source::{SourceEvent, find_vertex};
        use fgdb_delta_types::{DeltaRow, PropertyKeyId};
        use point_effect_selector::{EffectKey, select_range};
        let snapshot = self.query_snapshot(database).map_err(source_error)?;
        control(SourceEvent::Work)?;
        let mut properties = match find_vertex(snapshot, vertex, self.basis, control)? {
            Some(row) => {
                control(SourceEvent::SnapshotRecord)?;
                Some(PointProperties::new(&row.props))
            }
            None => None,
        };
        if let Some(prepared) = &self.prepared {
            for coordinate in prepared.template.coordinate_entries() {
                poll()?;
                // Preserve canonical tag order while borrowing only this
                // identity's lifetime and complete property-key intervals.
                // Searches inspect no payload; hidden fields only poll until
                // visibility admits a retained edit and its original charges.
                for (first, last) in [
                    (
                        EffectKey::CreateVertex(vertex),
                        EffectKey::CreateVertex(vertex),
                    ),
                    (
                        EffectKey::DeleteVertex(vertex),
                        EffectKey::DeleteVertex(vertex),
                    ),
                    (
                        EffectKey::Property(ElementId::Vertex(vertex), PropertyKeyId(0)),
                        EffectKey::Property(ElementId::Vertex(vertex), PropertyKeyId(u64::MAX)),
                    ),
                ] {
                    for effect in select_range(&coordinate.rows, first, last, poll)? {
                        poll()?;
                        match effect {
                            DeltaRow::CreateVertex { vid, props, .. } if *vid == vertex => {
                                control(SourceEvent::Work)?;
                                properties = Some(PointProperties::new(props));
                            }
                            DeltaRow::DeleteVertex { vid, .. } if *vid == vertex => {
                                control(SourceEvent::Work)?;
                                properties = None;
                            }
                            DeltaRow::Property {
                                elem: ElementId::Vertex(vid),
                                property,
                                after,
                                ..
                            } if *vid == vertex && visible(*property) => {
                                control(SourceEvent::Work)?;
                                if let Some(properties) = &mut properties {
                                    control(SourceEvent::ScratchEntry)?;
                                    properties.edits.insert(*property, after.as_ref());
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        Ok(properties)
    }

    /// Borrow the selected vertex's effective labels from the same admitted
    /// basis and NET effects as `vertex_properties_with_control` (fgdb-iufb7).
    /// A label `visible` rejects adds no charge and no retained edit; the
    /// caller masks the historical labels the same way before naming them.
    fn vertex_labels_with_control<'a, V: Vfs + Clone, E>(
        &'a self,
        database: &'a Database<V>,
        vertex: VId,
        visible: &mut impl FnMut(LabelId) -> bool,
        poll: &mut impl FnMut() -> Result<(), E>,
        control: &mut impl FnMut(crate::gql_exec::source::SourceEvent) -> Result<(), E>,
        source_error: &impl Fn(WriteTxnError) -> E,
    ) -> Result<Option<PointLabels<'a>>, E> {
        use crate::gql_exec::source::{SourceEvent, find_vertex};
        use fgdb_delta_types::DeltaRow;
        use point_effect_selector::{EffectKey, select_range};
        let snapshot = self.query_snapshot(database).map_err(source_error)?;
        control(SourceEvent::Work)?;
        let mut labels = match find_vertex(snapshot, vertex, self.basis, control)? {
            Some(row) => {
                control(SourceEvent::SnapshotRecord)?;
                Some(PointLabels::new(&row.labels))
            }
            None => None,
        };
        if let Some(prepared) = &self.prepared {
            for coordinate in prepared.template.coordinate_entries() {
                poll()?;
                for (first, last) in [
                    (
                        EffectKey::CreateVertex(vertex),
                        EffectKey::CreateVertex(vertex),
                    ),
                    (
                        EffectKey::DeleteVertex(vertex),
                        EffectKey::DeleteVertex(vertex),
                    ),
                    (
                        EffectKey::Label(vertex, LabelId(0)),
                        EffectKey::Label(vertex, LabelId(u64::MAX)),
                    ),
                ] {
                    for effect in select_range(&coordinate.rows, first, last, poll)? {
                        poll()?;
                        match effect {
                            DeltaRow::CreateVertex {
                                vid,
                                labels: created,
                                ..
                            } if *vid == vertex => {
                                control(SourceEvent::Work)?;
                                labels = Some(PointLabels::new(created));
                            }
                            DeltaRow::DeleteVertex { vid, .. } if *vid == vertex => {
                                control(SourceEvent::Work)?;
                                labels = None;
                            }
                            DeltaRow::LabelMembership {
                                vid, label, after, ..
                            } if *vid == vertex && visible(*label) => {
                                control(SourceEvent::Work)?;
                                if let Some(labels) = &mut labels {
                                    control(SourceEvent::ScratchEntry)?;
                                    labels.edits.insert(*label, *after);
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        Ok(labels)
    }

    /// Borrow an edge's immutable type and effective fields at the transaction
    /// cut. Retirement may occur through either endpoint's canonical cascade;
    /// it never requires cloning the historical edge or interpreting intents.
    fn edge_metadata_with_control<'a, V: Vfs + Clone, E>(
        &'a self,
        database: &'a Database<V>,
        edge: EId,
        visible: &mut impl FnMut(fgdb_delta_types::PropertyKeyId) -> bool,
        poll: &mut impl FnMut() -> Result<(), E>,
        control: &mut impl FnMut(crate::gql_exec::source::SourceEvent) -> Result<(), E>,
        source_error: &impl Fn(WriteTxnError) -> E,
    ) -> Result<Option<PointEdgeMetadata<'a>>, E> {
        use crate::gql_exec::source::{SourceEvent, edge_properties_at};
        use fgdb_delta_types::{DeltaRow, PropertyKeyId};
        use point_effect_selector::{EffectKey, select_range};
        let snapshot = self.query_snapshot(database).map_err(source_error)?;
        control(SourceEvent::Work)?;
        let coordinate = snapshot.adjacency_index().statement_at_controlled(
            &snapshot.blocks,
            edge,
            self.basis,
            control,
        )?;
        let mut endpoints = None;
        let mut projected = match coordinate {
            Some((block, row)) => {
                control(SourceEvent::SnapshotRecord)?;
                let edge = &snapshot.blocks[block][row];
                endpoints = Some((edge.src, edge.dst));
                let properties = edge_properties_at(&snapshot.block_props, block, row);
                Some(PointEdgeMetadata {
                    relation: edge.relation,
                    properties: PointProperties::new(properties),
                })
            }
            None => None,
        };
        if let Some(prepared) = &self.prepared {
            for coordinate in prepared.template.coordinate_entries() {
                poll()?;
                let rows = &coordinate.rows;
                // Creation precedes every retirement in canonical order and
                // supplies the endpoints of an entirely staged edge.
                for effect in select_range(
                    rows,
                    EffectKey::CreateEdge(edge),
                    EffectKey::CreateEdge(edge),
                    poll,
                )? {
                    poll()?;
                    if let DeltaRow::CreateEdge {
                        eid,
                        src,
                        relation,
                        dst,
                        props,
                        ..
                    } = effect
                        && *eid == edge
                    {
                        control(SourceEvent::Work)?;
                        endpoints = Some((*src, *dst));
                        projected = Some(PointEdgeMetadata {
                            relation: *relation,
                            properties: PointProperties::new(props),
                        });
                    }
                }
                // PreparedWrite admits complete physical incidence. Only
                // these endpoints' retirements can contain this edge. Sort
                // the two IDs to retain canonical order and visit loops once.
                let endpoints = endpoints.map(|(src, dst)| (src.min(dst), src.max(dst)));
                for (first, last) in [
                    endpoints.map(|(first, _)| {
                        (
                            EffectKey::DeleteVertex(first),
                            EffectKey::DeleteVertex(first),
                        )
                    }),
                    endpoints.and_then(|(first, last)| {
                        (first != last).then_some((
                            EffectKey::DeleteVertex(last),
                            EffectKey::DeleteVertex(last),
                        ))
                    }),
                    Some((EffectKey::DeleteEdge(edge), EffectKey::DeleteEdge(edge))),
                    Some((
                        EffectKey::Property(ElementId::Edge(edge), PropertyKeyId(0)),
                        EffectKey::Property(ElementId::Edge(edge), PropertyKeyId(u64::MAX)),
                    )),
                ]
                .into_iter()
                .flatten()
                {
                    for effect in select_range(rows, first, last, poll)? {
                        poll()?;
                        match effect {
                            DeltaRow::DeleteVertex {
                                vid,
                                sorted_retired_incident_edges,
                                ..
                            } if endpoints.is_some_and(|(src, dst)| *vid == src || *vid == dst) => {
                                control(SourceEvent::Work)?;
                                if point_find(
                                    sorted_retired_incident_edges,
                                    edge,
                                    |id| *id,
                                    control,
                                )?
                                .is_some()
                                {
                                    projected = None;
                                }
                            }
                            DeltaRow::DeleteEdge { eid, .. } if *eid == edge => {
                                control(SourceEvent::Work)?;
                                projected = None;
                            }
                            DeltaRow::Property {
                                elem: ElementId::Edge(eid),
                                property,
                                after,
                                ..
                            } if *eid == edge && visible(*property) => {
                                if let Some(projected) = &mut projected {
                                    control(SourceEvent::Work)?;
                                    control(SourceEvent::ScratchEntry)?;
                                    projected.properties.edits.insert(*property, after.as_ref());
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        Ok(projected)
    }

    fn point_projection<'a, V: Vfs + Clone>(
        &'a self,
        database: &'a Database<V>,
        element: ElementId,
        field: PointReadField,
    ) -> Result<PointProjection<'a>, WriteTxnError> {
        self.point_projection_with_control(database, element, field, &mut |_| Ok(()), &|e| e)
    }

    // A borrowed source primitive. The accessor owns observation admission and
    // delivery. Health and the exact-cut check apply even to entirely staged or
    // absent elements. No query may use a stale writer as its history authority.
    fn point_projection_with_control<'a, V: Vfs + Clone, E>(
        &'a self,
        database: &'a Database<V>,
        element: ElementId,
        field: PointReadField,
        control: &mut impl FnMut(crate::gql_exec::source::SourceEvent) -> Result<(), E>,
        source_error: &impl Fn(WriteTxnError) -> E,
    ) -> Result<PointProjection<'a>, E> {
        use crate::gql_exec::source::{SourceEvent, find_vertex};
        self.ensure_database(database).map_err(source_error)?;
        database
            .ensure_readable()
            .map_err(WriteTxnError::from)
            .map_err(source_error)?;
        let snapshot = &database.snapshot;
        snapshot
            .check_frontier(self.basis)
            .map_err(WriteTxnError::from)
            .map_err(source_error)?;
        control(SourceEvent::Work)?;
        let mut edge_endpoints = None;
        if matches!(
            field,
            PointReadField::Property(_) | PointReadField::Label(_)
        ) {
            self.check_chain_element_read(element, control, source_error)?;
        }
        let mut projected = match element {
            ElementId::Vertex(vid) => match find_vertex(snapshot, vid, self.basis, control)? {
                Some(row) => {
                    control(SourceEvent::SnapshotRecord)?;
                    PointProjection::from_fields(field, &row.labels, &row.props, control)?
                }
                None => PointProjection::default(),
            },
            ElementId::Edge(eid) => {
                // Topology and payload use the SAME winning historical
                // coordinate, including retirement restatements. This is the
                // existing admitted index, not an independently built map.
                let coordinate = snapshot.adjacency_index().statement_at_controlled(
                    &snapshot.blocks,
                    eid,
                    self.basis,
                    control,
                )?;
                control(SourceEvent::Work)?;
                match coordinate {
                    Some((block, row)) => {
                        control(SourceEvent::SnapshotRecord)?;
                        let edge = &snapshot.blocks[block][row];
                        edge_endpoints = Some((edge.src, edge.dst));
                        let properties: &[(
                            fgdb_delta_types::PropertyKeyId,
                            CanonicalPropertyValue,
                        )] = match snapshot.block_props.get(block).and_then(Option::as_ref) {
                            Some(block) => {
                                let locator = block.locators[row];
                                if locator == 0 {
                                    &[]
                                } else {
                                    &block.rows[usize::from(locator) - 1]
                                }
                            }
                            None => &[],
                        };
                        PointProjection::from_fields(field, &[], properties, control)?
                    }
                    None => PointProjection::default(),
                }
            }
        };
        if let Some(prepared) = &self.prepared {
            use point_effect_selector::{EffectKey, select};
            for coordinate in prepared.template.coordinate_entries() {
                control(SourceEvent::Work)?;
                let rows = &coordinate.rows;
                // Coordinate order and the original canonical tag order stay
                // intact. Search only this identity's lifetime and field
                // prefixes; never scan or clone unrelated staged payloads.
                match element {
                    ElementId::Vertex(vertex) => {
                        for key in [
                            EffectKey::CreateVertex(vertex),
                            EffectKey::DeleteVertex(vertex),
                        ] {
                            projected.apply_selected(rows, key, element, field, control)?;
                        }
                    }
                    ElementId::Edge(edge) => {
                        for effect in select(rows, EffectKey::CreateEdge(edge), &mut || {
                            control(SourceEvent::Work)
                        })? {
                            control(SourceEvent::Work)?;
                            if let fgdb_delta_types::DeltaRow::CreateEdge { src, dst, .. } = effect
                            {
                                edge_endpoints = Some((*src, *dst));
                            }
                            projected.apply(element, field, effect, control)?;
                        }
                        // PreparedWrite admits complete, physical incidence.
                        // Only a retirement of one of these endpoints can
                        // contain this edge; apply still checks the exact
                        // sorted retirement image before clearing its value.
                        if let Some((source, target)) = edge_endpoints {
                            let (first, last) = (source.min(target), source.max(target));
                            for vertex in [Some(first), (last != first).then_some(last)]
                                .into_iter()
                                .flatten()
                            {
                                projected.apply_selected(
                                    rows,
                                    EffectKey::DeleteVertex(vertex),
                                    element,
                                    field,
                                    control,
                                )?;
                            }
                        }
                        projected.apply_selected(
                            rows,
                            EffectKey::DeleteEdge(edge),
                            element,
                            field,
                            control,
                        )?;
                    }
                }
                let key = match (element, field) {
                    (ElementId::Vertex(vertex), PointReadField::Label(label)) => {
                        Some(EffectKey::Label(vertex, label))
                    }
                    (_, PointReadField::Property(property)) => {
                        Some(EffectKey::Property(element, property))
                    }
                    _ => None,
                };
                if let Some(key) = key {
                    projected.apply_selected(rows, key, element, field, control)?;
                }
            }
        }
        Ok(projected)
    }
}

#[cfg(test)]
mod point_projection_tests {
    include!("point_projection_tests.rs");
}
