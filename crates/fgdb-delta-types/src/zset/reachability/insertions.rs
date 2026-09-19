//! Monotone closure derivative. A new path has a first inserted edge preceded
//! by an OLD path, and decomposes thereafter into inserted edges and OLD paths.
//! Seed those old predecessors, then traverse only new edges and old closure
//! cones. A completed cone already includes every old successor of its members,
//! so reaching an already admitted member never requires expanding it again.
//!
//! Only new pairs are staged; old forward/reverse rows are borrowed. This is
//! not an output-linear worst-case bound: overlapping old cones and candidate
//! seeds still cost work, but unchanged adjacency is never traversed. Retained
//! closure may remain quadratic, just as for the deletion-safe parent operator.

use super::*;

type Derivative<V> = (Relation<V>, ZSet<(V, V)>);

impl<V: Ord + Clone> IncrementalReachability<V> {
    pub(super) fn derive_insertions<E>(
        &self,
        inserted: &Relation<V>,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<Derivative<V>, ZSetError<E>> {
        let mut seeds = Relation::new();
        for (source, row) in inserted {
            event(control, ZSetEvent::Work)?;
            for destination in row {
                event(control, ZSetEvent::Work)?;
                // Retain this physical edge at commit, but its logical path is
                // already represented. In particular do not enumerate a huge
                // predecessor row for a redundant transitive insertion.
                if self.contains(source, destination) {
                    continue;
                }
                self.seed_insertion(&mut seeds, source, destination, control)?;
                for predecessor in self.predecessors.get(source).into_iter().flatten() {
                    event(control, ZSetEvent::Work)?;
                    self.seed_insertion(&mut seeds, predecessor, destination, control)?;
                }
            }
        }
        let mut additions = Relation::new();
        let mut output = ZSet::new();
        for (source, targets) in seeds {
            event(control, ZSetEvent::Work)?;
            let old = self.reachable.get(&source);
            let mut reached = BTreeSet::new();
            let mut pending = Vec::new();
            for target in targets {
                event(control, ZSetEvent::Work)?;
                event(control, ZSetEvent::ScratchEntry)?;
                pending.push(target);
            }
            while let Some(target) = pending.pop() {
                event(control, ZSetEvent::Work)?;
                if old.is_some_and(|row| row.contains(&target)) || reached.contains(&target) {
                    continue;
                }
                // Finish this entire OLD cone before processing the next seed.
                // Include target itself because the prefix contains a new edge;
                // this admits positive-length cycles, not reflexive zero hops.
                for vertex in std::iter::once(&target)
                    .chain(self.reachable.get(&target).into_iter().flatten())
                {
                    event(control, ZSetEvent::Work)?;
                    if old.is_some_and(|row| row.contains(vertex))
                        || !insert_vertex(&mut reached, vertex, control)?
                    {
                        continue;
                    }
                    // Reserve eventual forward and reverse entries/groups,
                    // independently of the private staging and output entries.
                    for _ in 0..4 {
                        event(control, ZSetEvent::ScratchEntry)?;
                    }
                    output.accumulate(
                        (source.clone(), vertex.clone()),
                        ZWeight::ONE,
                        limbs,
                        control,
                    )?;
                    for destination in inserted.get(vertex).into_iter().flatten() {
                        event(control, ZSetEvent::Work)?;
                        event(control, ZSetEvent::ScratchEntry)?;
                        pending.push(destination.clone());
                    }
                }
            }
            if !reached.is_empty() {
                event(control, ZSetEvent::ScratchEntry)?;
                additions.insert(source, reached);
            }
        }
        Ok((additions, output))
    }

    fn seed_insertion<E>(
        &self,
        seeds: &mut Relation<V>,
        source: &V,
        destination: &V,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<(), ZSetError<E>> {
        event(control, ZSetEvent::Work)?;
        if !self.contains(source, destination) {
            insert_pair(seeds, source, destination, control)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMBS: LimbLimit = LimbLimit::new(16);
    fn allow(_: ZSetEvent) -> Result<(), usize> {
        Ok(())
    }

    fn z(rows: impl IntoIterator<Item = ((usize, usize), i128)>) -> ZSet<(usize, usize)> {
        ZSet::from_updates(
            rows.into_iter()
                .map(|(key, weight)| (key, ZWeight::from_i128(weight))),
            LIMBS,
            &mut allow,
        )
        .unwrap()
    }

    // Independent finite-domain Floyd-Warshall oracle, including nonempty
    // cycles. No diagonal is initialized merely because a vertex exists.
    fn oracle(edges: &[(usize, usize)], n: usize) -> BTreeSet<(usize, usize)> {
        let mut paths = vec![vec![false; n]; n];
        for &(a, b) in edges {
            paths[a][b] = true;
        }
        for k in 0..n {
            for a in 0..n {
                for b in 0..n {
                    paths[a][b] |= paths[a][k] && paths[k][b];
                }
            }
        }
        let mut result = BTreeSet::new();
        for (a, row) in paths.iter().enumerate() {
            for (b, &present) in row.iter().enumerate() {
                if present {
                    result.insert((a, b));
                }
            }
        }
        result
    }

    fn pairs(state: &IncrementalReachability<usize>) -> BTreeSet<(usize, usize)> {
        state.pairs().map(|(&a, &b)| (a, b)).collect()
    }

    #[test]
    fn every_three_vertex_monotone_transition_matches_independent_closure() {
        // Each directed edge (including loops) is absent, old, or inserted.
        // This enumerates every old <= new topology pair, not just one-edge ticks.
        for mut code in 0..3_usize.pow(9) {
            let mut old = Vec::new();
            let mut added = Vec::new();
            for edge in 0..9 {
                match code % 3 {
                    1 => old.push((edge / 3, edge % 3)),
                    2 => added.push((edge / 3, edge % 3)),
                    _ => {}
                }
                code /= 3;
            }
            let mut state = IncrementalReachability::new();
            state
                .apply(&z(old.iter().map(|&edge| (edge, 1))), LIMBS, &mut allow)
                .unwrap();
            let before = oracle(&old, 3);
            assert_eq!(pairs(&state), before);
            let delta = state
                .apply(&z(added.iter().map(|&edge| (edge, 1))), LIMBS, &mut allow)
                .unwrap();
            old.extend(added);
            let after = oracle(&old, 3);
            assert_eq!(pairs(&state), after);
            let expected: BTreeSet<_> = after.difference(&before).copied().collect();
            assert_eq!(
                delta.iter().map(|(key, _)| *key).collect::<BTreeSet<_>>(),
                expected
            );
            assert!(delta.iter().all(|(_, weight)| weight == &ZWeight::ONE));
            // The reverse dependency arrangement must agree, or future deletion
            // would miss roots even though this tick's forward rows look right.
            for a in 0..3 {
                for b in 0..3 {
                    assert_eq!(
                        state
                            .predecessors
                            .get(&b)
                            .is_some_and(|row| row.contains(&a)),
                        after.contains(&(a, b))
                    );
                }
            }
        }
    }

    #[test]
    fn every_preparation_refusal_and_downstream_drop_leave_all_state_unchanged() {
        let base = z([((0, 1), 1), ((2, 3), 1), ((3, 4), 1)]);
        let change = z([((1, 2), 1), ((4, 0), 1), ((4, 5), 1)]);
        let mut state = IncrementalReachability::new();
        state.apply(&base, LIMBS, &mut allow).unwrap();
        let mut reference = IncrementalReachability::new();
        reference.apply(&base, LIMBS, &mut allow).unwrap();
        let mut calls = 0;
        {
            let pending = state
                .prepare(&change, LIMBS, &mut |_| {
                    calls += 1;
                    Ok::<_, usize>(())
                })
                .unwrap();
            assert!(!pending.delta().is_empty());
            // Simulates a refusing downstream sink: its tentative derivative is
            // visible only under the guard, then every staged arrangement drops.
            drop(pending);
        }
        assert_eq!(state, reference);
        for stop in 1..=calls {
            let mut visited = 0;
            match state.prepare(&change, LIMBS, &mut |_| {
                visited += 1;
                if visited == stop { Err(stop) } else { Ok(()) }
            }) {
                Err(error) => assert_eq!(error, ReachabilityError::Delta(ZSetError::Control(stop))),
                Ok(update) => {
                    drop(update);
                    panic!("checkpoint {stop} was not visited");
                }
            }
            assert_eq!(state, reference);
        }
        let delta = state.apply(&change, LIMBS, &mut allow).unwrap();
        let mut sink = reference.snapshot(LIMBS, &mut allow).unwrap();
        sink.integrate(&delta, LIMBS, &mut allow).unwrap();
        assert_eq!(sink, state.snapshot(LIMBS, &mut allow).unwrap());
        assert_eq!(
            pairs(&state),
            oracle(&[(0, 1), (2, 3), (3, 4), (1, 2), (4, 0), (4, 5)], 6)
        );
    }

    #[test]
    fn extending_a_chain_visits_new_pairs_not_old_paths() {
        for n in [32, 64, 128] {
            let mut state = IncrementalReachability::new();
            state
                .apply(&z((0..n).map(|a| ((a, a + 1), 1))), LIMBS, &mut allow)
                .unwrap();
            let mut work = 0;
            let mut scratch = 0;
            let delta = state
                .apply(&z([((n, n + 1), 1)]), LIMBS, &mut |event| {
                    match event {
                        ZSetEvent::Work => work += 1,
                        ZSetEvent::ScratchEntry => scratch += 1,
                    }
                    Ok::<_, usize>(())
                })
                .unwrap();
            assert_eq!(delta.len(), n + 1);
            assert!(work < 24 * (n + 1) + 32, "work={work}, n={n}");
            assert!(scratch < 16 * (n + 1) + 32, "scratch={scratch}, n={n}");
        }
    }

    #[test]
    fn redundant_insertions_keep_physical_support_without_visiting_old_roots() {
        let n = 64;
        let mut state = IncrementalReachability::new();
        state
            .apply(&z((0..n).map(|a| ((a, a + 1), 1))), LIMBS, &mut allow)
            .unwrap();
        let before = pairs(&state);
        let mut work = 0;
        let delta = state
            .apply(&z([((n / 2, n), 1)]), LIMBS, &mut |event| {
                if event == ZSetEvent::Work {
                    work += 1;
                }
                Ok::<_, usize>(())
            })
            .unwrap();
        assert!(delta.is_empty());
        assert!(work < 24, "redundant insertion visited old roots: {work}");
        assert_eq!(pairs(&state), before);
        assert_eq!(state.edge_weight(&(n / 2, n)), Some(&ZWeight::ONE));
        state
            .apply(&z([((n - 1, n), -1)]), LIMBS, &mut allow)
            .unwrap();
        let mut expected: Vec<_> = (0..n - 1).map(|a| (a, a + 1)).collect();
        expected.push((n / 2, n));
        assert_eq!(pairs(&state), oracle(&expected, n + 1));
    }

    #[test]
    fn exact_parallel_counts_and_invalid_retractions_preserve_the_closure() {
        let mut state = IncrementalReachability::new();
        state
            .apply(&z([((0, 1), i128::MAX)]), LIMBS, &mut allow)
            .unwrap();
        assert!(
            state
                .apply(&z([((0, 1), 1)]), LIMBS, &mut allow)
                .unwrap()
                .is_empty()
        );
        assert!(state.edge_weight(&(0, 1)).unwrap().is_promoted());
        assert!(
            state
                .apply(&z([((0, 1), -i128::MAX)]), LIMBS, &mut allow)
                .unwrap()
                .is_empty()
        );
        assert_eq!(state.edge_weight(&(0, 1)), Some(&ZWeight::ONE));
        assert!(matches!(
            state.prepare(&z([((0, 1), -2)]), LIMBS, &mut allow),
            Err(ReachabilityError::NegativeMultiplicity)
        ));
        assert_eq!(state.edge_weight(&(0, 1)), Some(&ZWeight::ONE));
        assert!(state.contains(&0, &1));
        state.apply(&z([((0, 1), -1)]), LIMBS, &mut allow).unwrap();
        assert!(state.pairs().next().is_none());
    }
}
