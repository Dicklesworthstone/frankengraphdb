//! Independent finite-domain quorum oracles. No network, timer or disk model.
use super::{Configuration, Domain, MemberId};
use std::collections::BTreeSet;

fn members(mask: u32) -> impl Iterator<Item = MemberId> {
    (0..5_u32)
        .filter(move |slot| mask & (1_u32 << *slot) != 0)
        .map(|slot| MemberId(u128::from(slot + 1)))
}

#[test]
fn every_five_member_vote_subset_obeys_both_independent_majorities() {
    for old in 1..32_u32 {
        for new in 1..32_u32 {
            let configuration =
                Configuration::joint(Domain([1; 32]), [2; 32], members(old), members(new), [])
                    .unwrap();
            for votes in 0..32_u32 {
                let received: BTreeSet<_> = members(votes).collect();
                let expected = (old & votes).count_ones() > old.count_ones() / 2
                    && (new & votes).count_ones() > new.count_ones() / 2;
                assert_eq!(
                    configuration.quorum(&received),
                    expected,
                    "old={old} new={new} votes={votes}"
                );
                if old == new {
                    let stable =
                        Configuration::stable(Domain([1; 32]), [3; 32], members(old), []).unwrap();
                    assert_eq!(configuration.quorum(&received), stable.quorum(&received));
                }
            }
        }
    }
}

#[test]
fn every_small_joint_match_vector_agrees_with_a_threshold_count_oracle() {
    for old in 1..16_u32 {
        for new in 1..16_u32 {
            let configuration =
                Configuration::joint(Domain([1; 32]), [2; 32], members(old), members(new), [])
                    .unwrap();
            for encoded in 0..81_u32 {
                let mut quotient = encoded;
                let mut indices = [0_u64; 4];
                for index in &mut indices {
                    *index = u64::from(quotient % 3);
                    quotient /= 3;
                }
                // Scan candidate thresholds, not the implementation's order
                // statistic. Count each group independently, including overlap.
                let expected = (0..=2_u64)
                    .rev()
                    .find(|cut| {
                        [old, new].into_iter().all(|group| {
                            let reached = (0..4)
                                .filter(|slot| group & (1 << slot) != 0 && indices[*slot] >= *cut)
                                .count();
                            reached > group.count_ones() as usize / 2
                        })
                    })
                    .unwrap();
                let matched = |member: MemberId| indices[member.0 as usize - 1];
                assert_eq!(
                    configuration.quorum_index(matched),
                    expected,
                    "old={old} new={new} indices={indices:?}"
                );
                if old == new {
                    let stable =
                        Configuration::stable(Domain([1; 32]), [3; 32], members(old), []).unwrap();
                    assert_eq!(stable.quorum_index(matched), expected);
                }
            }
        }
    }
}
