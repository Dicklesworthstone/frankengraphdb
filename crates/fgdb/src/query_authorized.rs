//! Warden-scoped reads of the embedded database's authenticated generation.
//!
//! Only admitted vertices/edges reach GLA. User predicates see masked labels
//! and properties, independently of their Boolean shape. Historical winners
//! are selected BEFORE authorization; a hidden successor cannot revive an old
//! visible version. No alternate query evaluator or synthetic snapshot exists.
//!
//! This is a resident-source boundary. Existing authenticated history merges
//! may inspect mixed-scope blocks, and their work is still charged. It does not
//! establish descriptor-I/O, timing or resource-failure noninterference, nor
//! install authorization on the privileged, ordinary Database APIs. The host
//! must not expose those APIs, its Authority, or its clock to token holders.

use super::{Cancel, QueryError};
use crate::gql_exec::{AdmissionUsage, source::{self, SourceEvent}};
use crate::{Database, GqlError, ReadError, Snapshot, VertexRow};
use asupersync::fs::Vfs;
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GlaOutput, GlaPlan, GraphValue, PreparedGraphPattern, VertexPredicate};
use fgdb_gql::{GqlQueryError, GqlQueryExecution, GqlQueryPolicy};
use fgdb_types::{CanonicalScalar, CommitSeq, EId, QueryCx, VId};
use fgdb_warden::{Authority, CapabilityToken, ExecutionPermit, PlannerPredicates, ReadAccess};
use std::cell::RefCell;
use std::collections::BTreeMap;

type Fault = GqlQueryError<ReadError, QueryError>;
type Governed<T> = Result<GqlQueryExecution<T>, Fault>;
type Edge<'a> = ((EId, VId, RelationId, VId), &'a [(PropertyKeyId, CanonicalScalar)]);

// The sole live permit spans source admission, evaluation and final delivery.
// RefCell permits sequential callbacks to share it, never concurrent borrowing
// or independent per-operand allowance resets. Neither it nor its data escapes.
struct Execution<'cx, 'permit, Clock> {
    cx: &'cx QueryCx,
    permit: ExecutionPermit<'permit, ReadAccess>,
    clock: Clock,
}
impl<Clock: FnMut() -> u64> Execution<'_, '_, Clock> {
    fn checkpoint(&mut self) -> Result<(), QueryError> {
        self.cx.checkpoint().map_err(interrupted)?;
        self.permit.charge_work_at((self.clock)(), 1).map_err(QueryError::Authorization)
    }
    fn node(&mut self) -> Result<(), QueryError> {
        self.checkpoint()?;
        self.permit.charge_nodes_at((self.clock)(), 1).map_err(QueryError::Authorization)
    }
    fn deliver(&mut self, rows: usize) -> Result<(), QueryError> {
        self.checkpoint()?;
        let rows = u64::try_from(rows).map_err(|_| QueryError::Authorization(fgdb_warden::Error::TooLarge))?;
        self.permit.charge_rows_at((self.clock)(), rows).map_err(QueryError::Authorization)?;
        self.checkpoint()
    }
}
fn interrupted(error: Cancel) -> QueryError {
    QueryError::Pattern(GqlQueryError::Interrupted(error))
}

// Keep original source/budget/cancellation classes. The private interruption
// carrier also transports a live Warden refusal without flattening its cause.
fn query_error(error: GqlQueryError<ReadError, QueryError>) -> QueryError {
    match error {
        GqlQueryError::Interrupted(error) => error,
        GqlQueryError::Source(error) => QueryError::Pattern(GqlQueryError::Source(GqlError::Read(error))),
        GqlQueryError::Rows(error) => QueryError::Pattern(GqlQueryError::Rows(error)),
        GqlQueryError::Evaluator(error) => QueryError::Pattern(GqlQueryError::Evaluator(error)),
        GqlQueryError::IdentifiedEdgesRequired => QueryError::Pattern(GqlQueryError::IdentifiedEdgesRequired),
    }
}

