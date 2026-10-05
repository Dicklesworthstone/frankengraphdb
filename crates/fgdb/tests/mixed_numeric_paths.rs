//! An Int/Float comparison answers the same in every execution path
//! (fgdb-qnqrj).
//!
//! Int and Float compare exactly across kinds: `p.born > 1800.0` holds for an
//! Int 1815. The row predicate implemented that, but the property index
//! searched only the literal's own kind, so the README's own shape,
//! `MATCH (p:Person) WHERE p.born > 1800.0`, silently returned nothing while
//! `MATCH (p:Person) WITH p WHERE p.born > 1800.0` answered. Membership in an
//! evaluated list (`WITH p, [1815.0] AS xs WHERE p.born IN xs`) still refused
//! to equate the kinds, although the literal-list form did.
//!
//! Each shape is checked against one oracle computed here, without the
//! engine's comparator: the indexed single-vertex scan (range and equality
//! probes), an expansion, a row WHERE, and IN.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, QueryResult, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind};
use fgdb_types::{
    CanonicalDecimal, CanonicalF64, CanonicalScalar, DatabaseSecurityNamespaceId, EId,
    PurposeContexts, VId,
};
use std::cmp::Ordering;

const KNOWS: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(1);
const HUB: LabelId = LabelId(2);
const ID: PropertyKeyId = PropertyKeyId(1);
const BORN: PropertyKeyId = PropertyKeyId(2);
const HUB_VID: u128 = 100;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x71; 32],
        DatabaseSecurityNamespaceId([0x72; 32]),
        [0x73; 32],
    )
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Label, "Hub") => Some(GraphSymbol::Label(HUB)),
        (GraphSymbolKind::Relation, "KNOWS") => Some(GraphSymbol::Relation(KNOWS)),
        (GraphSymbolKind::Property, "id") => Some(GraphSymbol::Property(ID)),
        (GraphSymbolKind::Property, "born") => Some(GraphSymbol::Property(BORN)),
        _ => None,
    }
}

fn float(value: f64) -> CanonicalScalar {
    CanonicalScalar::Float(CanonicalF64::new(value))
}

/// Every vertex's `born`: both numeric kinds on both sides of each literal,
/// kinds that never compare with a number, NaN, an absent property, and the
/// first integer binary64 cannot hold (2^53 + 1) beside its rounded neighbour.
fn fixture() -> Vec<(u128, Option<CanonicalScalar>)> {
    vec![
        (1, Some(CanonicalScalar::Int(1815))),
        (2, Some(CanonicalScalar::Int(1791))),
        (3, Some(float(1815.0))),
        (4, Some(float(1815.5))),
        (5, Some(float(1790.25))),
        (
            6,
            Some(CanonicalScalar::Decimal(
                CanonicalDecimal::from_integer(1815).unwrap(),
            )),
        ),
        (7, Some(CanonicalScalar::ucs_basic_text("1815").unwrap())),
        (8, Some(float(f64::NAN))),
        (9, None),
        (10, Some(CanonicalScalar::Int(9_007_199_254_740_993))),
        (11, Some(float(9_007_199_254_740_992.0))),
    ]
}

/// The literals as GQL text, each with the scalar it denotes.
fn literals() -> Vec<(&'static str, CanonicalScalar)> {
    vec![
        ("1815", CanonicalScalar::Int(1815)),
        ("1815.0", float(1815.0)),
        ("1815.5", float(1815.5)),
        ("1800.0", float(1800.0)),
        ("1790.25", float(1790.25)),
        (
            "9007199254740993",
            CanonicalScalar::Int(9_007_199_254_740_993),
        ),
        ("9007199254740992.0", float(9_007_199_254_740_992.0)),
    ]
}

/// Exact Int/Float order by i128 arithmetic, independent of the engine's
/// comparator. NaN orders after every number. Other kinds never compare.
fn exact_order(left: &CanonicalScalar, right: &CanonicalScalar) -> Option<Ordering> {
    fn int_float(integer: i64, float: f64) -> Ordering {
        if float.is_nan() {
            return Ordering::Less;
        }
        if float.is_infinite() {
            return if float > 0.0 {
                Ordering::Less
            } else {
                Ordering::Greater
            };
        }
        let floor = float.floor();
        // Saturation past 2^127 is far outside the i64 range either way.
        match i128::from(integer).cmp(&(floor as i128)) {
            Ordering::Equal if floor < float => Ordering::Less,
            order => order,
        }
    }
    fn float_float(left: f64, right: f64) -> Ordering {
        match (left.is_nan(), right.is_nan()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => left.partial_cmp(&right).unwrap(),
        }
    }
    match (left, right) {
        (CanonicalScalar::Int(left), CanonicalScalar::Int(right)) => Some(left.cmp(right)),
        (CanonicalScalar::Float(left), CanonicalScalar::Float(right)) => {
            Some(float_float(left.get(), right.get()))
        }
        (CanonicalScalar::Int(left), CanonicalScalar::Float(right)) => {
            Some(int_float(*left, right.get()))
        }
        (CanonicalScalar::Float(left), CanonicalScalar::Int(right)) => {
            Some(int_float(*right, left.get()).reverse())
        }
        _ => None,
    }
}

