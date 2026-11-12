//! Sketch-guided, conformal-calibrated multiway FreeJoin planner, replayable
//! [`JoinDecisionCard`]s, and governed zero-`BTreeMap` execution
//! (`fgdb-rc1111-loom-conformal-freejoin-txo5d`).
//!
//! This module composes:
//! - [`fgdb_sketch`] (`DegreeHistogram` + `DistinctSketch`) for deterministic
//!   per-relation skew and distinct-endpoint summaries,
//! - [`fgdb_calibrate`] (`GraphMetricConformal` + `EProcessTrial`) for
//!   nonconformity-guarded candidate selection against a conservative
//!   deterministic `pinned_fallback`,
//! - [`SliceAdjacencyTrie`] (`fgdb-unsafe-simd` 4-wide gallop/SIMD seek) for
//!   worst-case-optimal Generic Join and [`FactorizedBatch`] execution over
//!   labeled, `WHERE`-filtered, `DifferentEdges`-constrained, and
//!   property-projecting conjunctive graph patterns.

use super::graph::SliceAdjacencyTrie;
use super::{
    FactorizedBatch, FreeJoin, FreeJoinPlan, GraphTrieError, JoinVariable, MAX_JOIN_RELATIONS,
    MAX_JOIN_VARIABLES,
};
use crate::algebra::{
    EdgeRelation, GlaDirection, GlaOperator, GlaOutput, GlaPlan, ValueProjection, VertexPredicate,
};
use crate::algebra_exec::{Charges, ProjectedRows};
use crate::{
    GlaExecutionEvent, GovernedCheckpoint, GqlBudgetDimension, GqlQueryError, GqlQueryExecution,
    GqlQueryPolicy,
};
use fgdb_calibrate::conformal::{
    ConformalProfile, GraphMetricConformal, GraphMetricIdentity, MetricThresholdMode,
    PolicySelection, SequenceWindow as ConformalWindow, SequencedMetricValue,
};
pub use fgdb_calibrate::eprocess::BinaryObservation;
use fgdb_calibrate::eprocess::{
    EProcessConfig, EProcessProfile, EProcessTrial, SequenceWindow as EProcessWindow,
    SequencedObservation, TrialIdentity,
};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_sketch::degree_histogram::DegreeHistogram;
use fgdb_sketch::distinct::{DistinctProfile, DistinctSketch};
use fgdb_types::{CanonicalPropertyValue, EId, ObjectId, VId};

/// Versioned policy epoch for Loom's calibrated FreeJoin controller.
pub const JOIN_DECISION_POLICY_EPOCH: u64 = 1;

const SKETCH_SEED_BASE: u64 = 0x4647_4442_4c4f_4f4d;
const SKETCH_PRECISION: u8 = 8;
const DEFAULT_CONFORMAL_ALPHA: f64 = 0.15;
const DEFAULT_BASELINE_RESIDUALS: [f64; 6] = [0.02, 0.05, 0.08, 0.12, 0.18, 0.25];

/// Physical join strategy members in Loom's unified FreeJoin operator family.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum JoinStrategy {
    /// Variable-at-a-time Generic Join (`FreeJoinPlan::generic`) achieving the
    /// fractional edge-cover AGM bound up to ordered-search logarithms.
    WorstCaseOptimalGeneric = 1,
    /// Covered-group join with disconnected suffix d-trees evaluated once into
    /// a [`FactorizedBatch`] (`Flat` / `Product` / `Union` DAG).
    FactorizedFreeJoin = 2,
    /// Pair-first grouped binary/bushy FreeJoin (`FreeJoinPlan::new` with a
    /// covered 2-variable seed stage).
    BinaryBushy = 3,
}

impl JoinStrategy {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WorstCaseOptimalGeneric => "WorstCaseOptimalGeneric",
            Self::FactorizedFreeJoin => "FactorizedFreeJoin",
            Self::BinaryBushy => "BinaryBushy",
        }
    }

    #[must_use]
    pub fn policy_oid(self) -> ObjectId {
        derived_oid(b"fgdb:loom:join-strategy:v1\0", &[self as u8])
    }
}

/// Reduced rational fractional edge-cover AGM bound exponent $\rho^* = p / q$.
///
/// By the Balinski--Lovasz half-integrality theorem for 2-uniform graph
/// hypergraphs, $\rho^*$ is always half-integral ($q \in \{1, 2\}$).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AgmExponent {
    pub numerator: u32,
    pub denominator: u32,
}

impl AgmExponent {
    #[must_use]
    pub const fn reduced(numerator: u32, denominator: u32) -> Self {
        let g = gcd_u32(numerator, denominator);
        Self {
            numerator: numerator / g,
            denominator: denominator / g,
        }
    }
}

const fn gcd_u32(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        let r = a % b;
        a = b;
        b = r;
    }
    if a == 0 { 1 } else { a }
}

/// Deterministic `fgdb-sketch` summary of one physical edge relation slice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelationSketchSummary {
    pub relation_ordinal: u32,
    pub total_edges: u64,
    pub distinct_sources: u64,
    pub distinct_pairs: u64,
    pub estimated_distinct_endpoints: u64,
    pub exact_distinct_endpoints: u64,
    pub median_degree_upper: u64,
    pub p99_degree_upper: u64,
    pub max_degree: u64,
    pub hll_relative_residual_ppm: u64,
}

/// Optional explicit conformal and e-process calibration history for testing
/// distribution shift, nonconformity fallback, and drift detection.
#[derive(Clone, Debug)]
pub struct ConformalJoinCalibration {
    pub calibration_residuals: Vec<f64>,
    pub observed_residual_override: Option<f64>,
    pub drift_observations: Vec<BinaryObservation>,
}

impl Default for ConformalJoinCalibration {
    fn default() -> Self {
        Self {
            calibration_residuals: DEFAULT_BASELINE_RESIDUALS.to_vec(),
            observed_residual_override: None,
            drift_observations: Vec::new(),
        }
    }
}

/// Replayable adaptive decision card emitted by [`CalibratedFreeJoin`]
/// (`FG-INV-19`, policy epoch [`JOIN_DECISION_POLICY_EPOCH`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JoinDecisionCard {
    pub policy_epoch: u64,
    pub variable_count: u32,
    pub edge_atom_count: u32,
    pub has_predicates: bool,
    pub has_different_edges: bool,
    pub has_factorizable_suffix: bool,
    pub agm_exponent: AgmExponent,
    pub candidate_strategies: Vec<JoinStrategy>,
    pub candidate_strategy: JoinStrategy,
    pub pinned_fallback: JoinStrategy,
    pub selected_strategy: JoinStrategy,
    pub conformal_conforming: Option<bool>,
    pub conformal_threshold_bits: Option<u64>,
    pub nonconformity_score_bits: Option<u64>,
    pub drift_e_value_bits: u64,
    pub drift_rejected_null: bool,
    pub variable_order: Vec<JoinVariable>,
    pub relation_sketches: Vec<RelationSketchSummary>,
}

