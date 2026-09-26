//! Mask canonical native transaction rows before they enter the common GLA.
//! This module does not apply mutations, interpret intentions or build history.

use super::{
    AdmissionUsage, Database, GlaOutput, Governed, GqlQueryError, GqlQueryPolicy,
    PlannerPredicates, PreparedGraphPattern, QueryCx, QueryError, ReadError, Tables,
    VertexRow, Vfs, execute_tables,
};
use crate::EdgeRecord;

impl<V: Vfs + Clone> Database<V> {
    /// Internal only: rows must be from the same exclusively borrowed native
    /// WriteTxn's vertices()/edges() at its pinned basis. That source retains
    /// the native read dependencies and resolves its canonical prepared net
    /// effects, including deletes and ensure aliases, BEFORE scope admission.
    /// The trusted caller already verified ReadWrite rights and owns the one
    /// write permit backing these controls. No graph rows are returned here.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn select_for_authorized_overlay<Row: GlaOutput>(
        &self,
        cx: &QueryCx,
        vertices: &[VertexRow],
        edges: &[EdgeRecord],
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
                for row in vertices {
                    // Original hidden rows incur cancellation polls, not
                    // signed/native resource charges (FG-INV-20).
                    poll().map_err(GqlQueryError::Interrupted)?;
                    tables.admit_vertex(
                        row,
                        scope,
                        &mut || node().map_err(GqlQueryError::Interrupted),
                        &mut control,
                    )?;
                }
                if pattern.plan().reads_edges() {
                    for record in edges {
                        poll().map_err(GqlQueryError::Interrupted)?;
                        let edge = &record.entry;
                        tables.admit_edge(
                            ((edge.eid, edge.src, edge.relation, edge.dst), &record.props),
                            scope,
                            &mut control,
                        )?;
                    }
                }
                tables.metadata(pattern.plan(), scope, &mut control)?;
            }
            execute_tables(tables, pattern, scope, policy, usage, &mut checkpoint)
        })
    }
}
