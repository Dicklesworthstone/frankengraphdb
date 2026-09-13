//! Disconnected text MATCH over real Chronicle/Strata and canonical overlays.
//! New independent vertices must remain phantom dependencies, even after a
//! refusal or LIMIT 0. Expected joins are derived directly from stored rows.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, VertexRow, WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValueRow, PreparedGraphPattern};
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateRow,
    GraphSymbol, GraphSymbolKind, PreparedGraphAggregate, PreparedGraphAggregateText, PreparedGraphText};
use fgdb_types::{CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use std::collections::{BTreeMap, BTreeSet};

const COMPANY: LabelId = LabelId(1);
const FILING: LabelId = LabelId(2);
const CIK: PropertyKeyId = PropertyKeyId(1);
const SCORE: PropertyKeyId = PropertyKeyId(2);
const R: RelationId = RelationId(1);
const HEAD: &str = "MATCH (company:Company),(filing:Filing) \
    WHERE company.cik = filing.cik AND filing.score >= $floor";
type Pair = (VId, VId, i64);
type Summary = (VId, u64, u64, i128);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xd1; 32], DatabaseSecurityNamespaceId([0xd2; 32]), [0xd3; 32])
}
fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(1_000, 1_000, 5_000_000, 2_000_000) }
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Company") => Some(GraphSymbol::Label(COMPANY)),
        (GraphSymbolKind::Label, "Filing") => Some(GraphSymbol::Label(FILING)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "cik") => Some(GraphSymbol::Property(CIK)),
        (GraphSymbolKind::Property, "score") => Some(GraphSymbol::Property(SCORE)),
        _ => None,
    }
}
fn arguments(floor: i64) -> GqlParameters { GqlParameters::new().with_int64("floor", floor).unwrap() }
fn pattern(floor: i64, limit: Option<u64>) -> PreparedGraphPattern<GraphValueRow> {
    let tail = limit.map_or(String::new(), |limit| format!(" LIMIT {limit}"));
    PreparedGraphText::prepare(&format!("{HEAD} RETURN company,filing,filing.score AS score \
        ORDER BY company,filing{tail}"), symbols).unwrap().bind_parameters(&arguments(floor)).unwrap()
}
fn aggregate(floor: i64) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(&format!("{HEAD} \
        RETURN company,COUNT(*) AS filings,COUNT(DISTINCT filing) AS unique_filings,SUM(filing.score) AS total \
        GROUP BY company ORDER BY company"), symbols).unwrap().bind_parameters(&arguments(floor)).unwrap()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut batch = WriteBatch::new(R);
    for (id, cik) in [(1, 10), (2, 20)] {
        batch.create_vertex(VId(id), vec![COMPANY], vec![(CIK, CanonicalScalar::Int(cik))]);
    }
    for (id, cik, score) in [(10, 10, 4), (11, 10, 6), (12, 20, 8), (15, 30, 12)] {
        batch.create_vertex(VId(id), vec![FILING],
            vec![(CIK, CanonicalScalar::Int(cik)), (SCORE, CanonicalScalar::Int(score))]);
    }
    batch.create_vertex(VId(13), vec![FILING], vec![(CIK, CanonicalScalar::Null), (SCORE, CanonicalScalar::Int(9))]);
    batch.create_vertex(VId(14), vec![FILING], vec![(SCORE, CanonicalScalar::Int(10))]);
    db.write(cx, batch).await.unwrap()
}
fn integer(row: &VertexRow, key: PropertyKeyId) -> Option<i64> {
    row.props.iter().find_map(|(actual, value)| match value {
        CanonicalScalar::Int(value) if *actual == key => Some(*value), _ => None,
    })
}
fn oracle(vertices: &[VertexRow], floor: i64) -> Vec<Pair> {
    let mut rows = Vec::new();
    for company in vertices.iter().filter(|row| row.labels.contains(&COMPANY)) {
        let Some(cik) = integer(company, CIK) else { continue; };
        for filing in vertices.iter().filter(|row| row.labels.contains(&FILING)) {
            if integer(filing, CIK) != Some(cik) { continue; }
            if let Some(score) = integer(filing, SCORE).filter(|score| *score >= floor) {
                rows.push((company.vid, filing.vid, score));
            }
        }
    }
    rows.sort(); rows
}
fn plain(rows: &[GraphValueRow]) -> Vec<Pair> {
    rows.iter().map(|row| {
        let CanonicalScalar::Int(score) = row.values()[2].as_scalar().unwrap() else { panic!("fixture score") };
        (row.values()[0].as_vertex().unwrap(), row.values()[1].as_vertex().unwrap(), *score)
    }).collect()
}
fn expected_summary(rows: &[Pair]) -> Vec<Summary> {
    let mut groups = BTreeMap::<VId, (u64, BTreeSet<VId>, i128)>::new();
    for &(company, filing, score) in rows {
        let group = groups.entry(company).or_default();
        group.0 += 1; group.1.insert(filing); group.2 += i128::from(score);
    }
    groups.into_iter().map(|(company, (count, unique, sum))| (company, count, unique.len() as u64, sum)).collect()
}
fn summary(rows: &[GraphAggregateRow]) -> Vec<Summary> {
    rows.iter().map(|row| (row.keys()[0].as_vertex().unwrap(), row.values()[0].as_count().unwrap(),
        row.values()[1].as_count().unwrap(), row.values()[2].as_integer().unwrap())).collect()
}

