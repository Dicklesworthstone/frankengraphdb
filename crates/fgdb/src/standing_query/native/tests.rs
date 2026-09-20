use super::*;
use fgdb_delta_types::LimbLimit;

fn bag() -> ZSet<u8> {
    ZSet::from_updates([(1, ZWeight::from_i128(2)), (2, ZWeight::ONE)],
        LimbLimit::new(4), &mut |_| Ok::<_, ()>(())).unwrap()
}

#[test]
fn every_delivery_checkpoint_refuses_without_changing_the_published_generation() {
    let bag = bag();
    let ordered = [Arc::new(2_u8), Arc::new(1_u8), Arc::new(1_u8)];
    let stats = StandingQueryStats::default();
    let view = StandingQueryView { rows: &bag, ordered: Some(&ordered), frontier: CommitSeq(7), stats: &stats };
    let run = |stop, policy| {
        let mut calls = 0;
        let mut checkpoint = || { calls += 1;
            if calls == stop { Err(StandingQueryFailure::Interrupted) } else { Ok(()) } };
        let mut meter = Meter { policy, stats, checkpoint: &mut checkpoint };
        let result = collect(&view, 1, &mut meter, |row, meter| {
            meter.charge(ZSetEvent::Work)?;
            meter.charge(ZSetEvent::ScratchEntry)?;
            Ok(vec![QueryValue::Count(u64::from(*row))])
        });
        let observed = meter.stats;
        (result, observed, calls)
    };
    let policy = GqlQueryPolicy::new(0, 3, 1000, 1000);
    let (expected, measured, calls) = run(usize::MAX, policy);
    assert_eq!(expected.as_ref().unwrap(), &vec![vec![QueryValue::Count(2)],
        vec![QueryValue::Count(1)], vec![QueryValue::Count(1)]]);
    for stop in 1..=calls {
        let (result, _, visited) = run(stop, policy);
        assert_eq!(result, Err(StandingQueryFailure::Interrupted));
        assert_eq!(visited, stop);
        assert_eq!(view.frontier(), CommitSeq(7));
        assert_eq!(*view.rows(), bag);
    }
    assert_eq!(run(usize::MAX, GqlQueryPolicy::new(0, 3, measured.work_units, measured.scratch_entries)).0, expected);
    for (work, scratch, rows, error) in [
        (measured.work_units - 1, measured.scratch_entries, 3, StandingQueryFailure::WorkBudget),
        (measured.work_units, measured.scratch_entries - 1, 3, StandingQueryFailure::ScratchBudget),
        (measured.work_units, measured.scratch_entries, 2, StandingQueryFailure::ResultBudget),
    ] {
        assert_eq!(run(usize::MAX, GqlQueryPolicy::new(0, rows, work, scratch)).0, Err(error));
    }
    assert_eq!(run(usize::MAX, policy).0, expected);
}

#[test]
fn bag_delivery_preserves_weights_and_refuses_impossible_counts_or_row_width() {
    let stats = StandingQueryStats::default();
    let mut checkpoint = || Ok(());
    let mut meter = Meter { policy: GqlQueryPolicy::new(0, 10, 1000, 1000), stats, checkpoint: &mut checkpoint };
    let bag = bag();
    let view = StandingQueryView { rows: &bag, ordered: None, frontier: CommitSeq(1), stats: &stats };
    let actual = collect(&view, 1, &mut meter, |row, _| Ok(vec![QueryValue::Count(u64::from(*row))])).unwrap();
    assert_eq!(actual, vec![vec![QueryValue::Count(1)], vec![QueryValue::Count(1)], vec![QueryValue::Count(2)]]);
    assert_eq!(collect(&view, 2, &mut meter, |_, _| Ok(vec![])), Err(StandingQueryFailure::InvalidDelta));
    for (weight, error) in [(ZWeight::from_i128(-1), StandingQueryFailure::InvalidDelta),
        (ZWeight::from_i128(i128::MAX), StandingQueryFailure::ResultBudget)] {
        let bag = ZSet::from_updates([(1_u8, weight)], LimbLimit::new(4), &mut |_| Ok::<_, ()>(())).unwrap();
        let view = StandingQueryView { rows: &bag, ordered: None, frontier: CommitSeq(1), stats: &stats };
        assert_eq!(collect(&view, 1, &mut meter, |_, _| panic!("must refuse before projecting")), Err(error));
    }
}

#[test]
fn failed_native_registrations_do_not_leave_registry_entries() {
    use asupersync::lab::run_async_under_lab;
    use crate::{DatabaseKeys, NativeReadClass};
    use fgdb_delta_types::PropertyKeyId;
    use fgdb_gql::{GraphSymbol, GraphSymbolKind};
    use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};
    let ((), report) = run_async_under_lab(0x6e73_7110, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let keys = DatabaseKeys::new([1; 32], DatabaseSecurityNamespaceId([2; 32]), [3; 32]);
        let mut db = Database::open_memory(&contexts.commit(), keys).await.unwrap();
        let params = GqlParameters::new();
        let policy = GqlQueryPolicy::new(1000, 1000, 100_000, 100_000);
        let resolve = |kind, name: &str| match (kind, name) {
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            _ => None,
        };
        let handle = db.register_standing_native(&cx, "MATCH (n) RETURN n.p AS p", &params, resolve, policy).unwrap();
        let before = db.standing_queries.len();
        for text in ["not a query", "MATCH (n) WHERE n.p > $missing RETURN n.p AS p"] {
            assert!(matches!(db.register_standing_native(&cx, text, &params, resolve, policy),
                Err(StandingQueryError::NativePrepare(_))));
            assert_eq!(db.standing_queries.len(), before);
        }
        for (text, class) in [
            ("MATCH (n) FOR SYSTEM_TIME AS OF SEQ 0 RETURN n.p AS p", NativeReadClass::TemporalPattern),
            ("MATCH (n) FOR SYSTEM_TIME AS OF SEQ 0 RETURN COUNT(*) AS c", NativeReadClass::TemporalAggregate),
            ("MATCH (n) FOR SYSTEM_TIME AS OF SEQ 0 RETURN n.p AS p UNION ALL MATCH (m) RETURN m.p AS p", NativeReadClass::TemporalSet),
        ] {
            assert!(matches!(db.register_standing_native(&cx, text, &params, resolve, policy),
                Err(StandingQueryError::NativeClassUnsupported { facade }) if facade == class));
            assert_eq!(db.standing_queries.len(), before);
        }
        assert!(db.register_standing_native(&cx, "MATCH (n) RETURN COUNT(*) AS c", &params, resolve,
            GqlQueryPolicy::new(0, 0, 0, 0)).is_err());
        assert_eq!(db.standing_queries.len(), before);
        assert!(db.standing_native_query(&cx, &handle, policy).is_ok());
        let raw = db.register_standing_rows(&cx, fgdb_gql::PreparedGraphText::prepare("MATCH (n) RETURN n", resolve)
            .unwrap().bind_parameters(&params).unwrap(), policy).unwrap();
        assert!(matches!(db.standing_native_query(&cx, &raw, policy), Err(StandingQueryError::Unsupported)));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
