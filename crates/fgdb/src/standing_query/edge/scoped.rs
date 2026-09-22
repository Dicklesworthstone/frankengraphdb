//! Correlated fixed-hop OPTIONAL/EXISTS/NOT EXISTS over canonical standing inputs.
//! Qualification uses the admitted GLA predicates; this module owns only scope
//! boundaries, per-root witness counts and null-extension derivatives. Counts
//! advance with the same whole-commit edge/vertex/aggregate/result publication.

use super::*;
use fgdb_gql::algebra::{MAX_PATTERN_BINDINGS, MAX_PATTERN_EDGES};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Optional,
    Exists,
    NotExists,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Shape {
    mode: Mode,
    pub(super) relation: RelationId,
    pub(super) direction: GlaDirection,
    start: usize,
    end: usize,
    width: u32,
}

fn predicate(op: &GlaOperator, width: u32) -> bool {
    match op {
        GlaOperator::Select { slot, .. } => slot.ordinal() < width,
        GlaOperator::VertexIdentity { left, right, .. }
        | GlaOperator::CompareProperties { left, right, .. } => {
            left.ordinal() < width && right.ordinal() < width
        }
        GlaOperator::SelectBoolean { expression } => {
            expression.supports_vertex_bindings(width as usize)
        }
        _ => false,
    }
}

impl Shape {
    /// Preserve the specialized one-hop lane. Multi-hop scopes are admitted
    /// separately by the affected-binding join engine, not flattened to edges.
    pub(super) fn of(query: &PreparedGraphAggregate) -> Option<Self> {
        Self::fixed_hop(query).filter(|shape| shape.width == 3)
    }

    pub(super) fn multi_hop(query: &PreparedGraphAggregate) -> Option<Self> {
        Self::fixed_hop(query).filter(|shape| shape.width > 3)
    }

    pub(super) fn width(self) -> usize {
        self.width as usize
    }

    pub(super) fn body(self, query: &PreparedGraphAggregate) -> &[GlaOperator] {
        &query.input_pattern().plan().operators()[self.start + 2..self.end]
    }

    /// One correlated positive child rooted at the outer vertex. The compiler
    /// copies slot 0 to child slot 1; each expansion adds one child slot, even
    /// when an identity predicate closes a graph cycle. Independent scans,
    /// additional/nested scopes, captured paths and nullable outer captures
    /// refuse. OPTIONAL must declare the exact complete child-frame width.
    fn fixed_hop(query: &PreparedGraphAggregate) -> Option<Self> {
        let ops = query.input_pattern().plan().operators();
        if !matches!(ops.first(), Some(GlaOperator::ScanVertices)) {
            return None;
        }
        let mut start = 1;
        while ops.get(start).is_some_and(|op| predicate(op, 1)) {
            start += 1;
        }
        let (mode, group, end, slots) = match ops.get(start)? {
            GlaOperator::Optional { group, end, slots } => {
                (Mode::Optional, *group, *end, Some(*slots))
            }
            GlaOperator::Probe { group, end, anti } => (
                if *anti { Mode::NotExists } else { Mode::Exists },
                *group,
                *end,
                None,
            ),
            _ => return None,
        };
        let end = usize::try_from(end).ok()?;
        let closes = match (mode, ops.get(end)) {
            (Mode::Optional, Some(GlaOperator::OptionalEnd { group: actual })) => *actual == group,
            (Mode::Exists | Mode::NotExists, Some(GlaOperator::ProbeEnd { group: actual })) => {
                *actual == group
            }
            _ => false,
        };
        if end <= start + 2 || !closes {
            return None;
        }
        if !matches!(ops.get(start + 1),
            Some(GlaOperator::BindVertex { source }) if source.ordinal() == 0)
        {
            return None;
        }
        let mut expansion = None;
        let mut width = 2_u32;
        let mut hops = 0;
        for op in &ops[start + 2..end] {
            match op {
                GlaOperator::Expand {
                    source,
                    relation,
                    direction,
                } if source.ordinal() >= 1
                    && source.ordinal() < width
                    && hops < MAX_PATTERN_EDGES =>
                {
                    expansion.get_or_insert((*relation, *direction));
                    hops += 1;
                    width = width.checked_add(1)?;
                    if width as usize > MAX_PATTERN_BINDINGS {
                        return None;
                    }
                }
                op if predicate(op, width) => {}
                _ => return None,
            }
        }
        let (relation, direction) = expansion?;
        if slots.is_some_and(|slots| slots != width - 1) {
            return None;
        }
        let GlaOperator::ProjectValues { columns } = ops.get(end + 1)? else {
            return None;
        };
        // Existential locals never escape the probe frame. Only OPTIONAL
        // exports nullable child bindings to grouping/aggregate arguments.
        let output_width = if mode == Mode::Optional { width } else { 1 };
        if columns.iter().any(|column| {
            !matches!(column,
            ValueProjection::Vertex { slot } | ValueProjection::Property { slot, .. }
                if slot.ordinal() < output_width)
        }) {
            return None;
        }
        if !matches!(
            &ops[end + 2..],
            [
                GlaOperator::OrderByValues,
                GlaOperator::Limit {
                    offset: 0,
                    count: None
                },
            ]
        ) {
            return None;
        }
        Some(Self {
            mode,
            relation,
            direction,
            start,
            end,
            width,
        })
    }

