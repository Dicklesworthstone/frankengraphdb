//! Scoped subqueries invoke a native write chain once per outer row. Unit
//! bodies preserve that occurrence; returning bodies join their typed exports
//! onto it. The host owns the transaction and cumulative allowances.

use super::{PreparedGraphWriteChain, PreparedGraphWriteChainText};
use crate::algebra::{GraphValueRow, PreparedGraphPattern};
use crate::set_text::multipart::BoundReadInput;
use crate::{GraphSetColumnType, PreparedGraphSet};
use fgdb_delta_types::PropertyKeyId;

#[derive(Clone)]
pub(crate) struct PreparedGraphCallText {
    pub(crate) input: BoundReadInput,
    pub(crate) output: Option<BoundReadInput>,
    pub(crate) body: PreparedGraphWriteChainText,
    pub(crate) outer_columns: Vec<String>,
    pub(crate) outer_types: Vec<GraphSetColumnType>,
    pub(crate) imports: Vec<usize>,
    pub(crate) read_keys: Vec<PropertyKeyId>,
    pub(crate) returning: bool,
    /// None is a unit body. Returned columns are disjoint from the outer row.
    pub(crate) exports: Option<(Vec<String>, Vec<GraphSetColumnType>)>,
}

/// A native unit `CALL { ... }` with a checked import schema. Its body runs
/// once for each selected row in the same transaction as the surrounding
/// statement. Body-local bindings never become outer columns.
#[derive(Clone, PartialEq, Eq)]
pub struct PreparedGraphUnitCall {
    pub(crate) input: PreparedGraphSet,
    pub(crate) output: Option<PreparedGraphSet>,
    pub(crate) body: PreparedGraphWriteChain,
    pub(crate) outer_columns: Vec<String>,
    pub(crate) outer_types: Vec<GraphSetColumnType>,
    pub(crate) imports: Vec<usize>,
    pub(crate) read_keys: Vec<PropertyKeyId>,
    pub(crate) returning: bool,
}

/// A native write-bearing `CALL { ... RETURN ... }`. Each returned body row
/// extends its own outer occurrence; a body with no returned rows removes
/// that occurrence. All writes and reads share the surrounding transaction.
#[derive(Clone, PartialEq, Eq)]
pub struct PreparedGraphReturningCall {
    pub(crate) input: PreparedGraphSet,
    pub(crate) output: Option<PreparedGraphSet>,
    pub(crate) body: PreparedGraphWriteChain,
    pub(crate) outer_types: Vec<GraphSetColumnType>,
    pub(crate) imports: Vec<usize>,
    pub(crate) export_columns: Vec<String>,
    pub(crate) export_types: Vec<GraphSetColumnType>,
    pub(crate) joined_columns: Vec<String>,
    pub(crate) joined_types: Vec<GraphSetColumnType>,
    pub(crate) read_keys: Vec<PropertyKeyId>,
    pub(crate) returning: bool,
}

