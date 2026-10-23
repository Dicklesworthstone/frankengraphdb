// Vertex MERGE has one source-parameterized collector. Native overlays and
// capability-masked sources share target reduction, creation and accounting.
// The output owner chooses singular or all-target cardinality, not a matcher.

fn merge_remaining_budget(
    original: fgdb_gql::GqlExecutionBudget,
    used: fgdb_gql::GqlExecutionStats,
) -> fgdb_gql::GqlExecutionBudget {
    let snapshot = original.max_snapshot_records().map(|limit| {
        limit
            .checked_sub(used.snapshot_records)
            .expect("successful match selection is within its snapshot limit")
    });
    let results = original.max_result_rows().map(|limit| {
        limit
            .checked_sub(used.result_rows)
            .expect("successful match selection is within its result limit")
    });
    match (snapshot, results) {
        (None, None) => fgdb_gql::GqlExecutionBudget::UNLIMITED,
        (Some(snapshot), None) => fgdb_gql::GqlExecutionBudget::snapshot_records(snapshot),
        (None, Some(results)) => fgdb_gql::GqlExecutionBudget::result_rows(results),
        (Some(snapshot), Some(results)) => fgdb_gql::GqlExecutionBudget::new(snapshot, results),
    }
}

/// One held vertex: its identity, labels and properties.
type FrozenVertex = (
    VId,
    Vec<LabelId>,
    Vec<(fgdb_delta_types::PropertyKeyId, CanonicalPropertyValue)>,
);

/// A write chain's creations while it runs frozen (fgdb-0s2pq). The chain
/// stages nothing until after its last step, so every step reads the overlay
/// as the chain found it. These are the chain's own creations, which a later
/// MERGE must also see, as it would see them staged in a sequential chain.
#[derive(Default)]
pub(crate) struct FrozenCreations {
    vertices: Vec<FrozenVertex>,
    // Candidate buckets: a superset of the equal values. The MERGE's own
    // predicates decide every match.
    buckets:
        std::collections::BTreeMap<(fgdb_delta_types::PropertyKeyId, FrozenValueClass), Vec<usize>>,
    /// Each held vertex's position in `vertices`.
    index: std::collections::BTreeMap<VId, usize>,
    /// Every creation batch, and every ON CREATE clause on a held vertex, in
    /// statement order, for the one ordered staging.
    batches: Vec<WriteBatch>,
}

/// The bucket of every value a scalar can equal: integers and floats by their
/// f64 image, all decimals together, each temporal kind together, and any
/// other scalar by its canonical bytes.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum FrozenValueClass {
    Number(u64),
    Decimal,
    Temporal(u8),
    Exact(Vec<u8>),
}

impl FrozenValueClass {
    fn of(value: &CanonicalScalar) -> Self {
        // -0.0 equals 0.0. NaN equals nothing, so any bucket serves it.
        let number =
            |value: f64| Self::Number(if value == 0.0 { 0.0_f64 } else { value }.to_bits());
        match value {
            CanonicalScalar::Int(value) => number(*value as f64),
            CanonicalScalar::Float(value) => number(value.get()),
            CanonicalScalar::Decimal(_) => Self::Decimal,
            CanonicalScalar::Timestamp(_) => Self::Temporal(0),
            CanonicalScalar::Date(_) => Self::Temporal(1),
            CanonicalScalar::LocalTime(_) => Self::Temporal(2),
            CanonicalScalar::Time(_) => Self::Temporal(3),
            CanonicalScalar::LocalDateTime(_) => Self::Temporal(4),
            CanonicalScalar::Duration(_) => Self::Temporal(5),
            other => Self::Exact(other.encode().unwrap_or_default()),
        }
    }
}

impl FrozenCreations {
    /// Hold one creation batch for the chain's ordered staging; its vertices
    /// join every later lookup.
    pub(crate) fn defer(&mut self, batch: WriteBatch) {
        for row in &batch.rows {
            match row {
                PendingRow::Vertex {
                    vid, labels, props, ..
                } => {
                    let at = self.vertices.len();
                    for (key, value) in props {
                        self.bucket(at, *key, value);
                    }
                    self.index.insert(*vid, at);
                    self.vertices.push((*vid, labels.clone(), props.clone()));
                }
                // An ON CREATE clause on a held vertex: later lookups see its
                // assignments, as they would see them staged. Its old bucket
                // entries stay; buckets are only a superset.
                PendingRow::SetProperty { vid, key, value } => {
                    if let Some(&at) = self.index.get(vid) {
                        self.vertices[at].2.retain(|(present, _)| present != key);
                        if let Some(value) = value {
                            self.vertices[at].2.push((*key, value.clone()));
                            self.bucket(at, *key, value);
                        }
                    }
                }
                PendingRow::SetLabel { vid, label, member } => {
                    if let Some(&at) = self.index.get(vid) {
                        let labels = &mut self.vertices[at].1;
                        labels.retain(|present| present != label);
                        if *member {
                            labels.push(*label);
                        }
                    }
                }
                _ => {}
            }
        }
        self.batches.push(batch);
    }

    fn bucket(
        &mut self,
        at: usize,
        key: fgdb_delta_types::PropertyKeyId,
        value: &CanonicalPropertyValue,
    ) {
        if let CanonicalPropertyValue::Scalar(value) = value {
            self.buckets
                .entry((key, FrozenValueClass::of(value)))
                .or_default()
                .push(at);
        }
    }

    /// A held vertex's property: `None` when the vertex is not held, else
    /// its current value, if any.
    fn property(
        &self,
        vertex: VId,
        key: fgdb_delta_types::PropertyKeyId,
    ) -> Option<Option<CanonicalPropertyValue>> {
        let &at = self.index.get(&vertex)?;
        Some(
            self.vertices[at]
                .2
                .iter()
                .find(|(present, _)| *present == key)
                .map(|(_, value)| value.clone()),
        )
    }

