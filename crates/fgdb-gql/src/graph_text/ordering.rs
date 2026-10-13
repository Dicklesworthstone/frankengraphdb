//! Resolve returned expressions and aliases before catalog access. Ordering
//! stores evaluation-column positions; hidden sort cells introduce no aliases.

use super::*;

impl Parser<'_> {
    pub(super) fn parse_row_ordering(&mut self) -> Result<(), GraphPatternTextError> {
        if !self.take_word("ORDER")? {
            return Ok(());
        }
        self.word("BY")?;
        // ORDER BY may append private evaluation cells as it parses. Freeze
        // the returned alias scope before doing so: a synthetic property or
        // function name must never shadow a later bound graph variable.
        let visible_columns = self
            .syntax
            .visible_columns
            .unwrap_or(self.syntax.columns.len());
        loop {
            let name = self.name()?;
            let function = if self.take(b'(')? {
                let function = Self::path_function(name)?;
                let variable = match function {
                    GraphPathFunction::Labels => self.vertex_variable()?,
                    GraphPathFunction::Type => self.edge_variable()?,
                    GraphPathFunction::Properties => self.property_variable()?,
                    _ => self.path_variable()?,
                };
                self.punct(b')', ")")?;
                Some((variable, function))
            } else {
                None
            };
            let property = if function.is_none() && self.take(b'.')? {
                Some(self.name()?)
            } else {
                None
            };
            // `RETURN n AS m ORDER BY m.p`: a property of an alias that
            // projects an element bare is that element's property. A name
            // that is itself a binding keeps meaning the binding.
            let name = match property {
                Some(_)
                    if function.is_none()
                        && self.syntax.bare_path_function(name.text).is_none()
                        && !self
                            .syntax
                            .variables
                            .iter()
                            .any(|variable| variable.text == name.text) =>
                {
                    self.syntax
                        .columns
                        .iter()
                        .find(|column| {
                            column.alias.text == name.text
                                && column.property.is_none()
                                && column.collect.is_none()
                                && matches!(column.path, None | Some(GraphPathFunction::Edge))
                        })
                        .map_or(name, |column| column.variable)
                }
                _ => name,
            };
            let resolved = if let Some((variable, function)) = function {
                self.syntax.columns.iter().position(|column| {
                    column.variable.text == variable.text && column.path == Some(function)
                })
            } else if let Some(property) = property {
                self.syntax.columns.iter().position(|column| {
                    column.variable.text == name.text
                        && column.property.is_some_and(|key| key.text == property.text)
                })
            } else {
                // A returned alias wins over a same-spelled original variable.
                self.syntax
                    .columns
                    .iter()
                    .take(visible_columns)
                    .position(|column| column.alias.text == name.text)
                    .or_else(|| {
                        self.syntax.columns.iter().position(|column| {
                            column.property.is_none()
                                && column.variable.text == name.text
                                && matches!(column.path, None | Some(GraphPathFunction::Value))
                        })
                    })
            };
            // A property of an element RETURN projects bare is a function of
            // that element: rows equal on the element are equal on it, so its
            // hidden sort cell cannot change which rows DISTINCT keeps
            // (openCypher `RETURN DISTINCT a ORDER BY a.name`).
            let dependent = property.is_some()
                && function.is_none()
                && self.syntax.columns.iter().any(|column| {
                    column.property.is_none()
                        && column.variable.text == name.text
                        && matches!(column.path, None | Some(GraphPathFunction::Edge))
                });
            let column = if let Some(column) = resolved {
                column
            } else if self.syntax.distinct && !dependent {
                // DISTINCT deduplicates whole evaluation rows; any other
                // hidden sort cell would change which occurrences survive.
                // Refuse typed instead of silently picking survivors.
                return Err(error(
                    name.at,
                    GraphPatternTextErrorKind::Expected(
                        "projected ORDER BY expression or alias under DISTINCT",
                    ),
                ));
            } else if let Some((variable, function)) = function {
                self.capacity(
                    self.syntax.columns.len(),
                    MAX_PATTERN_VERTICES,
                    crate::algebra::PatternLimitDimension::Columns,
                )?;
                let width = self.syntax.columns.len();
                self.syntax.columns.push(Column {
                    alias: name,
                    variable,
                    property: None,
                    path: Some(function),
                    collect: None,
                });
                if self.syntax.visible_columns.is_none() {
                    self.syntax.visible_columns = Some(width);
                }
                width
            } else if let Some(property) = property {
                // Hidden sort key: evaluate the property for ranking without
                // projecting it publicly. The property symbol is resolved once
                // in from_syntax like every returned column; only the position
                // of this appended evaluation cell is referenced afterwards.
                self.capacity(
                    self.syntax.columns.len(),
                    MAX_PATTERN_VERTICES,
                    crate::algebra::PatternLimitDimension::Columns,
                )?;
                let width = self.syntax.columns.len();
                self.syntax.columns.push(Column {
                    alias: property,
                    variable: name,
                    property: Some(property),
                    path: None,
                    collect: None,
                });
                // Multiple hidden keys must all sit in the hidden tail; the
                // visible prefix is the width BEFORE the first appended cell.
                if self.syntax.visible_columns.is_none() {
                    self.syntax.visible_columns = Some(width);
                }
                width
            } else {
                // Hidden sort key over a bound variable RETURN did not project
                // (`RETURN n.p AS p ORDER BY n`). The cell holds the value
                // `RETURN n` would: the path, the edge or the vertex.
                let path = self.syntax.bare_path_function(name.text);
                if path.is_none()
                    && !self
                        .syntax
                        .variables
                        .iter()
                        .any(|variable| variable.text == name.text)
                {
                    return Err(error(
                        name.at,
                        GraphPatternTextErrorKind::Expected(
                            "projected ORDER BY expression or alias",
                        ),
                    ));
                }
                self.capacity(
                    self.syntax.columns.len(),
                    MAX_PATTERN_VERTICES,
                    crate::algebra::PatternLimitDimension::Columns,
                )?;
                let width = self.syntax.columns.len();
                self.syntax.columns.push(Column {
                    alias: name,
                    variable: name,
                    property: None,
                    path,
                    collect: None,
                });
                if self.syntax.visible_columns.is_none() {
                    self.syntax.visible_columns = Some(width);
                }
                width
            };
            if self
                .syntax
                .ordering
                .iter()
                .any(|order| order.column == column)
            {
                return Err(error(
                    name.at,
                    GraphPatternTextErrorKind::OrderBuild(
                        crate::algebra::GraphOrderError::DuplicateColumn { column },
                    ),
                ));
            }
            // openCypher and GQL spell the directions ASC/ASCENDING and
            // DESC/DESCENDING.
            let descending = self.take_word("DESC")? || self.take_word("DESCENDING")?;
            if !descending && !self.take_word("ASC")? {
                self.take_word("ASCENDING")?;
            }
            let nulls_first = if self.take_word("NULLS")? {
                if self.take_word("FIRST")? {
                    true
                } else {
                    self.word("LAST")?;
                    false
                }
            } else {
                false
            };
            // Unique indices into the already bounded RETURN schema bound
            // this list without a second, differently sized definition cap.
            self.syntax.ordering.push(GraphValueOrder {
                column,
                descending,
                nulls_first,
            });
            if !self.take(b',')? {
                break;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod hidden_alias_tests {
    use super::*;

    fn name(text: &'static str) -> Name<'static> {
        Name { text, at: 0 }
    }

    // Exercise the real ORDER BY parser with a checked RETURN n boundary.
    // No catalog or evaluator is involved in deciding the alias scope.
    fn parser(text: &str) -> Parser<'_> {
        let mut parser = Parser::new(text).expect("valid ordering tokens");
        parser.syntax.variables.push(name("n"));
        parser.syntax.columns.push(Column {
            variable: name("n"),
            property: None,
            path: None,
            alias: name("n"),
        });
        parser
    }

    #[test]
    fn hidden_sort_names_do_not_shadow_bound_vertices() {
        for (text, variable) in [
            ("ORDER BY n.rank, rank DESC NULLS FIRST", "rank"),
            ("ORDER BY labels(n), labels DESC NULLS FIRST", "labels"),
        ] {
            let mut parser = parser(text);
            parser.syntax.variables.push(name(variable));
            parser.parse_row_ordering().unwrap();
            assert_eq!(parser.syntax.visible_columns, Some(1));
            assert_eq!(parser.syntax.columns.len(), 3);
            assert_eq!(
                parser.syntax.ordering,
                vec![
                    GraphValueOrder {
                        column: 1,
                        descending: false,
                        nulls_first: false,
                    },
                    GraphValueOrder {
                        column: 2,
                        descending: true,
                        nulls_first: true,
                    },
                ]
            );
            let vertex = &parser.syntax.columns[2];
            assert_eq!(vertex.variable.text, variable);
            assert!(vertex.property.is_none());
            assert!(vertex.path.is_none());
        }
    }

    #[test]
    fn hidden_sort_names_do_not_become_returned_aliases() {
        for (text, alias) in [
            ("ORDER BY n.rank, rank", "rank"),
            ("ORDER BY labels(n), labels", "labels"),
        ] {
            let mut parser = parser(text);
            let error = parser.parse_row_ordering().unwrap_err();
            assert_eq!(error.offset, text.rfind(alias).unwrap());
            assert_eq!(
                error.kind,
                GraphPatternTextErrorKind::Expected("projected ORDER BY expression or alias")
            );
        }
    }

    #[test]
    fn returned_alias_still_wins_after_a_hidden_sort_cell() {
        let mut parser = parser("ORDER BY n.rank, rank DESC");
        parser.syntax.variables.push(name("rank"));
        parser.syntax.columns.push(Column {
            variable: name("n"),
            property: Some(name("p")),
            path: None,
            alias: name("rank"),
        });
        parser.parse_row_ordering().unwrap();
        assert_eq!(parser.syntax.visible_columns, Some(2));
        assert_eq!(parser.syntax.columns.len(), 3);
        assert_eq!(parser.syntax.ordering[0].column, 2);
        assert_eq!(parser.syntax.ordering[1].column, 1);
        assert!(parser.syntax.ordering[1].descending);
    }

    #[test]
    fn an_existing_hidden_tail_does_not_widen_the_alias_scope() {
        let text = "ORDER BY rank";
        let mut parser = parser(text);
        parser.syntax.visible_columns = Some(1);
        parser.syntax.columns.push(Column {
            variable: name("n"),
            property: Some(name("rank")),
            path: None,
            alias: name("rank"),
        });
        let error = parser.parse_row_ordering().unwrap_err();
        assert_eq!(error.offset, text.rfind("rank").unwrap());
        assert_eq!(
            error.kind,
            GraphPatternTextErrorKind::Expected("projected ORDER BY expression or alias")
        );
        assert!(parser.syntax.ordering.is_empty());
    }

    #[test]
    fn repeated_hidden_expressions_still_reuse_the_same_column() {
        for text in ["ORDER BY n.rank, n.rank", "ORDER BY labels(n), labels(n)"] {
            let mut parser = parser(text);
            let error = parser.parse_row_ordering().unwrap_err();
            assert_eq!(
                error.kind,
                GraphPatternTextErrorKind::OrderBuild(
                    crate::algebra::GraphOrderError::DuplicateColumn { column: 1 }
                )
            );
            assert_eq!(parser.syntax.columns.len(), 2);
        }
    }

    #[test]
    fn distinct_does_not_export_dependent_hidden_property_aliases() {
        let text = "ORDER BY n.rank, rank";
        let mut parser = parser(text);
        parser.syntax.distinct = true;
        let error = parser.parse_row_ordering().unwrap_err();
        assert_eq!(error.offset, text.rfind("rank").unwrap());
        assert_eq!(
            error.kind,
            GraphPatternTextErrorKind::Expected(
                "projected ORDER BY expression or alias under DISTINCT"
            )
        );
        // RETURN DISTINCT n may still order on its dependent n.rank property.
        assert_eq!(parser.syntax.visible_columns, Some(1));
        assert_eq!(parser.syntax.ordering.len(), 1);
        assert_eq!(parser.syntax.ordering[0].column, 1);
    }
}
