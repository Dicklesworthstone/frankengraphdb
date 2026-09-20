use super::*;
use crate::{DatabaseKeys, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::PropertyKeyId;
use fgdb_gql::{GqlParameters, GraphSetColumnType, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};

fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(100_000, 100_000, 20_000_000, 20_000_000) }
fn same(left: &State, right: &State) {
    assert_eq!(left.operator, right.operator); assert_eq!(left.rows, right.rows);
    assert_eq!(left.last_delta, right.last_delta); assert_eq!(left.frontier, right.frontier);
    assert_eq!(left.failure, right.failure); assert_eq!(left.stats, right.stats);
}

#[test]
fn every_composed_refusal_preserves_ordered_page_canonical_sink_and_last_delta() {
    let ((), report) = run_async_under_lab(0x7769_0110, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root); let cx = contexts.query(); let commit = contexts.commit();
        let keys = DatabaseKeys::new([0xd1; 32], DatabaseSecurityNamespaceId([0xd2; 32]), [0xd3; 32]);
        let mut db = Database::open_memory(&commit, keys).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for id in 1..=3 { seed.create_vertex(VId(id), vec![], vec![(PropertyKeyId(1), CanonicalScalar::Int(id as i64))]); }
        let basis = db.write(&commit, seed).await.unwrap();
        let definition = PreparedGraphText::prepare("MATCH (n) RETURN n.p AS p", |kind, name: &str| match (kind, name) {
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))), _ => None,
        }).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
        let source = db.register_standing_rows(&cx, definition, policy()).unwrap();
        let baseline = db.standing_rows(&cx, &source).unwrap().rows().checked_clone(LIMBS, &mut |_| Ok::<_, ()>(())).unwrap();
        let build = || {
            let spec = RowWindowSpec::new(vec![GraphSetColumnType::Scalar], vec![GraphValueOrder {
                column: 0, descending: true, nulls_first: false,
            }], GraphSetQuantifier::Distinct, 0, 2).unwrap();
            let mut state = State { input: source.index, columns: vec!["p".into()],
                operator: IncrementalRowWindow::new(spec), rows: ZSet::new(), last_delta: None,
                policy: policy(), frontier: basis, stats: StandingQueryStats::default(), failure: None };
            let mut checkpoint = || Ok(());
            let mut meter = Meter { policy: policy(), stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
            state.apply(&baseline, &mut meter).unwrap(); state.last_delta = None; state
        };
        let before = build();
        let mut change = WriteBatch::new(RelationId(1)); change.delete_vertex(VId(3));
        db.write(&commit, change).await.unwrap();
        let batch = db.delta_since(basis).unwrap().next().unwrap();
        let mut success = build(); let mut calls = 0;
        let stats = {
            let mut checkpoint = || { calls += 1; Ok(()) };
            let mut meter = Meter { policy: policy(), stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
            success.maintain(batch, &db.standing_queries, &mut meter).unwrap(); meter.stats
        };
        for stop in 1..=calls {
            let mut state = build(); let mut seen = 0;
            let mut checkpoint = || { seen += 1; if seen == stop { Err(StandingQueryFailure::Interrupted) } else { Ok(()) } };
            let mut meter = Meter { policy: policy(), stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
            assert_eq!(state.maintain(batch, &db.standing_queries, &mut meter), Err(StandingQueryFailure::Interrupted));
            same(&state, &before);
        }
        for (work, scratch, expected) in [(stats.work_units, stats.scratch_entries, None),
            (stats.work_units - 1, stats.scratch_entries, Some(StandingQueryFailure::WorkBudget)),
            (stats.work_units, stats.scratch_entries - 1, Some(StandingQueryFailure::ScratchBudget))] {
            let mut state = build(); let mut checkpoint = || Ok(());
            let mut meter = Meter { policy: GqlQueryPolicy::new(100, 2, work, scratch),
                stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
            let result = state.maintain(batch, &db.standing_queries, &mut meter);
            if let Some(error) = expected { assert_eq!(result, Err(error)); same(&state, &before); }
            else { result.unwrap(); same(&state, &success); }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
