//! Schema-bound graph components lowered to the shared GLA evaluator.
//! Projection may return vertex IDs, correlated bindings or canonical values.
//! Captured chains preserve ordered routes; uncaptured WALKs retain endpoints.

mod property_comparison;
mod value_projection;

use super::{
    BindingSlot, GlaDirection, GlaOperator, GlaPlan, GraphBindingRow, GraphPathFunction,
    GraphWalkSearch, IntegerComparison, VertexPredicate,
};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::VId;
use property_comparison::PropertyComparison;

pub const MAX_PATTERN_EDGES: usize = 64;
pub const MAX_PATTERN_VERTICES: usize = MAX_PATTERN_EDGES + 1;
pub const MAX_PATTERN_PREDICATES: usize = 256;
pub const MAX_PATTERN_IDENTITIES: usize = 64;
pub const MAX_PATTERN_NAME_BYTES: usize = 128;
/// Definition-wide binding frames, including copied correlations and probe
/// locals. Independent components must not evade the bounded scope inventory.
pub const MAX_PATTERN_BINDINGS: usize =
    MAX_PATTERN_VERTICES + MAX_PATTERN_EDGES + MAX_PATTERN_IDENTITIES;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatternLimitDimension {
    Vertices,
    Edges,
    Predicates,
    Identities,
    Columns,
    Bindings,
    PathCaptures,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatternBuildError {
    EmptyPattern,
    InvalidVariableName,
    DuplicateVariable,
    UnknownVariable,
    OuterVertexRequiresScope,
    UnknownOuterVertex,
    OuterVertexInPattern,
    Disconnected,
    EmptyProjection,
    DuplicateProjection,
    InvalidColumnName,
    RequiresValueProjection,
    InvalidPathCapture,
    LimitExceeded {
        dimension: PatternLimitDimension,
        limit: usize,
        observed: usize,
    },
}
impl core::fmt::Display for PatternBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::EmptyPattern => f.write_str("graph pattern requires a vertex"),
            Self::InvalidVariableName => f.write_str("invalid graph-pattern variable name"),
            Self::DuplicateVariable => f.write_str("graph-pattern variable is already declared"),
            Self::UnknownVariable => f.write_str("graph pattern references an undeclared variable"),
            Self::OuterVertexRequiresScope => f.write_str("outer vertex operands require a containing graph clause"),
            Self::UnknownOuterVertex => f.write_str("outer vertex operand is not visible at this clause"),
            Self::OuterVertexInPattern => f.write_str("an outer predicate operand cannot be an edge endpoint"),
            Self::Disconnected => {
                f.write_str("graph-pattern clause requires an already visible correlation")
            }
            Self::EmptyProjection => f.write_str("binding projection requires a column"),
            Self::DuplicateProjection => f.write_str("binding projection repeats a column"),
            Self::InvalidColumnName => f.write_str("invalid graph-pattern column name"),
            Self::RequiresValueProjection => f.write_str(
                "binding property comparisons and path captures require prepare_values or its scoped variants",
            ),
            Self::InvalidPathCapture => f.write_str("path capture requires an ordered connected root chain or a single WALK atom"),
            Self::LimitExceeded {
                dimension,
                limit,
                observed,
            } => write!(
                f,
                "graph-pattern {dimension:?} limit exceeded: {observed} > {limit}"
            ),
        }
    }
}
impl core::error::Error for PatternBuildError {}

#[derive(Clone)]
struct Variable {
    name: String,
    predicates: Vec<VertexPredicate>,
    outer: bool,
}
#[derive(Clone, Copy)]
struct Edge {
    source: usize,
    destination: usize,
    relation: RelationId,
    direction: GlaDirection,
    walk: Option<crate::GraphWalkBounds>,
    search: GraphWalkSearch,
}
impl Edge {
    fn expansion(self, source: BindingSlot, direction: GlaDirection) -> GlaOperator {
        match self.walk {
            Some(bounds) => GlaOperator::VarLengthExpand {
                source,
                relation: self.relation,
                direction,
                bounds,
                search: self.search,
            },
            None => GlaOperator::Expand {
                source,
                relation: self.relation,
                direction,
            },
        }
    }
}
#[derive(Clone, Copy)]
struct Identity {
    left: usize,
    right: usize,
    equal: bool,
}

#[derive(Clone)]
struct PathCapture {
    name: String,
    start: usize,
    first_edge: usize,
    edge_count: usize,
    edge_identity: bool,
}

#[derive(Clone)]
enum PathPredicate {
    Length {
        capture: u32,
        comparison: IntegerComparison,
        value: i64,
    },
    Null {
        capture: u32,
        function: GraphPathFunction,
        is_null: bool,
    },
}

/// Bounded definition metadata. Mutators validate before changing the builder.
#[derive(Clone, Default)]
pub struct GraphPatternBuilder {
    variables: Vec<Variable>,
    edges: Vec<Edge>,
    identities: Vec<Identity>,
    property_comparisons: Vec<PropertyComparison>,
    path_captures: Vec<PathCapture>,
    path_predicates: Vec<PathPredicate>,
    predicate_count: usize,
}
impl core::fmt::Debug for GraphPatternBuilder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GraphPatternBuilder")
            .field("variables", &self.variables.len())
            .field("edges", &self.edges.len())
            .field("predicates", &self.predicate_count)
            .field("identities", &self.identities.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

/// Immutable logical definition and ordered output schema. Names are retained
/// as column metadata, never substituted into execution. The default preserves
/// the original single-vertex API; tuple preparations carry GraphBindingRow.
#[derive(Clone, PartialEq, Eq)]
pub struct PreparedGraphPattern<Row = VId> {
    logical: GlaPlan<Row>,
    variable_count: usize,
    edge_count: usize,
    columns: Vec<String>,
}
impl<Row> core::fmt::Debug for PreparedGraphPattern<Row> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphPattern")
            .field("variables", &self.variable_count)
            .field("edges", &self.edge_count)
            .field("columns", &self.columns.len())
            .finish()
    }
}
impl<Row> PreparedGraphPattern<Row> {
    #[must_use]
    pub fn plan(&self) -> &GlaPlan<Row> {
        &self.logical
    }
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        self.logical.canonical_bytes()
    }
    /// Output order. A row's `get(i)` refers to `columns()[i]`.
    #[must_use]
    pub fn columns(&self) -> &[String] {
        &self.columns
    }
    #[must_use]
    pub fn required_vertex_label(&self) -> Option<LabelId> {
        if self.logical.scans_edges()
            || self
                .logical
                .operators()
                .iter()
                .skip(1)
                .any(|op| matches!(op, GlaOperator::ScanVertices))
        {
            // The shared vertex domain serves every independent component.
            // A root label cannot narrow admission or phantom observations for
            // another component, including an OPTIONAL/EXISTS child scan.
            return None;
        }
        self.logical.operators().iter().find_map(|op| match op {
            GlaOperator::Select { slot, predicates } if slot.ordinal() == 0 => {
                predicates.iter().find_map(|p| match p {
                    VertexPredicate::HasLabel(label) => Some(*label),
                    _ => None,
                })
            }
            _ => None,
        })
    }
}

