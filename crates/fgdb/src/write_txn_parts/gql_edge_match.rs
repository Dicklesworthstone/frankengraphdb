impl WriteTxn {
    fn execute_prepared_edge_match<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        plan: &BoundPlan,
        edge_relation: RelationId,
    ) -> Result<Vec<VId>, WriteTxnError> {
        let graph = self.overlay_graph(database)?;
        let src_labeled = self.label_holders(database, &graph.vertices, plan.src_label)?;
        let dst_labeled = self.label_holders(database, &graph.vertices, plan.dst_label)?;
        let src_props = PropertyPredicateSets::build(
            self,
            database,
            &graph.vertices,
            [
                plan.src_prop,
                plan.src_prop_ne,
                plan.src_prop_gt,
                plan.src_prop_lt,
                plan.src_prop_ge,
                plan.src_prop_le,
            ],
        )?;
        let dst_props = PropertyPredicateSets::build(
            self,
            database,
            &graph.vertices,
            [
                plan.dst_prop,
                plan.dst_prop_ne,
                plan.dst_prop_gt,
                plan.dst_prop_lt,
                plan.dst_prop_ge,
                plan.dst_prop_le,
            ],
        )?;
        let hop2_dst_props = PropertyPredicateSets::build(
            self,
            database,
            &graph.vertices,
            [
                plan.hop2_dst_prop,
                plan.hop2_dst_prop_ne,
                plan.hop2_dst_prop_gt,
                plan.hop2_dst_prop_lt,
                plan.hop2_dst_prop_ge,
                plan.hop2_dst_prop_le,
            ],
        )?;
        let incoming_two_hop_dst_prop_on_anchor =
            plan.direction == fgdb_gql::EdgeDirection::Incoming && plan.hop2_relation.is_some();
        let anchors: Vec<VId> = graph
            .vertices
            .iter()
            .copied()
            .filter(|anchor| {
                src_labeled
                    .as_ref()
                    .is_none_or(|labeled| labeled.contains(anchor))
                    && src_props.keeps(anchor)
                    && (!incoming_two_hop_dst_prop_on_anchor || dst_props.keeps(anchor))
            })
            .collect();
        let destinations = crate::execute_bound_plan_over(plan, anchors, |source, relation| {
            let undirected = plan.direction == fgdb_gql::EdgeDirection::Undirected;
            let reverse =
                plan.direction == fgdb_gql::EdgeDirection::Incoming && plan.hop2_relation.is_some();
            let step = |anchor: VId, step_relation: RelationId| -> Vec<VId> {
                graph
                    .edges
                    .values()
                    .filter_map(|(edge_src, edge_relation, edge_dst)| {
                        if *edge_relation != step_relation {
                            return None;
                        }
                        if !reverse && *edge_src == anchor && graph.vertices.contains(edge_dst) {
                            Some(*edge_dst)
                        } else if (reverse || undirected)
                            && *edge_dst == anchor
                            && graph.vertices.contains(edge_src)
                        {
                            Some(*edge_src)
                        } else {
                            None
                        }
                    })
                    .collect()
            };
            let mut vias = step(source, relation);
            if plan.neq.is_some() {
                vias.retain(|via| *via != source);
            }
            if plan.eq.is_some() {
                vias.retain(|via| *via == source);
            }
            if let Some(labeled) = dst_labeled.as_ref() {
                vias.retain(|via| labeled.contains(via));
            }
            if !incoming_two_hop_dst_prop_on_anchor {
                vias.retain(|via| dst_props.keeps(via));
            }
            let Some(hop2_relation) = plan.hop2_relation else {
                return Ok(vias);
            };
            let hop2_step = |via: VId| {
                let mut far_ends = step(via, hop2_relation);
                far_ends.retain(|far_end| hop2_dst_props.keeps(far_end));
                far_ends
            };
            Ok(match plan.projection {
                fgdb_gql::ReturnProjection::Destination => vias
                    .into_iter()
                    .filter(|via| !hop2_step(*via).is_empty())
                    .collect(),
                fgdb_gql::ReturnProjection::Source
                | fgdb_gql::ReturnProjection::Hop2Destination => {
                    vias.into_iter().flat_map(hop2_step).collect()
                }
            })
        })?;
        let mut read_set = self.read_set.borrow_mut();
        read_set.extend(graph.observed);
        read_set.extend(destinations.iter().copied().map(ElementId::Vertex));
        drop(read_set);
        self.match_expansions
            .borrow_mut()
            .extend(graph.vertices.iter().copied().map(|src| (src, edge_relation)));
        Ok(destinations)
    }
}