impl JoinDecisionCard {
    /// Canonical byte transcript for bit-exact certificate & replay digests.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = b"fgdb:loom:join-decision-card:v1\0".to_vec();
        bytes.extend_from_slice(&self.policy_epoch.to_le_bytes());
        bytes.extend_from_slice(&self.variable_count.to_le_bytes());
        bytes.extend_from_slice(&self.edge_atom_count.to_le_bytes());
        bytes.push(u8::from(self.has_predicates));
        bytes.push(u8::from(self.has_different_edges));
        bytes.push(u8::from(self.has_factorizable_suffix));
        bytes.extend_from_slice(&self.agm_exponent.numerator.to_le_bytes());
        bytes.extend_from_slice(&self.agm_exponent.denominator.to_le_bytes());
        bytes.extend_from_slice(&(self.candidate_strategies.len() as u32).to_le_bytes());
        for &strategy in &self.candidate_strategies {
            bytes.push(strategy as u8);
        }
        bytes.push(self.candidate_strategy as u8);
        bytes.push(self.pinned_fallback as u8);
        bytes.push(self.selected_strategy as u8);
        match self.conformal_conforming {
            None => bytes.push(0),
            Some(false) => bytes.push(1),
            Some(true) => bytes.push(2),
        }
        match self.conformal_threshold_bits {
            None => bytes.push(0),
            Some(bits) => {
                bytes.push(1);
                bytes.extend_from_slice(&bits.to_le_bytes());
            }
        }
        match self.nonconformity_score_bits {
            None => bytes.push(0),
            Some(bits) => {
                bytes.push(1);
                bytes.extend_from_slice(&bits.to_le_bytes());
            }
        }
        bytes.extend_from_slice(&self.drift_e_value_bits.to_le_bytes());
        bytes.push(u8::from(self.drift_rejected_null));
        bytes.extend_from_slice(&(self.variable_order.len() as u32).to_le_bytes());
        for &var in &self.variable_order {
            bytes.extend_from_slice(&var.0.to_le_bytes());
        }
        bytes.extend_from_slice(&(self.relation_sketches.len() as u32).to_le_bytes());
        for rel in &self.relation_sketches {
            bytes.extend_from_slice(&rel.relation_ordinal.to_le_bytes());
            bytes.extend_from_slice(&rel.total_edges.to_le_bytes());
            bytes.extend_from_slice(&rel.distinct_sources.to_le_bytes());
            bytes.extend_from_slice(&rel.distinct_pairs.to_le_bytes());
            bytes.extend_from_slice(&rel.estimated_distinct_endpoints.to_le_bytes());
            bytes.extend_from_slice(&rel.exact_distinct_endpoints.to_le_bytes());
            bytes.extend_from_slice(&rel.median_degree_upper.to_le_bytes());
            bytes.extend_from_slice(&rel.p99_degree_upper.to_le_bytes());
            bytes.extend_from_slice(&rel.max_degree.to_le_bytes());
            bytes.extend_from_slice(&rel.hll_relative_residual_ppm.to_le_bytes());
        }
        bytes
    }

    /// BLAKE3 digest of [`Self::canonical_bytes`].
    #[must_use]
    pub fn digest(&self) -> fgdb_crypto::Digest {
        let mut hasher = fgdb_crypto::Hasher::new();
        hasher.update(&self.canonical_bytes());
        hasher.finalize()
    }

    /// Printable-ASCII summary suitable for `EXPLAIN` rows.
    #[must_use]
    pub fn summary_ascii(&self) -> String {
        format!(
            "policy_epoch={} selected={} candidate={} fallback={} agm={}/{} vars={} edges={} factorized_suffix={} conforming={:?}",
            self.policy_epoch,
            self.selected_strategy.as_str(),
            self.candidate_strategy.as_str(),
            self.pinned_fallback.as_str(),
            self.agm_exponent.numerator,
            self.agm_exponent.denominator,
            self.variable_count,
            self.edge_atom_count,
            self.has_factorizable_suffix,
            self.conformal_conforming,
        )
    }
}

#[derive(Clone, Debug)]
struct EdgeAtom {
    source_slot: usize,
    target_slot: usize,
    var_u: JoinVariable,
    var_v: JoinVariable,
    relation: EdgeRelation,
    direction: GlaDirection,
    segment_slot: usize,
}

/// Compiled conjunctive multiway join lowered to calibrated `FreeJoin` over
/// [`SliceAdjacencyTrie`]s.
#[derive(Clone, Debug)]
pub struct CalibratedFreeJoin {
    slot_to_var: Vec<JoinVariable>,
    variables: Vec<JoinVariable>,
    scanned_vars: Vec<bool>,
    var_predicates: Vec<Vec<(usize, Vec<VertexPredicate>)>>,
    edge_atoms: Vec<EdgeAtom>,
    must_differ_from_earlier: Vec<Vec<usize>>,
    inequalities: Vec<(usize, usize)>,
    property_comparisons: Vec<usize>,
    projection_index: usize,

    distinct: bool,
    page_capacity: Option<u64>,
    agm_exponent: AgmExponent,
}

fn reverse_direction(direction: GlaDirection) -> GlaDirection {
    match direction {
        GlaDirection::Forward => GlaDirection::Reverse,
        GlaDirection::Reverse => GlaDirection::Forward,
        GlaDirection::Undirected => GlaDirection::Undirected,
    }
}

fn uf_root(parents: &[usize], mut slot: usize) -> usize {
    while parents[slot] != slot {
        slot = parents[slot];
    }
    slot
}

fn derived_oid(domain: &[u8], payload: &[u8]) -> ObjectId {
    let mut hasher = fgdb_crypto::Hasher::new();
    hasher.update(domain);
    hasher.update(payload);
    ObjectId(hasher.finalize().0)
}

/// Exact half-integral fractional edge-cover AGM bound for up to 10 binary/unary
/// atoms, with deterministic greedy fallback above 10 atoms.
fn compute_agm_exponent(
    variables: &[JoinVariable],
    edge_atoms: &[EdgeAtom],
    unary_vars: &[bool],
) -> AgmExponent {
    let n = variables.len();
    if n == 0 {
        return AgmExponent {
            numerator: 0,
            denominator: 1,
        };
    }
    let var_pos = |v: JoinVariable| {
        variables
            .binary_search(&v)
            .expect("variable in canonical list")
    };
    let mut atom_masks: Vec<u128> = Vec::with_capacity(edge_atoms.len() + n);
    for atom in edge_atoms {
        let u = var_pos(atom.var_u);
        let v = var_pos(atom.var_v);
        if u < 128 && v < 128 {
            atom_masks.push((1_u128 << u) | (1_u128 << v));
        }
    }
    // Ensure isolated scanned variables also have a covering unary atom.
    let mut covered_any = 0_u128;
    for &mask in &atom_masks {
        covered_any |= mask;
    }
    for (idx, &is_scanned) in unary_vars.iter().enumerate() {
        if idx < 128 && (covered_any & (1_u128 << idx)) == 0 && is_scanned {
            atom_masks.push(1_u128 << idx);
        }
    }

    if n <= 20 && !atom_masks.is_empty() && atom_masks.len() <= 10 {
        let m = atom_masks.len();
        let mut total_states = 1_u32;
        for _ in 0..m {
            total_states *= 3;
        }
        let mut best_twice = u32::MAX;
        let mut cov = [0_u8; 20];
        for state in 0..total_states {
            cov.fill(0);
            let mut rem = state;
            let mut sum_twice = 0_u32;
            for &mask in &atom_masks {
                let weight = (rem % 3) as u8;
                rem /= 3;
                if weight == 0 {
                    continue;
                }
                sum_twice += u32::from(weight);
                if sum_twice >= best_twice {
                    break;
                }
                let mut bits = mask;
                while bits != 0 {
                    let b = bits.trailing_zeros() as usize;
                    if b < n {
                        cov[b] = cov[b].saturating_add(weight);
                    }
                    bits &= bits - 1;
                }
            }
            if sum_twice < best_twice && cov[..n].iter().all(|&c| c >= 2) {
                best_twice = sum_twice;
            }
        }
        if best_twice < u32::MAX {
            return AgmExponent::reduced(best_twice, 2);
        }
    }

    // Deterministic greedy integral cover fallback for wide hypergraphs.
    let full_mask = if n >= 128 {
        u128::MAX
    } else {
        (1_u128 << n) - 1
    };
    let mut uncovered = full_mask;
    let mut chosen = 0_u32;
    while uncovered != 0 {
        let best_gain = atom_masks
            .iter()
            .map(|&mask| (mask & uncovered).count_ones())
            .max()
            .unwrap_or(0);
        if best_gain == 0 {
            chosen += uncovered.count_ones();
            break;
        }
        let pick = atom_masks
            .iter()
            .find(|&&mask| (mask & uncovered).count_ones() == best_gain)
            .copied()
            .unwrap_or(0);
        uncovered &= !pick;
        chosen += 1;
    }
    AgmExponent::reduced(chosen, 1)
}

impl CalibratedFreeJoin {
    /// Deterministic static template summary for `EXPLAIN` / `EXPLAIN (CERTIFICATE)`
    /// without reading snapshot records or consuming execution budget.
    #[must_use]
    pub fn template_summary_ascii(template_operators: &[&str]) -> Option<String> {
        if template_operators.iter().any(|&op| {
            matches!(
                op,
                "MatchScope"
                    | "VarLengthExpand"
                    | "CapturePath"
                    | "SelectPathLength"
                    | "SelectPathNull"
                    | "SelectBoolean"
                    | "FilterProperty"
                    | "FilterBoolean"
                    | "FilterWalkEdgeProperty"
                    | "FilterPathLength"
                    | "BindOuterVertex"
            )
        }) {
            return None;
        }
        let expand_count = template_operators
            .iter()
            .filter(|&&op| op == "Expand")
            .count();
        if expand_count < 3 {
            return None;
        }
        let has_different_edges = template_operators.contains(&"DifferentEdges");
        let has_predicates = template_operators.contains(&"Select")
            || template_operators.contains(&"CompareProperties");

        Some(format!(
            "policy_epoch={JOIN_DECISION_POLICY_EPOCH} family=CalibratedFreeJoin candidates=[WorstCaseOptimalGeneric,FactorizedFreeJoin,BinaryBushy] fallback=WorstCaseOptimalGeneric edges={expand_count} predicates={has_predicates} different_edges={has_different_edges}"
        ))
    }