fn check_next(
    count: usize,
    limit: usize,
    dimension: PatternLimitDimension,
) -> Result<(), PatternBuildError> {
    if count >= limit {
        return Err(PatternBuildError::LimitExceeded {
            dimension,
            limit,
            observed: count + 1,
        });
    }
    Ok(())
}
fn reverse(direction: GlaDirection) -> GlaDirection {
    match direction {
        GlaDirection::Forward => GlaDirection::Reverse,
        GlaDirection::Reverse => GlaDirection::Forward,
        GlaDirection::Undirected => GlaDirection::Undirected,
    }
}

/// Width of a positive compiled body, including temporary closing endpoints.
/// Count producers, not edges: each independent component has its own root.
fn binding_width(operators: &[GlaOperator]) -> u32 {
    operators
        .iter()
        .map(|operator| match operator {
            GlaOperator::ScanEdges { .. } => 2,
            GlaOperator::ScanVertices
            | GlaOperator::Expand { .. }
            | GlaOperator::VarLengthExpand { .. }
            | GlaOperator::BindVertex { .. }
            | GlaOperator::BindOuterVertex { .. } => 1,
            _ => 0,
        })
        .sum()
}

impl GraphPatternBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Encode the resolved definition without compiling a scope or supplying
    /// parameter values. Declaration ordinals retain topology and correlations.
    pub(crate) fn canonical_template_bytes(&self) -> Vec<u8> {
        fn ordinal(bytes: &mut Vec<u8>, value: usize) {
            bytes.extend_from_slice(&(value as u64).to_be_bytes());
        }
        fn name(bytes: &mut Vec<u8>, value: &str) {
            ordinal(bytes, value.len());
            bytes.extend_from_slice(value.as_bytes());
        }
        let mut bytes = b"fgdb:gql:pattern-template:v1\0".to_vec();
        ordinal(&mut bytes, self.variables.len());
        for variable in &self.variables {
            name(&mut bytes, &variable.name);
            bytes.push(u8::from(variable.outer));
            ordinal(&mut bytes, variable.predicates.len());
            for predicate in &variable.predicates {
                match predicate {
                    VertexPredicate::HasLabel(label) => {
                        bytes.push(0);
                        bytes.extend_from_slice(&label.0.to_be_bytes());
                    }
                    VertexPredicate::IntegerProperty {
                        key,
                        comparison,
                        value,
                    } => {
                        bytes.push(1);
                        bytes.extend_from_slice(&key.0.to_be_bytes());
                        bytes.push(comparison.tag());
                        bytes.extend_from_slice(&value.to_be_bytes());
                    }
                    VertexPredicate::ScalarProperty { key, predicate } => {
                        bytes.push(2);
                        bytes.extend_from_slice(&key.0.to_be_bytes());
                        predicate.append_transcript(&mut bytes);
                    }
                    VertexPredicate::PropertyNull { key, is_null } => {
                        bytes.push(3);
                        bytes.extend_from_slice(&key.0.to_be_bytes());
                        bytes.push(u8::from(*is_null));
                    }
                }
            }
        }
        ordinal(&mut bytes, self.edges.len());
        for edge in &self.edges {
            ordinal(&mut bytes, edge.source);
            ordinal(&mut bytes, edge.destination);
            bytes.extend_from_slice(&edge.relation.0.to_be_bytes());
            bytes.push(super::direction_tag(edge.direction));
            bytes.push(u8::from(edge.walk.is_some()));
            if let Some(bounds) = edge.walk {
                bytes.extend_from_slice(&bounds.minimum().to_be_bytes());
                bytes.extend_from_slice(&bounds.maximum().to_be_bytes());
            }
            bytes.push(match edge.search {
                GraphWalkSearch::All => 0,
                GraphWalkSearch::AllShortest => 1,
                GraphWalkSearch::AnyShortest => 2,
                GraphWalkSearch::Acyclic => 3,
                GraphWalkSearch::Simple => 4,
                GraphWalkSearch::Trail => 5,
            });
        }
        ordinal(&mut bytes, self.identities.len());
        for identity in &self.identities {
            ordinal(&mut bytes, identity.left);
            ordinal(&mut bytes, identity.right);
            bytes.push(u8::from(identity.equal));
        }
        ordinal(&mut bytes, self.property_comparisons.len());
        for comparison in &self.property_comparisons {
            match comparison {
                PropertyComparison::Properties {
                    left,
                    left_key,
                    right,
                    right_key,
                    comparison,
                } => {
                    bytes.push(0);
                    ordinal(&mut bytes, *left);
                    bytes.extend_from_slice(&left_key.0.to_be_bytes());
                    ordinal(&mut bytes, *right);
                    bytes.extend_from_slice(&right_key.0.to_be_bytes());
                    bytes.push(comparison.tag());
                }
                PropertyComparison::Boolean(expression) => {
                    bytes.push(1);
                    let expression = expression.template_bytes();
                    ordinal(&mut bytes, expression.len());
                    bytes.extend_from_slice(&expression);
                }
            }
        }
        ordinal(&mut bytes, self.path_captures.len());
        for capture in &self.path_captures {
            name(&mut bytes, &capture.name);
            ordinal(&mut bytes, capture.start);
            ordinal(&mut bytes, capture.first_edge);
            ordinal(&mut bytes, capture.edge_count);
            bytes.push(u8::from(capture.edge_identity));
        }
        ordinal(&mut bytes, self.path_predicates.len());
        for predicate in &self.path_predicates {
            match predicate {
                PathPredicate::Length {
                    capture,
                    comparison,
                    value,
                } => {
                    bytes.push(0);
                    bytes.extend_from_slice(&capture.to_be_bytes());
                    bytes.push(comparison.tag());
                    bytes.extend_from_slice(&value.to_be_bytes());
                }
                PathPredicate::Null {
                    capture,
                    function,
                    is_null,
                } => {
                    bytes.push(1);
                    bytes.extend_from_slice(&capture.to_be_bytes());
                    bytes.push(*function as u8);
                    bytes.push(u8::from(*is_null));
                }
            }
        }
        bytes
    }

    /// Logical template operators in declaration order, before scope lowering.
    pub(crate) fn template_operators(&self) -> Vec<&'static str> {
        let mut operators = Vec::new();
        for variable in &self.variables {
            operators.push(if variable.outer {
                "BindOuterVertex"
            } else {
                "ScanVertices"
            });
            if !variable.predicates.is_empty() {
                operators.push("Select");
            }
        }
        for edge in &self.edges {
            operators.push(if edge.walk.is_some() {
                "VarLengthExpand"
            } else {
                "Expand"
            });
        }
        for _ in &self.identities {
            operators.push("VertexIdentity");
        }
        for comparison in &self.property_comparisons {
            operators.push(match comparison {
                PropertyComparison::Properties { .. } => "CompareProperties",
                PropertyComparison::Boolean(_) => "SelectBoolean",
            });
        }
        for _ in &self.path_captures {
            operators.push("CapturePath");
        }
        for predicate in &self.path_predicates {
            operators.push(match predicate {
                PathPredicate::Length { .. } => "SelectPathLength",
                PathPredicate::Null { .. } => "SelectPathNull",
            });
        }
        operators
    }
    fn variable(&self, name: &str) -> Result<usize, PatternBuildError> {
        self.variables
            .iter()
            .position(|var| var.name == name)
            .ok_or(PatternBuildError::UnknownVariable)
    }
    pub fn vertex(&mut self, name: &str) -> Result<&mut Self, PatternBuildError> {
        let bytes = name.as_bytes();
        if bytes.is_empty()
            || bytes.len() > MAX_PATTERN_NAME_BYTES
            || !(bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
            || !bytes
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        {
            return Err(PatternBuildError::InvalidVariableName);
        }
        if self.variables.iter().any(|var| var.name == name)
            || self
                .path_captures
                .iter()
                .any(|capture| capture.name == name)
        {
            return Err(PatternBuildError::DuplicateVariable);
        }
        check_next(
            self.variables.len(),
            MAX_PATTERN_VERTICES,
            PatternLimitDimension::Vertices,
        )?;
        self.variables.push(Variable {
            name: name.to_owned(),
            predicates: Vec::new(),
            outer: false,
        });
        Ok(self)
    }

    /// Capture a visible outer vertex VALUE for a clause's predicates. Unlike
    /// vertex(), this does not require a positive node match and preserves an
    /// outer null for IS NULL and three-valued Boolean expressions. It may not
    /// occur in an edge atom. The containing required/optional/existential
    /// clause resolves it by name; standalone preparation refuses captures.
    /// At least one ordinary pattern vertex is still required in the child.
    /// Captures consume the ordinary variable and definition-wide frame caps.
    pub fn outer_vertex(&mut self, name: &str) -> Result<&mut Self, PatternBuildError> {
        self.vertex(name)?;
        self.variables
            .last_mut()
            .expect("one validated variable was added")
            .outer = true;
        Ok(self)
    }

    /// Capture the current complete root chain in edge declaration order.
    /// Later edges do not extend an existing capture. Captures are value-only
    /// root bindings; scoped children cannot declare them.
    pub fn capture_path(&mut self, name: &str) -> Result<&mut Self, PatternBuildError> {
        // Validate the same identifier namespace as vertex().
        let bytes = name.as_bytes();
        if bytes.is_empty()
            || bytes.len() > MAX_PATTERN_NAME_BYTES
            || !(bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
            || !bytes
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        {
            return Err(PatternBuildError::InvalidVariableName);
        }
        if self.variables.iter().any(|variable| variable.name == name)
            || self
                .path_captures
                .iter()
                .any(|capture| capture.name == name)
        {
            return Err(PatternBuildError::DuplicateVariable);
        }
        check_next(
            self.path_captures.len(),
            MAX_PATTERN_IDENTITIES,
            PatternLimitDimension::PathCaptures,
        )?;
        if self.variables.is_empty() {
            return Err(PatternBuildError::EmptyPattern);
        }
        if self.variables.iter().any(|variable| variable.outer)
            || (self.edges.len() > 1 && self.edges.iter().any(|edge| edge.walk.is_some()))
        {
            return Err(PatternBuildError::InvalidPathCapture);
        }
        let start = self.edges.first().map_or(0, |edge| edge.source);
        let mut visited = vec![false; self.variables.len()];
        visited[start] = true;
        let mut endpoint = start;
        for edge in &self.edges {
            endpoint = if edge.source == endpoint {
                edge.destination
            } else if edge.destination == endpoint {
                edge.source
            } else {
                return Err(PatternBuildError::InvalidPathCapture);
            };
            if visited[endpoint] && self.edges.len() > 1 {
                return Err(PatternBuildError::InvalidPathCapture);
            }
            visited[endpoint] = true;
        }
        if visited.iter().any(|visited| !visited) {
            return Err(PatternBuildError::InvalidPathCapture);
        }
        self.path_captures.push(PathCapture {
            name: name.to_owned(),
            start,
            first_edge: 0,
            edge_count: self.edges.len(),
            edge_identity: false,
        });
        Ok(self)
    }

    /// Capture one declared, fixed-length relationship using the same admitted
    /// traversal segment as path values. No endpoint-to-edge reconstruction.
    pub fn capture_edge(
        &mut self,
        name: &str,
        edge: usize,
    ) -> Result<&mut Self, PatternBuildError> {
        let definition = self
            .edges
            .get(edge)
            .ok_or(PatternBuildError::InvalidPathCapture)?;
        if definition.walk.is_some() {
            return Err(PatternBuildError::InvalidPathCapture);
        }
        let start = definition.source;
        let bytes = name.as_bytes();
        if bytes.is_empty()
            || bytes.len() > MAX_PATTERN_NAME_BYTES
            || !(bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
            || !bytes
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        {
            return Err(PatternBuildError::InvalidVariableName);
        }
        if self.variables.iter().any(|variable| variable.name == name)
            || self
                .path_captures
                .iter()
                .any(|capture| capture.name == name)
        {
            return Err(PatternBuildError::DuplicateVariable);
        }
        check_next(
            self.path_captures.len(),
            MAX_PATTERN_IDENTITIES,
            PatternLimitDimension::PathCaptures,
        )?;
        self.path_captures.push(PathCapture {
            name: name.to_owned(),
            start,
            first_edge: edge,
            edge_count: 1,
            edge_identity: true,
        });
        Ok(self)
    }

    fn path_capture(&self, name: &str) -> Result<usize, PatternBuildError> {
        self.path_captures
            .iter()
            .position(|capture| capture.name == name)
            .ok_or(PatternBuildError::UnknownVariable)
    }

    pub fn filter_path_length(
        &mut self,
        variable: &str,
        comparison: IntegerComparison,
        value: i64,
    ) -> Result<&mut Self, PatternBuildError> {
        let capture = self.path_capture(variable)? as u32;
        check_next(
            self.predicate_count,
            MAX_PATTERN_PREDICATES,
            PatternLimitDimension::Predicates,
        )?;
        self.path_predicates.push(PathPredicate::Length {
            capture,
            comparison,
            value,
        });
        self.predicate_count += 1;
        Ok(self)
    }

    pub fn filter_path_null(
        &mut self,
        variable: &str,
        function: GraphPathFunction,
        is_null: bool,
    ) -> Result<&mut Self, PatternBuildError> {
        let capture = self.path_capture(variable)? as u32;
        check_next(
            self.predicate_count,
            MAX_PATTERN_PREDICATES,
            PatternLimitDimension::Predicates,
        )?;
        self.path_predicates.push(PathPredicate::Null {
            capture,
            function,
            is_null,
        });
        self.predicate_count += 1;
        Ok(self)
    }

    pub fn filter(
        &mut self,
        variable: &str,
        predicate: VertexPredicate,
    ) -> Result<&mut Self, PatternBuildError> {
        let at = self.variable(variable)?;
        check_next(
            self.predicate_count,
            MAX_PATTERN_PREDICATES,
            PatternLimitDimension::Predicates,
        )?;
        self.variables[at].predicates.push(predicate);
        self.predicate_count += 1;
        Ok(self)
    }
    pub fn edge(
        &mut self,
        source: &str,
        relation: RelationId,
        direction: GlaDirection,
        destination: &str,
    ) -> Result<&mut Self, PatternBuildError> {
        let source = self.variable(source)?;
        let destination = self.variable(destination)?;
        if self.variables[source].outer || self.variables[destination].outer {
            return Err(PatternBuildError::OuterVertexInPattern);
        }
        check_next(
            self.edges.len(),
            MAX_PATTERN_EDGES,
            PatternLimitDimension::Edges,
        )?;
        self.edges.push(Edge {
            source,
            destination,
            relation,
            direction,
            walk: None,
            search: GraphWalkSearch::All,
        });
        Ok(self)
    }

    /// Add a finite WALK atom between declared endpoints. Every matching edge
    /// occurrence contributes, including repeated edges and vertices. Zero hops
    /// bind the destination to the source without inventing an edge. Predicates
    /// on the destination constrain endpoints, not intermediate vertices.
    ///
    /// The atom composes with fixed edges, cycles and correlated clauses through
    /// the same compiler. It consumes one definition edge slot, regardless of
    /// its checked hop bound. Runtime work/scratch policies govern enumeration.
    /// Direct execution must supply the complete admitted vertex iterator as
    /// well as topology; the database entrypoints select both automatically.
    pub fn walk(
        &mut self,
        source: &str,
        relation: RelationId,
        direction: GlaDirection,
        destination: &str,
        bounds: crate::GraphWalkBounds,
    ) -> Result<&mut Self, PatternBuildError> {
        self.edge(source, relation, direction, destination)?;
        self.edges
            .last_mut()
            .expect("one validated atom was just added")
            .walk = Some(bounds);
        Ok(self)
    }

    /// Add an ALL SHORTEST WALK atom. Each endpoint pair retains all tied
    /// minimum-hop occurrences WITHIN bounds, including parallel edges. This
    /// is a selector on this atom, not a shortest-total-length optimization of
    /// a surrounding multi-atom pattern. Destination predicates remain outside
    /// traversal, so rejecting an endpoint never removes a transit vertex.
    ///
    /// The existing breadth-first cursor executes the atom under the same
    /// work/scratch controls, slot mapping and snapshot admission as WALK.
    /// Capturing this atom additionally exposes its selected traversal as a path.
    pub fn shortest_walk(
        &mut self,
        source: &str,
        relation: RelationId,
        direction: GlaDirection,
        destination: &str,
        bounds: crate::GraphWalkBounds,
    ) -> Result<&mut Self, PatternBuildError> {
        self.walk(source, relation, direction, destination, bounds)?;
        self.edges
            .last_mut()
            .expect("one validated WALK atom was just added")
            .search = GraphWalkSearch::AllShortest;
        Ok(self)
    }

    /// Add an ANY SHORTEST WALK atom: one endpoint occurrence per pair within
    /// the finite interval. Equal-depth prefixes coalesce before expansion, so
    /// this does not enumerate every tied route before deduplicating output.
    /// Each incoming binding occurrence executes independently. Endpoint
    /// predicates, scopes and outer multiplicities retain their ordinary laws.
    /// Like shortest_walk, this is a per-atom selector; capture_path retains its route.
    pub fn any_shortest_walk(
        &mut self,
        source: &str,
        relation: RelationId,
        direction: GlaDirection,
        destination: &str,
        bounds: crate::GraphWalkBounds,
    ) -> Result<&mut Self, PatternBuildError> {
        self.walk(source, relation, direction, destination, bounds)?;
        self.edges
            .last_mut()
            .expect("one validated WALK atom was just added")
            .search = GraphWalkSearch::AnyShortest;
        Ok(self)
    }

    /// Add a finite ACYCLIC atom. Every vertex in this atom's route is unique;
    /// independent routes and parallel edges retain occurrence multiplicity.
    /// Membership checks precede frontier growth. Endpoint predicates, scopes
    /// and capture_path use the ordinary compiler and admitted source.
    /// The restriction is local to this atom, not to a compound pattern.
    pub fn acyclic_walk(
        &mut self,
        source: &str,
        relation: RelationId,
        direction: GlaDirection,
        destination: &str,
        bounds: crate::GraphWalkBounds,
    ) -> Result<&mut Self, PatternBuildError> {
        self.walk(source, relation, direction, destination, bounds)?;
        self.edges
            .last_mut()
            .expect("one validated atom was added")
            .search = GraphWalkSearch::Acyclic;
        Ok(self)
    }

    /// Add a finite SIMPLE atom. Only its first and last vertices may coincide;
    /// a closing return is terminal. This is not edge-unique TRAIL matching or
    /// shortest-path selection. Captures preserve the actual edge identities.
    pub fn simple_walk(
        &mut self,
        source: &str,
        relation: RelationId,
        direction: GlaDirection,
        destination: &str,
        bounds: crate::GraphWalkBounds,
    ) -> Result<&mut Self, PatternBuildError> {
        self.walk(source, relation, direction, destination, bounds)?;
        self.edges
            .last_mut()
            .expect("one validated atom was added")
            .search = GraphWalkSearch::Simple;
        Ok(self)
    }

    pub fn identity(
        &mut self,
        left: &str,
        right: &str,
        equal: bool,
    ) -> Result<&mut Self, PatternBuildError> {
        let left = self.variable(left)?;
        let right = self.variable(right)?;
        check_next(
            self.identities.len(),
            MAX_PATTERN_IDENTITIES,
            PatternLimitDimension::Identities,
        )?;
        self.identities.push(Identity { left, right, equal });
        Ok(self)
    }
    fn select(&self, variable: usize, slot: BindingSlot, operators: &mut Vec<GlaOperator>) {
        if !self.variables[variable].predicates.is_empty() {
            operators.push(GlaOperator::Select {
                slot,
                predicates: self.variables[variable].predicates.clone(),
            });
        }
    }
    fn identities(
        &self,
        slots: &[Option<BindingSlot>],
        emitted: &mut [bool],
        operators: &mut Vec<GlaOperator>,
    ) {
        for (at, identity) in self.identities.iter().enumerate() {
            if !emitted[at]
                && let (Some(left), Some(right)) = (slots[identity.left], slots[identity.right])
            {
                operators.push(GlaOperator::VertexIdentity {
                    left,
                    right,
                    equal: identity.equal,
                });
                emitted[at] = true;
            }
        }
    }

    /// Retains the original sorted/distinct single-column result contract.
    pub fn prepare(
        &self,
        projection: &str,
        offset: u64,
        count: Option<u64>,
    ) -> Result<PreparedGraphPattern, PatternBuildError> {
        if self.variables.is_empty() {
            return Err(PatternBuildError::EmptyPattern);
        }
        let projection_at = self.variable(projection)?;
        self.require_identity_projection()?;
        let (mut operators, slots) = self.compile()?;
        operators.extend([
            GlaOperator::Project {
                slot: slots[projection_at],
            },
            GlaOperator::Distinct,
            GlaOperator::OrderByVertexId,
            GlaOperator::Limit { offset, count },
        ]);
        Ok(PreparedGraphPattern {
            logical: GlaPlan::from_operators(operators),
            variable_count: self.variables.len(),
            edge_count: self.edges.len(),
            columns: vec![projection.to_owned()],
        })
    }

    /// Prepare 1..=65 unique named columns from each complete matching binding.
    /// Tuple-wide DISTINCT and lexicographic order precede pagination. Every
    /// row is one complete assignment, not a zip of independent projections.
    pub fn prepare_bindings(
        &self,
        projections: &[&str],
        offset: u64,
        count: Option<u64>,
    ) -> Result<PreparedGraphPattern<GraphBindingRow>, PatternBuildError> {
        if self.variables.is_empty() {
            return Err(PatternBuildError::EmptyPattern);
        }
        if projections.is_empty() {
            return Err(PatternBuildError::EmptyProjection);
        }
        if projections.len() > MAX_PATTERN_VERTICES {
            return Err(PatternBuildError::LimitExceeded {
                dimension: PatternLimitDimension::Columns,
                limit: MAX_PATTERN_VERTICES,
                observed: projections.len(),
            });
        }
        let mut projected = Vec::new();
        for name in projections {
            let at = self.variable(name)?;
            if projected.contains(&at) {
                return Err(PatternBuildError::DuplicateProjection);
            }
            projected.push(at);
        }
        self.require_identity_projection()?;
        let (mut operators, slots) = self.compile()?;
        operators.extend([
            GlaOperator::ProjectBindings {
                slots: projected.into_iter().map(|at| slots[at]).collect(),
            },
            GlaOperator::Distinct,
            GlaOperator::OrderByBindings,
            GlaOperator::Limit { offset, count },
        ]);
        Ok(PreparedGraphPattern {
            logical: GlaPlan::from_operators(operators),
            variable_count: self.variables.len(),
            edge_count: self.edges.len(),
            columns: projections.iter().map(|name| (*name).to_owned()).collect(),
        })
    }

    /// Preserve connected traversal; seed each independent component only when
    /// no remaining edge can extend the bound frontier. Predicates and identity
    /// constraints stay with their binding, and all outputs share the slot map.
    fn compile(&self) -> Result<(Vec<GlaOperator>, Vec<BindingSlot>), PatternBuildError> {
        if self.variables.iter().any(|variable| variable.outer) {
            return Err(PatternBuildError::OuterVertexRequiresScope);
        }
        self.compile_with_root(None)
    }

    /// A correlated isolated vertex can anchor a child without rewriting the
    /// child's names, property expressions, identity constraints or edge IDs.
    /// Only the scope compiler may resolve outer operands in this private body.
    fn compile_with_root(
        &self,
        root: Option<usize>,
    ) -> Result<(Vec<GlaOperator>, Vec<BindingSlot>), PatternBuildError> {
        let first_local = self
            .variables
            .iter()
            .position(|variable| !variable.outer)
            .ok_or(PatternBuildError::EmptyPattern)?;
        let mut slots = vec![None; self.variables.len()];
        let mut emitted = vec![false; self.identities.len()];
        let mut consumed = vec![false; self.edges.len()];
        let mut path_segments = if self.path_captures.is_empty() {
            Vec::new()
        } else {
            vec![None; self.edges.len()]
        };
        let mut edge_starts = if self.path_captures.is_empty() {
            Vec::new()
        } else {
            vec![None; self.edges.len()]
        };
        let mut operators = Vec::new();
        let mut next_slot;
        if root.is_some() || self.edges.is_empty() {
            let root = root.unwrap_or(first_local);
            operators.push(GlaOperator::ScanVertices);
            slots[root] = Some(BindingSlot(0));
            self.identities(&slots, &mut emitted, &mut operators);
            self.select(root, BindingSlot(0), &mut operators);
            next_slot = 1;
        } else {
            let first = self.edges[0];
            if first.walk.is_some() {
                operators.push(GlaOperator::ScanVertices);
                operators.push(first.expansion(BindingSlot(0), first.direction));
            } else {
                operators.push(GlaOperator::ScanEdges {
                    relation: first.relation,
                    direction: first.direction,
                });
            }
            slots[first.source] = Some(BindingSlot(0));
            if first.source == first.destination {
                operators.push(GlaOperator::VertexIdentity {
                    left: BindingSlot(0),
                    right: BindingSlot(1),
                    equal: true,
                });
            } else {
                slots[first.destination] = Some(BindingSlot(1));
            }
            self.identities(&slots, &mut emitted, &mut operators);
            self.select(first.source, BindingSlot(0), &mut operators);
            if first.source != first.destination {
                self.select(first.destination, BindingSlot(1), &mut operators);
            }
            consumed[0] = true;
            if !path_segments.is_empty() {
                path_segments[0] = Some(BindingSlot(1));
                edge_starts[0] = Some(BindingSlot(0));
            }
            next_slot = 2;
        }
        loop {
            let connected = self.edges.iter().enumerate().position(|(at, edge)| {
                !consumed[at] && (slots[edge.source].is_some() || slots[edge.destination].is_some())
            });
            if let Some(at) = connected {
                let edge = self.edges[at];
                let (source, target, direction) = if let Some(source) = slots[edge.source] {
                    (source, edge.destination, edge.direction)
                } else {
                    (
                        slots[edge.destination].expect("a bound endpoint was found"),
                        edge.source,
                        reverse(edge.direction),
                    )
                };
                let appended = BindingSlot(next_slot);
                next_slot += 1;
                operators.push(edge.expansion(source, direction));
                if !path_segments.is_empty() {
                    path_segments[at] = Some(appended);
                    edge_starts[at] = Some(source);
                }
                let previous = slots[target];
                if let Some(representative) = previous {
                    operators.push(GlaOperator::VertexIdentity {
                        left: representative,
                        right: appended,
                        equal: true,
                    });
                } else {
                    slots[target] = Some(appended);
                }
                self.identities(&slots, &mut emitted, &mut operators);
                if previous.is_none() {
                    self.select(target, appended, &mut operators);
                }
                consumed[at] = true;
                continue;
            }
            // Prefer a remaining edge component before isolated vertices. This
            // is a deterministic definition order, not data-dependent planning.
            let seed = self
                .edges
                .iter()
                .enumerate()
                .find(|(at, _)| !consumed[*at])
                .map(|(_, edge)| edge.source)
                .or_else(|| slots.iter().position(Option::is_none));
            let Some(seed) = seed else {
                break;
            };
            debug_assert!(slots[seed].is_none());
            let appended = BindingSlot(next_slot);
            next_slot += 1;
            operators.push(GlaOperator::ScanVertices);
            slots[seed] = Some(appended);
            self.identities(&slots, &mut emitted, &mut operators);
            self.select(seed, appended, &mut operators);
        }
        debug_assert!(consumed.iter().all(|consumed| *consumed));
        debug_assert!(emitted.iter().all(|emitted| *emitted));
        debug_assert_eq!(next_slot, binding_width(&operators));
        let slots: Vec<_> = slots
            .into_iter()
            .map(|slot| slot.expect("every component has been bound"))
            .collect();
        for (capture, definition) in self.path_captures.iter().enumerate() {
            operators.push(GlaOperator::CapturePath {
                capture: capture as u32,
                start: if definition.edge_identity {
                    edge_starts[definition.first_edge].expect("every captured edge has a source")
                } else {
                    slots[definition.start]
                },
                segments: path_segments
                    [definition.first_edge..definition.first_edge + definition.edge_count]
                    .iter()
                    .map(|slot| slot.expect("every captured edge has been bound"))
                    .collect(),
            });
        }
        for predicate in &self.path_predicates {
            operators.push(match *predicate {
                PathPredicate::Length {
                    capture,
                    comparison,
                    value,
                } => GlaOperator::SelectPathLength {
                    capture,
                    comparison,
                    value,
                },
                PathPredicate::Null {
                    capture,
                    function,
                    is_null,
                } => GlaOperator::SelectPathNull {
                    capture,
                    function,
                    is_null,
                },
            });
        }
        self.emit_property_comparisons(&slots, &mut operators);
        Ok((operators, slots))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GqlQueryError, GqlQueryPolicy};
    use std::collections::BTreeSet;

    fn builder(names: &[&str]) -> GraphPatternBuilder {
        let mut builder = GraphPatternBuilder::new();
        for name in names {
            builder.vertex(name).unwrap();
        }
        builder
    }

    #[test]
    fn template_preserves_resolved_symbols_topology_and_literals() {
        let mut original = builder(&["a", "b"]);
        original
            .edge("a", RelationId(1), GlaDirection::Forward, "b")
            .unwrap();
        let baseline = original.canonical_template_bytes();
        assert_eq!(baseline, original.clone().canonical_template_bytes());
        let mut changed = original.clone();
        changed.edges[0].relation = RelationId(2);
        assert_ne!(baseline, changed.canonical_template_bytes());
        changed = original.clone();
        changed.edges[0].destination = 0;
        assert_ne!(baseline, changed.canonical_template_bytes());
        changed = original.clone();
        changed.variables[0].outer = true;
        assert_ne!(baseline, changed.canonical_template_bytes());
        assert!(changed.template_operators().contains(&"BindOuterVertex"));
        original.path_captures.push(PathCapture {
            name: "route".into(),
            start: 0,
            first_edge: 0,
            edge_count: 1,
            edge_identity: false,
        });
        original.path_predicates.push(PathPredicate::Length {
            capture: 0,
            comparison: IntegerComparison::Equal,
            value: 2,
        });
        changed = original.clone();
        changed.path_predicates[0] = PathPredicate::Length {
            capture: 0,
            comparison: IntegerComparison::Equal,
            value: 3,
        };
        assert_ne!(
            original.canonical_template_bytes(),
            changed.canonical_template_bytes()
        );
        assert!(original.template_operators().contains(&"CapturePath"));
        assert!(original.template_operators().contains(&"SelectPathLength"));
    }

    #[test]
    fn long_paths_branching_cycles_and_reversed_binding_use_existing_operators() {
        let mut b = builder(&["a", "b", "c", "d"]);
        b.edge("a", RelationId(1), GlaDirection::Forward, "b")
            .unwrap();
        b.edge("c", RelationId(2), GlaDirection::Reverse, "d")
            .unwrap();
        b.edge("c", RelationId(1), GlaDirection::Reverse, "b")
            .unwrap();
        b.edge("d", RelationId(2), GlaDirection::Forward, "a")
            .unwrap();
        b.edge("a", RelationId(1), GlaDirection::Forward, "c")
            .unwrap();
        b.identity("a", "d", false).unwrap();
        let query = b.prepare("d", 0, None).unwrap();
        let edges = [
            (VId(1), RelationId(1), VId(2)),
            (VId(2), RelationId(1), VId(3)),
            (VId(4), RelationId(2), VId(3)),
            (VId(4), RelationId(2), VId(1)),
            (VId(1), RelationId(1), VId(3)),
        ];
        assert_eq!(
            query
                .plan()
                .execute([], edges, |_, _| Ok::<_, ()>(true))
                .unwrap(),
            vec![VId(4)]
        );
        assert_eq!(
            query
                .plan()
                .operators()
                .iter()
                .filter(|op| matches!(op, GlaOperator::Expand { .. }))
                .count(),
            4
        );
    }

    // Complete assignment oracle; no GLA slots, indexes or scheduling code.
    fn assignments(b: &GraphPatternBuilder, edges: &[(VId, RelationId, VId)]) -> Vec<Vec<VId>> {
        let mut result = Vec::new();
        for bits in 0..(1_usize << b.variables.len()) {
            let assignment: Vec<_> = (0..b.variables.len())
                .map(|i| VId(1 + ((bits >> i) & 1) as u128))
                .collect();
            let identities = b
                .identities
                .iter()
                .all(|test| (assignment[test.left] == assignment[test.right]) == test.equal);
            let matches = b.edges.iter().all(|atom| {
                edges.iter().any(|&(s, r, d)| {
                    if r != atom.relation {
                        return false;
                    }
                    let left = assignment[atom.source];
                    let right = assignment[atom.destination];
                    match atom.direction {
                        GlaDirection::Forward => s == left && d == right,
                        GlaDirection::Reverse => d == left && s == right,
                        GlaDirection::Undirected => {
                            (s == left && d == right) || (s == right && d == left)
                        }
                    }
                })
            });
            if identities && matches {
                result.push(assignment);
            }
        }
        result
    }
    fn oracle(
        b: &GraphPatternBuilder,
        edges: &[(VId, RelationId, VId)],
        projected: usize,
    ) -> Vec<VId> {
        assignments(b, edges)
            .into_iter()
            .map(|row| row[projected])
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    #[test]
    fn connected_conjunctions_match_exhaustive_assignment_oracle() {
        let universe: Vec<_> = [RelationId(1), RelationId(2)]
            .into_iter()
            .flat_map(|r| {
                [VId(1), VId(2)]
                    .into_iter()
                    .flat_map(move |s| [VId(1), VId(2)].into_iter().map(move |d| (s, r, d)))
            })
            .collect();
        for mask in 0..256_usize {
            let mut edges: Vec<_> = universe
                .iter()
                .enumerate()
                .filter(|(i, _)| mask & (1 << i) != 0)
                .map(|(_, e)| *e)
                .collect();
            if let Some(first) = edges.first().copied() {
                edges.push(first);
            }
            for direction in [
                GlaDirection::Forward,
                GlaDirection::Reverse,
                GlaDirection::Undirected,
            ] {
                for shape in 0..3 {
                    let mut b = builder(&["a", "b", "c", "d"]);
                    b.edge("a", RelationId(1), direction, "b").unwrap();
                    b.edge("c", RelationId(2), direction, "d").unwrap();
                    b.edge("b", RelationId(1), GlaDirection::Reverse, "c")
                        .unwrap();
                    if shape >= 1 {
                        b.edge("d", RelationId(2), direction, "a").unwrap();
                    }
                    if shape == 2 {
                        b.identity("a", "c", false).unwrap();
                        b.edge("b", RelationId(1), direction, "b").unwrap();
                    }
                    for (projected, name) in ["a", "b", "c", "d"].into_iter().enumerate() {
                        let query = b.prepare(name, 0, None).unwrap();
                        let actual = query
                            .plan()
                            .execute([], edges.iter().copied(), |_, _| Ok::<_, ()>(true))
                            .unwrap();
                        assert_eq!(
                            actual,
                            oracle(&b, &edges, projected),
                            "mask={mask}, shape={shape}, direction={direction:?}"
                        );
                    }
                    // Tuple verification uses the same independent assignments,
                    // not a zip of the scalar oracle's independent projections.
                    for columns in [
                        vec!["a", "c"],
                        vec!["d", "b", "a"],
                        vec!["a", "b", "c", "d"],
                    ] {
                        let positions: Vec<_> = columns
                            .iter()
                            .map(|name| b.variable(name).unwrap())
                            .collect();
                        let expected: Vec<_> = assignments(&b, &edges)
                            .into_iter()
                            .map(|row| positions.iter().map(|at| row[*at]).collect::<Vec<_>>())
                            .collect::<BTreeSet<_>>()
                            .into_iter()
                            .skip(1)
                            .take(3)
                            .collect();
                        let query = b.prepare_bindings(&columns, 1, Some(3)).unwrap();
                        let actual = query
                            .plan()
                            .execute([], edges.iter().copied(), |_, _| Ok::<_, ()>(true))
                            .unwrap();
                        assert_eq!(
                            actual
                                .iter()
                                .map(|row| row.values().to_vec())
                                .collect::<Vec<_>>(),
                            expected,
                            "tuple mask={mask}, shape={shape}, direction={direction:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn validation_is_bounded_and_rejected_edits_leave_the_builder_unchanged() {
        let mut b = builder(&["a"]);
        let original = b.prepare("a", 0, None).unwrap();
        assert_eq!(
            b.vertex("a").unwrap_err(),
            PatternBuildError::DuplicateVariable
        );
        assert_eq!(
            b.vertex("$bad").unwrap_err(),
            PatternBuildError::InvalidVariableName
        );
        assert_eq!(
            b.edge("a", RelationId(1), GlaDirection::Forward, "missing")
                .unwrap_err(),
            PatternBuildError::UnknownVariable
        );
        assert_eq!(b.prepare("a", 0, None).unwrap(), original);
        let independent = builder(&["a", "b"]).prepare("a", 0, None).unwrap();
        assert_eq!(
            independent
                .plan()
                .execute([VId(1), VId(2)], [], |_, _| Ok::<_, ()>(true))
                .unwrap(),
            vec![VId(1), VId(2)]
        );
        assert_eq!(
            GraphPatternBuilder::new().prepare("a", 0, None),
            Err(PatternBuildError::EmptyPattern)
        );
        for _ in 0..MAX_PATTERN_EDGES {
            b.edge("a", RelationId(1), GlaDirection::Forward, "a")
                .unwrap();
        }
        let before = b.prepare("a", 0, None).unwrap();
        assert!(matches!(
            b.edge("a", RelationId(1), GlaDirection::Forward, "a"),
            Err(PatternBuildError::LimitExceeded {
                dimension: PatternLimitDimension::Edges,
                ..
            })
        ));
        assert_eq!(b.prepare("a", 0, None).unwrap(), before);
    }

    #[test]
    fn maximum_length_path_and_node_scan_keep_exact_ordered_pagination() {
        let mut b = GraphPatternBuilder::new();
        for i in 0..=MAX_PATTERN_EDGES {
            b.vertex(&format!("n{i}")).unwrap();
        }
        for i in 0..MAX_PATTERN_EDGES {
            b.edge(
                &format!("n{i}"),
                RelationId(1),
                GlaDirection::Forward,
                &format!("n{}", i + 1),
            )
            .unwrap();
        }
        let query = b
            .prepare(&format!("n{MAX_PATTERN_EDGES}"), 0, None)
            .unwrap();
        let edges =
            (0..MAX_PATTERN_EDGES).map(|i| (VId(i as u128), RelationId(1), VId(i as u128 + 1)));
        assert_eq!(
            query
                .plan()
                .execute([], edges, |_, _| Ok::<_, ()>(true))
                .unwrap(),
            vec![VId(MAX_PATTERN_EDGES as u128)]
        );
        let b = builder(&["a"]);
        let query = b.prepare("a", 1, Some(1)).unwrap();
        assert_eq!(
            query
                .plan()
                .execute([VId(3), VId(1), VId(1), VId(2)], [], |_, _| Ok::<_, ()>(
                    true
                ))
                .unwrap(),
            vec![VId(2)]
        );
        assert_eq!(query.required_vertex_label(), None);
    }

    #[test]
    fn renamed_patterns_preserve_transcripts_but_identity_and_filters_do_not() {
        let make = |names: [&str; 3]| {
            let mut b = builder(&names);
            b.edge(names[0], RelationId(1), GlaDirection::Forward, names[1])
                .unwrap();
            b.edge(names[1], RelationId(2), GlaDirection::Reverse, names[2])
                .unwrap();
            b.edge(names[2], RelationId(1), GlaDirection::Undirected, names[0])
                .unwrap();
            b.prepare(names[2], 0, None).unwrap()
        };
        assert_eq!(
            make(["a", "b", "c"]).canonical_bytes(),
            make(["x", "y", "z"]).canonical_bytes()
        );
        let mut b = builder(&["sensitive_name"]);
        let original = b.prepare("sensitive_name", 0, None).unwrap();
        b.filter("sensitive_name", VertexPredicate::HasLabel(LabelId(7)))
            .unwrap();
        let filtered = b.prepare("sensitive_name", 0, None).unwrap();
        assert_eq!(filtered.required_vertex_label(), Some(LabelId(7)));
        assert_ne!(filtered.canonical_bytes(), original.canonical_bytes());
        assert!(!format!("{b:?} {filtered:?}").contains("sensitive_name"));
    }

    #[test]
    fn general_patterns_share_governed_limits_and_every_interruption_checkpoint() {
        let mut b = builder(&["a", "b", "c", "d"]);
        for (s, d) in [("a", "b"), ("b", "c"), ("c", "d"), ("d", "a")] {
            b.edge(s, RelationId(1), GlaDirection::Undirected, d)
                .unwrap();
        }
        let query = b.prepare("d", 0, Some(1)).unwrap();
        let edges = [
            (VId(1), RelationId(1), VId(2)),
            (VId(2), RelationId(1), VId(3)),
        ];
        let wide = GqlQueryPolicy::new(2, 1, u64::MAX, u64::MAX);
        let mut calls = 0;
        let measured = query
            .plan()
            .execute_governed(
                2,
                [],
                edges,
                |_, _| Ok::<_, ()>(true),
                wide,
                || {
                    calls += 1;
                    Ok::<_, usize>(())
                },
            )
            .unwrap();
        assert_eq!(measured.value, vec![VId(1)]);
        for stop in 1..=calls {
            let mut at = 0;
            let result = query.plan().execute_governed(
                2,
                [],
                edges,
                |_, _| Ok::<_, ()>(true),
                wide,
                || {
                    at += 1;
                    if at == stop { Err(stop) } else { Ok(()) }
                },
            );
            assert!(matches!(result, Err(GqlQueryError::Interrupted(value)) if value == stop));
            assert_eq!(at, stop);
        }
        let exact = GqlQueryPolicy::new(
            2,
            1,
            measured.evaluator.work_units,
            measured.evaluator.scratch_entries,
        );
        assert_eq!(
            query
                .plan()
                .execute_governed(
                    2,
                    [],
                    edges,
                    |_, _| Ok::<_, ()>(true),
                    exact,
                    || Ok::<_, ()>(())
                )
                .unwrap(),
            measured
        );
        let short = GqlQueryPolicy::new(
            2,
            1,
            measured.evaluator.work_units - 1,
            measured.evaluator.scratch_entries,
        );
        assert!(matches!(
            query.plan().execute_governed(
                2,
                [],
                edges,
                |_, _| Ok::<_, ()>(true),
                short,
                || Ok::<_, ()>(())
            ),
            Err(GqlQueryError::Evaluator(_))
        ));
    }

    #[test]
    fn tuple_schema_and_column_order_are_immutable_and_validated() {
        let mut b = builder(&["a", "b"]);
        b.edge("a", RelationId(1), GlaDirection::Forward, "b")
            .unwrap();
        let tuple = b.prepare_bindings(&["b", "a"], 0, None).unwrap();
        assert_eq!(tuple.columns(), &["b".to_owned(), "a".to_owned()]);
        assert_ne!(
            tuple.canonical_bytes(),
            b.prepare_bindings(&["a", "b"], 0, None)
                .unwrap()
                .canonical_bytes()
        );
        assert_ne!(
            b.prepare("a", 0, None).unwrap().canonical_bytes(),
            b.prepare_bindings(&["a"], 0, None)
                .unwrap()
                .canonical_bytes()
        );
        assert_eq!(
            b.prepare_bindings(&[], 0, None).unwrap_err(),
            PatternBuildError::EmptyProjection
        );
        assert_eq!(
            b.prepare_bindings(&["a", "a"], 0, None).unwrap_err(),
            PatternBuildError::DuplicateProjection
        );
        assert_eq!(
            b.prepare_bindings(&["missing"], 0, None).unwrap_err(),
            PatternBuildError::UnknownVariable
        );
        assert!(matches!(
            b.prepare_bindings(&["a"; MAX_PATTERN_VERTICES + 1], 0, None),
            Err(PatternBuildError::LimitExceeded {
                dimension: PatternLimitDimension::Columns,
                ..
            })
        ));
        let frozen = tuple.canonical_bytes();
        b.identity("a", "b", false).unwrap();
        assert_eq!(tuple.canonical_bytes(), frozen);
    }

    #[test]
    fn all_sixty_five_columns_are_retained_without_a_lossy_scalar_conversion() {
        let names: Vec<_> = (0..=MAX_PATTERN_EDGES).map(|i| format!("n{i}")).collect();
        let refs: Vec<_> = names.iter().map(String::as_str).collect();
        let mut b = builder(&refs);
        for i in 0..MAX_PATTERN_EDGES {
            b.edge(refs[i], RelationId(1), GlaDirection::Forward, refs[i + 1])
                .unwrap();
        }
        let query = b.prepare_bindings(&refs, 0, None).unwrap();
        let rows = query
            .plan()
            .execute(
                [],
                (0..MAX_PATTERN_EDGES).map(|i| (VId(i as u128), RelationId(1), VId(i as u128 + 1))),
                |_, _| Ok::<_, ()>(true),
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].values(),
            &(0..=MAX_PATTERN_EDGES)
                .map(|i| VId(i as u128))
                .collect::<Vec<_>>()
        );
    }
}
