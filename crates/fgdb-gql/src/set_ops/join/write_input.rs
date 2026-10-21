//! Validate the single owned input of a post-write relational continuation.
//! Walking the prepared tree never observes a graph or invokes a callback.

use super::super::{GraphSetColumnType, PreparedGraphSet, SetNode};

impl PreparedGraphSet {
    pub(crate) fn has_one_owned_input(
        &self,
        columns: &[String],
        types: &[GraphSetColumnType],
    ) -> bool {
        fn visit(
            plan: &PreparedGraphSet,
            columns: &[String],
            types: &[GraphSetColumnType],
            found: &mut bool,
        ) -> bool {
            match &plan.node {
                SetNode::Owned => {
                    if *found
                        || plan.columns.as_slice() != columns
                        || plan.types.as_slice() != types
                    {
                        return false;
                    }
                    *found = true;
                    true
                }
                SetNode::Aggregate(summary) => visit(summary.input(), columns, types, found),
                SetNode::Scope(input)
                | SetNode::Unwind { input, .. }
                | SetNode::Filter { input, .. }
                | SetNode::Project { input, .. } => visit(input, columns, types, found),
                SetNode::CrossJoin { left, right }
                | SetNode::Join { left, right, .. }
                | SetNode::Binary { left, right, .. } => {
                    visit(left, columns, types, found) && visit(right, columns, types, found)
                }
                SetNode::Pattern(_) | SetNode::Values | SetNode::ProcedureCall(_) => true,
            }
        }
        let mut found = false;
        visit(self, columns, types, &mut found) && found
    }
}