    /// The held batches, in creation order.
    pub(crate) fn into_batches(self) -> Vec<WriteBatch> {
        self.batches
    }

    /// The chain's created vertices that satisfy every predicate of a MERGE
    /// whose first key is `key = value`, in creation order.
    fn matching(
        &self,
        key: Option<(fgdb_delta_types::PropertyKeyId, &CanonicalScalar)>,
        predicates: &[fgdb_gql::algebra::VertexPredicate],
    ) -> Vec<VId> {
        let accepts = |at: &usize| {
            let (_, labels, props) = &self.vertices[*at];
            predicates
                .iter()
                .all(|predicate| predicate.matches(labels, props))
        };
        let Some((key, value)) = key else {
            return (0..self.vertices.len())
                .filter(accepts)
                .map(|at| self.vertices[at].0)
                .collect();
        };
        let bucket = |class: FrozenValueClass| {
            self.buckets
                .get(&(key, class))
                .map_or(&[][..], Vec::as_slice)
        };
        let mut candidates: Vec<usize> = match FrozenValueClass::of(value) {
            // A number can equal a decimal, and a decimal any number.
            class @ FrozenValueClass::Number(_) => bucket(class)
                .iter()
                .chain(bucket(FrozenValueClass::Decimal))
                .copied()
                .collect(),
            FrozenValueClass::Decimal => self
                .buckets
                .range((key, FrozenValueClass::Number(0))..=(key, FrozenValueClass::Decimal))
                .flat_map(|(_, at)| at.iter().copied())
                .collect(),
            class => bucket(class).to_vec(),
        };
        candidates.sort_unstable();
        candidates.dedup();
        candidates
            .into_iter()
            .filter(accepts)
            .map(|at| self.vertices[at].0)
            .collect()
    }
}

/// Whether a write chain runs frozen (fgdb-0s2pq): every step is a vertex
/// MERGE with no clause but ON CREATE, or a relationship MERGE whose
/// selection reads only the previous step's rows, with at most one
/// relationship MERGE, at most a final CREATE over the previous step's rows,
/// and no RETURN. Such a chain's effects are creations and assignments to
/// what it created. Each lookup sees the chain-entry overlay plus the chain's
/// own held vertices with their ON CREATE assignments, and the relationship
/// MERGE finds no relationship on a held vertex. Nothing reads what the final
/// CREATE makes. So holding every effect until the end gives the sequential
/// chain's answer.
fn frozen_chain(chain: &fgdb_gql::PreparedGraphWriteChain) -> bool {
    use fgdb_gql::GraphWriteChainStep;
    let mut vertex_steps = 0;
    let mut edge_steps = 0;
    let last = chain.steps().len().saturating_sub(1);
    for (at, step) in chain.steps().iter().enumerate() {
        match step {
            GraphWriteChainStep::VertexMerge(merge) => {
                // ON CREATE acts only on the vertex its row's MERGE created,
                // which the chain holds. ON MATCH and a trailing SET update
                // vertices the chain did not create.
                if merge.actions().is_some_and(|actions| {
                    !actions.on_match.is_empty() || !actions.after.is_empty()
                }) {
                    return false;
                }
                // Every tuple's single MERGE shares this template's shape.
                let probe: Vec<CanonicalScalar> = merge
                    .property_keys()
                    .map(|_| CanonicalScalar::Int(0))
                    .collect();
                if !merge
                    .merge_for(&probe)
                    .is_ok_and(|single| frozen_merge_predicates(&single).is_some())
                {
                    return false;
                }
                vertex_steps += 1;
            }
            GraphWriteChainStep::EdgeMergeRows { merge, .. }
            | GraphWriteChainStep::EdgeMerge(merge)
                if merge
                    .input_relation()
                    .is_some_and(|relation| !relation.reads_graph()) =>
            {
                edge_steps += 1;
            }
            GraphWriteChainStep::Insert(insert)
                if at == last
                    && insert
                        .relational_selection()
                        .is_some_and(|relation| !relation.reads_graph()) => {}
            _ => return false,
        }
    }
    vertex_steps > 0 && edge_steps <= 1
}

/// The predicates of a single-tuple MERGE's one-vertex selection, when that
/// selection is a plain scan filtered by them and projecting the vertex alone
/// (what `PreparedGraphVertexMergeRows::merge_for` builds). Anything else has
/// no frozen form.
fn frozen_merge_predicates(
    merge: &fgdb_gql::PreparedGraphVertexMerge,
) -> Option<Vec<fgdb_gql::algebra::VertexPredicate>> {
    use fgdb_gql::algebra::GlaOperator;
    let selection = merge.selection();
    if selection.columns().len() != 1 || merge.target_column() != 0 {
        return None;
    }
    let mut predicates = Vec::new();
    let mut scans = 0;
    for operator in selection.plan().operators() {
        match operator {
            GlaOperator::ScanVertices => scans += 1,
            GlaOperator::Select {
                predicates: more, ..
            } => predicates.extend(more.iter().cloned()),
            // The value projection's tail (`prepare_values`): ordering and an
            // unbounded window change no match. A real page would cap the
            // matches, so it has no frozen form.
            GlaOperator::VertexIdentity { .. }
            | GlaOperator::Project { .. }
            | GlaOperator::ProjectBindings { .. }
            | GlaOperator::ProjectValues { .. }
            | GlaOperator::Distinct
            | GlaOperator::OrderByVertexId
            | GlaOperator::OrderByValues
            | GlaOperator::Limit {
                offset: 0,
                count: None,
            } => {}
            _ => return None,
        }
    }
    (scans == 1).then_some(predicates)
}

// A proposal is private until its owner has staged and completed it. It is not
// a second evaluator: the supplied GLA source and ordinary insertion collector
// still own all expression, matching and allocation semantics.
struct VertexMergeProposal<O = fgdb_gql::GraphVertexMergeOutcome> {
    stats: fgdb_gql::GraphVertexMergeStats,
    outcome: O,
    creation: Option<fgdb_gql::insertion::GraphInsertBatch>,
}

