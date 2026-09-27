//! Mask canonical native transaction rows before they enter the common GLA.
//! This module does not apply mutations, interpret intentions or build history.

use super::{
    AdmissionUsage, Database, GlaOutput, Governed, GqlQueryError, GqlQueryPolicy,
    PlannerPredicates, PreparedGraphPattern, QueryCx, QueryError, ReadError, Tables,
    Vfs, execute_tables,
};
use crate::write_txn::overlay_scan::OverlayRows;

impl<V: Vfs + Clone> Database<V> {
    /// Internal only: the owner borrows the native transaction and its database
    /// at the pinned basis and owns only changed rows. That source retains
    /// the native read dependencies and resolves its canonical prepared net
    /// effects, including deletes and ensure aliases, BEFORE scope admission.
    /// The trusted caller already verified ReadWrite rights and owns the one
    /// write permit backing these controls. No graph rows are returned here.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn select_for_authorized_overlay<Row: GlaOutput>(
        &self,
        cx: &QueryCx,
        overlay: &OverlayRows<'_, V>,
        pattern: &PreparedGraphPattern<Row>,
        scope: &PlannerPredicates,
        policy: GqlQueryPolicy,
        mut node: impl FnMut() -> Result<(), QueryError>,
        mut poll: impl FnMut() -> Result<(), QueryError>,
        mut checkpoint: impl FnMut() -> Result<(), QueryError>,
    ) -> Governed<Row> {
        checkpoint().map_err(GqlQueryError::Interrupted)?;
        self.ensure_readable().map_err(GqlQueryError::Source)?;
        cx.with_restriction(|| {
            let mut usage = AdmissionUsage::default();
            let mut tables = Tables::new();
            {
                let mut control = |event| {
                    checkpoint().map_err(GqlQueryError::Interrupted)?;
                    usage.observe::<ReadError, QueryError>(policy, event)
                };
                // Historical traversal and sparse shadowing never bill hidden
                // data. Tables admit each winner BEFORE retaining its reference.
                let mut scan = |_| poll().map_err(GqlQueryError::Interrupted);
                overlay.visit_vertices(&mut scan, |row, _| {
                    tables.admit_vertex(
                        row,
                        scope,
                        &mut || node().map_err(GqlQueryError::Interrupted),
                        &mut control,
                    )
                })?;
                if pattern.plan().reads_edges() {
                    overlay.visit_edges(&mut scan, |edge, properties, _| {
                        tables.admit_edge(
                            ((edge.eid, edge.src, edge.relation, edge.dst), properties),
                            scope,
                            &mut control,
                        )
                    }).ok_or(GqlQueryError::IdentifiedEdgesRequired)??;
                }
                tables.metadata(pattern.plan(), scope, &mut control)?;
            }
            execute_tables(tables, pattern, scope, policy, usage, &mut checkpoint)
        })
    }
}