    /// Compile a conjunctive multiway `GlaPlan` (`>= 3` distinct variables and
    /// `>= 3` fixed-hop edge atoms) into a [`CalibratedFreeJoin`] descriptor.
    ///
    /// Correlated subqueries (`Probe`, `Optional`, `Collect`), variable-length
    /// walks, captured paths, cross-slot boolean/property filters, and element
    /// metadata projections return `None` and stay on the existing evaluator.
    #[must_use]
    pub fn try_compile(operators: &[GlaOperator]) -> Option<Self> {
        let first = operators.first()?;
        if !matches!(
            first,
            GlaOperator::ScanEdges { .. } | GlaOperator::ScanVertices
        ) {
            return None;
        }

        let mut width = 0_usize;
        let mut scanned_slots = Vec::new();
        let mut raw_selects: Vec<(usize, usize, Vec<VertexPredicate>)> = Vec::new();
        let mut raw_edges: Vec<(usize, usize, EdgeRelation, GlaDirection, usize)> = Vec::new();
        let mut raw_equal: Vec<(usize, usize)> = Vec::new();
        let mut inequalities: Vec<(usize, usize)> = Vec::new();
        let mut property_comparisons: Vec<usize> = Vec::new();
        let mut diff_edge_groups: Vec<Vec<usize>> = Vec::new();

        let mut projection_index = None;

        for (at, operator) in operators.iter().enumerate() {
            match operator {
                GlaOperator::ScanEdges {
                    relation,
                    direction,
                } if at == 0 => {
                    width = 2;
                    raw_edges.push((0, 1, *relation, *direction, 1));
                }
                GlaOperator::ScanVertices if width < MAX_JOIN_VARIABLES => {
                    scanned_slots.push(width);
                    width += 1;
                }
                GlaOperator::Expand {
                    source,
                    relation,
                    direction,
                } if (source.ordinal() as usize) < width && width < MAX_JOIN_VARIABLES => {
                    let target = width;
                    width += 1;
                    raw_edges.push((
                        source.ordinal() as usize,
                        target,
                        *relation,
                        *direction,
                        target,
                    ));
                }
                GlaOperator::Select { slot, predicates } if (slot.ordinal() as usize) < width => {
                    raw_selects.push((at, slot.ordinal() as usize, predicates.clone()));
                }
                GlaOperator::VertexIdentity {
                    left,
                    right,
                    equal: true,
                } if (left.ordinal() as usize) < width && (right.ordinal() as usize) < width => {
                    raw_equal.push((left.ordinal() as usize, right.ordinal() as usize));
                }
                GlaOperator::VertexIdentity {
                    left,
                    right,
                    equal: false,
                } if (left.ordinal() as usize) < width && (right.ordinal() as usize) < width => {
                    inequalities.push((left.ordinal() as usize, right.ordinal() as usize));
                }
                GlaOperator::CompareProperties { left, right, .. }
                    if (left.ordinal() as usize) < width && (right.ordinal() as usize) < width =>
                {
                    property_comparisons.push(at);
                }
                GlaOperator::DifferentEdges { segments } => {
                    let group: Vec<usize> = segments.iter().map(|s| s.ordinal() as usize).collect();
                    if group.is_empty() {
                        return None;
                    }
                    diff_edge_groups.push(group);
                }
                GlaOperator::Project { slot } if (slot.ordinal() as usize) < width => {
                    projection_index = Some(at);
                    break;
                }
                GlaOperator::ProjectBindings { slots }
                    if !slots.is_empty()
                        && slots.iter().all(|slot| (slot.ordinal() as usize) < width) =>
                {
                    projection_index = Some(at);
                    break;
                }
                GlaOperator::ProjectValues { columns }
                    if !columns.is_empty()
                        && columns.iter().all(|column| match column {
                            ValueProjection::Vertex { slot }
                            | ValueProjection::Property { slot, .. } => {
                                (slot.ordinal() as usize) < width
                            }
                            _ => false,
                        }) =>
                {
                    projection_index = Some(at);
                    break;
                }
                _ => return None,
            }
        }

        let projection_index = projection_index?;
        if raw_edges.len() < 3 || raw_edges.len() > MAX_JOIN_RELATIONS {
            return None;
        }

        let mut tail = projection_index + 1;
        let distinct = matches!(operators.get(tail), Some(GlaOperator::Distinct));
        if distinct {
            tail += 1;
        }
        let ordered = matches!(
            (&operators[projection_index], operators.get(tail)),
            (
                GlaOperator::Project { .. },
                Some(GlaOperator::OrderByVertexId)
            ) | (
                GlaOperator::ProjectBindings { .. },
                Some(GlaOperator::OrderByBindings)
            ) | (
                GlaOperator::ProjectValues { .. },
                Some(GlaOperator::OrderByValues | GlaOperator::OrderByValueColumns { .. })
            )
        );
        if !ordered || operators.len() != tail + 2 {
            return None;
        }
        let GlaOperator::Limit { offset, count } = operators.last()? else {
            return None;
        };
        let page_capacity = if *count == Some(0) {
            Some(0)
        } else {
            count.and_then(|n| offset.checked_add(n))
        };

        // Build equivalence classes for `VertexIdentity { equal: true }`.
        let mut parents: Vec<usize> = (0..width).collect();
        for &(left, right) in &raw_equal {
            let a = uf_root(&parents, left);
            let b = uf_root(&parents, right);
            parents[a.max(b)] = a.min(b);
        }

        let mut slot_to_var = Vec::with_capacity(width);
        let mut variables = Vec::new();
        for slot in 0..width {
            let rep = uf_root(&parents, slot);
            let var = JoinVariable(rep as u32);
            slot_to_var.push(var);
            if rep == slot {
                variables.push(var);
            }
        }
        if variables.len() < 3 {
            return None;
        }

        // Reject trivially unsatisfiable `x <> x` before planning? Wait: if `x <> x`
        // occurs, `inequalities` check on bound `slots` cleanly rejects every binding
        // and returns empty rows, preserving exact semantics!
        let mut scanned_vars = vec![false; variables.len()];
        for slot in scanned_slots {
            let pos = variables
                .binary_search(&slot_to_var[slot])
                .expect("representative present");
            scanned_vars[pos] = true;
        }

        let mut var_predicates: Vec<Vec<(usize, Vec<VertexPredicate>)>> =
            vec![Vec::new(); variables.len()];
        for (op_idx, slot, preds) in raw_selects {
            if !preds.is_empty() {
                let pos = variables
                    .binary_search(&slot_to_var[slot])
                    .expect("representative present");
                var_predicates[pos].push((op_idx, preds));
            }
        }

        let mut edge_atoms = Vec::with_capacity(raw_edges.len());
        for (src_slot, dst_slot, relation, direction, segment_slot) in raw_edges {
            edge_atoms.push(EdgeAtom {
                source_slot: src_slot,
                target_slot: dst_slot,
                var_u: slot_to_var[src_slot],
                var_v: slot_to_var[dst_slot],
                relation,
                direction,
                segment_slot,
            });
        }

        // Map `DifferentEdges` segment slots onto `edge_atoms` indices.
        let mut must_differ_from_earlier: Vec<Vec<usize>> = vec![Vec::new(); edge_atoms.len()];
        for group in &diff_edge_groups {
            let mut atom_indices = Vec::with_capacity(group.len());
            for &seg_slot in group {
                let atom_pos = edge_atoms
                    .iter()
                    .position(|atom| atom.segment_slot == seg_slot)?;
                atom_indices.push(atom_pos);
            }
            atom_indices.sort_unstable();
            atom_indices.dedup();
            for i in 1..atom_indices.len() {
                let hi = atom_indices[i];
                for &lo in &atom_indices[..i] {
                    if !must_differ_from_earlier[hi].contains(&lo) {
                        must_differ_from_earlier[hi].push(lo);
                    }
                }
            }
        }
        for list in &mut must_differ_from_earlier {
            list.sort_unstable();
        }

        if edge_atoms.len() + variables.len() > MAX_JOIN_RELATIONS {
            return None;
        }

        let agm_exponent = compute_agm_exponent(&variables, &edge_atoms, &scanned_vars);

        Some(Self {
            slot_to_var,
            variables,
            scanned_vars,
            var_predicates,
            edge_atoms,
            must_differ_from_earlier,
            inequalities,
            property_comparisons,
            projection_index,

            distinct,
            page_capacity,
            agm_exponent,
        })
    }