// Ownership transfer only. The collector has already admitted and charged
// every retained identity. A bulk owner can retain this very set, without
// allocating a second result bag or changing the singular primitive's meter.
trait VertexMergeOutput {
    const UNIQUE: bool;
    fn matched(matches: std::collections::BTreeSet<VId>) -> Self;
    fn created(vertex: VId) -> Self;
}

impl VertexMergeOutput for fgdb_gql::GraphVertexMergeOutcome {
    const UNIQUE: bool = true;

    fn matched(matches: std::collections::BTreeSet<VId>) -> Self {
        Self::Matched(
            matches
                .into_iter()
                .next()
                .expect("one admitted MERGE target"),
        )
    }

    fn created(vertex: VId) -> Self {
        Self::Created(vertex)
    }
}

type VertexMergeFault<E, A, C> = fgdb_gql::GqlQueryError<fgdb_gql::GraphVertexMergeError<E, A>, C>;
type VertexMergeCollection<E, A, C, O = fgdb_gql::GraphVertexMergeOutcome> =
    Result<VertexMergeProposal<O>, VertexMergeFault<E, A, C>>;
type VertexMergeSelection<E, C> = Result<
    fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
    fgdb_gql::GqlQueryError<E, C>,
>;

fn collect_vertex_merge<E, A, C>(
    merge: &fgdb_gql::PreparedGraphVertexMerge,
    policy: fgdb_gql::GraphVertexMergePolicy,
    source: impl FnOnce(
        &fgdb_gql::algebra::PreparedGraphPattern<fgdb_gql::algebra::GraphValueRow>,
        fgdb_gql::GqlQueryPolicy,
    ) -> VertexMergeSelection<E, C>,
    allocate: impl FnMut(fgdb_gql::insertion::GraphInsertRequest) -> Result<ElementId, A>,
    checkpoint: impl FnMut() -> Result<(), C>,
) -> VertexMergeCollection<E, A, C> {
    collect_vertex_merge_output(merge, policy, source, allocate, checkpoint)
}

fn collect_vertex_merge_output<O: VertexMergeOutput, E, A, C>(
    merge: &fgdb_gql::PreparedGraphVertexMerge,
    policy: fgdb_gql::GraphVertexMergePolicy,
    source: impl FnOnce(
        &fgdb_gql::algebra::PreparedGraphPattern<fgdb_gql::algebra::GraphValueRow>,
        fgdb_gql::GqlQueryPolicy,
    ) -> VertexMergeSelection<E, C>,
    allocate: impl FnMut(fgdb_gql::insertion::GraphInsertRequest) -> Result<ElementId, A>,
    mut checkpoint: impl FnMut() -> Result<(), C>,
) -> VertexMergeCollection<E, A, C, O> {
    use fgdb_gql::insertion::{GraphInsertIntent, GraphInsertPolicy};
    use fgdb_gql::{
        GlaExecutionEvent, GlaLimitDimension, GlaLimitExceeded, GqlBudgetDimension, GqlQueryError,
        GraphVertexMergeError, GraphVertexMergeStats,
    };
    let selected = source(merge.selection(), policy.query)
        .map_err(|error| error.map_source(GraphVertexMergeError::Source))?;
    if u64::try_from(selected.value.len()).ok() != Some(selected.rows.result_rows) {
        return Err(GqlQueryError::Source(
            GraphVertexMergeError::InvalidSourceStatistics,
        ));
    }
    let mut evaluator = selected.evaluator;
    let mut event = |kind: GlaExecutionEvent| -> Result<(), VertexMergeFault<E, A, C>> {
        checkpoint().map_err(GqlQueryError::Interrupted)?;
        let work = u128::from(evaluator.work_units) + 1;
        let scratch = u128::from(evaluator.scratch_entries)
            + u128::from(kind == GlaExecutionEvent::ScratchEntry);
        for (observed, limit, dimension) in [
            (
                work,
                policy.query.evaluator.max_work_units,
                GlaLimitDimension::WorkUnits,
            ),
            (
                scratch,
                policy.query.evaluator.max_scratch_entries,
                GlaLimitDimension::ScratchEntries,
            ),
        ] {
            if observed > u128::from(limit) {
                return Err(GqlQueryError::Evaluator(GlaLimitExceeded {
                    dimension,
                    limit,
                    observed,
                }));
            }
        }
        evaluator.work_units = work as u64;
        evaluator.scratch_entries = scratch as u64;
        Ok(())
    };
    event(GlaExecutionEvent::Work)?;
    let expected_columns = merge.selection().columns().len();
    let target = merge.target_column();
    let mut matches = std::collections::BTreeSet::new();
    for (row_at, row) in selected.value.iter().enumerate() {
        event(GlaExecutionEvent::Work)?;
        if row.len() != expected_columns || target >= row.len() {
            return Err(GqlQueryError::Source(GraphVertexMergeError::InputSchema {
                row: row_at,
                column: row.len().min(expected_columns),
            }));
        }
        let value = &row.values()[target];
        if value.is_null() {
            continue;
        }
        let Some(vertex) = value.as_vertex() else {
            return Err(GqlQueryError::Source(GraphVertexMergeError::InputSchema {
                row: row_at,
                column: target,
            }));
        };
        if !matches.contains(&vertex) {
            event(GlaExecutionEvent::ScratchEntry)?;
            matches.insert(vertex);
        }
    }
    checkpoint().map_err(GqlQueryError::Interrupted)?;
    if O::UNIQUE && matches.len() > 1 {
        return Err(GqlQueryError::Source(
            GraphVertexMergeError::AmbiguousMatches {
                observed: matches.len() as u64,
            },
        ));
    }
    if !matches.is_empty() {
        return Ok(VertexMergeProposal {
            stats: GraphVertexMergeStats {
                match_selection: selected.rows,
                evaluator,
                created_vertices: 0,
            },
            outcome: O::matched(matches),
            creation: None,
        });
    }
    // Creation receives only the remaining MATCH/reduction allowance. Its unit
    // input is internal, but must not obtain a fresh query or evaluator quota.
    let remaining = fgdb_gql::GqlQueryPolicy {
        rows: merge_remaining_budget(policy.query.rows, selected.rows),
        evaluator: fgdb_gql::GlaExecutionLimits::new(
            policy
                .query
                .evaluator
                .max_work_units
                .checked_sub(evaluator.work_units)
                .expect("successful reduction is within the work limit"),
            policy
                .query
                .evaluator
                .max_scratch_entries
                .checked_sub(evaluator.scratch_entries)
                .expect("successful reduction is within the scratch limit"),
        ),
    };
    let created = merge.creation().execute_governed::<E, A, C>(
        GraphInsertPolicy::new(remaining, policy.max_created_vertices, 0),
        |_, _| unreachable!("validated MERGE creation is standalone"),
        allocate,
        &mut checkpoint,
    );
    let creation = match created {
        Ok(value) => value,
        Err(GqlQueryError::Source(error)) => {
            return Err(GqlQueryError::Source(GraphVertexMergeError::Creation(
                error,
            )));
        }
        Err(GqlQueryError::Rows(mut error)) => {
            let used = match error.dimension {
                GqlBudgetDimension::SnapshotRecords => selected.rows.snapshot_records,
                GqlBudgetDimension::ResultRows => selected.rows.result_rows,
            };
            error.limit = match error.dimension {
                GqlBudgetDimension::SnapshotRecords => policy.query.rows.max_snapshot_records(),
                GqlBudgetDimension::ResultRows => policy.query.rows.max_result_rows(),
            }
            .expect("a remaining budget error implies an original finite limit");
            error.observed = error
                .observed
                .checked_add(used)
                .ok_or(GqlQueryError::Source(
                    GraphVertexMergeError::AccountingOverflow,
                ))?;
            return Err(GqlQueryError::Rows(error));
        }
        Err(GqlQueryError::Evaluator(mut error)) => {
            match error.dimension {
                GlaLimitDimension::WorkUnits => {
                    error.limit = policy.query.evaluator.max_work_units;
                    error.observed += u128::from(evaluator.work_units);
                }
                GlaLimitDimension::ScratchEntries => {
                    error.limit = policy.query.evaluator.max_scratch_entries;
                    error.observed += u128::from(evaluator.scratch_entries);
                }
            }
            return Err(GqlQueryError::Evaluator(error));
        }
        Err(GqlQueryError::Interrupted(error)) => return Err(GqlQueryError::Interrupted(error)),
        Err(GqlQueryError::IdentifiedEdgesRequired) => {
            return Err(GqlQueryError::IdentifiedEdgesRequired);
        }
        Err(GqlQueryError::Data(error)) => return Err(GqlQueryError::Data(error)),
    };
    let [GraphInsertIntent::Vertex { vertex, .. }] = creation.intents() else {
        unreachable!("validated MERGE creation produces exactly one vertex")
    };
    let outcome = O::created(*vertex);
    let inserted = creation.stats();
    let stats = GraphVertexMergeStats {
        match_selection: selected.rows,
        evaluator: fgdb_gql::GlaExecutionStats {
            work_units: evaluator
                .work_units
                .checked_add(inserted.evaluator.work_units)
                .expect("MERGE work is bounded by the original u64 policy"),
            scratch_entries: evaluator
                .scratch_entries
                .checked_add(inserted.evaluator.scratch_entries)
                .expect("MERGE scratch is bounded by the original u64 policy"),
        },
        created_vertices: 1,
    };
    Ok(VertexMergeProposal {
        stats,
        outcome,
        creation: Some(creation),
    })
}