#[allow(clippy::too_many_arguments)]
fn authorized<V: Vfs + Clone, Row, Clock: FnMut() -> u64>(
    database: &Database<V>,
    cx: &QueryCx,
    authority: &Authority,
    token: &CapabilityToken,
    branch: &str,
    as_of: Option<CommitSeq>,
    mut clock: Clock,
    evaluate: impl FnOnce(
        &Snapshot,
        CommitSeq,
        &PlannerPredicates,
        &RefCell<Execution<'_, '_, Clock>>,
    ) -> Result<Vec<Row>, QueryError>,
) -> Result<Vec<Row>, QueryError> {
    // The host picks its issuer, not the request. Reject a different database
    // namespace before reading a frontier, source, or catalog. Graph/catalog/
    // branch-name routing remains the host's trusted registry responsibility.
    if authority.namespace() != database.keys.namespace {
        return Err(QueryError::Authorization(fgdb_warden::Error::WrongAuthority));
    }
    let now = clock();
    let verified = authority.verify_at(token, branch, now).map_err(QueryError::Authorization)?;
    let permit = verified.begin_read_at(branch, now).map_err(QueryError::Authorization)?;
    let execution = RefCell::new(Execution { cx, permit, clock });
    execution.borrow_mut().checkpoint()?;
    database.ensure_readable().map_err(QueryError::Read)?;
    let at = as_of.unwrap_or(database.snapshot.frontier);
    database.snapshot.check_frontier(at).map_err(QueryError::Read)?;
    cx.with_restriction(|| {
        let rows = evaluate(&database.snapshot, at, verified.predicates(), &execution)?;
        // No result prefix, source rows or private statistics were released.
        // Empty outputs still recheck expiry, retirement and cancellation.
        execution.borrow_mut().deliver(rows.len())?;
        Ok(rows)
    })
}

struct Tables<'a> {
    vertices: BTreeMap<VId, &'a VertexRow>,
    edges: BTreeMap<EId, Edge<'a>>,
    labels: BTreeMap<VId, Vec<GraphValue>>,
    types: BTreeMap<EId, CanonicalScalar>,
    records: u64,
}
impl<'a> Tables<'a> {
    fn admit<Row: GlaOutput>(
        snapshot: &'a Snapshot,
        plan: &GlaPlan<Row>,
        at: CommitSeq,
        predicates: &PlannerPredicates,
        mut node: impl FnMut() -> Result<(), Fault>,
        control: &mut impl FnMut(SourceEvent) -> Result<(), Fault>,
    ) -> Result<Self, Fault> {
        let mut tables = Self {
            vertices: BTreeMap::new(), edges: BTreeMap::new(),
            labels: BTreeMap::new(), types: BTreeMap::new(), records: 0,
        };
        source::visit_vertices(&snapshot.patches, at, control, |row, control| {
            // Authorization examines original labels; WHERE/labels() will not.
            control(SourceEvent::Work)?;
            for _ in &row.labels { control(SourceEvent::Work)?; }
            if predicates.allows_vertex(&row.labels) {
                node()?;
                control(SourceEvent::SnapshotRecord)?;
                control(SourceEvent::ScratchEntry)?;
                tables.records += 1; // bounded by successful record admission
                tables.vertices.insert(row.vid, row);
            }
            Ok(())
        })?;
        if plan.reads_edges() {
            source::visit_edges_with_properties(snapshot, at, control, |edge, properties, control| {
                control(SourceEvent::Work)?;
                if predicates.allows_relation(edge.relation)
                    && tables.vertices.contains_key(&edge.src)
                    && tables.vertices.contains_key(&edge.dst)
                {
                    control(SourceEvent::SnapshotRecord)?;
                    control(SourceEvent::ScratchEntry)?;
                    tables.records += 1;
                    tables.edges.insert(edge.eid, ((edge.eid, edge.src, edge.relation, edge.dst), properties));
                }
                Ok(())
            })?;
        }
        // Resolve names only after both topology and metadata scopes apply.
        // A forbidden unmapped label/type cannot cause a data-dependent error.
        if plan.projects_labels() {
            for row in tables.vertices.values() {
                control(SourceEvent::Work)?;
                let mut labels = Vec::new();
                for &label in &row.labels {
                    control(SourceEvent::Work)?;
                    if !predicates.allows_label(label) { continue; }
                    let name = plan.reverse_catalog.as_deref()
                        .and_then(|catalog| catalog.labels.get(&label))
                        .ok_or_else(|| GqlQueryError::Source(ReadError::UnmappedLabel(label)))?;
                    let name = text(name, control).map_err(|error| match error {
                        Some(error) => error,
                        None => GqlQueryError::Source(ReadError::UnmappedLabel(label)),
                    })?;
                    labels.push(GraphValue::Scalar(name));
                }
                control(SourceEvent::ScratchEntry)?;
                tables.labels.insert(row.vid, labels);
            }
        }
        if plan.projects_types() {
            for (&eid, ((_, _, relation, _), _)) in &tables.edges {
                control(SourceEvent::Work)?;
                let name = plan.reverse_catalog.as_deref()
                    .and_then(|catalog| catalog.relations.get(relation))
                    .ok_or_else(|| GqlQueryError::Source(ReadError::UnmappedRelation(*relation)))?;
                let name = text(name, control).map_err(|error| match error {
                    Some(error) => error,
                    None => GqlQueryError::Source(ReadError::UnmappedRelation(*relation)),
                })?;
                control(SourceEvent::ScratchEntry)?;
                tables.types.insert(eid, name);
            }
        }
        Ok(tables)
    }