    /// Execute the compiled multiway join under the standard governed policy
    /// and checkpoint seam, returning both the result rows/stats and the
    /// replayable [`JoinDecisionCard`].
    #[allow(clippy::too_many_arguments)]
    pub fn execute_governed<'a, Row, E, C>(
        &self,
        logical: &GlaPlan<Row>,
        snapshot_records: u64,
        vertices: impl IntoIterator<Item = VId>,
        edges: impl IntoIterator<Item = (EId, VId, RelationId, VId)>,
        test_vertex: impl FnMut(VId, &[VertexPredicate]) -> Result<bool, E>,
        property: impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalPropertyValue>, E>,
        policy: GqlQueryPolicy,
        checkpoint: impl GovernedCheckpoint<C>,
    ) -> Result<(GqlQueryExecution<Row>, JoinDecisionCard), GqlQueryError<E, C>>
    where
        Row: GlaOutput,
    {
        self.execute_governed_with_calibration(
            logical,
            snapshot_records,
            vertices,
            edges,
            test_vertex,
            property,
            &ConformalJoinCalibration::default(),
            policy,
            checkpoint,
        )
    }

    /// Execute with an explicit [`ConformalJoinCalibration`] history.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_governed_with_calibration<'a, Row, E, C>(
        &self,
        logical: &GlaPlan<Row>,
        snapshot_records: u64,
        vertices: impl IntoIterator<Item = VId>,
        edges: impl IntoIterator<Item = (EId, VId, RelationId, VId)>,
        mut test_vertex: impl FnMut(VId, &[VertexPredicate]) -> Result<bool, E>,
        mut property: impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalPropertyValue>, E>,
        calibration: &ConformalJoinCalibration,
        policy: GqlQueryPolicy,
        mut checkpoint: impl GovernedCheckpoint<C>,
    ) -> Result<(GqlQueryExecution<Row>, JoinDecisionCard), GqlQueryError<E, C>>
    where
        Row: GlaOutput,
    {
        checkpoint.unit().map_err(GqlQueryError::Interrupted)?;
        policy
            .rows
            .check(GqlBudgetDimension::SnapshotRecords, snapshot_records)
            .map_err(GqlQueryError::Rows)?;
        let mut meter = Charges::new(checkpoint, policy, snapshot_records);
        let outcome = self.run_metered(
            logical,
            vertices,
            edges,
            &mut test_vertex,
            &mut property,
            calibration,
            &mut |event| meter.event(event),
        );
        meter.settle()?;
        let (value, card) = outcome?;
        checkpoint_after_settle(&mut meter)?;
        debug_assert_eq!(
            u64::try_from(value.len()).ok(),
            Some(meter.rows.result_rows)
        );
        Ok((
            GqlQueryExecution {
                value,
                rows: meter.rows,
                evaluator: meter.evaluator,
            },
            card,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn run_metered<'a, Row, E, C, M>(
        &self,
        logical: &GlaPlan<Row>,
        vertices: impl IntoIterator<Item = VId>,
        edges: impl IntoIterator<Item = (EId, VId, RelationId, VId)>,
        test_vertex: &mut impl FnMut(VId, &[VertexPredicate]) -> Result<bool, E>,
        property: &mut impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalPropertyValue>, E>,
        calibration: &ConformalJoinCalibration,
        control: &mut M,
    ) -> Result<(Vec<Row>, JoinDecisionCard), GqlQueryError<E, C>>
    where
        Row: GlaOutput,
        M: FnMut(GlaExecutionEvent) -> Result<(), GqlQueryError<E, C>>,
    {
        // 1. Collect all required `(EdgeRelation, GlaDirection)` orientations,
        // including reverse orientations so sketch-guided variable ordering can
        // orient any binary atom in either direction without rescanning input.
        let mut orientation_keys: Vec<(EdgeRelation, GlaDirection)> = Vec::new();
        for atom in &self.edge_atoms {
            for dir in [atom.direction, reverse_direction(atom.direction)] {
                let key = (atom.relation, dir);
                if let Err(pos) = orientation_keys.binary_search(&key) {
                    control(GlaExecutionEvent::ScratchEntry)?;
                    orientation_keys.insert(pos, key);
                }
            }
        }

        let mut oriented_runs: Vec<Vec<(VId, VId, EId)>> = vec![Vec::new(); orientation_keys.len()];
        for (eid, src, rel, dst) in edges {
            control(GlaExecutionEvent::Work)?;
            for (idx, &(selector, dir)) in orientation_keys.iter().enumerate() {
                if !selector.matches(rel) {
                    continue;
                }
                let (from, to) = if dir == GlaDirection::Reverse {
                    (dst, src)
                } else {
                    (src, dst)
                };
                control(GlaExecutionEvent::ScratchEntry)?;
                oriented_runs[idx].push((from, to, eid));
                if dir == GlaDirection::Undirected && src != dst {
                    control(GlaExecutionEvent::ScratchEntry)?;
                    oriented_runs[idx].push((dst, src, eid));
                }
            }
        }
        for run in &mut oriented_runs {
            sort_triples(run, control)?;
        }

        // 2. Summarize each logical edge atom using `fgdb-sketch` (`DegreeHistogram`
        // + `DistinctSketch`).
        let mut relation_sketches = Vec::with_capacity(self.edge_atoms.len());
        let mut atom_forward_idx = Vec::with_capacity(self.edge_atoms.len());
        let mut atom_reverse_idx = Vec::with_capacity(self.edge_atoms.len());
        for (atom_ordinal, atom) in self.edge_atoms.iter().enumerate() {
            let fwd_idx = orientation_keys
                .binary_search(&(atom.relation, atom.direction))
                .expect("forward orientation registered");
            let rev_idx = orientation_keys
                .binary_search(&(atom.relation, reverse_direction(atom.direction)))
                .expect("reverse orientation registered");
            atom_forward_idx.push(fwd_idx);
            atom_reverse_idx.push(rev_idx);
            let summary = summarize_run(atom_ordinal as u32, &oriented_runs[fwd_idx], control)?;
            relation_sketches.push(summary);
        }

        // 3. Build unary vertex-domain tries for `ScanVertices` roots and
        // `Select`-filtered variables, pushing labels and `WHERE` predicates
        // directly into WCO leapfrog intersection BEFORE join expansion.
        let need_vertex_scan = self.scanned_vars.iter().any(|&b| b);
        let mut scanned_domain = Vec::new();
        if need_vertex_scan {
            for vid in vertices {
                control(GlaExecutionEvent::Work)?;
                control(GlaExecutionEvent::ScratchEntry)?;
                scanned_domain.push(vid);
            }
            sort_vids(&mut scanned_domain, control)?;
            scanned_domain.dedup();
        }

        let mut unary_domains: Vec<Option<Vec<VId>>> = vec![None; self.variables.len()];
        for (var_idx, &var) in self.variables.iter().enumerate() {
            let has_preds = !self.var_predicates[var_idx].is_empty();
            let is_scanned = self.scanned_vars[var_idx];
            if !has_preds && !is_scanned {
                continue;
            }
            let candidates: Vec<VId> = if is_scanned {
                scanned_domain.clone()
            } else {
                smallest_endpoint_domain(
                    var,
                    &self.edge_atoms,
                    &atom_forward_idx,
                    &atom_reverse_idx,
                    &oriented_runs,
                    control,
                )?
            };
            let mut filtered = Vec::new();
            for vid in candidates {
                control(GlaExecutionEvent::Work)?;
                let mut keep = true;
                for (_, preds) in &self.var_predicates[var_idx] {
                    for pred in preds {
                        for _ in 0..pred.comparison_work_units() {
                            control(GlaExecutionEvent::Work)?;
                        }
                    }
                    if !test_vertex(vid, preds).map_err(GqlQueryError::Source)? {
                        keep = false;
                        break;
                    }
                }
                if keep {
                    control(GlaExecutionEvent::ScratchEntry)?;
                    filtered.push(vid);
                }
            }
            unary_domains[var_idx] = Some(filtered);
        }

        // 4. Sketch-guided variable ordering + adaptive decision card via
        // `fgdb-calibrate` (`GraphMetricConformal` + `EProcessTrial`).
        let variable_order = self.choose_variable_order(&relation_sketches, &unary_domains);
        let has_different_edges = self.must_differ_from_earlier.iter().any(|v| !v.is_empty());
        let has_predicates = self.var_predicates.iter().any(|v| !v.is_empty())
            || !self.property_comparisons.is_empty();
        let has_factorizable_suffix = !has_different_edges
            && self.inequalities.is_empty()
            && self.property_comparisons.is_empty()
            && suffix_disconnects(&variable_order, &self.edge_atoms);

        let (card, physical) = self.build_decision_and_plan(
            &variable_order,
            &unary_domains,
            relation_sketches,
            has_predicates,
            has_different_edges,
            has_factorizable_suffix,
            calibration,
            control,
        )?;

        // 5. Materialize zero-`BTreeMap` `SliceAdjacencyTrie`s matching
        // `physical.required_order(rel)`.
        let mut tries = Vec::with_capacity(physical.relation_count());
        for (atom_idx, atom) in self.edge_atoms.iter().enumerate() {
            control(GlaExecutionEvent::ScratchEntry)?;
            let req = physical
                .required_order(atom_idx)
                .expect("one schema per edge atom");
            let trie = if req.len() == 1 {
                let pairs = extract_pairs(&oriented_runs[atom_forward_idx[atom_idx]], control)?;
                admitted_trie(SliceAdjacencyTrie::diagonal_from_sorted_pairs(
                    req[0], &pairs, 0, control,
                ))?
            } else {
                let run_idx = if req[0] == atom.var_u && req[1] == atom.var_v {
                    atom_forward_idx[atom_idx]
                } else {
                    debug_assert_eq!((req[0], req[1]), (atom.var_v, atom.var_u));
                    atom_reverse_idx[atom_idx]
                };
                let pairs = extract_pairs(&oriented_runs[run_idx], control)?;
                admitted_trie(SliceAdjacencyTrie::from_sorted_pairs(
                    [req[0], req[1]],
                    &pairs,
                    0,
                    control,
                ))?
            };
            tries.push(trie);
        }
        for (var_idx, maybe_domain) in unary_domains.iter().enumerate() {
            if let Some(domain) = maybe_domain {
                control(GlaExecutionEvent::ScratchEntry)?;
                let trie = admitted_trie(SliceAdjacencyTrie::from_sorted_vertices(
                    self.variables[var_idx],
                    domain,
                    0,
                    control,
                ))?;
                tries.push(trie);
            }
        }

        let join = FreeJoin::new(&physical, &tries)
            .expect("SliceAdjacencyTrie orders match FreeJoinPlan::required_order");
        let operators = logical.operators();
        let mut projected = ProjectedRows::<Row>::for_plan(self.distinct, operators);
        let mut bound_slots = [None; MAX_JOIN_VARIABLES];

        if card.selected_strategy == JoinStrategy::FactorizedFreeJoin {
            // Genuine `FactorizedBatch` intermediate execution for patterns with
            // independent suffix components.
            let batch: FactorizedBatch<VId> = join.factorize(control)?;
            if let Ok(mut cursor) = batch.cursor() {
                loop {
                    let cols = cursor.next_batch(256, control).map_err(|err| match err {
                        super::FactorizedReadError::Control(e) => e,
                        super::FactorizedReadError::Failed => {
                            unreachable!("fresh cursor has not failed")
                        }
                    })?;
                    if cols.row_count() == 0 {
                        break;
                    }
                    let batch_vars = cols.variables();
                    let batch_cols = cols.columns();
                    for row_idx in 0..cols.row_count() {
                        for (slot_at, &var) in self.slot_to_var.iter().enumerate() {
                            control(GlaExecutionEvent::Work)?;
                            let var_col = batch_vars
                                .iter()
                                .position(|&v| v == var)
                                .expect("factorized batch binds every plan variable");
                            bound_slots[slot_at] = Some(batch_cols[var_col][row_idx]);
                        }
                        control(GlaExecutionEvent::Work)?;
                        Row::collect_properties(
                            &operators[self.projection_index],
                            &bound_slots[..self.slot_to_var.len()],
                            &mut projected,
                            &mut |vid, key| property(vid, key).map_err(GqlQueryError::Source),
                            control,
                        )?;
                    }
                }
            } else {
                self.emit_streaming_bindings(
                    &join,
                    operators,
                    has_different_edges,
                    &atom_forward_idx,
                    &oriented_runs,
                    &mut bound_slots,
                    &mut projected,
                    property,
                    control,
                )?;
            }
        } else {
            self.emit_streaming_bindings(
                &join,
                operators,
                has_different_edges,
                &atom_forward_idx,
                &oriented_runs,
                &mut bound_slots,
                &mut projected,
                property,
                control,
            )?;
        }

        let Some(GlaOperator::Limit { offset, count }) = operators.last() else {
            unreachable!("validated terminal Limit in try_compile")
        };
        let offset = usize::try_from(*offset).unwrap_or(usize::MAX);
        let count = count
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(usize::MAX);
        let mut value = Vec::new();
        for row in projected.into_rows().skip(offset).take(count) {
            control(GlaExecutionEvent::ResultRow)?;
            value.push(row.retain_visible(logical.visible_columns));
        }
        Ok((value, card))
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_streaming_bindings<'a, Row, E, C, M>(
        &self,
        join: &FreeJoin<'_, VId, SliceAdjacencyTrie>,
        operators: &[GlaOperator],
        has_different_edges: bool,
        atom_forward_idx: &[usize],
        oriented_runs: &[Vec<(VId, VId, EId)>],
        bound_slots: &mut [Option<VId>; MAX_JOIN_VARIABLES],
        projected: &mut ProjectedRows<Row>,
        property: &mut impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalPropertyValue>, E>,
        control: &mut M,
    ) -> Result<(), GqlQueryError<E, C>>
    where
        Row: GlaOutput,
        M: FnMut(GlaExecutionEvent) -> Result<(), GqlQueryError<E, C>>,
    {
        join.for_each_binding(control, |binding, control| {
            for (slot_at, &var) in self.slot_to_var.iter().enumerate() {
                control(GlaExecutionEvent::Work)?;
                bound_slots[slot_at] = Some(*binding.get(var).expect("complete physical binding"));
            }
            for &(left, right) in &self.inequalities {
                control(GlaExecutionEvent::Work)?;
                if bound_slots[left] == bound_slots[right] {
                    return Ok(());
                }
            }
            for &op_idx in &self.property_comparisons {
                let matched = crate::algebra_exec::compare_properties(
                    &operators[op_idx],
                    &bound_slots[..self.slot_to_var.len()],
                    &mut |vid, key| property(vid, key).map_err(GqlQueryError::Source),
                    control,
                )?;
                if !matched {
                    return Ok(());
                }
            }

            let mut emit = |control: &mut M| {
                control(GlaExecutionEvent::Work)?;
                Row::collect_properties(
                    &operators[self.projection_index],
                    &bound_slots[..self.slot_to_var.len()],
                    projected,
                    &mut |vid, key| property(vid, key).map_err(GqlQueryError::Source),
                    control,
                )
            };

            if has_different_edges {
                let cap = if self.distinct {
                    Some(1_u128)
                } else {
                    self.page_capacity.map(u128::from)
                };
                if cap == Some(0) {
                    // `LIMIT 0` still validates at least one witness before
                    // reading projected properties for error-precedence parity!
                    let exists = self.count_different_edge_witnesses(
                        bound_slots,
                        atom_forward_idx,
                        oriented_runs,
                        Some(1),
                        control,
                    )?;
                    if exists > 0 {
                        emit(control)?;
                    }
                } else {
                    let count = self.count_different_edge_witnesses(
                        bound_slots,
                        atom_forward_idx,
                        oriented_runs,
                        cap,
                        control,
                    )?;
                    for _ in 0..count {
                        emit(control)?;
                    }
                }
            } else if self.distinct {
                emit(control)?;
            } else if let Some(capacity) = self.page_capacity {
                if capacity == 0 {
                    if !binding.multiplicity_factors().contains(&0) {
                        emit(control)?;
                    }
                } else {
                    let copies =
                        binding
                            .multiplicity_factors()
                            .iter()
                            .fold(1_u128, |n, &weight| {
                                n.checked_mul(weight as u128)
                                    .unwrap_or(u128::from(capacity))
                                    .min(u128::from(capacity))
                            });
                    for _ in 0..copies {
                        emit(control)?;
                    }
                }
            } else {
                binding.for_each_occurrence(control, |_, control| emit(control))?;
            }
            Ok(())
        })
    }

    fn count_different_edge_witnesses<M, Err>(
        &self,
        bound_slots: &[Option<VId>],
        atom_forward_idx: &[usize],
        oriented_runs: &[Vec<(VId, VId, EId)>],
        cap: Option<u128>,
        control: &mut M,
    ) -> Result<u128, Err>
    where
        M: FnMut(GlaExecutionEvent) -> Result<(), Err>,
    {
        let m = self.edge_atoms.len();
        let mut eid_slices: Vec<&[(VId, VId, EId)]> = Vec::with_capacity(m);
        for (atom_idx, atom) in self.edge_atoms.iter().enumerate() {
            control(GlaExecutionEvent::Work)?;
            let u = bound_slots[atom.source_slot].expect("bound source");
            let v = bound_slots[atom.target_slot].expect("bound target");
            let slice = lookup_eid_slice(&oriented_runs[atom_forward_idx[atom_idx]], u, v);
            if slice.is_empty() {
                return Ok(0);
            }
            eid_slices.push(slice);
        }

        // Fast O(m^2) disjointness check: if no constrained pair shares an EId,
        // every tuple in the Cartesian product of parallel EId lists is valid.
        let mut any_overlap = false;
        'outer: for hi in 1..m {
            for &lo in &self.must_differ_from_earlier[hi] {
                control(GlaExecutionEvent::Work)?;
                if slices_share_eid(eid_slices[lo], eid_slices[hi]) {
                    any_overlap = true;
                    break 'outer;
                }
            }
        }
        if !any_overlap {
            let mut prod = 1_u128;
            for slice in &eid_slices {
                prod = prod.saturating_mul(slice.len() as u128);
                if let Some(limit) = cap
                    && prod >= limit
                {
                    return Ok(limit);
                }
            }
            return Ok(prod);
        }

        // Exact backtracking over `eid_slices` enforcing `must_differ_from_earlier`.
        let mut chosen = vec![EId(0); m];
        let mut count = 0_u128;
        backtrack_eids(
            0,
            &eid_slices,
            &self.must_differ_from_earlier,
            &mut chosen,
            &mut count,
            cap,
            control,
        )?;
        Ok(count)
    }

    fn choose_variable_order(
        &self,
        sketches: &[RelationSketchSummary],
        unary_domains: &[Option<Vec<VId>>],
    ) -> Vec<JoinVariable> {
        // Score each variable deterministically:
        // 1. Prefer variables participating in >= 2 binary atoms (cycle/join hubs)
        //    with the smallest effective domain (unary filtered count or smallest
        //    incident relation `distinct_pairs`) so WCO leapfrog prunes maximally
        //    at shallow depth and leaves degree-1 tree tails at the suffix for
        //    `FactorizedBatch`!
        let mut remaining: Vec<usize> = (0..self.variables.len()).collect();
        let mut ordered = Vec::with_capacity(self.variables.len());
        let mut bound_mask = vec![false; self.variables.len()];

        while !remaining.is_empty() {
            let best_pos = remaining
                .iter()
                .enumerate()
                .min_by_key(|&(_, &var_idx)| {
                    let var = self.variables[var_idx];
                    let mut degree = 0_u32;
                    let mut bound_neighbors = 0_u32;
                    let mut min_pairs = u64::MAX;
                    for (atom_idx, atom) in self.edge_atoms.iter().enumerate() {
                        if atom.var_u == var || atom.var_v == var {
                            degree += 1;
                            min_pairs = min_pairs.min(sketches[atom_idx].distinct_pairs);
                            let other = if atom.var_u == var {
                                atom.var_v
                            } else {
                                atom.var_u
                            };
                            if let Ok(other_idx) = self.variables.binary_search(&other)
                                && bound_mask[other_idx]
                            {
                                bound_neighbors += 1;
                            }
                        }
                    }
                    if let Some(Some(dom)) = unary_domains.get(var_idx) {
                        min_pairs = min_pairs.min(dom.len() as u64);
                    }
                    let is_leaf = u8::from(degree <= 1);
                    (
                        is_leaf,
                        core::cmp::Reverse(bound_neighbors),
                        min_pairs,
                        core::cmp::Reverse(degree),
                        var.0,
                    )
                })
                .map(|(pos, _)| pos)
                .expect("remaining non-empty");
            let chosen_idx = remaining.remove(best_pos);
            bound_mask[chosen_idx] = true;
            ordered.push(self.variables[chosen_idx]);
        }
        ordered
    }

    #[allow(clippy::too_many_arguments)]
    fn build_decision_and_plan<M, Err>(
        &self,
        variable_order: &[JoinVariable],
        unary_domains: &[Option<Vec<VId>>],
        relation_sketches: Vec<RelationSketchSummary>,
        has_predicates: bool,
        has_different_edges: bool,
        has_factorizable_suffix: bool,
        calibration: &ConformalJoinCalibration,
        control: &mut M,
    ) -> Result<(JoinDecisionCard, FreeJoinPlan), Err>
    where
        M: FnMut(GlaExecutionEvent) -> Result<(), Err>,
    {
        let candidate_strategies = vec![
            JoinStrategy::WorstCaseOptimalGeneric,
            JoinStrategy::FactorizedFreeJoin,
            JoinStrategy::BinaryBushy,
        ];
        let (candidate_strategy, pinned_fallback) = if has_factorizable_suffix {
            (
                JoinStrategy::FactorizedFreeJoin,
                JoinStrategy::WorstCaseOptimalGeneric,
            )
        } else {
            (
                JoinStrategy::WorstCaseOptimalGeneric,
                JoinStrategy::BinaryBushy,
            )
        };

        // Compute observed HLL relative residual across relation sketches.
        let max_residual_ppm = relation_sketches
            .iter()
            .map(|s| s.hll_relative_residual_ppm)
            .max()
            .unwrap_or(0);
        let observed_residual = calibration
            .observed_residual_override
            .unwrap_or_else(|| (max_residual_ppm as f64) / 1_000_000.0);

        // Conformal assessment via `fgdb-calibrate::conformal`.
        let cal_samples = if calibration.calibration_residuals.is_empty() {
            &DEFAULT_BASELINE_RESIDUALS[..]
        } else {
            calibration.calibration_residuals.as_slice()
        };
        let total_window = (cal_samples.len() as u64) + 1;
        let conf_window =
            ConformalWindow::try_new(1, total_window).expect("valid conformal window");
        let metric_oid = derived_oid(b"fgdb:loom:hll-residual-metric:v1\0", &[]);
        let pop_oid = derived_oid(
            b"fgdb:loom:join-population:v1\0",
            &(self.edge_atoms.len() as u32).to_le_bytes(),
        );
        let sel_oid = derived_oid(b"fgdb:loom:join-selection:v1\0", &[]);
        let win_oid = derived_oid(b"fgdb:loom:join-window:v1\0", &total_window.to_le_bytes());
        let conf_identity = GraphMetricIdentity::try_new(
            metric_oid,
            pop_oid,
            sel_oid,
            win_oid,
            conf_window,
            JOIN_DECISION_POLICY_EPOCH,
            candidate_strategy.policy_oid(),
            pinned_fallback.policy_oid(),
        )
        .expect("distinct candidate and fallback strategy OIDs");
        let min_cal = cal_samples.len().clamp(1, 4);
        let max_cal = cal_samples.len().max(min_cal);
        let conf_profile = ConformalProfile::try_new(
            derived_oid(b"fgdb:loom:conformal-profile:v1\0", &[]),
            DEFAULT_CONFORMAL_ALPHA,
            MetricThresholdMode::Upper,
            min_cal,
            max_cal,
        )
        .expect("valid conformal profile");
        let mut conformal =
            GraphMetricConformal::try_new(conf_identity, conf_profile).expect("valid calibrator");
        for (idx, &sample) in cal_samples.iter().enumerate() {
            control(GlaExecutionEvent::Work)?;
            let finite = if sample.is_finite() { sample } else { 0.0 };
            let _ = conformal.calibrate(SequencedMetricValue::new(
                conf_identity,
                conf_profile,
                (idx as u64) + 1,
                finite,
            ));
        }
        let assessment = conformal
            .assess(SequencedMetricValue::new(
                conf_identity,
                conf_profile,
                total_window,
                observed_residual,
            ))
            .expect("valid assessment envelope");

        // Anytime-valid drift monitor via `fgdb-calibrate::eprocess`.
        // Here `One` denotes an anomalous sketch-drift event; rejecting the
        // stationary null (`p0 = 0.05`) signals regime shift and forces
        // conservative fallback!
        let drift_obs: &[BinaryObservation] = if calibration.drift_observations.is_empty() {
            if assessment.selection() == PolicySelection::CandidateDecision {
                &[BinaryObservation::Zero]
            } else {
                &[BinaryObservation::One]
            }
        } else {
            calibration.drift_observations.as_slice()
        };
        let ep_window =
            EProcessWindow::try_new(1, drift_obs.len() as u64).expect("non-empty drift window");
        let drift_candidate_oid = derived_oid(b"fgdb:loom:drift-alert:v1\0", &[1]);
        let drift_null_oid = derived_oid(b"fgdb:loom:drift-stationary:v1\0", &[0]);
        let ep_identity = TrialIdentity::try_new(
            derived_oid(b"fgdb:loom:drift-monitor:v1\0", &[]),
            derived_oid(b"fgdb:loom:drift-filtration:v1\0", &[]),
            ep_window,
            JOIN_DECISION_POLICY_EPOCH,
            drift_candidate_oid,
            drift_null_oid,
        )
        .expect("valid eprocess identity");
        let ep_profile = EProcessProfile::try_new(
            derived_oid(b"fgdb:loom:eprocess-profile:v1\0", &[]),
            EProcessConfig {
                p0: 0.05,
                lambda: 0.5,
                alpha: 0.05,
                max_evalue: 1_000_000.0,
            },
        )
        .expect("valid eprocess config");
        let mut ep_trial =
            EProcessTrial::try_new(ep_identity, ep_profile).expect("valid eprocess trial");
        for (idx, &obs) in drift_obs.iter().enumerate() {
            control(GlaExecutionEvent::Work)?;
            let _ = ep_trial.observe(SequencedObservation::new(
                ep_identity,
                ep_profile,
                (idx as u64) + 1,
                obs,
            ));
        }
        let ep_evidence = ep_trial.evidence();
        let drift_rejected_null = ep_evidence.first_rejection_sequence().is_some();

        let selected_strategy = if assessment.selection() == PolicySelection::CandidateDecision
            && !drift_rejected_null
        {
            candidate_strategy
        } else {
            pinned_fallback
        };

        // Construct `schemas` matching `self.edge_atoms` + unary domains.
        let mut schemas = Vec::with_capacity(self.edge_atoms.len() + unary_domains.len());
        for atom in &self.edge_atoms {
            control(GlaExecutionEvent::ScratchEntry)?;
            if atom.var_u == atom.var_v {
                schemas.push(vec![atom.var_u]);
            } else {
                schemas.push(vec![atom.var_u.min(atom.var_v), atom.var_u.max(atom.var_v)]);
            }
        }
        for (var_idx, maybe_dom) in unary_domains.iter().enumerate() {
            if maybe_dom.is_some() {
                control(GlaExecutionEvent::ScratchEntry)?;
                schemas.push(vec![self.variables[var_idx]]);
            }
        }

        for _ in variable_order {
            for _ in &schemas {
                control(GlaExecutionEvent::ScratchEntry)?;
            }
        }

        let physical = match selected_strategy {
            JoinStrategy::WorstCaseOptimalGeneric | JoinStrategy::FactorizedFreeJoin => {
                FreeJoinPlan::generic(schemas, variable_order.to_vec())
                    .expect("validated schemas cover every variable")
            }
            JoinStrategy::BinaryBushy => build_binary_bushy_plan(schemas, variable_order),
        };

        let card = JoinDecisionCard {
            policy_epoch: JOIN_DECISION_POLICY_EPOCH,
            variable_count: self.variables.len() as u32,
            edge_atom_count: self.edge_atoms.len() as u32,
            has_predicates,
            has_different_edges,
            has_factorizable_suffix,
            agm_exponent: self.agm_exponent,
            candidate_strategies,
            candidate_strategy,
            pinned_fallback,
            selected_strategy,
            conformal_conforming: assessment.conforming(),
            conformal_threshold_bits: assessment.threshold_bits(),
            nonconformity_score_bits: assessment.nonconformity_score_bits(),
            drift_e_value_bits: ep_evidence.e_value_bits(),
            drift_rejected_null,
            variable_order: physical.variables().to_vec(),
            relation_sketches,
        };
        Ok((card, physical))
    }
}

