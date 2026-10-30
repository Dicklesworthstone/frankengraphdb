//! Private candidate keys for the existing transactional row-join derivative.
//!
//! A necessary ON equality narrows candidate groups but does not change the
//! definition, stored row, output domain, or predicate interpreter. Explicit
//! RowJoinSpec keys continue to use canonical typed identity without coercion.

use super::*;
use core::cmp::Ordering;
use core::num::NonZeroU64;
use fgdb_types::CanonicalScalar;

pub(super) enum Probe<'a> {
    Typed(&'a [(usize, usize)]),
    Predicate(Vec<(usize, usize)>),
}

impl<'a> Probe<'a> {
    pub(super) fn new<E>(
        spec: &'a RowJoinSpec,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<Self, ZSetError<E>> {
        if !spec.keys().is_empty() || spec.predicate().is_none() {
            // Keep explicit joins and unconditional products on their existing
            // key/callback contract. There is no speculative input inspection.
            return Ok(Self::Typed(spec.keys()));
        }
        let keys = spec.predicate_probe_keys_with_control(&mut |event| {
            charge(
                control,
                match event {
                    GlaExecutionEvent::ScratchEntry => ZSetEvent::ScratchEntry,
                    _ => ZSetEvent::Work,
                },
            )
        })?;
        Ok(Self::Predicate(keys))
    }

    pub(super) fn columns(&self) -> &[(usize, usize)] {
        match self {
            Self::Typed(keys) => keys,
            Self::Predicate(keys) => keys,
        }
    }

