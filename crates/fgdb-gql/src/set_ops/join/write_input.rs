//! Validate the single owned input of a post-write relational continuation.
//! Walking the prepared tree never observes a graph or invokes a callback.

use super::super::{GraphSetColumnType, PreparedGraphSet, SetNode};

impl PreparedGraphSet {
    pub(crate) fn has_one_owned_input(
        &self,
        columns: &[String],
        types: &[GraphSetColumnType],
    ) -> bool {
        self.check_owned_input(columns, types, false)
    }

    /// A read arm receives both one owned occurrence and an immutable copy
    /// of that invocation's import row for restoring scope after WITH.
    pub(crate) fn has_one_owned_call_input(
        &self,
        columns: &[String],
        types: &[GraphSetColumnType],
    ) -> bool {
        self.check_owned_input(columns, types, true)
    }

    fn check_owned_input(
        &self,
        columns: &[String],
        types: &[GraphSetColumnType],
        allow_import_frame: bool,
    ) -> bool {
        fn visit(
            plan: &PreparedGraphSet,
            schema: (&[String], &[GraphSetColumnType]),
            allow_import_frame: bool,
            found: &mut bool,
        ) -> bool {
            let matches_input = || (plan.columns.as_slice(), plan.types.as_slice()) == schema;
            match &plan.node {
                SetNode::Owned { .. } => {
                    if *found || !matches_input() {
                        return false;
                    }
                    *found = true;
                    true
                }
                SetNode::Aggregate(summary) => {
                    visit(summary.input(), schema, allow_import_frame, found)
                }
                SetNode::Scope(input)
                | SetNode::Unwind { input, .. }
                | SetNode::Filter { input, .. }
                | SetNode::Project { input, .. } => visit(input, schema, allow_import_frame, found),
                SetNode::CrossJoin { left, right }
                | SetNode::Join { left, right, .. }
                | SetNode::Binary { left, right, .. } => {
                    visit(left, schema, allow_import_frame, found)
                        && visit(right, schema, allow_import_frame, found)
                }
                // An Apply's outer input executes exactly once, so it may carry
                // the single-use owned write result. A correlated body executes
                // once per outer row and must never borrow it.
                SetNode::Apply { left, right, .. } => {
                    visit(left, schema, allow_import_frame, found) && !owns_rows(right)
                }
                SetNode::Arguments => allow_import_frame && matches_input(),
                SetNode::Pattern(_) | SetNode::Values | SetNode::ProcedureCall(_) => true,
            }
        }
        fn owns_rows(plan: &PreparedGraphSet) -> bool {
            match &plan.node {
                SetNode::Owned { .. } => true,
                SetNode::Arguments
                | SetNode::Pattern(_)
                | SetNode::Values
                | SetNode::ProcedureCall(_) => false,
                SetNode::Aggregate(summary) => owns_rows(summary.input()),
                SetNode::Scope(input)
                | SetNode::Unwind { input, .. }
                | SetNode::Filter { input, .. }
                | SetNode::Project { input, .. } => owns_rows(input),
                SetNode::CrossJoin { left, right }
                | SetNode::Join { left, right, .. }
                | SetNode::Apply { left, right, .. }
                | SetNode::Binary { left, right, .. } => owns_rows(left) || owns_rows(right),
            }
        }
        let mut found = false;
        visit(self, (columns, types), allow_import_frame, &mut found) && found
    }
}
