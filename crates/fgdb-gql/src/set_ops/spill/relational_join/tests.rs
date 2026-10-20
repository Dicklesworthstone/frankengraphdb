use super::*;
use crate::algebra::IntegerComparison;
use crate::{
    GqlParameters, GraphSetOperand, GraphSetPredicateOp, GraphSymbol, GraphSymbolKind,
    PreparedGraphText,
};
use fgdb_delta_types::PropertyKeyId;
fn leaf() -> PreparedGraphSet {
    PreparedGraphText::prepare("MATCH (n) RETURN n.p AS value", |kind, _: &str| {
        (kind == GraphSymbolKind::Property).then_some(GraphSymbol::Property(PropertyKeyId(1)))
    })
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
    .with_duplicates()
    .into()
}
#[test]
fn all_join_kinds_have_a_sealed_external_source_and_canonical_finish() {
    for kind in [
        RowJoinKind::Inner,
        RowJoinKind::Left,
        RowJoinKind::Right,
        RowJoinKind::Full,
        RowJoinKind::Semi,
        RowJoinKind::Anti,
    ] {
        for keyed in [false, true] {
            // `n.p` is a property column, which is Stored (fgdb-kr8j1).
            let types = [GraphSetColumnType::Stored];
            let spec = if keyed {
                RowJoinSpec::new(&types, &types, &[(0, 0)])
            } else {
                RowJoinSpec::cross(&types, &types)
            }
            .unwrap()
            .with_kind(kind)
            .with_predicate(&[GraphSetPredicateOp::Compare {
                left: GraphSetOperand::Column(0),
                comparison: IntegerComparison::LessOrEqual,
                right: GraphSetOperand::Column(1),
            }])
            .unwrap();
            let query = leaf()
                .with_page(1, Some(2))
                .join(leaf().with_page(0, Some(3)), spec)
                .unwrap();
            let plan = AsyncSpillSetPlan::compile(&query).unwrap();
            let AsyncSpillSetSourcePlan::RowJoin(join) = plan.source() else {
                panic!("row join source");
            };
            assert_eq!(join.kind(), kind);
            assert_eq!(join.left().stages().last().unwrap().offset(), 1);
            assert_eq!(join.right().stages().last().unwrap().count(), Some(3));
            assert!(plan.source_pattern().is_none());
            assert_eq!(plan.source_columns(), query.columns());
        }
    }
}