    fn keep_root(
        &self,
        query: &PreparedGraphAggregate,
        vid: VId,
        state: &VertexState,
        meter: &mut Meter<'_>,
    ) -> Result<bool, StandingQueryFailure> {
        grouped::keeps(
            &query.input_pattern().plan().operators()[1..self.start],
            &[(vid, state)],
            meter,
        )
    }

    /// Count a COMPLETE child occurrence. The shared join engine supplies the
    /// whole binding; edge prefixes are never OPTIONAL/EXISTS witnesses. The
    /// same routine serves one-hop and multi-hop inputs and preserves ordinary
    /// GLA predicate order, Boolean UNKNOWN and hidden-property dependencies.
    pub(super) fn binding_contribution(
        &self,
        query: &PreparedGraphAggregate,
        binding: &[(VId, &VertexState)],
        sign: i128,
        counts: &mut BTreeMap<VId, i128>,
        output: &mut Vec<grouped::Contribution>,
        meter: &mut Meter<'_>,
    ) -> Result<(), StandingQueryFailure> {
        if binding.len() != self.width() || binding[0].0 != binding[1].0 {
            return Err(StandingQueryFailure::InvalidDelta);
        }
        let (root, state) = binding[0];
        if !self.keep_root(query, root, state, meter)? {
            return Ok(());
        }
        if !grouped::keeps(self.body(query), binding, meter)? {
            return Ok(());
        }
        change(counts, root, sign, meter)?;
        // Semi/anti output is determined once per root after the complete
        // tick. Individual witnesses neither multiply nor retract a root.
        if self.mode != Mode::Optional {
            return Ok(());
        }
        grouped::project_contributions(
            query,
            |slot| {
                binding
                    .get(slot as usize)
                    .copied()
                    .map(Some)
                    .ok_or(StandingQueryFailure::InvalidDelta)
            },
            sign,
            output,
            meter,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn contribute(
        &self,
        query: &PreparedGraphAggregate,
        pair: Endpoints,
        vertices: &Vertices,
        staged: &VertexPatch,
        sign: i128,
        counts: &mut BTreeMap<VId, i128>,
        output: &mut Vec<grouped::Contribution>,
        meter: &mut Meter<'_>,
    ) -> Result<(), StandingQueryFailure> {
        meter.charge(ZSetEvent::Work)?;
        // Validate both physical endpoints before testing predicates. A rejected
        // witness cannot hide a dangling edge or an incomplete vertex cascade.
        let source = vertex(vertices, staged, pair.0)?;
        let target = vertex(vertices, staged, pair.1)?;
        let mut visit = |root, root_state, child, child_state| {
            let binding = [(root, root_state), (root, root_state), (child, child_state)];
            self.binding_contribution(query, &binding, sign, counts, output, meter)
        };
        match self.direction {
            GlaDirection::Forward => visit(pair.0, source, pair.1, target),
            GlaDirection::Reverse => visit(pair.1, target, pair.0, source),
            GlaDirection::Undirected => {
                visit(pair.0, source, pair.1, target)?;
                if pair.0 != pair.1 {
                    visit(pair.1, target, pair.0, source)?;
                }
                Ok(())
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn root_contribution(
        &self,
        query: &PreparedGraphAggregate,
        vid: VId,
        state: Option<&VertexState>,
        witnesses: u64,
        sign: i128,
        output: &mut Vec<grouped::Contribution>,
        meter: &mut Meter<'_>,
    ) -> Result<(), StandingQueryFailure> {
        meter.charge(ZSetEvent::Work)?;
        let Some(state) = state else {
            return if witnesses == 0 {
                Ok(())
            } else {
                Err(StandingQueryFailure::InvalidDelta)
            };
        };
        let selected = match self.mode {
            Mode::Optional | Mode::NotExists => witnesses == 0,
            Mode::Exists => witnesses != 0,
        };
        if !selected || !self.keep_root(query, vid, state, meter)? {
            return Ok(());
        }
        // Preserve the outer root but null-extend the ENTIRE child frame. Do
        // not test child WHERE or root-copy identities against these nulls.
        grouped::project_contributions(
            query,
            |slot| match slot {
                0 => Ok(Some((vid, state))),
                slot if self.mode == Mode::Optional && slot < self.width => Ok(None),
                _ => Err(StandingQueryFailure::InvalidDelta),
            },
            sign,
            output,
            meter,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn finish_roots(
        &self,
        query: &PreparedGraphAggregate,
        before: &BTreeMap<VId, u64>,
        changes: BTreeMap<VId, i128>,
        vertices: &Vertices,
        staged: &VertexPatch,
        output: &mut Vec<grouped::Contribution>,
        meter: &mut Meter<'_>,
    ) -> Result<BTreeMap<VId, u64>, StandingQueryFailure> {
        let mut replacements = BTreeMap::new();
        for (vid, change) in changes {
            meter.charge(ZSetEvent::Work)?;
            let old = before.get(&vid).copied().unwrap_or(0);
            let new = i128::from(old)
                .checked_add(change)
                .and_then(|count| u64::try_from(count).ok())
                .ok_or(StandingQueryFailure::InvalidDelta)?;
            let next = match staged.get(&vid) {
                Some(state) => state.as_ref(),
                None => vertices.get(&vid),
            };
            // A whole-tick witness replacement sees old>0 and new>0: it cannot
            // create a transient null row even when all old EIds were removed.
            // A changed witness count is not a changed semi/anti result. With
            // an untouched root and unchanged presence, retain its aggregate
            // contribution without copying its payload or emitting a fake tick.
            let unchanged_presence = self.mode != Mode::Optional
                && (old == 0) == (new == 0)
                && !staged.contains_key(&vid);
            if !unchanged_presence {
                self.root_contribution(query, vid, vertices.get(&vid), old, -1, output, meter)?;
                self.root_contribution(query, vid, next, new, 1, output, meter)?;
            }
            meter.charge(ZSetEvent::ScratchEntry)?;
            if old == 0 && new != 0 {
                meter.charge(ZSetEvent::ScratchEntry)?;
            }
            replacements.insert(vid, new);
        }
        Ok(replacements)
    }
}

pub(super) fn change(
    counts: &mut BTreeMap<VId, i128>,
    vid: VId,
    change: i128,
    meter: &mut Meter<'_>,
) -> Result<(), StandingQueryFailure> {
    meter.charge(ZSetEvent::Work)?;
    let previous = counts.get(&vid).copied().unwrap_or(0);
    let next = previous
        .checked_add(change)
        .ok_or(StandingQueryFailure::Arithmetic)?;
    if !counts.contains_key(&vid) {
        meter.charge(ZSetEvent::ScratchEntry)?;
    }
    counts.insert(vid, next);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DatabaseKeys, WriteBatch};
    use asupersync::lab::run_async_under_lab;
    use fgdb_gql::GraphAggregate;
    use fgdb_gql::algebra::{
        GraphColumn, GraphMatchClause, GraphPatternBuilder, IntegerComparison,
    };
    use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};

    fn definition(mode: Mode) -> PreparedGraphAggregate {
        let mut root = GraphPatternBuilder::new();
        root.vertex("a").unwrap();
        let mut child = GraphPatternBuilder::new();
        child.vertex("a").unwrap().vertex("b").unwrap();
        child
            .edge("a", RelationId(1), GlaDirection::Undirected, "b")
            .unwrap();
        child
            .compare_properties(
                "a",
                PropertyKeyId(1),
                IntegerComparison::LessOrEqual,
                "b",
                PropertyKeyId(1),
            )
            .unwrap();
        let clause = match mode {
            Mode::Optional => GraphMatchClause::optional(&child),
            Mode::Exists => GraphMatchClause::exists(&child),
            Mode::NotExists => GraphMatchClause::not_exists(&child),
        };
        let value = if mode == Mode::Optional { "b" } else { "a" };
        let input = root
            .prepare_values_with_clauses(
                &[clause],
                &[
                    GraphColumn::vertex("root", "a"),
                    GraphColumn::vertex("child", value),
                    GraphColumn::property("amount", value, PropertyKeyId(1)),
                ],
                0,
                None,
            )
            .unwrap()
            .with_duplicates();
        PreparedGraphAggregate::prepare(
            input,
            &[0],
            &[
                GraphAggregate::count_rows("rows"),
                GraphAggregate::count("matches", 1),
                GraphAggregate::sum_int("sum", 2),
            ],
            0,
            None,
        )
        .unwrap()
    }

    fn seeded(batch: &LogicalDeltaBatch, mode: Mode) -> StandingQuery {
        let definition = definition(mode);
        assert!(eligible(&definition));
        let edges = State::for_definition(&definition);
        assert!(edges.as_ref().unwrap().has_scope());
        let policy = GqlQueryPolicy::new(100_000, 10_000, 10_000_000, 10_000_000);
        let mut query = StandingQuery {
            definition,
            policy,
            edges,
            vertices: BTreeMap::new(),
            aggregate: IncrementalAggregate::new(),
            rows: ZSet::new(),
            last_delta: None,
            frontier: CommitSeq::ORIGIN,
            stats: StandingQueryStats::default(),
            failure: None,
        };
        let mut checkpoint = || Ok(());
        let mut meter = Meter {
            policy,
            stats: StandingQueryStats::default(),
            checkpoint: &mut checkpoint,
        };
        query.maintain(batch, &mut meter).unwrap();
        query.frontier = batch.commit_seq();
        query
    }
    fn unchanged(actual: &StandingQuery, expected: &StandingQuery) {
        assert_eq!(actual.vertices, expected.vertices);
        assert_eq!(actual.edges, expected.edges); // includes all witness counts
        assert_eq!(actual.aggregate, expected.aggregate);
        assert_eq!(actual.rows, expected.rows);
        assert_eq!(actual.last_delta, expected.last_delta);
        assert_eq!(actual.frontier, expected.frontier);
    }

    #[test]
    fn every_optional_checkpoint_rolls_back_witnesses_sources_and_results_then_retries() {
        check_every_boundary(Mode::Optional);
    }

    #[test]
    fn every_semijoin_and_antijoin_checkpoint_preserves_counts_and_can_retry() {
        check_every_boundary(Mode::Exists);
        check_every_boundary(Mode::NotExists);
    }

    fn check_every_boundary(mode: Mode) {
        let ((), report) = run_async_under_lab(0x8f05, move |runtime| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&runtime);
            let cx = contexts.commit();
            let keys = DatabaseKeys::new([1; 32], DatabaseSecurityNamespaceId([2; 32]), [3; 32]);
            let mut db = Database::open_memory(&cx, keys).await.unwrap();
            let mut batch = WriteBatch::new(RelationId(1));
            for id in 1..=3 {
                batch.create_vertex(
                    VId(id),
                    vec![],
                    vec![(PropertyKeyId(1), CanonicalScalar::Int(id as i64))],
                );
            }
            batch.add_edge(EId(1), VId(1), VId(2), vec![]);
            batch.add_edge(EId(2), VId(1), VId(2), vec![]);
            let basis = db.write(&cx, batch).await.unwrap();
            let first = db.delta_index().unwrap().get(basis).unwrap().clone();
            let mut next = WriteBatch::new(RelationId(1));
            next.delete_vertex(VId(2));
            next.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(9)));
            next.create_vertex(
                VId(4),
                vec![],
                vec![(PropertyKeyId(1), CanonicalScalar::Int(20))],
            );
            next.add_edge(EId(3), VId(3), VId(4), vec![]);
            let at = db.write(&cx, next).await.unwrap();
            let delta = db.delta_index().unwrap().get(at).unwrap().clone();
            let before = seeded(&first, mode);
            let mut success = seeded(&first, mode);
            let policy = success.policy;
            let mut total = 0;
            {
                let mut checkpoint = || {
                    total += 1;
                    Ok(())
                };
                let mut meter = Meter {
                    policy,
                    stats: StandingQueryStats::default(),
                    checkpoint: &mut checkpoint,
                };
                success.maintain(&delta, &mut meter).unwrap();
            }
            assert!(total > 0);
            for stop in 1..=total {
                let mut candidate = seeded(&first, mode);
                let mut seen = 0;
                {
                    let mut checkpoint = || {
                        seen += 1;
                        if seen == stop {
                            Err(StandingQueryFailure::Interrupted)
                        } else {
                            Ok(())
                        }
                    };
                    let mut meter = Meter {
                        policy,
                        stats: StandingQueryStats::default(),
                        checkpoint: &mut checkpoint,
                    };
                    assert_eq!(
                        candidate.maintain(&delta, &mut meter),
                        Err(StandingQueryFailure::Interrupted)
                    );
                }
                assert_eq!(seen, stop);
                unchanged(&candidate, &before);
                let mut checkpoint = || Ok(());
                let mut meter = Meter {
                    policy,
                    stats: StandingQueryStats::default(),
                    checkpoint: &mut checkpoint,
                };
                candidate.maintain(&delta, &mut meter).unwrap();
                unchanged(&candidate, &success);
            }
            let mut entries = delta.coordinate_entries().to_vec();
            for entry in &mut entries {
                for row in &mut entry.rows {
                    if let DeltaRow::DeleteVertex {
                        sorted_retired_incident_edges,
                        ..
                    } = row
                    {
                        sorted_retired_incident_edges.clear();
                    }
                }
            }
            let malformed = LogicalDeltaBatch::from_parts_for_test(
                entries,
                *delta.source_template_digest(),
                delta.commit_marker_identity(),
                delta.commit_seq(),
                delta.frontier(),
            );
            let mut candidate = seeded(&first, mode);
            let mut checkpoint = || Ok(());
            let mut meter = Meter {
                policy,
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            assert_eq!(
                candidate.maintain(&malformed, &mut meter),
                Err(StandingQueryFailure::InvalidDelta)
            );
            unchanged(&candidate, &before);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