fn checkpoint_after_settle<E, C, K: GovernedCheckpoint<C>>(
    meter: &mut Charges<K>,
) -> Result<(), GqlQueryError<E, C>> {
    meter.finish_unit()
}

fn build_binary_bushy_plan(
    schemas: Vec<Vec<JoinVariable>>,
    variable_order: &[JoinVariable],
) -> FreeJoinPlan {
    // Find the first binary schema in `variable_order` priority to form a 2-var
    // seed group `[u0, v0]`, followed by singleton stages for remaining vars.
    let mut seed_pair: Option<[JoinVariable; 2]> = None;
    'search: for &u in variable_order {
        for &v in variable_order {
            if u == v {
                continue;
            }
            let target = [u.min(v), u.max(v)];
            if schemas
                .iter()
                .any(|s| s.len() == 2 && s[0] == target[0] && s[1] == target[1])
            {
                seed_pair = Some([u, v]);
                break 'search;
            }
        }
    }
    if let Some([u, v]) = seed_pair {
        let mut groups = Vec::with_capacity(variable_order.len() - 1);
        groups.push(vec![u, v]);
        for &w in variable_order {
            if w != u && w != v {
                groups.push(vec![w]);
            }
        }
        if let Ok(plan) = FreeJoinPlan::new(schemas.clone(), groups) {
            return plan;
        }
    }
    FreeJoinPlan::generic(schemas, variable_order.to_vec())
        .expect("generic plan always covers validated schemas")
}

