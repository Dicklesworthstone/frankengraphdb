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
            let property = if self.take(b'.')? {
                Some(self.name()?)
            } else {
                None
            };
            let column = if let Some(property) = property {
                self.syntax.columns.iter().position(|column| {
                    column.variable.text == name.text
                        && column.property.is_some_and(|key| key.text == property.text)
                })
            } else {
                // A returned alias wins over a same-spelled original variable.
                self.syntax.columns.iter().position(|column| column.alias.text == name.text)
                    .or_else(|| self.syntax.columns.iter().position(|column| {
                        column.property.is_none() && column.variable.text == name.text
                    }))
            }
            .ok_or_else(|| error(name.at, GraphPatternTextErrorKind::Expected(
                "projected ORDER BY expression or alias",
            )))?;
            if self.syntax.ordering.iter().any(|order| order.column == column) {
                return Err(error(name.at, GraphPatternTextErrorKind::OrderBuild(
                    crate::algebra::GraphOrderError::DuplicateColumn { column },
                )));
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
            self.syntax.ordering.push(GraphValueOrder { column, descending, nulls_first });
            if !self.take(b',')? {
                break;
            }
        }
        Ok(())
    }
}