impl WriteTxn {
    /// Match exactly one distinct vertex or create exactly one new vertex.
    /// Repeated occurrences of the same vertex are one match; two or more
    /// distinct identities refuse. A missing match invokes the definition's
    /// validated standalone single-vertex insertion through the ordinary
    /// allocator and storage-preparation path.
    ///
    /// The match scan remains a transaction conflict witness. Therefore a
    /// concurrent insertion that changes the matched domain after a zero-match
    /// decision invalidates completion instead of silently creating a duplicate.
    /// Success only stages the create branch; an existing match is read-only.
    pub fn execute_graph_vertex_merge_governed<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        merge: &fgdb_gql::PreparedGraphVertexMerge,
        policy: fgdb_gql::GraphVertexMergePolicy,
        allocate: impl FnMut(fgdb_gql::insertion::GraphInsertRequest) -> Result<ElementId, A>,
    ) -> Result<
        (
            fgdb_gql::GraphVertexMergeStats,
            fgdb_gql::GraphVertexMergeOutcome,
        ),
        TxnGqlError<fgdb_gql::GraphVertexMergeError<WriteTxnError, A>>,
    > {
        use fgdb_gql::{GqlQueryError, GraphVertexMergeError};
        let infrastructure = |error| GqlQueryError::Source(GraphVertexMergeError::Source(error));
        self.ensure_database(database).map_err(infrastructure)?;
        let live = database
            .frontier()
            .map_err(WriteTxnError::from)
            .map_err(infrastructure)?;
        if live != self.basis {
            return Err(infrastructure(WriteTxnError::SnapshotAdvanced {
                pinned: self.basis,
                live,
            }));
        }
        if let Some(first) = self.staged.first()
            && !self.program_multi_relation
            && self
                .staged
                .iter()
                .all(|batch| batch.relation == first.relation)
            && merge.relation() != first.relation
        {
            return Err(infrastructure(WriteTxnError::RelationMismatch {
                expected: first.relation,
                found: merge.relation(),
            }));
        }
        cx.with_restriction(|| {
            let proposal = collect_vertex_merge(
                merge,
                policy,
                |pattern, allowance| {
                    self.execute_graph_pattern_governed(database, cx, pattern, allowance)
                },
                allocate,
                || cx.checkpoint(),
            )?;
            if let Some(creation) = proposal.creation {
                stage_vertex_merge_creation(self, database, cx, merge.relation(), creation)?;
            }
            // No fallible work follows atomic staging.
            Ok((proposal.stats, proposal.outcome))
        })
    }

    /// A vertex MERGE over input rows: a chained write's step (fgdb-cpny9).
    /// Rows with equal property tuples share one MERGE. Each distinct tuple
    /// binds every vertex it selects, in identity order, or creates one.
    /// Tuples run in first-row order and each creation stages before the
    /// next tuple selects, so a later row sees an earlier row's vertex, as
    /// openCypher's row-by-row MERGE does. That holds even when two
    /// different tuples compare equal, such as `1` and `1.0`. The receipt
    /// lists each tuple's outcomes. The result carries the input rows (the
    /// previous write's, a leading UNWIND's, or one empty row) and one
    /// (input row, vertex) pair per vertex each row merged, in row order and
    /// then identity order. Each pair, like each tuple, takes one scratch
    /// entry. These are private occurrences, not final result rows: the
    /// enclosing chain admits RETURN only after grouping and pagination.
    ///
    /// With `frozen` (a chain of creating MERGEs, fgdb-0s2pq), creations and
    /// their ON CREATE clauses are held there instead of staged, and each
    /// tuple also matches the chain's held vertices that its predicates
    /// admit: the same answer as staging each effect, without re-reading a
    /// growing overlay per tuple.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn execute_graph_vertex_merges_over<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        merge: &fgdb_gql::PreparedGraphVertexMergeRows,
        policy: fgdb_gql::GraphVertexMergePolicy,
        owned: Option<Vec<fgdb_gql::algebra::GraphValueRow>>,
        mut frozen: Option<&mut FrozenCreations>,
        mut allocate: impl FnMut(fgdb_gql::insertion::GraphInsertRequest) -> Result<ElementId, A>,
    ) -> VertexMergeRowsResult<A> {
        use fgdb_gql::insertion::{GraphInsertError, GraphInsertIntent};
        use fgdb_gql::{
            GlaExecutionEvent, GlaLimitDimension, GlaLimitExceeded, GqlExecutionStats,
            GqlQueryError, GqlQueryPolicy, GraphVertexMergeError, GraphVertexMergeOutcome,
            GraphVertexMergePolicy, GraphVertexMergeStats, GraphWriteStepReceipt,
        };
        // Both the leading relation and every per-key match feed the write,
        // not the public result. A one-row aggregate (or LIMIT 0) must not
        // truncate/refuse those occurrences before all effects are staged.
        // Keep the caller's snapshot, work, scratch and creation limits;
        // only final-row admission belongs to the chain's RETURN owner.
        // This private adapter does not change the standalone MERGE APIs.
        let mut policy = policy;
        policy.query.rows = match policy.query.rows.max_snapshot_records() {
            Some(limit) => fgdb_gql::GqlExecutionBudget::snapshot_records(limit),
            None => fgdb_gql::GqlExecutionBudget::UNLIMITED,
        };
        let infrastructure = |error| GqlQueryError::Source(GraphVertexMergeError::Source(error));
        self.ensure_database(database).map_err(infrastructure)?;
        let live = database
            .frontier()
            .map_err(WriteTxnError::from)
            .map_err(infrastructure)?;
        if live != self.basis {
            return Err(infrastructure(WriteTxnError::SnapshotAdvanced {
                pinned: self.basis,
                live,
            }));
        }
        cx.with_restriction(|| {
            let selected = merge.select_governed::<WriteTxnError, A, _>(
                owned,
                policy.query,
                |pattern, allowance| {
                    self.execute_graph_pattern_governed(database, cx, pattern, allowance)
                },
                || cx.checkpoint(),
            )?;
            let rows = selected.value;
            let mut used = GraphVertexMergeStats {
                match_selection: GqlExecutionStats {
                    snapshot_records: selected.rows.snapshot_records,
                    result_rows: 0,
                },
                evaluator: selected.evaluator,
                created_vertices: 0,
            };
            // One scratch entry, within the statement's cumulative allowance.
            let scratch = |used: &mut GraphVertexMergeStats| -> Result<
                (),
                TxnGqlError<GraphVertexMergeError<WriteTxnError, A>>,
            > {
                let observed = u128::from(used.evaluator.scratch_entries) + 1;
                let limit = policy.query.evaluator.max_scratch_entries;
                if observed > u128::from(limit) {
                    return Err(GqlQueryError::Evaluator(GlaLimitExceeded {
                        dimension: GlaLimitDimension::ScratchEntries,
                        limit,
                        observed,
                    }));
                }
                used.evaluator.scratch_entries = observed as u64;
                Ok(())
            };
            // One evaluation event of a property value computed from the row.
            let evaluate = |used: &mut GraphVertexMergeStats,
                            event: GlaExecutionEvent|
             -> Result<(), TxnGqlError<GraphVertexMergeError<WriteTxnError, A>>> {
                cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
                if event == GlaExecutionEvent::ScratchEntry {
                    return scratch(used);
                }
                let observed = u128::from(used.evaluator.work_units) + 1;
                let limit = policy.query.evaluator.max_work_units;
                if observed > u128::from(limit) {
                    return Err(GqlQueryError::Evaluator(GlaLimitExceeded {
                        dimension: GlaLimitDimension::WorkUnits,
                        limit,
                        observed,
                    }));
                }
                used.evaluator.work_units = observed as u64;
                Ok(())
            };
            let mut resolved: std::collections::BTreeMap<Vec<CanonicalScalar>, Vec<VId>> =
                std::collections::BTreeMap::new();
            let mut outcomes = Vec::new();
            let mut created = Vec::new();
            let mut pairs = Vec::new();
            // ON/SET clauses may change what a later row's MERGE selects, so
            // with clauses every row runs its own MERGE.
            let memoized = merge.actions().is_none();
            for (at, row) in rows.iter().enumerate() {
                cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
                let tuple = merge.tuple(at, row, &mut |event| evaluate(&mut used, event))?;
                // Whether this row's MERGE created its vertex.
                let mut created_here = false;
                if !memoized || !resolved.contains_key(&tuple) {
                    scratch(&mut used)?;
                    let single = merge.merge_for(&tuple).map_err(|source| {
                        GqlQueryError::Source(GraphVertexMergeError::RowMerge {
                            row: at,
                            source: Box::new(source),
                        })
                    })?;
                    // What the statement has left after the tuples so far.
                    let rows_left = |limit: Option<u64>, spent: u64| {
                        limit.map_or(u64::MAX, |limit| limit.saturating_sub(spent))
                    };
                    let allowance = GraphVertexMergePolicy::new(GqlQueryPolicy::new(
                        rows_left(
                            policy.query.rows.max_snapshot_records(),
                            used.match_selection.snapshot_records,
                        ),
                        rows_left(
                            policy.query.rows.max_result_rows(),
                            used.match_selection.result_rows,
                        ),
                        policy
                            .query
                            .evaluator
                            .max_work_units
                            .saturating_sub(used.evaluator.work_units),
                        policy
                            .query
                            .evaluator
                            .max_scratch_entries
                            .saturating_sub(used.evaluator.scratch_entries),
                    ))
                    .with_creation_limit(
                        policy
                            .max_created_vertices
                            .saturating_sub(used.created_vertices),
                    );
                    // A frozen chain's held vertices are part of what this
                    // MERGE sees, exactly as if they had been staged.
                    let held = match frozen.as_deref() {
                        Some(frozen) => {
                            let predicates = frozen_merge_predicates(&single).ok_or(
                                GqlQueryError::Source(GraphVertexMergeError::Source(
                                    WriteTxnError::UnsupportedAtomicMutation,
                                )),
                            )?;
                            frozen.matching(
                                merge.property_keys().next().zip(tuple.first()),
                                &predicates,
                            )
                        }
                        None => Vec::new(),
                    };
                    // Every distinct match is bound (openCypher MERGE), never
                    // refused as ambiguous.
                    let proposal = collect_vertex_merge_output::<VertexMergeTargets, _, A, _>(
                        &single,
                        allowance,
                        |pattern, allowance| {
                            let mut selected = self
                                .execute_graph_pattern_governed(database, cx, pattern, allowance)?;
                            for vertex in held {
                                selected.value.push(
                                    fgdb_gql::algebra::GraphValueRow::from_owned_values(vec![
                                        fgdb_gql::algebra::GraphValue::Vertex(vertex),
                                    ]),
                                );
                                selected.rows.result_rows += 1;
                            }
                            Ok(selected)
                        },
                        &mut allocate,
                        || cx.checkpoint(),
                    )?;
                    let stats = proposal.stats;
                    used.match_selection.snapshot_records = used
                        .match_selection
                        .snapshot_records
                        .saturating_add(stats.match_selection.snapshot_records);
                    used.match_selection.result_rows = used
                        .match_selection
                        .result_rows
                        .saturating_add(stats.match_selection.result_rows);
                    used.evaluator.work_units = used
                        .evaluator
                        .work_units
                        .saturating_add(stats.evaluator.work_units);
                    used.evaluator.scratch_entries = used
                        .evaluator
                        .scratch_entries
                        .saturating_add(stats.evaluator.scratch_entries);
                    used.created_vertices =
                        used.created_vertices.saturating_add(stats.created_vertices);
                    let vertices = match proposal.creation {
                        Some(creation) => {
                            let mut vertices = Vec::new();
                            let mut batch = WriteBatch::new(merge.relation());
                            for intent in creation.into_intents() {
                                cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
                                let GraphInsertIntent::Vertex {
                                    vertex,
                                    labels,
                                    properties,
                                } = intent
                                else {
                                    unreachable!("validated vertex MERGE contains no edge creation")
                                };
                                batch.create_vertex_values(vertex, labels, properties);
                                created.push(vertex);
                                outcomes.push(GraphVertexMergeOutcome::Created(vertex));
                                vertices.push(vertex);
                            }
                            cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
                            // Preserve the insertion adapter's composition choice.
                            let staged = if let Some(frozen) = frozen.as_deref_mut() {
                                frozen.defer(batch);
                                Ok(())
                            } else if self
                                .staged
                                .iter()
                                .all(|staged| staged.relation == merge.relation())
                            {
                                self.write(database, batch)
                            } else {
                                self.write_atomic(database, vec![batch])
                            };
                            staged.map_err(|error| {
                                GqlQueryError::Source(GraphVertexMergeError::Creation(
                                    GraphInsertError::Source(error),
                                ))
                            })?;
                            created_here = true;
                            vertices
                        }
                        None => {
                            let matched = proposal.outcome.matched;
                            outcomes.extend(
                                matched
                                    .iter()
                                    .map(|vertex| GraphVertexMergeOutcome::Matched(*vertex)),
                            );
                            matched.into_iter().collect()
                        }
                    };
                    resolved.insert(tuple.clone(), vertices);
                }
                if let Some(actions) = merge.actions() {
                    let selected = if created_here {
                        &actions.on_create
                    } else {
                        &actions.on_match
                    };
                    for vertex in &resolved[&tuple] {
                        self.vertex_merge_row_actions::<V, A>(
                            database,
                            cx,
                            (merge.relation(), *vertex),
                            (at, row),
                            [selected, &actions.after],
                            policy,
                            &mut used,
                            frozen.as_deref_mut(),
                        )?;
                    }
                }
                for vertex in &resolved[&tuple] {
                    scratch(&mut used)?;
                    pairs.push((at, *vertex));
                }
            }
            Ok((
                used,
                GraphWriteStepReceipt::VertexMergeRows { outcomes, created },
                rows,
                pairs,
            ))
        })
    }

    /// One row's ON/SET clauses on one vertex it merged (fgdb-cpny9): the
    /// selected ON clause, then the trailing SET. Each clause reads all of
    /// its right-hand sides (the vertex's properties before the clause, and
    /// the row) before staging any of them, and the next clause sees that
    /// staging, as a unique MERGE's clauses do. Every action, input and
    /// payload unit is charged to the statement's evaluator allowance.
    #[allow(clippy::too_many_arguments)]
    fn vertex_merge_row_actions<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        (relation, vertex): (RelationId, VId),
        (at, row): (usize, &fgdb_gql::algebra::GraphValueRow),
        clauses: [&[fgdb_gql::GraphVertexMergeRowAction]; 2],
        policy: fgdb_gql::GraphVertexMergePolicy,
        used: &mut fgdb_gql::GraphVertexMergeStats,
        held: Option<&mut FrozenCreations>,
    ) -> Result<(), TxnGqlError<fgdb_gql::GraphVertexMergeError<WriteTxnError, A>>> {
        use fgdb_gql::algebra::GraphValue;
        use fgdb_gql::{GqlQueryError, GraphVertexMergeError};
        let source = |error| GqlQueryError::Source(GraphVertexMergeError::Source(error));
        let state = std::cell::RefCell::new((self, database, held));
        vertex_merge_row_actions_controlled(
            (relation, vertex),
            (at, row),
            clauses,
            policy,
            used,
            || cx.checkpoint(),
            |key, _control| {
                let state = state.borrow();
                let stored = match state
                    .2
                    .as_deref()
                    .and_then(|held| held.property(vertex, key))
                {
                    Some(stored) => stored,
                    None => state
                        .0
                        .merged_vertex_property(state.1, vertex, key)
                        .map_err(source)?,
                };
                match stored {
                    None => Ok(GraphValue::Scalar(CanonicalScalar::Null)),
                    Some(value) => GraphValue::from_property_value(&value).ok_or(
                        GqlQueryError::Data(fgdb_gql::GraphIntegerError {
                            instruction: 0,
                            kind: fgdb_gql::GraphIntegerErrorKind::StoredValueBounds,
                        }),
                    ),
                }
            },
            |batch| {
                let mut state = state.borrow_mut();
                let (transaction, database, held) = &mut *state;
                let staged = if let Some(held) = held.as_deref_mut() {
                    held.defer(batch);
                    Ok(())
                } else if transaction
                    .staged
                    .iter()
                    .all(|staged| staged.relation == relation)
                {
                    transaction.write(database, batch)
                } else {
                    transaction.write_atomic(database, vec![batch])
                };
                staged.map_err(source)
            },
        )
    }
}

