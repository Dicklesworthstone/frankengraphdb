// An exact basis count plus the canonical staged membership transitions.
// Only identities touched by those transitions need historical before-images;
// neither the live graph nor its property payloads are materialized.

struct CountMembership {
    before: bool,
    after: bool,
    live: bool,
}

fn count_label_member<E>(
    labels: &[LabelId],
    wanted: LabelId,
    control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
) -> Result<bool, E> {
    let (mut low, mut high) = (0, labels.len());
    while low < high {
        control(SourceEvent::Work)?;
        let mid = low + (high - low) / 2;
        match labels[mid].cmp(&wanted) {
            core::cmp::Ordering::Less => low = mid + 1,
            core::cmp::Ordering::Greater => high = mid,
            core::cmp::Ordering::Equal => return Ok(true),
        }
    }
    Ok(false)
}

fn count_basis_membership<E>(
    snapshot: &Snapshot,
    basis: fgdb_types::CommitSeq,
    source: fgdb_gql::GraphCountSource,
    element: ElementId,
    control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
) -> Result<CountMembership, E> {
    use fgdb_gql::GraphCountSource;
    let (live, member) = match element {
        ElementId::Vertex(vid) => {
            if let Some(row) = source::find_vertex(snapshot, vid, basis, control)? {
                control(SourceEvent::SnapshotRecord)?;
                let member = match source {
                    GraphCountSource::Vertices => true,
                    GraphCountSource::Label(label) => {
                        count_label_member(&row.labels, label, control)?
                    }
                    _ => false,
                };
                (true, member)
            } else {
                (false, false)
            }
        }
        ElementId::Edge(eid) => {
            if let Some((block, row)) = snapshot.adjacency_index().statement_at_controlled(
                &snapshot.blocks,
                eid,
                basis,
                control,
            )? {
                control(SourceEvent::SnapshotRecord)?;
                let member = match source {
                    GraphCountSource::Edges => true,
                    GraphCountSource::Relation(relation) => {
                        snapshot.blocks[block][row].relation == relation
                    }
                    _ => false,
                };
                (true, member)
            } else {
                (false, false)
            }
        }
    };
    Ok(CountMembership {
        before: member,
        after: member,
        live,
    })
}

fn count_transition<'a, E>(
    changes: &'a mut BTreeMap<ElementId, CountMembership>,
    snapshot: &Snapshot,
    basis: fgdb_types::CommitSeq,
    source: fgdb_gql::GraphCountSource,
    element: ElementId,
    control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
) -> Result<&'a mut CountMembership, E> {
    use std::collections::btree_map::Entry;
    control(SourceEvent::Work)?;
    match changes.entry(element) {
        Entry::Occupied(entry) => Ok(entry.into_mut()),
        Entry::Vacant(entry) => {
            control(SourceEvent::ScratchEntry)?;
            let before = count_basis_membership(snapshot, basis, source, element, control)?;
            Ok(entry.insert(before))
        }
    }
}

fn count_created<E>(
    changes: &mut BTreeMap<ElementId, CountMembership>,
    element: ElementId,
    member: bool,
    control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
) -> Result<(), E> {
    use std::collections::btree_map::Entry;
    control(SourceEvent::Work)?;
    match changes.entry(element) {
        Entry::Occupied(mut entry) => {
            let change = entry.get_mut();
            change.after = member;
            change.live = true;
        }
        Entry::Vacant(entry) => {
            control(SourceEvent::ScratchEntry)?;
            // Canonical creates already prove absence at the preparation's
            // basis. Do not probe or copy an unrelated stored payload again.
            entry.insert(CountMembership {
                before: false,
                after: member,
                live: true,
            });
        }
    }
    Ok(())
}

