use std::collections::BTreeSet;

/// A logical binding identity, not a position in a particular source tuple.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JoinVariable(pub u32);

pub const MAX_JOIN_VARIABLES: usize = 128;
pub const MAX_JOIN_RELATIONS: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JoinPlanError {
    TooManyVariables,
    TooManyRelations,
    DuplicateVariable(JoinVariable),
    MissingVariable(JoinVariable),
    UnknownVariable(JoinVariable),
    EmptyGroup,
    UncoveredGroup(usize),
    RelationCount { expected: usize, actual: usize },
    AttributeOrder { relation: usize },
}

impl core::fmt::Display for JoinPlanError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "invalid FreeJoin plan: {self:?}")
    }
}
impl core::error::Error for JoinPlanError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Probe {
    pub relation: usize,
    // Positions in this group's candidate, in the relation's trie order.
    pub key_positions: Vec<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Stage {
    pub start: usize,
    pub variables: Vec<JoinVariable>,
    pub covers: Vec<usize>,
    pub probes: Vec<Probe>,
}

/// An immutable physical plan over an ordered set of relation occurrences.
///
/// Each schema lists the logical variables of one relation; its tuple column
/// order may differ from the required trie order. A group must be covered by
/// at least one relation. Groups are a partition, never an implicit projection.
/// Repeated relation occurrences remain separate multiplicity factors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FreeJoinPlan {
    pub(super) schemas: Vec<Vec<JoinVariable>>,
    pub(super) order: Vec<JoinVariable>,
    pub(super) stages: Vec<Stage>,
    pub(super) required_orders: Vec<Vec<JoinVariable>>,
}

impl FreeJoinPlan {
    pub fn new(
        schemas: Vec<Vec<JoinVariable>>,
        groups: Vec<Vec<JoinVariable>>,
    ) -> Result<Self, JoinPlanError> {
        if schemas.len() > MAX_JOIN_RELATIONS {
            return Err(JoinPlanError::TooManyRelations);
        }
        if groups.len() > MAX_JOIN_VARIABLES {
            return Err(JoinPlanError::TooManyVariables);
        }
        let mut universe = BTreeSet::new();
        for schema in &schemas {
            if schema.len() > MAX_JOIN_VARIABLES {
                return Err(JoinPlanError::TooManyVariables);
            }
            let mut local = BTreeSet::new();
            for &variable in schema {
                if !local.insert(variable) {
                    return Err(JoinPlanError::DuplicateVariable(variable));
                }
                universe.insert(variable);
                if universe.len() > MAX_JOIN_VARIABLES {
                    return Err(JoinPlanError::TooManyVariables);
                }
            }
        }
        if universe.len() > MAX_JOIN_VARIABLES {
            return Err(JoinPlanError::TooManyVariables);
        }
        let mut seen = BTreeSet::new();
        let mut order = Vec::new();
        let mut stages = Vec::new();
        for (ordinal, variables) in groups.into_iter().enumerate() {
            if variables.len() > MAX_JOIN_VARIABLES {
                return Err(JoinPlanError::TooManyVariables);
            }
            if variables.is_empty() {
                return Err(JoinPlanError::EmptyGroup);
            }
            for &variable in &variables {
                if !universe.contains(&variable) {
                    return Err(JoinPlanError::UnknownVariable(variable));
                }
                if !seen.insert(variable) {
                    return Err(JoinPlanError::DuplicateVariable(variable));
                }
            }
            let mut covers = Vec::new();
            let mut probes = Vec::new();
            for (relation, schema) in schemas.iter().enumerate() {
                let key_positions: Vec<_> = variables
                    .iter()
                    .enumerate()
                    .filter_map(|(position, variable)| schema.contains(variable).then_some(position))
                    .collect();
                if key_positions.len() == variables.len() {
                    covers.push(relation);
                }
                if !key_positions.is_empty() {
                    probes.push(Probe { relation, key_positions });
                }
            }
            if covers.is_empty() {
                return Err(JoinPlanError::UncoveredGroup(ordinal));
            }
            let start = order.len();
            order.extend_from_slice(&variables);
            stages.push(Stage { start, variables, covers, probes });
        }
        if let Some(&missing) = universe.difference(&seen).next() {
            return Err(JoinPlanError::MissingVariable(missing));
        }
        let required_orders = schemas
            .iter()
            .map(|schema| order.iter().copied().filter(|variable| schema.contains(variable)).collect())
            .collect();
        Ok(Self { schemas, order, stages, required_orders })
    }

    /// The worst-case-optimal, variable-at-a-time member of the same family.
    pub fn generic(
        schemas: Vec<Vec<JoinVariable>>,
        variable_order: Vec<JoinVariable>,
    ) -> Result<Self, JoinPlanError> {
        Self::new(schemas, variable_order.into_iter().map(|variable| vec![variable]).collect())
    }

    /// Intersect the shared key projection, then enumerate the two independent
    /// payload groups. A disjoint binary join is an explicit product; empty
    /// inputs and zero-column relations retain ordinary natural-join semantics.
    pub fn binary(
        left: Vec<JoinVariable>,
        right: Vec<JoinVariable>,
    ) -> Result<Self, JoinPlanError> {
        let shared: Vec<_> = left.iter().copied().filter(|v| right.contains(v)).collect();
        let left_only: Vec<_> = left.iter().copied().filter(|v| !right.contains(v)).collect();
        let right_only: Vec<_> = right.iter().copied().filter(|v| !left.contains(v)).collect();
        let groups = [shared, left_only, right_only]
            .into_iter()
            .filter(|group| !group.is_empty())
            .collect();
        Self::new(vec![left, right], groups)
    }

    #[must_use]
    pub fn variables(&self) -> &[JoinVariable] {
        &self.order
    }

    #[must_use]
    pub fn relation_count(&self) -> usize {
        self.schemas.len()
    }

    #[must_use]
    pub fn required_order(&self, relation: usize) -> Option<&[JoinVariable]> {
        self.required_orders.get(relation).map(Vec::as_slice)
    }

    #[must_use]
    pub fn is_generic_join(&self) -> bool {
        self.stages.iter().all(|stage| stage.variables.len() == 1)
    }

    /// Versioned physical identity bytes. This is not a logical query digest or
    /// a security certificate. Input generations belong to execution evidence.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = b"fgdb-freejoin-plan\0\x01".to_vec();
        bytes.extend_from_slice(&(self.schemas.len() as u32).to_le_bytes());
        for schema in &self.schemas {
            bytes.extend_from_slice(&(schema.len() as u32).to_le_bytes());
            for variable in schema {
                bytes.extend_from_slice(&variable.0.to_le_bytes());
            }
        }
        bytes.extend_from_slice(&(self.stages.len() as u32).to_le_bytes());
        for stage in &self.stages {
            bytes.extend_from_slice(&(stage.variables.len() as u32).to_le_bytes());
            for variable in &stage.variables {
                bytes.extend_from_slice(&variable.0.to_le_bytes());
            }
        }
        bytes
    }
}
