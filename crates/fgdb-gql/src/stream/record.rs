//! One source-owned record for the existing root scan, not a table or cache.
//! Borrowed sources retain their zero-copy behavior. A trusted source may mask
//! fields before the SAME predicate and projection machinery borrows them.

use super::{VertexScanEvent, VertexScanRow};
use fgdb_delta_types::{LabelId, PropertyKeyId};
use fgdb_types::CanonicalScalar;

/// Root fields from one admitted vertex. Both forms obey VertexScanRow's sorted,
/// unique metadata contract. Owning fields confers no source authentication;
/// the host must still resolve visibility and enforce topology authorization.
pub enum VertexScanRecord<'a> {
    Borrowed(VertexScanRow<'a>),
    Owned {
        labels: Vec<LabelId>,
        properties: Vec<(PropertyKeyId, CanonicalScalar)>,
    },
}
impl VertexScanRecord<'_> {
    pub fn as_row(&self) -> VertexScanRow<'_> {
        match self {
            Self::Borrowed(row) => *row,
            Self::Owned { labels, properties } => VertexScanRow { labels, properties },
        }
    }

    /// Preserve the source's metadata order while copying ONLY admitted fields.
    /// Hidden scalar payloads are never cloned or encoded for this operation.
    /// Each retained field and payload unit is admitted before its copy; a
    /// refusal releases no partially masked record. These are logical units,
    /// not allocator capacity, byte-memory isolation or a spill guarantee.
    pub fn copy_masked<E>(
        row: VertexScanRow<'_>,
        mut allows_label: impl FnMut(LabelId) -> bool,
        mut allows_property: impl FnMut(PropertyKeyId) -> bool,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), E>,
    ) -> Result<Self, E> {
        // Masked labels and properties are skipped before any charge: how many
        // a visible record carries is itself hidden data (FG-INV-20).
        let mut labels = Vec::new();
        for &label in row.labels {
            if allows_label(label) {
                control(VertexScanEvent::Work)?;
                control(VertexScanEvent::ScratchEntry)?;
                labels.push(label);
            }
        }
        let mut properties = Vec::new();
        for (key, value) in row.properties {
            if allows_property(*key) {
                control(VertexScanEvent::Work)?;
                control(VertexScanEvent::ScratchEntry)?;
                crate::algebra_exec::charge_payload(value, &mut |_| {
                    control(VertexScanEvent::Work)?;
                    control(VertexScanEvent::ScratchEntry)
                })?;
                properties.push((*key, value.clone()));
            }
        }
        Ok(Self::Owned { labels, properties })
    }
}
impl core::fmt::Debug for VertexScanRecord<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("VertexScanRecord([REDACTED])")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algebra::{GraphValue, GraphValueRow};
    use crate::stream::{
        VertexScanCursor, VertexScanPlan, VertexScanSource, VertexScanSourceError, VertexScanState,
    };
    use crate::{
        GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind,
        PreparedGraphText,
    };
    use fgdb_types::{CommitSeq, VId};

    struct Source {
        sent: bool,
        labels: Vec<LabelId>,
        properties: Vec<(PropertyKeyId, CanonicalScalar)>,
    }
    impl VertexScanSource for Source {
        type Error = ();
        fn snapshot_seq(&self) -> CommitSeq {
            CommitSeq(3)
        }
        fn next_vertex<C>(
            &mut self,
            control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
        ) -> Result<Option<VId>, VertexScanSourceError<(), C>> {
            control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
            Ok((!std::mem::replace(&mut self.sent, true)).then_some(VId(7)))
        }
        fn vertex<'a, C>(
            &'a self,
            _: VId,
            control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
        ) -> Result<Option<VertexScanRow<'a>>, VertexScanSourceError<(), C>> {
            control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
            Ok(Some(VertexScanRow {
                labels: &self.labels,
                properties: &self.properties,
            }))
        }
    }
    struct Masked(Source);
    impl VertexScanSource for Masked {
        type Error = ();
        fn snapshot_seq(&self) -> CommitSeq {
            self.0.snapshot_seq()
        }
        fn next_vertex<C>(
            &mut self,
            control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
        ) -> Result<Option<VId>, VertexScanSourceError<(), C>> {
            self.0.next_vertex(control)
        }
        fn vertex<'a, C>(
            &'a self,
            _: VId,
            _: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
        ) -> Result<Option<VertexScanRow<'a>>, VertexScanSourceError<(), C>> {
            panic!("root cursor must not request this source's unmasked borrowed record")
        }
        fn vertex_record<'a, C>(
            &'a self,
            vid: VId,
            control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
        ) -> Result<Option<VertexScanRecord<'a>>, VertexScanSourceError<(), C>> {
            self.0
                .vertex(vid, control)?
                .map(|row| {
                    VertexScanRecord::copy_masked(
                        row,
                        |label| label == LabelId(1),
                        |key| key == PropertyKeyId(1),
                        control,
                    )
                    .map_err(VertexScanSourceError::Control)
                })
                .transpose()
        }
    }
    fn source() -> Source {
        Source {
            sent: false,
            labels: vec![LabelId(1), LabelId(99)],
            properties: vec![
                (PropertyKeyId(1), CanonicalScalar::Int(8)),
                (
                    PropertyKeyId(2),
                    CanonicalScalar::ucs_basic_text(&"private".repeat(100)).unwrap(),
                ),
            ],
        }
    }
    fn plan(text: &str) -> VertexScanPlan<GraphValueRow> {
        let pattern = PreparedGraphText::prepare(text, |kind, name: &str| match (kind, name) {
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            (GraphSymbolKind::Property, "hidden") => Some(GraphSymbol::Property(PropertyKeyId(2))),
            (GraphSymbolKind::Label, "H") => Some(GraphSymbol::Label(LabelId(99))),
            _ => None,
        })
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        VertexScanPlan::compile(pattern.plan()).unwrap()
    }
    fn policy() -> GqlQueryPolicy {
        GqlQueryPolicy::new(100, 100, 100_000, 100_000)
    }

    #[test]
    fn default_record_is_the_same_borrow_and_does_not_add_source_events() {
        let source = source();
        let mut events = Vec::new();
        let record = source
            .vertex_record(VId(7), &mut |event| {
                events.push(event);
                Ok::<_, ()>(())
            })
            .unwrap()
            .unwrap();
        assert!(matches!(record, VertexScanRecord::Borrowed(_)));
        assert!(std::ptr::eq(
            record.as_row().properties.as_ptr(),
            source.properties.as_ptr()
        ));
        assert_eq!(events, vec![VertexScanEvent::Work]);
    }

    #[test]
    fn owned_masking_precedes_boolean_predicates_and_projections_in_the_existing_cursor() {
        let query = "MATCH (n) WHERE n.hidden IS NULL OR n.p = 100 RETURN n AS id, n.p AS p, n.hidden AS hidden";
        let mut raw = VertexScanCursor::new(source(), plan(query), policy(), || Ok::<_, ()>(()));
        assert!(
            raw.next().is_none(),
            "unmasked negative control sees the hidden property"
        );
        let mut masked =
            VertexScanCursor::new(Masked(source()), plan(query), policy(), || Ok::<_, ()>(()));
        assert_eq!(
            masked.next().unwrap().unwrap().values(),
            &[
                GraphValue::Vertex(VId(7)),
                GraphValue::Scalar(CanonicalScalar::Int(8)),
                GraphValue::Scalar(CanonicalScalar::Null),
            ]
        );
        assert!(masked.next().is_none());
        let mut masked = VertexScanCursor::new(
            Masked(source()),
            plan("MATCH (n:H) RETURN n AS id"),
            policy(),
            || Ok::<_, ()>(()),
        );
        assert!(masked.next().is_none());
    }

    #[test]
    fn each_record_copy_cut_refuses_without_output_and_hidden_payload_size_adds_no_copy_work() {
        let large = source();
        let tiny = Source {
            properties: vec![
                (PropertyKeyId(1), CanonicalScalar::Int(8)),
                (PropertyKeyId(2), CanonicalScalar::Int(0)),
            ],
            ..source()
        };
        let copy = |source: &Source, stop: usize| {
            let mut calls = 0;
            let result = VertexScanRecord::copy_masked(
                VertexScanRow {
                    labels: &source.labels,
                    properties: &source.properties,
                },
                |id| id == LabelId(1),
                |key| key == PropertyKeyId(1),
                &mut |_| {
                    calls += 1;
                    if calls == stop { Err(stop) } else { Ok(()) }
                },
            );
            (result, calls)
        };
        let (result, count) = copy(&large, usize::MAX);
        let copied = result.unwrap();
        assert_eq!(copied.as_row().labels, &[LabelId(1)]);
        assert_eq!(
            copied.as_row().properties,
            &[(PropertyKeyId(1), CanonicalScalar::Int(8))]
        );
        assert_eq!(copy(&tiny, usize::MAX).1, count);
        for cut in 1..=count {
            assert!(matches!(copy(&large, cut), (Err(at), calls) if at == cut && calls == cut));
        }
        let mut calls = 0;
        let visible = VertexScanRecord::copy_masked(
            VertexScanRow {
                labels: &large.labels,
                properties: &large.properties,
            },
            |_| true,
            |_| true,
            &mut |_| {
                calls += 1;
                Ok::<_, ()>(())
            },
        )
        .unwrap();
        assert_eq!(visible.as_row().properties, large.properties);
        assert!(
            calls > count,
            "visible scalar payload copies must spend their payload units"
        );
    }

    #[test]
    fn owned_record_cursor_retains_exact_native_quotas_and_fuses_every_interruption() {
        let text = "MATCH (n) RETURN n AS id, n.p AS p";
        let mut calls = 0;
        let (rows, stats) = {
            let mut cursor = VertexScanCursor::new(Masked(source()), plan(text), policy(), || {
                calls += 1;
                Ok::<_, usize>(())
            });
            let rows = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
            (rows, cursor.evaluator_stats())
        };
        for cut in 1..=calls {
            let mut seen = 0;
            let mut cursor = VertexScanCursor::new(Masked(source()), plan(text), policy(), || {
                seen += 1;
                if seen == cut { Err(cut) } else { Ok(()) }
            });
            let mut prefix = Vec::new();
            loop {
                match cursor.next() {
                    Some(Ok(row)) => prefix.push(row),
                    Some(Err(GqlQueryError::Interrupted(at))) => {
                        assert_eq!(at, cut);
                        break;
                    }
                    other => panic!("expected exact injected interruption, got {other:?}"),
                }
            }
            assert!(rows.starts_with(&prefix));
            assert_eq!(cursor.state(), VertexScanState::Failed);
            assert!(cursor.next().is_none());
        }
        let exact = GqlQueryPolicy::new(1, 1, stats.work_units, stats.scratch_entries);
        let mut cursor =
            VertexScanCursor::new(Masked(source()), plan(text), exact, || Ok::<_, ()>(()));
        assert_eq!(
            cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
            rows
        );
        for budget in [
            GqlQueryPolicy::new(1, 1, stats.work_units - 1, stats.scratch_entries),
            GqlQueryPolicy::new(1, 1, stats.work_units, stats.scratch_entries - 1),
        ] {
            let mut cursor =
                VertexScanCursor::new(Masked(source()), plan(text), budget, || Ok::<_, ()>(()));
            assert!(cursor.by_ref().collect::<Result<Vec<_>, _>>().is_err());
            assert_eq!(cursor.state(), VertexScanState::Failed);
        }
    }
}
