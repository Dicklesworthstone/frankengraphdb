//! Compiled CALL pipelines over an exact-session prepared retrieval corpus.
//! The ordinary GLA owns expressions, ordering and sets; only its source changes.

use super::super::super::{AuthorizedPreparedRead, check_branch, selector_error};
use super::*;
use crate::gql_exec::property_maps::{self, MetadataRecord};
use crate::gql_exec::{AdmissionUsage, source};
use crate::query::authorized::relational::{aggregate_gql_error, seeded, set_error};
use crate::query::authorized::{Tables, WorkCheckpoint, execute_tables};
use crate::query::{EndpointFilter, HybridCallError, HybridSearch, ProcedureError, hybrid_refusal};
use crate::{EmbeddedReadView, GqlError, PreparedNativeRead, QueryResult};
use fgdb_beacon::GraphHybridQuery;
use fgdb_delta_types::ElementId;
use fgdb_gql::algebra::{GlaOperator, GraphValue, GraphValueRow, PreparedGraphPattern};
use fgdb_gql::{
    GlaExecutionStats, GqlExecutionStats, GqlParameters, GqlQueryExecution, GqlQueryPolicy,
    PreparedProcedureCall,
};
use fgdb_warden::PlannerPredicates;
use std::cell::Cell;
use std::collections::BTreeSet;

type Live<'cx, 'permit, 'clock> = super::super::Live<'cx, 'permit, 'clock>;
type CallResult = Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GqlError, QueryError>>;

fn source_refusal(message: &'static str) -> GqlError {
    GqlError::Procedure(ProcedureError::Search(HybridCallError::Combination(
        message,
    )))
}

/// Seek one returned identity without initializing a snapshot-wide property
/// index (or its later flat-row cache). Canonical patches are ordered by
/// (VId, creation), so one predecessor per patch suffices. The latest patch
/// wins a restatement of the same creation; apply visibility only AFTER that
/// winner is chosen, otherwise a retirement could resurrect an older row.
///
/// Physical history depth can include hidden records. Poll every comparison
/// without charging that hidden work (FG-INV-20); callers charge the visible
/// lookup and its metadata/expression work. This takes constant scratch and
/// O(patches * log(rows per patch)) comparisons, including on a cold snapshot.
fn returned_vertex_at<'a>(
    view: &'a EmbeddedReadView,
    id: VId,
    at: fgdb_types::CommitSeq,
    poll: &mut impl FnMut() -> Result<(), QueryError>,
) -> Result<Option<&'a crate::VertexRow>, QueryError> {
    poll()?;
    let mut winner: Option<&crate::VertexRow> = None;
    for patch in &view.snapshot.patches {
        poll()?;
        if patch.first().is_none_or(|row| row.vid > id)
            || patch.last().is_some_and(|row| row.vid < id)
        {
            continue;
        }
        let (mut low, mut high) = (0, patch.len());
        while low < high {
            poll()?;
            let middle = low + (high - low) / 2;
            let row = &patch[middle];
            if (row.vid, row.created_at) <= (id, at) {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        if let Some(position) = low.checked_sub(1) {
            let row = &patch[position];
            if row.vid == id && winner.is_none_or(|previous| row.created_at >= previous.created_at)
            {
                winner = Some(row);
            }
        }
    }
    Ok(winner.filter(|row| row.visible_at(at)))
}

/// Resolve only an already admitted identity at the prepared source's cut.
/// An edge's relation and BOTH winning endpoint rows must still be visible;
/// membership in a provenance set never substitutes for those predicates.
/// History/index probes poll without exposing hidden topology in the meter.
fn admitted_record_at<'a>(
    view: &'a EmbeddedReadView,
    at: fgdb_types::CommitSeq,
    scope: &PlannerPredicates,
    element: ElementId,
    poll: &mut impl FnMut() -> Result<(), QueryError>,
) -> Result<Option<MetadataRecord<'a>>, QueryError> {
    match element {
        ElementId::Vertex(id) => Ok(returned_vertex_at(view, id, at, poll)?
            .filter(|row| scope.allows_vertex(&row.labels))
            .map(MetadataRecord::Vertex)),
        ElementId::Edge(id) => {
            let Some((block, row)) = view.snapshot.adjacency_index().statement_at_controlled(
                &view.snapshot.blocks,
                id,
                at,
                &mut |_| poll(),
            )?
            else {
                return Ok(None);
            };
            let edge = &view.snapshot.blocks[block][row];
            if !scope.allows_relation(edge.relation) {
                return Ok(None);
            }
            for id in [edge.src, edge.dst] {
                if returned_vertex_at(view, id, at, poll)?
                    .is_none_or(|row| !scope.allows_vertex(&row.labels))
                {
                    return Ok(None);
                }
            }
            Ok(Some(MetadataRecord::Edge(
                edge.relation,
                source::edge_properties_at(&view.snapshot.block_props, block, row),
            )))
        }
    }
}

