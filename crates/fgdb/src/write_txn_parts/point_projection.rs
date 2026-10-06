// A projection of the admitted historical winner and the prepared NET effects.
// This does not interpret PendingRow, expose metadata, or own a second overlay.

#[derive(Default)]
struct PointProjection<'a> {
    exists: bool,
    property: Option<&'a CanonicalScalar>,
    label: bool,
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
    fn from_fields<E>(
        field: PointReadField,
        labels: &[LabelId],
        properties: &'a [(fgdb_delta_types::PropertyKeyId, CanonicalScalar)],
        control: &mut impl FnMut(crate::gql_exec::source::SourceEvent) -> Result<(), E>,
    ) -> Result<Self, E> {
        let mut projected = Self {
            exists: true,
            ..Self::default()
        };
        match field {
            PointReadField::Property(requested) => {
                projected.property = point_find(properties, requested, |(key, _)| *key, control)?
                    .map(|(_, value)| value);
            }
            PointReadField::Label(requested) => {
                projected.label = point_find(labels, requested, |label| *label, control)?.is_some();
            }
            _ => unreachable!("only explicit property and label accessors project a point"),
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
        let mut projected = match element {
            ElementId::Vertex(vid) => {
                match find_vertex(&snapshot.patches, vid, self.basis, control)? {
                    Some(row) => {
                        control(SourceEvent::SnapshotRecord)?;
                        PointProjection::from_fields(field, &row.labels, &row.props, control)?
                    }
                    None => PointProjection::default(),
                }
            }
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
                        let properties: &[(fgdb_delta_types::PropertyKeyId, CanonicalScalar)] =
                            match snapshot.block_props.get(block).and_then(Option::as_ref) {
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
            for coordinate in prepared.template.coordinate_entries() {
                control(SourceEvent::Work)?;
                for effect in &coordinate.rows {
                    control(SourceEvent::Work)?;
                    projected.apply(element, field, effect, control)?;
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
