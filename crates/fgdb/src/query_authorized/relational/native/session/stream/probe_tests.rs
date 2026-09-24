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
