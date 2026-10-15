use super::*;
use crate::WorkBudget;

fn make_cursor<'a>(seeds: &'a [VId], work: &mut dyn WorkControl) -> ExpansionCursor<'a> {
    ExpansionCursor::new(seeds, 2, false, 1, ExpansionLimits::default(), work).unwrap()
}

#[test]
fn suspended_requests_keep_minimum_hops_and_admit_each_visit_once() {
    let seeds = [VId(0), VId(0), VId(99)];
    let high = VId(u128::MAX);
    let mut work = WorkBudget::new(10_000);
    let mut cursor = make_cursor(&seeds, &mut work);
    let mut admissions = 0;
    let mut sources = Vec::new();
    loop {
        match cursor.request(&mut work).unwrap() {
            ExpansionRequest::Seed(id) => cursor.resolve_membership(id != VId(99), || {
                admissions += 1;
                Ok(())
            }).unwrap(),
            ExpansionRequest::Neighbors(id) => {
                sources.push(id);
                // A host may suspend after this request and each membership
                // preflight. No source or work reference is held by the cursor.
                let next = if id == VId(0) { [VId(1), VId(1)] } else { [high, VId(0)] };
                for id in next {
                    cursor.prepare_neighbor(id, &mut work).unwrap();
                    cursor.resolve_membership(true, || {
                        admissions += 1;
                        Ok(())
                    }).unwrap();
                }
                cursor.finish_neighbors().unwrap();
            }
            ExpansionRequest::Complete => break,
        }
    }
    assert_eq!(sources, [VId(0), VId(1)]);
    assert_eq!(admissions, 3);
    assert_eq!(cursor.visited_vertices(), 3);
    assert_eq!(cursor.finish(|id, _| Ok(id == high), &mut work).unwrap(),
        [GraphHit { id: high, hops: 2 }]);
}

#[test]
fn incomplete_sources_protocol_errors_and_external_refusals_cannot_finish() {
    let seeds = [VId(1)];
    for stage in 0..4 {
        let mut work = WorkBudget::new(1000);
        let mut cursor = make_cursor(&seeds, &mut work);
        if stage > 0 {
            assert_eq!(cursor.request(&mut work).unwrap(), ExpansionRequest::Seed(VId(1)));
        }
        if stage > 1 {
            cursor.resolve_membership(true, || Ok(())).unwrap();
            assert_eq!(cursor.request(&mut work).unwrap(), ExpansionRequest::Neighbors(VId(1)));
        }
        if stage > 2 {
            cursor.prepare_neighbor(VId(2), &mut work).unwrap();
        }
        assert!(cursor.finish(|_, _| Ok(true), &mut work).is_err());
    }
    let mut work = WorkBudget::new(1000);
    let mut cursor = make_cursor(&seeds, &mut work);
    cursor.request(&mut work).unwrap();
    let first = cursor.request(&mut work).unwrap_err();
    assert_eq!(cursor.resolve_membership(true, || panic!("poisoned admission")).unwrap_err(), first);
    assert_eq!(cursor.refuse(BeaconError::Cancelled), first);
    assert_eq!(cursor.finish(|_, _| panic!("poisoned ranking"), &mut work).unwrap_err(), first);

    let mut cursor = make_cursor(&seeds, &mut work);
    let first = BeaconError::InvalidQuery("source failed while suspended");
    assert_eq!(cursor.refuse(first.clone()), first);
    assert_eq!(cursor.request(&mut work).unwrap_err(), first);
}

#[test]
fn allocation_admission_is_pre_retention_and_unwind_poisoned() {
    let mut work = WorkBudget::new(1000);
    let mut cursor = make_cursor(&[VId(1)], &mut work);
    cursor.request(&mut work).unwrap();
    let error = BeaconError::ResourceLimit { resource: "test visit allocation", limit: 0 };
    assert_eq!(cursor.resolve_membership(true, || Err(error.clone())).unwrap_err(), error);
    assert_eq!(cursor.visited_vertices(), 0);
    assert_eq!(cursor.request(&mut work).unwrap_err(), error);

    let mut cursor = make_cursor(&[VId(1)], &mut work);
    cursor.request(&mut work).unwrap();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = cursor.resolve_membership(true, || panic!("admission unwind"));
    }));
    assert!(panic.is_err());
    assert_eq!(cursor.visited_vertices(), 0);
    assert_eq!(cursor.request(&mut work).unwrap_err(),
        BeaconError::Invariant("unfinished expansion step"));
}

#[test]
fn zero_candidates_are_source_free_but_seed_bounds_still_apply() {
    let mut work = WorkBudget::new(1);
    let mut cursor = ExpansionCursor::new(&[VId(9)], u32::MAX, true, 0,
        ExpansionLimits::default(), &mut work).unwrap();
    assert_eq!(cursor.request(&mut work).unwrap(), ExpansionRequest::Complete);
    assert!(cursor.finish(|_, _| panic!("disabled ranking"), &mut work).unwrap().is_empty());
    assert_eq!(work.remaining(), 0);
    assert!(ExpansionCursor::new(&[VId(9)], 0, true, 0, ExpansionLimits {
        max_seed_ids: 0, ..ExpansionLimits::default()
    }, &mut WorkBudget::new(1)).is_err());
}