fn admitted_element(
    element: ElementId,
    retrieved: &RefCell<BTreeSet<VId>>,
    derived: &RefCell<BTreeSet<ElementId>>,
) -> bool {
    matches!(element, ElementId::Vertex(id) if retrieved.borrow().contains(&id))
        || derived.borrow().contains(&element)
}

/// The compiler marks CALL outputs correlated with the following identity
/// pattern. Its vertex roots must ALL be those outputs before a prepared
/// source can substitute returned hits for a graph scan. Consume this source
/// once: a later MATCH or set sibling cannot inherit a preceding CALL's scope.
struct IdentityRows {
    roots: usize,
    vertices: BTreeSet<VId>,
}

fn identity_at(
    index: &AuthorizedBeaconIndex,
    view: &EmbeddedReadView,
    scope: &PlannerPredicates,
    execution: &Live<'_, '_, '_>,
    pattern: &PreparedGraphPattern<GraphValueRow>,
    policy: GqlQueryPolicy,
    pending: &RefCell<Option<IdentityRows>>,
) -> CallResult {
    let source = pending.borrow_mut().take().ok_or_else(|| {
        GqlQueryError::Source(source_refusal(
            "prepared index patterns require the preceding CALL's returned vertices",
        ))
    })?;
    let mut roots = 0;
    for operator in pattern.plan().operators() {
        match operator {
            GlaOperator::ScanVertices => roots += 1,
            GlaOperator::Select { .. }
            | GlaOperator::VertexIdentity { .. }
            | GlaOperator::CompareProperties { .. }
            | GlaOperator::SelectBoolean { .. }
            | GlaOperator::ProjectValues { .. }
            | GlaOperator::Distinct
            | GlaOperator::OrderByValues
            | GlaOperator::OrderByValueColumns { .. }
            | GlaOperator::Limit { .. } => {}
            _ => {
                return Err(GqlQueryError::Source(source_refusal(
                    "prepared index patterns accept only returned-vertex identity reads",
                )));
            }
        }
    }
    if roots == 0 || roots != source.roots {
        return Err(GqlQueryError::Source(source_refusal(
            "prepared index patterns cannot introduce uncorrelated vertices",
        )));
    }

    (|| {
        let mut usage = AdmissionUsage::default();
        let mut control = |event| {
            execution
                .borrow_mut()
                .checkpoint()
                .map_err(GqlQueryError::Interrupted)?;
            usage.observe(policy, event)
        };
        let mut tables = Tables::new();
        for id in source.vertices {
            // Membership and the saved population were already admitted by
            // this CALL. These are borrowed property lookups over its private
            // hits, with no extra source-record or signed-node admission.
            control(source::SourceEvent::Work)?;
            let row = returned_vertex_at(view, id, index.sequence, &mut || {
                execution.borrow_mut().poll()
            })
            .map_err(GqlQueryError::Interrupted)?;
            if let Some(row) = row
                && scope.allows_vertex(&row.labels)
            {
                control(source::SourceEvent::ScratchEntry)?;
                // The source set is already in VId order, without duplicates.
                tables.vertices.push((id, row));
            }
        }
        tables.metadata(pattern.plan(), scope, &mut control)?;
        execute_tables(
            tables,
            pattern,
            scope,
            policy,
            usage,
            WorkCheckpoint(execution),
        )
    })()
    .map_err(|error| error.map_source(GqlError::Read))
}

