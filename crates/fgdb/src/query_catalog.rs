//! Catalog procedures over the statement's frozen host-declared names.
//! A catalog is not graph visibility: the live capability masks identities
//! before names are inspected, charged, copied or returned.

use crate::gql_exec::{AdmissionUsage, source::SourceEvent};
use crate::{GqlError, ReadError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GRAPH_VALUE_PAYLOAD_UNIT_BYTES, GraphValue, GraphValueRow};
use fgdb_gql::{
    GlaExecutionStats, GqlBudgetDimension, GqlExecutionStats, GqlQueryError, GqlQueryExecution,
    GqlQueryPolicy, ProcedureCatalogKind, ProcedureCatalogProjection,
};
use fgdb_types::CanonicalScalar;
use fgdb_warden::PlannerPredicates;

/// `control(0)` is a live cancellation poll, with no signed work charge.
/// Hidden catalog entries therefore affect neither visible work/scratch
/// counters nor quota refusals. The caller retains the same operation permit
/// through final delivery; procedure rows are private intermediate rows.
pub(crate) fn catalog_procedure<C>(
    catalog: &ProcedureCatalogProjection,
    policy: GqlQueryPolicy,
    scope: Option<&PlannerPredicates>,
    mut control: impl FnMut(u64) -> Result<(), C>,
) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GqlError, C>> {
    let mut usage = AdmissionUsage::default();
    control(1).map_err(GqlQueryError::Interrupted)?;
    usage.observe(policy, SourceEvent::Work)?;
    let mut rows = Vec::new();
    for (id, name) in catalog.entries() {
        control(0).map_err(GqlQueryError::Interrupted)?;
        if scope.is_some_and(|scope| match catalog.kind() {
            ProcedureCatalogKind::Labels => !scope.allows_label(LabelId(*id)),
            ProcedureCatalogKind::RelationshipTypes => !scope.allows_relation(RelationId(*id)),
            ProcedureCatalogKind::PropertyKeys => !scope.allows_property(PropertyKeyId(*id)),
        }) {
            continue;
        }
        policy
            .rows
            .check(GqlBudgetDimension::ResultRows, rows.len() as u64 + 1)
            .map_err(GqlQueryError::Rows)?;
        control(1).map_err(GqlQueryError::Interrupted)?;
        usage.observe(policy, SourceEvent::ScratchEntry)?;
        for _ in 0..name.len().div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES) {
            control(1).map_err(GqlQueryError::Interrupted)?;
            usage.observe(policy, SourceEvent::ScratchEntry)?;
        }
        let value = CanonicalScalar::ucs_basic_text(name)
            .map_err(|_| GqlQueryError::Source(GqlError::Read(ReadError::InvalidPropertyMap)))?;
        rows.push(GraphValueRow::from_owned_values(vec![GraphValue::Scalar(
            value,
        )]));
    }
    control(0).map_err(GqlQueryError::Interrupted)?;
    usage.finish(
        policy,
        Ok(GqlQueryExecution {
            rows: GqlExecutionStats {
                snapshot_records: 0,
                result_rows: rows.len() as u64,
            },
            evaluator: GlaExecutionStats::default(),
            value: rows,
        }),
    )
}
