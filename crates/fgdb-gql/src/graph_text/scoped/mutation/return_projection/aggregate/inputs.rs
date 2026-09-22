//! Intern whole terminal expressions over the completed WITH schema. A single
//! late projection computes only admitted grouping keys and aggregate inputs;
//! it never moves through a preceding filter, DISTINCT, UNWIND or page.

use super::*;

pub(super) struct Inputs {
    width: usize,
    computed: Vec<(Vec<u8>, ReadValueTemplate, GraphSetColumnType)>,
}

impl Inputs {
    pub(super) fn new(width: usize) -> Self {
        Self {
            width,
            computed: Vec::new(),
        }
    }

    pub(super) fn read<'a>(
        &mut self,
        parser: &mut Parser<'a>,
        schema: &[(Name<'a>, GraphSetColumnType)],
    ) -> Result<usize, Error> {
        let value = parser.read_row_value(schema, 0)?;
        if let ReadValueTemplate::Column(index) = value {
            return Ok(index);
        }
        // Parsing registers every explicit parameter occurrence. Interning
        // then ignores diagnostic offsets, never parameter indices or types.
        let mut identity = Vec::new();
        value.append_template_transcript(&mut identity);
        if let Some(index) = self
            .computed
            .iter()
            .position(|(previous, _, _)| *previous == identity)
        {
            return Ok(self.width + index);
        }
        parser.capacity(
            self.computed.len(),
            MAX_PATTERN_VERTICES,
            crate::algebra::PatternLimitDimension::Columns,
        )?;
        let types = schema.iter().map(|(_, kind)| *kind).collect::<Vec<_>>();
        let kind = value.column_type(&types, &parser.syntax.parameters);
        let index = self.width + self.computed.len();
        self.computed.push((identity, value, kind));
        Ok(index)
    }

    pub(super) fn kind(
        &self,
        index: usize,
        schema: &[(Name<'_>, GraphSetColumnType)],
    ) -> GraphSetColumnType {
        if index < self.width {
            schema[index].1
        } else {
            self.computed[index - self.width].2
        }
    }

    pub(super) fn is_computed(&self, index: usize) -> bool {
        index >= self.width
    }

    /// Leave plain-column definitions entirely untouched, including canonical
    /// bytes and event traces. Computed definitions retain only the columns
    /// actually consumed by grouping/summaries, without suppressing any earlier
    /// source reads, row expressions or errors in the original WITH pipeline.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn append_projection(
        self,
        parser: &Parser<'_>,
        schema: &[(Name<'_>, GraphSetColumnType)],
        keys: &mut [usize],
        summaries: &mut [PipelineSummary],
        stages: &mut Vec<ReadStageTemplate>,
        depth: usize,
        at: usize,
    ) -> Result<(), Error> {
        if self.computed.is_empty() {
            return Ok(());
        }
        // One Project node plus the Aggregate parent. Existing WITH depth is
        // already checked; no projection may bypass the definition-wide cap.
        if depth + 1 >= crate::MAX_GRAPH_SET_DEPTH {
            return Err(build(
                at,
                GraphAggregateBuildError::RelationalInput(crate::GraphSetBuildError::TooDeep {
                    limit: crate::MAX_GRAPH_SET_DEPTH,
                    observed: depth + 2,
                }),
            ));
        }
        let mut selected = Vec::new();
        for column in keys
            .iter()
            .copied()
            .chain(summaries.iter().filter_map(|summary| summary.column))
        {
            if !selected.contains(&column) {
                parser.capacity(
                    selected.len(),
                    MAX_PATTERN_VERTICES,
                    crate::algebra::PatternLimitDimension::Columns,
                )?;
                selected.push(column);
            }
        }
        let mut projection = Vec::new();
        let mut candidate = 0usize;
        for &column in &selected {
            let (name, value) = if column < self.width {
                (
                    schema[column].0.text.to_owned(),
                    ReadValueTemplate::Column(column),
                )
            } else {
                let name = loop {
                    let name = format!("__fgdb_pipeline_input_{candidate}");
                    candidate += 1;
                    if !schema.iter().any(|(alias, _)| alias.text == name)
                        && !summaries.iter().any(|summary| summary.name == name)
                    {
                        break name;
                    }
                };
                (name, self.computed[column - self.width].1.clone())
            };
            projection.push(ReadProjectionTemplate { name, value });
        }
        // These indices address the projected input, not the public result
        // slots or HAVING/ORDER BY evaluation-key and summary namespaces.
        for key in keys {
            *key = selected
                .iter()
                .position(|column| *column == *key)
                .expect("selected key");
        }
        for summary in summaries {
            if let Some(column) = &mut summary.column {
                *column = selected
                    .iter()
                    .position(|input| *input == *column)
                    .expect("selected input");
            }
        }
        stages.push(ReadStageTemplate::Project {
            at,
            projection,
            quantifier: crate::GraphSetQuantifier::All,
        });
        Ok(())
    }
}