/// The GLA supplies identity seeds only after proving an equality from the
/// leading bag to this pattern's single root. A prepared source additionally
/// requires every seed to be a hit of the immediately preceding CALL. The
/// ordinary scoped closure owner retains bag/path/edge-identity semantics;
/// only source admission narrows. A declined shape is a refusal, never an
/// invitation to scan the prepared snapshot with a fresh allowance.
#[allow(clippy::too_many_arguments)]
fn incidence_at(
    index: &AuthorizedBeaconIndex,
    view: &EmbeddedReadView,
    scope: &PlannerPredicates,
    execution: &Live<'_, '_, '_>,
    pattern: &PreparedGraphPattern<GraphValueRow>,
    seeds: fgdb_gql::GraphSeeds<'_>,
    policy: GqlQueryPolicy,
    pending: &RefCell<Option<IdentityRows>>,
    derived: &RefCell<BTreeSet<ElementId>>,
) -> Option<CallResult> {
    if !pattern.plan().operators().iter().any(|operator| {
        matches!(
            operator,
            GlaOperator::ScanEdges { .. }
                | GlaOperator::Expand { .. }
                | GlaOperator::VarLengthExpand { .. }
        )
    }) {
        // Keep the established flat identity path and its exact accounting.
        return None;
    }
    Some((|| {
        let source = pending.borrow_mut().take().ok_or_else(|| {
            GqlQueryError::Source(source_refusal(
                "prepared index incidence requires the preceding CALL's returned vertices",
            ))
        })?;
        let fgdb_gql::GraphSeeds::Identities(identities) = seeds else {
            return Err(GqlQueryError::Source(source_refusal(
                "prepared index incidence requires returned-vertex identity roots",
            )));
        };
        if source.roots != 1 {
            return Err(GqlQueryError::Source(source_refusal(
                "prepared index incidence requires one correlated CALL vertex root",
            )));
        }
        for id in identities {
            execution
                .borrow_mut()
                .poll()
                .map_err(GqlQueryError::Interrupted)?;
            if !source.vertices.contains(id) {
                return Err(GqlQueryError::Source(source_refusal(
                    "prepared index incidence roots must be returned by the preceding CALL",
                )));
            }
        }
        // The owner validates the complete shape even with zero seeds. It
        // admits both endpoints and the relation BEFORE charging incidence,
        // and records provenance inside that same cumulative source meter.
        seeded::pattern_at_with_provenance(
            &view.snapshot,
            index.sequence,
            pattern,
            fgdb_gql::GraphSeeds::Identities(identities),
            scope,
            execution,
            policy,
            Some(derived),
        )
        .ok_or_else(|| {
            GqlQueryError::Source(source_refusal(
                "prepared index incidence requires a bounded single-root pattern",
            ))
        })?
        .map_err(|error| error.map_source(GqlError::Read))
    })())
}

/// Capability discovery must not change flat identity queries' admission:
/// advertising seeded reads would itself make the GLA allocate/charge seeds
/// before the host could decline them. Inspect only compiled definitions.
fn has_incidence(query: &fgdb_gql::PreparedGraphSet) -> bool {
    let mut incidence = false;
    let result: Result<(), core::convert::Infallible> = query.admit_patterns(&mut |pattern| {
        incidence |= pattern.plan().operators().iter().any(|operator| {
            matches!(
                operator,
                GlaOperator::ScanEdges { .. }
                    | GlaOperator::Expand { .. }
                    | GlaOperator::VarLengthExpand { .. }
            )
        });
        Ok(())
    });
    match result {
        Ok(()) => incidence,
        Err(never) => match never {},
    }
}

/// This profile owns only pattern and materialized CALL sources; the outer
/// adapters supply governed element reads. Keep the seeded capability absent
/// on statements which already used the flat identity owner.
struct PreparedSource<S, F> {
    source: S,
    seeded: F,
    incidence: bool,
}

