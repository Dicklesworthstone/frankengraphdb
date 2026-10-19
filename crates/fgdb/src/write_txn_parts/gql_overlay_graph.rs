mod query_source {
    use super::{BoundPlan, Database, PendingRow, TxnGqlError, WriteTxn, WriteTxnError};
    use crate::Snapshot;
    use crate::gql_exec::property_maps::{self, PropertyMaps};
    use crate::gql_exec::source::{self, SourceEvent};
    use asupersync::fs::Vfs;
    use fgdb_delta_types::{DeltaRow, ElementId, LabelId, PropertyKeyId, RelationId};
    use fgdb_gql::algebra::{
        EdgeRelation, GlaDirection, GlaIdentityOutput, GlaOperator, GlaOutput, GlaPlan,
        PreparedGraphPattern, VertexPredicate,
    };
    use fgdb_types::{CanonicalScalar, EId, VId};
    use std::collections::{BTreeMap, BTreeSet};

    include!("aggregate_queries.rs");
    include!("procedure_queries.rs");

    type EdgeTriple = (VId, RelationId, VId);
    type IdentifiedEdge = (EId, VId, RelationId, VId);
    struct VertexView<'a> {
        labels: &'a [LabelId],
        props: &'a [(PropertyKeyId, CanonicalScalar)],
        label_edits: BTreeMap<LabelId, bool>,
        property_edits: BTreeMap<PropertyKeyId, Option<&'a CanonicalScalar>>,
    }
    impl<'a> VertexView<'a> {
        fn new(labels: &'a [LabelId], props: &'a [(PropertyKeyId, CanonicalScalar)]) -> Self {
            Self {
                labels,
                props,
                label_edits: BTreeMap::new(),
                property_edits: BTreeMap::new(),
            }
        }
        fn property(&self, key: PropertyKeyId) -> Option<&CanonicalScalar> {
            if let Some(value) = self.property_edits.get(&key) {
                *value
            } else {
                self.props
                    .binary_search_by_key(&key, |(key, _)| *key)
                    .ok()
                    .map(|at| &self.props[at].1)
            }
        }
        fn effective_properties(&self) -> impl Iterator<Item = (PropertyKeyId, &CanonicalScalar)> {
            self.props
                .iter()
                .filter(move |(key, _)| !self.property_edits.contains_key(key))
                .map(|(key, value)| (*key, value))
                .chain(
                    self.property_edits
                        .iter()
                        .filter_map(|(key, value)| value.map(|value| (*key, value))),
                )
        }
        fn matches(&self, predicate: &VertexPredicate) -> bool {
            let label = if let VertexPredicate::HasLabel(label) = predicate {
                self.label_edits
                    .get(label)
                    .copied()
                    .unwrap_or_else(|| self.labels.binary_search(label).is_ok())
                    .then_some(*label)
            } else {
                None
            };
            let property = predicate
                .property_key()
                .and_then(|key| self.property(key).map(|value| (key, value)));
            predicate.matches_borrowed(label, property)
        }
        /// The basis labels with this transaction's staged label edits.
        fn effective_labels(&self) -> BTreeSet<LabelId> {
            let mut labels: BTreeSet<LabelId> = self.labels.iter().copied().collect();
            for (&label, &present) in &self.label_edits {
                if present {
                    labels.insert(label);
                } else {
                    labels.remove(&label);
                }
            }
            labels
        }
    }

    struct EdgeView<'a> {
        props: &'a [(PropertyKeyId, CanonicalScalar)],
        edits: BTreeMap<PropertyKeyId, Option<&'a CanonicalScalar>>,
    }
    impl<'a> EdgeView<'a> {
        fn new(props: &'a [(PropertyKeyId, CanonicalScalar)]) -> Self {
            Self {
                props,
                edits: BTreeMap::new(),
            }
        }
        fn property(&self, key: PropertyKeyId) -> Option<&CanonicalScalar> {
            if let Some(value) = self.edits.get(&key) {
                *value
            } else {
                self.props
                    .binary_search_by_key(&key, |(key, _)| *key)
                    .ok()
                    .map(|at| &self.props[at].1)
            }
        }
        fn effective_properties(&self) -> impl Iterator<Item = (PropertyKeyId, &CanonicalScalar)> {
            self.props
                .iter()
                .filter(move |(key, _)| !self.edits.contains_key(key))
                .map(|(key, value)| (*key, value))
                .chain(
                    self.edits
                        .iter()
                        .filter_map(|(key, value)| value.map(|value| (*key, value))),
                )
        }
    }

    type OverlayEdges<'a> = BTreeMap<EId, (VId, RelationId, VId, EdgeView<'a>)>;
    type StagedAdjacency<'a> =
        BTreeMap<(VId, bool, EId), (IdentifiedEdge, &'a [(PropertyKeyId, CanonicalScalar)])>;

    /// A conservative input closure, before applying the canonical effects.
    /// Retired edges may still be in this set; the ordinary overlay removes
    /// them below. Every staged creation that a binding can traverse is here.
    struct BoundOverlay<'a> {
        vertices: BTreeSet<VId>,
        edges: OverlayEdges<'a>,
        root_observations: Vec<usize>,
    }

    #[allow(clippy::too_many_arguments)]
    fn select_bound_edge<'a, E>(
        edge: IdentifiedEdge,
        props: &'a [(PropertyKeyId, CanonicalScalar)],
        near: VId,
        direction: GlaDirection,
        target: &mut BTreeSet<VId>,
        selected: &mut OverlayEdges<'a>,
        control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        let (eid, src, relation, dst) = edge;
        if let std::collections::btree_map::Entry::Vacant(slot) = selected.entry(eid) {
            control(SourceEvent::ScratchEntry)?;
            slot.insert((src, relation, dst, EdgeView::new(props)));
        }
        let far = match direction {
            GlaDirection::Forward => dst,
            GlaDirection::Reverse => src,
            GlaDirection::Undirected if src == near => dst,
            GlaDirection::Undirected => src,
        };
        if !target.contains(&far) {
            control(SourceEvent::ScratchEntry)?;
            target.insert(far);
        }
        Ok(())
    }

    /// Follow one typed step through both the exact-cut adjacency index and
    /// staged creations. The index is only a candidate source; the unchanged
    /// algebra still checks predicates, correlations and edge uniqueness.
    #[allow(clippy::too_many_arguments)]
    fn extend_bound<'a, E>(
        snapshot: &'a Snapshot,
        as_of: fgdb_types::CommitSeq,
        roots: &BTreeSet<VId>,
        relation: &EdgeRelation,
        direction: GlaDirection,
        staged: &StagedAdjacency<'a>,
        selected: &mut OverlayEdges<'a>,
        control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
    ) -> Result<BTreeSet<VId>, E> {
        let mut target = BTreeSet::new();
        for &near in roots {
            control(SourceEvent::Work)?;
            snapshot.adjacency_index().visit(
                &snapshot.blocks,
                near,
                direction,
                as_of,
                control,
                |entry, block, row, control| {
                    if relation.matches(entry.relation) {
                        select_bound_edge(
                            (entry.eid, entry.src, entry.relation, entry.dst),
                            source::edge_properties_at(&snapshot.block_props, block, row),
                            near,
                            direction,
                            &mut target,
                            selected,
                            control,
                        )?;
                    }
                    Ok(())
                },
            )?;
            for incoming in [false, true] {
                if matches!(direction, GlaDirection::Forward) && incoming
                    || matches!(direction, GlaDirection::Reverse) && !incoming
                {
                    continue;
                }
                for (_, &(edge, props)) in
                    staged.range((near, incoming, EId(0))..=(near, incoming, EId(u128::MAX)))
                {
                    control(SourceEvent::Work)?;
                    if relation.matches(edge.2) {
                        select_bound_edge(
                            edge,
                            props,
                            near,
                            direction,
                            &mut target,
                            selected,
                            control,
                        )?;
                    }
                }
            }
        }
        Ok(target)
    }

    fn indexed_predicates<E>(
        predicates: &[VertexPredicate],
        control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
    ) -> Result<bool, E> {
        use fgdb_gql::algebra::IntegerComparison;
        for predicate in predicates {
            control(SourceEvent::Work)?;
            let indexed = match predicate {
                VertexPredicate::IntegerProperty { comparison, .. } => {
                    *comparison != IntegerComparison::NotEqual
                }
                VertexPredicate::ScalarProperty { predicate, .. } => {
                    let comparison = predicate.comparison();
                    (comparison == IntegerComparison::Equal
                        && !matches!(predicate.value(), CanonicalScalar::Null))
                        || (comparison != IntegerComparison::NotEqual
                            && matches!(
                                predicate.value(),
                                CanonicalScalar::Int(_) | CanonicalScalar::Float(_)
                            ))
                }
                _ => false,
            };
            if indexed {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Per-execution label and type names for `labels(n)` and `type(r)`. An
    /// element whose id the plan's catalog cannot name keeps that id, and
    /// refuses only when its own labels or type are read: a staged label on
    /// a vertex the statement never returns must not refuse the statement.
    pub(super) struct CatalogNames {
        labels: BTreeMap<VId, Result<Vec<fgdb_gql::algebra::GraphValue>, LabelId>>,
        types: BTreeMap<EId, Result<CanonicalScalar, RelationId>>,
    }
    impl CatalogNames {
        pub(super) fn labels(
            &self,
            vid: VId,
        ) -> Result<Option<&[fgdb_gql::algebra::GraphValue]>, WriteTxnError> {
            match self.labels.get(&vid) {
                None => Ok(None),
                Some(Ok(names)) => Ok(Some(names)),
                Some(Err(id)) => Err(WriteTxnError::Read(crate::ReadError::UnmappedLabel(*id))),
            }
        }
        pub(super) fn edge_type(
            &self,
            eid: EId,
        ) -> Result<Option<&CanonicalScalar>, WriteTxnError> {
            match self.types.get(&eid) {
                None => Ok(None),
                Some(Ok(name)) => Ok(Some(name)),
                Some(Err(relation)) => Err(WriteTxnError::Read(
                    crate::ReadError::UnmappedRelation(*relation),
                )),
            }
        }
    }

    /// One basis/template pair, with output shape carried by the checked plan.
    /// The borrowed source is the same for scalar and tuple query projections.
    pub(super) struct OverlayQuerySource<'a, Row = VId> {
        pub(super) logical: GlaPlan<Row>,
        tables: OverlayGraphTables<'a>,
    }
    /// Shared canonical overlay admission. A graph procedure consumes both
    /// complete tables, including isolated vertices, without inventing a
    /// pattern or rebuilding the transaction's staged effects independently.
    pub(super) struct OverlayGraphTables<'a> {
        vertices: Vec<(VId, VertexView<'a>)>,
        edges: Vec<(IdentifiedEdge, EdgeView<'a>)>,
        pub(super) snapshot_records: usize,
        // Incomplete until THIS execution and its final allowance check pass.
        scan_observations: Vec<(&'a WriteTxn, usize)>,
    }
    impl<'a, Row> core::ops::Deref for OverlayQuerySource<'a, Row> {
        type Target = OverlayGraphTables<'a>;

        fn deref(&self) -> &Self::Target {
            &self.tables
        }
    }
    impl<Row: GlaOutput> OverlayQuerySource<'_, Row> {
        fn accept_observations(&self) {
            for &(transaction, index) in &self.scan_observations {
                transaction
                    .point_reads
                    .borrow_mut()
                    .complete_vertex_scan(index);
            }
        }

        pub(super) fn vertex_ids(&self) -> impl Iterator<Item = VId> + '_ {
            self.vertices.iter().map(|(vid, _)| *vid)
        }

        /// `labels(n)` and `type(r)` names from the plan's reverse catalog,
        /// over staged label edits, as the snapshot source resolves them. An
        /// unmapped symbol refuses when its element is read; it never reads
        /// as NULL.
        pub(super) fn catalog_names(&self) -> CatalogNames {
            let reverse = self.logical.reverse_catalog.as_deref();
            let name = |text: Option<&String>| {
                text.and_then(|text| CanonicalScalar::ucs_basic_text(text).ok())
            };
            let mut labels = BTreeMap::new();
            if self.logical.projects_labels() {
                for (vid, view) in &self.vertices {
                    let names = view
                        .effective_labels()
                        .into_iter()
                        .map(|id| {
                            name(reverse.and_then(|catalog| catalog.labels.get(&id)))
                                .map(fgdb_gql::algebra::GraphValue::Scalar)
                                .ok_or(id)
                        })
                        .collect();
                    labels.insert(*vid, names);
                }
            }
            let mut types = BTreeMap::new();
            if self.logical.projects_types() {
                for ((eid, _, relation, _), _) in &self.edges {
                    let text = name(reverse.and_then(|catalog| catalog.relations.get(relation)))
                        .ok_or(*relation);
                    types.insert(*eid, text);
                }
            }
            CatalogNames { labels, types }
        }
        pub(super) fn property_maps<E>(
            &self,
            control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
            failure: impl Fn(crate::ReadError) -> E,
        ) -> Result<PropertyMaps, E> {
            let catalog = self.logical.reverse_catalog.as_deref();
            let mut maps = PropertyMaps::default();
            if self.logical.projects_vertex_property_maps() {
                for (vid, view) in &self.vertices {
                    let map = property_maps::collect(
                        view.effective_properties(),
                        catalog,
                        |_| true,
                        control,
                        &failure,
                    )?;
                    control(SourceEvent::ScratchEntry)?;
                    maps.vertices.insert(*vid, map);
                }
            }
            if self.logical.projects_edge_property_maps() {
                for ((eid, _, _, _), view) in &self.edges {
                    let map = property_maps::collect(
                        view.effective_properties(),
                        catalog,
                        |_| true,
                        control,
                        &failure,
                    )?;
                    control(SourceEvent::ScratchEntry)?;
                    maps.edges.insert(*eid, map);
                }
            }
            Ok(maps)
        }
        pub(super) fn identified_edges(&self) -> impl Iterator<Item = IdentifiedEdge> + '_ {
            self.edges.iter().map(|(edge, _)| *edge)
        }
        pub(super) fn edge_triples(&self) -> impl Iterator<Item = EdgeTriple> + '_ {
            self.identified_edges()
                .map(|(_, src, relation, dst)| (src, relation, dst))
        }
        pub(super) fn matches(&self, vid: VId, predicates: &[VertexPredicate]) -> bool {
            self.vertices
                .binary_search_by_key(&vid, |(vid, _)| *vid)
                .ok()
                .is_some_and(|at| {
                    predicates
                        .iter()
                        .all(|predicate| self.vertices[at].1.matches(predicate))
                })
        }
        pub(super) fn property(&self, vid: VId, key: PropertyKeyId) -> Option<&CanonicalScalar> {
            let at = self
                .vertices
                .binary_search_by_key(&vid, |(vid, _)| *vid)
                .ok()?;
            self.vertices[at].1.property(key)
        }
        pub(super) fn edge_property(
            &self,
            eid: EId,
            key: PropertyKeyId,
        ) -> Option<&CanonicalScalar> {
            let at = self
                .edges
                .binary_search_by_key(&eid, |(edge, _)| edge.0)
                .ok()?;
            self.edges[at].1.property(key)
        }
        pub(super) fn execute(self) -> Result<Vec<Row>, WriteTxnError>
        where
            Row: GlaIdentityOutput,
        {
            let result = self.logical.execute(
                self.vertex_ids(),
                self.edge_triples(),
                |vid, predicates| Ok::<_, WriteTxnError>(self.matches(vid, predicates)),
            )?;
            self.accept_observations();
            Ok(result)
        }
    }
    fn is_vertex_effect(row: &DeltaRow) -> bool {
        matches!(
            row,
            DeltaRow::CreateVertex { .. }
                | DeltaRow::DeleteVertex { .. }
                | DeltaRow::LabelMembership { .. }
                | DeltaRow::Property {
                    elem: ElementId::Vertex(_),
                    ..
                }
        )
    }
    fn apply_vertex<'a, E>(
        rows: &mut BTreeMap<VId, VertexView<'a>>,
        effect: &'a DeltaRow,
        candidates: Option<&BTreeSet<VId>>,
        control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        match effect {
            DeltaRow::CreateVertex {
                vid, labels, props, ..
            } if candidates.is_none_or(|candidates| candidates.contains(vid)) => {
                if !rows.contains_key(vid) {
                    control(SourceEvent::ScratchEntry)?;
                }
                rows.insert(*vid, VertexView::new(labels, props));
            }
            DeltaRow::DeleteVertex { vid, .. } => {
                rows.remove(vid);
            }
            DeltaRow::LabelMembership {
                vid, label, after, ..
            } => {
                if let Some(row) = rows.get_mut(vid) {
                    if !row.label_edits.contains_key(label) {
                        control(SourceEvent::ScratchEntry)?;
                    }
                    row.label_edits.insert(*label, *after);
                }
            }
            DeltaRow::Property {
                elem: ElementId::Vertex(vid),
                property,
                after,
                ..
            } => {
                if let Some(row) = rows.get_mut(vid) {
                    if !row.property_edits.contains_key(property) {
                        control(SourceEvent::ScratchEntry)?;
                    }
                    row.property_edits.insert(*property, after.as_ref());
                }
            }
            _ => {}
        }
        Ok(())
    }

    impl WriteTxn {
        /// Admit indexed flat fixed-hop patterns without scanning the two
        /// complete tables. Roots that gain a predicate field in this
        /// transaction join the historical index candidates. Walking the
        /// union of basis edges and staged creations is a superset of the
        /// final overlay even when a preceding statement deleted an edge or
        /// vertex. The existing effect application then removes those rows.
        ///
        /// Scoped bodies, independent edge components and quantified paths
        /// retain complete admission until their closure has a completeness law.
        fn bound_query_source<'a, E, Row>(
            &'a self,
            snapshot: &'a Snapshot,
            logical: &GlaPlan<Row>,
            control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
        ) -> Result<Option<BoundOverlay<'a>>, E> {
            let Some(root) = logical.operators().first() else {
                return Ok(None);
            };
            let width = match root {
                GlaOperator::ScanVertices => 1,
                GlaOperator::ScanEdges { .. } => 2,
                _ => return Ok(None),
            };
            let prefix = &logical.operators()[1..];
            let Some(steps) = source::bound_edge_steps(prefix, width) else {
                return Ok(None);
            };
            if steps.iter().any(|step| step.walk.is_some()) {
                return Ok(None);
            }
            let mut root_selection = None;
            for operator in prefix.iter().take_while(|operator| {
                !matches!(
                    operator,
                    GlaOperator::Expand { .. } | GlaOperator::SelectBoolean { .. }
                )
            }) {
                control(SourceEvent::Work)?;
                let GlaOperator::Select { slot, predicates } = operator else {
                    continue;
                };
                let Ok(root_slot) = usize::try_from(slot.ordinal()) else {
                    continue;
                };
                if root_slot >= width {
                    continue;
                }
                // Either root endpoint's conjunctive selection can bound the
                // edge candidates. A label-only left endpoint must not hide
                // an indexed key on the right endpoint. Do not add a witness
                // until a selection can seek the index.
                if indexed_predicates(predicates, control)? {
                    root_selection = Some((root_slot, operator, predicates));
                    break;
                }
            }
            let Some((root_slot, selection, predicates)) = root_selection else {
                return Ok(None);
            };
            // A root outside the current closure can enter it by changing a
            // predicate field without inserting an edge. Keep the existing
            // membership witness before probing, including after a refusal.
            // Primitive predicates read one vertex regardless of its root slot.
            let selections = core::slice::from_ref(selection);
            super::VertexScanRead::charge_definition(selections, control)?;
            let root_observation = self.point_reads.borrow_mut().begin_vertex_scan(selections);
            let Some(roots) = source::indexed_roots(snapshot, predicates, self.basis, control)?
            else {
                return Ok(None);
            };
            let mut candidates = BTreeSet::new();
            for vid in roots {
                control(SourceEvent::ScratchEntry)?;
                candidates.insert(vid);
            }
            let mut staged = StagedAdjacency::new();
            if let Some(prepared) = &self.prepared {
                for coordinate in prepared.template.coordinate_entries() {
                    control(SourceEvent::Work)?;
                    for effect in &coordinate.rows {
                        control(SourceEvent::Work)?;
                        let new_root = match effect {
                            DeltaRow::CreateVertex { vid, .. } => Some(*vid),
                            DeltaRow::LabelMembership { vid, label, .. }
                                if predicates.iter().any(|predicate| {
                                    matches!(predicate, VertexPredicate::HasLabel(id) if id == label)
                                }) => Some(*vid),
                            DeltaRow::Property {
                                elem: ElementId::Vertex(vid),
                                property,
                                ..
                            } if predicates
                                .iter()
                                .any(|predicate| predicate.property_key() == Some(*property)) =>
                            {
                                Some(*vid)
                            }
                            _ => None,
                        };
                        if let Some(vid) = new_root
                            && !candidates.contains(&vid)
                        {
                            control(SourceEvent::ScratchEntry)?;
                            candidates.insert(vid);
                        }
                        if let DeltaRow::CreateEdge {
                            eid,
                            src,
                            relation,
                            dst,
                            props,
                            ..
                        } = effect
                        {
                            for (near, incoming) in [(*src, false), (*dst, true)] {
                                control(SourceEvent::ScratchEntry)?;
                                staged.insert(
                                    (near, incoming, *eid),
                                    ((*eid, *src, *relation, *dst), props),
                                );
                            }
                        }
                    }
                }
            }
            let mut slots = (0..width).map(|_| BTreeSet::new()).collect::<Vec<_>>();
            slots[root_slot] = candidates;
            let mut edges = OverlayEdges::new();
            if let GlaOperator::ScanEdges {
                relation,
                direction,
            } = root
            {
                let direction = match (*direction, root_slot) {
                    (GlaDirection::Forward, 1) => GlaDirection::Reverse,
                    (GlaDirection::Reverse, 1) => GlaDirection::Forward,
                    (direction, _) => direction,
                };
                slots[1 - root_slot] = extend_bound(
                    snapshot,
                    self.basis,
                    &slots[root_slot],
                    relation,
                    direction,
                    &staged,
                    &mut edges,
                    control,
                )?;
            }
            for step in steps {
                let target = extend_bound(
                    snapshot,
                    self.basis,
                    &slots[step.source],
                    step.relation,
                    step.direction,
                    &staged,
                    &mut edges,
                    control,
                )?;
                slots.push(target);
            }
            let mut vertices = BTreeSet::new();
            for slot in slots {
                for vid in slot {
                    control(SourceEvent::Work)?;
                    if !vertices.contains(&vid) {
                        control(SourceEvent::ScratchEntry)?;
                        vertices.insert(vid);
                    }
                }
            }
            control(SourceEvent::ScratchEntry)?;
            Ok(Some(BoundOverlay {
                vertices,
                edges,
                root_observations: vec![root_observation],
            }))
        }

        /// Independent positive vertex roots share one admitted table. Its
        /// safe bound is the UNION of every root's indexed candidates, not the
        /// intersection and not the first root alone. The unchanged algebra
        /// still applies each root's own predicate and preserves the product.
        ///
        /// Every root must have a primitive indexed selection before any
        /// fallible Boolean selection. Scoped bodies, extra binding kinds and
        /// unindexed roots retain complete admission. Register all membership
        /// domains before probing any index, including empty or refused probes.
        fn bound_vertex_sources<'a, E, Row>(
            &'a self,
            snapshot: &'a Snapshot,
            logical: &GlaPlan<Row>,
            record_membership: bool,
            control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
        ) -> Result<Option<BoundOverlay<'a>>, E> {
            let mut roots: Vec<Option<&GlaOperator>> = Vec::new();
            let mut selections_open = true;
            let mut projected = false;
            for operator in logical.operators() {
                control(SourceEvent::Work)?;
                match operator {
                    GlaOperator::ScanVertices if selections_open && !projected => {
                        control(SourceEvent::ScratchEntry)?;
                        roots.push(None);
                    }
                    GlaOperator::Select { slot, predicates } if !projected => {
                        if selections_open {
                            let Ok(slot) = usize::try_from(slot.ordinal()) else {
                                return Ok(None);
                            };
                            let Some(root) = roots.get_mut(slot) else {
                                return Ok(None);
                            };
                            if root.is_none() && indexed_predicates(predicates, control)? {
                                *root = Some(operator);
                            }
                        }
                    }
                    GlaOperator::VertexIdentity { .. } if !projected => {}
                    GlaOperator::CompareProperties { .. } | GlaOperator::SelectBoolean { .. }
                        if !projected =>
                    {
                        selections_open = false;
                    }
                    GlaOperator::Project { .. }
                    | GlaOperator::ProjectBindings { .. }
                    | GlaOperator::ProjectValues { .. }
                    | GlaOperator::Distinct
                    | GlaOperator::OrderByVertexId
                    | GlaOperator::OrderByBindings
                    | GlaOperator::OrderByValues
                    | GlaOperator::OrderByValueColumns { .. }
                    | GlaOperator::Limit { .. } => projected = true,
                    _ => return Ok(None),
                }
            }
            if roots.is_empty() || roots.iter().any(Option::is_none) {
                return Ok(None);
            }
            let mut root_observations = Vec::new();
            let mut keys = BTreeSet::new();
            let mut labels = BTreeSet::new();
            for selection in roots.iter().flatten() {
                if record_membership {
                    let selections = core::slice::from_ref(*selection);
                    super::VertexScanRead::charge_definition(selections, control)?;
                    let observation = self.point_reads.borrow_mut().begin_vertex_scan(selections);
                    control(SourceEvent::ScratchEntry)?;
                    root_observations.push(observation);
                }
                if let GlaOperator::Select { predicates, .. } = selection {
                    for predicate in predicates {
                        control(SourceEvent::Work)?;
                        if let Some(key) = predicate.property_key()
                            && !keys.contains(&key)
                        {
                            control(SourceEvent::ScratchEntry)?;
                            keys.insert(key);
                        }
                        if let VertexPredicate::HasLabel(label) = predicate
                            && !labels.contains(label)
                        {
                            control(SourceEvent::ScratchEntry)?;
                            labels.insert(*label);
                        }
                    }
                }
            }
            let mut vertices = BTreeSet::new();
            for selection in roots.into_iter().flatten() {
                let GlaOperator::Select { predicates, .. } = selection else {
                    unreachable!("only primitive root selections were retained");
                };
                let Some(candidates) =
                    source::indexed_roots(snapshot, predicates, self.basis, control)?
                else {
                    return Ok(None);
                };
                for vid in candidates {
                    control(SourceEvent::Work)?;
                    if !vertices.contains(&vid) {
                        control(SourceEvent::ScratchEntry)?;
                        vertices.insert(vid);
                    }
                }
            }
            if let Some(prepared) = &self.prepared {
                for coordinate in prepared.template.coordinate_entries() {
                    control(SourceEvent::Work)?;
                    for effect in &coordinate.rows {
                        control(SourceEvent::Work)?;
                        let candidate = match effect {
                            DeltaRow::CreateVertex { vid, .. } => Some(*vid),
                            DeltaRow::LabelMembership { vid, label, .. }
                                if labels.contains(label) =>
                            {
                                Some(*vid)
                            }
                            DeltaRow::Property {
                                elem: ElementId::Vertex(vid),
                                property,
                                ..
                            } if keys.contains(property) => Some(*vid),
                            _ => None,
                        };
                        if let Some(vid) = candidate
                            && !vertices.contains(&vid)
                        {
                            control(SourceEvent::ScratchEntry)?;
                            vertices.insert(vid);
                        }
                    }
                }
            }
            Ok(Some(BoundOverlay {
                vertices,
                edges: OverlayEdges::new(),
                root_observations,
            }))
        }

        pub(super) fn query_snapshot<'a, V: Vfs + Clone>(
            &self,
            database: &'a Database<V>,
        ) -> Result<&'a Snapshot, WriteTxnError> {
            self.ensure_database(database)?;
            database.ensure_readable()?;
            database.snapshot.check_frontier(self.basis)?;
            Ok(&database.snapshot)
        }
        pub(super) fn query_source<'a, V: Vfs + Clone>(
            &'a self,
            database: &'a Database<V>,
            plan: &BoundPlan,
        ) -> Result<OverlayQuerySource<'a>, WriteTxnError> {
            let snapshot = self.query_snapshot(database)?;
            self.query_source_with_witnesses(
                snapshot,
                GlaPlan::lower(plan),
                plan.src_label,
                true,
                &mut |_| Ok(()),
            )
        }

        /// Run one scalar or correlated binding projection over the original
        /// basis plus canonical staged effects. All shapes retain the same
        /// source, witnesses and one shared source/evaluator allowance.
        /// Single-vertex selections and indexed independent roots retain
        /// membership witnesses plus full reads of matching/staged vertices.
        /// Indexed flat expansions admit their basis/staged adjacency closure
        /// and retain root-membership witnesses. Other shapes keep scans.
        /// Refusals keep a broad vertex-domain witness; no prior getter, failed
        /// scan or savepoint observation is narrowed.
        pub fn execute_graph_pattern_governed<V: Vfs + Clone, Row: GlaOutput>(
            &self,
            database: &Database<V>,
            cx: &fgdb_types::QueryCx,
            pattern: &PreparedGraphPattern<Row>,
            policy: fgdb_gql::GqlQueryPolicy,
        ) -> Result<
            fgdb_gql::GqlQueryExecution<Row>,
            fgdb_gql::GqlQueryError<WriteTxnError, Box<asupersync::error::Error>>,
        > {
            cx.with_restriction(|| {
                let snapshot = self
                    .query_snapshot(database)
                    .map_err(fgdb_gql::GqlQueryError::Source)?;
                cx.checkpoint()
                    .map_err(fgdb_gql::GqlQueryError::Interrupted)?;
                let mut usage = crate::gql_exec::AdmissionUsage::default();
                let source = self.query_source_with_witnesses(
                    snapshot,
                    pattern.plan().clone(),
                    pattern.required_vertex_label(),
                    true,
                    &mut |event| {
                        cx.checkpoint()
                            .map_err(fgdb_gql::GqlQueryError::Interrupted)?;
                        usage.observe(policy, event)
                    },
                )?;
                let names = source.catalog_names();
                let maps = source.property_maps(
                    &mut |event| {
                        cx.checkpoint()
                            .map_err(fgdb_gql::GqlQueryError::Interrupted)?;
                        usage.observe(policy, event)
                    },
                    |error| fgdb_gql::GqlQueryError::Source(WriteTxnError::Read(error)),
                )?;
                let result = source.logical.execute_governed_with_element_maps(
                    source.snapshot_records as u64,
                    source.vertex_ids(),
                    source.identified_edges(),
                    |vid, predicates| Ok::<_, WriteTxnError>(source.matches(vid, predicates)),
                    |vid, key| Ok(source.property(vid, key)),
                    |eid, key| Ok(source.edge_property(eid, key)),
                    |vid| names.labels(vid),
                    |eid| names.edge_type(eid),
                    |vid| Ok(maps.vertices.get(&vid)),
                    |eid| Ok(maps.edges.get(&eid)),
                    usage.remaining(policy),
                    || cx.checkpoint(),
                );
                let result = usage.finish(policy, result)?;
                source.accept_observations();
                Ok(result)
            })
        }

        fn note_query_read<E>(
            &self,
            observed: &mut BTreeSet<ElementId>,
            element: ElementId,
            control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
        ) -> Result<(), E> {
            control(SourceEvent::Work)?;
            if !observed.contains(&element) {
                control(SourceEvent::ScratchEntry)?;
                control(SourceEvent::ScratchEntry)?;
                observed.insert(element);
                self.read_set.borrow_mut().insert(element);
            }
            Ok(())
        }
        pub(super) fn query_source_over<'a, E>(
            &'a self,
            snapshot: &'a Snapshot,
            plan: &BoundPlan,
            control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
        ) -> Result<OverlayQuerySource<'a>, E> {
            self.query_source_over_logical(snapshot, GlaPlan::lower(plan), plan.src_label, control)
        }

        /// Row limits count the final canonical overlay, not removed basis rows.
        /// Once an observation is retained, later refusal does not discard it.
        fn query_source_over_logical<'a, E, Row: GlaOutput>(
            &'a self,
            snapshot: &'a Snapshot,
            logical: GlaPlan<Row>,
            required_vertex_label: Option<LabelId>,
            control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
        ) -> Result<OverlayQuerySource<'a, Row>, E> {
            // Callers that own a different acceptance boundary keep the old
            // broad witnesses until they explicitly opt into this protocol.
            self.query_source_with_witnesses(
                snapshot,
                logical,
                required_vertex_label,
                false,
                control,
            )
        }

        fn query_source_with_witnesses<'a, E, Row: GlaOutput>(
            &'a self,
            snapshot: &'a Snapshot,
            logical: GlaPlan<Row>,
            required_vertex_label: Option<LabelId>,
            precise: bool,
            control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
        ) -> Result<OverlayQuerySource<'a, Row>, E> {
            let tables = self.query_source_tables(
                snapshot,
                Some(&logical),
                required_vertex_label,
                precise,
                control,
            )?;
            Ok(OverlayQuerySource { logical, tables })
        }

        /// None selects a complete graph projection for a registered
        /// procedure. It requires both table phantom witnesses even when the
        /// current graph is empty; existing query plans keep their narrower
        /// source requirements and precise acceptance protocol.
        fn query_source_tables<'a, E, Row: GlaOutput, C>(
            &'a self,
            snapshot: &'a Snapshot,
            logical: Option<&GlaPlan<Row>>,
            required_vertex_label: Option<LabelId>,
            precise: bool,
            control: &mut C,
        ) -> Result<OverlayGraphTables<'a>, E>
        where
            C: FnMut(SourceEvent) -> Result<(), E>,
        {
            let predicates = logical
                .filter(|_| precise)
                .and_then(super::VertexScanRead::predicates);
            let mut scan_observations = if let Some(predicates) = predicates {
                // Charge the bounded definition copy before allocation or graph
                // reads. A failure here has learned nothing from the database.
                super::VertexScanRead::charge_definition(predicates, control)?;
                let index = self.point_reads.borrow_mut().begin_vertex_scan(predicates);
                control(SourceEvent::ScratchEntry)?;
                vec![(self, index)]
            } else {
                Vec::new()
            };
            let edge_scan = logical.is_some_and(GlaPlan::scans_edges);
            // An edge root does not imply an endpoint-only vertex domain.
            // Independent components and uncorrelated probes can scan every
            // vertex, including isolates. Admit that table once, as the native
            // snapshot source does, and apply staged effects to the same rows.
            let vertex_scan = !edge_scan
                || logical.is_some_and(|plan| {
                    plan.operators().iter().any(|operator| {
                        matches!(operator, fgdb_gql::algebra::GlaOperator::ScanVertices)
                    })
                });
            let reads_edges = logical.is_none_or(GlaPlan::reads_edges);
            // Typed edges retain per-relation witnesses. An untyped atom can
            // see a relation created after this read, so it needs the complete
            // edge-table phantom witness, including an initially empty table.
            let edge_relations = logical.and_then(GlaPlan::edge_relations);
            control(SourceEvent::Work)?;
            if reads_edges {
                match &edge_relations {
                    Some(relations) => self
                        .scanned_edge_relations
                        .borrow_mut()
                        .extend(relations.iter().copied()),
                    None => self.scanned_edges.set(true),
                }
            }
            // Eligible vertex-only roots establish every membership domain
            // before touching index candidates. An unfinished domain retains
            // the old conservative refusal semantics; a completed domain does
            // not need an additional whole-table insertion witness. A precise
            // single-root selection already has its full predicate witness.
            let vertex_bound = if precise && !reads_edges {
                match logical {
                    Some(logical) => {
                        self.bound_vertex_sources(snapshot, logical, predicates.is_none(), control)?
                    }
                    None => None,
                }
            } else {
                None
            };
            if vertex_scan && scan_observations.is_empty() && vertex_bound.is_none() {
                // An edge endpoint's label cannot constrain an independent
                // vertex root. Keep its insertion gap even for empty results.
                if let Some(label) = required_vertex_label.filter(|_| !edge_scan) {
                    control(SourceEvent::ScratchEntry)?;
                    self.scanned_vertex_labels.borrow_mut().insert(label);
                } else {
                    self.scanned_vertices.set(true);
                }
            }
            // Under that label witness, a basis vertex without the label cannot
            // be bound by the root: it enters the domain only by gaining the
            // label, which the witness catches, and an expansion reaches it only
            // through a read edge. So only labelled vertices are recorded, never
            // the whole table (fgdb-h1d6l). The plan's own domain must agree
            // with the caller's label, or the scan stays broad.
            let witness_label = required_vertex_label.filter(|label| {
                !edge_scan
                    && scan_observations.is_empty()
                    && vertex_bound.is_none()
                    && logical.is_some_and(|plan| {
                        plan.vertex_scan_domain()
                            == fgdb_gql::algebra::VertexScanDomain::Label(*label)
                    })
            });
            let mut observed = BTreeSet::new();
            // Raw intentions contribute only negative-read identities. The
            // canonical template, not these intentions, creates query rows.
            for batch in &self.staged {
                control(SourceEvent::Work)?;
                for pending in &batch.rows {
                    control(SourceEvent::Work)?;
                    match pending {
                        PendingRow::Vertex { vid, .. } | PendingRow::DeleteVertex { vid, .. } => {
                            self.note_query_read(&mut observed, ElementId::Vertex(*vid), control)?;
                        }
                        PendingRow::Edge { eid, src, dst, .. }
                            if reads_edges
                                && edge_relations.as_ref().is_none_or(|relations| {
                                    relations.contains(&batch.relation)
                                }) =>
                        {
                            for element in [
                                ElementId::Edge(*eid),
                                ElementId::Vertex(*src),
                                ElementId::Vertex(*dst),
                            ] {
                                self.note_query_read(&mut observed, element, control)?;
                            }
                        }
                        PendingRow::DeleteEdge { eid, .. } if reads_edges => {
                            self.note_query_read(&mut observed, ElementId::Edge(*eid), control)?;
                        }
                        _ => {}
                    }
                }
            }
            let bound = match logical {
                Some(logical) if reads_edges => {
                    self.bound_query_source(snapshot, logical, control)?
                }
                Some(_) if precise => vertex_bound,
                Some(logical) => self.bound_vertex_sources(snapshot, logical, true, control)?,
                None => None,
            };
            let (mut edges, vertex_candidates) = if let Some(bound) = bound {
                if precise {
                    for observation in bound.root_observations {
                        control(SourceEvent::ScratchEntry)?;
                        scan_observations.push((self, observation));
                    }
                }
                for (&eid, (src, relation, dst, _)) in &bound.edges {
                    control(SourceEvent::Work)?;
                    if edge_relations
                        .as_ref()
                        .is_none_or(|relations| relations.contains(relation))
                    {
                        for element in [
                            ElementId::Edge(eid),
                            ElementId::Vertex(*src),
                            ElementId::Vertex(*dst),
                        ] {
                            self.note_query_read(&mut observed, element, control)?;
                        }
                    }
                }
                (bound.edges, Some(bound.vertices))
            } else {
                (BTreeMap::new(), None)
            };
            let mut vertices = BTreeMap::new();
            if reads_edges && vertex_candidates.is_none() {
                source::visit_edges_with_properties(
                    snapshot,
                    self.basis,
                    control,
                    |entry, props, control| {
                        // Source accounting still counts every scanned edge
                        // (the cross-surface stats contract); only the read
                        // witness narrows to the relations the plan reads.
                        if edge_relations
                            .as_ref()
                            .is_none_or(|relations| relations.contains(&entry.relation))
                        {
                            for element in [
                                ElementId::Edge(entry.eid),
                                ElementId::Vertex(entry.src),
                                ElementId::Vertex(entry.dst),
                            ] {
                                self.note_query_read(&mut observed, element, control)?;
                            }
                        }
                        control(SourceEvent::ScratchEntry)?;
                        edges.insert(
                            entry.eid,
                            (entry.src, entry.relation, entry.dst, EdgeView::new(props)),
                        );
                        Ok(())
                    },
                )?;
            }
            if vertex_scan {
                let mut admit = |row: &'a crate::VertexRow, control: &mut C| {
                    let view = VertexView::new(&row.labels, &row.props);
                    let selected = match predicates {
                        Some(predicates) => {
                            super::VertexScanRead::may_match(predicates, row, control)?
                        }
                        None => witness_label.is_none_or(|label| row.labels.contains(&label)),
                    };
                    if selected {
                        self.note_query_read(&mut observed, ElementId::Vertex(row.vid), control)?;
                    }
                    // Every row in a bound closure remains a candidate; the
                    // algebra, after staged effects, owns the actual selection.
                    control(SourceEvent::ScratchEntry)?;
                    vertices.insert(row.vid, view);
                    Ok(())
                };
                if let Some(candidates) = &vertex_candidates {
                    for &vid in candidates {
                        if let Some(row) = source::find_vertex(snapshot, vid, self.basis, control)?
                        {
                            admit(row, control)?;
                        }
                    }
                } else {
                    source::visit_vertices(&snapshot.patches, self.basis, control, admit)?;
                }
            }
            let mut vertex_effects = Vec::new();
            if let Some(prepared) = &self.prepared {
                for coordinate in prepared.template.coordinate_entries() {
                    control(SourceEvent::Work)?;
                    for effect in &coordinate.rows {
                        control(SourceEvent::Work)?;
                        if vertex_scan {
                            if (!scan_observations.is_empty() || witness_label.is_some())
                                && let Some(vid) = super::VertexScanRead::target(effect)
                            {
                                // Staged overrides are transaction-local, not
                                // historical images. Observe every affected ID
                                // broadly, even if it leaves the predicate or
                                // its statement is subsequently rolled back.
                                self.note_query_read(
                                    &mut observed,
                                    ElementId::Vertex(vid),
                                    control,
                                )?;
                            }
                            apply_vertex(
                                &mut vertices,
                                effect,
                                vertex_candidates.as_ref(),
                                control,
                            )?;
                        }
                        if reads_edges {
                            match effect {
                                DeltaRow::CreateEdge {
                                    eid,
                                    src,
                                    relation,
                                    dst,
                                    props,
                                    ..
                                } if vertex_candidates.is_none() || edges.contains_key(eid) => {
                                    if !edges.contains_key(eid) {
                                        control(SourceEvent::ScratchEntry)?;
                                    }
                                    edges.insert(
                                        *eid,
                                        (*src, *relation, *dst, EdgeView::new(props)),
                                    );
                                }
                                DeltaRow::DeleteEdge { eid, .. } => {
                                    edges.remove(eid);
                                }
                                DeltaRow::Property {
                                    elem: ElementId::Edge(eid),
                                    property,
                                    after,
                                    ..
                                } => {
                                    if let Some((_, _, _, row)) = edges.get_mut(eid) {
                                        if !row.edits.contains_key(property) {
                                            control(SourceEvent::ScratchEntry)?;
                                        }
                                        row.edits.insert(*property, after.as_ref());
                                    }
                                }
                                DeltaRow::DeleteVertex {
                                    sorted_retired_incident_edges,
                                    ..
                                } => {
                                    for eid in sorted_retired_incident_edges {
                                        control(SourceEvent::Work)?;
                                        self.note_query_read(
                                            &mut observed,
                                            ElementId::Edge(*eid),
                                            control,
                                        )?;
                                        edges.remove(eid);
                                    }
                                }
                                _ => {}
                            }
                        }
                        if !vertex_scan && is_vertex_effect(effect) {
                            control(SourceEvent::ScratchEntry)?;
                            vertex_effects.push(effect);
                        }
                    }
                }
            }
            if !vertex_scan && logical.is_some_and(GlaPlan::needs_vertex_values) {
                let mut candidates = BTreeSet::new();
                for &(src, relation, dst, _) in edges.values() {
                    control(SourceEvent::Work)?;
                    let requested = edge_relations
                        .as_ref()
                        .is_none_or(|relations| relations.contains(&relation));
                    if requested {
                        for vid in [src, dst] {
                            if !candidates.contains(&vid) {
                                control(SourceEvent::ScratchEntry)?;
                                candidates.insert(vid);
                            }
                        }
                    }
                }
                for &vid in &candidates {
                    self.note_query_read(&mut observed, ElementId::Vertex(vid), control)?;
                    if let Some(row) = source::find_vertex(snapshot, vid, self.basis, control)? {
                        control(SourceEvent::ScratchEntry)?;
                        vertices.insert(vid, VertexView::new(&row.labels, &row.props));
                    }
                }
                for effect in vertex_effects {
                    control(SourceEvent::Work)?;
                    apply_vertex(&mut vertices, effect, Some(&candidates), control)?;
                }
            }
            let mut vertex_rows = Vec::new();
            for (vid, row) in vertices {
                control(SourceEvent::Work)?;
                if vertex_scan {
                    control(SourceEvent::SnapshotRecord)?;
                }
                control(SourceEvent::ScratchEntry)?;
                vertex_rows.push((vid, row));
            }
            let mut edge_rows = Vec::new();
            for (eid, (src, relation, dst, props)) in edges {
                control(SourceEvent::Work)?;
                control(SourceEvent::SnapshotRecord)?;
                control(SourceEvent::ScratchEntry)?;
                edge_rows.push(((eid, src, relation, dst), props));
            }
            let snapshot_records =
                edge_rows.len() + if vertex_scan { vertex_rows.len() } else { 0 };
            Ok(OverlayGraphTables {
                vertices: vertex_rows,
                edges: edge_rows,
                snapshot_records,
                scan_observations,
            })
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use fgdb_gql::algebra::IntegerComparison;
        #[test]
        fn borrowed_property_overrides_shadow_without_cloning_or_coercion() {
            let key = PropertyKeyId(1);
            let properties = [(key, CanonicalScalar::Int(7))];
            let changed = CanonicalScalar::Int(9);
            let mut row = VertexView::new(&[LabelId(2)], &properties);
            assert!(std::ptr::eq(row.property(key).unwrap(), &properties[0].1));
            row.property_edits.insert(key, Some(&changed));
            assert!(std::ptr::eq(row.property(key).unwrap(), &changed));
            assert!(row.matches(&VertexPredicate::IntegerProperty {
                key,
                comparison: IntegerComparison::Equal,
                value: 9
            }));
            row.property_edits.insert(key, None);
            assert!(row.property(key).is_none());
            for comparison in [
                IntegerComparison::Equal,
                IntegerComparison::NotEqual,
                IntegerComparison::Greater,
                IntegerComparison::Less,
                IntegerComparison::GreaterOrEqual,
                IntegerComparison::LessOrEqual,
            ] {
                assert!(!row.matches(&VertexPredicate::IntegerProperty {
                    key,
                    comparison,
                    value: 9
                }));
            }
        }
        #[test]
        fn complete_property_maps_use_staged_add_update_and_removal_for_both_element_kinds() {
            use fgdb_gql::algebra::GraphValue;
            let properties = [
                (PropertyKeyId(1), CanonicalScalar::Int(7)),
                // An effective removal must not consult this unmapped key.
                (PropertyKeyId(2), CanonicalScalar::Int(8)),
                (PropertyKeyId(4), CanonicalScalar::Int(9)),
            ];
            let updated = CanonicalScalar::Bool(false);
            let added = CanonicalScalar::Null;
            let mut vertex = VertexView::new(&[], &properties);
            vertex
                .property_edits
                .insert(PropertyKeyId(1), Some(&updated));
            vertex.property_edits.insert(PropertyKeyId(2), None);
            vertex.property_edits.insert(PropertyKeyId(3), Some(&added));
            let mut edge = EdgeView::new(&properties);
            edge.edits.insert(PropertyKeyId(1), Some(&updated));
            edge.edits.insert(PropertyKeyId(2), None);
            edge.edits.insert(PropertyKeyId(3), Some(&added));
            let mut catalog = fgdb_gql::ReverseSymbolCatalog::new();
            catalog.insert_property(PropertyKeyId(1), "updated");
            catalog.insert_property(PropertyKeyId(3), "added");
            catalog.insert_property(PropertyKeyId(4), "kept");
            let expected = GraphValue::map(vec![
                ("added".into(), GraphValue::Scalar(CanonicalScalar::Null)),
                ("kept".into(), GraphValue::Scalar(CanonicalScalar::Int(9))),
                (
                    "updated".into(),
                    GraphValue::Scalar(CanonicalScalar::Bool(false)),
                ),
            ])
            .unwrap();
            let vertex_map = property_maps::collect(
                vertex.effective_properties(),
                Some(&catalog),
                |_| true,
                &mut |_| Ok::<_, crate::ReadError>(()),
                &core::convert::identity,
            )
            .unwrap();
            let edge_map = property_maps::collect(
                edge.effective_properties(),
                Some(&catalog),
                |_| true,
                &mut |_| Ok::<_, crate::ReadError>(()),
                &core::convert::identity,
            )
            .unwrap();
            assert_eq!(vertex_map, expected);
            assert_eq!(edge_map, expected);
            assert_eq!(properties[0].1, CanonicalScalar::Int(7));
            assert_eq!(properties[1].1, CanonicalScalar::Int(8));
            assert!(std::ptr::eq(
                vertex.property(PropertyKeyId(1)).unwrap(),
                &updated
            ));
            assert!(std::ptr::eq(
                edge.property(PropertyKeyId(3)).unwrap(),
                &added
            ));
        }
        #[test]
        fn label_edits_override_membership_without_mutating_the_borrowed_row() {
            let labels = [LabelId(2), LabelId(4)];
            let mut row = VertexView::new(&labels, &[]);
            row.label_edits.insert(LabelId(2), false);
            row.label_edits.insert(LabelId(3), true);
            assert!(!row.matches(&VertexPredicate::HasLabel(LabelId(2))));
            assert!(row.matches(&VertexPredicate::HasLabel(LabelId(3))));
            assert!(row.matches(&VertexPredicate::HasLabel(LabelId(4))));
            assert_eq!(labels, [LabelId(2), LabelId(4)]);
        }
        #[test]
        fn scalar_and_null_predicates_read_the_canonical_override_not_the_old_type() {
            use fgdb_gql::algebra::ScalarPredicate;
            let key = PropertyKeyId(1);
            let properties = [(key, CanonicalScalar::ucs_basic_text("old value").unwrap())];
            let changed = CanonicalScalar::Bool(false);
            let null = CanonicalScalar::Null;
            let mut row = VertexView::new(&[], &properties);
            let scalar = VertexPredicate::ScalarProperty {
                key,
                predicate: ScalarPredicate::new(changed.clone(), IntegerComparison::Equal).unwrap(),
            };
            let absent = VertexPredicate::PropertyNull { key, is_null: true };
            assert!(!row.matches(&scalar));
            assert!(!row.matches(&absent));
            row.property_edits.insert(key, Some(&changed));
            assert!(row.matches(&scalar));
            assert!(!row.matches(&absent));
            assert!(std::ptr::eq(row.property(key).unwrap(), &changed));
            row.property_edits.insert(key, Some(&null));
            assert!(!row.matches(&scalar));
            assert!(row.matches(&absent));
            row.property_edits.insert(key, None);
            assert!(!row.matches(&scalar));
            assert!(row.matches(&absent));
            assert!(std::ptr::eq(&properties[0].1, &properties[0].1));
        }
    }
}
