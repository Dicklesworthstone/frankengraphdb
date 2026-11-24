//! Correlated scalar aggregates retain one summary per invocation, independent of outer multiplicity.
//! Literal fixture values exercise native COUNT bodies and returning CALL aggregates.

use super::*;
use crate::{DatabaseKeys, MemVfs, QueryResult, QueryValue, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LabelId, LimbLimit, PropertyKeyId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::row_join::RowJoinKind;
use fgdb_gql::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphSetText};
use fgdb_types::{
    CanonicalF64, CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts,
};
use std::collections::BTreeMap;

const P: PropertyKeyId = PropertyKeyId(1);
const SELECTED: PropertyKeyId = PropertyKeyId(2);
const R: RelationId = RelationId(1);
const L: LabelId = LabelId(1);
const M: LabelId = LabelId(2);
const LIMBS: LimbLimit = LimbLimit::new(4);
const COUNT_BODY: &str = concat!(
    "MATCH (a:L) UNWIND [7,7] AS token WITH token,a ",
    "RETURN token,a,COUNT { MATCH (a)-[:R]->(b:M) } AS degree"
);
const CALL_COUNT: &str = concat!(
    "MATCH (a:L) UNWIND [7,7] AS token WITH token,a ",
    "CALL (a) { MATCH (a)-[:R]->(b:M) RETURN count(*) AS degree } ",
    "RETURN token,a,degree"
);
const CALL_VALUE_COUNT: &str = concat!(
    "MATCH (a:L) UNWIND [7,7] AS token WITH token,a ",
    "CALL (a) { MATCH (a)-[:R]->(b:M) RETURN count(b) AS degree } ",
    "RETURN token,a,degree"
);
const CALL_DISTINCT_NEIGHBORS: &str = concat!(
    "MATCH (a:L) UNWIND [7,7] AS token WITH token,a ",
    "CALL (a) { MATCH (a)-[:R]->(b:M) RETURN count(DISTINCT b) AS degree } ",
    "RETURN token,a,degree"
);
const CALL_DISTINCT_EDGES: &str = concat!(
    "MATCH (a:L) UNWIND [7,7] AS token WITH token,a ",
    "CALL (a) { MATCH (a)-[r:R]->(b:M) RETURN count(DISTINCT r) AS degree } ",
    "RETURN token,a,degree"
);
const COUNT_DISTINCT_BODY: &str = concat!(
    "MATCH (a:L) UNWIND [7,7] AS token WITH token,a ",
    "RETURN token,a,COUNT { MATCH (a)-[:R]->(b:M) RETURN DISTINCT b } AS degree"
);
const COUNT_FILTER: &str = concat!(
    "MATCH (a:L) UNWIND [7,7] AS token WITH token,a ",
    "WHERE COUNT { MATCH (a)-[:R]->(b:M) } >= 2 RETURN token,a"
);
const COUNT_EXPRESSION: &str = concat!(
    "MATCH (a:L) UNWIND [7,7] AS token WITH token,a ",
    "RETURN token,a,COUNT { MATCH (a)-[:R]->(b:M) } + 1 AS next_degree"
);
const NULL_IMPORT: &str = concat!(
    "MATCH (a:L) WITH a OPTIONAL MATCH (a)-[:R]->(b:M) WITH a,b ",
    "RETURN a,b,COUNT { MATCH (b)-[:R]->(c:M) } AS degree"
);
const UNCORRELATED: &str = concat!(
    "MATCH (a:L) WITH a ",
    "RETURN a,COUNT { MATCH (b:M) } AS population"
);
type Bag = BTreeMap<GraphValueRow, i128>;

fn argument_summary_text(profile: usize) -> String {
    let body = match profile {
        0 => {
            "CALL (value) { RETURN count(*) AS degree,count(value) AS present,sum(value) AS total }"
        }
        1 => {
            "CALL () { UNWIND [1,1,NULL] AS item \
             RETURN count(*) AS degree,count(item) AS present,sum(item) AS total }"
        }
        2 => {
            "CALL (value) { \
             UNWIND CASE WHEN value IS NULL THEN [] ELSE [value,value,NULL] END AS item \
             RETURN count(*) AS degree,count(item) AS present,sum(item) AS total }"
        }
        3 => {
            "CALL (a,value) { MATCH (a)-[:R]->(b:M) \
             UNWIND CASE WHEN value IS NULL THEN [] ELSE [value,value,NULL] END AS item \
             RETURN count(*) AS degree,count(item) AS present,sum(item) AS total }"
        }
        _ => unreachable!(),
    };
    format!(
        "MATCH (a:L) UNWIND [7,7] AS token WITH a,token,a.p AS value \
         {body} RETURN a,degree,present,total"
    )
}

// The fixture supplies each owner's scalar and concrete edge-occurrence count.
// These small list summaries are independent of every production row/group
// operator; outer duplication is restored only in the public bag.
fn argument_summary_bag(profile: usize, entries: &[(u128, Option<i64>, i64)]) -> Bag {
    bag(entries.iter().map(|&(owner, value, witnesses)| {
        let (degree, present, total) = match (profile, value) {
            (0, value) => (1, i64::from(value.is_some()), value),
            (1, _) => (3, 2, Some(2)),
            (2, Some(value)) => (3, 2, Some(2 * value)),
            (3, Some(value)) if witnesses > 0 => {
                (3 * witnesses, 2 * witnesses, Some(2 * value * witnesses))
            }
            (2 | 3, _) => (0, 0, None),
            _ => unreachable!(),
        };
        (
            row(vec![
                vertex(Some(owner)),
                integer(degree),
                integer(present),
                total.map_or(vertex(None), integer),
            ]),
            2,
        )
    }))
}

#[test]
fn argument_and_unwind_summaries_follow_real_property_edge_and_outer_changes() {
    let ((), report) = run_async_under_lab(0x756e_7701, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0x73)).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let initial = [(1, Some(10), 2), (3, Some(10), 1), (5, Some(10), 0)];
        let mut definitions = Vec::new();
        let mut expected = Vec::new();
        for profile in 0..4 {
            let text = argument_summary_text(profile);
            let query = bind(&text);
            let handle = db
                .register_standing_native(&cx, &text, &GqlParameters::new(), symbols, policy())
                .unwrap();
            let rows = argument_summary_bag(profile, &initial);
            check(&db, &cx, &text, &query, &handle, &rows, policy());
            definitions.push((text, query, handle));
            expected.push(rows);
        }
        for phase in 0..9 {
            let from = db.frontier().unwrap();
            let mut edit = WriteBatch::new(R);
            match phase {
                0 => edit.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(2))),
                1 => edit.set_vertex_property(VId(3), P, None),
                2 => edit.delete_edge(EId(11)),
                3 => {
                    edit.delete_edge(EId(12));
                    edit.add_edge(EId(15), VId(5), VId(2), vec![])
                }
                4 => edit.set_vertex_label(VId(5), L, false),
                5 => edit.delete_vertex(VId(1)),
                6 => {
                    edit.set_vertex_label(VId(5), L, true);
                    edit.set_vertex_property(VId(5), P, Some(CanonicalScalar::Int(0)))
                }
                7 => {
                    edit.set_vertex_label(VId(3), L, false);
                    edit.set_vertex_label(VId(5), L, false)
                }
                8 => {
                    edit.create_vertex(VId(u128::MAX), vec![L], vec![(P, CanonicalScalar::Int(3))])
                }
                _ => unreachable!(),
            };
            db.write(&commit, edit).await.unwrap();
            let entries = match phase {
                0 => vec![(1, Some(2), 2), (3, Some(10), 1), (5, Some(10), 0)],
                1 => vec![(1, Some(2), 2), (3, None, 1), (5, Some(10), 0)],
                2 => vec![(1, Some(2), 1), (3, None, 1), (5, Some(10), 0)],
                3 => vec![(1, Some(2), 0), (3, None, 1), (5, Some(10), 1)],
                4 => vec![(1, Some(2), 0), (3, None, 1)],
                5 => vec![(3, None, 1)],
                6 => vec![(3, None, 1), (5, Some(0), 1)],
                7 => vec![],
                8 => vec![(u128::MAX, Some(3), 0)],
                _ => unreachable!(),
            };
            for (profile, ((text, query, handle), before)) in
                definitions.iter().zip(&mut expected).enumerate()
            {
                let after = argument_summary_bag(profile, &entries);
                check(&db, &cx, text, query, handle, &after, policy());
                check_delta(&db, &cx, handle, from, before, &after);
                *before = after;
            }
        }
        for ((text, query, handle), rows) in definitions.iter().zip(&expected) {
            db.rebuild_standing_query(&cx, handle, policy()).unwrap();
            check(&db, &cx, text, query, handle, rows, policy());
        }
        super::tests::check_first_tick_after_rebuild(
            &mut db,
            &cx,
            &commit,
            definitions.iter().map(|entry| &entry.2),
        )
        .await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn expanded_group_text(optional: bool) -> String {
    let call = if optional { "OPTIONAL CALL" } else { "CALL" };
    format!(
        "MATCH (a:L) UNWIND [7,7] AS token WITH a,token,a.p AS value \
         {call} (value) {{ UNWIND range(1,value) AS item \
         WITH item%2 AS bucket,count(*) AS degree,sum(item) AS total \
         UNWIND [total,total] AS copied RETURN bucket,degree,copied }} \
         RETURN a,bucket,degree,copied"
    )
}

fn expanded_group_bag(owners: &[u128], groups: &[(u128, i64, i64, i64)], optional: bool) -> Bag {
    let mut result = Bag::new();
    for &owner in owners {
        let mut emitted = false;
        for &(group_owner, bucket, degree, total) in groups {
            if owner == group_owner {
                emitted = true;
                result.insert(
                    row(vec![
                        vertex(Some(owner)),
                        integer(bucket),
                        integer(degree),
                        integer(total),
                    ]),
                    4,
                );
            }
        }
        if optional && !emitted {
            result.insert(
                row(vec![
                    vertex(Some(owner)),
                    vertex(None),
                    vertex(None),
                    vertex(None),
                ]),
                2,
            );
        }
    }
    result
}