impl<S, F> fgdb_gql::GraphSetSource<GqlError, QueryError> for PreparedSource<S, F>
where
    S: fgdb_gql::GraphSetSource<GqlError, QueryError>,
    F: FnMut(
        &PreparedGraphPattern<GraphValueRow>,
        fgdb_gql::GraphSeeds<'_>,
        GqlQueryPolicy,
    ) -> Option<CallResult>,
{
    fn pattern(
        &mut self,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        policy: GqlQueryPolicy,
    ) -> CallResult {
        self.source.pattern(pattern, policy)
    }

    fn procedure(
        &mut self,
        call: &PreparedProcedureCall,
        arguments: &[GraphValue],
        policy: GqlQueryPolicy,
    ) -> Option<CallResult> {
        self.source.procedure(call, arguments, policy)
    }

    fn supports_seeded_patterns(&self) -> bool {
        self.incidence
    }

    fn seeded_pattern(
        &mut self,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        seeds: fgdb_gql::GraphSeeds<'_>,
        policy: GqlQueryPolicy,
    ) -> Option<CallResult> {
        (self.seeded)(pattern, seeds, policy)
    }
}

// Equality is about the logical corpus and numeric projection, not build knobs.
// An exact call may use a link-bearing generation; ANN still uses its frozen
// topology. An unused lane neither constrains nor revalidates its projection.
fn matching_definition(
    frozen: &Options,
    requested: &Options,
    query: GraphHybridQuery<'_>,
) -> Result<(), BeaconError> {
    if frozen.vertex_label != requested.vertex_label {
        return Err(BeaconError::InvalidQuery(
            "prepared index label selection must match the CALL",
        ));
    }
    let (vector, text) = Search::Hybrid(query.source_query()).lanes();
    if text {
        if frozen.index.text.is_none() {
            return Err(BeaconError::Disabled("text"));
        }
        if frozen.projection.text != requested.projection.text {
            return Err(BeaconError::InvalidQuery(
                "prepared index text property must match the CALL",
            ));
        }
    }
    if vector {
        let stored = frozen
            .index
            .vector
            .as_ref()
            .ok_or(BeaconError::Disabled("vector"))?;
        let requested_vector = requested
            .index
            .vector
            .as_ref()
            .ok_or(BeaconError::Invariant(
                "bound vector CALL has no vector definition",
            ))?;
        if frozen.projection.vector != requested.projection.vector
            || frozen.projection.encoding != requested.projection.encoding
            || stored.dimensions != requested_vector.dimensions
            || stored.metric != requested_vector.metric
        {
            return Err(BeaconError::InvalidQuery(
                "prepared index vector projection and metric must match the CALL",
            ));
        }
    }
    Ok(())
}

