use super::*;
use core::convert::Infallible;

fn allow(_: GlaExecutionEvent) -> Result<(), Infallible> {
    Ok(())
}

#[test]
fn smallest_membership_drives_seeks_without_reweighting_the_primary_bag() {
    let primary = [VId(0), VId(1), VId(1), VId(3)];
    let broad = [VId(0), VId(1), VId(1), VId(2), VId(3), VId(3)];
    let sparse = [VId(1), VId(1)];
    let other = [VId(1), VId(2), VId(3)];
    let mut cursor = Candidates::all(&primary);
    cursor.membership = Some(&broad);
    cursor.add_membership(&sparse, &mut allow).unwrap();
    cursor.add_membership(&other, &mut allow).unwrap();
    assert_eq!(cursor.membership, Some(sparse.as_slice()));
    let mut actual = Vec::new();
    while let Some(value) = cursor.next(&mut allow).unwrap() {
        actual.push(value);
    }
    // Membership duplicates do not multiply output. Logical expansions still
    // provide their real edge occurrences through the unchanged continuation.
    assert_eq!(actual, vec![VId(1), VId(1)]);
}

#[test]
fn refused_descriptor_does_not_change_the_chosen_membership() {
    let primary = [VId(1)];
    let broad = [VId(1), VId(2)];
    let sparse = [VId(1)];
    for refusal in 0..2 {
        let mut cursor = Candidates::all(&primary);
        cursor.membership = Some(&broad);
        let mut visited = 0;
        let result = cursor.add_membership(&sparse, &mut |_| {
            let at = visited;
            visited += 1;
            if at == refusal { Err("stop") } else { Ok(()) }
        });
        assert_eq!(result, Err("stop"));
        assert_eq!(visited, refusal + 1);
        assert_eq!(cursor.membership, Some(broad.as_slice()));
        assert!(cursor.additional.is_empty());
    }
}

#[test]
fn an_empty_smallest_domain_exhausts_without_visiting_the_primary() {
    let primary = [VId(0), VId(u128::MAX)];
    let broad = [VId(0), VId(u128::MAX)];
    let mut cursor = Candidates::all(&primary);
    cursor.membership = Some(&broad);
    cursor.add_membership(&[], &mut allow).unwrap();
    let mut visits = 0;
    assert_eq!(
        cursor
            .next(&mut |_| {
                visits += 1;
                Ok::<_, Infallible>(())
            })
            .unwrap(),
        None
    );
    assert_eq!(visits, 0);
}

#[test]
fn constructor_reordering_matches_every_small_bag_and_membership_permutation() {
    let choices = [VId(0), VId(1_u128 << 100), VId(u128::MAX)];
    let mut arrays = vec![Vec::new()];
    for (at, &left) in choices.iter().enumerate() {
        arrays.push(vec![left]);
        for &right in &choices[at..] {
            arrays.push(vec![left, right]);
        }
    }
    for primary in &arrays {
        for first in &arrays {
            for second in &arrays {
                for third in &arrays {
                    let expected: Vec<_> = primary
                        .iter()
                        .copied()
                        .filter(|value| {
                            first.contains(value) && second.contains(value) && third.contains(value)
                        })
                        .collect();
                    let mut cursor = Candidates::all(primary);
                    cursor.membership = Some(first);
                    cursor.add_membership(second, &mut allow).unwrap();
                    cursor.add_membership(third, &mut allow).unwrap();
                    assert_eq!(
                        cursor.membership.unwrap().len(),
                        first.len().min(second.len()).min(third.len())
                    );
                    let mut actual = Vec::new();
                    while let Some(value) = cursor.next(&mut allow).unwrap() {
                        actual.push(value);
                    }
                    assert_eq!(actual, expected);
                }
            }
        }
    }
}
