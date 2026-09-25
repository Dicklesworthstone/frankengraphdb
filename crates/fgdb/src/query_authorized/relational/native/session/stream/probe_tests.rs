//! Denied relation admission must not touch even the endpoint's history.
use super::*;
use asupersync::security::key::AuthKey;
use fgdb_delta_types::SchemaEpoch;
use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};
use fgdb_warden::{Authority, Grant, QueryLimits, Scope};

struct NoReads;
impl VertexScanSource for NoReads {
    type Error = ReadError;
    fn snapshot_seq(&self) -> CommitSeq {
        CommitSeq(0)
    }
    fn next_vertex<C>(
        &mut self,
        _: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VId>, VertexScanSourceError<ReadError, C>> {
        panic!("unexpected root lookup")
    }
    fn vertex<'a, C>(
        &'a self,
        _: VId,
        _: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'a>>, VertexScanSourceError<ReadError, C>> {
        panic!("denied relation opened endpoint history")
    }
    fn next_probe_edge<C>(
        &self,
        _: VId,
        _: fgdb_gql::algebra::GlaDirection,
        _: Option<EId>,
        _: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeExpansionSourceError<ReadError, C>> {
        panic!("denied relation opened incidence history")
    }
    fn probe_edge<'a, C>(
        &'a self,
        _: EId,
        _: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EdgeScanRow<'a>>, EdgeExpansionSourceError<ReadError, C>> {
        panic!("denied relation opened edge history")
    }
}

#[test]
fn deny_before_endpoint_and_incidence_still_checks_the_live_permit() {
    let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    let c = PurposeContexts::narrow_runtime_root(&root);
    let cx = c.query();
    let issuer = Authority::new(
        AuthKey::from_seed(7302),
        DatabaseSecurityNamespaceId([1; 32]),
        "graph",
        SchemaEpoch(0),
        1,
    )
    .unwrap();
    let mut grant = Grant::read_only(
        "branch",
        1000,
        QueryLimits {
            max_nodes: 0,
            max_work: 100,
            max_rows: 0,
        },
    );
    grant.labels = Scope::All; // All relations remain denied.
    let token = issuer.issue_at(&grant, 100).unwrap();
    let verified = issuer.verify_at(&token, "branch", 100).unwrap();
    let permit = verified.begin_read_at("branch", 100).unwrap();
    let execution: Shared<'_> =
        Rc::new(RefCell::new(Execution::new(&cx, permit, Box::new(|| 100))));
    let source = ScopedSource {
        inner: NoReads,
        execution: Rc::clone(&execution),
    };
    let mut control = |_| execution.borrow_mut().checkpoint();
    for direction in [
        fgdb_gql::algebra::GlaDirection::Forward,
        fgdb_gql::algebra::GlaDirection::Reverse,
        fgdb_gql::algebra::GlaDirection::Undirected,
    ] {
        for after in [None, Some(EId(u128::MAX))] {
            assert!(
                source
                    .next_probe_edge_for_relation(
                        VId(u128::MAX),
                        RelationId(7),
                        direction,
                        after,
                        &mut control
                    )
                    .unwrap()
                    .is_none()
            );
        }
    }
    assert_eq!(execution.borrow().permit.usage().nodes, 0);
    issuer.retire();
    assert!(matches!(
        source.next_probe_edge_for_relation(
            VId(0),
            RelationId(7),
            fgdb_gql::algebra::GlaDirection::Forward,
            None,
            &mut control
        ),
        Err(EdgeExpansionSourceError::Read(
            VertexScanSourceError::Control(QueryError::Authorization(
                AuthorizationError::AuthorityRetired
            ))
        ))
    ));
}

#[test]
fn probe_error_translation_preserves_source_authority_and_structural_causes() {
    assert!(matches!(
        probe_error(EdgeScanError::Source(QueryError::Authorization(
            AuthorizationError::Expired
        ))),
        QueryError::Authorization(AuthorizationError::Expired)
    ));
    for (number, error) in [
        EdgeScanError::ExpansionUnavailable,
        EdgeScanError::DanglingEndpoint,
        EdgeScanError::BoundEdgeUnavailable,
        EdgeScanError::NonIncreasingIdentity,
        EdgeScanError::CounterExhausted,
    ]
    .into_iter()
    .enumerate()
    {
        let QueryError::Stream(GqlQueryError::Source(VertexScanError::Probe(error))) =
            probe_error(error)
        else {
            panic!("structural probe refusal was flattened")
        };
        assert!(matches!(
            (number, error),
            (0, EdgeScanError::ExpansionUnavailable)
                | (1, EdgeScanError::DanglingEndpoint)
                | (2, EdgeScanError::BoundEdgeUnavailable)
                | (3, EdgeScanError::NonIncreasingIdentity)
                | (4, EdgeScanError::CounterExhausted)
        ));
    }
}