#[test]
fn grouped_unwind_before_and_after_summary_preserves_invocations_and_optional_absence() {
    let ((), report) = run_async_under_lab(0x756e_7702, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0x74)).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let initial = [
            (1, 0, 5, 30),
            (1, 1, 5, 25),
            (3, 0, 5, 30),
            (3, 1, 5, 25),
            (5, 0, 5, 30),
            (5, 1, 5, 25),
        ];
        let mut definitions = Vec::new();
        for optional in [false, true] {
            let text = expanded_group_text(optional);
            let query = bind(&text);
            let handle = db
                .register_standing_native(&cx, &text, &GqlParameters::new(), symbols, policy())
                .unwrap();
            let expected = expanded_group_bag(&[1, 3, 5], &initial, optional);
            check(&db, &cx, &text, &query, &handle, &expected, policy());
            definitions.push((text, query, handle, optional, expected));
        }
        for phase in 0..5 {
            let from = db.frontier().unwrap();
            let mut edit = WriteBatch::new(R);
            match phase {
                0 => edit.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(3))),
                1 => edit.set_vertex_property(VId(3), P, None),
                2 => edit.set_vertex_property(VId(5), P, Some(CanonicalScalar::Int(0))),
                3 => edit.set_vertex_label(VId(1), L, false),
                4 => edit.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(1))),
                _ => unreachable!(),
            };
            db.write(&commit, edit).await.unwrap();
            let groups = match phase {
                0 => vec![
                    (1, 0, 1, 2),
                    (1, 1, 2, 4),
                    (3, 0, 5, 30),
                    (3, 1, 5, 25),
                    (5, 0, 5, 30),
                    (5, 1, 5, 25),
                ],
                1 => vec![(1, 0, 1, 2), (1, 1, 2, 4), (5, 0, 5, 30), (5, 1, 5, 25)],
                2 => vec![(1, 0, 1, 2), (1, 1, 2, 4)],
                3 => vec![],
                4 => vec![(3, 1, 1, 1)],
                _ => unreachable!(),
            };
            let owners: &[u128] = if phase < 3 { &[1, 3, 5] } else { &[3, 5] };
            for (text, query, handle, optional, before) in &mut definitions {
                let after = expanded_group_bag(owners, &groups, *optional);
                check(&db, &cx, text, query, handle, &after, policy());
                check_delta(&db, &cx, handle, from, before, &after);
                *before = after;
            }
        }
        for (text, query, handle, _, rows) in &definitions {
            db.rebuild_standing_query(&cx, handle, policy()).unwrap();
            check(&db, &cx, text, query, handle, rows, policy());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn source_free_unwind_fanout_spends_only_public_summary_quota() {
    let ((), report) = run_async_under_lab(0x756e_7703, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0x75)).await.unwrap();
        let mut initial = WriteBatch::new(R);
        initial.create_vertex(VId(1), vec![L], vec![]);
        db.write(&commit, initial).await.unwrap();
        let text = concat!(
            "MATCH (a:L) UNWIND [7,7] AS token WITH a,token ",
            "CALL () { UNWIND range(1,128) AS item ",
            "RETURN count(*) AS degree,sum(item) AS total } RETURN a,degree,total"
        );
        let two = GqlQueryPolicy::new(1_000, 2, 50_000_000, 50_000_000);
        let parameters = GqlParameters::new();
        let prepared = PreparedNativeRead::prepare(text, &parameters, symbols).unwrap();
        let (nodes, _) = prepared.subscription_footprint(&cx, &parameters).unwrap();
        let first = db.standing_queries.len();
        let handle = prepared
            .register_standing(&mut db, &cx, &parameters, two)
            .unwrap();
        // The footprint adds exactly its documented unused terminal-window
        // reservation; each private expansion/reorder owner is counted once.
        assert_eq!(
            nodes,
            u64::try_from(db.standing_queries.len() - first + 1).unwrap()
        );
        assert_eq!(
            db.standing_queries[first..]
                .iter()
                .filter(|query| matches!(query, StandingQuery::Constant(_)))
                .count(),
            1
        );
        let query = bind(text);
        let expected = bag([(row(vec![vertex(Some(1)), integer(128), integer(8256)]), 2)]);
        check(&db, &cx, text, &query, &handle, &expected, two);
        let before = saved(&db.standing_queries);
        for refused in [
            GqlQueryPolicy::new(1_000, 2, 0, 50_000_000),
            GqlQueryPolicy::new(1_000, 2, 50_000_000, 0),
            GqlQueryPolicy::new(1_000, 1, 50_000_000, 50_000_000),
        ] {
            assert!(
                prepared
                    .register_standing(&mut db, &cx, &parameters, refused)
                    .is_err()
            );
            assert_eq!(saved(&db.standing_queries), before);
            assert!(db.rebuild_standing_query(&cx, &handle, refused).is_err());
            assert_eq!(saved(&db.standing_queries), before);
        }
        let from = db.frontier().unwrap();
        let mut edit = WriteBatch::new(R);
        edit.set_vertex_label(VId(1), L, false);
        db.write(&commit, edit).await.unwrap();
        check(&db, &cx, text, &query, &handle, &Bag::new(), two);
        check_delta(&db, &cx, &handle, from, &expected, &Bag::new());
        db.rebuild_standing_query(&cx, &handle, two).unwrap();
        check(&db, &cx, text, &query, &handle, &Bag::new(), two);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn empty_outer_skips_source_free_unwind_errors_and_rebuild_restores_demand() {
    let ((), report) = run_async_under_lab(0x756e_7704, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0x76)).await.unwrap();
        let text = concat!(
            "MATCH (a:L) WITH a CALL () { UNWIND [1/0] AS item ",
            "RETURN count(*) AS degree } RETURN a,degree"
        );
        let query = bind(text);
        let sibling = db
            .register_standing_native(
                &cx,
                "MATCH (a:L) RETURN a",
                &GqlParameters::new(),
                symbols,
                policy(),
            )
            .unwrap();
        let handle = db
            .register_standing_native(&cx, text, &GqlParameters::new(), symbols, policy())
            .unwrap();
        check(&db, &cx, text, &query, &handle, &Bag::new(), policy());
        let mut edit = WriteBatch::new(R);
        edit.create_vertex(VId(1), vec![L], vec![]);
        db.write(&commit, edit).await.unwrap();
        assert!(db.standing_native_query(&cx, &handle, policy()).is_err());
        assert!(db.standing_native_query(&cx, &sibling, policy()).is_ok());
        assert!(db.standing_queries.iter().any(|query| matches!(
            query.status().2,
            Some(StandingQueryFailure::OutputExpression {
                error: fgdb_gql::GraphIntegerError {
                    kind: fgdb_gql::GraphIntegerErrorKind::DivisionByZero,
                    ..
                },
                ..
            })
        )));
        let before = saved(&db.standing_queries);
        assert_summary_division_by_zero(
            db.register_standing_native(&cx, text, &GqlParameters::new(), symbols, policy())
                .unwrap_err(),
        );
        assert_eq!(saved(&db.standing_queries), before);
        assert!(db.rebuild_standing_query(&cx, &handle, policy()).is_err());
        assert_eq!(saved(&db.standing_queries), before);
        let mut edit = WriteBatch::new(R);
        edit.set_vertex_label(VId(1), L, false);
        db.write(&commit, edit).await.unwrap();
        db.rebuild_standing_query(&cx, &handle, policy()).unwrap();
        check(&db, &cx, text, &query, &handle, &Bag::new(), policy());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn grouped_call_text(optional: bool, filtered: bool) -> String {
    let call = if optional { "OPTIONAL CALL" } else { "CALL" };
    let summary = if filtered {
        "WITH value AS category,id(b)%3 AS bucket,count(*) AS degree,\
         sum(id(b)) AS total,min(b) AS smallest WHERE degree>1 \
         RETURN category,bucket,degree,total,smallest"
    } else {
        "RETURN value AS category,id(b)%3 AS bucket,count(*) AS degree,\
         sum(id(b)) AS total,min(b) AS smallest"
    };
    format!(
        "MATCH (a:L) UNWIND [7,7] AS token WITH a,token,a.p AS value \
         {call} (a,value) {{ MATCH (a)-[:R]->(b:M) {summary} }} \
         RETURN a,category,bucket,degree,total,smallest"
    )
}

// Owner, nullable category, bucket, degree, sum, and smallest vertex.
type ExpectedOwnerGroup = (u128, Option<i64>, i64, i64, i64, u128);

// All group values are fixture literals. This only places them in the public
// schema and restores the specified two outer occurrences per active owner.
fn grouped_call_bag(
    owners: &[u128],
    groups: &[ExpectedOwnerGroup],
    optional: bool,
    filtered: bool,
) -> Bag {
    let mut result = Bag::new();
    for &owner in owners {
        let mut emitted = false;
        for &(group_owner, category, bucket, degree, total, smallest) in groups {
            if group_owner != owner || (filtered && degree <= 1) {
                continue;
            }
            emitted = true;
            result.insert(
                row(vec![
                    vertex(Some(owner)),
                    category.map_or(vertex(None), integer),
                    integer(bucket),
                    integer(degree),
                    integer(total),
                    vertex(Some(smallest)),
                ]),
                2,
            );
        }
        if optional && !emitted {
            let mut values = vec![vertex(Some(owner))];
            values.extend(std::iter::repeat_n(vertex(None), 5));
            result.insert(row(values), 2);
        }
    }
    result
}

#[test]
fn grouped_correlated_summaries_follow_group_lifetimes_and_restore_optional_occurrences() {
    let ((), report) = run_async_under_lab(0x6772_7021, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0x71)).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let mut queries = Vec::new();
        for optional in [false, true] {
            for filtered in [false, true] {
                let text = grouped_call_text(optional, filtered);
                let query = bind(&text);
                let handle = db
                    .register_standing_native(&cx, &text, &GqlParameters::new(), symbols, policy())
                    .unwrap();
                let expected = grouped_call_bag(
                    &[1, 3, 5],
                    &[(1, Some(10), 2, 2, 4, 2), (3, Some(10), 1, 1, 4, 4)],
                    optional,
                    filtered,
                );
                check(&db, &cx, &text, &query, &handle, &expected, policy());
                queries.push((text, query, handle, optional, filtered, expected));
            }
        }
        for phase in 0..6 {
            let from = db.frontier().unwrap();
            let mut change = WriteBatch::new(R);
            match phase {
                0 => change.add_edge(EId(15), VId(1), VId(4), vec![]),
                1 => {
                    change.set_vertex_property(VId(1), P, None);
                    change.delete_edge(EId(11))
                }
                2 => change.delete_edge(EId(12)),
                3 => change.delete_edge(EId(15)),
                4 => change.set_vertex_label(VId(4), M, false),
                5 => change.set_vertex_label(VId(5), L, false),
                _ => unreachable!(),
            };
            db.write(&commit, change).await.unwrap();
            let groups = match phase {
                0 => vec![
                    (1, Some(10), 2, 2, 4, 2),
                    (1, Some(10), 1, 1, 4, 4),
                    (3, Some(10), 1, 1, 4, 4),
                ],
                1 => vec![
                    (1, None, 2, 1, 2, 2),
                    (1, None, 1, 1, 4, 4),
                    (3, Some(10), 1, 1, 4, 4),
                ],
                2 => vec![(1, None, 1, 1, 4, 4), (3, Some(10), 1, 1, 4, 4)],
                3 => vec![(3, Some(10), 1, 1, 4, 4)],
                4 | 5 => Vec::new(),
                _ => unreachable!(),
            };
            let owners: &[u128] = if phase == 5 { &[1, 3] } else { &[1, 3, 5] };
            for (text, query, handle, optional, filtered, expected) in &mut queries {
                let next = grouped_call_bag(owners, &groups, *optional, *filtered);
                check(&db, &cx, text, query, handle, &next, policy());
                check_delta(&db, &cx, handle, from, expected, &next);
                *expected = next;
            }
        }
        for (text, query, handle, _, _, expected) in &queries {
            db.rebuild_standing_query(&cx, handle, policy()).unwrap();
            check(&db, &cx, text, query, handle, expected, policy());
        }
        super::tests::check_first_tick_after_rebuild(
            &mut db,
            &cx,
            &commit,
            queries.iter().map(|entry| &entry.2),
        )
        .await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn grouped_witnesses_preserve_public_quotas_and_failed_registration_atomicity() {
    let ((), report) = run_async_under_lab(0x6772_7022, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0x72)).await.unwrap();
        let mut initial = WriteBatch::new(R);
        initial.create_vertex(VId(1), vec![L], vec![]);
        initial.create_vertex(VId(2), vec![M], vec![(SELECTED, CanonicalScalar::Int(1))]);
        for edge in 10..74 {
            initial.add_edge(EId(edge), VId(1), VId(2), vec![]);
        }
        db.write(&commit, initial).await.unwrap();
        let text = concat!(
            "MATCH (a:L) UNWIND [1,1,2,2] AS token WITH token,a ",
            "OPTIONAL CALL (a) { MATCH (a)-[:R]->(b:M {selected:1}) ",
            "RETURN id(b)%3 AS bucket,count(*) AS degree,sum(id(b)) AS total ",
            "} RETURN token,a,bucket,degree,total"
        );
        let four = GqlQueryPolicy::new(1_000, 4, 50_000_000, 50_000_000);
        let query = bind(text);
        let handle = db
            .register_standing_native(&cx, text, &GqlParameters::new(), symbols, four)
            .unwrap();
        let expected = bag((1..=2).map(|token| {
            (
                row(vec![
                    integer(token),
                    vertex(Some(1)),
                    integer(2),
                    integer(64),
                    integer(128),
                ]),
                2,
            )
        }));
        check(&db, &cx, text, &query, &handle, &expected, four);
        let before = saved(&db.standing_queries);
        for refused in [
            GqlQueryPolicy::new(1_000, 4, 0, 50_000_000),
            GqlQueryPolicy::new(1_000, 4, 50_000_000, 0),
            GqlQueryPolicy::new(1_000, 3, 50_000_000, 50_000_000),
        ] {
            assert!(
                db.register_standing_native(&cx, text, &GqlParameters::new(), symbols, refused)
                    .is_err()
            );
            assert_eq!(saved(&db.standing_queries), before);
            assert!(db.rebuild_standing_query(&cx, &handle, refused).is_err());
            assert_eq!(saved(&db.standing_queries), before);
        }
        check_delivery_limits(&db, &cx, &handle, &expected);
        assert_eq!(saved(&db.standing_queries), before);
        let from = db.frontier().unwrap();
        let mut change = WriteBatch::new(R);
        change.set_vertex_property(VId(2), SELECTED, None);
        db.write(&commit, change).await.unwrap();
        let empty = bag((1..=2).map(|token| {
            (
                row(vec![
                    integer(token),
                    vertex(Some(1)),
                    vertex(None),
                    vertex(None),
                    vertex(None),
                ]),
                2,
            )
        }));
        check(&db, &cx, text, &query, &handle, &empty, four);
        check_delta(&db, &cx, &handle, from, &expected, &empty);
        db.rebuild_standing_query(&cx, &handle, four).unwrap();
        check(&db, &cx, text, &query, &handle, &empty, four);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 50_000_000, 50_000_000)
}

fn keys(tag: u8) -> DatabaseKeys {
    DatabaseKeys::new([tag; 32], DatabaseSecurityNamespaceId([tag; 32]), [tag; 32])
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Label, "M") => Some(GraphSymbol::Label(M)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "selected") => Some(GraphSymbol::Property(SELECTED)),
        _ => None,
    }
}

fn bind(text: &str) -> PreparedGraphSet {
    PreparedGraphSetText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}

fn integer(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

fn vertex(value: Option<u128>) -> GraphValue {
    value.map_or(GraphValue::Scalar(CanonicalScalar::Null), |id| {
        GraphValue::Vertex(VId(id))
    })
}

fn row(values: Vec<GraphValue>) -> GraphValueRow {
    GraphValueRow::from_owned_values(values)
}

fn bag(entries: impl IntoIterator<Item = (GraphValueRow, i128)>) -> Bag {
    entries.into_iter().collect()
}

fn occurrences(rows: impl IntoIterator<Item = GraphValueRow>) -> Bag {
    let mut result = Bag::new();
    for row in rows {
        *result.entry(row).or_default() += 1;
    }
    result
}

fn cells(values: Vec<QueryValue>) -> GraphValueRow {
    row(values
        .into_iter()
        .map(|value| match value {
            QueryValue::Value(value) => value,
            _ => panic!("a native relational aggregate retains its value cells"),
        })
        .collect())
}

fn native_bag(rows: &ZSet<Vec<QueryValue>>) -> Bag {
    rows.iter()
        .map(|(row, weight)| (cells(row.clone()), weight.to_i128().unwrap()))
        .collect()
}

fn difference(before: &Bag, after: &Bag) -> Bag {
    let mut delta = after.clone();
    for (row, weight) in before {
        *delta.entry(row.clone()).or_default() -= weight;
    }
    delta.retain(|_, weight| *weight != 0);
    delta
}

fn assert_summary_division_by_zero(error: StandingQueryError) {
    assert!(
        matches!(
            &error,
            StandingQueryError::Maintenance(StandingQueryFailure::OutputExpression {
                error: fgdb_gql::GraphIntegerError {
                    kind: fgdb_gql::GraphIntegerErrorKind::DivisionByZero,
                    ..
                },
                ..
            })
        ),
        "the admitted summary must execute its projection and raise division by zero: {error:?}",
    );
}

fn check(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    text: &str,
    query: &PreparedGraphSet,
    handle: &StandingQueryHandle,
    expected: &Bag,
    policy: GqlQueryPolicy,
) {
    assert_eq!(
        occurrences(
            db.execute_graph_set_governed(cx, query, policy)
                .unwrap()
                .value
        ),
        *expected,
        "{text}"
    );
    let (at, rows) = db.standing_native_bag(cx, handle, policy).unwrap();
    assert_eq!(at, db.frontier().unwrap());
    assert_eq!(native_bag(&rows), *expected, "{text}");
    assert_eq!(
        db.standing_native_columns(cx, handle).unwrap(),
        query.columns()
    );
    for result in [
        db.query(cx, text, &GqlParameters::new(), symbols, policy)
            .unwrap(),
        db.standing_native_query(cx, handle, policy).unwrap().1,
    ] {
        let QueryResult::Rows { columns, rows } = result else {
            panic!("native correlated aggregates return rows");
        };
        assert_eq!(columns.as_slice(), query.columns());
        assert_eq!(
            occurrences(rows.into_iter().map(cells)),
            *expected,
            "{text}"
        );
    }
}

fn check_delivery_limits(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    handle: &StandingQueryHandle,
    expected: &Bag,
) {
    let limit = |rows| GqlQueryPolicy::new(1_000, rows, 50_000_000, 50_000_000);
    let support = u64::try_from(expected.len()).unwrap();
    let occurrences = u64::try_from(expected.values().sum::<i128>()).unwrap();
    assert!(support > 0 && support < occurrences);
    let (_, rows) = db.standing_native_bag(cx, handle, limit(support)).unwrap();
    assert_eq!(native_bag(&rows), *expected);
    assert!(matches!(
        db.standing_native_bag(cx, handle, limit(support - 1)),
        Err(StandingQueryError::Delivery(
            StandingQueryFailure::ResultBudget
        ))
    ));
    assert!(matches!(
        db.standing_native_query(cx, handle, limit(occurrences - 1)),
        Err(StandingQueryError::Delivery(
            StandingQueryFailure::ResultBudget
        ))
    ));
}

fn check_delta(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    handle: &StandingQueryHandle,
    from: CommitSeq,
    before: &Bag,
    after: &Bag,
) {
    let (at, delta) = db
        .standing_native_delta(cx, handle, from, policy())
        .unwrap();
    assert_eq!(at, db.frontier().unwrap());
    assert_eq!(native_bag(&delta), difference(before, after));
    let (_, repeated) = db
        .standing_native_delta(cx, handle, from, policy())
        .unwrap();
    assert_eq!(repeated, delta);
}

fn seed() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    for (id, labels) in [
        (1, vec![L]),
        (2, vec![M]),
        (3, vec![L]),
        (4, vec![M]),
        (5, vec![L]),
        (6, vec![]),
    ] {
        batch.create_vertex(VId(id), labels, vec![(P, CanonicalScalar::Int(10))]);
    }
    batch.add_edge(EId(11), VId(1), VId(2), vec![]);
    batch.add_edge(EId(12), VId(1), VId(2), vec![]);
    batch.add_edge(EId(13), VId(3), VId(4), vec![]);
    batch.add_edge(EId(14), VId(5), VId(6), vec![]);
    batch
}

