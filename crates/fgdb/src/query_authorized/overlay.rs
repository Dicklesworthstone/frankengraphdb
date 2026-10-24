//! Mask canonical native transaction rows before they enter the common GLA.
//! This module does not apply mutations, interpret intentions or build history.

use super::{
    AdmissionUsage, Database, Fault, GlaOutput, Governed, GqlQueryError, GqlQueryPolicy,
    GraphPatternFactorExecution, PlannerPredicates, PreparedGraphPattern,
    PreparedGraphPatternFactors, QueryCx, QueryError, ReadError, Tables, Vfs,
    execute_factor_tables, execute_tables,
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
            let tables = admit_overlay_tables(
                overlay,
                pattern,
                scope,
                policy,
                &mut usage,
                &mut node,
                &mut poll,
                &mut checkpoint,
            )?;
            execute_tables(tables, pattern, scope, policy, usage, &mut checkpoint)
        })
    }

    /// Evaluate both endpoint factors over one original, masked overlay
    /// admission. The original plan owns every witness and metadata demand;
    /// neither factor opens another source or bills the signed node quota again.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn select_factors_for_authorized_overlay(
        &self,
        cx: &QueryCx,
        overlay: &OverlayRows<'_, V>,
        request: &PreparedGraphPatternFactors<'_>,
        scope: &PlannerPredicates,
        policy: GqlQueryPolicy,
        mut node: impl FnMut() -> Result<(), QueryError>,
        mut poll: impl FnMut() -> Result<(), QueryError>,
        mut checkpoint: impl FnMut() -> Result<(), QueryError>,
    ) -> Result<GraphPatternFactorExecution, Fault> {
        checkpoint().map_err(GqlQueryError::Interrupted)?;
        self.ensure_readable().map_err(GqlQueryError::Source)?;
        cx.with_restriction(|| {
            let mut usage = AdmissionUsage::default();
            let tables = admit_overlay_tables(
                overlay,
                request.original(),
                scope,
                policy,
                &mut usage,
                &mut node,
                &mut poll,
                &mut checkpoint,
            )?;
            execute_factor_tables(tables, request, scope, policy, usage, &mut checkpoint)
        })
    }
}

/// The exact ordinary admission body, shared by ordinary and factor execution.
/// Returned references cannot outlive the canonical sparse overlay owner.
#[allow(clippy::too_many_arguments)]
fn admit_overlay_tables<'a, V: Vfs + Clone, Row: GlaOutput>(
    overlay: &'a OverlayRows<'_, V>,
    pattern: &PreparedGraphPattern<Row>,
    scope: &PlannerPredicates,
    policy: GqlQueryPolicy,
    usage: &mut AdmissionUsage,
    node: &mut impl FnMut() -> Result<(), QueryError>,
    poll: &mut impl FnMut() -> Result<(), QueryError>,
    checkpoint: &mut impl FnMut() -> Result<(), QueryError>,
) -> Result<Tables<'a>, Fault> {
    let mut tables = Tables::new();
    let mut control = |event| {
        checkpoint().map_err(GqlQueryError::Interrupted)?;
        usage.observe::<ReadError, QueryError>(policy, event)
    };
    // Historical traversal and sparse shadowing never bill hidden data.
    // Tables admit each winner BEFORE retaining its reference. Every record
    // is still admitted and charged; only the read witness narrows to what
    // the original plan can bind (fgdb-h1d6l).
    let mut scan = |_| poll().map_err(GqlQueryError::Interrupted);
    overlay.visit_vertices(pattern.plan().vertex_scan_domain(), &mut scan, |row, _| {
        tables
            .admit_vertex(
                row,
                scope,
                &mut || node().map_err(GqlQueryError::Interrupted),
                &mut control,
            )
            .map(drop)
    })?;
    if pattern.plan().reads_edges() {
        let relations = pattern.plan().edge_relations();
        overlay
            .visit_edges(relations.as_ref(), &mut scan, |edge, properties, _| {
                tables
                    .admit_edge(
                        ((edge.eid, edge.src, edge.relation, edge.dst), properties),
                        scope,
                        &mut control,
                    )
                    .map(drop)
            })
            .ok_or(GqlQueryError::IdentifiedEdgesRequired)??;
    }
    tables.seal();
    tables.metadata(pattern.plan(), scope, &mut control)?;
    Ok(tables)
}

#[cfg(test)]
#[path = "overlay_tests.rs"]
mod tests;
