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

/// Deterministic admission limits for one owned prepared-query execution.
///
/// `snapshot_records` counts the complete immutable table admitted to the
/// bounded executor: vertices for a node scan, edges for an edge pattern.
/// `result_rows` counts final rows after predicates, projection, sorting,
/// deduplication, `SKIP`, and `LIMIT`.
///
/// These are deterministic work-shape guards, not wall-clock cancellation,
/// allocation or I/O preemption, spill governance, or physical cost evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GqlExecutionBudget {
    max_snapshot_records: Option<u64>,
    max_result_rows: Option<u64>,
}

impl GqlExecutionBudget {
    /// No deterministic row-count limits.
    pub const UNLIMITED: Self = Self {
        max_snapshot_records: None,
        max_result_rows: None,
    };

    /// Bound both deterministic dimensions. Zero is a valid fail-closed limit.
    #[must_use]
    pub const fn new(max_snapshot_records: u64, max_result_rows: u64) -> Self {
        Self {
            max_snapshot_records: Some(max_snapshot_records),
            max_result_rows: Some(max_result_rows),
        }
    }

    /// Bound admitted snapshot records while leaving final rows unlimited.
    #[must_use]
    pub const fn snapshot_records(max_snapshot_records: u64) -> Self {
        Self {
            max_snapshot_records: Some(max_snapshot_records),
            max_result_rows: None,
        }
    }

    /// Bound final result rows while leaving snapshot admission unlimited.
    #[must_use]
    pub const fn result_rows(max_result_rows: u64) -> Self {
        Self {
            max_snapshot_records: None,
            max_result_rows: Some(max_result_rows),
        }
    }

    #[must_use]
    pub const fn max_snapshot_records(self) -> Option<u64> {
        self.max_snapshot_records
    }

    #[must_use]
    pub const fn max_result_rows(self) -> Option<u64> {
        self.max_result_rows
    }

    /// Check one dimension using the same exact-boundary semantics as runtime
    /// execution: `observed == limit` succeeds and `observed > limit` refuses.
    pub fn check(
        self,
        dimension: GqlBudgetDimension,
        observed: u64,
    ) -> Result<(), GqlBudgetExceeded> {
        let limit = match dimension {
            GqlBudgetDimension::SnapshotRecords => self.max_snapshot_records,
            GqlBudgetDimension::ResultRows => self.max_result_rows,
        };
        match limit {
            Some(limit) if observed > limit => Err(GqlBudgetExceeded {
                dimension,
                limit,
                observed,
            }),
            Some(_) | None => Ok(()),
        }
    }
}

impl Default for GqlExecutionBudget {
    fn default() -> Self {
        Self::UNLIMITED
    }
}

/// The deterministic dimension that exhausted its configured bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GqlBudgetDimension {
    SnapshotRecords,
    ResultRows,
}

/// Typed refusal produced when one deterministic query bound is exceeded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GqlBudgetExceeded {
    pub dimension: GqlBudgetDimension,
    pub limit: u64,
    pub observed: u64,
}

impl core::fmt::Display for GqlBudgetExceeded {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            formatter,
            "GQL {:?} budget exceeded: observed {}, limit {}",
            self.dimension, self.observed, self.limit
        )
    }
}

impl core::error::Error for GqlBudgetExceeded {}

/// Exact deterministic counters returned by successful budgeted execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GqlExecutionStats {
    pub snapshot_records: u64,
    pub result_rows: u64,
}

/// One successful value plus the exact counters checked during its execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BudgetedGqlExecution<T> {
    pub value: T,
    pub stats: GqlExecutionStats,
}

/// Execution refusal that preserves whether the source read/query failed or a
/// deterministic budget was exhausted.
#[derive(Debug)]
pub enum BudgetedGqlError<E> {
    Execution(E),
    Budget(GqlBudgetExceeded),
}

impl<E: core::fmt::Display> core::fmt::Display for BudgetedGqlError<E> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Execution(source) => core::fmt::Display::fmt(source, formatter),
            Self::Budget(source) => core::fmt::Display::fmt(source, formatter),
        }
    }
}

impl<E: core::error::Error + 'static> core::error::Error for BudgetedGqlError<E> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Execution(source) => Some(source),
            Self::Budget(source) => Some(source),
        }
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
    use super::{GqlBudgetDimension, GqlExecutionBudget, PreparedGqlQuery};
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

    #[test]
    fn execution_budget_uses_exact_boundaries_and_typed_refusals() {
        let budget = GqlExecutionBudget::new(7, 3);
        assert!(
            budget
                .check(GqlBudgetDimension::SnapshotRecords, 7)
                .is_ok()
        );
        assert!(budget.check(GqlBudgetDimension::ResultRows, 3).is_ok());

        let snapshot = budget
            .check(GqlBudgetDimension::SnapshotRecords, 8)
            .expect_err("one record over the limit refuses");
        assert_eq!(snapshot.dimension, GqlBudgetDimension::SnapshotRecords);
        assert_eq!(snapshot.limit, 7);
        assert_eq!(snapshot.observed, 8);

        let rows = budget
            .check(GqlBudgetDimension::ResultRows, 4)
            .expect_err("one row over the limit refuses");
        assert_eq!(rows.dimension, GqlBudgetDimension::ResultRows);
        assert_eq!(rows.limit, 3);
        assert_eq!(rows.observed, 4);
    }

    #[test]
    fn unlimited_budget_accepts_both_dimensions() {
        assert!(
            GqlExecutionBudget::UNLIMITED
                .check(GqlBudgetDimension::SnapshotRecords, u64::MAX)
                .is_ok()
        );
        assert!(
            GqlExecutionBudget::default()
                .check(GqlBudgetDimension::ResultRows, u64::MAX)
                .is_ok()
        );
    }
}