    /// The caller has already validated the COMPLETE original row. Reserve
    /// this private cell before cloning payloads; original row ownership is
    /// admitted separately and is never replaced by the normalized key.
    pub(super) fn copy_cell<E>(
        &self,
        value: &GraphValue,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<GraphValue, ZSetError<E>> {
        if matches!(self, Self::Predicate(_)) {
            if matches!(
                value,
                GraphValue::Scalar(_) | GraphValue::Vertex(_) | GraphValue::Edge(_)
            ) {
                // Preserve the existing atomic-key event contract. A scalar
                // payload needs one admitted copy, not a self-comparison and
                // another normalization traversal over its bytes.
                charge(control, ZSetEvent::Work)?;
                if let GraphValue::Scalar(CanonicalScalar::Float(number)) = value
                    && let Some(candidate) = exact_integer_peer(*number)
                {
                    let value = GraphValue::Scalar(CanonicalScalar::Int(candidate));
                    reserve_cell(&value, control)?;
                    return Ok(value);
                }
                reserve_cell(value, control)?;
                return Ok(value.clone());
            }
            // A nested NULL cannot supply an equality witness, but retaining
            // its original row is necessary for outer/anti joins. The shared
            // eager comparison governs every child before selecting a private
            // side-specific sentinel. Canonical NaN retains its native law.
            let reflexive =
                crate::set_ops::projection::membership::equal(value, value, &mut |event| {
                    charge_gla(control, event)
                })? == Some(true);
            if !reflexive {
                let null = GraphValue::Scalar(CanonicalScalar::Null);
                reserve_cell(&null, control)?;
                return Ok(null);
            }
            let mut value = value.copy_with_control(&mut |event| charge_gla(control, event))?;
            normalize(&mut value, control)?;
            return Ok(value);
        }
        reserve_cell(value, control)?;
        Ok(value.clone())
    }
}

fn exact_integer_peer(number: fgdb_types::CanonicalF64) -> Option<i64> {
    // A cast supplies only a candidate. The EXACT comparison proves it,
    // keeping fractions, infinities, NaN, >2^53 rounding and the saturated
    // i64 boundary distinct from a false integer peer.
    let candidate = number.get() as i64;
    (number.compare_rational(i128::from(candidate), NonZeroU64::MIN) == Ordering::Equal)
        .then_some(candidate)
}

fn charge_gla<E>(
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    event: GlaExecutionEvent,
) -> Result<(), ZSetError<E>> {
    charge(
        control,
        match event {
            GlaExecutionEvent::ScratchEntry => ZSetEvent::ScratchEntry,
            _ => ZSetEvent::Work,
        },
    )
}

/// Normalize only the admitted private copy. Its depth has already passed
/// GraphValue::MAX_LIST_DEPTH, so recursion is bounded; original rows and
/// canonical storage/grouping identity remain untouched.
fn normalize<E>(
    value: &mut GraphValue,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<(), ZSetError<E>> {
    charge(control, ZSetEvent::Work)?;
    match value {
        GraphValue::Scalar(CanonicalScalar::Float(number)) => {
            if let Some(candidate) = exact_integer_peer(*number) {
                *value = GraphValue::Scalar(CanonicalScalar::Int(candidate));
            }
        }
        GraphValue::List(values) | GraphValue::Map { values, .. } => {
            for value in values {
                normalize(value, control)?;
            }
        }
        GraphValue::Vertices(ids) => {
            let mut values = Vec::new();
            for &id in ids.iter() {
                charge(control, ZSetEvent::Work)?;
                charge(control, ZSetEvent::ScratchEntry)?;
                values.push(GraphValue::Vertex(id));
            }
            *value = GraphValue::List(values.into_boxed_slice());
        }
        GraphValue::Edges(ids) => {
            let mut values = Vec::new();
            for &id in ids.iter() {
                charge(control, ZSetEvent::Work)?;
                charge(control, ZSetEvent::ScratchEntry)?;
                values.push(GraphValue::Edge(id));
            }
            *value = GraphValue::List(values.into_boxed_slice());
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GraphSetOperand;
    use crate::algebra::IntegerComparison;
    use fgdb_types::{CanonicalDecimal, CanonicalF64, EId, VId};

    const LIMBS: LimbLimit = LimbLimit::new(4);
    const KINDS: [RowJoinKind; 6] = [
        RowJoinKind::Inner,
        RowJoinKind::Left,
        RowJoinKind::Right,
        RowJoinKind::Full,
        RowJoinKind::Semi,
        RowJoinKind::Anti,
    ];
    type Rows = Vec<(GraphValueRow, i128)>;

    fn allow(_: ZSetEvent) -> Result<(), usize> {
        Ok(())
    }
    fn int(n: i64) -> GraphValue {
        GraphValue::Scalar(CanonicalScalar::Int(n))
    }
    fn float(n: f64) -> GraphValue {
        GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(n)))
    }
    fn null() -> GraphValue {
        GraphValue::Scalar(CanonicalScalar::Null)
    }
    fn row(values: Vec<GraphValue>) -> GraphValueRow {
        GraphValueRow::from_owned_values(values)
    }
    fn equal(a: usize, b: usize) -> GraphSetPredicateOp {
        GraphSetPredicateOp::Compare {
            left: GraphSetOperand::Column(a),
            comparison: IntegerComparison::Equal,
            right: GraphSetOperand::Column(b),
        }
    }
    fn definition(kind: RowJoinKind, residual: bool, indexed: bool) -> RowJoinSpec {
        let mut code = vec![equal(0, 2)];
        if !indexed {
            // The native proof deliberately does not infer through NOT.
            // Double NOT is the live unindexed implementation of the SAME
            // three-valued predicate, not a different reference matcher.
            code.extend([GraphSetPredicateOp::Not, GraphSetPredicateOp::Not]);
        }
        if residual {
            code.extend([
                GraphSetPredicateOp::Compare {
                    left: GraphSetOperand::Column(1),
                    comparison: IntegerComparison::Less,
                    right: GraphSetOperand::Column(3),
                },
                GraphSetPredicateOp::And,
            ]);
        }
        RowJoinSpec::cross(&[GraphSetColumnType::Any; 2], &[GraphSetColumnType::Any; 2])
            .unwrap()
            .with_kind(kind)
            .with_predicate(&code)
            .unwrap()
    }
    fn bag(rows: &Rows) -> ZSet<GraphValueRow> {
        ZSet::from_updates(
            rows.iter()
                .map(|(row, n)| (row.clone(), ZWeight::from_i128(*n))),
            LIMBS,
            &mut allow,
        )
        .unwrap()
    }
    fn changed(after: &Rows, before: &Rows) -> ZSet<GraphValueRow> {
        bag(&after
            .iter()
            .cloned()
            .chain(before.iter().map(|(row, n)| (row.clone(), -n)))
            .collect())
    }
    fn seeded(spec: RowJoinSpec, left: &Rows, right: &Rows) -> IncrementalRowJoin {
        let mut join = IncrementalRowJoin::new(spec);
        join.prepare(&bag(left), &bag(right), LIMBS, None, &mut allow)
            .unwrap()
            .commit();
        join
    }

    #[test]
    fn private_key_equivalence_matches_native_truth_without_rewriting_payloads() {
        let spec = RowJoinSpec::cross(&[GraphSetColumnType::Any], &[GraphSetColumnType::Any])
            .unwrap()
            .with_predicate(&[equal(0, 1)])
            .unwrap();
        let probe = Probe::new(&spec, &mut allow).unwrap();
        assert_eq!(probe.columns(), &[(0, 0)]);
        let values = vec![
            null(),
            int(0),
            int(7),
            int(-7),
            int(i64::MIN),
            int(i64::MAX),
            int(9_007_199_254_740_993),
            float(-0.0),
            float(7.0),
            float(-7.0),
            float(0.5),
            float(f64::from_bits(1)),
            float(-9_223_372_036_854_775_808.0),
            float(9_223_372_036_854_775_808.0),
            float(9_007_199_254_740_992.0),
            float(f64::NEG_INFINITY),
            float(f64::INFINITY),
            float(f64::NAN),
            GraphValue::Scalar(CanonicalScalar::Decimal(
                CanonicalDecimal::from_integer(7).unwrap(),
            )),
            GraphValue::Scalar(CanonicalScalar::Bool(true)),
            GraphValue::Scalar(CanonicalScalar::ucs_basic_text("7").unwrap()),
            GraphValue::Scalar(CanonicalScalar::bytes(vec![7]).unwrap()),
            GraphValue::Vertex(VId(0)),
            GraphValue::Vertex(VId(u128::MAX)),
            GraphValue::Edge(EId(0)),
            GraphValue::Edge(EId(u128::MAX)),
            GraphValue::List(vec![int(7)].into_boxed_slice()),
            GraphValue::List(vec![float(7.0)].into_boxed_slice()),
            GraphValue::List(vec![null()].into_boxed_slice()),
            GraphValue::List(vec![int(9_007_199_254_740_993)].into_boxed_slice()),
            GraphValue::List(vec![float(9_007_199_254_740_992.0)].into_boxed_slice()),
            GraphValue::List(vec![GraphValue::Vertex(VId(7))].into_boxed_slice()),
            GraphValue::List(vec![GraphValue::Edge(EId(7))].into_boxed_slice()),
            GraphValue::Vertices(vec![VId(7)].into_boxed_slice()),
            GraphValue::Edges(vec![EId(7)].into_boxed_slice()),
            GraphValue::Vertices(Box::new([])),
            GraphValue::Edges(Box::new([])),
            GraphValue::List(Box::new([])),
            GraphValue::map(vec![("key".into(), int(7))]).unwrap(),
            GraphValue::map(vec![("key".into(), float(7.0))]).unwrap(),
            GraphValue::map(vec![(
                "key".into(),
                GraphValue::List(Box::new([float(f64::NAN)])),
            )])
            .unwrap(),
            GraphValue::map(vec![("key".into(), GraphValue::List(Box::new([null()])))]).unwrap(),
        ];
        for a in &values {
            for b in &values {
                let left = row(vec![a.clone()]);
                let right = row(vec![b.clone()]);
                let frozen = (
                    left.canonical_bytes().unwrap(),
                    right.canonical_bytes().unwrap(),
                );
                let ka = probe.copy_cell(a, &mut allow).unwrap();
                let kb = probe.copy_cell(b, &mut allow).unwrap();
                let native = spec.matches(&left, &right, &mut allow).unwrap();
                assert_eq!(native, !ka.is_null() && !kb.is_null() && ka == kb);
                assert_eq!(
                    (
                        left.canonical_bytes().unwrap(),
                        right.canonical_bytes().unwrap()
                    ),
                    frozen
                );
            }
        }
        assert!(
            spec.keys().is_empty(),
            "a private probe must not become a typed join condition"
        );
        let typed = RowJoinSpec::new(
            &[GraphSetColumnType::Scalar],
            &[GraphSetColumnType::Scalar],
            &[(0, 0)],
        )
        .unwrap()
        .with_predicate(&[equal(0, 1)])
        .unwrap();
        let typed_probe = Probe::new(&typed, &mut allow).unwrap();
        assert_ne!(
            typed_probe.copy_cell(&int(7), &mut allow).unwrap(),
            typed_probe.copy_cell(&float(7.0), &mut allow).unwrap()
        );
        drop(typed_probe);
        let mut join = IncrementalRowJoin::new(typed);
        join.prepare(
            &bag(&vec![(row(vec![int(7)]), 2)]),
            &bag(&vec![(row(vec![float(7.0)]), 3)]),
            LIMBS,
            None,
            &mut allow,
        )
        .unwrap()
        .commit();
        assert!(
            join.rows().is_empty(),
            "explicit typed equality is intentionally different"
        );
    }

    #[test]
    fn candidate_proof_keeps_boolean_boundaries_and_repeated_cross_positions() {
        for (code, keys) in [
            (vec![equal(0, 2)], vec![(0, 0)]),
            (
                vec![equal(2, 0), equal(0, 2), GraphSetPredicateOp::And],
                vec![(0, 0)],
            ),
            (
                vec![equal(0, 2), equal(0, 3), GraphSetPredicateOp::And],
                vec![(0, 0), (0, 1)],
            ),
            (
                vec![equal(0, 2), equal(2, 0), GraphSetPredicateOp::Or],
                vec![(0, 0)],
            ),
            (
                vec![
                    equal(0, 2),
                    GraphSetPredicateOp::Truth(Some(true)),
                    GraphSetPredicateOp::Or,
                ],
                vec![],
            ),
            (
                vec![
                    equal(0, 2),
                    GraphSetPredicateOp::Truth(None),
                    GraphSetPredicateOp::Or,
                ],
                vec![],
            ),
            (
                vec![
                    equal(0, 2),
                    GraphSetPredicateOp::Not,
                    GraphSetPredicateOp::Not,
                ],
                vec![],
            ),
        ] {
            let spec =
                RowJoinSpec::cross(&[GraphSetColumnType::Any; 2], &[GraphSetColumnType::Any; 2])
                    .unwrap()
                    .with_predicate(&code)
                    .unwrap();
            assert_eq!(Probe::new(&spec, &mut allow).unwrap().columns(), keys);
            assert!(spec.keys().is_empty());
        }
    }

    fn input(mask: usize, right: bool) -> Rows {
        let rows = if right {
            [
                (row(vec![float(7.0), int(20)]), 3),
                (row(vec![int(7), int(5)]), 2),
            ]
        } else {
            [
                (row(vec![int(7), int(10)]), 2),
                (row(vec![float(7.0), int(30)]), 1),
            ]
        };
        rows.into_iter()
            .enumerate()
            .filter(|(at, _)| mask & (1 << at) != 0)
            .map(|(_, entry)| entry)
            .collect()
    }
    // Every fixture key has the mathematical value seven. Only the literal
    // integer payload comparison decides residual truth here; no production
    // key builder, join matcher or derivative constructs the expected bag.
    fn oracle(left: &Rows, right: &Rows, kind: RowJoinKind, residual: bool) -> Rows {
        let payload = |row: &GraphValueRow| match &row.values()[1] {
            GraphValue::Scalar(CanonicalScalar::Int(n)) => *n,
            _ => panic!("literal integer fixture payload"),
        };
        let matches = |l: &GraphValueRow, r: &GraphValueRow| !residual || payload(l) < payload(r);
        let mut out = Vec::new();
        for (l, lw) in left {
            let mut found = false;
            for (r, rw) in right {
                if matches(l, r) {
                    found = true;
                    if kind.includes_right() {
                        out.push((
                            row(l.values().iter().chain(r.values()).cloned().collect()),
                            lw * rw,
                        ));
                    }
                }
            }
            match kind {
                RowJoinKind::Semi if found => out.push((l.clone(), *lw)),
                RowJoinKind::Anti if !found => out.push((l.clone(), *lw)),
                RowJoinKind::Left | RowJoinKind::Full if !found => {
                    let mut cells = l.values().to_vec();
                    cells.extend([null(), null()]);
                    out.push((row(cells), *lw));
                }
                _ => {}
            }
        }
        if matches!(kind, RowJoinKind::Right | RowJoinKind::Full) {
            for (r, rw) in right {
                if !left.iter().any(|(l, _)| matches(l, r)) {
                    let mut cells = vec![null(), null()];
                    cells.extend_from_slice(r.values());
                    out.push((row(cells), *rw));
                }
            }
        }
        out
    }

    #[test]
    fn all_modes_preserve_3072_mixed_representation_bag_transitions() {
        for kind in KINDS {
            for residual in [false, true] {
                for before in 0..16 {
                    for after in 0..16 {
                        let (l0, r0) = (input(before & 3, false), input(before >> 2, true));
                        let (l1, r1) = (input(after & 3, false), input(after >> 2, true));
                        let spec = definition(kind, residual, true);
                        let frozen = spec.clone();
                        let old = oracle(&l0, &r0, kind, residual);
                        let new = oracle(&l1, &r1, kind, residual);
                        let mut join = seeded(spec.clone(), &l0, &r0);
                        assert_eq!(join.rows(), &bag(&old));
                        let (dl, dr) = (changed(&l1, &l0), changed(&r1, &r0));
                        let delta = changed(&new, &old);
                        {
                            let pending = join.prepare(&dl, &dr, LIMBS, None, &mut allow).unwrap();
                            assert_eq!(pending.delta(), &delta);
                        }
                        assert_eq!(join, seeded(spec.clone(), &l0, &r0));
                        assert_eq!(
                            join.prepare(&dl, &dr, LIMBS, None, &mut allow)
                                .unwrap()
                                .commit(),
                            delta
                        );
                        assert_eq!(join.rows(), &bag(&new));
                        assert_eq!(join.spec(), &frozen);
                        join.prepare(
                            &changed(&l0, &l1),
                            &changed(&r0, &r1),
                            LIMBS,
                            None,
                            &mut allow,
                        )
                        .unwrap()
                        .commit();
                        assert_eq!(join, seeded(spec, &l0, &r0));
                    }
                }
            }
        }
    }

    #[test]
    fn every_prepare_refusal_and_dropped_guard_preserves_all_arrangements() {
        let (l0, r0) = (input(1, false), input(1, true));
        let (l1, r1) = (input(3, false), input(2, true));
        for kind in KINDS {
            let spec = definition(kind, true, true);
            let mut join = seeded(spec.clone(), &l0, &r0);
            let original = seeded(spec, &l0, &r0);
            let (dl, dr) = (changed(&l1, &l0), changed(&r1, &r0));
            let mut checkpoints = 0;
            drop(
                join.prepare(&dl, &dr, LIMBS, None, &mut |_| {
                    checkpoints += 1;
                    Ok::<_, usize>(())
                })
                .unwrap(),
            );
            assert_eq!(join, original);
            for stop in 1..=checkpoints {
                let mut calls = 0;
                let error = join
                    .prepare(&dl, &dr, LIMBS, None, &mut |_| {
                        calls += 1;
                        if calls == stop { Err(stop) } else { Ok(()) }
                    })
                    .unwrap_err();
                assert_eq!(error, RowJoinError::Delta(ZSetError::Control(stop)));
                assert_eq!(calls, stop);
                assert_eq!(join, original);
            }
            let old = oracle(&l0, &r0, kind, true);
            let new = oracle(&l1, &r1, kind, true);
            assert_eq!(
                join.prepare(&dl, &dr, LIMBS, None, &mut allow)
                    .unwrap()
                    .commit(),
                changed(&new, &old)
            );
            assert_eq!(join.rows(), &bag(&new));
        }
    }

    #[test]
    fn same_probe_key_never_hides_an_invalid_typed_row_retraction() {
        let left = vec![(row(vec![int(7), int(10)]), 2)];
        let right = vec![(row(vec![float(7.0), int(20)]), 3)];
        for kind in KINDS {
            let spec = definition(kind, false, true);
            let mut join = seeded(spec.clone(), &left, &right);
            let original = seeded(spec, &left, &right);
            // Both missing rows have the SAME private key as a retained row,
            // but their original typed payloads were never inserted.
            let missing_left = bag(&vec![(row(vec![float(7.0), int(10)]), -1)]);
            let missing_right = bag(&vec![(row(vec![int(7), int(20)]), -1)]);
            for (dl, dr, side) in [
                (missing_left, ZSet::new(), 0),
                (ZSet::new(), missing_right, 1),
            ] {
                assert_eq!(
                    join.prepare(&dl, &dr, LIMBS, None, &mut allow).unwrap_err(),
                    RowJoinError::NegativeMultiplicity { side }
                );
                assert_eq!(join, original);
            }
        }
        let scalar = RowJoinSpec::cross(
            &[GraphSetColumnType::Scalar; 2],
            &[GraphSetColumnType::Scalar; 2],
        )
        .unwrap()
        .with_predicate(&[equal(0, 2)])
        .unwrap();
        let mut join = IncrementalRowJoin::new(scalar.clone());
        let invalid = bag(&vec![(row(vec![GraphValue::Vertex(VId(7)), int(0)]), 1)]);
        assert_eq!(
            join.prepare(&invalid, &ZSet::new(), LIMBS, Some(0), &mut allow)
                .unwrap_err(),
            RowJoinError::InputSchema { side: 0 }
        );
        assert_eq!(join, IncrementalRowJoin::new(scalar));
    }

    #[test]
    fn dynamic_non_witness_rows_are_retained_and_outer_padded_independently() {
        // NULL, and a composite holding a NULL anywhere, are never TRUE-equal.
        for value in [
            null(),
            GraphValue::List(vec![int(7), null()].into_boxed_slice()),
            GraphValue::map(vec![("k".into(), null())]).unwrap(),
            GraphValue::map(vec![("k".into(), GraphValue::List(Box::new([null()])))]).unwrap(),
        ] {
            let left = row(vec![value.clone(), int(10)]);
            let right = row(vec![value, int(20)]);
            for kind in KINDS {
                let l = vec![(left.clone(), 2)];
                let r = vec![(right.clone(), 3)];
                let spec = definition(kind, false, true);
                let mut join = seeded(spec.clone(), &l, &r);
                let mut expected = Vec::new();
                if matches!(kind, RowJoinKind::Left | RowJoinKind::Full) {
                    let mut cells = left.values().to_vec();
                    cells.extend([null(), null()]);
                    expected.push((row(cells), 2));
                }
                if matches!(kind, RowJoinKind::Right | RowJoinKind::Full) {
                    let mut cells = vec![null(), null()];
                    cells.extend_from_slice(right.values());
                    expected.push((row(cells), 3));
                }
                if kind == RowJoinKind::Anti {
                    expected.push((left.clone(), 2));
                }
                assert_eq!(join.rows(), &bag(&expected));
                // Even an unmatched input's full original count is owned.
                let excessive = bag(&vec![(right.clone(), -4)]);
                assert_eq!(
                    join.prepare(&ZSet::new(), &excessive, LIMBS, None, &mut allow)
                        .unwrap_err(),
                    RowJoinError::NegativeMultiplicity { side: 1 }
                );
                assert_eq!(join, seeded(spec, &l, &r));
            }
        }
    }

    /// A list or map key equals another element by element, Int 7 and Float
    /// 7.0 alike at any depth, as membership::equal says (fgdb-2npty): the
    /// probe keys both sides by one normalized form, and a different
    /// composite stays in its own group.
    #[test]
    fn composite_keys_join_element_by_element_across_numeric_peers() {
        let list = |items: Vec<GraphValue>| GraphValue::List(items.into_boxed_slice());
        let pairs = [
            (list(vec![int(7)]), list(vec![float(7.0)])),
            (
                GraphValue::map(vec![("k".into(), int(7))]).unwrap(),
                GraphValue::map(vec![("k".into(), float(7.0))]).unwrap(),
            ),
            (
                list(vec![list(vec![int(1)]), int(2)]),
                list(vec![list(vec![float(1.0)]), float(2.0)]),
            ),
        ];
        for (left_key, right_key) in pairs {
            let left = row(vec![left_key, int(10)]);
            let right = row(vec![right_key, int(20)]);
            let other = row(vec![list(vec![int(8)]), int(30)]);
            for kind in [RowJoinKind::Inner, RowJoinKind::Semi, RowJoinKind::Anti] {
                let join = seeded(
                    definition(kind, false, true),
                    &vec![(left.clone(), 2)],
                    &vec![(right.clone(), 3), (other.clone(), 1)],
                );
                let expected = match kind {
                    RowJoinKind::Inner => {
                        let mut cells = left.values().to_vec();
                        cells.extend_from_slice(right.values());
                        vec![(row(cells), 6)]
                    }
                    RowJoinKind::Semi => vec![(left.clone(), 2)],
                    _ => Vec::new(),
                };
                assert_eq!(join.rows(), &bag(&expected), "{kind:?}");
            }
        }
    }

    #[test]
    fn nested_numeric_bag_transitions_and_refusals_preserve_every_join_mode() {
        let nested = |value| {
            GraphValue::map(vec![("k".into(), GraphValue::List(Box::new([value])))]).unwrap()
        };
        let wrap = |rows: Rows| {
            rows.into_iter()
                .map(|(row, weight)| {
                    (
                        GraphValueRow::from_owned_values(vec![
                            nested(row.values()[0].clone()),
                            row.values()[1].clone(),
                        ]),
                        weight,
                    )
                })
                .collect::<Rows>()
        };
        let (l0, r0) = (wrap(input(1, false)), wrap(input(1, true)));
        let (l1, r1) = (wrap(input(3, false)), wrap(input(2, true)));
        for kind in KINDS {
            let spec = definition(kind, true, true);
            let mut join = seeded(spec.clone(), &l0, &r0);
            let original = seeded(spec.clone(), &l0, &r0);
            assert_eq!(join.rows(), &bag(&oracle(&l0, &r0, kind, true)));
            let (dl, dr) = (changed(&l1, &l0), changed(&r1, &r0));
            let mut total = 0;
            drop(
                join.prepare(&dl, &dr, LIMBS, None, &mut |_| {
                    total += 1;
                    Ok::<_, usize>(())
                })
                .unwrap(),
            );
            for stop in 1..=total {
                let mut seen = 0;
                assert_eq!(
                    join.prepare(&dl, &dr, LIMBS, None, &mut |_| {
                        seen += 1;
                        if seen == stop { Err(stop) } else { Ok(()) }
                    })
                    .unwrap_err(),
                    RowJoinError::Delta(ZSetError::Control(stop))
                );
                assert_eq!(seen, stop);
                assert_eq!(join, original);
            }
            join.prepare(&dl, &dr, LIMBS, None, &mut allow)
                .unwrap()
                .commit();
            assert_eq!(join.rows(), &bag(&oracle(&l1, &r1, kind, true)));
            let unindexed = seeded(definition(kind, true, false), &l1, &r1);
            assert_eq!(join.rows(), unindexed.rows());
        }
    }

    #[test]
    fn maintained_projection_keeps_aliases_repeated_cells_and_simultaneous_collisions() {
        use crate::row_projection::IncrementalRowProjection;
        use crate::{
            GqlParameters, GraphSetProjection, GraphSetQuantifier, GraphSetValue, GraphSymbol,
            GraphSymbolKind, PreparedGraphSet, PreparedGraphText,
        };
        use fgdb_delta_types::PropertyKeyId;

        let leaf = || -> PreparedGraphSet {
            PreparedGraphText::prepare("MATCH (n) RETURN n.k AS k,n.p AS p", |kind, name| {
                match (kind, name) {
                    (GraphSymbolKind::Property, "k") => {
                        Some(GraphSymbol::Property(PropertyKeyId(1)))
                    }
                    (GraphSymbolKind::Property, "p") => {
                        Some(GraphSymbol::Property(PropertyKeyId(2)))
                    }
                    _ => None,
                }
            })
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap()
            .with_duplicates()
            .into()
        };
        let query = leaf()
            .cross_join(leaf())
            .unwrap()
            .project(
                vec![
                    GraphSetProjection::new("right_key", GraphSetValue::Column(2)),
                    GraphSetProjection::new("left_key", GraphSetValue::Column(0)),
                    GraphSetProjection::new("again", GraphSetValue::Column(2)),
                ],
                GraphSetQuantifier::All,
            )
            .unwrap()
            .nested()
            .unwrap()
            .filter(&[equal(0, 1)])
            .unwrap();
        let frozen = query.canonical_bytes();
        let (_, _, spec, output) = query
            .incremental_selected_join_with_control(&mut |_| Ok::<_, usize>(()))
            .unwrap()
            .unwrap();
        assert!(spec.keys().is_empty());
        assert_eq!(Probe::new(&spec, &mut allow).unwrap().columns(), &[(0, 0)]);
        assert_eq!(
            output.columns().collect::<Vec<_>>(),
            ["right_key", "left_key", "again"]
        );
        let mut join = IncrementalRowJoin::new(spec);
        let mut projection = IncrementalRowProjection::new(output);
        let left = vec![
            (row(vec![int(7), int(10)]), 2),
            (row(vec![int(7), int(30)]), 1),
        ];
        let right = vec![(row(vec![float(7.0), int(20)]), 3)];
        let initial = join
            .prepare(&bag(&left), &bag(&right), LIMBS, None, &mut allow)
            .unwrap()
            .commit();
        projection
            .prepare(&initial, LIMBS, None, &mut allow)
            .unwrap()
            .commit();
        let result = row(vec![float(7.0), int(7), float(7.0)]);
        assert_eq!(projection.rows(), &bag(&vec![(result.clone(), 9)]));
        let delta = join
            .prepare(
                &bag(&vec![(left[0].0.clone(), -2)]),
                &bag(&vec![(right[0].0.clone(), -2)]),
                LIMBS,
                Some(1),
                &mut allow,
            )
            .unwrap()
            .commit();
        let delta = projection
            .prepare(&delta, LIMBS, Some(1), &mut allow)
            .unwrap()
            .commit();
        assert_eq!(delta, bag(&vec![(result.clone(), -8)]));
        assert_eq!(projection.rows(), &bag(&vec![(result, 1)]));
        assert_eq!(query.canonical_bytes(), frozen);
    }

    #[test]
    fn selective_scalar_updates_compare_against_the_live_unindexed_derivative() {
        // Both operators use the same native matcher and exact delta kernels.
        // Double NOT prevents proof extraction in the incumbent, while leaving
        // its eager three-valued truth unchanged. These are logical event
        // assertions, NOT a wall-clock or end-to-end database benchmark.
        for n in [32_i64, 128] {
            let left: Rows = (0..n).map(|k| (row(vec![int(k), int(0)]), 1)).collect();
            let right: Rows = (0..n)
                .map(|k| (row(vec![float(k as f64), int(1)]), 1))
                .collect();
            let expected: Rows = (0..n)
                .map(|k| (row(vec![int(k), int(0), float(k as f64), int(1)]), 1))
                .collect();
            let mut indexed = IncrementalRowJoin::new(definition(RowJoinKind::Full, true, true));
            let mut general = IncrementalRowJoin::new(definition(RowJoinKind::Full, true, false));
            let apply = |join: &mut IncrementalRowJoin,
                         dl: &ZSet<GraphValueRow>,
                         dr: &ZSet<GraphValueRow>| {
                let mut events = 0_u64;
                join.prepare(dl, dr, LIMBS, None, &mut |_| {
                    events += 1;
                    Ok::<_, usize>(())
                })
                .unwrap()
                .commit();
                events
            };
            let (dl, dr) = (bag(&left), bag(&right));
            let indexed_events = apply(&mut indexed, &dl, &dr);
            let general_events = apply(&mut general, &dl, &dr);
            assert_eq!(indexed.rows(), &bag(&expected));
            assert_eq!(general.rows(), indexed.rows());
            assert!(indexed_events < general_events);

            // Change one left row from matching to unmatched. Only its key's
            // original right row can gain a null extension; other keys remain
            // untouched. A final output cap must still refuse the extra row.
            let update = bag(&vec![
                (row(vec![int(0), int(0)]), -1),
                (row(vec![int(0), int(2)]), 1),
            ]);
            assert_eq!(
                indexed
                    .prepare(&update, &ZSet::new(), LIMBS, Some(n as u64), &mut allow)
                    .unwrap_err(),
                RowJoinError::ResultBudget { limit: n as u64 }
            );
            assert_eq!(indexed.rows(), &bag(&expected));
            let indexed_events = apply(&mut indexed, &update, &ZSet::new());
            let general_events = apply(&mut general, &update, &ZSet::new());
            let mut after = expected[1..].to_vec();
            after.extend([
                (row(vec![int(0), int(2), null(), null()]), 1),
                (row(vec![null(), null(), float(0.0), int(1)]), 1),
            ]);
            assert_eq!(indexed.rows(), &bag(&after));
            assert_eq!(general.rows(), indexed.rows());
            assert!(indexed_events < general_events);
        }
    }
}
