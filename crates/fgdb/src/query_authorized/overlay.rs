//! Mask canonical native transaction rows before they enter the common GLA.
//! This module does not apply mutations, interpret intentions or build history.

use super::{
    AdmissionUsage, Database, Fault, GlaOutput, Governed, GqlQueryError, GqlQueryPolicy,
    GraphPatternFactorExecution, PlannerPredicates, PreparedGraphPattern,
    PreparedGraphPatternFactors, QueryCx, QueryError, ReadError, Tables, Vfs,
    execute_factor_tables, execute_tables,
};
use crate::write_txn::overlay_scan::{BoundOverlayCandidates, OverlayRows};

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
        node: impl FnMut() -> Result<(), QueryError>,
        poll: impl FnMut() -> Result<(), QueryError>,
        checkpoint: impl FnMut() -> Result<(), QueryError>,
    ) -> Governed<Row> {
        self.select_for_authorized_overlay_with_seeds(
            cx, overlay, pattern, None, scope, policy, node, poll, checkpoint,
        )
    }

    /// A relational root restriction shares the ordinary scoped table and
    /// signed-node admission. The write adapter checks definition eligibility
    /// before entering; no alternate evaluator or capability is introduced.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn select_for_authorized_overlay_with_seeds<Row: GlaOutput>(
        &self,
        cx: &QueryCx,
        overlay: &OverlayRows<'_, V>,
        pattern: &PreparedGraphPattern<Row>,
        seeds: Option<fgdb_gql::GraphSeeds<'_>>,
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
            let (tables, bound) = admit_overlay_tables(
                overlay,
                pattern,
                seeds,
                scope,
                policy,
                &mut usage,
                &mut node,
                &mut poll,
                &mut checkpoint,
            )?;
            let result = execute_tables(tables, pattern, scope, policy, usage, &mut checkpoint)?;
            if let Some(bound) = bound {
                overlay.accept_bound_observations(&bound);
            }
            Ok(result)
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
            let (tables, bound) = admit_overlay_tables(
                overlay,
                request.original(),
                None,
                scope,
                policy,
                &mut usage,
                &mut node,
                &mut poll,
                &mut checkpoint,
            )?;
            let result =
                execute_factor_tables(tables, request, scope, policy, usage, &mut checkpoint)?;
            if let Some(bound) = bound {
                overlay.accept_bound_observations(&bound);
            }
            Ok(result)
        })
    }
}

/// The exact ordinary admission body, shared by ordinary and factor execution.
/// Returned references cannot outlive the canonical sparse overlay owner.
#[allow(clippy::too_many_arguments)]
fn admit_overlay_tables<'a, V: Vfs + Clone, Row: GlaOutput>(
    overlay: &'a OverlayRows<'_, V>,
    pattern: &PreparedGraphPattern<Row>,
    seeds: Option<fgdb_gql::GraphSeeds<'_>>,
    scope: &PlannerPredicates,
    policy: GqlQueryPolicy,
    usage: &mut AdmissionUsage,
    node: &mut impl FnMut() -> Result<(), QueryError>,
    poll: &mut impl FnMut() -> Result<(), QueryError>,
    checkpoint: &mut impl FnMut() -> Result<(), QueryError>,
) -> Result<(Tables<'a>, Option<BoundOverlayCandidates>), Fault> {
    let mut tables = Tables::new();
    let mut control = |event| {
        checkpoint().map_err(GqlQueryError::Interrupted)?;
        usage.observe::<ReadError, QueryError>(policy, event)
    };
    // Physical traversal and sparse shadowing only poll. The same native
    // indexed closure as the owner path is usable when the scope hides
    // nothing; scoped or unsupported plans retain complete masked admission.
    // Every retained winner still crosses the ordinary signed/native charge
    // boundary, and all statements continue sharing one permit and allowance.
    let mut scan = |_| poll().map_err(GqlQueryError::Interrupted);
    let bound = match seeds {
        Some(seeds) => overlay.seeded_candidates(pattern.plan(), seeds, scope, &mut scan)?,
        None => overlay.bound_candidates(pattern.plan(), scope, &mut scan)?,
    };
    let mut admit_vertex = |row: &'a crate::VertexRow, _: &mut _| {
        tables
            .admit_vertex(
                row,
                scope,
                &mut || node().map_err(GqlQueryError::Interrupted),
                &mut control,
            )
            .map(drop)
    };
    match &bound {
        Some(bound) => overlay.visit_bound_vertices(bound, &mut scan, &mut admit_vertex)?,
        None => overlay.visit_vertices(
            pattern.plan().vertex_scan_domain(),
            &mut scan,
            &mut admit_vertex,
        )?,
    }
    if pattern.plan().reads_edges() {
        let mut admit_edge = |edge: &'a fgdb_strata::AdjacencyEntry,
                              properties: &'a [(
            fgdb_delta_types::PropertyKeyId,
            fgdb_types::CanonicalPropertyValue,
        )],
                              _: &mut _| {
            tables
                .admit_edge(
                    ((edge.eid, edge.src, edge.relation, edge.dst), properties),
                    scope,
                    &mut control,
                )
                .map(drop)
        };
        match &bound {
            Some(bound) => overlay.visit_bound_edges(bound, &mut scan, &mut admit_edge),
            None => {
                let relations = pattern.plan().edge_relations();
                overlay.visit_edges(relations.as_ref(), &mut scan, &mut admit_edge)
            }
        }
        .ok_or(GqlQueryError::IdentifiedEdgesRequired)??;
    }
    tables.seal();
    tables.metadata(pattern.plan(), scope, &mut control)?;
    Ok((tables, bound))
}

#[cfg(test)]
#[path = "overlay_tests.rs"]
mod tests;
