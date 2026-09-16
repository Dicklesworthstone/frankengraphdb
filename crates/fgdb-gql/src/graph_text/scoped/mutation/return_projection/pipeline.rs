//! WITH is preparation-only relational staging over the SAME native lexer.
//! Runtime receives only Project/Filter/page nodes in PreparedGraphSet. There
//! are no graph-slot aliases, synthetic MATCH text, or source calls here.

use super::*;
use crate::algebra::{GraphValueOrder, MAX_BOOLEAN_INSTRUCTIONS};
use crate::set_text::{ReadFilterOp, ReadFilterOperand, ReadPageNumber};
use crate::{GraphSetFilterError, GraphSetOperand, GraphSetPredicateOp, GraphSetQuantifier};

const MAX_FILTER_NESTING: usize = 64;
pub(super) type RowSchema<'a> = Vec<(Name<'a>, GraphSetColumnType)>;

fn expected(at: usize, item: &'static str) -> GraphSetTextError {
    GraphSetTextError { offset: at, kind: GraphSetTextErrorKind::Expected(item) }
}
fn append_stage(stages: &mut Vec<ReadStageTemplate>, stage: ReadStageTemplate,
    depth: &mut usize) -> Result<(), GraphSetTextError> {
    let (at, growth) = match &stage {
        ReadStageTemplate::Project { at, .. } | ReadStageTemplate::Filter { at, .. } => (*at, 1),
        ReadStageTemplate::Page { at, .. } => (*at, 0),
    };
    *depth += growth;
    if *depth > crate::MAX_GRAPH_SET_DEPTH {
        return Err(GraphSetTextError { offset: at, kind: GraphSetTextErrorKind::SetBuild(
            crate::GraphSetBuildError::TooDeep { limit: crate::MAX_GRAPH_SET_DEPTH, observed: *depth }) });
    }
    stages.push(stage);
    Ok(())
}
fn emit(code: &mut Vec<ReadFilterOp>, op: ReadFilterOp, at: usize) -> Result<(), GraphSetTextError> {
    if code.len() == MAX_BOOLEAN_INSTRUCTIONS {
        return Err(GraphSetTextError { offset: at, kind: GraphSetTextErrorKind::FilterBuild(
            GraphSetFilterError::TooManyInstructions { limit: MAX_BOOLEAN_INSTRUCTIONS, observed: code.len() + 1 }) });
    }
    code.push(op);
    Ok(())
}