#[test]
fn isolated_joins_and_aggregates_share_live_historical_pinned_and_staged_sources() {
    let ((), report) = run_async_under_lab(0xd15c_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txn_cx = contexts.txn();
        let vfs = MemVfs::new().unwrap(); let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        assert!(db.edges().unwrap().is_empty(), "all filings are isolated vertices");
        let view = db.read_session().unwrap(); let query = pattern(0, None); let grouped = aggregate(0);
        let frozen = query.canonical_bytes(); let old = oracle(&db.vertices().unwrap(), 0);
        assert_eq!(old, vec![(VId(1), VId(10), 4), (VId(1), VId(11), 6), (VId(2), VId(12), 8)]);
        let mut txn = db.begin(&txn_cx).unwrap();
        for result in [db.execute_graph_pattern_governed(&cx, &query, policy()).unwrap(),
            db.execute_graph_pattern_governed_at(&cx, &query, basis, policy()).unwrap(),
            view.execute_graph_pattern_governed(&cx, &query, policy()).unwrap(),
            view.execute_graph_pattern_governed_at(&cx, &query, basis, policy()).unwrap(),
            txn.execute_graph_pattern_governed(&db, &cx, &query, policy()).unwrap()] {
            assert_eq!(plain(&result.value), old); assert_eq!(result.rows.snapshot_records, 8);
        }
        for result in [db.execute_graph_aggregate_governed(&cx, &grouped, policy()).unwrap(),
            db.execute_graph_aggregate_governed_at(&cx, &grouped, basis, policy()).unwrap(),
            view.execute_graph_aggregate_governed(&cx, &grouped, policy()).unwrap(),
            txn.execute_graph_aggregate_governed(&db, &cx, &grouped, policy()).unwrap()] {
            assert_eq!(summary(&result.value), expected_summary(&old));
        }
        let filtered = pattern(5, None);
        assert_eq!(plain(&db.execute_graph_pattern_governed(&cx, &filtered, policy()).unwrap().value),
            oracle(&db.vertices().unwrap(), 5));
        let mut changes = WriteBatch::new(R);
        changes.set_vertex_property(VId(10), SCORE, Some(CanonicalScalar::Int(40)));
        changes.set_vertex_property(VId(11), CIK, None);
        changes.create_vertex(VId(16), vec![FILING], vec![(CIK, CanonicalScalar::Int(20)), (SCORE, CanonicalScalar::Int(5))]);
        txn.write(&mut db, changes).unwrap();
        let expected = oracle(&txn.vertices(&db).unwrap(), 0);
        assert_eq!(expected_summary(&expected), vec![(VId(1), 1, 1, 40), (VId(2), 2, 2, 13)]);
        assert_eq!(plain(&txn.execute_graph_pattern_governed(&db, &cx, &query, policy()).unwrap().value), expected);
        assert_eq!(summary(&txn.execute_graph_aggregate_governed(&db, &cx, &grouped, policy()).unwrap().value), expected_summary(&expected));
        assert_eq!(plain(&db.execute_graph_pattern_governed(&cx, &query, policy()).unwrap().value), old);
        txn.commit(&mut db, &commit).await.unwrap();
        db.compact(&commit).await.unwrap(); drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert_eq!(plain(&reopened.execute_graph_pattern_governed(&cx, &query, policy()).unwrap().value), expected);
        assert_eq!(summary(&reopened.execute_graph_aggregate_governed(&cx, &grouped, policy()).unwrap().value), expected_summary(&expected));
        assert_eq!(plain(&reopened.execute_graph_pattern_governed_at(&cx, &query, basis, policy()).unwrap().value), old);
        assert_eq!(summary(&reopened.execute_graph_aggregate_governed_at(&cx, &grouped, basis, policy()).unwrap().value), expected_summary(&old));
        assert_eq!(plain(&view.execute_graph_pattern_governed(&cx, &query, policy()).unwrap().value), old);
        assert_eq!(query.canonical_bytes(), frozen);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn mixed_edge_and_vertex_components_admit_isolates_and_all_policy_dimensions() {
    let ((), report) = run_async_under_lab(0xd15c_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap(); seed(&mut db, &commit).await;
        let mut edges = WriteBatch::new(R);
        edges.add_edge(EId(1), VId(1), VId(1), vec![]);
        edges.add_edge(EId(2), VId(1), VId(1), vec![]);
        edges.add_edge(EId(3), VId(2), VId(2), vec![]);
        db.write(&commit, edges).await.unwrap();
        let text = "MATCH (company:Company)-[:R]->(company),(filing:Filing) \
            WHERE company.cik = filing.cik RETURN company,filing,filing.score AS score ORDER BY company,filing";
        let query = PreparedGraphText::prepare(text, symbols).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
        assert!(!query.plan().scans_edges()); assert!(query.plan().reads_edges());
        assert_eq!(query.required_vertex_label(), None);
        let run = |cap| db.execute_graph_pattern_governed(&cx, &query, cap);
        let result = run(policy()).unwrap();
        assert_eq!(plain(&result.value), vec![(VId(1), VId(10), 4), (VId(1), VId(10), 4),
            (VId(1), VId(11), 6), (VId(1), VId(11), 6), (VId(2), VId(12), 8)]);
        assert_eq!(result.rows.snapshot_records, 11);
        let work = result.evaluator.work_units; let scratch = result.evaluator.scratch_entries;
        assert_eq!(run(GqlQueryPolicy::new(11, 5, work, scratch)).unwrap(), result);
        for cap in [GqlQueryPolicy::new(10, 5, work, scratch), GqlQueryPolicy::new(11, 4, work, scratch),
            GqlQueryPolicy::new(11, 5, work - 1, scratch), GqlQueryPolicy::new(11, 5, work, scratch - 1)] {
            assert!(run(cap).is_err());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn independent_insert_and_property_phantoms_conflict_after_success_refusal_or_limit_zero() {
    let ((), report) = run_async_under_lab(0xd15c_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txn_cx = contexts.txn();
        for mode in 0..4 {
            for change in 0..3 {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap(); seed(&mut db, &commit).await;
                let mut txn = db.begin(&txn_cx).unwrap();
                let mut stage = WriteBatch::new(R); stage.create_vertex(VId(777), vec![], vec![]);
                txn.write(&mut db, stage).unwrap();
                if mode == 3 {
                    let grouped = aggregate(0);
                    assert_eq!(summary(&txn.execute_graph_aggregate_governed(&db, &cx, &grouped, policy()).unwrap().value),
                        vec![(VId(1), 2, 2, 10), (VId(2), 1, 1, 8)]);
                } else {
                    let query = pattern(0, (mode == 2).then_some(0));
                    let cap = GqlQueryPolicy::new(1_000, if mode == 1 { 0 } else { 1_000 }, 5_000_000, 2_000_000);
                    let result = txn.execute_graph_pattern_governed(&db, &cx, &query, cap);
                    if mode == 1 { assert!(matches!(result, Err(GqlQueryError::Rows(_)))); }
                    else { assert_eq!(result.unwrap().value.len(), if mode == 2 { 0 } else { 3 }); }
                }
                let mut winner = WriteBatch::new(R);
                match change {
                    0 => { winner.create_vertex(VId(888), vec![FILING],
                        vec![(CIK, CanonicalScalar::Int(10)), (SCORE, CanonicalScalar::Int(1))]); }
                    1 => { winner.set_vertex_property(VId(15), CIK, Some(CanonicalScalar::Int(10))); }
                    _ => { winner.set_vertex_property(VId(14), CIK, Some(CanonicalScalar::Int(20))); }
                }
                db.write(&commit, winner).await.unwrap(); let frontier = db.frontier().unwrap();
                // No subsequent transaction read is allowed to repair a lost
                // witness before the authoritative FCW validation below.
                assert!(matches!(txn.commit(&mut db, &commit).await,
                    Err(WriteTxnError::Write(WriteError::FirstCommitterWins { law: "FG-LAW-FCW-READ-01", .. }))));
                assert_eq!(db.frontier().unwrap(), frontier);
                assert!(db.vertex(VId(777)).unwrap().is_none());
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn absent_optional_and_anti_join_witnesses_remain_vertex_phantom_dependencies() {
    let ((), report) = run_async_under_lab(0xd15c_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txn_cx = contexts.txn();
        for text in [
            "MATCH (company:Company) OPTIONAL MATCH (filing:Filing),(company) \
                WHERE filing.cik = company.cik RETURN company,filing",
            "MATCH (company:Company) WHERE NOT EXISTS { MATCH (filing:Filing),(company) \
                WHERE filing.cik = company.cik } RETURN company",
        ] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut data = WriteBatch::new(R);
            data.create_vertex(VId(1), vec![COMPANY], vec![(CIK, CanonicalScalar::Int(10))]);
            db.write(&commit, data).await.unwrap();
            let query = PreparedGraphText::prepare(text, symbols).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
            assert_eq!(query.required_vertex_label(), None);
            let mut txn = db.begin(&txn_cx).unwrap();
            let mut stage = WriteBatch::new(R); stage.create_vertex(VId(777), vec![], vec![]);
            txn.write(&mut db, stage).unwrap();
            let result = txn.execute_graph_pattern_governed(&db, &cx, &query, policy()).unwrap();
            assert_eq!(result.value.len(), 1);
            if result.value[0].values().len() == 2 { assert!(result.value[0].values()[1].is_null()); }
            let mut winner = WriteBatch::new(R);
            winner.create_vertex(VId(888), vec![FILING], vec![(CIK, CanonicalScalar::Int(10))]);
            db.write(&commit, winner).await.unwrap();
            assert!(matches!(txn.commit(&mut db, &commit).await,
                Err(WriteTxnError::Write(WriteError::FirstCommitterWins { law: "FG-LAW-FCW-READ-01", .. }))));
            assert!(db.vertex(VId(777)).unwrap().is_none());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
