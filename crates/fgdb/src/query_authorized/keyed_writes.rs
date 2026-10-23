//! Equality-bound write selections over one or two independent vertex roots.
//!
//! A frozen MERGE batch must not rescan every visible vertex for every key.
//! This source keeps the normal masked table and GLA evaluator, admitting the
//! union of the roots' candidate domains. Every pruning predicate must name
//! a field the capability can see. Hidden/history candidates are poll-only;
//! only admitted current winners reach signed and native source counters.

use super::{
    CommitSeq, Fault, GlaOutput, GlaPlan, PlannerPredicates, Snapshot, SourceEvent, Tables,
    VertexPredicate, source,
};
use fgdb_gql::algebra::{GlaOperator, IntegerComparison, ValueProjection};
use fgdb_types::CanonicalScalar;

/// Recognize the complete shape before touching a source. Independent scans,
/// total label/equality filters and identity output have no fallible expression
/// or outer correlation to move across an empty product. A slot's full filter
/// occurs once, before output; every slot has a seekable equality. Other plans
/// keep their existing source, including predicates on masked properties.
fn domains<'p, Row>(
    plan: &'p GlaPlan<Row>,
    scope: &PlannerPredicates,
) -> Option<Vec<&'p [VertexPredicate]>> {
    let mut domains: Vec<Option<&[VertexPredicate]>> = Vec::new();
    let mut projected = false;
    for operator in plan.operators() {
        match operator {
            GlaOperator::ScanVertices if !projected && domains.len() < 2 => domains.push(None),
            GlaOperator::Select { slot, predicates } if !projected => {
                let domain = domains.get_mut(slot.ordinal() as usize)?;
                if domain.is_some() {
                    return None;
                }
                let mut equality = false;
                for predicate in predicates {
                    match predicate {
                        VertexPredicate::HasLabel(label) if scope.allows_label(*label) => {}
                        VertexPredicate::IntegerProperty {
                            key,
                            comparison: IntegerComparison::Equal,
                            ..
                        } if scope.allows_property(*key) => equality = true,
                        VertexPredicate::ScalarProperty { key, predicate }
                            if scope.allows_property(*key)
                                && predicate.comparison() == IntegerComparison::Equal
                                && !matches!(predicate.value(), CanonicalScalar::Null) =>
                        {
                            equality = true;
                        }
                        _ => return None,
                    }
                }
                if !equality {
                    return None;
                }
                *domain = Some(predicates);
            }
            GlaOperator::Project { slot }
                if !projected && (slot.ordinal() as usize) < domains.len() =>
            {
                projected = true;
            }
            GlaOperator::ProjectValues { columns }
                if !projected
                    && columns.iter().all(|column| {
                        matches!(column, ValueProjection::Vertex { slot }
                            if (slot.ordinal() as usize) < domains.len())
                    }) =>
            {
                projected = true;
            }
            GlaOperator::Distinct
            | GlaOperator::OrderByVertexId
            | GlaOperator::OrderByValues
            | GlaOperator::Limit { .. }
                if projected => {}
            _ => return None,
        }
    }
    if !projected || domains.is_empty() {
        return None;
    }
    domains.into_iter().collect()
}

#[allow(clippy::too_many_arguments)]
pub(super) fn admit<'a, Row: GlaOutput>(
    snapshot: &'a Snapshot,
    plan: &GlaPlan<Row>,
    at: CommitSeq,
    scope: &PlannerPredicates,
    node: &mut impl FnMut() -> Result<(), Fault>,
    scan: &mut impl FnMut(SourceEvent) -> Result<(), Fault>,
    control: &mut impl FnMut(SourceEvent) -> Result<(), Fault>,
) -> Result<Option<Tables<'a>>, Fault> {
    let Some(domains) = domains(plan, scope) else {
        return Ok(None);
    };
    let mut vertices = Vec::new();
    for predicates in domains {
        // The existing source handles exact Int/Float equality and resolves
        // the winning historical row before rechecking every predicate.
        let Some(found) = source::indexed_roots(snapshot, predicates, at, scan)? else {
            return Ok(None);
        };
        vertices.extend(found);
    }
    scan(SourceEvent::Work)?;
    vertices.sort_unstable();
    vertices.dedup();
    let mut tables = Tables::new();
    tables.admit_found(snapshot, &vertices, at, scope, node, scan, control)?;
    tables.seal();
    tables.metadata(plan, scope, control)?;
    Ok(Some(tables))
}