// The source intentionally has hidden fields between the permitted keys. It
// lends an already-selected historical row; masking remains the product code's
// responsibility, not the fixture's. No raw payload is copied by field lookup.
struct PropertyRow {
    fields: Vec<(PropertyKeyId, CanonicalScalar)>,
}
impl VertexScanSource for PropertyRow {
    type Error = ReadError;
    fn snapshot_seq(&self) -> CommitSeq {
        CommitSeq(7)
    }
    fn next_vertex<C>(
        &mut self,
        _: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VId>, VertexScanSourceError<ReadError, C>> {
        panic!("a field probe must not restart the root scan")
    }
    fn vertex<'a, C>(
        &'a self,
        vid: VId,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'a>>, VertexScanSourceError<ReadError, C>> {
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        Ok((vid == VId(7)).then_some(VertexScanRow {
            labels: &[],
            properties: &self.fields,
        }))
    }
}

// Return public field semantics and exact native/signed usage. The clock is
// constant: this tests the resource channel, not wall-clock noninterference.
fn property_observation(
    cx: &QueryCx,
    issuer: &Authority,
    fields: &[(PropertyKeyId, CanonicalScalar)],
    vid: VId,
    key: PropertyKeyId,
    limit: u64,
) -> (Result<Option<Option<CanonicalScalar>>, QueryError>, u64, u64, u64) {
    let mut grant = Grant::read_only(
        "branch",
        1000,
        QueryLimits { max_nodes: 1, max_work: limit, max_rows: 0 },
    );
    grant.labels = Scope::All;
    // Some allowed keys are intentionally absent, before/between/after fields.
    grant.properties = Scope::only([0, 17, 25, 33, 200].map(PropertyKeyId));
    let token = issuer.issue_at(&grant, 100).unwrap();
    let verified = issuer.verify_at(&token, "branch", 100).unwrap();
    let permit = verified.begin_read_at("branch", 100).unwrap();
    let execution: Shared<'_> =
        Rc::new(RefCell::new(Execution::new(cx, permit, Box::new(|| 100))));
    let source = ScopedSource {
        inner: PropertyRow { fields: fields.to_vec() },
        execution: Rc::clone(&execution),
    };
    let mut work = 0;
    let mut scratch = 0;
    let result = source.vertex_property(vid, key, &mut |event| {
        match event {
            VertexScanEvent::Work => work += 1,
            VertexScanEvent::ScratchEntry => scratch += 1,
        }
        execution.borrow_mut().checkpoint()
    }).map(|value| value.map(|value| value.cloned())).map_err(|error| match error {
        VertexScanSourceError::Source(error) | VertexScanSourceError::Control(error) => error,
    });
    assert_eq!(scratch, 0, "borrowed fields must not clone a masked row");
    let execution = execution.borrow();
    let usage = execution.permit.usage();
    (result, work, usage.work, usage.nodes)
}

#[test]
fn field_probe_usage_and_exact_refusal_threshold_ignore_hidden_key_population() {
    let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let cx = contexts.query();
    let issuer = Authority::new(
        AuthKey::from_seed(7303), DatabaseSecurityNamespaceId([1; 32]),
        "graph", SchemaEpoch(0), 1,
    ).unwrap();
    let visible = vec![
        (PropertyKeyId(17), CanonicalScalar::Int(7)),
        (PropertyKeyId(33), CanonicalScalar::Int(9)),
    ];
    for size in [0, 1, 8, 32, 64, 128] {
        let mut hidden = visible.clone();
        for key in 1..=size {
            if ![17, 25, 33].contains(&key) {
                hidden.push((PropertyKeyId(key), CanonicalScalar::Int(-1)));
            }
        }
        hidden.sort_by_key(|(key, _)| *key);
        for key in [0, 17, 25, 33, 63, 200].map(PropertyKeyId) {
            let (expected, native, signed, nodes) =
                property_observation(&cx, &issuer, &visible, VId(7), key, 1000);
            let expected = expected.unwrap();
            let (got, actual_native, actual_signed, actual_nodes) =
                property_observation(&cx, &issuer, &hidden, VId(7), key, 1000);
            assert_eq!(got.unwrap(), expected);
            assert_eq!((actual_native, actual_signed, actual_nodes), (native, signed, nodes));
            assert_eq!(nodes, 1);
            for fields in [&visible, &hidden] {
                assert_eq!(
                    property_observation(&cx, &issuer, fields, VId(7), key, signed).0.unwrap(),
                    expected,
                );
                assert!(matches!(
                    property_observation(&cx, &issuer, fields, VId(7), key, signed - 1).0,
                    Err(QueryError::Authorization(AuthorizationError::LimitExceeded(
                        fgdb_warden::LimitDimension::Work
                    )))
                ));
            }
        }
    }
    let (missing, native, signed, nodes) =
        property_observation(&cx, &issuer, &visible, VId(8), PropertyKeyId(17), 0);
    assert_eq!(missing.unwrap(), None);
    assert_eq!((native, signed, nodes), (0, 0, 0));
    // A present vertex with an absent OR denied field remains SQL NULL, never
    // absence of the vertex or a leaked hidden value.
    for key in [25, 63].map(PropertyKeyId) {
        assert_eq!(
            property_observation(&cx, &issuer, &visible, VId(7), key, 1000).0.unwrap(),
            Some(None),
        );
    }
}