    fn matches(&self, vid: VId, required: &[VertexPredicate], scope: &PlannerPredicates) -> bool {
        self.vertices.get(&vid).is_some_and(|row| required.iter().all(|predicate| {
            predicate.matches_borrowed(
                row.labels.iter().copied().filter(|label| scope.allows_label(*label)),
                row.props.iter().filter(|(key, _)| scope.allows_property(*key)).map(|(key, value)| (*key, value)),
            )
        }))
    }
    fn property(&self, vid: VId, key: PropertyKeyId, scope: &PlannerPredicates) -> Option<&CanonicalScalar> {
        if !scope.allows_property(key) { return None; }
        let row = self.vertices.get(&vid)?;
        row.props.binary_search_by_key(&key, |(key, _)| *key).ok().map(|at| &row.props[at].1)
    }
    fn edge_property(&self, eid: EId, key: PropertyKeyId, scope: &PlannerPredicates) -> Option<&CanonicalScalar> {
        if !scope.allows_property(key) { return None; }
        let (_, row) = self.edges.get(&eid)?;
        row.binary_search_by_key(&key, |(key, _)| *key).ok().map(|at| &row[at].1)
    }
}

// Reserve each copied catalog scalar and payload before constructing it.
fn text(
    name: &str,
    control: &mut impl FnMut(SourceEvent) -> Result<(), Fault>,
) -> Result<CanonicalScalar, Option<Fault>> {
    control(SourceEvent::ScratchEntry).map_err(Some)?;
    for _ in 0..name.len().div_ceil(64) {
        control(SourceEvent::Work).map_err(Some)?;
        control(SourceEvent::ScratchEntry).map_err(Some)?;
    }
    CanonicalScalar::ucs_basic_text(name).map_err(|_| None)
}

