//! Discover immutable names at the selected revision before query binding.
//!
//! Operator declarations extend the invocation's resolver only. Persisting
//! them is an explicit creation option routed through the native transaction.

use super::{Failure, Options, execution_failure, open_failure, refusal};
use fgdb::{Database, DatabaseKeys, PinnedTzdb};
use fgdb_delta_types::catalog::{CatalogSymbol, SymbolCatalog};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{GqlMapArgumentText, GqlParameters, bind_graph_snapshot_selector};
use fgdb_types::{CommitCx, CommitSeq, QueryCx};
use std::collections::{BTreeMap, BTreeSet};

fn with_declarations(
    options: &Options,
    mut catalog: SymbolCatalog,
) -> Result<SymbolCatalog, Failure> {
    for (name, id) in &options.labels {
        catalog
            .bind(name, CatalogSymbol::Label(LabelId(*id)))
            .map_err(Failure::usage)?;
    }
    for (name, id) in &options.relations {
        catalog
            .bind(name, CatalogSymbol::Relation(RelationId(*id)))
            .map_err(Failure::usage)?;
    }
    for (name, id) in &options.properties {
        catalog
            .bind(name, CatalogSymbol::Property(PropertyKeyId(*id)))
            .map_err(Failure::usage)?;
    }
    Ok(catalog)
}

pub(super) fn declared(options: &Options) -> Result<SymbolCatalog, Failure> {
    with_declarations(options, SymbolCatalog::default())
}

/// Exact redeclarations are harmless; neither axis of an existing binding
/// may be changed. Build off-side so a conflict cannot partly alter Options.
pub(super) fn merge(options: &mut Options, durable: &SymbolCatalog) -> Result<(), Failure> {
    let merged = with_declarations(options, durable.clone())?;
    let (mut labels, mut relations, mut properties) =
        (BTreeMap::new(), BTreeMap::new(), BTreeMap::new());
    for (symbol, name) in merged.bindings() {
        match symbol {
            CatalogSymbol::Label(LabelId(id)) => labels.insert(name.to_owned(), id),
            CatalogSymbol::Relation(RelationId(id)) => relations.insert(name.to_owned(), id),
            CatalogSymbol::Property(PropertyKeyId(id)) => properties.insert(name.to_owned(), id),
        };
    }
    options.labels = labels;
    options.relations = relations;
    options.properties = properties;
    Ok(())
}

pub(super) fn text_cut(
    statement: &str,
    parameters: &GqlParameters,
) -> Result<Option<CommitSeq>, Failure> {
    let lowered = GqlMapArgumentText::lower_fields(statement, parameters);
    let (statement, parameters) = lowered.as_ref().map_or(
        (statement, parameters),
        |(text, parameters)| (text.text(), parameters),
    );
    bind_graph_snapshot_selector(statement, parameters).map_err(Failure::statement)
}

fn requested_cuts(
    command: &str,
    options: &Options,
    resolver: Option<&PinnedTzdb>,
    cx: &QueryCx,
) -> Result<Vec<Option<CommitSeq>>, Failure> {
    match command {
        "query" if options.weighted_path.enabled() => Ok(vec![options.weighted_path.as_of()]),
        "query" if options.fnx_call => Ok(vec![options.fnx.as_of()]),
        "query" => {
            let text = text_cut(&options.text, &options.params)?;
            let flag = options.fnx.as_of();
            if let (Some(text), Some(flag)) = (text, flag)
                && text != flag
            {
                return Err(Failure::coded(
                    refusal::Code::InvalidValue,
                    "the query and --as-of select different snapshots",
                ));
            }
            Ok(vec![text.or(flag)])
        }
        "search" => Ok(vec![options.search.as_of()]),
        "diff" => Ok(options.diff.catalog_cuts()?.into_iter().map(Some).collect()),
        "transaction" => super::transaction::catalog_cuts(options, resolver, cx),
        _ => Ok(vec![None]),
    }
}

/// A current checkpoint needs only authenticated catalog metadata. Ordinary
/// admission may first recover publication of previously committed effects;
/// bounded hosts require a current checkpoint and never use that fallback.
/// An open failure is retained until the caller finishes its input preflight,
/// then returned before execution. Catalog selection/merge failures are
/// immediate and never become a fallback resolver.
pub(super) async fn discover(
    command: &str,
    options: &mut Options,
    keys: DatabaseKeys,
    resolver: Option<&PinnedTzdb>,
    open_cx: &CommitCx,
    cx: &QueryCx,
) -> Result<Option<Failure>, Failure> {
    let requested = requested_cuts(command, options, resolver, cx)?;
    // Bounded graph hosts must never recover through a resident graph while
    // discovering names. They retain the same current-checkpoint requirement
    // as their eventual buffered/sealed source open.
    let opened = if options.buffered.enabled() || options.fnx.is_sealed() {
        Database::open_current_symbol_catalog(open_cx, &options.db, keys)
            .await
            .map_err(|error| match error {
                fgdb::SymbolCatalogOpenError::Open(source) => open_failure(*source),
                error => Failure::open(error),
            })
    } else {
        Database::open_symbol_catalog(open_cx, &options.db, keys)
            .await
            .map_err(open_failure)
    };
    let view = match opened {
        Ok(view) => view,
        Err(error) => return Ok(Some(error)),
    };
    let mut cuts: BTreeSet<_> = requested
        .into_iter()
        .map(|cut| cut.unwrap_or(view.frontier()))
        .collect();
    if cuts.is_empty() {
        cuts.insert(view.frontier());
    }
    let selected = view
        .symbol_catalog_at(*cuts.first().expect("at least one cut"))
        .map_err(execution_failure)?;
    // Diff and an ordered transaction share one prepared resolver. It cannot
    // truthfully answer reverse metadata at two distinct visible catalogs.
    // Floor-only changes do not affect binding or introspection.
    for cut in cuts.iter().skip(1) {
        cx.checkpoint().map_err(Failure::query)?;
        let other = view.symbol_catalog_at(*cut).map_err(execution_failure)?;
        if !selected.bindings().eq(other.bindings()) {
            return Err(Failure::coded(
                refusal::Code::Unsupported,
                "this request spans snapshots with different symbol catalogs; query each snapshot separately",
            ));
        }
    }
    merge(options, &selected)?;
    options.catalog_frontier = Some(view.frontier());
    Ok(None)
}

/// A second open must not silently pair prepared names with another current
/// generation. Explicitly pinned sealed readers use their existing at-seq
/// argument; other hosts check this before executing or emitting any rows.
pub(super) fn check_frontier(options: &Options, actual: CommitSeq) -> Result<(), Failure> {
    if options.catalog_frontier.is_some_and(|expected| expected != actual) {
        return Err(Failure::coded(
            refusal::Code::Conflict,
            "the database changed while binding its symbol catalog; retry the invocation",
        ));
    }
    Ok(())
}
