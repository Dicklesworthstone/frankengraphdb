//! Mandatory object predicates, separate from ordinary query WHERE clauses.

use crate::{Error, LimitDimension, QueryLimits, Rights, Scope};
use core::marker::PhantomData;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use std::collections::BTreeSet;

/// Immutable result of authenticated caveat compilation.
///
/// No public constructor, default, deserializer, or mutable fields exist.
/// A planner must retain this program independently of rewritable user
/// predicates. These predicates operate on the original object's metadata;
/// user predicates and projections operate on the MASKED metadata instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannerPredicates {
    pub(crate) branch: String,
    pub(crate) label_clauses: Vec<BTreeSet<LabelId>>,
    pub(crate) relations: Scope<RelationId>,
    pub(crate) properties: Scope<PropertyKeyId>,
    pub(crate) denied_properties: BTreeSet<PropertyKeyId>,
    pub(crate) rights: Rights,
    pub(crate) limits: QueryLimits,
    pub(crate) not_before_ms: u64,
    pub(crate) expires_at_ms: u64,
}

impl PlannerPredicates {
    #[must_use]
    pub fn branch(&self) -> &str {
        &self.branch
    }

    #[must_use]
    pub const fn rights(&self) -> Rights {
        self.rights
    }

    #[must_use]
    pub const fn limits(&self) -> QueryLimits {
        self.limits
    }

    /// ALL of the signed any-label clauses must match a vertex.
    ///
    /// Do not replace this CNF with an intersection of label sets: a vertex
    /// can have several labels and satisfy separate clauses with different
    /// labels. An explicit empty clause denies even unlabeled vertices.
    #[must_use]
    pub fn allows_vertex(&self, original_labels: &[LabelId]) -> bool {
        self.label_clauses
            .iter()
            .all(|clause| original_labels.iter().any(|label| clause.contains(label)))
    }

    /// Label names used in WHERE, labels(), certificates and metadata must
    /// satisfy EVERY label scope, even if the vertex itself was admitted.
    #[must_use]
    pub fn allows_label(&self, label: LabelId) -> bool {
        self.label_clauses
            .iter()
            .all(|clause| clause.contains(&label))
    }

    #[must_use]
    pub fn allows_relation(&self, relation: RelationId) -> bool {
        self.relations.contains(&relation)
    }

    /// Apply to predicates, projections, ordering, aggregates, index probes,
    /// and property-existence tests, not just to returned property values.
    #[must_use]
    pub fn allows_property(&self, property: PropertyKeyId) -> bool {
        self.properties.contains(&property) && !self.denied_properties.contains(&property)
    }

    /// Apply before admitting an edge into scans, adjacency, degree, or paths.
    /// Both endpoint predicates are required, including intermediate vertices
    /// that do not appear in the query's RETURN clause.
    #[must_use]
    pub fn allows_edge(
        &self,
        relation: RelationId,
        source_labels: &[LabelId],
        destination_labels: &[LabelId],
    ) -> bool {
        self.allows_relation(relation)
            && self.allows_vertex(source_labels)
            && self.allows_vertex(destination_labels)
    }

    pub(crate) fn check_at(&self, branch: &str, now_ms: u64) -> Result<(), Error> {
        if branch != self.branch {
            return Err(Error::ScopeDenied);
        }
        if now_ms < self.not_before_ms {
            return Err(Error::NotYetValid);
        }
        if now_ms >= self.expires_at_ms {
            return Err(Error::Expired);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub nodes: u64,
    pub work: u64,
    pub rows: u64,
}

/// Type-level read authority; a read permit cannot be passed as a write permit.
///
/// ```compile_fail
/// use fgdb_warden::{ExecutionPermit, ReadAccess, WriteAccess};
/// fn promote<'a>(read: ExecutionPermit<'a, ReadAccess>) -> ExecutionPermit<'a, WriteAccess> {
///     read
/// }
/// ```
#[derive(Debug)]
pub enum ReadAccess {}

/// Type-level write authority, obtainable only after checking signed rights.
#[derive(Debug)]
pub enum WriteAccess {}

/// One non-cloneable execution allowance borrowed from a verified capability.
///
/// A failed charge or validity check terminally stops this execution; catching
/// an error does not permit continued work or a later smaller charge. A new
/// execution must receive a newly issued permit from the verified capability.
/// The ceilings are intentionally per execution, not a global rate limiter.
#[derive(Debug)]
pub struct ExecutionPermit<'a, Access> {
    program: &'a PlannerPredicates,
    usage: Usage,
    stopped: bool,
    access: PhantomData<Access>,
}

impl<'a, Access> ExecutionPermit<'a, Access> {
    pub(crate) fn new(program: &'a PlannerPredicates) -> Self {
        Self {
            program,
            usage: Usage::default(),
            stopped: false,
            access: PhantomData,
        }
    }

    #[must_use]
    pub fn predicates(&self) -> &PlannerPredicates {
        self.program
    }

    #[must_use]
    pub const fn usage(&self) -> Usage {
        self.usage
    }

    pub fn checkpoint_at(&mut self, now_ms: u64) -> Result<(), Error> {
        if self.stopped {
            return Err(Error::ExecutionStopped);
        }
        if let Err(error) = self.program.check_at(&self.program.branch, now_ms) {
            self.stopped = true;
            return Err(error);
        }
        Ok(())
    }

    pub fn charge_work_at(&mut self, now_ms: u64, amount: u64) -> Result<(), Error> {
        self.charge_at(now_ms, amount, LimitDimension::Work)
    }

    pub fn charge_nodes_at(&mut self, now_ms: u64, amount: u64) -> Result<(), Error> {
        self.charge_at(now_ms, amount, LimitDimension::Nodes)
    }

    pub fn charge_rows_at(&mut self, now_ms: u64, amount: u64) -> Result<(), Error> {
        self.charge_at(now_ms, amount, LimitDimension::Rows)
    }

    fn charge_at(
        &mut self,
        now_ms: u64,
        amount: u64,
        dimension: LimitDimension,
    ) -> Result<(), Error> {
        self.checkpoint_at(now_ms)?;
        let (used, limit) = match dimension {
            LimitDimension::Nodes => (&mut self.usage.nodes, self.program.limits.max_nodes),
            LimitDimension::Work => (&mut self.usage.work, self.program.limits.max_work),
            LimitDimension::Rows => (&mut self.usage.rows, self.program.limits.max_rows),
        };
        match used.checked_add(amount) {
            Some(next) if next <= limit => {
                *used = next;
                Ok(())
            }
            _ => {
                self.stopped = true;
                Err(Error::LimitExceeded(dimension))
            }
        }
    }
}

impl ExecutionPermit<'_, ReadAccess> {
    /// An unauthorized relation does not even invoke the descriptor opener.
    ///
    /// The trusted source supplies the opener; its returned data stays behind
    /// that source boundary. This is NOT a raw-adjacency API for token holders.
    /// Visible degree must additionally count only `allows_edge` endpoints.
    pub fn with_relation_at<T>(
        &mut self,
        now_ms: u64,
        relation: RelationId,
        open: impl FnOnce() -> T,
    ) -> Result<Option<T>, Error> {
        self.checkpoint_at(now_ms)?;
        if !self.program.rights.can_read() {
            self.stopped = true;
            return Err(Error::PermissionDenied);
        }
        if !self.program.allows_relation(relation) {
            return Ok(None);
        }
        self.charge_work_at(now_ms, 1)?;
        Ok(Some(open()))
    }
}