fn suffix_disconnects(variable_order: &[JoinVariable], edge_atoms: &[EdgeAtom]) -> bool {
    if variable_order.len() < 4 {
        return false;
    }
    // Check whether removing the first `prefix_len` variables splits the
    // remaining variables into >= 2 contiguous independent components!
    for prefix_len in 1..(variable_order.len() - 1) {
        let suffix = &variable_order[prefix_len..];
        if suffix.len() < 2 {
            continue;
        }
        let mut max_reach = 0_usize;
        for (idx, &var) in suffix.iter().enumerate() {
            for atom in edge_atoms {
                let other = if atom.var_u == var {
                    Some(atom.var_v)
                } else if atom.var_v == var {
                    Some(atom.var_u)
                } else {
                    None
                };
                if let Some(other_var) = other
                    && let Some(pos) = suffix.iter().position(|&v| v == other_var)
                {
                    max_reach = max_reach.max(pos);
                }
            }
            if max_reach == idx && idx + 1 < suffix.len() {
                return true;
            }
        }
    }
    false
}

fn summarize_run<M, Err>(
    relation_ordinal: u32,
    run: &[(VId, VId, EId)],
    control: &mut M,
) -> Result<RelationSketchSummary, Err>
where
    M: FnMut(GlaExecutionEvent) -> Result<(), Err>,
{
    let max_obs = (run.len() as u64).saturating_add(1).max(16);
    let mut hist = DegreeHistogram::new(max_obs);
    let mut hll = DistinctSketch::try_new(DistinctProfile::new(
        SKETCH_PRECISION,
        SKETCH_SEED_BASE ^ u64::from(relation_ordinal),
    ))
    .expect("valid fixed precision 8");

    let mut distinct_sources = 0_u64;
    let mut distinct_pairs = 0_u64;
    let mut max_degree = 0_u64;
    let mut exact_endpoints: Vec<VId> = Vec::new();

    let mut idx = 0_usize;
    while idx < run.len() {
        control(GlaExecutionEvent::Work)?;
        let src = run[idx].0;
        let _ = hll.observe(&src.0.to_le_bytes());
        exact_endpoints.push(src);
        let mut out_distinct_neighbors = 0_u64;
        let mut prev_dst: Option<VId> = None;
        while idx < run.len() && run[idx].0 == src {
            let dst = run[idx].1;
            if prev_dst != Some(dst) {
                out_distinct_neighbors += 1;
                distinct_pairs += 1;
                let _ = hll.observe(&dst.0.to_le_bytes());
                exact_endpoints.push(dst);
                prev_dst = Some(dst);
            }
            idx += 1;
        }
        distinct_sources += 1;
        max_degree = max_degree.max(out_distinct_neighbors);
        let _ = hist.try_observe(out_distinct_neighbors);
    }

    exact_endpoints.sort_unstable();
    exact_endpoints.dedup();
    let exact_distinct_endpoints = exact_endpoints.len() as u64;
    let estimated_distinct_endpoints = if exact_distinct_endpoints == 0 {
        0
    } else {
        hll.estimate_fixed().rounded()
    };
    let hll_relative_residual_ppm = if exact_distinct_endpoints < 16 {
        // Small exact populations have bounded discrete quantization; avoid
        // mistaking 1-register granularity on tiny toy graphs for skew drift.
        0
    } else {
        let diff = estimated_distinct_endpoints.abs_diff(exact_distinct_endpoints);
        u64::try_from(
            (u128::from(diff) * 1_000_000_u128) / u128::from(exact_distinct_endpoints.max(1)),
        )
        .unwrap_or(u64::MAX)
    };

    let median_degree_upper = if hist.is_empty() {
        0
    } else {
        hist.select_bucket(hist.len() / 2)
            .map_or(0, |b| b.upper.min(max_degree))
    };
    let p99_degree_upper = if hist.is_empty() {
        0
    } else {
        let ord = ((u128::from(hist.len()) * 99) / 100) as u64;
        hist.select_bucket(ord.min(hist.len() - 1))
            .map_or(0, |b| b.upper.min(max_degree))
    };

    Ok(RelationSketchSummary {
        relation_ordinal,
        total_edges: run.len() as u64,
        distinct_sources,
        distinct_pairs,
        estimated_distinct_endpoints,
        exact_distinct_endpoints,
        median_degree_upper,
        p99_degree_upper,
        max_degree,
        hll_relative_residual_ppm,
    })
}

