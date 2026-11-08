//! Unit subqueries invoke a private native write chain once per outer row.
//! They export no bindings and preserve each outer occurrence, including when
//! a body read selects no rows. The host owns the transaction and allowances.

use super::{PreparedGraphWriteChain, PreparedGraphWriteChainText};
use crate::algebra::{GraphValueRow, PreparedGraphPattern};
use crate::set_text::multipart::BoundReadInput;
use crate::{GraphSetColumnType, PreparedGraphSet};
use fgdb_delta_types::PropertyKeyId;

#[derive(Clone)]
pub(crate) struct PreparedGraphUnitCallText {
    pub(crate) input: BoundReadInput,
    pub(crate) output: Option<BoundReadInput>,
    pub(crate) body: PreparedGraphWriteChainText,
    pub(crate) outer_columns: Vec<String>,
    pub(crate) outer_types: Vec<GraphSetColumnType>,
    pub(crate) imports: Vec<usize>,
    pub(crate) read_keys: Vec<PropertyKeyId>,
    pub(crate) returning: bool,
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