impl core::fmt::Debug for PreparedGraphReturningCall {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphReturningCall")
            .field("body_steps", &self.body.steps().len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

impl PreparedGraphReturningCall {
    /// Extend one saved outer occurrence with each returned native row. Every
    /// cell and payload copy is admitted before allocation; these are private
    /// relation rows and never spend the final delivered-row allowance.
    pub(super) fn join_returned_controlled<E: From<crate::GraphIntegerError>>(
        &self,
        outer: &GraphValueRow,
        returned: Vec<GraphValueRow>,
        joined: &mut Vec<GraphValueRow>,
        control: &mut impl FnMut(crate::GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        use crate::GlaExecutionEvent::{ScratchEntry, Work};
        let invalid = || crate::GraphIntegerError {
            instruction: 0,
            kind: crate::GraphIntegerErrorKind::IncompatibleOperands,
        };
        control(Work)?;
        if outer.len() != self.outer_types.len() {
            return Err(invalid().into());
        }
        for row in returned {
            control(Work)?;
            if row.len() != self.export_types.len() {
                return Err(invalid().into());
            }
            control(ScratchEntry)?;
            let mut cells = Vec::new();
            for (value, kind) in outer
                .values()
                .iter()
                .zip(&self.outer_types)
                .chain(row.values().iter().zip(&self.export_types))
            {
                control(Work)?;
                if !kind.accepts(value) {
                    return Err(invalid().into());
                }
                control(ScratchEntry)?;
                cells.push(value.copy_with_control(control)?);
            }
            control(ScratchEntry)?;
            joined.push(GraphValueRow::from_owned_values(cells));
        }
        Ok(())
    }

    pub fn input(&self) -> &PreparedGraphSet {
        &self.input
    }

    pub fn output(&self) -> Option<&PreparedGraphSet> {
        self.output.as_ref()
    }

    pub fn body(&self) -> &PreparedGraphWriteChain {
        &self.body
    }

    pub fn columns(&self) -> &[String] {
        self.output
            .as_ref()
            .map_or(&self.joined_columns, PreparedGraphSet::columns)
    }

    pub fn column_types(&self) -> &[GraphSetColumnType] {
        self.output
            .as_ref()
            .map_or(&self.joined_types, PreparedGraphSet::column_types)
    }

    /// The body's public columns, before they are joined to the outer row.
    pub fn export_columns(&self) -> &[String] {
        &self.export_columns
    }

    pub fn export_column_types(&self) -> &[GraphSetColumnType] {
        &self.export_types
    }

    pub fn read_element_keys(&self) -> &[PropertyKeyId] {
        &self.read_keys
    }

    pub fn returns_rows(&self) -> bool {
        self.returning
    }

    /// Body steps are admitted recursively by the host. A private body read
    /// still needs Read authority when it observes graph or element values.
    pub fn requires_read(&self) -> bool {
        self.input.reads_graph()
            || self.input.reads_elements()
            || self
                .output
                .as_ref()
                .is_some_and(|output| output.reads_graph() || output.reads_elements())
            || self
                .body
                .read_continuation()
                .is_some_and(|read| read.reads_graph() || read.reads_elements())
    }

    /// Visit the source and outer tail. The host also admits the complete
    /// body chain, including its terminal read, before evaluating any source.
    pub fn admit_patterns<E>(
        &self,
        mut visitor: impl FnMut(&PreparedGraphPattern<GraphValueRow>) -> Result<(), E>,
    ) -> Result<(), E> {
        self.input.admit_patterns(&mut visitor)?;
        if let Some(output) = &self.output {
            output.admit_patterns(&mut visitor)?;
        }
        Ok(())
    }
}

impl core::fmt::Debug for PreparedGraphUnitCall {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphUnitCall")
            .field("body_steps", &self.body.steps().len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

impl PreparedGraphUnitCall {
    pub fn input(&self) -> &PreparedGraphSet {
        &self.input
    }

    pub fn output(&self) -> Option<&PreparedGraphSet> {
        self.output.as_ref()
    }

    pub fn body(&self) -> &PreparedGraphWriteChain {
        &self.body
    }

    pub fn columns(&self) -> &[String] {
        self.output
            .as_ref()
            .map_or(&self.outer_columns, PreparedGraphSet::columns)
    }

    pub fn column_types(&self) -> &[GraphSetColumnType] {
        self.output
            .as_ref()
            .map_or(&self.outer_types, PreparedGraphSet::column_types)
    }

    pub fn read_element_keys(&self) -> &[PropertyKeyId] {
        &self.read_keys
    }

    pub fn returns_rows(&self) -> bool {
        self.returning
    }

    pub fn requires_read(&self) -> bool {
        self.input.reads_graph()
            || self.input.reads_elements()
            || self
                .output
                .as_ref()
                .is_some_and(|output| output.reads_graph() || output.reads_elements())
    }

    /// Visit the invocation's source and outer tail. Hosts must also preflight
    /// every body step before executing any source, even for an empty input.
    pub fn admit_patterns<E>(
        &self,
        mut visitor: impl FnMut(&PreparedGraphPattern<GraphValueRow>) -> Result<(), E>,
    ) -> Result<(), E> {
        self.input.admit_patterns(&mut visitor)?;
        if let Some(output) = &self.output {
            output.admit_patterns(&mut visitor)?;
        }
        Ok(())
    }
}
