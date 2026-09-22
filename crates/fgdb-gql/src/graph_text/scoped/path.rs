//! Path selectors and relationship repetition in the shared MATCH compiler.
//! GQL postfix intervals and Cypher-style in-bracket intervals become the same
//! checked bounds. Neither spelling changes the native WALK/TRAIL semantics,
//! guesses an upper bound, adds an evaluator, or bypasses execution governance.

use super::*;
use crate::GraphWalkBounds;

impl Parser<'_> {
    /// Select the existing physical search, never enumerate all paths and
    /// post-filter them. ALL is nonselective; ANY may deterministically choose
    /// a shortest admissible occurrence. SHORTEST 1 selects one, whereas
    /// SHORTEST [1] GROUP[S] and ALL SHORTEST retain all tied minima.
    /// Bare SHORTEST is a native shorthand for SHORTEST 1. PATH/PATHS are
    /// optional selector noise words; the native default mode remains WALK.
    /// Counts beyond one and shortest/repetition-mode combinations need new
    /// logical operators and refuse rather than silently weakening the query.
    pub(super) fn path_search(&mut self) -> Result<GraphWalkSearch, GraphPatternTextError> {
        let search = if self.take_word("ALL")? {
            if self.take_word("SHORTEST")? {
                self.path_selector_words()?;
                GraphWalkSearch::AllShortest
            } else {
                self.path_selector_words()?;
                return self.all_path_mode();
            }
        } else if self.take_word("ANY")? {
            if !self.take_word("SHORTEST")? {
                self.single_path_count()?;
            }
            self.path_selector_words()?;
            GraphWalkSearch::AnyShortest
        } else if self.take_word("SHORTEST")? {
            self.single_path_count()?;
            self.path_selector_words()?;
            if self.take_word("GROUP")? || self.take_word("GROUPS")? {
                GraphWalkSearch::AllShortest
            } else {
                GraphWalkSearch::AnyShortest
            }
        } else {
            return self.all_path_mode();
        };
        // Existing ALL/ANY SHORTEST WALK keeps its exact lowering. Do not
        // consume TRAIL, SIMPLE or ACYCLIC here: those are not shortest WALK.
        self.take_word("WALK")?;
        Ok(search)
    }

    fn path_selector_words(&mut self) -> Result<(), GraphPatternTextError> {
        if !self.take_word("PATH")? {
            self.take_word("PATHS")?;
        }
        Ok(())
    }

    fn single_path_count(&mut self) -> Result<(), GraphPatternTextError> {
        let at = self.current.at;
        match self.current.kind {
            TokenKind::Digits(digits) if matches!(digits.parse::<u64>(), Ok(1)) => {
                self.advance()?;
                Ok(())
            }
            TokenKind::Digits(_) | TokenKind::Parameter(_) => Err(error(
                at,
                GraphPatternTextErrorKind::Expected("supported literal path count (1)"),
            )),
            _ => Ok(()),
        }
    }

    fn all_path_mode(&mut self) -> Result<GraphWalkSearch, GraphPatternTextError> {
        if self.take_word("ACYCLIC")? {
            Ok(GraphWalkSearch::Acyclic)
        } else if self.take_word("SIMPLE")? {
            Ok(GraphWalkSearch::Simple)
        } else if self.take_word("TRAIL")? {
            Ok(GraphWalkSearch::Trail)
        } else {
            self.take_word("WALK")?;
            Ok(GraphWalkSearch::All)
        }
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
    use crate::{GqlQueryPolicy, PreparedGraphWriteScript};
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
        let mut rows: Vec<_> = pattern
            .plan()
            .execute_governed_with_properties(
                10,
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
            .collect();
        // Assert bag contents without inventing an implicit ORDER BY contract.
        rows.sort();
        rows
    }

    #[test]
    fn postfix_intervals_preserve_bounds_direction_and_selector() {
        for selector in [
            "",
            "WALK",
            "TRAIL",
            "ACYCLIC",
            "SIMPLE",
            "ALL SHORTEST WALK",
        ] {
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
                    let text =
                        format!("MATCH {selector} (a){left}[:R]{right}{quantifier}(b) RETURN a,b");
                    let syntax = parse(&text);
                    let edge = &syntax.edges[0];
                    assert_eq!(edge.direction, direction);
                    assert_eq!(
                        edge.walk,
                        Some(GraphWalkBounds::new(minimum, maximum).unwrap())
                    );
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
        assert!(
            PreparedGraphAggregateText::prepare(
                "MATCH (a)-[:R]->{1,3}(b) RETURN count(*) AS total",
                symbols,
            )
            .is_ok()
        );
        for terminal in ["SET b.n = 1", "DELETE b", "INSERT (c)"] {
            let text = format!("MATCH (a)-[:R]->{{1,3}}(b) {terminal}");
            let mut parser = Parser::new(&text).unwrap();
            parser.parse_match_prefix().unwrap();
            assert_eq!(
                parser.syntax.edges[0].walk,
                Some(GraphWalkBounds::new(1, 3).unwrap())
            );
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

    #[test]
    fn selectors_lower_to_their_exact_native_search() {
        for (selector, search) in [
            ("ALL", GraphWalkSearch::All),
            ("ALL PATHS", GraphWalkSearch::All),
            ("ALL PATH WALK", GraphWalkSearch::All),
            ("ALL TRAIL", GraphWalkSearch::Trail),
            ("ALL ACYCLIC", GraphWalkSearch::Acyclic),
            ("ALL SIMPLE", GraphWalkSearch::Simple),
            ("SHORTEST", GraphWalkSearch::AnyShortest),
            ("SHORTEST 1", GraphWalkSearch::AnyShortest),
            ("SHORTEST 1 PATH", GraphWalkSearch::AnyShortest),
            ("ANY", GraphWalkSearch::AnyShortest),
            ("ANY 1 PATHS", GraphWalkSearch::AnyShortest),
            ("ANY SHORTEST", GraphWalkSearch::AnyShortest),
            ("ANY SHORTEST WALK", GraphWalkSearch::AnyShortest),
            ("ALL SHORTEST", GraphWalkSearch::AllShortest),
            ("ALL SHORTEST PATHS", GraphWalkSearch::AllShortest),
            ("ALL SHORTEST WALK", GraphWalkSearch::AllShortest),
            ("SHORTEST GROUP", GraphWalkSearch::AllShortest),
            ("SHORTEST 1 GROUPS", GraphWalkSearch::AllShortest),
            ("SHORTEST 1 PATH GROUP", GraphWalkSearch::AllShortest),
            ("aLl sHoRtEsT pAtHs", GraphWalkSearch::AllShortest),
        ] {
            let text = format!("MATCH {selector} (a)-[:R]->{{1,3}}(b) RETURN a,b");
            assert_eq!(parse(&text).edges[0].search, search, "{selector}");
            let captured = format!("MATCH p = {selector} (a)-[:R]->{{1,3}}(b) RETURN p");
            let syntax = parse(&captured);
            assert_eq!(syntax.path.unwrap().text, "p");
            assert_eq!(syntax.edges[0].search, search);
        }
    }

    #[test]
    fn all_is_not_shortest_and_shortest_groups_keep_tied_occurrences() {
        let arguments = GqlParameters::new();
        let all = run("MATCH ALL (a {n:1})-[:R]->{1,2}(b) RETURN b", &arguments);
        assert_eq!(all.len(), 5);
        let shortest = run(
            "MATCH SHORTEST 1 (a {n:1})-[:R]->{1,2}(b) RETURN b",
            &arguments,
        );
        assert_eq!(
            shortest,
            vec![vec![Some(VId(2))], vec![Some(VId(3))], vec![Some(VId(4))]]
        );
        for selector in ["ALL SHORTEST", "SHORTEST GROUPS", "SHORTEST 1 GROUP"] {
            let text = format!("MATCH {selector} (a {{n:1}})-[:R]->{{2}}(b) RETURN b");
            assert_eq!(
                run(&text, &arguments),
                vec![vec![Some(VId(4))], vec![Some(VId(4))]]
            );
        }
        for selector in ["ANY", "ANY 1", "ANY SHORTEST", "SHORTEST", "SHORTEST 1"] {
            let text = format!("MATCH {selector} (a {{n:1}})-[:R]->{{2}}(b) RETURN b");
            assert_eq!(run(&text, &arguments), vec![vec![Some(VId(4))]]);
        }
    }

    #[test]
    fn nonselective_all_keeps_compound_patterns_and_required_scopes() {
        let arguments = GqlParameters::new();
        assert_eq!(
            run(
                "MATCH ALL (a {n:1})-[:R]->{1,2}(b)-[:R]->{1}(c) RETURN c",
                &arguments,
            ),
            vec![vec![Some(VId(4))], vec![Some(VId(4))]]
        );
        assert_eq!(
            run(
                "MATCH (a {n:1}) MATCH ANY SHORTEST (a)-[:R]->{2}(b) RETURN b",
                &arguments,
            ),
            vec![vec![Some(VId(4))]]
        );
        assert_eq!(
            run(
                "MATCH (a {n:5}) OPTIONAL MATCH SHORTEST 1 (a)-[:R]->{1,2}(b) RETURN a,b",
                &arguments,
            ),
            vec![vec![Some(VId(5)), None]]
        );
    }

    #[test]
    fn selectors_prepare_in_aggregate_and_native_write_programs() {
        for selector in ["ALL", "ALL SHORTEST", "SHORTEST 1", "ANY 1"] {
            let prefix = format!("MATCH {selector} (a)-[:R]->{{1,3}}(b)");
            let aggregate = format!("{prefix} RETURN count(*) AS total, sum(b.n) AS amount");
            assert!(PreparedGraphAggregateText::prepare(&aggregate, symbols).is_ok());
            for terminal in ["SET b.n = 1", "DELETE b", "INSERT (c)"] {
                let text = format!("{prefix} {terminal}");
                assert!(
                    PreparedGraphWriteScript::prepare_with_parameter_types(
                        &text,
                        RelationId(1),
                        &[],
                        symbols,
                    )
                    .is_ok(),
                    "{selector} {terminal}"
                );
            }
        }
    }

    #[test]
    fn unsupported_selectors_never_degrade_to_an_existing_but_different_search() {
        for selector in [
            "SHORTEST 0",
            "SHORTEST 2",
            "SHORTEST 2 GROUPS",
            "ANY 2",
            "ANY $count",
            "SHORTEST $count",
            "SHORTEST 18446744073709551616",
            "ALL SHORTEST TRAIL",
            "ANY SHORTEST SIMPLE",
            "SHORTEST 1 ACYCLIC",
            "ALL PATHS PATHS",
        ] {
            let text = format!("MATCH {selector} (a)-[:R]->{{1,3}}(b) RETURN b LIMIT 0");
            let mut resolutions = 0;
            assert!(
                PreparedGraphText::prepare(&text, |kind, name| {
                    resolutions += 1;
                    symbols(kind, name)
                })
                .is_err(),
                "{selector}"
            );
            assert_eq!(resolutions, 0);
        }
        // A selector applies to a whole pattern, not each atom independently.
        for selector in ["SHORTEST 1", "ALL SHORTEST", "ANY"] {
            let text = format!("MATCH {selector} (a)-[:R]->{{1,2}}(b)-[:R]->{{1,2}}(c) RETURN c");
            assert!(Parser::new(&text).unwrap().parse().is_err());
        }
    }

    #[test]
    fn equivalent_spellings_bind_to_identical_parameterized_property_plans() {
        let arguments = GqlParameters::new()
            .with_int64("key", 1)
            .unwrap()
            .with_uint64("skip", 0)
            .unwrap()
            .with_uint64("take", 10)
            .unwrap();
        for (selector, legacy) in [
            ("ALL PATHS", "WALK"),
            ("ANY 1", "ANY SHORTEST WALK"),
            ("SHORTEST 1", "ANY SHORTEST WALK"),
            ("ALL SHORTEST", "ALL SHORTEST WALK"),
            ("SHORTEST 1 GROUP", "ALL SHORTEST WALK"),
        ] {
            let tail = "WHERE a.n=$key RETURN b.n AS number,b SKIP $skip LIMIT $take";
            let original = format!("MATCH {legacy} (a)-[:R*0..3]->(b) {tail}");
            let expanded = format!("MATCH {selector} (a)-[:R]->{{0,3}}(b) {tail}");
            let before = PreparedGraphText::prepare(&original, symbols)
                .unwrap()
                .bind_parameters(&arguments)
                .unwrap();
            let after = PreparedGraphText::prepare(&expanded, symbols)
                .unwrap()
                .bind_parameters(&arguments)
                .unwrap();
            assert_eq!(before, after, "{selector}");
            assert_eq!(before.canonical_bytes(), after.canonical_bytes());
        }
    }

    #[test]
    fn expanded_syntax_keeps_work_result_and_cancellation_governance() {
        for selector in ["ALL", "ANY", "ALL SHORTEST", "SHORTEST 1"] {
            let text = format!("MATCH {selector} (a)-[:R]->{{1,3}}(b) RETURN b");
            let prepared = PreparedGraphText::prepare(&text, symbols)
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap();
            for policy in [
                GqlQueryPolicy::new(100, 100, 0, 100_000),
                GqlQueryPolicy::new(100, 0, 100_000, 100_000),
            ] {
                let result = prepared.plan().execute_governed_with_properties(
                    3,
                    [VId(1)],
                    [(VId(1), RelationId(1), VId(1)); 2],
                    |_, _| Ok::<_, ()>(true),
                    |_, _| Ok(None),
                    policy,
                    || Ok::<_, ()>(()),
                );
                assert!(result.is_err(), "{selector} ignored its execution budget");
            }
            let mut polls = 0;
            let cancelled = prepared.plan().execute_governed_with_properties(
                3,
                [VId(1)],
                [(VId(1), RelationId(1), VId(1)); 2],
                |_, _| Ok::<_, ()>(true),
                |_, _| Ok(None),
                GqlQueryPolicy::new(100, 100, 100_000, 100_000),
                || {
                    polls += 1;
                    Err::<(), ()>(())
                },
            );
            assert!(cancelled.is_err(), "{selector} ignored cancellation");
            assert!(polls > 0);
        }
    }

    #[test]
    fn expanded_syntax_preserves_original_utf8_parameter_and_bound_offsets() {
        let text = "\u{2003}MATCH SHORTEST 1 (a)-[:R]->{1,3}(b) \
                    WHERE a.n=$key RETURN b LIMIT $count";
        let prepared = PreparedGraphText::prepare(text, symbols).unwrap();
        let missing = prepared.bind_parameters(&GqlParameters::new()).unwrap_err();
        assert_eq!(missing.offset, text.find('$').unwrap());
        assert_eq!(missing.kind, GraphPatternTextErrorKind::MissingParameter);
        let reversed = "\u{2003}MATCH ALL (a)-[:R]->{3,1}(b) RETURN b LIMIT 0";
        let failure = PreparedGraphText::prepare(reversed, symbols).unwrap_err();
        assert_eq!(failure.offset, reversed.find('{').unwrap());
        for at in (0..=text.len()).filter(|at| text.is_char_boundary(*at)) {
            let _ = PreparedGraphText::prepare(&text[..at], symbols);
        }
    }
}