/// One shared ON/SET evaluator for owner and capability-authorized row MERGE.
/// The host owns property visibility and original-intent staging; expressions,
/// simultaneous assignments and clause order have no alternate evaluator.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub(crate) fn vertex_merge_row_actions_controlled<E, A, C>(
    (relation, vertex): (RelationId, VId),
    (at, row): (usize, &fgdb_gql::algebra::GraphValueRow),
    clauses: [&[fgdb_gql::GraphVertexMergeRowAction]; 2],
    policy: fgdb_gql::GraphVertexMergePolicy,
    used: &mut fgdb_gql::GraphVertexMergeStats,
    checkpoint: impl FnMut() -> Result<(), C>,
    mut read: impl FnMut(
        fgdb_delta_types::PropertyKeyId,
        &mut dyn FnMut(
            fgdb_gql::GlaExecutionEvent,
        ) -> Result<
            (),
            fgdb_gql::GqlQueryError<fgdb_gql::GraphVertexMergeError<E, A>, C>,
        >,
    ) -> Result<
        fgdb_gql::algebra::GraphValue,
        fgdb_gql::GqlQueryError<fgdb_gql::GraphVertexMergeError<E, A>, C>,
    >,
    mut stage: impl FnMut(
        WriteBatch,
    ) -> Result<
        (),
        fgdb_gql::GqlQueryError<fgdb_gql::GraphVertexMergeError<E, A>, C>,
    >,
) -> Result<(), fgdb_gql::GqlQueryError<fgdb_gql::GraphVertexMergeError<E, A>, C>> {
    use crate::gql_exec::source::SourceEvent;
    use fgdb_gql::{
        GlaExecutionEvent, GlaLimitDimension, GlaLimitExceeded, GqlQueryError,
        GraphIntegerEvaluationError, GraphVertexMergeError, GraphVertexMergeRowAction,
        GraphVertexMergeRowInput, GraphVertexUpsertAction,
    };
    let limits = policy.query.evaluator;
    let checkpoint = std::cell::RefCell::new(checkpoint);
    let charge = |used: &mut fgdb_gql::GraphVertexMergeStats,
                  event: GlaExecutionEvent|
     -> Result<(), GqlQueryError<GraphVertexMergeError<E, A>, C>> {
        checkpoint.borrow_mut()().map_err(GqlQueryError::Interrupted)?;
        let work = u128::from(used.evaluator.work_units) + 1;
        let scratch = u128::from(used.evaluator.scratch_entries)
            + u128::from(event == GlaExecutionEvent::ScratchEntry);
        for (observed, limit, dimension) in [
            (work, limits.max_work_units, GlaLimitDimension::WorkUnits),
            (
                scratch,
                limits.max_scratch_entries,
                GlaLimitDimension::ScratchEntries,
            ),
        ] {
            if observed > u128::from(limit) {
                return Err(GqlQueryError::Evaluator(GlaLimitExceeded {
                    dimension,
                    limit,
                    observed,
                }));
            }
        }
        used.evaluator.work_units = work as u64;
        used.evaluator.scratch_entries = scratch as u64;
        Ok(())
    };
    let payload = |event: SourceEvent| match event {
        SourceEvent::ScratchEntry => GlaExecutionEvent::ScratchEntry,
        _ => GlaExecutionEvent::Work,
    };
    for (clause, actions) in clauses.into_iter().enumerate() {
        if actions.is_empty() {
            continue;
        }
        let mut batch = WriteBatch::new(relation);
        for (action, definition) in actions.iter().enumerate() {
            charge(used, GlaExecutionEvent::Work)?;
            charge(used, GlaExecutionEvent::ScratchEntry)?; // proposal before allocation
            match definition {
                GraphVertexMergeRowAction::Vertex(GraphVertexUpsertAction::SetProperty {
                    key,
                    value,
                }) => {
                    point_scalar_payload_admission(value.value(), &mut |event| {
                        charge(used, payload(event))
                    })?;
                    batch.set_vertex_property(vertex, *key, Some(value.value().clone()));
                }
                GraphVertexMergeRowAction::Vertex(GraphVertexUpsertAction::SetStored {
                    key,
                    value,
                }) => {
                    point_payload_admission(value, &mut |event| charge(used, payload(event)))?;
                    batch.set_vertex_property_value(vertex, *key, Some(value.clone()));
                }
                GraphVertexMergeRowAction::Vertex(GraphVertexUpsertAction::SetLabel {
                    label,
                    present,
                }) => {
                    batch.set_vertex_label(vertex, *label, *present);
                }
                // The step compiler builds every expression as a row
                // expression; a vertex-only one is the unique MERGE's.
                GraphVertexMergeRowAction::Vertex(GraphVertexUpsertAction::SetExpression {
                    ..
                }) => {
                    return Err(GqlQueryError::Source(GraphVertexMergeError::InputSchema {
                        row: at,
                        column: action,
                    }));
                }
                GraphVertexMergeRowAction::Expression { key, inputs, value } => {
                    let mut frame = Vec::with_capacity(inputs.len());
                    for input in inputs {
                        charge(used, GlaExecutionEvent::ScratchEntry)?;
                        frame.push(match *input {
                            GraphVertexMergeRowInput::Property(key) => {
                                charge(used, GlaExecutionEvent::Work)?;
                                read(key, &mut |event| charge(used, event))?
                            }
                            GraphVertexMergeRowInput::Column(column) => {
                                row.values()
                                    .get(column)
                                    .ok_or(GqlQueryError::Source(
                                        GraphVertexMergeError::InputSchema { row: at, column },
                                    ))?
                                    .copy_with_control(&mut |event| charge(used, event))?
                            }
                        });
                    }
                    let value = value
                        .evaluate_scalar_with_control(&frame, &mut |event| charge(used, event))
                        .map_err(|error| match error {
                            GraphIntegerEvaluationError::Control(error) => error,
                            GraphIntegerEvaluationError::Value(source) => {
                                GqlQueryError::Source(GraphVertexMergeError::RowAction {
                                    row: at,
                                    clause,
                                    action,
                                    source,
                                })
                            }
                        })?;
                    batch.set_vertex_property(vertex, *key, Some(value));
                }
                GraphVertexMergeRowAction::Value { key, value } => {
                    let refusal = |source| {
                        GqlQueryError::Source(GraphVertexMergeError::RowAction {
                            row: at,
                            clause,
                            action,
                            source,
                        })
                    };
                    let written = value
                        .written(row, &mut |event| charge(used, event))
                        .map_err(|error| match error {
                            GraphIntegerEvaluationError::Control(error) => error,
                            GraphIntegerEvaluationError::Value(source) => refusal(source),
                        })?
                        .ok_or_else(|| {
                            refusal(fgdb_gql::GraphIntegerError {
                                instruction: 0,
                                kind: fgdb_gql::GraphIntegerErrorKind::NonScalar,
                            })
                        })?;
                    point_payload_admission(&written, &mut |event| charge(used, payload(event)))?;
                    batch.set_vertex_property_value(vertex, *key, Some(written));
                }
            }
        }
        // No right-hand side observes a sibling assignment; the next
        // clause sees the host's completed original-intent staging.
        checkpoint.borrow_mut()().map_err(GqlQueryError::Interrupted)?;
        stage(batch)?;
    }
    Ok(())
}