fn pattern_at<Row: GlaOutput, Clock: FnMut() -> u64>(
    snapshot: &Snapshot,
    at: CommitSeq,
    pattern: &PreparedGraphPattern<Row>,
    scope: &PlannerPredicates,
    execution: &RefCell<Execution<'_, '_, Clock>>,
    policy: GqlQueryPolicy,
) -> Governed<Row> {
    let mut usage = AdmissionUsage::default();
    let tables = Tables::admit(
        snapshot, pattern.plan(), at, scope,
        || execution.borrow_mut().node().map_err(GqlQueryError::Interrupted),
        &mut |event| {
            execution.borrow_mut().checkpoint().map_err(GqlQueryError::Interrupted)?;
            usage.observe::<ReadError, QueryError>(policy, event)
        },
    )?;
    let result = pattern.plan().execute_governed_with_element_accessors(
        tables.records,
        tables.vertices.keys().copied(),
        tables.edges.values().map(|(edge, _)| *edge),
        |vid, required| Ok(tables.matches(vid, required, scope)),
        |vid, key| Ok(tables.property(vid, key, scope)),
        |eid, key| Ok(tables.edge_property(eid, key, scope)),
        |vid| Ok(tables.labels.get(&vid).map(Vec::as_slice)),
        |eid| Ok(tables.types.get(&eid)),
        usage.remaining(policy),
        || execution.borrow_mut().checkpoint(),
    );
    usage.finish(policy, result)
}

impl<V: Vfs + Clone> Database<V> {
    /// Execute a typed GQL pattern on the capability's visible induced graph.
    ///
    /// The trusted host selects its Authority and exact branch mapping; its
    /// clock must be monotone and in the issuer's millisecond epoch. Neither
    /// issuer selection nor clock/branch routing may come from token claims.
    /// The database automatically rejects a mismatched security namespace.
    /// Signature, rights and validity precede source access. Retirement and
    /// expiry are rechecked at governed source/evaluator/delivery boundaries.
    ///
    /// All original label clauses admit vertices conjunctively. Both endpoints
    /// and the relation must admit an edge BEFORE GLA sees it, including path
    /// transit vertices. User predicates/projections see only allowed labels
    /// and properties; forbidden properties behave as absent, not as values
    /// filtered from an already computed result. NULL/OPTIONAL/existence and
    /// bag semantics remain the ordinary GLA executor's responsibility.
    ///
    /// Nodes count admitted vertices, including repeated source admissions;
    /// work counts live source/evaluator/checkpoint boundaries; rows count only
    /// the final result. Both the supplied native policy and signed ceilings
    /// apply. No rows or private execution statistics escape before success.
    ///
    /// This does not authorize ordinary Database methods, bind a durable branch
    /// catalog, or provide storage-descriptor/timing noninterference. Sources
    /// remain resident and their authenticated mixed-scope history work is
    /// charged. Keep the raw database and issuer in the trusted host.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_graph_pattern_authorized<Row: GlaOutput>(
        &self, cx: &QueryCx, authority: &Authority, token: &CapabilityToken,
        branch: &str, pattern: &PreparedGraphPattern<Row>, policy: GqlQueryPolicy,
        clock: impl FnMut() -> u64,
    ) -> Result<Vec<Row>, QueryError> {
        authorized(self, cx, authority, token, branch, None, clock, |snapshot, at, scope, execution| {
            pattern_at(snapshot, at, pattern, scope, execution, policy).map(|value| value.value).map_err(query_error)
        })
    }

    /// Apply the same current capability to an exact historical cut of this
    /// database. Resolve each winning historical statement before masking it;
    /// never resurrect an older permitted row hidden by a forbidden successor.
    /// A future cut refuses after authentication rather than being clamped.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_graph_pattern_authorized_at<Row: GlaOutput>(
        &self, cx: &QueryCx, authority: &Authority, token: &CapabilityToken,
        branch: &str, pattern: &PreparedGraphPattern<Row>, as_of: CommitSeq,
        policy: GqlQueryPolicy, clock: impl FnMut() -> u64,
    ) -> Result<Vec<Row>, QueryError> {
        authorized(self, cx, authority, token, branch, Some(as_of), clock, |snapshot, at, scope, execution| {
            pattern_at(snapshot, at, pattern, scope, execution, policy).map(|value| value.value).map_err(query_error)
        })
    }
}
