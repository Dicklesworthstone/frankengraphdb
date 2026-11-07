//! Catalog discovery is a regular ProcedureCall source. Names are frozen by
//! preparation and filtered by the executing host's authority before they
//! consume visible work, scratch or row allowance (FG-INV-20).

use crate::GqlError;
use crate::gql_exec::{AdmissionUsage, source::SourceEvent};
use fgdb_gql::algebra::{GRAPH_VALUE_PAYLOAD_UNIT_BYTES, GraphValue, GraphValueRow};
use fgdb_gql::{
    GlaExecutionStats, GqlBudgetDimension, GqlExecutionStats, GqlQueryError, GqlQueryExecution,
    GqlQueryPolicy, GraphSymbol, PreparedProcedureCall,
};
use fgdb_types::{CanonicalScalar, QueryCx};

pub(crate) fn is_catalog(call: &PreparedProcedureCall) -> bool {
    call.namespace().eq_ignore_ascii_case("db")
}

fn refusal<C>(reason: &'static str) -> GqlQueryError<GqlError, C> {
    GqlQueryError::Source(GqlError::Procedure(super::ProcedureError::Catalog(reason)))
}

/// The physical enumeration polls even for hidden entries, but only visible
/// entries spend signed work and native resource allowance. The two controls
/// stay separate so a hidden catalog extension cannot change a visible quota
/// refusal or the query's reported counters. Preparation sorts the frozen
/// entries by name, so filtering preserves deterministic lexical order.
pub(crate) fn catalog_procedure<C>(
    call: &PreparedProcedureCall,
    arguments: &[GraphValue],
    policy: GqlQueryPolicy,
    mut visible: impl FnMut(GraphSymbol) -> bool,
    mut poll: impl FnMut() -> Result<(), C>,
    mut charge_work: impl FnMut(u64) -> Result<(), C>,
) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GqlError, C>> {
    poll().map_err(GqlQueryError::Interrupted)?;
    let output = if call.name().eq_ignore_ascii_case("labels") {
        "label"
    } else if call.name().eq_ignore_ascii_case("relationshipTypes") {
        "relationshipType"
    } else if call.name().eq_ignore_ascii_case("propertyKeys") {
        "propertyKey"
    } else {
        return Err(refusal("unknown db procedure"));
    };
    if !is_catalog(call)
        || !arguments.is_empty()
        || !call.argument_names().is_empty()
        || call.graph_projection().is_some()
    {
        return Err(refusal(
            "catalog procedures take no arguments or graph projection",
        ));
    }
    if call.outputs().is_empty()
        || call
            .outputs()
            .iter()
            .any(|name| !name.eq_ignore_ascii_case(output))
    {
        return Err(refusal("unknown catalog output column"));
    }
    if !call.vertex_outputs().is_empty() {
        return Err(refusal("catalog output columns are text, not vertices"));
    }
    let symbols = call
        .catalog_symbols()
        .ok_or_else(|| refusal("a complete reverse symbol catalog is required"))?;
    let mut usage = AdmissionUsage::default();
    usage.observe(policy, SourceEvent::Work)?;
    charge_work(1).map_err(GqlQueryError::Interrupted)?;
    let mut rows = Vec::new();
    for (symbol, name) in symbols {
        poll().map_err(GqlQueryError::Interrupted)?;
        if !matches!(
            (output, symbol),
            ("label", GraphSymbol::Label(_))
                | ("relationshipType", GraphSymbol::Relation(_))
                | ("propertyKey", GraphSymbol::Property(_))
        ) {
            return Err(refusal("catalog symbol has the wrong identity domain"));
        }
        if !visible(*symbol) {
            continue;
        }
        policy
            .rows
            .check(GqlBudgetDimension::ResultRows, rows.len() as u64 + 1)
            .map_err(GqlQueryError::Rows)?;
        usage.observe(policy, SourceEvent::ScratchEntry)?;
        charge_work(1).map_err(GqlQueryError::Interrupted)?;
        let mut values = Vec::new();
        for _ in call.outputs() {
            // Both the cell and its copied UTF-8 bytes are admitted before
            // allocation; a repeated YIELD alias pays for each actual copy.
            usage.observe(policy, SourceEvent::ScratchEntry)?;
            charge_work(1).map_err(GqlQueryError::Interrupted)?;
            for _ in 0..name.len().div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES) {
                poll().map_err(GqlQueryError::Interrupted)?;
                usage.observe(policy, SourceEvent::ScratchEntry)?;
                charge_work(1).map_err(GqlQueryError::Interrupted)?;
            }
            values.push(GraphValue::Scalar(
                CanonicalScalar::ucs_basic_text(name)
                    .map_err(|_| refusal("catalog name is not representable as text"))?,
            ));
        }
        rows.push(GraphValueRow::from_owned_values(values));
    }
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

pub(crate) fn privileged(
    call: &PreparedProcedureCall,
    arguments: &[GraphValue],
    policy: GqlQueryPolicy,
    cx: &QueryCx,
) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GqlError, super::Cancel>> {
    catalog_procedure(
        call,
        arguments,
        policy,
        |_| true,
        || cx.checkpoint(),
        |_| Ok(()),
    )
}
