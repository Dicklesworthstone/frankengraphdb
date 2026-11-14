//! Read-only FGP transactions retain native authority without taking a writer
//! lock. The transport owns delivery and recovery; native owns the one permit.

use super::{Answer, Created, Refusal, finished};
use crate::execute::{convert, edge_upsert_output, query_refusal};
use crate::recovery::Generation;
use crate::{Served, TRUNK, unix_millis};
use asupersync::Cx;
use fgdb::AuthorizedReadTransaction;
use fgdb_protocol::body::{Body, ErrorCode, Execute, ExecuteMode, Outcome};
use fgdb_types::{CommitSeq, PurposeContexts};
use fgdb_warden::CapabilityToken;

pub(crate) struct NativeReadTransaction<'s> {
    db: &'s Served,
    owner: Option<Box<AuthorizedReadTransaction<'s, crate::Symbols, fn() -> u64>>>,
    generation: Generation,
    basis: CommitSeq,
}

impl<'s> NativeReadTransaction<'s> {
    pub(crate) async fn begin(
        cx: &Cx,
        db: &'s Served,
        token: &CapabilityToken,
    ) -> Result<Self, Refusal> {
        let _operation = db.db.enter().map_err(Refusal::from)?;
        let contexts = PurposeContexts::narrow_runtime_root(cx);
        let query = crate::query_cx(&contexts);
        let (view, generation) = db.db.read_view(cx).await.map_err(Refusal::from)?;
        let owner = view
            .begin_authorized_read(
                &query,
                &db.authority,
                token,
                TRUNK,
                db.symbols.clone(),
                db.query_policy,
                unix_millis as fn() -> u64,
            )
            .map_err(query_refusal)?;
        generation.check().map_err(Refusal::from)?;
        let basis = owner.basis();
        Ok(Self {
            db,
            owner: Some(Box::new(owner)),
            generation,
            basis,
        })
    }

    pub(crate) fn generation(&self) -> Generation {
        self.generation.clone()
    }

    pub(crate) const fn basis(&self) -> CommitSeq {
        self.basis
    }

    pub(crate) fn execute_framed(
        &mut self,
        cx: &Cx,
        request: &Execute,
        frame_budget: usize,
    ) -> Result<Answer, Refusal> {
        let mut owner = self.owner.take().ok_or_else(finished)?;
        let generation = self.generation.clone();
        let _operation = generation.enter().map_err(Refusal::from)?;
        if request.mode != ExecuteMode::Read {
            return Err(Refusal::new(
                ErrorCode::PermissionDenied,
                "a read-only transaction accepts only Read statements",
            ));
        }
        if owner.completed_statements() >= self.db.max_statements as u64 {
            return Err(Refusal::new(
                ErrorCode::Budget,
                "the explicit transaction statement limit was exceeded",
            ));
        }
        request
            .encode()
            .map_err(|error| Refusal::new(ErrorCode::Statement, error.to_string()))?;
        let contexts = PurposeContexts::narrow_runtime_root(cx);
        let query = crate::query_cx(&contexts);
        query
            .checkpoint()
            .map_err(|error| Refusal::new(ErrorCode::Cancelled, error.to_string()))?;
        let parameters = query.with_restriction(|| {
            convert::parameters(&request.parameters, None)
                .map_err(|error| Refusal::new(ErrorCode::Statement, error.to_string()))
        })?;
        let (columns, rows) = owner.query_with_output(
            &query,
            &request.statement,
            &parameters,
            |result, work| {
                edge_upsert_output::collect_query_with_frame_budget(result, work, frame_budget)
            },
            query_refusal,
        )?;
        generation.check().map_err(Refusal::from)?;
        self.owner = Some(owner);
        Ok(Answer {
            columns,
            rows,
            outcome: Outcome::Rows { seq: self.basis.0 },
            generation,
            created: Created::default(),
        })
    }

    pub(crate) fn commit(mut self, cx: &Cx) -> Result<Answer, Refusal> {
        let owner = self.owner.take().ok_or_else(finished)?;
        let generation = self.generation.clone();
        let _operation = generation.enter().map_err(Refusal::from)?;
        let statements = owner.completed_statements();
        let contexts = PurposeContexts::narrow_runtime_root(cx);
        let query = crate::query_cx(&contexts);
        let basis = (*owner).close(&query).map_err(query_refusal)?;
        generation.check().map_err(Refusal::from)?;
        Ok(Answer {
            columns: Vec::new(),
            rows: Vec::new(),
            outcome: Outcome::ReadClosed {
                seq: basis.0,
                statements,
            },
            generation,
            created: Created::default(),
        })
    }

    pub(crate) fn rollback(mut self) {
        if let Some(owner) = self.owner.take() {
            (*owner).rollback();
        }
    }
}
