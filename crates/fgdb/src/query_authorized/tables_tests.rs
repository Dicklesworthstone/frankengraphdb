//! fgdb-61p1n: admitted tables are sorted vectors. Sealing must give exactly
//! the ordered map the tables replaced: ascending identities, and the LAST
//! admission of a repeated identity, for any admission order.

use super::seal_by_key;
use std::collections::BTreeMap;

/// xorshift64*: deterministic, dependency-free.
fn next(state: &mut u64) -> u64 {
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    state.wrapping_mul(0x2545_f491_4f6c_dd1d)
}

#[test]
fn sealing_equals_the_ordered_map_it_replaced() {
    let mut state = 0x61b1_7ab1_e5ee_d001;
    let (mut sorted_inputs, mut repeated) = (0, 0);
    for trial in 0..20_000 {
        let len = (next(&mut state) % 40) as usize;
        let span = 1 + next(&mut state) % 60;
        let ascending = next(&mut state).is_multiple_of(3);
        let mut items: Vec<(u64, usize)> = (0..len)
            .map(|at| {
                let key = if ascending {
                    at as u64 * 2 + next(&mut state) % 2
                } else {
                    next(&mut state) % span
                };
                (key, at)
            })
            .collect();
        let mut map = BTreeMap::new();
        for &(key, at) in &items {
            map.insert(key, at);
        }
        sorted_inputs += usize::from(items.windows(2).all(|pair| pair[0].0 < pair[1].0));
        repeated += usize::from(map.len() < items.len());
        seal_by_key(&mut items, |&(key, _)| key);
        assert_eq!(items, map.into_iter().collect::<Vec<_>>(), "trial {trial}");
    }
    assert!(
        sorted_inputs > 5_000 && repeated > 5_000,
        "{sorted_inputs} {repeated}"
    );
}
