//! In-process retirement/clock fencing over the real foundation token path.
//! These tests do not model durable policy publication or distributed revocation.
use asupersync::security::key::AuthKey;
use fgdb_delta_types::{RelationId, SchemaEpoch};
use fgdb_types::DatabaseSecurityNamespaceId;
use fgdb_warden::{Authority, Error, Grant, QueryLimits, Restriction, Rights, Scope, Usage};
use std::cell::Cell;
use std::sync::{Arc, Barrier};

const NOW: u64 = 100;
const BRANCH: &str = "main";

fn authority(epoch: u64) -> Authority {
    Authority::new(
        AuthKey::from_seed(91),
        DatabaseSecurityNamespaceId([2; 32]),
        "graph",
        SchemaEpoch(3),
        epoch,
    )
    .unwrap()
}

fn grant() -> Grant {
    let mut grant = Grant::read_only(
        BRANCH,
        1000,
        QueryLimits {
            max_nodes: 10,
            max_work: 10,
            max_rows: 10,
        },
    );
    grant.rights = Rights::ReadWrite;
    grant.labels = Scope::All;
    grant.relations = Scope::only([RelationId(1)]);
    grant.properties = Scope::All;
    grant
}

#[test]
fn retirement_closes_all_admission_paths_including_preverified_capabilities() {
    let issuer = authority(1);
    let token = issuer.issue_at(&grant(), NOW).unwrap();
    let child = token.attenuate(Restriction::MaxRows(1)).unwrap();
    let verified = issuer.verify_at(&token, BRANCH, NOW).unwrap();
    assert!(!issuer.is_retired());
    assert!(issuer.retire());
    assert!(!issuer.retire());
    assert!(issuer.is_retired());
    assert_eq!(issuer.namespace(), DatabaseSecurityNamespaceId([2; 32]));
    assert!(matches!(
        issuer.issue_at(&grant(), NOW),
        Err(Error::AuthorityRetired)
    ));
    for bearer in [&token, &child] {
        assert!(matches!(
            issuer.verify_at(bearer, BRANCH, NOW),
            Err(Error::AuthorityRetired)
        ));
    }
    assert_eq!(
        issuer.recheck_at(&verified, BRANCH, NOW),
        Err(Error::AuthorityRetired)
    );
    assert!(matches!(
        verified.begin_read_at(BRANCH, NOW),
        Err(Error::AuthorityRetired)
    ));
    assert!(matches!(
        verified.begin_write_at(BRANCH, NOW),
        Err(Error::AuthorityRetired)
    ));
}

#[test]
fn previously_borrowed_read_and_write_permits_stop_without_resetting_usage() {
    let issuer = authority(1);
    let token = issuer.issue_at(&grant(), NOW).unwrap();
    let verified = issuer.verify_at(&token, BRANCH, NOW).unwrap();
    let mut read = verified.begin_read_at(BRANCH, NOW).unwrap();
    let mut write = verified.begin_write_at(BRANCH, NOW).unwrap();
    read.charge_nodes_at(NOW, 3).unwrap();
    write.charge_work_at(NOW, 2).unwrap();
    let old_read = read.usage();
    let old_write = write.usage();
    issuer.retire();
    assert_eq!(read.checkpoint_at(NOW), Err(Error::AuthorityRetired));
    assert_eq!(write.charge_rows_at(NOW, 0), Err(Error::AuthorityRetired));
    assert_eq!(read.charge_work_at(NOW, 0), Err(Error::ExecutionStopped));
    assert_eq!(write.checkpoint_at(NOW), Err(Error::ExecutionStopped));
    assert_eq!(read.usage(), old_read);
    assert_eq!(write.usage(), old_write);
}

#[test]
fn every_charge_dimension_and_zero_charge_observe_retirement() {
    for dimension in 0..3 {
        for amount in [0, 1, u64::MAX] {
            let issuer = authority(1);
            let token = issuer.issue_at(&grant(), NOW).unwrap();
            let verified = issuer.verify_at(&token, BRANCH, NOW).unwrap();
            let mut permit = verified.begin_read_at(BRANCH, NOW).unwrap();
            issuer.retire();
            let result = match dimension {
                0 => permit.charge_nodes_at(NOW, amount),
                1 => permit.charge_work_at(NOW, amount),
                _ => permit.charge_rows_at(NOW, amount),
            };
            assert_eq!(result, Err(Error::AuthorityRetired));
            assert_eq!(permit.usage(), Usage::default());
        }
    }
}

#[test]
fn denied_or_retired_relations_never_invoke_the_opener() {
    let issuer = authority(1);
    let token = issuer.issue_at(&grant(), NOW).unwrap();
    let verified = issuer.verify_at(&token, BRANCH, NOW).unwrap();
    let mut permit = verified.begin_read_at(BRANCH, NOW).unwrap();
    assert_eq!(
        permit.with_relation_at(NOW, RelationId(2), || panic!("denied opener")),
        Ok(None::<()>)
    );
    assert_eq!(permit.usage(), Usage::default());
    issuer.retire();
    assert_eq!(
        permit.with_relation_at(NOW, RelationId(1), || panic!("retired opener")),
        Err::<Option<()>, _>(Error::AuthorityRetired)
    );
}