fn smallest_endpoint_domain<M, Err>(
    var: JoinVariable,
    edge_atoms: &[EdgeAtom],
    atom_forward_idx: &[usize],
    atom_reverse_idx: &[usize],
    oriented_runs: &[Vec<(VId, VId, EId)>],
    control: &mut M,
) -> Result<Vec<VId>, Err>
where
    M: FnMut(GlaExecutionEvent) -> Result<(), Err>,
{
    let mut best_run: Option<&[(VId, VId, EId)]> = None;
    for (atom_idx, atom) in edge_atoms.iter().enumerate() {
        let candidate_idx = if atom.var_u == var {
            Some(atom_forward_idx[atom_idx])
        } else if atom.var_v == var {
            Some(atom_reverse_idx[atom_idx])
        } else {
            None
        };
        if let Some(run_idx) = candidate_idx {
            let slice = oriented_runs[run_idx].as_slice();
            if best_run.is_none_or(|prev| slice.len() < prev.len()) {
                best_run = Some(slice);
            }
        }
    }
    let Some(run) = best_run else {
        return Ok(Vec::new());
    };
    let mut domain = Vec::new();
    for &(src, _, _) in run {
        control(GlaExecutionEvent::Work)?;
        if domain.last().copied() != Some(src) {
            control(GlaExecutionEvent::ScratchEntry)?;
            domain.push(src);
        }
    }
    Ok(domain)
}

