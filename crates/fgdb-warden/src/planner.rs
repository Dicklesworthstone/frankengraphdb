//! Mandatory object predicates, separate from ordinary query WHERE clauses.

mod write;
pub use write::{EdgeWriteImage, VertexWriteFields, VertexWriteImage, WriteEndpoint};

use crate::{Authority, Error, LimitDimension, QueryLimits, Rights, Scope};
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
    /// Exact identity of this compiled program for a trusted host's derived
    /// data cache. This is neither a capability nor a live authorization
    /// decision: every reuse still needs a fresh verified execution permit.
    /// The bytes are process-local metadata, not a durable or public wire
    /// format. A cache must also bind its actual source owner and request.
    ///
    /// Every field participates, including individual conjunctive label
    /// clauses, unrestricted versus empty scopes, all 64 bits of each ordinal,
    /// rights, limits and validity. Lengths delimit every variable-size value;
    /// equality therefore needs no digest-collision assumption. Construction
    /// is bounded by the authenticated token's caveat/scope ingress limits.
    /// A retaining cache can charge the returned vector's actual capacity.
    /// Size overflow, an oversized compiled program, or failed reservation
    /// returns None; the host must then use its ordinary uncached path.
    #[must_use]
    pub fn cache_identity(&self) -> Option<Vec<u8>> {
        // Intentionally exhaustive: adding a compiled predicate must force a
        // decision here instead of silently sharing entries across that axis.
        let Self {
            branch,
            label_clauses,
            relations,
            properties,
            denied_properties,
            rights,
            limits,
            not_before_ms,
            expires_at_ms,
        } = self;
        // Nine fixed words, two one-byte tags, and length-delimited
        // branch/clauses/scopes. All additions precede allocation. The cap
        // follows the authenticated maximum caveat count and scope size,
        // with room for the branch and fixed metadata; no cache request can
        // create an unbounded temporary key even if compilation later grows.
        let words = label_clauses.iter().try_fold(9usize, |words, clause| {
            words.checked_add(1)?.checked_add(clause.len())
        })?;
        let scope_words = |len: Option<usize>| match len {
            Some(len) => len.checked_add(1),
            None => Some(0),
        };
        let relation_words = scope_words(match relations {
            Scope::All => None,
            Scope::Only(values) => Some(values.len()),
        })?;
        let property_words = scope_words(match properties {
            Scope::All => None,
            Scope::Only(values) => Some(values.len()),
        })?;
        let needed = words
            .checked_add(relation_words)?
            .checked_add(property_words)?
            .checked_add(denied_properties.len())?
            .checked_mul(size_of::<u64>())?
            .checked_add(branch.len())?
            .checked_add(2)?;
        const MAX_BYTES: usize = size_of::<u64>()
            * (crate::MAX_CAVEATS * (crate::MAX_SCOPE_ORDINALS + 1) + 32)
            + crate::MAX_NAME_BYTES;
        if needed > MAX_BYTES {
            return None;
        }
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(needed).ok()?;
        let append_word = |bytes: &mut Vec<u8>, word: u64| {
            bytes.extend_from_slice(&word.to_le_bytes());
        };
        append_word(&mut bytes, branch.len() as u64);
        bytes.extend_from_slice(branch.as_bytes());
        append_word(&mut bytes, label_clauses.len() as u64);
        for clause in label_clauses {
            append_word(&mut bytes, clause.len() as u64);
            for label in clause {
                append_word(&mut bytes, label.0);
            }
        }
        append_scope_identity(&mut bytes, relations, |id| id.0);
        append_scope_identity(&mut bytes, properties, |id| id.0);
        append_word(&mut bytes, denied_properties.len() as u64);
        for property in denied_properties {
            append_word(&mut bytes, property.0);
        }
        append_word(&mut bytes, rights.bits());
        for word in [
            limits.max_nodes,
            limits.max_work,
            limits.max_rows,
            *not_before_ms,
            *expires_at_ms,
        ] {
            append_word(&mut bytes, word);
        }
        debug_assert_eq!(bytes.len(), needed);
        Some(bytes)
    }

    /// Compare authenticated effective predicates, never token spelling or
    /// public bindings. Every axis must narrow independently; an empty visible
    /// population does not excuse widened metadata, rights, time or budgets.
    pub(crate) fn narrows(&self, previous: &Self) -> bool {
        let relations = match (&self.relations, &previous.relations) {
            (_, Scope::All) => true,
            (Scope::Only(next), Scope::Only(old)) => next.is_subset(old),
            (Scope::All, Scope::Only(_)) => false,
        };
        let properties = match (&self.properties, &previous.properties) {
            (Scope::Only(next), _) => next
                .iter()
                .all(|key| self.denied_properties.contains(key) || previous.allows_property(*key)),
            (Scope::All, Scope::All) => previous
                .denied_properties
                .is_subset(&self.denied_properties),
            (Scope::All, Scope::Only(_)) => false,
        };
        // Positive any-label clauses are conjunctive. A new clause contained
        // in each old clause proves implication, including DENY ALL's empty
        // clause. Intersecting all labels would be unsound for multi-label
        // vertices, which may satisfy different clauses with different labels.
        let labels = previous
            .label_clauses
            .iter()
            .all(|old| self.label_clauses.iter().any(|next| next.is_subset(old)));
        self.branch == previous.branch
            && self.rights.bits() & !previous.rights.bits() == 0
            && self.limits.max_nodes <= previous.limits.max_nodes
            && self.limits.max_work <= previous.limits.max_work
            && self.limits.max_rows <= previous.limits.max_rows
            && self.not_before_ms >= previous.not_before_ms
            && self.expires_at_ms <= previous.expires_at_ms
            && relations
            && properties
            && labels
    }

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

    /// Whether every incident edge of a visible vertex is visible: there is no
    /// relation scope and no label clause that could hide an edge or its other
    /// endpoint. A decision that depends on complete incidence, such as a
    /// cascading vertex delete, must require this up front so that its refusal
    /// depends on the capability alone, never on hidden data (fgdb-4iiho).
    #[must_use]
    pub fn sees_all_incidence(&self) -> bool {
        self.label_clauses.is_empty() && matches!(self.relations, Scope::All)
    }

    /// Whether no property can be hidden by this capability. Check this before
    /// looking up a whole-edge deletion target: accepting only when the target
    /// happens to have no hidden properties would disclose their existence.
    /// Unlike vertex deletion, deleting an admitted edge erases no endpoint
    /// labels or other incidence, so label/relation scopes remain permitted.
    #[must_use]
    pub fn sees_all_properties(&self) -> bool {
        matches!(self.properties, Scope::All) && self.denied_properties.is_empty()
    }

    /// Whether every label and property of a visible element is visible, so an
    /// operation that erases the whole element cannot erase or reveal fields
    /// the capability cannot observe.
    #[must_use]
    pub fn sees_all_fields(&self) -> bool {
        self.label_clauses.is_empty() && self.sees_all_properties()
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

fn append_scope_identity<T: Ord>(
    bytes: &mut Vec<u8>,
    scope: &Scope<T>,
    ordinal: impl Fn(&T) -> u64,
) {
    match scope {
        Scope::All => bytes.push(0),
        Scope::Only(values) => {
            bytes.push(1);
            bytes.extend_from_slice(&(values.len() as u64).to_le_bytes());
            for value in values {
                bytes.extend_from_slice(&ordinal(value).to_le_bytes());
            }
        }
    }
}

#[cfg(test)]
mod cache_identity_tests {
    use super::*;

    fn program() -> PlannerPredicates {
        PlannerPredicates {
            branch: "main".into(),
            label_clauses: vec![[LabelId(1), LabelId(2)].into()],
            relations: Scope::Only([RelationId(3)].into()),
            properties: Scope::Only([PropertyKeyId(4)].into()),
            denied_properties: [PropertyKeyId(5)].into(),
            rights: Rights::Read,
            limits: QueryLimits {
                max_nodes: 6,
                max_work: 7,
                max_rows: 8,
            },
            not_before_ms: 9,
            expires_at_ms: 10,
        }
    }

    #[test]
    fn cache_identity_distinguishes_every_compiled_axis_and_full_width_ordinal() {
        let base = program();
        let variants: [fn(&mut PlannerPredicates); 17] = [
            |p| p.branch.push('\0'),
            |p| p.label_clauses.clear(),
            |p| p.label_clauses.push(BTreeSet::new()),
            |p| p.label_clauses[0] = [LabelId(1), LabelId(u64::MAX)].into(),
            |p| p.relations = Scope::All,
            |p| p.relations = Scope::Only(BTreeSet::new()),
            |p| p.relations = Scope::Only([RelationId(3 + (1 << 32))].into()),
            |p| p.properties = Scope::All,
            |p| p.properties = Scope::Only(BTreeSet::new()),
            |p| p.properties = Scope::Only([PropertyKeyId(4 + (1 << 32))].into()),
            |p| p.denied_properties = [PropertyKeyId(u64::MAX)].into(),
            |p| p.rights = Rights::ReadWrite,
            |p| p.limits.max_nodes += 1,
            |p| p.limits.max_work += 1,
            |p| p.limits.max_rows += 1,
            |p| p.not_before_ms += 1,
            |p| p.expires_at_ms += 1,
        ];
        let mut programs = vec![base.clone()];
        for change in variants {
            let mut next = base.clone();
            change(&mut next);
            programs.push(next);
        }
        // Distinct clause partitions with identical flattened ordinals may
        // admit different multi-label vertices. Empty All/Only scopes above
        // similarly have opposite meanings despite both holding zero IDs.
        for clauses in [
            vec![[LabelId(1), LabelId(2)].into(), [LabelId(3)].into()],
            vec![[LabelId(1)].into(), [LabelId(2), LabelId(3)].into()],
            vec![[LabelId(1), LabelId(2), LabelId(3)].into()],
        ] {
            let mut next = base.clone();
            next.label_clauses = clauses;
            programs.push(next);
        }
        for left in &programs {
            let identity = left.cache_identity().expect("bounded identity");
            assert_eq!(identity, left.clone().cache_identity().unwrap());
            for right in &programs {
                assert_eq!(
                    identity == right.cache_identity().unwrap(),
                    left == right,
                    "identity collision: {left:?} / {right:?}"
                );
            }
        }
    }

    #[test]
    fn cache_identity_size_is_exactly_delimited_and_bounded_before_allocation() {
        let mut value = program();
        // Authenticated programs cannot exceed the aggregate caveat ordinal
        // bound. Exercise it directly, independently of token spelling.
        value.label_clauses = (0..crate::MAX_CAVEATS)
            .map(|clause| {
                (0..crate::MAX_SCOPE_ORDINALS)
                    .map(|id| LabelId((clause as u64) << 32 | id as u64))
                    .collect()
            })
            .collect();
        value.relations = Scope::All;
        value.properties = Scope::All;
        value.denied_properties.clear();
        value.branch = "x".repeat(crate::MAX_NAME_BYTES);
        let identity = value.cache_identity().unwrap();
        let expected = 9 * 8
            + 2
            + value.branch.len()
            + crate::MAX_CAVEATS * (crate::MAX_SCOPE_ORDINALS + 1) * 8;
        assert_eq!(identity.len(), expected);
        assert!(identity.capacity() >= identity.len());
        // Simulates a future compiler admitting a shape beyond today's
        // ingress cap: optional reuse declines; no partial identity escapes.
        value.branch.push_str(&"x".repeat(1 << 20));
        assert!(value.cache_identity().is_none());
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
/// Retirement is checked at every boundary; host time cannot move backwards
/// within a permit. Equal timestamps are valid. Predicates are definition
/// metadata, not an independent live execution allowance.
#[derive(Debug)]
pub struct ExecutionPermit<'a, Access> {
    program: &'a PlannerPredicates,
    authority: &'a Authority,
    usage: Usage,
    stopped: bool,
    last_now_ms: u64,
    access: PhantomData<Access>,
}

impl<'a, Access> ExecutionPermit<'a, Access> {
    pub(crate) fn new(
        program: &'a PlannerPredicates,
        authority: &'a Authority,
        now_ms: u64,
    ) -> Self {
        Self {
            program,
            authority,
            usage: Usage::default(),
            stopped: false,
            last_now_ms: now_ms,
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

    /// Canonical, domain-separated byte transcript of the verified authority
    /// and compiled capability predicates that admitted this permit
    /// (`fgdb.warden.authz_decision.v1`, FG-INV-20).
    ///
    /// Never includes the macaroon's secret HMAC attenuation tag or mutable
    /// execution usage/clock state, so identical capability + policy epoch
    /// produces a byte-identical transcript while any caveat attenuation
    /// changes it deterministically.
    #[must_use]
    pub fn canonical_decision_bytes(&self) -> Vec<u8> {
        let mut out = Vec::from(&b"fgdb.warden.authz_decision.v1\0"[..]);
        let id = self.authority.identifier().as_bytes();
        out.extend_from_slice(&(id.len() as u64).to_be_bytes());
        out.extend_from_slice(id);
        let branch = self.program.branch.as_bytes();
        out.extend_from_slice(&(branch.len() as u64).to_be_bytes());
        out.extend_from_slice(branch);
        out.extend_from_slice(&self.program.rights.bits().to_be_bytes());
        out.extend_from_slice(&self.program.limits.max_nodes.to_be_bytes());
        out.extend_from_slice(&self.program.limits.max_work.to_be_bytes());
        out.extend_from_slice(&self.program.limits.max_rows.to_be_bytes());
        out.extend_from_slice(&self.program.not_before_ms.to_be_bytes());
        out.extend_from_slice(&self.program.expires_at_ms.to_be_bytes());
        out.extend_from_slice(&(self.program.label_clauses.len() as u64).to_be_bytes());
        for clause in &self.program.label_clauses {
            out.extend_from_slice(&(clause.len() as u64).to_be_bytes());
            for label in clause {
                out.extend_from_slice(&label.0.to_be_bytes());
            }
        }
        match &self.program.relations {
            Scope::All => out.push(0),
            Scope::Only(set) => {
                out.push(1);
                out.extend_from_slice(&(set.len() as u64).to_be_bytes());
                for relation in set {
                    out.extend_from_slice(&relation.0.to_be_bytes());
                }
            }
        }
        match &self.program.properties {
            Scope::All => out.push(0),
            Scope::Only(set) => {
                out.push(1);
                out.extend_from_slice(&(set.len() as u64).to_be_bytes());
                for property in set {
                    out.extend_from_slice(&property.0.to_be_bytes());
                }
            }
        }
        out.extend_from_slice(&(self.program.denied_properties.len() as u64).to_be_bytes());
        for property in &self.program.denied_properties {
            out.extend_from_slice(&property.0.to_be_bytes());
        }
        out
    }

    pub fn checkpoint_at(&mut self, now_ms: u64) -> Result<(), Error> {
        if self.stopped {
            return Err(Error::ExecutionStopped);
        }
        if let Err(error) = self.authority.check_active() {
            self.stopped = true;
            return Err(error);
        }
        if now_ms < self.last_now_ms {
            self.stopped = true;
            return Err(Error::ClockWentBackwards);
        }
        if let Err(error) = self.program.check_at(&self.program.branch, now_ms) {
            self.stopped = true;
            return Err(error);
        }
        self.last_now_ms = now_ms;
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
    /// Retirement during the opener drops its returned value instead of
    /// releasing it. The opener must not itself publish externally observable
    /// effects. A caller must still sample fresh trusted time after slow I/O.
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
        let value = open();
        self.checkpoint_at(now_ms)?;
        Ok(Some(value))
    }
}
