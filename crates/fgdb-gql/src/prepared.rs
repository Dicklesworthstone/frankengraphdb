use crate::{BindError, BoundPlan, RelationBind};

/// One immutable, internally coherent prepared definition for the bounded GQL
/// slice.
///
/// Preparation owns the exact statement bytes and a clone of the canonical
/// caller-supplied bind map, then derives the [`BoundPlan`] from those owned
/// inputs exactly once. Private fields prevent a caller from pairing statement
/// or binding evidence with a plan produced from different inputs.
///
/// This is not yet the final parameterized prepared-statement protocol. It has
/// no typed parameters, catalog epoch, authorization context, cursor lifetime,
/// physical plan, or invalidation policy.
#[derive(Clone, PartialEq, Eq)]
#[must_use = "a prepared GQL query has no effect until it is executed or inspected"]
pub struct PreparedGqlQuery {
    statement: String,
    bind: RelationBind,
    plan: BoundPlan,
}

impl PreparedGqlQuery {
    /// Parse and bind one statement while retaining the exact inputs that
    /// produced the resulting plan.
    pub fn prepare(
        statement: impl Into<String>,
        bind: &RelationBind,
    ) -> Result<Self, BindError> {
        let statement = statement.into();
        let plan = bind.bind(&statement)?;
        Ok(Self {
            statement,
            bind: bind.clone(),
            plan,
        })
    }

    /// The exact statement bytes supplied at preparation time.
    #[must_use]
    pub fn statement(&self) -> &str {
        &self.statement
    }

    /// The owned canonical name-to-identifier bindings used to derive the plan.
    #[must_use]
    pub fn bind(&self) -> &RelationBind {
        &self.bind
    }

    /// The immutable executor-ready plan derived at preparation time.
    #[must_use]
    pub fn plan(&self) -> &BoundPlan {
        &self.plan
    }

    /// Reparse and rebind the retained inputs as an explicit audit check.
    ///
    /// Normal execution does not call this method: private fields already make
    /// the construction invariant stable. It exists for external evidence and
    /// persistence layers that want to re-establish coherence independently.
    #[must_use]
    pub fn verifies_definition(&self) -> bool {
        self.bind
            .bind(&self.statement)
            .is_ok_and(|plan| plan == self.plan)
    }
}

impl core::fmt::Debug for PreparedGqlQuery {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("PreparedGqlQuery")
            .field("statement", &"[REDACTED]")
            .field("bind", &"[REDACTED]")
            .field("plan", &"[REDACTED]")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::PreparedGqlQuery;
    use crate::{BindError, RelationBind};
    use fgdb_delta_types::RelationId;

    const STATEMENT: &str = "MATCH (a)-[:R]->(b) RETURN b";

    #[test]
    fn preparation_owns_one_coherent_definition() {
        let mut statement = STATEMENT.to_owned();
        let mut bind = RelationBind::new().with_relation("R", RelationId(7));
        let expected_bind = bind.clone();

        let prepared = PreparedGqlQuery::prepare(statement.clone(), &bind)
            .expect("the statement binds");

        statement.push_str(" LIMIT 1");
        bind.insert("R", RelationId(99));

        assert_eq!(prepared.statement(), STATEMENT);
        assert_eq!(prepared.bind(), &expected_bind);
        assert_eq!(prepared.plan().relation, Some(RelationId(7)));
        assert!(prepared.verifies_definition());
    }

    #[test]
    fn debug_redacts_statement_bind_and_plan() {
        let prepared = PreparedGqlQuery::prepare(
            STATEMENT,
            &RelationBind::new().with_relation("R", RelationId(7)),
        )
        .expect("the statement binds");
        let debug = format!("{prepared:?}");

        assert_eq!(debug.matches("[REDACTED]").count(), 3);
        assert!(!debug.contains(STATEMENT));
        assert!(!debug.contains("RelationId(7)"));
    }

    #[test]
    fn preparation_preserves_the_binder_refusal() {
        let error = PreparedGqlQuery::prepare(STATEMENT, &RelationBind::new())
            .expect_err("an unbound relation must refuse preparation");
        assert!(matches!(
            error,
            BindError::UnknownRelation { name } if name == "R"
        ));
    }
}