impl<'a> Parser<'a> {
    pub(super) fn row_pipeline(&mut self, schema: RowSchema<'a>)
        -> Result<Vec<ReadStageTemplate>, GraphSetTextError> {
        let (mut stages, schema, mut depth) = self.row_pipeline_prefix(schema)?;
        let at = self.current.at;
        self.word("RETURN")?;
        let distinct = self.take_word("DISTINCT")?;
        if !distinct { self.take_word("ALL")?; }
        let quantifier = if distinct { GraphSetQuantifier::Distinct } else { GraphSetQuantifier::All };
        let (projection, _) = self.row_projection(&schema)?;
        append_stage(&mut stages, ReadStageTemplate::Project { at, projection, quantifier }, &mut depth)?;
        Ok(stages)
    }

    /// Parse complete WITH stages but leave their terminal RETURN to its typed
    /// owner. Ordinary value projection and exact aggregation share this path;
    /// neither has to rewrite or reparse a prefix as a different statement.
    pub(super) fn row_pipeline_prefix(&mut self, mut schema: RowSchema<'a>)
        -> Result<(Vec<ReadStageTemplate>, RowSchema<'a>, usize), GraphSetTextError> {
        let mut stages = Vec::new();
        // The MATCH input and first WITH projection each own one relational node.
        let mut depth = 2;
        loop {
            if let Some(page) = self.row_page(&schema)? {
                append_stage(&mut stages, page, &mut depth)?;
            }
            let at = self.current.at;
            if self.take_word("WHERE")? {
                let mut code = Vec::new();
                self.row_disjunction(&schema, 0, &mut code)?;
                let types = schema.iter().map(|(_, kind)| *kind).collect::<Vec<_>>();
                let shape = bind_filter(&code, None)?;
                GraphSetPredicateOp::validate_schema(&types, &shape).map_err(|kind| GraphSetTextError {
                    offset: at, kind: GraphSetTextErrorKind::FilterBuild(kind),
                })?;
                append_stage(&mut stages, ReadStageTemplate::Filter { at, code }, &mut depth)?;
            }
            let at = self.current.at;
            if !self.take_word("WITH")? { break; }
            let distinct = self.take_word("DISTINCT")?;
            if !distinct { self.take_word("ALL")?; }
            let quantifier = if distinct { GraphSetQuantifier::Distinct } else { GraphSetQuantifier::All };
            let (projection, next_schema) = self.row_projection(&schema)?;
            append_stage(&mut stages, ReadStageTemplate::Project { at, projection, quantifier }, &mut depth)?;
            schema = next_schema;
        }
        Ok((stages, schema, depth))
    }

    fn row_projection(&mut self, schema: &[(Name<'a>, GraphSetColumnType)])
        -> Result<(Vec<ReadProjectionTemplate>, RowSchema<'a>), GraphSetTextError> {
        let mut projection = Vec::new();
        let mut next_schema: RowSchema<'a> = Vec::new();
        if self.take(b'*')? {
            for (index, &(name, kind)) in schema.iter().enumerate() {
                projection.push(ReadProjectionTemplate { name: name.text.to_owned(), value: ReadValueTemplate::Column(index) });
                next_schema.push((name, kind));
            }
        } else {
            loop {
                self.capacity(projection.len(), MAX_PATTERN_VERTICES, crate::algebra::PatternLimitDimension::Columns)?;
                let at = self.current.at;
                let operand = self.row_expression(schema).map_err(expression_error)?;
                let alias = if self.take_word("AS")? {
                    self.name()?
                } else if let Operand::Column(column) = &operand { schema[*column].0 }
                else { return Err(expected(at, "AS alias for a computed row value")); };
                if next_schema.iter().any(|(name, _)| name.text == alias.text) {
                    return Err(error(alias.at, GraphPatternTextErrorKind::Build(PatternBuildError::DuplicateProjection)).into());
                }
                let kind = match &operand {
                    Operand::Column(column) => schema[*column].1,
                    _ => GraphSetColumnType::Scalar,
                };
                projection.push(ReadProjectionTemplate { name: alias.text.to_owned(),
                    value: self.read_value_template(operand, alias.at)? });
                next_schema.push((alias, kind));
                if !self.take(b',')? { break; }
            }
        }
        Ok((projection, next_schema))
    }

    pub(super) fn row_page(&mut self, schema: &[(Name<'a>, GraphSetColumnType)])
        -> Result<Option<ReadStageTemplate>, GraphSetTextError> {
        let at = self.current.at;
        let mut present = false;
        let mut order = Vec::new();
        if self.take_word("ORDER")? {
            present = true;
            self.word("BY")?;
            loop {
                let name = self.name()?;
                let column = schema.iter().position(|(alias, _)| alias.text == name.text)
                    .ok_or_else(|| expected(name.at, "projected WITH column"))?;
                if order.iter().any(|key: &GraphValueOrder| key.column == column) {
                    return Err(GraphSetTextError { offset: name.at,
                        kind: GraphSetTextErrorKind::OrderBuild(crate::algebra::GraphOrderError::DuplicateColumn { column }) });
                }
                let descending = self.take_word("DESC")?;
                if !descending { self.take_word("ASC")?; }
                let nulls_first = if self.take_word("NULLS")? {
                    if self.take_word("FIRST")? { true }
                    else if self.take_word("LAST")? { false }
                    else { return Err(expected(self.current.at, "FIRST or LAST")); }
                } else { false };
                order.push(GraphValueOrder { column, descending, nulls_first });
                if !self.take(b',')? { break; }
            }
        }
        let offset = if self.take_word("SKIP")? {
            present = true;
            self.row_page_number()?
        } else { ReadPageNumber::Literal(0) };
        let count = if self.take_word("LIMIT")? {
            present = true;
            Some(self.row_page_number()?)
        } else { None };
        Ok(present.then_some(ReadStageTemplate::Page { at, order, offset, count }))
    }

    fn row_page_number(&mut self) -> Result<ReadPageNumber, GraphPatternTextError> {
        Ok(match self.number(GqlParameterType::UInt64)? {
            Number::Literal(GqlParameterValue::UInt64(value)) => ReadPageNumber::Literal(value),
            Number::Parameter(index) => ReadPageNumber::Parameter(index),
            _ => unreachable!("UInt64 page parser returns its declared domain"),
        })
    }

    pub(super) fn row_disjunction(&mut self, schema: &[(Name<'a>, GraphSetColumnType)], depth: usize,
        code: &mut Vec<ReadFilterOp>) -> Result<(), GraphSetTextError> {
        self.row_conjunction(schema, depth, code)?;
        while self.is_word("OR") {
            let at = self.current.at; self.advance()?;
            self.row_conjunction(schema, depth, code)?;
            emit(code, ReadFilterOp::Or, at)?;
        }
        Ok(())
    }
    fn row_conjunction(&mut self, schema: &[(Name<'a>, GraphSetColumnType)], depth: usize,
        code: &mut Vec<ReadFilterOp>) -> Result<(), GraphSetTextError> {
        self.row_predicate(schema, depth, code)?;
        while self.is_word("AND") {
            let at = self.current.at; self.advance()?;
            self.row_predicate(schema, depth, code)?;
            emit(code, ReadFilterOp::And, at)?;
        }
        Ok(())
    }
    fn row_predicate(&mut self, schema: &[(Name<'a>, GraphSetColumnType)], depth: usize,
        code: &mut Vec<ReadFilterOp>) -> Result<(), GraphSetTextError> {
        let at = self.current.at;
        if depth > MAX_FILTER_NESTING { return Err(expected(at, "bounded row predicate nesting")); }
        // An admitted alias named not wins when used as a comparison operand.
        let alias = matches!(self.current.kind, TokenKind::Word(word)
            if schema.iter().any(|(name, _)| name.text == word));
        if self.is_word("NOT") && !alias {
            self.advance()?;
            self.row_predicate(schema, depth + 1, code)?;
            return emit(code, ReadFilterOp::Not, at);
        }
        if self.take(b'(')? {
            self.row_disjunction(schema, depth + 1, code)?;
            self.punct(b')', ")")?;
            return Ok(());
        }
        let left = self.row_filter_operand(schema)?;
        if self.take_word("IS")? {
            let negate = self.take_word("NOT")?;
            self.word("NULL")?;
            return emit(code, ReadFilterOp::IsNull { operand: left, is_null: !negate }, at);
        }
        if self.is_punct(b'=') || self.is_punct(b'!') || self.is_punct(b'<') || self.is_punct(b'>') {
            let comparison = self.comparison()?;
            let right = self.row_filter_operand(schema)?;
            return emit(code, ReadFilterOp::Compare { left, comparison, right }, at);
        }
        let truth = match left {
            ReadFilterOperand::Literal(value) => match value.value() {
                CanonicalScalar::Null => None,
                CanonicalScalar::Bool(value) => Some(*value),
                _ => return Err(expected(at, "row comparison or IS NULL")),
            },
            _ => return Err(expected(at, "row comparison or IS NULL")),
        };
        emit(code, ReadFilterOp::Truth(truth), at)
    }
    fn row_filter_operand(&mut self, schema: &[(Name<'a>, GraphSetColumnType)])
        -> Result<ReadFilterOperand, GraphSetTextError> {
        let at = self.current.at;
        let operand = self.row_operand(schema)?;
        Ok(match operand {
            Operand::Column(column) => ReadFilterOperand::Column(column),
            Operand::Literal(value) => ReadFilterOperand::Literal(value),
            Operand::Number(Number::Literal(value)) => ReadFilterOperand::Literal(scalar(value, at)?),
            Operand::Number(Number::Parameter(index)) => ReadFilterOperand::Parameter { index, at },
            Operand::Integer { .. } => unreachable!("row operands are not arithmetic expressions"),
        })
    }
}

pub(super) fn page_value(number: &ReadPageNumber, values: &[GqlParameterValue]) -> u64 {
    match number {
        ReadPageNumber::Literal(value) => *value,
        ReadPageNumber::Parameter(index) => match &values[*index] {
            GqlParameterValue::UInt64(value) => *value,
            _ => unreachable!("the complete parameter map was checked before binding"),
        },
    }
}

/// None uses scalar NULL placeholders for preparation-time schema validation.
/// No placeholder is ever executed or substituted into the retained template.
pub(super) fn bind_filter(code: &[ReadFilterOp], values: Option<&[GqlParameterValue]>)
    -> Result<Vec<GraphSetPredicateOp>, GraphSetTextError> {
    let operand = |value: &ReadFilterOperand| -> Result<GraphSetOperand, GraphSetTextError> {
        Ok(match value {
            ReadFilterOperand::Column(column) => GraphSetOperand::Column(*column),
            ReadFilterOperand::Literal(value) => GraphSetOperand::Literal(value.clone()),
            ReadFilterOperand::Parameter { index, at } => GraphSetOperand::Literal(match values {
                Some(values) => scalar(values[*index].clone(), *at)?,
                None => GqlScalarParameter::new(CanonicalScalar::Null)
                    .expect("canonical null is a bounded scalar"),
            }),
        })
    };
    code.iter().map(|op| Ok(match op {
        ReadFilterOp::Compare { left, comparison, right } => GraphSetPredicateOp::Compare {
            left: operand(left)?, comparison: *comparison, right: operand(right)?,
        },
        ReadFilterOp::IsNull { operand: value, is_null } => GraphSetPredicateOp::IsNull {
            operand: operand(value)?, is_null: *is_null,
        },
        ReadFilterOp::Truth(value) => GraphSetPredicateOp::Truth(*value),
        ReadFilterOp::Not => GraphSetPredicateOp::Not,
        ReadFilterOp::And => GraphSetPredicateOp::And,
        ReadFilterOp::Or => GraphSetPredicateOp::Or,
    })).collect()
}
