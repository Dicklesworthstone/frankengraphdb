//! Keep immediate preparation futures Send while opting cooperative callers
//! into the projection constructor's resumable state and runtime yield.

use super::*;
use std::future::{Future, Ready, ready};
use std::pin::Pin;

pub(crate) trait SealedBuild {
    type Build<'a>: Future<Output = Result<SealedGraphView, SealedProjectionError>>
    where
        Self: 'a;

    fn build<'a>(
        &'a mut self,
        cx: &'a QueryCx,
        partition: &'a SealedPartition,
        vertices: &'a [VId],
        config: SealedProjectionSpec,
        limits: ProjectionLimits,
    ) -> Self::Build<'a>;
}

pub(crate) struct ImmediateSealedBuild;

impl SealedBuild for ImmediateSealedBuild {
    type Build<'a> = Ready<Result<SealedGraphView, SealedProjectionError>>;

    fn build<'a>(
        &'a mut self,
        cx: &'a QueryCx,
        partition: &'a SealedPartition,
        vertices: &'a [VId],
        config: SealedProjectionSpec,
        limits: ProjectionLimits,
    ) -> Self::Build<'a> {
        ready(SealedGraphView::build(
            cx, partition, vertices, config, limits,
        ))
    }
}

pub(crate) struct CooperativeSealedBuild(pub(crate) NonZeroUsize);

impl SealedBuild for CooperativeSealedBuild {
    type Build<'a> =
        Pin<Box<dyn Future<Output = Result<SealedGraphView, SealedProjectionError>> + 'a>>;

    fn build<'a>(
        &'a mut self,
        cx: &'a QueryCx,
        partition: &'a SealedPartition,
        vertices: &'a [VId],
        config: SealedProjectionSpec,
        limits: ProjectionLimits,
    ) -> Self::Build<'a> {
        Box::pin(SealedGraphView::build_cooperative(
            cx,
            partition,
            vertices,
            config,
            limits,
            self.0,
            asupersync::runtime::yield_now::yield_now,
        ))
    }
}
