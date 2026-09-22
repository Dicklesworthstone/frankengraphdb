//! Path selectors and relationship repetition in the shared MATCH compiler.
//! GQL postfix intervals and Cypher-style in-bracket intervals become the same
//! checked bounds. Neither spelling changes the native WALK/TRAIL semantics,
//! guesses an upper bound, adds an evaluator, or bypasses execution governance.

use super::*;
use crate::GraphWalkBounds;

impl Parser<'_> {
    pub(super) fn path_search(&mut self) -> Result<GraphWalkSearch, GraphPatternTextError> {
        let search = if self.take_word("ALL")? {
            GraphWalkSearch::AllShortest
        } else if self.take_word("ANY")? {
            GraphWalkSearch::AnyShortest
        } else if self.take_word("ACYCLIC")? {
            return Ok(GraphWalkSearch::Acyclic);
        } else if self.take_word("SIMPLE")? {
            return Ok(GraphWalkSearch::Simple);
        } else if self.take_word("TRAIL")? {
            return Ok(GraphWalkSearch::Trail);
        } else {
            self.take_word("WALK")?;
            return Ok(GraphWalkSearch::All);
        };
        self.word("SHORTEST")?;
        self.word("WALK")?;
        Ok(search)
    }

    /// Postfix `{m,n}`, `{n}` and `{,n}` follow the complete relationship,
    /// including its direction. GQL's omitted lower bound is zero, unlike
    /// Cypher's `*..n` lower bound of one. An upper bound is always required.
    /// A second quantifier refuses even when its interval happens to agree.
    pub(super) fn relationship_walk_bounds(
        &mut self,
        inner: Option<GraphWalkBounds>,
    ) -> Result<Option<GraphWalkBounds>, GraphPatternTextError> {
        let at = self.current.at;
        if !self.is_punct(b'{') {
            return Ok(inner);
        }
        if inner.is_some() {
            return Err(error(
                at,
                GraphPatternTextErrorKind::Expected("one relationship quantifier"),
            ));
        }
        self.advance()?;
        let (minimum, maximum) = if self.take(b',')? {
            (0, self.walk_hop_literal()?)
        } else {
            let minimum = self.walk_hop_literal()?;
            let maximum = if self.take(b',')? {
                self.walk_hop_literal()?
            } else {
                minimum
            };
            (minimum, maximum)
        };
        self.punct(b'}', "}")?;
        GraphWalkBounds::new(minimum, maximum)
            .map(Some)
            .map_err(|_| {
                error(
                    at,
                    GraphPatternTextErrorKind::Expected(
                        "finite ordered WALK bounds within the hop limit",
                    ),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GqlQueryPolicy;
    use fgdb_types::{CanonicalScalar, VId};

    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
            (GraphSymbolKind::Property, "n") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            _ => None,
        }
    }

    fn parse(text: &str) -> Syntax<'_> {
        Parser::new(text).unwrap().parse().unwrap()
    }

    fn run(text: &str, arguments: &GqlParameters) -> Vec<Vec<Option<VId>>> {
        // A diamond with a direct edge: three occurrences reach vertex 4,
        // but only the direct edge is shortest. Vertex 5 is an isolate.
        let numbers: BTreeMap<_, _> = (1..=5)
            .map(|id| (VId(id), CanonicalScalar::Int(id as i64)))
            .collect();
        let pattern = PreparedGraphText::prepare(text, symbols)
            .unwrap()
            .bind_parameters(arguments)
            .unwrap();
        pattern
            .plan()
            .execute_governed_with_properties(
                5,
                (1..=5).map(VId),
                [
                    (VId(1), RelationId(1), VId(2)),
                    (VId(1), RelationId(1), VId(3)),
                    (VId(2), RelationId(1), VId(4)),
                    (VId(3), RelationId(1), VId(4)),
                    (VId(1), RelationId(1), VId(4)),
                ],
                |vid, predicates| {
                    let properties: Vec<_> = numbers
                        .get(&vid)
                        .map(|value| (PropertyKeyId(1), value.clone()))
                        .into_iter()
                        .collect();
                    Ok::<_, ()>(predicates.iter().all(|p| p.matches(&[], &properties)))
                },
                |vid, _| Ok(numbers.get(&vid)),
                GqlQueryPolicy::new(100, 100, 100_000, 100_000),
                || Ok::<_, ()>(()),
            )
            .unwrap()
            .value
            .iter()
            .map(|row| row.values().iter().map(|value| value.as_vertex()).collect())
            .collect()
    }

    #[test]
    fn postfix_intervals_preserve_bounds_direction_and_selector() {
        for selector in ["", "WALK", "TRAIL", "ACYCLIC", "SIMPLE", "ALL SHORTEST WALK"] {
            for (left, right, direction) in [
                ("-", "->", GlaDirection::Forward),
                ("<-", "-", GlaDirection::Reverse),
                ("-", "-", GlaDirection::Undirected),
            ] {
                for (quantifier, minimum, maximum) in [
                    ("{1,3}", 1, 3),
                    ("{2}", 2, 2),
                    ("{,3}", 0, 3),
                    ("{0}", 0, 0),
                    ("{0,1024}", 0, 1024),
                ] {
                    let text = format!(
                        "MATCH {selector} (a){left}[:R]{right}{quantifier}(b) RETURN a,b"
                    );
                    let syntax = parse(&text);
                    let edge = &syntax.edges[0];
                    assert_eq!(edge.direction, direction);
                    assert_eq!(edge.walk, Some(GraphWalkBounds::new(minimum, maximum).unwrap()));
                    let legacy = format!(
                        "MATCH {selector} (a){left}[:R*{minimum}..{maximum}]{right}(b) RETURN a,b"
                    );
                    assert_eq!(edge.search, parse(&legacy).edges[0].search);
                }
            }
        }
    }

    #[test]
    fn postfix_execution_retains_bags_zero_hops_and_parameter_predicates() {
        let arguments = GqlParameters::new().with_int64("start", 1).unwrap();
        let postfix = "MATCH (a {n:$start})-[:R]->{1,2}(b) RETURN b";
        let legacy = "MATCH (a {n:$start})-[:R*1..2]->(b) RETURN b";
        let expected = vec![
            vec![Some(VId(2))],
            vec![Some(VId(3))],
            vec![Some(VId(4))],
            vec![Some(VId(4))],
            vec![Some(VId(4))],
        ];
        assert_eq!(run(postfix, &arguments), expected);
        assert_eq!(run(postfix, &arguments), run(legacy, &arguments));
        let arguments = GqlParameters::new().with_int64("start", 5).unwrap();
        assert_eq!(
            run("MATCH (a {n:$start})-[:R]->{,2}(b) RETURN b", &arguments),
            vec![vec![Some(VId(5))]]
        );
        assert!(run("MATCH (a {n:$start})-[:R*..2]->(b) RETURN b", &arguments).is_empty());
    }

    #[test]
    fn scoped_quantifiers_keep_optional_and_existential_ownership() {
        let arguments = GqlParameters::new();
        assert_eq!(
            run(
                "MATCH (a {n:5}) OPTIONAL MATCH (a)-[:R]->{1,2}(b) RETURN a,b",
                &arguments,
            ),
            vec![vec![Some(VId(5)), None]]
        );
        assert_eq!(
            run(
                "MATCH (a) WHERE EXISTS { MATCH (a)-[:R]->{2}(b) } RETURN a",
                &arguments,
            ),
            vec![vec![Some(VId(1))]]
        );
        assert_eq!(
            run(
                "MATCH (a {n:1}) MATCH (a)-[:R]->{2}(b) RETURN b",
                &arguments,
            ),
            vec![vec![Some(VId(4))], vec![Some(VId(4))]]
        );
    }

    #[test]
    fn aggregate_and_write_heads_use_the_same_quantifier_parser() {
        assert!(PreparedGraphAggregateText::prepare(
            "MATCH (a)-[:R]->{1,3}(b) RETURN count(*) AS total",
            symbols,
        )
        .is_ok());
        for terminal in ["SET b.n = 1", "DELETE b", "INSERT (c)"] {
            let text = format!("MATCH (a)-[:R]->{{1,3}}(b) {terminal}");
            let mut parser = Parser::new(&text).unwrap();
            parser.parse_match_prefix().unwrap();
            assert_eq!(parser.syntax.edges[0].walk, Some(GraphWalkBounds::new(1, 3).unwrap()));
            assert_eq!(&text[parser.current.at..], terminal);
        }
    }

    #[test]
    fn invalid_quantifiers_refuse_before_name_resolution_even_with_limit_zero() {
        for relationship in [
            "-[:R]->{}",
            "-[:R]->{,}",
            "-[:R]->{1,}",
            "-[:R]->{3,2}",
            "-[:R]->{1,1025}",
            "-[:R]->{4294967296}",
            "-[:R]->{-1,2}",
            "-[:R]->{$min,2}",
            "-[:R]->{1,$max}",
            "-[:R]->{1,2,3}",
            "-[:R]->{1,2",
            "-[:R*1..2]->{1,2}",
            "-[:R]->{1,2}{1,2}",
            "<-[:R]->{1,2}",
        ] {
            let text = format!("MATCH (a){relationship}(b) RETURN b LIMIT 0");
            let mut resolutions = 0;
            let result = PreparedGraphText::prepare(&text, |kind, name| {
                resolutions += 1;
                symbols(kind, name)
            });
            assert!(result.is_err(), "accepted {relationship}");
            assert_eq!(resolutions, 0, "resolved names for {relationship}");
        }
    }
}
