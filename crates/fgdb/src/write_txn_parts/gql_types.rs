type OverlayVertexSet = std::collections::BTreeSet<VId>;
type OverlayEdgeMap =
    std::collections::BTreeMap<EId, (VId, RelationId, VId)>;

struct OverlayGraph {
    observed: std::collections::BTreeSet<ElementId>,
    vertices: OverlayVertexSet,
    edges: OverlayEdgeMap,
}

#[derive(Clone, Copy)]
enum IntegerComparison {
    Equal,
    NotEqual,
    Greater,
    Less,
    GreaterOrEqual,
    LessOrEqual,
}

impl IntegerComparison {
    fn accepts(self, actual: i64, expected: i64) -> bool {
        match self {
            Self::Equal => actual == expected,
            Self::NotEqual => actual != expected,
            Self::Greater => actual > expected,
            Self::Less => actual < expected,
            Self::GreaterOrEqual => actual >= expected,
            Self::LessOrEqual => actual <= expected,
        }
    }
}

struct PropertyPredicateSets {
    equal: Option<OverlayVertexSet>,
    not_equal: Option<OverlayVertexSet>,
    greater: Option<OverlayVertexSet>,
    less: Option<OverlayVertexSet>,
    greater_or_equal: Option<OverlayVertexSet>,
    less_or_equal: Option<OverlayVertexSet>,
}

impl PropertyPredicateSets {
    fn build<V: Vfs + Clone>(
        transaction: &WriteTxn,
        database: &Database<V>,
        vertices: &OverlayVertexSet,
        predicates: [Option<(fgdb_delta_types::PropertyKeyId, i64)>; 6],
    ) -> Result<Self, WriteTxnError> {
        let [equal, not_equal, greater, less, greater_or_equal, less_or_equal] = predicates;
        Ok(Self {
            equal: Self::holders(
                transaction,
                database,
                vertices,
                equal,
                IntegerComparison::Equal,
            )?,
            not_equal: Self::holders(
                transaction,
                database,
                vertices,
                not_equal,
                IntegerComparison::NotEqual,
            )?,
            greater: Self::holders(
                transaction,
                database,
                vertices,
                greater,
                IntegerComparison::Greater,
            )?,
            less: Self::holders(
                transaction,
                database,
                vertices,
                less,
                IntegerComparison::Less,
            )?,
            greater_or_equal: Self::holders(
                transaction,
                database,
                vertices,
                greater_or_equal,
                IntegerComparison::GreaterOrEqual,
            )?,
            less_or_equal: Self::holders(
                transaction,
                database,
                vertices,
                less_or_equal,
                IntegerComparison::LessOrEqual,
            )?,
        })
    }

    fn holders<V: Vfs + Clone>(
        transaction: &WriteTxn,
        database: &Database<V>,
        vertices: &OverlayVertexSet,
        predicate: Option<(fgdb_delta_types::PropertyKeyId, i64)>,
        comparison: IntegerComparison,
    ) -> Result<Option<OverlayVertexSet>, WriteTxnError> {
        let Some((key, expected)) = predicate else {
            return Ok(None);
        };
        let mut holders = OverlayVertexSet::new();
        for vid in vertices.iter().copied() {
            if transaction.vertex(database, vid)?.is_some_and(|row| {
                row.props.iter().any(|(property, scalar)| {
                    *property == key
                        && matches!(
                            scalar,
                            CanonicalScalar::Int(actual)
                                if comparison.accepts(*actual, expected)
                        )
                })
            }) {
                holders.insert(vid);
            }
        }
        Ok(Some(holders))
    }

    fn keeps(&self, vid: &VId) -> bool {
        self.equal
            .as_ref()
            .is_none_or(|holders| holders.contains(vid))
            && self
                .not_equal
                .as_ref()
                .is_none_or(|holders| holders.contains(vid))
            && self
                .greater
                .as_ref()
                .is_none_or(|holders| holders.contains(vid))
            && self
                .less
                .as_ref()
                .is_none_or(|holders| holders.contains(vid))
            && self
                .greater_or_equal
                .as_ref()
                .is_none_or(|holders| holders.contains(vid))
            && self
                .less_or_equal
                .as_ref()
                .is_none_or(|holders| holders.contains(vid))
    }
}

fn row_matches_property_predicates(
    row: &VertexRow,
    predicates: [Option<(fgdb_delta_types::PropertyKeyId, i64)>; 6],
) -> bool {
    let comparisons = [
        IntegerComparison::Equal,
        IntegerComparison::NotEqual,
        IntegerComparison::Greater,
        IntegerComparison::Less,
        IntegerComparison::GreaterOrEqual,
        IntegerComparison::LessOrEqual,
    ];
    predicates
        .into_iter()
        .zip(comparisons)
        .all(|(predicate, comparison)| {
            predicate.is_none_or(|(key, expected)| {
                row.props.iter().any(|(property, scalar)| {
                    *property == key
                        && matches!(
                            scalar,
                            CanonicalScalar::Int(actual)
                                if comparison.accepts(*actual, expected)
                        )
                })
            })
        })
}
