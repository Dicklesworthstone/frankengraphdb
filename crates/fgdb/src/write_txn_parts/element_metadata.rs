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
        use crate::gql_exec::source::SourceEvent;
        use fgdb_gql::{GlaExecutionEvent, GraphElementMetadataError};
        if let Some(error) = failure.borrow().as_ref() {
            return Err(GraphElementMetadataError::Source(
                WriteTxnError::Interrupted(error.clone()),
            ));
        }
        let checkpoint = || {
            if let Some(error) = failure.borrow().as_ref() {
                return Err(GraphElementMetadataError::Source(
                    WriteTxnError::Interrupted(error.clone()),
                ));
            }
            cx.checkpoint().map_err(|error| {
                let error = failure.borrow_mut().get_or_insert(error).clone();
                GraphElementMetadataError::Source(WriteTxnError::Interrupted(error))
            })
        };
        // The shared admitted collector owns scoped insertion-gap witnesses,
        // borrowed field merging and pre-copy charges. None retains the native
        // physical work contract; authorized hosts supply their verified mask.
        self.admitted_element_metadata_with_control(
            database,
            element,
            kind,
            catalog,
            None,
            &mut || checkpoint(),
            &mut |event| {
                checkpoint()?;
                control(match event {
                    SourceEvent::ScratchEntry => GlaExecutionEvent::ScratchEntry,
                    SourceEvent::Work | SourceEvent::SnapshotRecord => GlaExecutionEvent::Work,
                })
                .map_err(|()| GraphElementMetadataError::Interrupted)
            },
            &GraphElementMetadataError::Source,
        )
    }

    /// One source-bound reader for final RETURN/continuation expressions.
    /// Its two capabilities share the same staged view and interruption latch.
    fn element_reader_with_checkpoint<'a, V: Vfs + Clone>(
        &'a self,
        database: &'a Database<V>,
        cx: &'a fgdb_types::QueryCx,
        failure: &'a std::cell::RefCell<Option<Box<asupersync::error::Error>>>,
    ) -> impl fgdb_gql::GraphElementReader<WriteTxnError> + 'a {
        fgdb_gql::with_element_metadata(
            move |element, key| {
                self.element_property_with_checkpoint(database, cx, failure, element, key)
            },
            move |element, kind, catalog, control| {
                self.element_metadata_with_checkpoint(
                    database, cx, failure, element, kind, catalog, control,
                )
            },
        )
    }
}