fn extract_pairs<M, Err>(run: &[(VId, VId, EId)], control: &mut M) -> Result<Vec<(VId, VId)>, Err>
where
    M: FnMut(GlaExecutionEvent) -> Result<(), Err>,
{
    let mut pairs = Vec::with_capacity(run.len());
    for &(src, dst, _) in run {
        control(GlaExecutionEvent::Work)?;
        control(GlaExecutionEvent::ScratchEntry)?;
        pairs.push((src, dst));
    }
    Ok(pairs)
}

fn admitted_trie<T, E>(result: Result<T, GraphTrieError<E>>) -> Result<T, E> {
    match result {
        Ok(value) => Ok(value),
        Err(GraphTrieError::Control(err)) => Err(err),
        Err(GraphTrieError::UnsortedInput | GraphTrieError::RepeatedVariable) => {
            unreachable!("sorted runs and checked schemas preserve SliceAdjacencyTrie invariants")
        }
    }
}

fn lookup_eid_slice(run: &[(VId, VId, EId)], u: VId, v: VId) -> &[(VId, VId, EId)] {
    let start = run.partition_point(|&(src, dst, _)| (src, dst) < (u, v));
    let end = run[start..].partition_point(|&(src, dst, _)| (src, dst) == (u, v)) + start;
    &run[start..end]
}

fn slices_share_eid(left: &[(VId, VId, EId)], right: &[(VId, VId, EId)]) -> bool {
    let (mut i, mut j) = (0, 0);
    while i < left.len() && j < right.len() {
        match left[i].2.cmp(&right[j].2) {
            core::cmp::Ordering::Less => i += 1,
            core::cmp::Ordering::Greater => j += 1,
            core::cmp::Ordering::Equal => return true,
        }
    }
    false
}

fn backtrack_eids<M, Err>(
    depth: usize,
    eid_slices: &[&[(VId, VId, EId)]],
    must_differ_from_earlier: &[Vec<usize>],
    chosen: &mut [EId],
    count: &mut u128,
    cap: Option<u128>,
    control: &mut M,
) -> Result<(), Err>
where
    M: FnMut(GlaExecutionEvent) -> Result<(), Err>,
{
    if let Some(limit) = cap
        && *count >= limit
    {
        return Ok(());
    }
    if depth == eid_slices.len() {
        *count = count.saturating_add(1);
        return Ok(());
    }
    for &(_, _, eid) in eid_slices[depth] {
        control(GlaExecutionEvent::Work)?;
        let conflict = must_differ_from_earlier[depth]
            .iter()
            .any(|&prev| chosen[prev] == eid);
        if conflict {
            continue;
        }
        chosen[depth] = eid;
        backtrack_eids(
            depth + 1,
            eid_slices,
            must_differ_from_earlier,
            chosen,
            count,
            cap,
            control,
        )?;
        if let Some(limit) = cap
            && *count >= limit
        {
            return Ok(());
        }
    }
    Ok(())
}

fn sort_triples<M, Err>(values: &mut [(VId, VId, EId)], control: &mut M) -> Result<(), Err>
where
    M: FnMut(GlaExecutionEvent) -> Result<(), Err>,
{
    fn sift<M, Err>(
        values: &mut [(VId, VId, EId)],
        mut root: usize,
        end: usize,
        control: &mut M,
    ) -> Result<(), Err>
    where
        M: FnMut(GlaExecutionEvent) -> Result<(), Err>,
    {
        while root < end / 2 {
            let mut child = 2 * root + 1;
            if child + 1 < end {
                control(GlaExecutionEvent::Work)?;
                if values[child] < values[child + 1] {
                    child += 1;
                }
            }
            control(GlaExecutionEvent::Work)?;
            if values[root] >= values[child] {
                break;
            }
            control(GlaExecutionEvent::Work)?;
            values.swap(root, child);
            root = child;
        }
        Ok(())
    }
    let mut sorted = true;
    for w in values.windows(2) {
        control(GlaExecutionEvent::Work)?;
        if w[0] > w[1] {
            sorted = false;
            break;
        }
    }
    if sorted {
        return Ok(());
    }
    let n = values.len();
    for root in (0..n / 2).rev() {
        sift(values, root, n, control)?;
    }
    for end in (1..n).rev() {
        control(GlaExecutionEvent::Work)?;
        values.swap(0, end);
        sift(values, 0, end, control)?;
    }
    Ok(())
}

fn sort_vids<M, Err>(values: &mut [VId], control: &mut M) -> Result<(), Err>
where
    M: FnMut(GlaExecutionEvent) -> Result<(), Err>,
{
    fn sift<M, Err>(
        values: &mut [VId],
        mut root: usize,
        end: usize,
        control: &mut M,
    ) -> Result<(), Err>
    where
        M: FnMut(GlaExecutionEvent) -> Result<(), Err>,
    {
        while root < end / 2 {
            let mut child = 2 * root + 1;
            if child + 1 < end {
                control(GlaExecutionEvent::Work)?;
                if values[child] < values[child + 1] {
                    child += 1;
                }
            }
            control(GlaExecutionEvent::Work)?;
            if values[root] >= values[child] {
                break;
            }
            control(GlaExecutionEvent::Work)?;
            values.swap(root, child);
            root = child;
        }
        Ok(())
    }
    let mut sorted = true;
    for w in values.windows(2) {
        control(GlaExecutionEvent::Work)?;
        if w[0] > w[1] {
            sorted = false;
            break;
        }
    }
    if sorted {
        return Ok(());
    }
    let n = values.len();
    for root in (0..n / 2).rev() {
        sift(values, root, n, control)?;
    }
    for end in (1..n).rev() {
        control(GlaExecutionEvent::Work)?;
        values.swap(0, end);
        sift(values, 0, end, control)?;
    }
    Ok(())
}