// Counts are fixture literals. This helper only expresses the four public
// schemas and the separately specified two outer occurrences per owner.
fn expected_bags(counts: &[(u128, i64)]) -> [Bag; 4] {
    let mut expected = [Bag::new(), Bag::new(), Bag::new(), Bag::new()];
    for &(owner, count) in counts {
        let values = vec![integer(7), vertex(Some(owner))];
        let mut counted = values.clone();
        counted.push(integer(count));
        expected[0].insert(row(counted.clone()), 2);
        expected[1].insert(row(counted), 2);
        if count >= 2 {
            expected[2].insert(row(values.clone()), 2);
        }
        let mut projected = values;
        projected.push(integer(count + 1));
        expected[3].insert(row(projected), 2);
    }
    expected
}

fn assert_count_circuit(queries: &[StandingQuery]) {
    assert!(
        queries
            .iter()
            .any(|query| matches!(query, StandingQuery::Group(_)))
    );
    for expected in [RowJoinKind::Semi, RowJoinKind::Left] {
        assert!(queries.iter().any(|query| {
            matches!(query, StandingQuery::Join(join) if join.spec().kind() == expected)
        }));
    }
}

#[test]
fn native_count_and_call_summarize_parallel_witnesses_once_per_outer_occurrence() {
    let ((), report) = run_async_under_lab(0x636e_7001, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xe1)).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let parameters = GqlParameters::new();
        let mut queries = Vec::new();
        for text in [COUNT_BODY, CALL_COUNT, COUNT_FILTER, COUNT_EXPRESSION] {
            let prepared = PreparedNativeRead::prepare(text, &parameters, symbols).unwrap();
            let (nodes, width) = prepared.subscription_footprint(&cx, &parameters).unwrap();
            let first = db.standing_queries.len();
            let handle = db
                .register_standing_native(&cx, text, &parameters, symbols, policy())
                .unwrap();
            assert!(width > 0);
            assert!(u64::try_from(db.standing_queries.len() - first).unwrap() <= nodes);
            assert_count_circuit(&db.standing_queries[first..]);
            queries.push((text, bind(text), handle));
        }
        let mut expected = expected_bags(&[(1, 2), (3, 1), (5, 0)]);
        for ((text, query, handle), rows) in queries.iter().zip(&expected) {
            check(&db, &cx, text, query, handle, rows, policy());
        }
        for phase in 0..16 {
            let from = db.frontier().unwrap();
            let mut change = WriteBatch::new(if phase == 1 { RelationId(2) } else { R });
            match phase {
                0 => {
                    change.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(99)));
                }
                1 => {
                    change.add_edge(EId(90), VId(5), VId(2), vec![]);
                }
                2 => {
                    change.add_edge(EId(15), VId(1), VId(4), vec![]);
                }
                3 => {
                    change.delete_edge(EId(11));
                }
                4 => {
                    change.delete_edge(EId(12));
                }
                5 => {
                    change.set_vertex_label(VId(4), M, false);
                }
                6 => {
                    change.set_vertex_label(VId(6), M, true);
                }
                7 => {
                    change.set_vertex_label(VId(4), M, true);
                }
                8 => {
                    change.delete_vertex(VId(4));
                }
                9 => {
                    change.delete_edge(EId(14));
                }
                10 => {
                    change.add_edge(EId(16), VId(3), VId(2), vec![]);
                }
                11 => {
                    change.delete_vertex(VId(3));
                }
                12 => {
                    change.create_vertex(VId(9), vec![L], vec![]);
                    change.create_vertex(VId(10), vec![M], vec![]);
                    change.add_edge(EId(17), VId(9), VId(10), vec![]);
                }
                13 => {
                    change.set_vertex_label(VId(5), L, false);
                }
                14 => {
                    change.set_vertex_label(VId(5), L, true);
                }
                15 => {
                    change.delete_vertex(VId(2));
                }
                _ => unreachable!(),
            }
            db.write(&commit, change).await.unwrap();
            let next = match phase {
                0 | 1 | 3 => expected_bags(&[(1, 2), (3, 1), (5, 0)]),
                2 => expected_bags(&[(1, 3), (3, 1), (5, 0)]),
                4 => expected_bags(&[(1, 1), (3, 1), (5, 0)]),
                5 | 9 => expected_bags(&[(1, 0), (3, 0), (5, 0)]),
                6 | 8 => expected_bags(&[(1, 0), (3, 0), (5, 1)]),
                7 => expected_bags(&[(1, 1), (3, 1), (5, 1)]),
                10 => expected_bags(&[(1, 0), (3, 1), (5, 0)]),
                11 => expected_bags(&[(1, 0), (5, 0)]),
                12 | 14 | 15 => expected_bags(&[(1, 0), (5, 0), (9, 1)]),
                13 => expected_bags(&[(1, 0), (9, 1)]),
                _ => unreachable!(),
            };
            for (((text, query, handle), before), after) in queries.iter().zip(&expected).zip(&next)
            {
                check(&db, &cx, text, query, handle, after, policy());
                check_delta(&db, &cx, handle, from, before, after);
            }
            expected = next;
        }
        for ((text, query, handle), rows) in queries.iter().zip(&expected) {
            db.rebuild_standing_query(&cx, handle, policy()).unwrap();
            check(&db, &cx, text, query, handle, rows, policy());
        }
        super::tests::check_first_tick_after_rebuild(
            &mut db,
            &cx,
            &commit,
            queries.iter().map(|entry| &entry.2),
        )
        .await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn expected_value_counts(counts: &[(u128, i64, i64)]) -> [Bag; 4] {
    let mut expected = [Bag::new(), Bag::new(), Bag::new(), Bag::new()];
    for &(owner, occurrences, neighbors) in counts {
        for (output, count) in
            expected
                .iter_mut()
                .zip([occurrences, neighbors, occurrences, neighbors])
        {
            output.insert(
                row(vec![integer(7), vertex(Some(owner)), integer(count)]),
                2,
            );
        }
    }
    expected
}

