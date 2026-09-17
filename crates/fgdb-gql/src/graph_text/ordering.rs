//! Resolve returned expressions and aliases before catalog access. Ordering
//! stores output positions, never execution-time names or hidden sort keys.

use super::*;

impl Parser<'_> {
    pub(super) fn parse_row_ordering(&mut self) -> Result<(), GraphPatternTextError> {
        if !self.take_word("ORDER")? {
            return Ok(());
        }
        self.word("BY")?;
        loop {
            let name = self.name()?;
            let function = if self.take(b'(')? {
                let function = Self::path_function(name)?;
                let variable = self.path_variable()?;
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
                    .position(|column| column.alias.text == name.text)
                    .or_else(|| {
                        self.syntax.columns.iter().position(|column| {
                            column.property.is_none()
                                && column.variable.text == name.text
                                && matches!(column.path, None | Some(GraphPathFunction::Value))
                        })
                    })
            };
            let column = if let Some(column) = resolved {
                column
            } else if property.is_some() && self.syntax.distinct {
                // DISTINCT deduplicates whole evaluation rows; a hidden sort
                // cell would change which occurrences survive. Refuse typed
                // instead of silently picking survivors.
                return Err(error(
                    name.at,
                    GraphPatternTextErrorKind::Expected(
                        "projected ORDER BY expression or alias under DISTINCT",
                    ),
                ));
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
                });
                // Multiple hidden keys must all sit in the hidden tail; the
                // visible prefix is the width BEFORE the first appended cell.
                if self.syntax.visible_columns.is_none() {
                    self.syntax.visible_columns = Some(width);
                }
                width
            } else {
                return Err(error(
                    name.at,
                    GraphPatternTextErrorKind::Expected("projected ORDER BY expression or alias"),
                ));
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
            let descending = self.take_word("DESC")?;
            if !descending {
                self.take_word("ASC")?;
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