impl<R: GraphSymbolResolver, C: FnMut() -> u64> AuthorizedReadSession<'_, R, C> {
    /// Execute a compiled CALL/YIELD/WITH/RETURN pipeline using an existing
    /// authorized text/vector index, without rescanning or rebuilding its corpus.
    /// Obtain `prepared` with this session's ordinary `prepare` method and the
    /// index with `prepare_beacon_index`. Both handles must belong to THIS session.
    /// Parameters rebind without resolving catalog symbols again.
    ///
    /// Each hybrid.search CALL must name the frozen label, active properties,
    /// vector encoding and metric. A mismatch refuses rather than changing the
    /// corpus. The frozen text scoring configuration and ANN topology remain
    /// authoritative. Result eligibility applies before candidate truncation.
    /// Every CALL spends the same statement's remaining native allowance and
    /// live signed permit; no set operand can reset them. Only final statement
    /// rows spend signed delivery, so intermediate search k is not a row charge.
    ///
    /// All property reads use the index's source_sequence, even when it predates
    /// the session frontier. Properties remain scope-masked. Only CALL hits and
    /// elements admitted by their correlated MATCH closure may be dereferenced:
    /// caller-supplied identities cannot turn an index handle into a raw reader.
    ///
    /// This explicit source profile accepts compiled row and terminal aggregate
    /// pipelines over hybrid.search, including graph-only and mixed expansion.
    /// Graph membership and incidence use the typed prepared graph reader at
    /// the SAME saved sequence. Propertyless vertices remain traversable;
    /// rejecting an endpoint does not remove it from paths. The retrieval
    /// corpus and text/vector indexes are reused; correlated MATCH uses the
    /// pinned snapshot's ordinary graph lookup indexes. Each CALL reports the
    /// saved population plus admitted incidence visits to the enclosing source
    /// allowance; MATCH reports its admitted closure on that same allowance.
    /// New visits, incidences and bounded ranking workspace are scratch-admitted
    /// before retention; these are logical counts, not a total byte-memory cap.
    ///
    /// Terminal COUNT/SUM/AVG, DISTINCT, extrema, collections, grouping, HAVING
    /// and output clauses use the existing native aggregate owner. Counts and
    /// exact averages keep their aggregate domains, not narrowed scalar cells.
    /// Private CALL hits and aggregate inputs spend source/work/scratch limits,
    /// not the final signed row allowance. The frozen native source-result cap
    /// still applies to every CALL; final group rows use normal session delivery.
    /// A failed property read or aggregate input is not hidden by LIMIT 0.
    ///
    /// CALL property/label reads and flat identity MATCHes use only that
    /// CALL's returned vertices. A bounded incidence MATCH may expand one of
    /// those identity roots through the ordinary authorized closure owner;
    /// reached vertices and edges retain masked properties and metadata at the
    /// same saved cut. The compiler's ordinary correlated relation, bags, edge
    /// identities and path semantics remain authoritative. Uncorrelated roots,
    /// other procedures and temporal wrappers refuse; there is no one-shot
    /// fallback. A nonnull graph_access
    /// override refuses: this explicit prepared source profile owns its native
    /// incidence path, rather than silently substituting a requested physical
    /// source. The normal execute method and its source choices are unchanged.
    ///
    /// Expiry, retirement and backwards time close the session, including on
    /// empty results or late errors. Other query/resource failures allow retry.
    /// This is resident prepared execution, not a durable/advancing index, spill,
    /// exhaustive hybrid answer certificate or physical timing-isolation claim.
    pub fn execute_beacon_index(
        &mut self,
        cx: &QueryCx,
        index: &AuthorizedBeaconIndex,
        prepared: &AuthorizedPreparedRead,
        params: &GqlParameters,
    ) -> Result<QueryResult, QueryError> {
        let owner = Arc::clone(&self.owner);
        self.run(cx, |view, branch, scope, _, policy, execution| {
            // run performs current credential admission before either handle
            // is inspected, including when a query would return zero rows.
            if !Arc::ptr_eq(&owner, &prepared.owner) {
                return Err(QueryError::Authorization(WardenError::WrongAuthority));
            }
            if !Arc::ptr_eq(&owner, &index.owner) {
                return Err(QueryError::Authorization(WardenError::ScopeDenied));
            }
            let selected = prepared
                .selector
                .bind_parameters(params)
                .map_err(selector_error)?;
            check_branch(&selected, branch)?;
            let params = prepared.parameters(selected.parameters())?;
            view.snapshot
                .check_frontier(index.sequence)
                .map_err(QueryError::Read)?;
            // This is a statement-local authorization provenance set, not an
            // eligibility predicate or a second source. Admit storage in call_at
            // before insertion, and discard it with the enclosing execution.
            let retrieved = RefCell::new(BTreeSet::new());
            let identity = RefCell::new(None);
            let derived = RefCell::new(BTreeSet::new());
            // The element callback's narrow source-error type cannot carry a
            // live QueryError. Retain the first interruption and return it
            // through the evaluator's control boundary or immediately after
            // execution, preserving its original class and stopped prefix.
            let element_failure = RefCell::new(None);
            let mut procedures = |call: &PreparedProcedureCall,
                                  arguments: &[GraphValue],
                                  remaining: GqlQueryPolicy| {
                // A new CALL cannot accidentally reuse its predecessor's
                // identity source, including when it produces no hits.
                *identity.borrow_mut() = None;
                call_at(
                    cx, &owner, index, view, scope, execution, call, arguments, remaining,
                    &retrieved, &identity,
                )
            };
            let mut elements = |element, key| {
                if !scope.allows_property(key) {
                    return Ok(None);
                }
                if !admitted_element(element, &retrieved, &derived) {
                    return Err(source_refusal(
                        "prepared index property reads require a retrieved or matched element",
                    ));
                }
                let record = admitted_record_at(view, index.sequence, scope, element, &mut || {
                    execution.borrow_mut().poll()
                })
                .map_err(|error| {
                    element_failure.borrow_mut().get_or_insert(error);
                    source_refusal("prepared index property read interrupted")
                })?;
                Ok(record.and_then(|record| {
                    let properties = match record {
                        MetadataRecord::Vertex(row) => row.props.as_slice(),
                        MetadataRecord::Edge(_, properties) => properties,
                    };
                    properties
                        .binary_search_by_key(&key, |(key, _)| *key)
                        .ok()
                        .map(|position| properties[position].1.clone())
                }))
            };
            let mut metadata = |element,
                                kind,
                                catalog: &fgdb_gql::ReverseSymbolCatalog,
                                control: &mut dyn FnMut(
                fgdb_gql::GlaExecutionEvent,
            ) -> Result<(), ()>| {
                use fgdb_gql::GraphElementMetadataError;
                if !admitted_element(element, &retrieved, &derived) {
                    return Err(GraphElementMetadataError::Source(source_refusal(
                        "prepared index metadata reads require a retrieved or matched element",
                    )));
                }
                let record = admitted_record_at(view, index.sequence, scope, element, &mut || {
                    execution.borrow_mut().poll()
                })
                .map_err(|error| {
                    element_failure.borrow_mut().get_or_insert(error);
                    GraphElementMetadataError::Source(source_refusal(
                        "prepared index metadata read interrupted",
                    ))
                })?;
                let Some(record) = record else {
                    return Ok(None);
                };
                property_maps::metadata(
                    record,
                    kind,
                    catalog,
                    |label| scope.allows_label(label),
                    |key| scope.allows_property(key),
                    &mut |event| {
                        control(match event {
                            crate::gql_exec::source::SourceEvent::Work
                            | crate::gql_exec::source::SourceEvent::SnapshotRecord => {
                                fgdb_gql::GlaExecutionEvent::Work
                            }
                            crate::gql_exec::source::SourceEvent::ScratchEntry => {
                                fgdb_gql::GlaExecutionEvent::ScratchEntry
                            }
                        })
                        .map_err(|()| GraphElementMetadataError::Interrupted)
                    },
                    &|error| GraphElementMetadataError::Source(GqlError::Read(error)),
                )
                .map(Some)
            };
            let mut checkpoint = || {
                if let Some(error) = element_failure.borrow_mut().take() {
                    return Err(error);
                }
                execution.borrow_mut().checkpoint()
            };
            let result = (|| match prepared.native.as_ref() {
                PreparedNativeRead::Set(template) => {
                    let bound = template
                        .bind_parameters(&params)
                        .map_err(QueryError::SetText)?;
                    let result = bound
                        .execute_governed_from_source(
                            policy,
                            fgdb_gql::with_element_metadata(
                                fgdb_gql::WithElementReads {
                                    source: PreparedSource {
                                        incidence: has_incidence(&bound),
                                        source: fgdb_gql::WithProcedures {
                                            patterns:
                                                |pattern: &PreparedGraphPattern<GraphValueRow>,
                                                 remaining| {
                                                    identity_at(
                                                        index, view, scope, execution, pattern,
                                                        remaining, &identity,
                                                    )
                                                },
                                            procedures: &mut procedures,
                                        },
                                        seeded: |pattern: &PreparedGraphPattern<GraphValueRow>,
                                                 seeds: fgdb_gql::GraphSeeds<'_>,
                                                 remaining| {
                                            incidence_at(
                                                index, view, scope, execution, pattern, seeds,
                                                remaining, &identity, &derived,
                                            )
                                        },
                                    },
                                    elements: &mut elements,
                                },
                                &mut metadata,
                            ),
                            &mut checkpoint,
                        )
                        .map_err(set_error)?;
                    let count = result.value.len();
                    Ok((
                        crate::query::values(template.columns().to_vec(), result.value),
                        count,
                    ))
                }
                PreparedNativeRead::PipelineAggregate(template) => {
                    // Bind the complete relational input, never a fabricated
                    // first graph leaf. Both physical aggregate profiles keep
                    // the same source/element owner and the original allowance.
                    let bound = template
                        .bind_relation_parameters(&params)
                        .map_err(QueryError::PipelineText)?;
                    let result = bound
                        .execute_governed_from_source(
                            policy,
                            fgdb_gql::with_element_metadata(
                                fgdb_gql::WithElementReads {
                                    source: PreparedSource {
                                        incidence: has_incidence(bound.input()),
                                        source: fgdb_gql::WithProcedures {
                                            patterns:
                                                |pattern: &PreparedGraphPattern<GraphValueRow>,
                                                 remaining| {
                                                    identity_at(
                                                        index, view, scope, execution, pattern,
                                                        remaining, &identity,
                                                    )
                                                },
                                            procedures: &mut procedures,
                                        },
                                        seeded: |pattern: &PreparedGraphPattern<GraphValueRow>,
                                                 seeds: fgdb_gql::GraphSeeds<'_>,
                                                 remaining| {
                                            incidence_at(
                                                index, view, scope, execution, pattern, seeds,
                                                remaining, &identity, &derived,
                                            )
                                        },
                                    },
                                    elements: &mut elements,
                                },
                                &mut metadata,
                            ),
                            &mut checkpoint,
                        )
                        .map_err(aggregate_gql_error)?;
                    let count = result.value.len();
                    Ok((
                        crate::query::aggregates(
                            template.columns().to_vec(),
                            template.output_slots(),
                            result.value,
                        ),
                        count,
                    ))
                }
                _ => Err(QueryError::Unsupported {
                    diagnostics: vec![
                        "prepared index execution requires a row or aggregate CALL pipeline"
                            .to_owned(),
                    ],
                }),
            })();
            let failure = element_failure.borrow_mut().take();
            match failure {
                Some(error) => Err(error),
                None => result,
            }
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn call_at(
    cx: &QueryCx,
    owner: &Arc<()>,
    index: &AuthorizedBeaconIndex,
    view: &EmbeddedReadView,
    scope: &PlannerPredicates,
    execution: &Live<'_, '_, '_>,
    call: &PreparedProcedureCall,
    arguments: &[GraphValue],
    remaining: GqlQueryPolicy,
    retrieved: &RefCell<BTreeSet<VId>>,
    identity: &RefCell<Option<IdentityRows>>,
) -> CallResult {
    if !crate::query::is_hybrid(call) {
        return Err(hybrid_refusal(HybridCallError::Combination(
            "prepared index execution accepts only hybrid.search sources",
        )));
    }
    if call
        .argument_names()
        .iter()
        .zip(arguments)
        .any(|(name, value)| {
            name == "graph_access"
                && !matches!(value, GraphValue::Scalar(fgdb_types::CanonicalScalar::Null))
        })
    {
        return Err(hybrid_refusal(HybridCallError::Combination(
            "prepared index execution owns its graph source; omit graph_access",
        )));
    }
    let (search, eligible) =
        HybridSearch::bind_snapshot_filtered(call, arguments, index.sequence, remaining)
            .map_err(hybrid_refusal)?;
    let query = search.query();
    matching_definition(&index.definition, search.options(), query)
        .map_err(|error| hybrid_refusal(HybridCallError::Index(error)))?;
    admit_prepared(
        owner,
        index,
        view,
        scope,
        remaining,
        execution,
        // The procedure's k is private input to the compiled row/group stages,
        // not a delivered page. Admit owner, history and the saved population
        // on this same permit, with zero delivered rows. Actual source k still
        // meets candidate_depths and the frozen native ceiling below; the final
        // statement result alone is charged by the enclosing session gate.
        Search::Hybrid(query.source_query()),
    )
    .map_err(GqlQueryError::Interrupted)?;
    let allowance = prepared_policy(index, search.options().policy, remaining);
    let spent = Cell::new(0_u64);
    let incidences = Cell::new(0_u64);
    let work = RefCell::new(Meter::new(allowance.max_work_units, |units| {
        charge(execution, units)?;
        spent.set(spent.get().saturating_add(units as u64));
        Ok(())
    }));
    let result = (|| {
        work.borrow_mut().charge(1)?;
        query.candidate_depths()?;
        if query.retrieval.k > allowance.max_result_rows {
            return Err(BeaconError::ResourceLimit {
                resource: "result rows",
                limit: allowance.max_result_rows,
            });
        }
        if !query.is_graph_only() {
            Search::Hybrid(query.source_query())
                .validate(&index.definition.index, &mut SharedWork(&work))?;
        }
        let filter = EndpointFilter::prepare(
            eligible,
            query.retrieval.k,
            remaining.evaluator.max_scratch_entries,
            &mut SharedWork(&work),
        )?;
        let graph_scratch = Cell::new(filter.scratch_entries());
        let admit_scratch = |entries: usize| {
            let next = u64::try_from(entries)
                .ok()
                .and_then(|n| graph_scratch.get().checked_add(n))
                .filter(|&n| n <= remaining.evaluator.max_scratch_entries)
                .ok_or(BeaconError::ResourceLimit {
                    resource: "prepared CALL graph scratch entries",
                    limit: usize::try_from(remaining.evaluator.max_scratch_entries)
                        .unwrap_or(usize::MAX),
                })?;
            graph_scratch.set(next);
            Ok(())
        };
        // The bool eligibility ABI retains its first refusal across BOTH
        // expansion and fusion. No intermediate result or fresh permit escapes.
        let hits = filter.finish((|| {
            let graph = if query.graph_enabled() {
                let (hits, visits) = super::graph::expand_at(
                    cx,
                    index,
                    view,
                    scope,
                    remaining,
                    execution,
                    search.expansion(),
                    query.graph_candidates,
                    allowance,
                    &work,
                    |id, _| Ok(filter.admits(id, &work)),
                    &admit_scratch,
                )?;
                incidences.set(visits);
                hits
            } else {
                Vec::new()
            };
            let mut kernel = super::graph::ScratchWork {
                work: &work,
                admit: &admit_scratch,
            };
            if query.is_graph_only() {
                query.fuse_graph_only(&graph, |id| filter.admits(id, &work), &mut kernel)
            } else {
                index.index.hybrid_search_graph_filtered(
                    query,
                    &graph,
                    |id| filter.admits(id, &work),
                    &mut kernel,
                )
            }
        })())?;
        // Reserve a row, all YIELD cells and a provenance-set entry per hit
        // before allocating output. Repeated identities conservatively pay
        // again, so warming the private set cannot widen the query allowance.
        let identity_roots = call.vertex_outputs().len();
        let mut identity_rows = (identity_roots != 0).then(|| IdentityRows {
            roots: identity_roots,
            vertices: BTreeSet::new(),
        });
        let scratch = (call.outputs().len() as u128 + 2 + u128::from(identity_roots != 0))
            .checked_mul(hits.len() as u128)
            .and_then(|count| count.checked_add(u128::from(graph_scratch.get())))
            .filter(|&count| count <= u128::from(remaining.evaluator.max_scratch_entries))
            .ok_or(BeaconError::ResourceLimit {
                resource: "prepared CALL scratch entries",
                limit: usize::try_from(remaining.evaluator.max_scratch_entries)
                    .unwrap_or(usize::MAX),
            })?;
        for hit in &hits {
            let entries = retrieved.borrow().len();
            work.borrow_mut()
                .charge(1 + entries.checked_ilog2().unwrap_or(0) as usize)?;
            for _ in call.outputs() {
                work.borrow_mut().charge(1)?;
            }
            retrieved.borrow_mut().insert(hit.id);
            if let Some(rows) = &mut identity_rows {
                work.borrow_mut()
                    .charge(1 + rows.vertices.len().checked_ilog2().unwrap_or(0) as usize)?;
                rows.vertices.insert(hit.id);
            }
        }
        work.borrow_mut().charge(1)?;
        Ok((search.rows(hits), scratch as u64, identity_rows))
    })();
    let (value, scratch_entries, identity_rows) = settle(work, result)
        .map_err(GqlQueryError::Interrupted)?
        .map_err(|error| hybrid_refusal(HybridCallError::Index(error)))?;
    *identity.borrow_mut() = identity_rows;
    Ok(GqlQueryExecution {
        rows: GqlExecutionStats {
            // expand_at checks this same sum at each admitted incidence, before
            // visiting it. Return it too, so UNION/CALL siblings cannot reset it.
            snapshot_records: index
                .admitted
                .checked_add(incidences.get())
                .ok_or_else(|| {
                    hybrid_refusal(HybridCallError::Index(BeaconError::Invariant(
                        "prepared source-record sum overflowed after admission",
                    )))
                })?,
            result_rows: value.len() as u64,
        },
        evaluator: GlaExecutionStats {
            work_units: spent.get(),
            scratch_entries,
        },
        value,
    })
}

#[cfg(test)]
#[path = "prepared_native_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "prepared_native_graph_tests.rs"]
mod graph_tests;

#[cfg(test)]
#[path = "prepared_native_aggregate_tests.rs"]
mod aggregate_tests;