#[test]
fn native_value_counts_distinguish_parallel_edges_and_last_neighbor_support() {
    let ((), report) = run_async_under_lab(0x636e_7007, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xe7)).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let parameters = GqlParameters::new();
        let mut queries = Vec::new();
        for text in [
            CALL_VALUE_COUNT,
            CALL_DISTINCT_NEIGHBORS,
            CALL_DISTINCT_EDGES,
            COUNT_DISTINCT_BODY,
        ] {
            let prepared = PreparedNativeRead::prepare(text, &parameters, symbols).unwrap();
            let (nodes, _) = prepared.subscription_footprint(&cx, &parameters).unwrap();
            let first = db.standing_queries.len();
            let handle = db
                .register_standing_native(&cx, text, &parameters, symbols, policy())
                .unwrap();
            assert!(u64::try_from(db.standing_queries.len() - first).unwrap() <= nodes);
            if text == COUNT_DISTINCT_BODY {
                assert!(
                    db.standing_queries[first..]
                        .iter()
                        .any(|query| matches!(query, StandingQuery::Group(_)))
                );
            } else {
                assert_count_circuit(&db.standing_queries[first..]);
            }
            queries.push((text, bind(text), handle));
        }
        let mut expected = expected_value_counts(&[(1, 2, 1), (3, 1, 1), (5, 0, 0)]);
        for ((text, query, handle), rows) in queries.iter().zip(&expected) {
            check(&db, &cx, text, query, handle, rows, policy());
        }
        for phase in 0..12 {
            let from = db.frontier().unwrap();
            let mut change = WriteBatch::new(R);
            match phase {
                0 => {
                    change.delete_edge(EId(11));
                }
                1 => {
                    change.add_edge(EId(15), VId(1), VId(4), vec![]);
                }
                2 => {
                    change.delete_edge(EId(12));
                }
                3 => {
                    change.add_edge(EId(16), VId(1), VId(4), vec![]);
                }
                4 => {
                    change.set_vertex_label(VId(4), M, false);
                }
                5 => {
                    change.set_vertex_label(VId(4), M, true);
                }
                6 => {
                    change.delete_vertex(VId(4));
                }
                7 => {
                    change.add_edge(EId(17), VId(1), VId(2), vec![]);
                }
                8 => {
                    change.set_vertex_label(VId(1), L, false);
                }
                9 => {
                    change.set_vertex_label(VId(1), L, true);
                    change.add_edge(EId(18), VId(1), VId(2), vec![]);
                }
                10 => {
                    change.delete_vertex(VId(2));
                }
                11 => {
                    change.set_vertex_label(VId(6), M, true);
                    change.add_edge(EId(19), VId(5), VId(6), vec![]);
                }
                _ => unreachable!(),
            }
            db.write(&commit, change).await.unwrap();
            let next = match phase {
                0 | 2 => expected_value_counts(&[(1, 1, 1), (3, 1, 1), (5, 0, 0)]),
                1 => expected_value_counts(&[(1, 2, 2), (3, 1, 1), (5, 0, 0)]),
                3 | 5 => expected_value_counts(&[(1, 2, 1), (3, 1, 1), (5, 0, 0)]),
                4 | 6 | 10 => expected_value_counts(&[(1, 0, 0), (3, 0, 0), (5, 0, 0)]),
                7 => expected_value_counts(&[(1, 1, 1), (3, 0, 0), (5, 0, 0)]),
                8 => expected_value_counts(&[(3, 0, 0), (5, 0, 0)]),
                9 => expected_value_counts(&[(1, 2, 1), (3, 0, 0), (5, 0, 0)]),
                11 => expected_value_counts(&[(1, 0, 0), (3, 0, 0), (5, 2, 1)]),
                _ => unreachable!(),
            };
            for (((text, query, handle), before), after) in queries.iter().zip(&expected).zip(&next)
            {
                check(&db, &cx, text, query, handle, after, policy());
                check_delta(&db, &cx, handle, from, before, after);
            }
            expected = next;
        }
        for ((text, query, handle), rows) in queries.iter().zip(&expected) {
            db.rebuild_standing_query(&cx, handle, policy()).unwrap();
            check(&db, &cx, text, query, handle, rows, policy());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn nullable_row(owner: u128, imported: Option<u128>, count: i64) -> GraphValueRow {
    row(vec![vertex(Some(owner)), vertex(imported), integer(count)])
}

#[test]
fn counts_of_missing_and_null_imports_are_zero_without_scaling_shared_key_summaries() {
    let ((), report) = run_async_under_lab(0x636e_7002, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xe2)).await.unwrap();
        let mut initial = WriteBatch::new(R);
        for (id, label) in [(1, L), (2, M), (3, L), (4, M)] {
            initial.create_vertex(VId(id), vec![label], vec![]);
        }
        initial.add_edge(EId(11), VId(1), VId(2), vec![]);
        initial.add_edge(EId(12), VId(1), VId(2), vec![]);
        initial.add_edge(EId(13), VId(2), VId(4), vec![]);
        initial.add_edge(EId(14), VId(2), VId(4), vec![]);
        db.write(&commit, initial).await.unwrap();
        let query = bind(NULL_IMPORT);
        let first = db.standing_queries.len();
        let handle = db
            .register_standing_native(&cx, NULL_IMPORT, &GqlParameters::new(), symbols, policy())
            .unwrap();
        assert_count_circuit(&db.standing_queries[first..]);
        let mut expected = bag([
            (nullable_row(1, Some(2), 2), 2),
            (nullable_row(3, None, 0), 1),
        ]);
        check(&db, &cx, NULL_IMPORT, &query, &handle, &expected, policy());
        for phase in 0..10 {
            let from = db.frontier().unwrap();
            let mut change = WriteBatch::new(R);
            match phase {
                0 => {
                    change.create_vertex(VId(5), vec![M], vec![]);
                    change.add_edge(EId(15), VId(3), VId(5), vec![]);
                }
                1 => {
                    change.add_edge(EId(16), VId(5), VId(4), vec![]);
                }
                2 => {
                    change.add_edge(EId(17), VId(2), VId(4), vec![]);
                }
                3 => {
                    change.delete_edge(EId(13));
                }
                4 => {
                    change.delete_vertex(VId(4));
                }
                5 => {
                    change.add_edge(EId(18), VId(5), VId(2), vec![]);
                }
                6 => {
                    change.delete_vertex(VId(5));
                }
                7 => {
                    change.create_vertex(VId(6), vec![M], vec![]);
                    change.add_edge(EId(19), VId(3), VId(2), vec![]);
                    change.add_edge(EId(20), VId(2), VId(6), vec![]);
                }
                8 => {
                    change.delete_vertex(VId(2));
                }
                9 => {
                    change.delete_vertex(VId(1));
                }
                _ => unreachable!(),
            }
            db.write(&commit, change).await.unwrap();
            let next = match phase {
                0 => bag([
                    (nullable_row(1, Some(2), 2), 2),
                    (nullable_row(3, Some(5), 0), 1),
                ]),
                1 => bag([
                    (nullable_row(1, Some(2), 2), 2),
                    (nullable_row(3, Some(5), 1), 1),
                ]),
                2 => bag([
                    (nullable_row(1, Some(2), 3), 2),
                    (nullable_row(3, Some(5), 1), 1),
                ]),
                3 => bag([
                    (nullable_row(1, Some(2), 2), 2),
                    (nullable_row(3, Some(5), 1), 1),
                ]),
                4 => bag([
                    (nullable_row(1, Some(2), 0), 2),
                    (nullable_row(3, Some(5), 0), 1),
                ]),
                5 => bag([
                    (nullable_row(1, Some(2), 0), 2),
                    (nullable_row(3, Some(5), 1), 1),
                ]),
                6 => bag([
                    (nullable_row(1, Some(2), 0), 2),
                    (nullable_row(3, None, 0), 1),
                ]),
                7 => bag([
                    (nullable_row(1, Some(2), 1), 2),
                    (nullable_row(3, Some(2), 1), 1),
                ]),
                8 => bag([(nullable_row(1, None, 0), 1), (nullable_row(3, None, 0), 1)]),
                9 => bag([(nullable_row(3, None, 0), 1)]),
                _ => unreachable!(),
            };
            check(&db, &cx, NULL_IMPORT, &query, &handle, &next, policy());
            check_delta(&db, &cx, &handle, from, &expected, &next);
            expected = next;
        }
        db.rebuild_standing_query(&cx, &handle, policy()).unwrap();
        check(&db, &cx, NULL_IMPORT, &query, &handle, &expected, policy());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn private_count_witnesses_do_not_spend_the_public_result_row_quota() {
    let ((), report) = run_async_under_lab(0x636e_7003, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xe3)).await.unwrap();
        let mut initial = WriteBatch::new(R);
        initial.create_vertex(VId(1), vec![L], vec![]);
        initial.create_vertex(VId(2), vec![M], vec![]);
        for edge in 10..74 {
            initial.add_edge(EId(edge), VId(1), VId(2), vec![]);
        }
        db.write(&commit, initial).await.unwrap();
        let two = GqlQueryPolicy::new(1_000, 2, 50_000_000, 50_000_000);
        let mut queries = Vec::new();
        for text in [COUNT_BODY, CALL_COUNT] {
            let query = bind(text);
            let first = db.standing_queries.len();
            let handle = db
                .register_standing_native(&cx, text, &GqlParameters::new(), symbols, two)
                .unwrap();
            assert_count_circuit(&db.standing_queries[first..]);
            queries.push((text, query, handle));
        }
        let mut expected = expected_bags(&[(1, 64)])[0].clone();
        for (text, query, handle) in &queries {
            check(&db, &cx, text, query, handle, &expected, two);
        }
        for phase in 0..4 {
            let from = db.frontier().unwrap();
            let mut change = WriteBatch::new(R);
            match phase {
                0 => {
                    change.add_edge(EId(1000), VId(1), VId(2), vec![]);
                }
                1 => {
                    change.set_vertex_label(VId(2), M, false);
                }
                2 => {
                    change.set_vertex_label(VId(2), M, true);
                }
                3 => {
                    change.delete_vertex(VId(2));
                }
                _ => unreachable!(),
            }
            db.write(&commit, change).await.unwrap();
            let count = match phase {
                0 | 2 => 65,
                1 | 3 => 0,
                _ => unreachable!(),
            };
            let next = expected_bags(&[(1, count)])[0].clone();
            for (text, query, handle) in &queries {
                check(&db, &cx, text, query, handle, &next, two);
                check_delta(&db, &cx, handle, from, &expected, &next);
                db.rebuild_standing_query(&cx, handle, two).unwrap();
                check(&db, &cx, text, query, handle, &next, two);
                for refused in [
                    GqlQueryPolicy::new(1_000, 1, 50_000_000, 50_000_000),
                    GqlQueryPolicy::new(0, 2, 50_000_000, 50_000_000),
                ] {
                    assert!(db.rebuild_standing_query(&cx, handle, refused).is_err());
                    check(&db, &cx, text, query, handle, &next, two);
                }
            }
            expected = next;
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn an_empty_outer_bag_keeps_a_private_global_zero_group_under_zero_public_rows() {
    let ((), report) = run_async_under_lab(0x636e_7004, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xe4)).await.unwrap();
        let mut initial = WriteBatch::new(R);
        for id in 100..164 {
            initial.create_vertex(VId(id), vec![M], vec![]);
        }
        db.write(&commit, initial).await.unwrap();
        let query = bind(UNCORRELATED);
        let zero = GqlQueryPolicy::new(1_000, 0, 50_000_000, 50_000_000);
        let first = db.standing_queries.len();
        let handle = db
            .register_standing_native(&cx, UNCORRELATED, &GqlParameters::new(), symbols, zero)
            .unwrap();
        assert_count_circuit(&db.standing_queries[first..]);
        check(&db, &cx, UNCORRELATED, &query, &handle, &Bag::new(), zero);
        db.rebuild_standing_query(&cx, &handle, zero).unwrap();
        check(&db, &cx, UNCORRELATED, &query, &handle, &Bag::new(), zero);

        let mut change = WriteBatch::new(R);
        change.create_vertex(VId(1), vec![L], vec![]);
        db.write(&commit, change).await.unwrap();
        // The private exemption does not relax the public row limit.
        assert!(db.standing_native_query(&cx, &handle, policy()).is_err());
        let one = GqlQueryPolicy::new(1_000, 1, 50_000_000, 50_000_000);
        db.rebuild_standing_query(&cx, &handle, one).unwrap();
        let expected = bag([(row(vec![vertex(Some(1)), integer(64)]), 1)]);
        check(&db, &cx, UNCORRELATED, &query, &handle, &expected, one);
        assert!(db.rebuild_standing_query(&cx, &handle, zero).is_err());
        check(&db, &cx, UNCORRELATED, &query, &handle, &expected, one);

        let from = db.frontier().unwrap();
        let mut change = WriteBatch::new(R);
        change.delete_vertex(VId(100));
        db.write(&commit, change).await.unwrap();
        let next = bag([(row(vec![vertex(Some(1)), integer(63)]), 1)]);
        check(&db, &cx, UNCORRELATED, &query, &handle, &next, one);
        check_delta(&db, &cx, &handle, from, &expected, &next);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

type Saved = (
    usize,
    GqlQueryPolicy,
    CommitSeq,
    Option<StandingQueryFailure>,
    ZSet<GraphValueRow>,
);

fn saved(queries: &[StandingQuery]) -> Vec<Saved> {
    queries
        .iter()
        .map(|query| {
            let rows = sets::rows(query).unwrap();
            let (policy, at, failure) = query.status();
            (
                std::ptr::from_ref(rows) as usize,
                policy,
                at,
                failure,
                rows.checked_clone(LIMBS, &mut |_| Ok::<_, ()>(())).unwrap(),
            )
        })
        .collect()
}

#[test]
fn each_aggregate_summary_registration_and_rebuild_checkpoint_preserves_existing_circuits() {
    let ((), report) = run_async_under_lab(0x636e_7005, |root| async move {
        for (text, expected) in [
            (
                CALL_COUNT.to_owned(),
                expected_bags(&[(1, 0), (3, 1), (5, 0)])[1].clone(),
            ),
            (
                scalar_call_text("avg(id(b))"),
                scalar_call_bag([vertex(None), floating(4.0), vertex(None)]),
            ),
            (
                continued_summary_text(
                    "WITH count(*) AS n,sum(id(b)) AS s RETURN n+1 AS degree,s+1 AS total",
                ),
                continued_summary_bag(&[(1, 1, None), (3, 2, Some(5)), (5, 1, None)]),
            ),
            (
                continued_summary_text(
                    "WITH count(*) AS n,sum(id(b)) AS s WHERE n>0 \
                     RETURN n+1 AS degree,s+1 AS total",
                ),
                continued_summary_bag(&[(3, 2, Some(5))]),
            ),
            (
                concat!(
                    "MATCH (a:L) WITH a OPTIONAL CALL (a) { ",
                    "MATCH (a)-[:R]->(b:M) RETURN b AS neighbor,count(*) AS degree ",
                    "} RETURN a,neighbor,degree"
                )
                .to_owned(),
                bag([
                    (row(vec![vertex(Some(1)), vertex(None), vertex(None)]), 1),
                    (row(vec![vertex(Some(3)), vertex(Some(4)), integer(1)]), 1),
                    (row(vec![vertex(Some(5)), vertex(None), vertex(None)]), 1),
                ]),
            ),
            (
                argument_summary_text(1),
                argument_summary_bag(1, &[(1, Some(10), 0), (3, Some(10), 1), (5, Some(10), 0)]),
            ),
            (
                expanded_group_text(true),
                expanded_group_bag(
                    &[1, 3, 5],
                    &[
                        (1, 0, 5, 30),
                        (1, 1, 5, 25),
                        (3, 0, 5, 30),
                        (3, 1, 5, 25),
                        (5, 0, 5, 30),
                        (5, 1, 5, 25),
                    ],
                    true,
                ),
            ),
        ] {
            let c = PurposeContexts::narrow_runtime_root(&root);
            let cx = c.query();
            let commit = c.commit();
            let mut db = Database::open_memory(&commit, keys(0xe5)).await.unwrap();
            db.write(&commit, seed()).await.unwrap();
            let sibling = db
                .register_standing_native(
                    &cx,
                    "MATCH (n) RETURN n",
                    &GqlParameters::new(),
                    symbols,
                    policy(),
                )
                .unwrap();
            let query = bind(&text);
            let before = saved(&db.standing_queries);
            let mut calls = 0;
            {
                let mut staged = Staging::new(&mut db);
                staged
                    .compile_root(&cx, &query, policy(), &mut || {
                        calls += 1;
                        Ok(())
                    })
                    .unwrap();
            }
            assert!(calls > 0);
            assert_eq!(saved(&db.standing_queries), before);
            for stop in 1..=calls {
                let mut seen = 0;
                {
                    let mut staged = Staging::new(&mut db);
                    assert!(
                        staged
                            .compile_root(&cx, &query, policy(), &mut || {
                                seen += 1;
                                if seen == stop {
                                    Err(StandingQueryError::Maintenance(
                                        StandingQueryFailure::Interrupted,
                                    ))
                                } else {
                                    Ok(())
                                }
                            })
                            .is_err()
                    );
                }
                assert_eq!(seen, stop);
                assert_eq!(saved(&db.standing_queries), before);
            }
            let handle = db
                .register_standing_relation(&cx, &query, policy())
                .unwrap();
            let Layout::Circuit { first, .. } = handle.native.as_deref().unwrap() else {
                panic!("scalar Apply owns one complete circuit");
            };
            let first = *first;
            let mut calls = 0;
            let private_inputs = handle.native.as_deref().unwrap().private_inputs().unwrap();
            rebuild_checked(
                &mut db,
                &cx,
                first,
                handle.index,
                private_inputs,
                policy(),
                &mut || {
                    calls += 1;
                    Ok(())
                },
            )
            .unwrap();
            let accepted = saved(&db.standing_queries);
            for stop in 1..=calls {
                let mut seen = 0;
                assert!(
                    rebuild_checked(
                        &mut db,
                        &cx,
                        first,
                        handle.index,
                        private_inputs,
                        policy(),
                        &mut || {
                            seen += 1;
                            if seen == stop {
                                Err(StandingQueryError::Maintenance(
                                    StandingQueryFailure::Interrupted,
                                ))
                            } else {
                                Ok(())
                            }
                        }
                    )
                    .is_err()
                );
                assert_eq!(seen, stop);
                assert_eq!(saved(&db.standing_queries), accepted);
            }
            let mut change = WriteBatch::new(R);
            change.delete_vertex(VId(2));
            db.write(&commit, change).await.unwrap();
            check(&db, &cx, &text, &query, &handle, &expected, policy());
            db.standing_native_query(&cx, &sibling, policy()).unwrap();
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unsupported_count_body_sources_refuse_even_under_empty_outer() {
    let ((), report) = run_async_under_lab(0x636e_7006, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xe6)).await.unwrap();
        let mut initial = WriteBatch::new(R);
        initial.create_vertex(VId(2), vec![M], vec![(P, CanonicalScalar::Int(0))]);
        db.write(&commit, initial).await.unwrap();
        let sibling = db
            .register_standing_native(
                &cx,
                "MATCH (b:M) RETURN b",
                &GqlParameters::new(),
                symbols,
                policy(),
            )
            .unwrap();
        let before = saved(&db.standing_queries);
        let parameters = GqlParameters::new();
        for text in [
            concat!(
                "MATCH (a:L) WITH a RETURN COUNT { ",
                "MATCH (a)-[:R]->(b) RETURN b LIMIT 1 } AS degree"
            ),
            concat!(
                "MATCH (a:L) WITH a RETURN COUNT { ",
                "MATCH (b:M) RETURN b.p } AS degree"
            ),
            concat!(
                "MATCH (a:L) WITH a,a.p AS threshold RETURN COUNT { ",
                "MATCH (a)-[:R]->(b:M) WHERE b.p > threshold RETURN b } AS degree"
            ),
            concat!(
                "MATCH (a:L) WITH a CALL (a) { ",
                "MATCH (a)-[:R]->(b) RETURN count(b.p) AS degree } RETURN a,degree"
            ),
        ] {
            assert!(
                db.execute_graph_set_governed(&cx, &bind(text), policy())
                    .unwrap()
                    .value
                    .is_empty()
            );
            for zero in [false, true] {
                let statement = if zero {
                    format!("{text} LIMIT 0")
                } else {
                    text.to_owned()
                };
                let prepared =
                    PreparedNativeRead::prepare(&statement, &parameters, symbols).unwrap();
                assert!(matches!(
                    prepared.subscription_footprint(&cx, &parameters),
                    Err(StandingQueryError::Unsupported)
                ));
                assert!(matches!(
                    prepared.register_standing(&mut db, &cx, &parameters, policy()),
                    Err(StandingQueryError::Unsupported)
                ));
                assert_eq!(saved(&db.standing_queries), before);
            }
        }
        db.standing_native_query(&cx, &sibling, policy()).unwrap();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn filtered_counts_text(aggregate: &str) -> String {
    format!(
        "MATCH (a:L) UNWIND [NULL,1,2,2] AS floor \
         WITH a,a.p AS score,toInteger(floor) AS floor \
         CALL (a,score,floor) {{ \
         MATCH (a)-[:R]->(b:M {{selected:1}}) \
         WHERE score >= floor AND b <> a RETURN {aggregate} AS degree }} \
         RETURN a,floor,degree"
    )
}

type FilteredCounts = (u128, [(i64, i64); 2]);

// Literal expected (occurrence count, distinct neighbor count) for thresholds
// one and two. NULL never passes the predicate; threshold two occurs twice.
fn filtered_expected(entries: &[FilteredCounts]) -> [Bag; 3] {
    let mut bags = [Bag::new(), Bag::new(), Bag::new()];
    for &(owner, counts) in entries {
        for (floor, (count, distinct), multiplicity) in [
            (None, (0, 0), 1),
            (Some(1), counts[0], 1),
            (Some(2), counts[1], 2),
        ] {
            for (result, value) in bags.iter_mut().zip([count, count, distinct]) {
                result.insert(
                    row(vec![
                        vertex(Some(owner)),
                        floor.map_or(GraphValue::Scalar(CanonicalScalar::Null), integer),
                        integer(value),
                    ]),
                    multiplicity,
                );
            }
        }
    }
    bags
}

#[test]
fn filtered_counts_maintain_nullable_invocations_and_property_selected_witnesses() {
    let ((), report) = run_async_under_lab(0x636e_7010, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xf0)).await.unwrap();
        let mut initial = WriteBatch::new(R);
        for (id, labels, props) in [
            (
                1,
                vec![L, M],
                vec![
                    (P, CanonicalScalar::Int(2)),
                    (SELECTED, CanonicalScalar::Int(1)),
                ],
            ),
            (2, vec![M], vec![(SELECTED, CanonicalScalar::Int(1))]),
            (3, vec![L], vec![(P, CanonicalScalar::Int(1))]),
            (4, vec![M], vec![(SELECTED, CanonicalScalar::Int(1))]),
            (5, vec![L], vec![]),
            (6, vec![M], vec![(SELECTED, CanonicalScalar::Int(0))]),
        ] {
            initial.create_vertex(VId(id), labels, props);
        }
        for (id, src, dst) in [
            (11, 1, 2),
            (12, 1, 2),
            (13, 1, 4),
            (14, 1, 1),
            (15, 3, 4),
            (16, 5, 2),
            (17, 1, 6),
        ] {
            initial.add_edge(EId(id), VId(src), VId(dst), vec![]);
        }
        db.write(&commit, initial).await.unwrap();
        let parameters = GqlParameters::new();
        let mut queries = Vec::new();
        for aggregate in ["count(*)", "count(b)", "count(DISTINCT b)"] {
            let text = filtered_counts_text(aggregate);
            let prepared = PreparedNativeRead::prepare(&text, &parameters, symbols).unwrap();
            let (nodes, _) = prepared.subscription_footprint(&cx, &parameters).unwrap();
            let before = db.standing_queries.len();
            let handle = prepared
                .register_standing(&mut db, &cx, &parameters, policy())
                .unwrap();
            assert!(nodes >= (db.standing_queries.len() - before) as u64);
            assert!(
                db.standing_queries[before..]
                    .iter()
                    .any(|query| matches!(query, StandingQuery::Group(_)))
            );
            queries.push((text.clone(), bind(&text), handle));
        }
        let mut expected = filtered_expected(&[
            (1, [(3, 2), (3, 2)]),
            (3, [(1, 1), (0, 0)]),
            (5, [(0, 0), (0, 0)]),
        ]);
        for ((text, query, handle), rows) in queries.iter().zip(&expected) {
            check(&db, &cx, text, query, handle, rows, policy());
        }
        for phase in 0..11 {
            let from = db.frontier().unwrap();
            let mut change = WriteBatch::new(R);
            match phase {
                0 => change.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(3))),
                1 => change.set_vertex_property(VId(2), SELECTED, None),
                2 => change.set_vertex_property(VId(5), P, Some(CanonicalScalar::Int(2))),
                3 => change.set_vertex_property(VId(2), SELECTED, Some(CanonicalScalar::Int(1))),
                4 => change.delete_edge(EId(11)),
                5 => change.delete_edge(EId(12)),
                6 => change.delete_vertex(VId(4)),
                7 => {
                    change.set_vertex_property(VId(1), P, None);
                    change.add_edge(EId(18), VId(1), VId(2), vec![])
                }
                8 => change.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(2))),
                9 => change.delete_vertex(VId(5)),
                10 => change.delete_vertex(VId(2)),
                _ => unreachable!(),
            };
            db.write(&commit, change).await.unwrap();
            let next = match phase {
                0 => filtered_expected(&[
                    (1, [(3, 2), (3, 2)]),
                    (3, [(1, 1), (1, 1)]),
                    (5, [(0, 0), (0, 0)]),
                ]),
                1 | 2 => filtered_expected(&[
                    (1, [(1, 1), (1, 1)]),
                    (3, [(1, 1), (1, 1)]),
                    (5, [(0, 0), (0, 0)]),
                ]),
                3 => filtered_expected(&[
                    (1, [(3, 2), (3, 2)]),
                    (3, [(1, 1), (1, 1)]),
                    (5, [(1, 1), (1, 1)]),
                ]),
                4 => filtered_expected(&[
                    (1, [(2, 2), (2, 2)]),
                    (3, [(1, 1), (1, 1)]),
                    (5, [(1, 1), (1, 1)]),
                ]),
                5 => filtered_expected(&[
                    (1, [(1, 1), (1, 1)]),
                    (3, [(1, 1), (1, 1)]),
                    (5, [(1, 1), (1, 1)]),
                ]),
                6 | 7 => filtered_expected(&[
                    (1, [(0, 0), (0, 0)]),
                    (3, [(0, 0), (0, 0)]),
                    (5, [(1, 1), (1, 1)]),
                ]),
                8 => filtered_expected(&[
                    (1, [(1, 1), (1, 1)]),
                    (3, [(0, 0), (0, 0)]),
                    (5, [(1, 1), (1, 1)]),
                ]),
                9 => filtered_expected(&[(1, [(1, 1), (1, 1)]), (3, [(0, 0), (0, 0)])]),
                10 => filtered_expected(&[(1, [(0, 0), (0, 0)]), (3, [(0, 0), (0, 0)])]),
                _ => unreachable!(),
            };
            for (((text, query, handle), before), after) in queries.iter().zip(&expected).zip(&next)
            {
                check(&db, &cx, text, query, handle, after, policy());
                check_delta(&db, &cx, handle, from, before, after);
            }
            expected = next;
        }
        for ((text, query, handle), rows) in queries.iter().zip(&expected) {
            db.rebuild_standing_query(&cx, handle, policy()).unwrap();
            check(&db, &cx, text, query, handle, rows, policy());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn filtered_count_private_witnesses_preserve_public_quota_and_registration_atomicity() {
    let ((), report) = run_async_under_lab(0x636e_7011, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xf1)).await.unwrap();
        let mut initial = WriteBatch::new(R);
        initial.create_vertex(VId(1), vec![L], vec![(P, CanonicalScalar::Int(2))]);
        initial.create_vertex(VId(2), vec![M], vec![(SELECTED, CanonicalScalar::Int(1))]);
        for edge in 10..74 {
            initial.add_edge(EId(edge), VId(1), VId(2), vec![]);
        }
        db.write(&commit, initial).await.unwrap();
        let four = GqlQueryPolicy::new(1_000, 4, 50_000_000, 50_000_000);
        let text = filtered_counts_text("count(*)");
        let query = bind(&text);
        let handle = db
            .register_standing_native(&cx, &text, &GqlParameters::new(), symbols, four)
            .unwrap();
        let expected = filtered_expected(&[(1, [(64, 1), (64, 1)])]);
        check(&db, &cx, &text, &query, &handle, &expected[0], four);

        let before = saved(&db.standing_queries);
        let refused = GqlQueryPolicy::new(1_000, 4, 0, 0);
        assert!(
            db.register_standing_native(&cx, &text, &GqlParameters::new(), symbols, refused)
                .is_err()
        );
        assert_eq!(saved(&db.standing_queries), before);
        check(&db, &cx, &text, &query, &handle, &expected[0], four);

        let from = db.frontier().unwrap();
        let mut change = WriteBatch::new(R);
        change.set_vertex_property(VId(2), SELECTED, None);
        db.write(&commit, change).await.unwrap();
        let empty = filtered_expected(&[(1, [(0, 0), (0, 0)])]);
        check(&db, &cx, &text, &query, &handle, &empty[0], four);
        check_delta(&db, &cx, &handle, from, &expected[0], &empty[0]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn filtered_count_keeps_uninvoked_and_invoked_expression_errors_distinct() {
    let ((), report) = run_async_under_lab(0x636e_7012, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xf2)).await.unwrap();
        let mut initial = WriteBatch::new(R);
        initial.create_vertex(VId(2), vec![M], vec![]);
        db.write(&commit, initial).await.unwrap();
        let parameters = GqlParameters::new();
        let theta = concat!(
            "MATCH (a:L) WITH a RETURN COUNT { ",
            "MATCH (b:M) WHERE b <> a RETURN b } AS degree"
        );
        let broken = concat!(
            "MATCH (a:L) WITH a RETURN COUNT { ",
            "MATCH (a)-[:R]->(b) WITH b,12 / 0 AS bad RETURN b } AS degree"
        );
        let mut dormant = Vec::new();
        for text in [theta, broken] {
            let handle = db
                .register_standing_native(&cx, text, &parameters, symbols, policy())
                .unwrap();
            check(&db, &cx, text, &bind(text), &handle, &Bag::new(), policy());
            dormant.push(handle);
        }
        let mut invoked = WriteBatch::new(R);
        invoked.create_vertex(VId(1), vec![L], vec![]);
        invoked.add_edge(EId(1), VId(1), VId(2), vec![]);
        db.write(&commit, invoked).await.unwrap();
        assert!(
            db.standing_native_query(&cx, &dormant[1], policy())
                .is_err()
        );
        let selected = db
            .register_standing_native(&cx, theta, &parameters, symbols, policy())
            .unwrap();
        let one = bag([(row(vec![integer(1)]), 1)]);
        check(&db, &cx, theta, &bind(theta), &selected, &one, policy());
        let saved_registry = saved(&db.standing_queries);
        for text in [
            broken.to_owned(),
            format!("{broken} ORDER BY degree LIMIT 0"),
        ] {
            assert!(
                db.execute_graph_set_governed(&cx, &bind(&text), policy())
                    .is_err()
            );
            let prepared = PreparedNativeRead::prepare(&text, &parameters, symbols).unwrap();
            prepared.subscription_footprint(&cx, &parameters).unwrap();
            assert!(
                prepared
                    .register_standing(&mut db, &cx, &parameters, policy())
                    .is_err()
            );
            assert_eq!(saved(&db.standing_queries), saved_registry);
        }
        check(&db, &cx, theta, &bind(theta), &selected, &one, policy());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn post_summary_calls_maintain_computed_rows_filters_and_local_distinct() {
    let ((), report) = run_async_under_lab(0x7073_7010, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xe7)).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let parameters = GqlParameters::new();
        let prefix = "MATCH (a:L) UNWIND [7,7] AS token WITH token,a CALL (a) { ";
        let statements = [
            format!(
                "{prefix}MATCH (a)-[:R]->(b:M) WITH count(*) AS n WHERE n > 1 RETURN n+1 AS degree }} RETURN token,a,degree"
            ),
            format!(
                "{prefix}MATCH (a)-[:R]->(b:M) WITH count(*) AS n,sum(id(b)) AS s WHERE n = 0 OR s >= 4 RETURN coalesce(s,7) AS total,n+1 AS degree }} RETURN token,a,total,degree"
            ),
            format!(
                "{prefix}MATCH (a)-[:R]->(b:M) WITH count(*) AS n WITH DISTINCT NULL AS absent RETURN absent }} RETURN token,a,absent"
            ),
            format!(
                "{prefix}MATCH (a)-[:R]->(b:M) RETURN count(*)+1 AS degree }} RETURN token,a,degree"
            ),
        ];
        let mut queries = Vec::new();
        for text in statements {
            let prepared = PreparedNativeRead::prepare(&text, &parameters, symbols).unwrap();
            let (nodes, _) = prepared.subscription_footprint(&cx, &parameters).unwrap();
            let first = db.standing_queries.len();
            let handle = prepared
                .register_standing(&mut db, &cx, &parameters, policy())
                .unwrap();
            assert!(nodes >= (db.standing_queries.len() - first) as u64);
            assert!(
                db.standing_queries[first..]
                    .iter()
                    .any(|query| matches!(query, StandingQuery::Group(_)))
            );
            queries.push((text.clone(), bind(&text), handle));
        }
        // Counts and sums are literal fixture answers. Formatting the returned
        // expressions here does not invoke an engine or maintain a second graph.
        let expected = |groups: &[(u128, i64, Option<i64>)]| {
            let mut rows = [Bag::new(), Bag::new(), Bag::new(), Bag::new()];
            for &(owner, count, sum) in groups {
                let prefix = vec![integer(7), vertex(Some(owner))];
                let mut computed = prefix.clone();
                computed.push(integer(count + 1));
                if count > 1 {
                    rows[0].insert(row(computed.clone()), 2);
                }
                rows[3].insert(row(computed), 2);
                if count == 0 || sum.is_some_and(|value| value >= 4) {
                    let mut tuple = prefix.clone();
                    tuple.extend([integer(sum.unwrap_or(7)), integer(count + 1)]);
                    rows[1].insert(row(tuple), 2);
                }
                let mut absent = prefix;
                absent.push(vertex(None));
                rows[2].insert(row(absent), 2);
            }
            rows
        };
        let mut previous = expected(&[(1, 2, Some(4)), (3, 1, Some(4)), (5, 0, None)]);
        for ((text, query, handle), rows) in queries.iter().zip(&previous) {
            check(&db, &cx, text, query, handle, rows, policy());
        }
        for phase in 0..6 {
            let from = db.frontier().unwrap();
            let mut change = WriteBatch::new(R);
            match phase {
                0 => {
                    change.delete_edge(EId(11));
                }
                1 => {
                    change.add_edge(EId(15), VId(5), VId(2), vec![]);
                }
                2 => {
                    change.delete_edge(EId(12));
                    change.delete_edge(EId(13));
                }
                3 => {
                    change.add_edge(EId(16), VId(3), VId(2), vec![]);
                    change.add_edge(EId(17), VId(3), VId(2), vec![]);
                    change.add_edge(EId(18), VId(1), VId(4), vec![]);
                }
                4 => {
                    change.delete_vertex(VId(2));
                }
                5 => {
                    change.set_vertex_label(VId(3), L, false);
                }
                _ => unreachable!(),
            };
            db.write(&commit, change).await.unwrap();
            let next = match phase {
                0 => expected(&[(1, 1, Some(2)), (3, 1, Some(4)), (5, 0, None)]),
                1 => expected(&[(1, 1, Some(2)), (3, 1, Some(4)), (5, 1, Some(2))]),
                2 => expected(&[(1, 0, None), (3, 0, None), (5, 1, Some(2))]),
                3 => expected(&[(1, 1, Some(4)), (3, 2, Some(4)), (5, 1, Some(2))]),
                4 => expected(&[(1, 1, Some(4)), (3, 0, None), (5, 0, None)]),
                5 => expected(&[(1, 1, Some(4)), (5, 0, None)]),
                _ => unreachable!(),
            };
            for (((text, query, handle), old), rows) in queries.iter().zip(&previous).zip(&next) {
                check(&db, &cx, text, query, handle, rows, policy());
                check_delta(&db, &cx, handle, from, old, rows);
            }
            previous = next;
        }
        for ((text, query, handle), rows) in queries.iter().zip(&previous) {
            db.rebuild_standing_query(&cx, handle, policy()).unwrap();
            check(&db, &cx, text, query, handle, rows, policy());
            let before = saved(&db.standing_queries);
            for refused in [
                GqlQueryPolicy::new(100_000, 100_000, 0, 50_000_000),
                GqlQueryPolicy::new(100_000, 100_000, 50_000_000, 0),
            ] {
                assert!(
                    db.register_standing_native(&cx, text, &parameters, symbols, refused)
                        .is_err()
                );
                assert_eq!(saved(&db.standing_queries), before);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn post_summary_projection_errors_cannot_publish_or_hide_behind_final_pages() {
    let ((), report) = run_async_under_lab(0x7073_7011, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xe8)).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let parameters = GqlParameters::new();
        let safe = "MATCH (a:L) WITH a CALL (a) { MATCH (a)-[:R]->(b:M) WITH count(*) AS n WHERE n > 0 RETURN 12/n AS quotient } RETURN a,quotient";
        let handle = db
            .register_standing_native(&cx, safe, &parameters, symbols, policy())
            .unwrap();
        let rows = bag([
            (row(vec![vertex(Some(1)), integer(6)]), 1),
            (row(vec![vertex(Some(3)), integer(12)]), 1),
        ]);
        check(&db, &cx, safe, &bind(safe), &handle, &rows, policy());
        for (tail, expected) in [
            ("", rows.clone()),
            (" ORDER BY quotient LIMIT 0", Bag::new()),
            (
                " ORDER BY quotient LIMIT 1",
                bag([(row(vec![vertex(Some(1)), integer(6)]), 1)]),
            ),
        ] {
            let guarded = format!("{safe}{tail}");
            let positive = db
                .register_standing_native(&cx, &guarded, &parameters, symbols, policy())
                .unwrap();
            check(
                &db,
                &cx,
                &guarded,
                &bind(&guarded),
                &positive,
                &expected,
                policy(),
            );
            let before = saved(&db.standing_queries);
            for body in [
                "WITH count(*) AS n RETURN 12/n AS quotient",
                "WITH count(*) AS n WITH n,12/0 AS discarded WHERE false RETURN n AS quotient",
            ] {
                let text = format!(
                    "MATCH (a:L) WITH a CALL (a) {{ MATCH (a)-[:R]->(b:M) {body} }} RETURN a,quotient{tail}"
                );
                assert!(
                    db.execute_graph_set_governed(&cx, &bind(&text), policy())
                        .is_err()
                );
                let prepared = PreparedNativeRead::prepare(&text, &parameters, symbols).unwrap();
                prepared.subscription_footprint(&cx, &parameters).unwrap();
                assert_summary_division_by_zero(
                    prepared
                        .register_standing(&mut db, &cx, &parameters, policy())
                        .unwrap_err(),
                );
                assert_eq!(saved(&db.standing_queries), before);
            }
        }
        check(&db, &cx, safe, &bind(safe), &handle, &rows, policy());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn floating(value: f64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(value)))
}

fn null_summaries() -> [GraphValue; 6] {
    std::array::from_fn(|_| GraphValue::Scalar(CanonicalScalar::Null))
}

// Fixture literals only: this helper formats the six native result domains.
fn scalar_summaries(
    sum: i64,
    distinct_sum: i64,
    average: f64,
    distinct_average: f64,
    min: i64,
    max: i64,
) -> [GraphValue; 6] {
    [
        integer(sum),
        integer(distinct_sum),
        floating(average),
        floating(distinct_average),
        integer(min),
        integer(max),
    ]
}

fn scalar_aggregates_text(aggregate: &str) -> String {
    format!(
        "MATCH (a:L) UNWIND [NULL,1,3,3] AS floor \
         WITH a,a.p AS score,toInteger(floor) AS floor \
         CALL (a,score,floor) {{ \
         MATCH (a)-[:R]->(b:M {{selected:1}}) \
         WHERE (floor IS NULL OR score >= floor) AND b <> a \
         WITH score + id(b) AS value RETURN {aggregate} AS total }} \
         RETURN a,floor,total"
    )
}

// Each explicit result tuple belongs to NULL, one, or three respectively.
// The duplicate three restores two outer occurrences without scaling the body.
fn scalar_aggregate_bags(entries: &[(u128, [[GraphValue; 6]; 3])]) -> [Bag; 6] {
    let mut bags: [Bag; 6] = std::array::from_fn(|_| Bag::new());
    for (owner, results) in entries {
        for ((floor, multiplicity), values) in [(None, 1), (Some(1), 1), (Some(3), 2)]
            .into_iter()
            .zip(results)
        {
            for (bag, value) in bags.iter_mut().zip(values) {
                bag.insert(
                    row(vec![
                        vertex(Some(*owner)),
                        floor.map_or(GraphValue::Scalar(CanonicalScalar::Null), integer),
                        value.clone(),
                    ]),
                    multiplicity,
                );
            }
        }
    }
    bags
}

#[test]
fn filtered_scalar_aggregates_maintain_nullable_duplicate_invocations_and_retractions() {
    let ((), report) = run_async_under_lab(0x7361_7010, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xf3)).await.unwrap();
        let mut initial = WriteBatch::new(R);
        for (id, labels, props) in [
            (
                1,
                vec![L, M],
                vec![
                    (P, CanonicalScalar::Int(2)),
                    (SELECTED, CanonicalScalar::Int(1)),
                ],
            ),
            (2, vec![M], vec![(SELECTED, CanonicalScalar::Int(1))]),
            (3, vec![L], vec![(P, CanonicalScalar::Int(1))]),
            (4, vec![M], vec![(SELECTED, CanonicalScalar::Int(1))]),
            (5, vec![L], vec![]),
            (6, vec![M], vec![(SELECTED, CanonicalScalar::Int(0))]),
        ] {
            initial.create_vertex(VId(id), labels, props);
        }
        for (id, src, dst) in [
            (11, 1, 2),
            (12, 1, 2),
            (13, 1, 4),
            (14, 1, 1),
            (15, 3, 4),
            (16, 5, 2),
            (17, 1, 6),
        ] {
            initial.add_edge(EId(id), VId(src), VId(dst), vec![]);
        }
        db.write(&commit, initial).await.unwrap();
        let parameters = GqlParameters::new();
        let mut queries = Vec::new();
        for aggregate in [
            "sum(value)",
            "sum(DISTINCT value)",
            "avg(value)",
            "avg(DISTINCT value)",
            "min(value)",
            "max(value)",
        ] {
            let text = scalar_aggregates_text(aggregate);
            let prepared = PreparedNativeRead::prepare(&text, &parameters, symbols).unwrap();
            let (nodes, _) = prepared.subscription_footprint(&cx, &parameters).unwrap();
            let first = db.standing_queries.len();
            let handle = prepared
                .register_standing(&mut db, &cx, &parameters, policy())
                .unwrap();
            assert!(nodes >= (db.standing_queries.len() - first) as u64);
            assert!(
                db.standing_queries[first..]
                    .iter()
                    .any(|query| matches!(query, StandingQuery::Group(_)))
            );
            queries.push((text.clone(), bind(&text), handle));
        }
        let nulls = null_summaries();
        let one = scalar_summaries(14, 10, 14.0 / 3.0, 5.0, 4, 6);
        let three = scalar_summaries(5, 5, 5.0, 5.0, 5, 5);
        let mut expected = scalar_aggregate_bags(&[
            (1, [one.clone(), one, nulls.clone()]),
            (3, [three.clone(), three.clone(), nulls.clone()]),
            (5, [nulls.clone(), nulls.clone(), nulls.clone()]),
        ]);
        for ((text, query, handle), rows) in queries.iter().zip(&expected) {
            check(&db, &cx, text, query, handle, rows, policy());
        }
        for phase in 0..5 {
            let from = db.frontier().unwrap();
            let mut change = WriteBatch::new(R);
            match phase {
                0 => {
                    // Simultaneous argument replacement and witness retraction.
                    change.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(4)));
                    change.delete_edge(EId(11));
                }
                1 => {
                    change.delete_edge(EId(13));
                    change.set_vertex_property(VId(4), SELECTED, None);
                }
                2 => {
                    change.set_vertex_property(VId(4), SELECTED, Some(CanonicalScalar::Int(1)));
                    change.add_edge(EId(21), VId(1), VId(4), vec![]);
                    change.delete_edge(EId(12));
                }
                3 => {
                    change.delete_edge(EId(21));
                    // All-NULL input becomes empty without changing its NULL result.
                    change.delete_edge(EId(16));
                }
                4 => {
                    change.set_vertex_property(VId(6), P, Some(CanonicalScalar::Int(99)));
                }
                _ => unreachable!(),
            }
            db.write(&commit, change).await.unwrap();
            let one = match phase {
                0 => scalar_summaries(14, 14, 7.0, 7.0, 6, 8),
                1 => scalar_summaries(6, 6, 6.0, 6.0, 6, 6),
                2 => scalar_summaries(8, 8, 8.0, 8.0, 8, 8),
                _ => nulls.clone(),
            };
            let three = if phase == 1 {
                nulls.clone()
            } else {
                three.clone()
            };
            let next = scalar_aggregate_bags(&[
                (1, [one.clone(), one.clone(), one]),
                (3, [three.clone(), three, nulls.clone()]),
                (5, [nulls.clone(), nulls.clone(), nulls.clone()]),
            ]);
            for (((text, query, handle), before), after) in queries.iter().zip(&expected).zip(&next)
            {
                check(&db, &cx, text, query, handle, after, policy());
                check_delta(&db, &cx, handle, from, before, after);
            }
            expected = next;
        }
        for ((text, query, handle), rows) in queries.iter().zip(&expected) {
            db.rebuild_standing_query(&cx, handle, policy()).unwrap();
            check(&db, &cx, text, query, handle, rows, policy());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn scalar_call_text(aggregate: &str) -> String {
    format!(
        "MATCH (a:L) UNWIND [7,7] AS token WITH token,a \
         CALL (a) {{ MATCH (a)-[:R]->(b:M) RETURN {aggregate} AS degree }} \
         RETURN token,a,degree"
    )
}

fn scalar_call_bag(values: [GraphValue; 3]) -> Bag {
    bag([1, 3, 5]
        .into_iter()
        .zip(values)
        .map(|(owner, value)| (row(vec![integer(7), vertex(Some(owner)), value]), 2)))
}

#[test]
fn filtered_extrema_restore_vertex_and_text_values_without_integer_coercion() {
    let ((), report) = run_async_under_lab(0x7361_7011, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xf4)).await.unwrap();
        let mut initial = seed();
        initial.add_edge(EId(15), VId(1), VId(4), vec![]);
        db.write(&commit, initial).await.unwrap();
        let mut queries = Vec::new();
        for aggregate in ["min(b)", "max(b)", "min(elementId(b))", "max(elementId(b))"] {
            let text = scalar_call_text(aggregate);
            let handle = db
                .register_standing_native(&cx, &text, &GqlParameters::new(), symbols, policy())
                .unwrap();
            queries.push((text.clone(), bind(&text), handle));
        }
        let text_value =
            |value: &str| GraphValue::Scalar(CanonicalScalar::ucs_basic_text(value).unwrap());
        let null = GraphValue::Scalar(CanonicalScalar::Null);
        let mut expected = [
            scalar_call_bag([vertex(Some(2)), vertex(Some(4)), null.clone()]),
            scalar_call_bag([vertex(Some(4)), vertex(Some(4)), null.clone()]),
            scalar_call_bag([text_value("2"), text_value("4"), null.clone()]),
            scalar_call_bag([text_value("4"), text_value("4"), null.clone()]),
        ];
        for ((text, query, handle), rows) in queries.iter().zip(&expected) {
            check(&db, &cx, text, query, handle, rows, policy());
        }
        for phase in 0..4 {
            let from = db.frontier().unwrap();
            let mut change = WriteBatch::new(R);
            match phase {
                0 => change.delete_edge(EId(11)),
                1 => change.delete_edge(EId(12)),
                2 => change.delete_edge(EId(15)),
                3 => change.delete_vertex(VId(4)),
                _ => unreachable!(),
            };
            db.write(&commit, change).await.unwrap();
            let (min, max) = match phase {
                0 => (Some(2), Some(4)),
                1 => (Some(4), Some(4)),
                _ => (None, None),
            };
            let other = if phase == 3 { None } else { Some(4) };
            let as_text = |id: Option<u128>| {
                id.map_or_else(|| null.clone(), |id| text_value(&id.to_string()))
            };
            let next = [
                scalar_call_bag([vertex(min), vertex(other), null.clone()]),
                scalar_call_bag([vertex(max), vertex(other), null.clone()]),
                scalar_call_bag([as_text(min), as_text(other), null.clone()]),
                scalar_call_bag([as_text(max), as_text(other), null.clone()]),
            ];
            for (((text, query, handle), before), after) in queries.iter().zip(&expected).zip(&next)
            {
                check(&db, &cx, text, query, handle, after, policy());
                check_delta(&db, &cx, handle, from, before, after);
            }
            expected = next;
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn filtered_scalar_aggregate_witnesses_preserve_final_quota_and_budget_atomicity() {
    let ((), report) = run_async_under_lab(0x7361_7012, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xf5)).await.unwrap();
        let mut initial = WriteBatch::new(R);
        initial.create_vertex(VId(1), vec![L], vec![(P, CanonicalScalar::Int(2))]);
        initial.create_vertex(VId(2), vec![M], vec![(SELECTED, CanonicalScalar::Int(1))]);
        for edge in 10..74 {
            initial.add_edge(EId(edge), VId(1), VId(2), vec![]);
        }
        db.write(&commit, initial).await.unwrap();
        let four = GqlQueryPolicy::new(1_000, 4, 50_000_000, 50_000_000);
        let value = scalar_summaries(256, 4, 4.0, 4.0, 4, 4);
        let nulls = null_summaries();
        let mut expected =
            scalar_aggregate_bags(&[(1, [value.clone(), value, nulls.clone()])]).to_vec();
        let mut queries = Vec::new();
        let mut statements: Vec<_> = [
            "sum(value)",
            "sum(DISTINCT value)",
            "avg(value)",
            "avg(DISTINCT value)",
            "min(value)",
            "max(value)",
        ]
        .into_iter()
        .map(scalar_aggregates_text)
        .collect();
        for profile in 0..3 {
            let value = if profile == 2 {
                mixed_values([1, 1, 1], Some((4, 4, 4.0, 4.0)), [Some(2), Some(2)])
            } else {
                mixed_values([64, 64, 1], Some((256, 4, 4.0, 4.0)), [Some(2), Some(2)])
            };
            expected.push(mixed_bag(&[(1, [value.clone(), value, mixed_empty()])]));
            statements.push(mixed_aggregates_text(profile));
        }
        for (index, text) in statements.into_iter().enumerate() {
            let query = bind(&text);
            let handle = db
                .register_standing_native(&cx, &text, &GqlParameters::new(), symbols, four)
                .unwrap();
            check(&db, &cx, &text, &query, &handle, &expected[index], four);
            let before = saved(&db.standing_queries);
            for refused in [
                GqlQueryPolicy::new(1_000, 4, 0, 50_000_000),
                GqlQueryPolicy::new(1_000, 4, 50_000_000, 0),
                GqlQueryPolicy::new(1_000, 3, 50_000_000, 50_000_000),
            ] {
                assert!(
                    db.register_standing_native(
                        &cx,
                        &text,
                        &GqlParameters::new(),
                        symbols,
                        refused
                    )
                    .is_err()
                );
                assert_eq!(saved(&db.standing_queries), before);
            }
            check_delivery_limits(&db, &cx, &handle, &expected[index]);
            assert_eq!(saved(&db.standing_queries), before);
            check(&db, &cx, &text, &query, &handle, &expected[index], four);
            queries.push((text, query, handle));
        }
        let from = db.frontier().unwrap();
        let mut change = WriteBatch::new(R);
        change.set_vertex_property(VId(2), SELECTED, None);
        db.write(&commit, change).await.unwrap();
        let mut empty =
            scalar_aggregate_bags(&[(1, [nulls.clone(), nulls.clone(), nulls])]).to_vec();
        for _ in 0..3 {
            empty.push(mixed_bag(&[(
                1,
                [mixed_empty(), mixed_empty(), mixed_empty()],
            )]));
        }
        for (((text, query, handle), before), after) in queries.iter().zip(&expected).zip(&empty) {
            check(&db, &cx, text, query, handle, after, four);
            check_delta(&db, &cx, handle, from, before, after);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn filtered_summary_tuple_keeps_dormant_errors_private_and_failed_conversion_retryable() {
    let ((), report) = run_async_under_lab(0x7361_7013, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xf6)).await.unwrap();
        let mut initial = WriteBatch::new(R);
        initial.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(i64::MAX))]);
        initial.create_vertex(VId(2), vec![M], vec![]);
        initial.add_edge(EId(11), VId(1), VId(2), vec![]);
        initial.add_edge(EId(12), VId(1), VId(2), vec![]);
        db.write(&commit, initial).await.unwrap();
        let text = concat!(
            "MATCH (a:L) WITH a,a.p AS value ",
            "CALL (a,value) { MATCH (a)-[:R]->(b:M) RETURN count(*) AS occurrences,sum(value) AS total } ",
            "RETURN a,occurrences,total"
        );
        let query = bind(text);
        let first = db.standing_queries.len();
        let handle = db
            .register_standing_native(&cx, text, &GqlParameters::new(), symbols, policy())
            .unwrap();
        check(&db, &cx, text, &query, &handle, &Bag::new(), policy());
        let group = (first..db.standing_queries.len())
            .find(|&index| matches!(db.standing_queries[index], StandingQuery::Group(_)))
            .unwrap();
        let old_frontier = db.standing_queries[group].status().1;
        let old_rows = sets::rows(&db.standing_queries[group])
            .unwrap()
            .checked_clone(LIMBS, &mut |_| Ok::<_, ()>(()))
            .unwrap();
        let mut activate = WriteBatch::new(R);
        activate.set_vertex_label(VId(1), L, true);
        db.write(&commit, activate).await.unwrap();
        assert!(matches!(
            db.standing_queries[group].status().2,
            Some(StandingQueryFailure::OutputExpression {
                error: fgdb_gql::GraphIntegerError {
                    kind: fgdb_gql::GraphIntegerErrorKind::Overflow,
                    ..
                },
                ..
            })
        ));
        assert_eq!(db.standing_queries[group].status().1, old_frontier);
        assert_eq!(sets::rows(&db.standing_queries[group]).unwrap(), &old_rows);
        assert!(db.standing_native_query(&cx, &handle, policy()).is_err());
        let before = saved(&db.standing_queries);
        let hidden_sum = concat!(
            "MATCH (a:L) WITH a,a.p AS value ",
            "CALL (a,value) { MATCH (a)-[:R]->(b:M) RETURN count(*) AS occurrences,sum(value) AS total } ",
            "RETURN a,occurrences"
        );
        for candidate in [
            text.to_owned(),
            format!("{text} ORDER BY total LIMIT 0"),
            hidden_sum.to_owned(),
            format!("{hidden_sum} LIMIT 0"),
        ] {
            assert!(
                db.execute_graph_set_governed(&cx, &bind(&candidate), policy())
                    .is_err()
            );
            assert!(
                db.register_standing_native(
                    &cx,
                    &candidate,
                    &GqlParameters::new(),
                    symbols,
                    policy()
                )
                .is_err()
            );
            assert_eq!(saved(&db.standing_queries), before);
        }
        let mut repair = WriteBatch::new(R);
        repair.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(1)));
        db.write(&commit, repair).await.unwrap();
        assert!(db.standing_native_query(&cx, &handle, policy()).is_err());
        let before = saved(&db.standing_queries);
        assert!(
            db.rebuild_standing_query(&cx, &handle, GqlQueryPolicy::new(1_000, 4, 0, 50_000_000))
                .is_err()
        );
        assert_eq!(saved(&db.standing_queries), before);
        db.rebuild_standing_query(&cx, &handle, policy()).unwrap();
        let two = bag([(row(vec![vertex(Some(1)), integer(2), integer(2)]), 1)]);
        check(&db, &cx, text, &query, &handle, &two, policy());
        let mut invalid = WriteBatch::new(R);
        invalid.set_vertex_property(
            VId(1),
            P,
            Some(CanonicalScalar::ucs_basic_text("bad").unwrap()),
        );
        db.write(&commit, invalid).await.unwrap();
        assert!(matches!(
            db.standing_queries[group].status().2,
            Some(StandingQueryFailure::NonIntegerAggregate { .. })
        ));
        assert!(db.standing_native_query(&cx, &handle, policy()).is_err());
        let before = saved(&db.standing_queries);
        assert!(db.rebuild_standing_query(&cx, &handle, policy()).is_err());
        assert_eq!(saved(&db.standing_queries), before);
        let mut repair = WriteBatch::new(R);
        repair.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(0)));
        db.write(&commit, repair).await.unwrap();
        db.rebuild_standing_query(&cx, &handle, policy()).unwrap();
        let zero = bag([(row(vec![vertex(Some(1)), integer(2), integer(0)]), 1)]);
        check(&db, &cx, text, &query, &handle, &zero, policy());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn mixed_aggregates_text(profile: usize) -> String {
    let stages = match profile {
        0 => "WITH b,score+id(b) AS value ",
        1 => "WITH DISTINCT b,r,score WITH b,score+id(b) AS value ",
        2 => {
            "WITH DISTINCT b,r,score WITH b,score+id(b) AS value WITH DISTINCT b,value WHERE value IS NULL OR value <> 8 "
        }
        _ => unreachable!(),
    };
    format!(
        "MATCH (a:L) UNWIND [NULL,1,3,3] AS floor \
         WITH a,a.p AS score,toInteger(floor) AS floor \
         CALL (a,score,floor) {{ MATCH (a)-[r:R]->(b:M {{selected:1}}) \
         WHERE (floor IS NULL OR score >= floor) AND b <> a \
         {stages}RETURN count(*) AS occurrences,count(value) AS nonnull, \
         count(DISTINCT value) AS unique_values,sum(value) AS total, \
         sum(DISTINCT value) AS unique_total,avg(value) AS mean, \
         avg(DISTINCT value) AS unique_mean,min(b) AS least,max(b) AS greatest }} \
         RETURN a,floor,occurrences,nonnull,unique_values,total,unique_total,mean,unique_mean,least,greatest"
    )
}