impl WriteTxn {
    fn query_count_metadata<E>(
        &self,
        snapshot: &Snapshot,
        source: fgdb_gql::GraphCountSource,
        control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
    ) -> Result<Option<u64>, E> {
        use fgdb_gql::GraphCountSource;
        // Retain a logical population even when finalization later refuses,
        // a savepoint is rolled back, or the result page suppresses every row.
        self.note_count_read(source, control)?;
        let Some(matching) = self.basis_counts.lookup(source, control)? else {
            return Ok(None);
        };
        let Some(prepared) = &self.prepared else {
            return Ok(Some(matching));
        };
        let vertices = matches!(
            source,
            GraphCountSource::Vertices | GraphCountSource::Label(_)
        );
        // A conditional staged birth may disappear or become active during
        // idempotent replay even when the committed winner is outside this
        // count's label domain. Retain its proposal identity, including raw
        // ENSURE no-ops absent from the canonical template. These are only
        // observations; raw intentions never supply the numeric answer.
        for batch in &self.staged {
            control(SourceEvent::Work)?;
            for row in &batch.rows {
                control(SourceEvent::Work)?;
                let element = match row {
                    PendingRow::Vertex { vid, .. } | PendingRow::DeleteVertex { vid, .. }
                        if vertices =>
                    {
                        Some(ElementId::Vertex(*vid))
                    }
                    PendingRow::Edge { eid, .. } | PendingRow::DeleteEdge { eid, .. }
                        if !vertices =>
                    {
                        Some(ElementId::Edge(*eid))
                    }
                    _ => None,
                };
                if let Some(element) = element
                    && !self.read_set.borrow().contains(&element)
                {
                    control(SourceEvent::ScratchEntry)?;
                    self.read_set.borrow_mut().insert(element);
                }
            }
        }
        let mut changes = BTreeMap::new();
        for coordinate in prepared.template.coordinate_entries() {
            control(SourceEvent::Work)?;
            for row in &coordinate.rows {
                control(SourceEvent::Work)?;
                match row {
                    DeltaRow::CreateVertex { vid, labels, .. } if vertices => {
                        let member = match source {
                            GraphCountSource::Label(label) => {
                                count_label_member(labels, label, control)?
                            }
                            _ => true,
                        };
                        count_created(&mut changes, ElementId::Vertex(*vid), member, control)?;
                    }
                    DeltaRow::LabelMembership {
                        vid, label, after, ..
                    } if source == GraphCountSource::Label(*label) => {
                        let change = count_transition(
                            &mut changes,
                            snapshot,
                            self.basis,
                            source,
                            ElementId::Vertex(*vid),
                            control,
                        )?;
                        change.after = change.live && *after;
                    }
                    DeltaRow::CreateEdge { eid, relation, .. } if !vertices => {
                        let member = match source {
                            GraphCountSource::Relation(wanted) => *relation == wanted,
                            _ => true,
                        };
                        count_created(&mut changes, ElementId::Edge(*eid), member, control)?;
                    }
                    DeltaRow::DeleteEdge { eid, .. } if !vertices => {
                        let change = count_transition(
                            &mut changes,
                            snapshot,
                            self.basis,
                            source,
                            ElementId::Edge(*eid),
                            control,
                        )?;
                        change.after = false;
                        change.live = false;
                    }
                    DeltaRow::DeleteVertex {
                        vid,
                        sorted_retired_incident_edges,
                        ..
                    } => {
                        if vertices {
                            let change = count_transition(
                                &mut changes,
                                snapshot,
                                self.basis,
                                source,
                                ElementId::Vertex(*vid),
                                control,
                            )?;
                            change.after = false;
                            change.live = false;
                        } else {
                            // Cascades contain actual edge identities across
                            // relations. Shared incidences contribute once.
                            for eid in sorted_retired_incident_edges {
                                control(SourceEvent::Work)?;
                                let change = count_transition(
                                    &mut changes,
                                    snapshot,
                                    self.basis,
                                    source,
                                    ElementId::Edge(*eid),
                                    control,
                                )?;
                                change.after = false;
                                change.live = false;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        // A signed accumulator makes the answer independent of canonical row
        // order. Intermediate removals cannot underflow an otherwise valid
        // net count, and repeated effects on one identity contribute once.
        let mut total = i128::from(matching);
        for change in changes.values() {
            control(SourceEvent::Work)?;
            let delta = i128::from(change.after) - i128::from(change.before);
            let Some(next) = total.checked_add(delta) else {
                return Ok(None);
            };
            total = next;
        }
        // Metadata is discardable. An inconsistent/unrepresentable count
        // must fall back to the original source, never wrap or saturate.
        Ok(u64::try_from(total).ok())
    }
}
