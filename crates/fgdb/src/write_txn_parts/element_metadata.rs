// Carried element metadata shares the transaction's admitted basis and
// canonical NET effects. No whole-row clone or second overlay is constructed.

impl WriteTxn {
    #[allow(clippy::too_many_arguments)]
    fn element_metadata_with_checkpoint<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        cx: &fgdb_types::QueryCx,
        failure: &std::cell::RefCell<Option<Box<asupersync::error::Error>>>,
        element: ElementId,
        kind: fgdb_gql::GraphElementMetadata,
        catalog: &fgdb_gql::ReverseSymbolCatalog,
        control: &mut dyn FnMut(fgdb_gql::GlaExecutionEvent) -> Result<(), ()>,
    ) -> Result<
        Option<fgdb_gql::algebra::GraphValue>,
        fgdb_gql::GraphElementMetadataError<WriteTxnError>,
    > {
        use crate::gql_exec::property_maps;
        use crate::gql_exec::source::SourceEvent;
        use fgdb_gql::{GlaExecutionEvent, GraphElementMetadata, GraphElementMetadataError};

        let source_error = GraphElementMetadataError::Source;
        if let Some(error) = failure.borrow().as_ref() {
            return Err(source_error(WriteTxnError::Interrupted(error.clone())));
        }
        // Wrong ownership, stale history and initial cancellation reveal no
        // data and must not create an observation. All later failures do.
        let _ = self.query_snapshot(database).map_err(source_error)?;
        let mut poll = || {
            cx.checkpoint().map_err(|error| {
                let mut failure = failure.borrow_mut();
                source_error(WriteTxnError::Interrupted(
                    failure.get_or_insert(error).clone(),
                ))
            })
        };
        poll()?;
        control(GlaExecutionEvent::Work).map_err(|()| GraphElementMetadataError::Interrupted)?;
        let mut attempt = ProjectionReadAttempt {
            reads: &self.point_reads,
            element,
            accepted: false,
        };
        let mut source_control = |event| {
            control(match event {
                SourceEvent::Work | SourceEvent::SnapshotRecord => GlaExecutionEvent::Work,
                SourceEvent::ScratchEntry => GlaExecutionEvent::ScratchEntry,
            })
            .map_err(|()| GraphElementMetadataError::Interrupted)
        };
        // An entire field set includes its insertion gap and an absent
        // element's lifetime. Reuse the ordinary conservative element witness;
        // observing only currently present field IDs would miss new fields.
        if kind == GraphElementMetadata::Type {
            source_control(SourceEvent::ScratchEntry)?;
            source_control(SourceEvent::ScratchEntry)?;
            self.point_reads
                .borrow_mut()
                .record(element, PointReadField::EdgeTopology);
        } else {
            source_control(SourceEvent::ScratchEntry)?;
            self.read_set.borrow_mut().insert(element);
        }
        let read_error = |error| source_error(WriteTxnError::Read(error));
        let value = match (element, kind) {
            (ElementId::Vertex(vertex), GraphElementMetadata::Labels) => {
                let projected = self.vertex_labels_with_control(
                    database,
                    vertex,
                    &mut |_| true,
                    &mut poll,
                    &mut source_control,
                    &source_error,
                )?;
                projected
                    .map(|labels| {
                        property_maps::collect_labels(
                            labels.effective_labels_iter(),
                            catalog,
                            |_| true,
                            &mut source_control,
                            &read_error,
                        )
                    })
                    .transpose()?
            }
            (
                ElementId::Vertex(vertex),
                GraphElementMetadata::Properties | GraphElementMetadata::Keys,
            ) => {
                let projected = self.vertex_properties_with_control(
                    database,
                    vertex,
                    &mut |_| true,
                    &mut poll,
                    &mut source_control,
                    &source_error,
                )?;
                projected
                    .map(|properties| {
                        transaction_metadata_properties(
                            &properties,
                            kind,
                            catalog,
                            &mut source_control,
                            &read_error,
                        )
                    })
                    .transpose()?
            }
            (ElementId::Edge(edge), kind) if kind != GraphElementMetadata::Labels => {
                let projected = self.edge_metadata_with_control(
                    database,
                    edge,
                    kind != GraphElementMetadata::Type,
                    &mut poll,
                    &mut source_control,
                    &source_error,
                )?;
                projected
                    .map(|(relation, properties)| {
                        if kind == GraphElementMetadata::Type {
                            property_maps::metadata(
                                property_maps::MetadataRecord::Edge(relation, &[]),
                                kind,
                                catalog,
                                |_| true,
                                |_| true,
                                &mut source_control,
                                &read_error,
                            )
                        } else {
                            transaction_metadata_properties(
                                &properties,
                                kind,
                                catalog,
                                &mut source_control,
                                &read_error,
                            )
                        }
                    })
                    .transpose()?
            }
            // Ordinary evaluation rejects incompatible value domains before
            // invoking the host. Keep this independent seam fail-closed too.
            _ => return Err(read_error(ReadError::InvalidPropertyMap)),
        };
        attempt.accepted = true;
        Ok(value)
    }

    /// Borrow the edge's selected historical fields and canonical edits. A
    /// cascade may retire the edge without any standalone DeleteEdge row.
    fn edge_metadata_with_control<'a, V: Vfs + Clone, E>(
        &'a self,
        database: &'a Database<V>,
        edge: EId,
        include_properties: bool,
        poll: &mut impl FnMut() -> Result<(), E>,
        control: &mut impl FnMut(crate::gql_exec::source::SourceEvent) -> Result<(), E>,
        source_error: &impl Fn(WriteTxnError) -> E,
    ) -> Result<Option<(RelationId, PointProperties<'a>)>, E> {
        use crate::gql_exec::source::{SourceEvent, edge_properties_at};
        use fgdb_delta_types::DeltaRow;

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
                let entry = &snapshot.blocks[block][row];
                endpoints = Some((entry.src, entry.dst));
                let properties = if include_properties {
                    edge_properties_at(&snapshot.block_props, block, row)
                } else {
                    &[]
                };
                Some((entry.relation, PointProperties::new(properties)))
            }
            None => None,
        };
        if let Some(prepared) = &self.prepared {
            for coordinate in prepared.template.coordinate_entries() {
                poll()?;
                for effect in &coordinate.rows {
                    poll()?;
                    match effect {
                        DeltaRow::CreateEdge {
                            eid,
                            src,
                            relation,
                            dst,
                            props,
                            ..
                        } if *eid == edge => {
                            control(SourceEvent::Work)?;
                            endpoints = Some((*src, *dst));
                            let properties = if include_properties {
                                props.as_slice()
                            } else {
                                &[]
                            };
                            projected = Some((*relation, PointProperties::new(properties)));
                        }
                        DeltaRow::DeleteEdge { eid, .. } if *eid == edge => {
                            control(SourceEvent::Work)?;
                            projected = None;
                        }
                        DeltaRow::DeleteVertex {
                            vid,
                            sorted_retired_incident_edges,
                            ..
                        } if endpoints.is_some_and(|(src, dst)| *vid == src || *vid == dst) => {
                            control(SourceEvent::Work)?;
                            if point_find(sorted_retired_incident_edges, edge, |eid| *eid, control)?
                                .is_some()
                            {
                                projected = None;
                            }
                        }
                        DeltaRow::Property {
                            elem: ElementId::Edge(eid),
                            property,
                            after,
                            ..
                        } if include_properties && *eid == edge => {
                            control(SourceEvent::Work)?;
                            if let Some((_, properties)) = &mut projected {
                                control(SourceEvent::ScratchEntry)?;
                                properties.edits.insert(*property, after.as_ref());
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        Ok(projected)
    }
}

fn transaction_metadata_properties<E>(
    properties: &PointProperties<'_>,
    kind: fgdb_gql::GraphElementMetadata,
    catalog: &fgdb_gql::ReverseSymbolCatalog,
    control: &mut impl FnMut(crate::gql_exec::source::SourceEvent) -> Result<(), E>,
    failure: &impl Fn(ReadError) -> E,
) -> Result<fgdb_gql::algebra::GraphValue, E> {
    if kind == fgdb_gql::GraphElementMetadata::Keys {
        crate::gql_exec::property_maps::collect_keys(
            properties.effective_properties().map(|(key, _)| key),
            catalog,
            |_| true,
            control,
            failure,
        )
    } else {
        crate::gql_exec::property_maps::collect(
            properties.effective_properties(),
            Some(catalog),
            |_| true,
            control,
            failure,
        )
    }
}