impl WriteTxn {
    /// One final-state stored property of a vertex a MERGE step returned,
    /// read through the governed point projection and witnessed, as
    /// relationship MERGE RETURN reads its endpoints. A missing field is NULL.
    pub(crate) fn merged_vertex_property<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        vertex: VId,
        key: fgdb_delta_types::PropertyKeyId,
    ) -> Result<Option<CanonicalPropertyValue>, WriteTxnError> {
        self.merge_return_property(database, ElementId::Vertex(vertex), key)
    }
}

// Both cardinality owners stage the collector's ordinary insertion proposal
// through this same path. No new identity, durable format or commit is minted.
fn stage_vertex_merge_creation<V: Vfs + Clone, A>(
    transaction: &mut WriteTxn,
    database: &mut Database<V>,
    cx: &fgdb_types::QueryCx,
    relation: fgdb_delta_types::RelationId,
    creation: fgdb_gql::insertion::GraphInsertBatch,
) -> Result<(), TxnGqlError<fgdb_gql::GraphVertexMergeError<WriteTxnError, A>>> {
    use fgdb_gql::insertion::{GraphInsertError, GraphInsertIntent};
    use fgdb_gql::{GqlQueryError, GraphVertexMergeError};

    let mut batch = WriteBatch::new(relation);
    for intent in creation.into_intents() {
        cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
        let GraphInsertIntent::Vertex {
            vertex,
            labels,
            properties,
        } = intent
        else {
            unreachable!("validated vertex MERGE contains no edge creation")
        };
        batch.create_vertex_values(vertex, labels, properties);
    }
    cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
    let staged = if transaction
        .staged
        .iter()
        .all(|batch| batch.relation == relation)
    {
        transaction.write(database, batch)
    } else {
        transaction.write_atomic(database, vec![batch])
    };
    staged.map(|_| ()).map_err(|error| {
        GqlQueryError::Source(GraphVertexMergeError::Creation(GraphInsertError::Source(
            error,
        )))
    })
}