const OPERATORS: [&str; 6] = ["=", "<>", "<", "<=", ">", ">="];

fn expected(operator: &str, literal: &CanonicalScalar) -> Vec<i64> {
    fixture()
        .into_iter()
        .filter(|(_, born)| {
            let Some(order) = born.as_ref().and_then(|born| exact_order(born, literal)) else {
                return false;
            };
            match operator {
                "=" => order == Ordering::Equal,
                "<>" => order != Ordering::Equal,
                "<" => order == Ordering::Less,
                "<=" => order != Ordering::Greater,
                ">" => order == Ordering::Greater,
                ">=" => order != Ordering::Less,
                _ => unreachable!(),
            }
        })
        .map(|(vid, _)| vid as i64)
        .collect()
}

fn ids(result: &QueryResult) -> Vec<i64> {
    let QueryResult::Rows { rows, .. } = result else {
        panic!("a read returns rows, got {result:?}");
    };
    let mut ids: Vec<i64> = rows
        .iter()
        .map(|row| match &row[0] {
            fgdb_gql::GraphAggregateValue::Value(fgdb_gql::algebra::GraphValue::Scalar(
                CanonicalScalar::Int(value),
            )) => *value,
            other => panic!("expected an integer id, got {other:?}"),
        })
        .collect();
    ids.sort_unstable();
    ids
}

#[test]
fn every_path_answers_an_int_float_comparison_like_the_exact_oracle() {
    let ((), report) = run_async_under_lab(0x6d69_7801, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(KNOWS);
        batch.create_vertex(VId(HUB_VID), vec![HUB], vec![]);
        for (vid, born) in fixture() {
            let mut props = vec![(ID, CanonicalScalar::Int(vid as i64))];
            props.extend(born.map(|born| (BORN, born)));
            batch.create_vertex(VId(vid), vec![PERSON], props);
            batch.add_edge(EId(vid), VId(HUB_VID), VId(vid), vec![]);
        }
        db.write(&commit, batch).await.unwrap();
        let params = GqlParameters::new();
        let policy = GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000);
        let run = |text: &str| {
            ids(&db
                .query(&cx, text, &params, symbols, policy)
                .unwrap_or_else(|error| panic!("{text}: {error:?}")))
        };
        let mut nonempty = 0;
        for (text, literal) in literals() {
            for operator in OPERATORS {
                let wanted = expected(operator, &literal);
                nonempty += usize::from(!wanted.is_empty());
                for shape in [
                    // Single-vertex scan: served by the property index.
                    format!("MATCH (p:Person) WHERE p.born {operator} {text} RETURN p.id"),
                    // Expansion: the row predicate on a hydrated endpoint.
                    format!(
                        "MATCH (h:Hub)-[:KNOWS]->(p:Person) WHERE p.born {operator} {text} \
                         RETURN p.id"
                    ),
                    // Row WHERE after WITH.
                    format!("MATCH (p:Person) WITH p WHERE p.born {operator} {text} RETURN p.id"),
                ] {
                    assert_eq!(run(&shape), wanted, "{shape}");
                }
            }
            for membership in [
                // A literal list.
                format!("MATCH (p:Person) WITH p WHERE p.born IN [{text}] RETURN p.id"),
                // An evaluated list bound to an alias: native list membership.
                format!("MATCH (p:Person) WITH p, [{text}] AS xs WHERE p.born IN xs RETURN p.id"),
            ] {
                assert_eq!(run(&membership), expected("=", &literal), "{membership}");
            }
        }
        // The fixture separates every literal: no law here is vacuously empty.
        assert!(nonempty >= 35, "{nonempty} nonempty expectations");
        // The README's own cases, stated directly.
        assert_eq!(
            run("MATCH (p:Person) WHERE p.born > 1800.0 AND p.born < 1900 RETURN p.id"),
            vec![1, 3, 4]
        );
        assert_eq!(
            run("MATCH (p:Person) WHERE p.born = 1815.0 RETURN p.id"),
            vec![1, 3]
        );
        // 2^53 + 1 is not the binary64 2^53, in either direction.
        assert_eq!(
            run("MATCH (p:Person) WHERE p.born = 9007199254740992.0 RETURN p.id"),
            vec![11]
        );
        assert_eq!(
            run("MATCH (p:Person) WHERE p.born > 9007199254740992.0 RETURN p.id"),
            vec![8, 10]
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
