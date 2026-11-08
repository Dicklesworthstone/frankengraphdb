//! Map projections after a row-fed write read the same admitted preimages as
//! individual RETURN fields. Only the mutation owner computes their final state.

use super::*;

impl<'a> ParsedMutationReturn<'a> {
    pub(super) fn row_map_input(
        &mut self,
        parser: &Parser<'a>,
        row: &[(Name<'a>, GraphSetColumnType)],
        scope: ReturnScope,
        source: Name<'a>,
        input: ReturnMapInput<'a>,
    ) -> Result<usize, GraphPatternTextError> {
        // Shorthand entries may name a visible scalar or element, but never a
        // generated hidden column or a graph name that the WITH discarded.
        if parser.elements.contains(&source.text) {
            return Err(error(source.at, GraphPatternTextErrorKind::UnknownVariable));
        }
        let visible = parser.visible_width(row);
        let target = row[..visible]
            .iter()
            .position(|(name, _)| name.text == source.text)
            .ok_or_else(|| error(source.at, GraphPatternTextErrorKind::UnknownVariable))?;
        let kind = row[target].1;
        if matches!(input, ReturnMapInput::Identity) {
            return self.column(Binding::Input(target), source, kind);
        }
        if !matches!(kind, GraphSetColumnType::Vertex | GraphSetColumnType::Edge) {
            return Err(error(
                source.at,
                GraphPatternTextErrorKind::Expected("a retained vertex or relationship map source"),
            ));
        }
        if matches!(input, ReturnMapInput::Guard) {
            // A NULL element guards the entire literal projection, not just
            // its selected fields. This is the same identity guard as MATCH.
            return self.column(Binding::Input(target), source, kind);
        }
        if kind == GraphSetColumnType::Edge && scope.deleting {
            return Err(error(
                source.at,
                GraphPatternTextErrorKind::Expected(
                    "a vertex property after DETACH DELETE, which may cascade to edges",
                ),
            ));
        }
        let key = match input {
            ReturnMapInput::Property(key) => Some(key),
            ReturnMapInput::Properties => None,
            ReturnMapInput::Identity | ReturnMapInput::Guard => {
                unreachable!("identity and guard handled above")
            }
        };
        let whole_map = key.is_none();
        let current = parser
            .boundary_reads
            .as_ref()
            .filter(|boundary| boundary.width == row.len() && target < boundary.visible)
            .and_then(|boundary| {
                boundary.reads.iter().find_map(|&(name, field, column)| {
                    (name == source.text
                        && match key {
                            Some(key) => field.is_property(key.text),
                            None => field.is_properties(),
                        })
                    .then_some(column)
                })
            })
            .filter(|&column| {
                row.get(column).is_some_and(|(_, kind)| {
                    if whole_map {
                        *kind == GraphSetColumnType::Any
                    } else {
                        kind.is_scalar_like() || *kind == GraphSetColumnType::Any
                    }
                })
            })
            .ok_or_else(|| {
                error(
                    key.map_or(source.at, |key| key.at),
                    GraphPatternTextErrorKind::Expected(
                        "a property carried by the incoming WITH row",
                    ),
                )
            })?;
        if let Some(key) = key {
            self.column(
                Binding::Property {
                    target,
                    key,
                    current,
                },
                key,
                GraphSetColumnType::Stored,
            )
        } else {
            self.column(
                Binding::Properties { target, current },
                source,
                GraphSetColumnType::Any,
            )
        }
    }
}