// Format independently stated fixture results; no production aggregate kernel
// or query execution participates in the expected values.
fn mixed_values(
    counts: [i64; 3],
    numeric: Option<(i64, i64, f64, f64)>,
    extrema: [Option<u128>; 2],
) -> Vec<GraphValue> {
    let mut values: Vec<_> = counts.into_iter().map(integer).collect();
    if let Some((sum, distinct_sum, mean, distinct_mean)) = numeric {
        values.extend([
            integer(sum),
            integer(distinct_sum),
            floating(mean),
            floating(distinct_mean),
        ]);
    } else {
        values.extend((0..4).map(|_| vertex(None)));
    }
    values.extend(extrema.into_iter().map(vertex));
    values
}

fn mixed_empty() -> Vec<GraphValue> {
    mixed_values([0, 0, 0], None, [None, None])
}

fn mixed_bag(entries: &[(u128, [Vec<GraphValue>; 3])]) -> Bag {
    let mut result = Bag::new();
    for (owner, summaries) in entries {
        for ((floor, multiplicity), summary) in [(None, 1), (Some(1), 1), (Some(3), 2)]
            .into_iter()
            .zip(summaries)
        {
            let mut cells = vec![
                vertex(Some(*owner)),
                floor.map_or_else(|| vertex(None), integer),
            ];
            cells.extend(summary.iter().cloned());
            result.insert(row(cells), multiplicity);
        }
    }
    result
}