#[test]
fn retirement_inside_opener_drops_its_value_before_returning() {
    struct Guard<'a>(&'a Cell<bool>);
    impl Drop for Guard<'_> {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }
    let issuer = authority(1);
    let token = issuer.issue_at(&grant(), NOW).unwrap();
    let verified = issuer.verify_at(&token, BRANCH, NOW).unwrap();
    let mut permit = verified.begin_read_at(BRANCH, NOW).unwrap();
    let dropped = Cell::new(false);
    let result = permit.with_relation_at(NOW, RelationId(1), || {
        issuer.retire();
        Guard(&dropped)
    });
    assert!(matches!(result, Err(Error::AuthorityRetired)));
    assert!(dropped.get());
    assert_eq!(permit.usage().work, 1);
    assert_eq!(permit.checkpoint_at(NOW), Err(Error::ExecutionStopped));
}

#[test]
fn replacement_epoch_cannot_reactivate_old_permits_or_bearers() {
    let old = authority(1);
    let token = old.issue_at(&grant(), NOW).unwrap();
    let verified = old.verify_at(&token, BRANCH, NOW).unwrap();
    let mut permit = verified.begin_read_at(BRANCH, NOW).unwrap();
    old.retire();
    let new = authority(2);
    assert!(matches!(
        new.verify_at(&token, BRANCH, NOW),
        Err(Error::WrongAuthority)
    ));
    assert_eq!(
        new.recheck_at(&verified, BRANCH, NOW),
        Err(Error::WrongAuthority)
    );
    let new_token = new.issue_at(&grant(), NOW).unwrap();
    let new_verified = new.verify_at(&new_token, BRANCH, NOW).unwrap();
    assert!(new_verified.begin_read_at(BRANCH, NOW).is_ok());
    assert_eq!(permit.checkpoint_at(NOW), Err(Error::AuthorityRetired));
    // No bearer format mutation or implicit key rotation was introduced.
    let equivalent = authority(2);
    assert_eq!(
        new_token.encode(),
        equivalent.issue_at(&grant(), NOW).unwrap().encode()
    );
}

#[test]
fn clock_rollback_is_terminal_and_equal_timestamps_are_legal() {
    let issuer = authority(1);
    let token = issuer.issue_at(&grant(), NOW).unwrap();
    let verified = issuer.verify_at(&token, BRANCH, NOW).unwrap();
    let mut permit = verified.begin_read_at(BRANCH, NOW).unwrap();
    for time in [NOW, NOW, NOW + 10, NOW + 10] {
        permit.charge_nodes_at(time, 1).unwrap();
    }
    let before = permit.usage();
    assert_eq!(
        permit.charge_work_at(NOW + 9, 0),
        Err(Error::ClockWentBackwards)
    );
    assert_eq!(permit.checkpoint_at(NOW + 11), Err(Error::ExecutionStopped));
    assert_eq!(permit.usage(), before);
    let mut write = verified.begin_write_at(BRANCH, NOW + 30).unwrap();
    assert_eq!(
        write.checkpoint_at(NOW + 29),
        Err(Error::ClockWentBackwards)
    );
}

#[test]
fn expiry_and_quota_failures_remain_terminal_after_retirement() {
    let issuer = authority(1);
    let token = issuer.issue_at(&grant(), NOW).unwrap();
    let verified = issuer.verify_at(&token, BRANCH, NOW).unwrap();
    let mut expired = verified.begin_read_at(BRANCH, NOW).unwrap();
    let mut exhausted = verified.begin_write_at(BRANCH, NOW).unwrap();
    assert_eq!(expired.checkpoint_at(1000), Err(Error::Expired));
    assert!(exhausted.charge_nodes_at(NOW, 11).is_err());
    issuer.retire();
    assert_eq!(expired.checkpoint_at(NOW), Err(Error::ExecutionStopped));
    assert_eq!(
        exhausted.charge_work_at(NOW, 0),
        Err(Error::ExecutionStopped)
    );
}

#[test]
fn concurrent_retirement_has_one_winner_and_fences_live_permits() {
    let issuer = Arc::new(authority(1));
    let token = issuer.issue_at(&grant(), NOW).unwrap();
    let verified = issuer.verify_at(&token, BRANCH, NOW).unwrap();
    let mut read = verified.begin_read_at(BRANCH, NOW).unwrap();
    let mut write = verified.begin_write_at(BRANCH, NOW).unwrap();
    let ready = Arc::new(Barrier::new(8));
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let issuer = Arc::clone(&issuer);
            let ready = Arc::clone(&ready);
            std::thread::spawn(move || {
                ready.wait();
                issuer.retire()
            })
        })
        .collect();
    let winners = threads
        .into_iter()
        .map(|t| usize::from(t.join().unwrap()))
        .sum::<usize>();
    assert_eq!(winners, 1);
    assert_eq!(read.checkpoint_at(NOW), Err(Error::AuthorityRetired));
    assert_eq!(write.checkpoint_at(NOW), Err(Error::AuthorityRetired));
}