#[test]
fn mixed_aggregate_tuples_maintain_distinct_body_support_and_complete_deltas() {
    let ((), report) = run_async_under_lab(0x7361_7014, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xf7)).await.unwrap();
        let mut initial = WriteBatch::new(R);
        for (id, labels, props) in [
            (
                1,
                vec![L, M],
                vec![
                    (P, CanonicalScalar::Int(2)),
                    (SELECTED, CanonicalScalar::Int(1)),
                ],
            ),
            (2, vec![M], vec![(SELECTED, CanonicalScalar::Int(1))]),
            (3, vec![L], vec![(P, CanonicalScalar::Int(1))]),
            (4, vec![M], vec![(SELECTED, CanonicalScalar::Int(1))]),
            (5, vec![L], vec![]),
            (6, vec![M], vec![(SELECTED, CanonicalScalar::Int(0))]),
        ] {
            initial.create_vertex(VId(id), labels, props);
        }
        for (id, src, dst) in [
            (11, 1, 2),
            (12, 1, 2),
            (13, 1, 4),
            (14, 1, 1),
            (15, 3, 4),
            (16, 5, 2),
            (17, 1, 6),
        ] {
            initial.add_edge(EId(id), VId(src), VId(dst), vec![]);
        }
        db.write(&commit, initial).await.unwrap();
        let mut queries = Vec::new();
        for profile in 0..3 {
            let text = mixed_aggregates_text(profile);
            let parameters = GqlParameters::new();
            let prepared = PreparedNativeRead::prepare(&text, &parameters, symbols).unwrap();
            let (nodes, _) = prepared.subscription_footprint(&cx, &parameters).unwrap();
            let first = db.standing_queries.len();
            let handle = prepared
                .register_standing(&mut db, &cx, &parameters, policy())
                .unwrap();
            assert!(nodes >= u64::try_from(db.standing_queries.len() - first).unwrap());
            assert_eq!(
                db.standing_queries[first..]
                    .iter()
                    .filter(|query| matches!(query, StandingQuery::Group(_)))
                    .count(),
                1
            );
            queries.push((text.clone(), bind(&text), handle));
        }
        let three = mixed_values([1, 1, 1], Some((5, 5, 5.0, 5.0)), [Some(4), Some(4)]);
        let all_null = mixed_values([1, 0, 0], None, [Some(2), Some(2)]);
        let mut expected: Vec<_> = (0..3)
            .map(|profile| {
                let one = if profile == 2 {
                    mixed_values([2, 2, 2], Some((10, 10, 5.0, 5.0)), [Some(2), Some(4)])
                } else {
                    mixed_values(
                        [3, 3, 2],
                        Some((14, 10, 14.0 / 3.0, 5.0)),
                        [Some(2), Some(4)],
                    )
                };
                mixed_bag(&[
                    (1, [one.clone(), one, mixed_empty()]),
                    (3, [three.clone(), three.clone(), mixed_empty()]),
                    (5, [all_null.clone(), mixed_empty(), mixed_empty()]),
                ])
            })
            .collect();
        for ((text, query, handle), rows) in queries.iter().zip(&expected) {
            check(&db, &cx, text, query, handle, rows, policy());
        }
        for phase in 0..7 {
            let from = db.frontier().unwrap();
            let mut change = WriteBatch::new(R);
            match phase {
                0 => {
                    change.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(4)));
                    change.delete_edge(EId(11));
                }
                1 => {
                    change.delete_edge(EId(13));
                    change.set_vertex_property(VId(4), SELECTED, None);
                }
                2 => {
                    change.set_vertex_property(VId(4), SELECTED, Some(CanonicalScalar::Int(1)));
                    change.add_edge(EId(21), VId(1), VId(4), vec![]);
                    change.delete_edge(EId(12));
                }
                3 => {
                    change.delete_edge(EId(21));
                    change.delete_edge(EId(16));
                }
                4 => {
                    change.add_edge(EId(22), VId(1), VId(2), vec![]);
                    change.add_edge(EId(23), VId(1), VId(2), vec![]);
                }
                5 => {
                    change.delete_edge(EId(22));
                }
                6 => {
                    change.delete_edge(EId(23));
                }
                _ => unreachable!(),
            }
            db.write(&commit, change).await.unwrap();
            let next: Vec<_> = (0..3)
                .map(|profile| {
                    let one = if profile == 2 {
                        match phase {
                            0 | 1 | 4 | 5 => {
                                mixed_values([1, 1, 1], Some((6, 6, 6.0, 6.0)), [Some(2), Some(2)])
                            }
                            _ => mixed_empty(),
                        }
                    } else {
                        match phase {
                            0 => mixed_values(
                                [2, 2, 2],
                                Some((14, 14, 7.0, 7.0)),
                                [Some(2), Some(4)],
                            ),
                            1 | 5 => {
                                mixed_values([1, 1, 1], Some((6, 6, 6.0, 6.0)), [Some(2), Some(2)])
                            }
                            2 => {
                                mixed_values([1, 1, 1], Some((8, 8, 8.0, 8.0)), [Some(4), Some(4)])
                            }
                            4 => {
                                mixed_values([2, 2, 1], Some((12, 6, 6.0, 6.0)), [Some(2), Some(2)])
                            }
                            _ => mixed_empty(),
                        }
                    };
                    let three = if phase == 1 {
                        mixed_empty()
                    } else {
                        three.clone()
                    };
                    let five = if phase < 3 {
                        all_null.clone()
                    } else {
                        mixed_empty()
                    };
                    mixed_bag(&[
                        (1, [one.clone(), one.clone(), one]),
                        (3, [three.clone(), three, mixed_empty()]),
                        (5, [five, mixed_empty(), mixed_empty()]),
                    ])
                })
                .collect();
            for (((text, query, handle), before), after) in queries.iter().zip(&expected).zip(&next)
            {
                check(&db, &cx, text, query, handle, after, policy());
                check_delta(&db, &cx, handle, from, before, after);
            }
            expected = next;
        }
        for ((text, query, handle), rows) in queries.iter().zip(&expected) {
            db.rebuild_standing_query(&cx, handle, policy()).unwrap();
            check(&db, &cx, text, query, handle, rows, policy());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn continued_summary_text(body: &str) -> String {
    format!(
        "MATCH (a:L) UNWIND [7,7] AS token WITH token,a \
         CALL (a) {{ MATCH (a)-[:R]->(b:M) {body} }} RETURN token,a,degree,total"
    )
}

// Degree and total below are already transformed fixture answers. This helper
// only formats their public schema and the two original occurrences.
fn continued_summary_bag(entries: &[(u128, i64, Option<i64>)]) -> Bag {
    bag(entries.iter().map(|&(owner, degree, total)| {
        (
            row(vec![
                integer(7),
                vertex(Some(owner)),
                integer(degree),
                total.map_or(vertex(None), integer),
            ]),
            2,
        )
    }))
}

#[test]
fn correlated_summary_continuations_keep_empty_groups_duplicates_and_exact_deltas() {
    let ((), report) = run_async_under_lab(0x7361_7015, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xf8)).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let mut queries = Vec::new();
        for (body, filtered) in [
            ("RETURN count(*)+1 AS degree,sum(id(b))+1 AS total", false),
            (
                "WITH count(*) AS n,sum(id(b)) AS s RETURN n+1 AS degree,s+1 AS total",
                false,
            ),
            (
                "WITH count(*) AS n,sum(id(b)) AS s WHERE n>0 \
                 RETURN n+1 AS degree,s+1 AS total",
                true,
            ),
            (
                "WITH count(*) AS n,sum(id(b)) AS s \
                 WITH n+1 AS degree,s+1 AS total WHERE degree>1 RETURN degree,total",
                true,
            ),
        ] {
            let text = continued_summary_text(body);
            let parameters = GqlParameters::new();
            let prepared = PreparedNativeRead::prepare(&text, &parameters, symbols).unwrap();
            let (nodes, _) = prepared.subscription_footprint(&cx, &parameters).unwrap();
            let first = db.standing_queries.len();
            let handle = prepared
                .register_standing(&mut db, &cx, &parameters, policy())
                .unwrap();
            assert!(nodes >= u64::try_from(db.standing_queries.len() - first).unwrap());
            assert_eq!(
                db.standing_queries[first..]
                    .iter()
                    .filter(|query| matches!(query, StandingQuery::Group(_)))
                    .count(),
                1,
                "the complete tuple shares one maintained group: {text}",
            );
            queries.push((text.clone(), bind(&text), handle, usize::from(filtered)));
        }
        let mut expected = [
            continued_summary_bag(&[(1, 3, Some(5)), (3, 2, Some(5)), (5, 1, None)]),
            continued_summary_bag(&[(1, 3, Some(5)), (3, 2, Some(5))]),
        ];
        for (text, query, handle, selected) in &queries {
            check(
                &db,
                &cx,
                text,
                query,
                handle,
                &expected[*selected],
                policy(),
            );
        }
        for phase in 0..7 {
            let from = db.frontier().unwrap();
            let mut change = WriteBatch::new(R);
            match phase {
                0 => {
                    change.delete_edge(EId(11));
                }
                1 => {
                    change.delete_edge(EId(12));
                    change.add_edge(EId(15), VId(5), VId(2), vec![]);
                }
                2 => {
                    change.set_vertex_label(VId(4), M, false);
                }
                3 => {
                    change.set_vertex_label(VId(6), M, true);
                    change.add_edge(EId(16), VId(1), VId(6), vec![]);
                }
                4 => {
                    change.delete_vertex(VId(2));
                }
                5 => {
                    change.set_vertex_label(VId(4), M, true);
                }
                6 => {
                    change.delete_vertex(VId(5));
                }
                _ => unreachable!(),
            }
            db.write(&commit, change).await.unwrap();
            let next = match phase {
                0 => [
                    continued_summary_bag(&[(1, 2, Some(3)), (3, 2, Some(5)), (5, 1, None)]),
                    continued_summary_bag(&[(1, 2, Some(3)), (3, 2, Some(5))]),
                ],
                1 => [
                    continued_summary_bag(&[(1, 1, None), (3, 2, Some(5)), (5, 2, Some(3))]),
                    continued_summary_bag(&[(3, 2, Some(5)), (5, 2, Some(3))]),
                ],
                2 => [
                    continued_summary_bag(&[(1, 1, None), (3, 1, None), (5, 2, Some(3))]),
                    continued_summary_bag(&[(5, 2, Some(3))]),
                ],
                3 => [
                    continued_summary_bag(&[(1, 2, Some(7)), (3, 1, None), (5, 3, Some(9))]),
                    continued_summary_bag(&[(1, 2, Some(7)), (5, 3, Some(9))]),
                ],
                4 => [
                    continued_summary_bag(&[(1, 2, Some(7)), (3, 1, None), (5, 2, Some(7))]),
                    continued_summary_bag(&[(1, 2, Some(7)), (5, 2, Some(7))]),
                ],
                5 => {
                    let rows =
                        continued_summary_bag(&[(1, 2, Some(7)), (3, 2, Some(5)), (5, 2, Some(7))]);
                    [rows.clone(), rows]
                }
                6 => {
                    let rows = continued_summary_bag(&[(1, 2, Some(7)), (3, 2, Some(5))]);
                    [rows.clone(), rows]
                }
                _ => unreachable!(),
            };
            for (text, query, handle, selected) in &queries {
                check(&db, &cx, text, query, handle, &next[*selected], policy());
                check_delta(
                    &db,
                    &cx,
                    handle,
                    from,
                    &expected[*selected],
                    &next[*selected],
                );
            }
            expected = next;
        }
        for (text, query, handle, selected) in &queries {
            db.rebuild_standing_query(&cx, handle, policy()).unwrap();
            check(
                &db,
                &cx,
                text,
                query,
                handle,
                &expected[*selected],
                policy(),
            );
        }
        super::tests::check_first_tick_after_rebuild(
            &mut db,
            &cx,
            &commit,
            queries.iter().map(|entry| &entry.2),
        )
        .await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn correlated_summary_filters_precede_projection_errors_but_public_pages_do_not() {
    let ((), report) = run_async_under_lab(0x7361_7016, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xf9)).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let guarded = concat!(
            "MATCH (a:L) UNWIND [7,7] AS token WITH token,a ",
            "CALL (a) { MATCH (a)-[:R]->(b:M) WITH count(*) AS n WHERE n>0 ",
            "RETURN n AS degree,1/n AS hidden } RETURN token,a,degree"
        );
        let handle = db
            .register_standing_native(&cx, guarded, &GqlParameters::new(), symbols, policy())
            .unwrap();
        let expected = bag([
            (row(vec![integer(7), vertex(Some(1)), integer(2)]), 2),
            (row(vec![integer(7), vertex(Some(3)), integer(1)]), 2),
        ]);
        check(
            &db,
            &cx,
            guarded,
            &bind(guarded),
            &handle,
            &expected,
            policy(),
        );
        let paged = format!("{guarded} ORDER BY degree LIMIT 0");
        let paged_handle = db
            .register_standing_native(&cx, &paged, &GqlParameters::new(), symbols, policy())
            .unwrap();
        check(
            &db,
            &cx,
            &paged,
            &bind(&paged),
            &paged_handle,
            &Bag::new(),
            policy(),
        );
        let before = saved(&db.standing_queries);
        let broken = concat!(
            "MATCH (a:L) UNWIND [7,7] AS token WITH token,a ",
            "CALL (a) { MATCH (a)-[:R]->(b:M) WITH count(*) AS n ",
            "RETURN n AS degree,1/n AS hidden } RETURN token,a,degree"
        );
        for candidate in [
            broken.to_owned(),
            format!("{broken} ORDER BY degree LIMIT 0"),
        ] {
            assert!(
                db.execute_graph_set_governed(&cx, &bind(&candidate), policy())
                    .is_err(),
                "the empty invocation evaluates its complete summary projection",
            );
            let parameters = GqlParameters::new();
            let prepared = PreparedNativeRead::prepare(&candidate, &parameters, symbols).unwrap();
            prepared.subscription_footprint(&cx, &parameters).unwrap();
            assert_summary_division_by_zero(
                prepared
                    .register_standing(&mut db, &cx, &parameters, policy())
                    .unwrap_err(),
            );
            assert_eq!(saved(&db.standing_queries), before);
        }
        let from = db.frontier().unwrap();
        let mut change = WriteBatch::new(R);
        change.delete_edge(EId(11));
        change.delete_edge(EId(12));
        db.write(&commit, change).await.unwrap();
        let next = bag([(row(vec![integer(7), vertex(Some(3)), integer(1)]), 2)]);
        check(&db, &cx, guarded, &bind(guarded), &handle, &next, policy());
        check_delta(&db, &cx, &handle, from, &expected, &next);
        db.rebuild_standing_query(&cx, &handle, policy()).unwrap();
        check(&db, &cx, guarded, &bind(guarded), &handle, &next, policy());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn correlated_summary_continuations_charge_public_occurrences_and_stage_atomically() {
    let ((), report) = run_async_under_lab(0x7361_7017, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xfa)).await.unwrap();
        let mut initial = WriteBatch::new(R);
        initial.create_vertex(VId(1), vec![L], vec![]);
        initial.create_vertex(VId(2), vec![M], vec![]);
        for edge in 10..74 {
            initial.add_edge(EId(edge), VId(1), VId(2), vec![]);
        }
        db.write(&commit, initial).await.unwrap();
        let text = continued_summary_text(
            "WITH count(*) AS n,sum(id(b)) AS s WHERE n>0 \
             RETURN n+1 AS degree,s+1 AS total",
        );
        let query = bind(&text);
        let two = GqlQueryPolicy::new(1_000, 2, 50_000_000, 50_000_000);
        let expected = continued_summary_bag(&[(1, 65, Some(129))]);
        let handle = db
            .register_standing_native(&cx, &text, &GqlParameters::new(), symbols, two)
            .unwrap();
        check(&db, &cx, &text, &query, &handle, &expected, two);
        let before = saved(&db.standing_queries);
        for refused in [
            GqlQueryPolicy::new(1_000, 1, 50_000_000, 50_000_000),
            GqlQueryPolicy::new(1_000, 2, 0, 50_000_000),
            GqlQueryPolicy::new(1_000, 2, 50_000_000, 0),
        ] {
            assert!(
                db.register_standing_native(&cx, &text, &GqlParameters::new(), symbols, refused)
                    .is_err()
            );
            assert_eq!(saved(&db.standing_queries), before);
            assert!(db.rebuild_standing_query(&cx, &handle, refused).is_err());
            assert_eq!(saved(&db.standing_queries), before);
            check(&db, &cx, &text, &query, &handle, &expected, two);
        }
        let suppressed = continued_summary_text(
            "WITH count(*) AS n,sum(id(b)) AS s WHERE n>100 \
             RETURN n+1 AS degree,s+1 AS total",
        );
        let hidden = db
            .register_standing_native(&cx, &suppressed, &GqlParameters::new(), symbols, two)
            .unwrap();
        check(
            &db,
            &cx,
            &suppressed,
            &bind(&suppressed),
            &hidden,
            &Bag::new(),
            two,
        );
        db.rebuild_standing_query(&cx, &hidden, two).unwrap();
        // The outer source still needs its two-row admission even when the
        // completed summary is filtered away. Preserve that existing boundary.
        let before = saved(&db.standing_queries);
        let zero = GqlQueryPolicy::new(1_000, 0, 50_000_000, 50_000_000);
        assert!(db.rebuild_standing_query(&cx, &hidden, zero).is_err());
        assert_eq!(saved(&db.standing_queries), before);
        check(&db, &cx, &text, &query, &handle, &expected, two);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn optional_summary_bag(
    entries: impl IntoIterator<Item = (u128, Option<i64>, Option<i64>)>,
) -> Bag {
    bag(entries.into_iter().map(|(owner, degree, total)| {
        (
            row(vec![
                integer(7),
                vertex(Some(owner)),
                degree.map_or(vertex(None), integer),
                total.map_or(vertex(None), integer),
            ]),
            2,
        )
    }))
}

#[test]
fn optional_summary_calls_distinguish_empty_aggregates_from_filtered_absence() {
    let ((), report) = run_async_under_lab(0x7361_7018, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xfb)).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let mut queries = Vec::new();
        for body in [
            "RETURN count(*) AS degree,sum(id(b)) AS total",
            "WITH count(*) AS n,sum(id(b)) AS s RETURN n+1 AS degree,s+1 AS total",
            "WITH count(*) AS n,sum(id(b)) AS s WHERE n>0 RETURN n+1 AS degree,s+1 AS total",
            "WITH sum(id(b)) AS total WHERE total IS NULL RETURN 7 AS degree,total",
        ] {
            let text = format!(
                "MATCH (a:L) UNWIND [7,7] AS token WITH token,a \
                 OPTIONAL CALL (a) {{ MATCH (a)-[:R]->(b:M) {body} }} \
                 RETURN token,a,degree,total"
            );
            let parameters = GqlParameters::new();
            let prepared = PreparedNativeRead::prepare(&text, &parameters, symbols).unwrap();
            let (nodes, _) = prepared.subscription_footprint(&cx, &parameters).unwrap();
            let first = db.standing_queries.len();
            let handle = prepared
                .register_standing(&mut db, &cx, &parameters, policy())
                .unwrap();
            assert!(nodes >= u64::try_from(db.standing_queries.len() - first).unwrap());
            queries.push((text.clone(), bind(&text), handle));
        }
        // Counts and sums are literal answers for each committed fixture. Only
        // the visible scalar expressions and optional NULL extension follow.
        let expected =
            |groups: &[(u128, i64, Option<i64>)]| {
                [
                    optional_summary_bag(
                        groups
                            .iter()
                            .map(|&(owner, count, sum)| (owner, Some(count), sum)),
                    ),
                    optional_summary_bag(groups.iter().map(|&(owner, count, sum)| {
                        (owner, Some(count + 1), sum.map(|sum| sum + 1))
                    })),
                    optional_summary_bag(groups.iter().map(|&(owner, count, sum)| {
                        if count == 0 {
                            (owner, None, None)
                        } else {
                            (owner, Some(count + 1), sum.map(|sum| sum + 1))
                        }
                    })),
                    optional_summary_bag(
                        groups
                            .iter()
                            .map(|&(owner, _, sum)| (owner, sum.is_none().then_some(7), None)),
                    ),
                ]
            };
        let mut previous = expected(&[(1, 2, Some(4)), (3, 1, Some(4)), (5, 0, None)]);
        for ((text, query, handle), rows) in queries.iter().zip(&previous) {
            check(&db, &cx, text, query, handle, rows, policy());
        }
        for phase in 0..6 {
            let from = db.frontier().unwrap();
            let mut change = WriteBatch::new(R);
            match phase {
                0 => {
                    change.delete_edge(EId(11));
                }
                1 => {
                    change.delete_edge(EId(12));
                    change.delete_edge(EId(13));
                }
                2 => {
                    change.add_edge(EId(15), VId(5), VId(2), vec![]);
                }
                3 => {
                    change.add_edge(EId(16), VId(1), VId(4), vec![]);
                    change.add_edge(EId(17), VId(1), VId(4), vec![]);
                }
                4 => {
                    change.delete_vertex(VId(2));
                }
                5 => {
                    change.set_vertex_label(VId(3), L, false);
                }
                _ => unreachable!(),
            }
            db.write(&commit, change).await.unwrap();
            let next = match phase {
                0 => expected(&[(1, 1, Some(2)), (3, 1, Some(4)), (5, 0, None)]),
                1 => expected(&[(1, 0, None), (3, 0, None), (5, 0, None)]),
                2 => expected(&[(1, 0, None), (3, 0, None), (5, 1, Some(2))]),
                3 => expected(&[(1, 2, Some(8)), (3, 0, None), (5, 1, Some(2))]),
                4 => expected(&[(1, 2, Some(8)), (3, 0, None), (5, 0, None)]),
                5 => expected(&[(1, 2, Some(8)), (5, 0, None)]),
                _ => unreachable!(),
            };
            for (((text, query, handle), before), after) in queries.iter().zip(&previous).zip(&next)
            {
                check(&db, &cx, text, query, handle, after, policy());
                check_delta(&db, &cx, handle, from, before, after);
            }
            previous = next;
        }
        for ((text, query, handle), rows) in queries.iter().zip(&previous) {
            db.rebuild_standing_query(&cx, handle, policy()).unwrap();
            check(&db, &cx, text, query, handle, rows, policy());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn optional_summary_calls_keep_nullable_invocations_and_do_not_mask_projection_errors() {
    let ((), report) = run_async_under_lab(0x7361_7019, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xfc)).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let text = concat!(
            "MATCH (a:L) WITH a OPTIONAL MATCH (a)-[:R]->(b:M) WITH a,b ",
            "OPTIONAL CALL (b) { MATCH (b)-[:R]->(c:M) ",
            "WITH count(*) AS n WHERE n>0 RETURN 12/n AS quotient } RETURN a,b,quotient"
        );
        let handle = db
            .register_standing_native(&cx, text, &GqlParameters::new(), symbols, policy())
            .unwrap();
        let query = bind(text);
        let missing = bag([
            (row(vec![vertex(Some(1)), vertex(Some(2)), vertex(None)]), 2),
            (row(vec![vertex(Some(3)), vertex(Some(4)), vertex(None)]), 1),
            (row(vec![vertex(Some(5)), vertex(None), vertex(None)]), 1),
        ]);
        check(&db, &cx, text, &query, &handle, &missing, policy());
        let from = db.frontier().unwrap();
        let mut change = WriteBatch::new(R);
        change.add_edge(EId(20), VId(2), VId(4), vec![]);
        change.add_edge(EId(21), VId(2), VId(4), vec![]);
        db.write(&commit, change).await.unwrap();
        let active = bag([
            (row(vec![vertex(Some(1)), vertex(Some(2)), integer(6)]), 2),
            (row(vec![vertex(Some(3)), vertex(Some(4)), vertex(None)]), 1),
            (row(vec![vertex(Some(5)), vertex(None), vertex(None)]), 1),
        ]);
        check(&db, &cx, text, &query, &handle, &active, policy());
        check_delta(&db, &cx, &handle, from, &missing, &active);
        for tail in ["", " ORDER BY degree LIMIT 0"] {
            let guarded = format!(
                "MATCH (a:L) WITH a OPTIONAL CALL (a) {{ MATCH (a)-[:R]->(b:M) \
                 WITH count(*) AS n WHERE n>0 RETURN n AS degree,12/n AS hidden }} RETURN a,degree{tail}"
            );
            let positive = db
                .register_standing_native(&cx, &guarded, &GqlParameters::new(), symbols, policy())
                .unwrap();
            let expected = if tail.is_empty() {
                bag([
                    (row(vec![vertex(Some(1)), integer(2)]), 1),
                    (row(vec![vertex(Some(3)), integer(1)]), 1),
                    (row(vec![vertex(Some(5)), vertex(None)]), 1),
                ])
            } else {
                Bag::new()
            };
            check(
                &db,
                &cx,
                &guarded,
                &bind(&guarded),
                &positive,
                &expected,
                policy(),
            );
            let before = saved(&db.standing_queries);
            let broken = format!(
                "MATCH (a:L) WITH a OPTIONAL CALL (a) {{ MATCH (a)-[:R]->(b:M) \
                 WITH count(*) AS n RETURN n AS degree,12/n AS hidden }} RETURN a,degree{tail}"
            );
            assert!(
                db.execute_graph_set_governed(&cx, &bind(&broken), policy())
                    .is_err()
            );
            let parameters = GqlParameters::new();
            let prepared = PreparedNativeRead::prepare(&broken, &parameters, symbols).unwrap();
            prepared.subscription_footprint(&cx, &parameters).unwrap();
            assert_summary_division_by_zero(
                prepared
                    .register_standing(&mut db, &cx, &parameters, policy())
                    .unwrap_err(),
            );
            assert_eq!(saved(&db.standing_queries), before);
        }
        let from = db.frontier().unwrap();
        let mut change = WriteBatch::new(R);
        change.delete_edge(EId(20));
        change.delete_edge(EId(21));
        db.write(&commit, change).await.unwrap();
        check(&db, &cx, text, &query, &handle, &missing, policy());
        check_delta(&db, &cx, &handle, from, &active, &missing);
        db.rebuild_standing_query(&cx, &handle, policy()).unwrap();
        check(&db, &cx, text, &query, &handle, &missing, policy());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
