//! Discover immutable names at the selected revision before query binding.
//!
//! Query declarations extend the invocation's resolver only. The schema command
//! and explicit creation option publish names through the native transaction.

use super::{Failure, Options, execution_failure, open_failure, refusal};
use fgdb::{Database, DatabaseKeys, PinnedTzdb};
use fgdb_delta_types::catalog::{CatalogSymbol, CatalogSymbolKind, SymbolCatalog};
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

pub(super) fn has_declarations(options: &Options) -> bool {
    !options.symbol_names.is_empty()
        || !options.labels.is_empty()
        || !options.relations.is_empty()
        || !options.properties.is_empty()
}

fn check_expected(options: &Options, actual: CommitSeq) -> Result<(), Failure> {
    if options
        .if_current
        .is_some_and(|expected| expected != actual.0)
    {
        return Err(Failure::coded(
            refusal::Code::Conflict,
            "the expected frontier is no longer current; no schema registration was executed",
        ));
    }
    Ok(())
}

/// Resolve and publish the entire declaration set under the native writer's
/// exclusive ownership. Numeric declarations and automatic names share one
/// transaction, so a late conflict or exhausted namespace cannot partly land.
pub(super) async fn register<V: asupersync::fs::Vfs + Clone>(
    database: &mut Database<V>,
    cx: &CommitCx,
    options: &Options,
) -> Result<CommitSeq, Failure> {
    check_expected(options, database.frontier().map_err(Failure::io)?)?;
    let explicit = declared(options)?;
    if options.symbol_names.is_empty() {
        return database
            .register_symbols(cx, &explicit)
            .await
            .map_err(execution_failure);
    }
    let names: Vec<_> = options
        .symbol_names
        .iter()
        .map(|(kind, name)| (*kind, name.as_str()))
        .collect();
    database
        .register_symbol_names(cx, &explicit, &names)
        .await
        .map_err(execution_failure)
}

/// Catalog inspection uses authenticated metadata, including when graph blocks
/// are unavailable. The selected catalog and terminal sequence come from the
/// same immutable view; a historical request never becomes a write basis.
pub(super) async fn inspect(
    open_cx: &CommitCx,
    cx: &QueryCx,
    options: &Options,
    keys: DatabaseKeys,
    robot: bool,
    out: &mut impl std::io::Write,
) -> Result<(), Failure> {
    let view = Database::open_symbol_catalog(open_cx, &options.db, keys)
        .await
        .map_err(open_failure)?;
    check_expected(options, view.frontier())?;
    let at = options.schema_at.unwrap_or(view.frontier());
    let catalog = view.symbol_catalog_at(at).map_err(execution_failure)?;
    render_catalog(&catalog, at, cx, robot, out)
}

pub(super) fn render_catalog(
    catalog: &SymbolCatalog,
    at: CommitSeq,
    cx: &QueryCx,
    robot: bool,
    out: &mut impl std::io::Write,
) -> Result<(), Failure> {
    let mut rows = Vec::new();
    for (symbol, name) in catalog.bindings() {
        cx.checkpoint().map_err(Failure::io)?;
        let kind = match symbol.kind() {
            CatalogSymbolKind::Label => "label",
            CatalogSymbolKind::Relation => "relation",
            CatalogSymbolKind::Property => "property",
        };
        let ordinal_type = if i64::try_from(symbol.ordinal()).is_ok() {
            "int"
        } else {
            "wideint"
        };
        rows.push(if robot {
            vec![
                format!(r#"{{"type":"text","value":{}}}"#, super::quoted(kind)),
                format!(r#"{{"type":"text","value":{}}}"#, super::quoted(name)),
                format!(
                    r#"{{"type":"{ordinal_type}","value":"{}"}}"#,
                    symbol.ordinal()
                ),
            ]
        } else {
            vec![
                kind.to_owned(),
                name.to_owned(),
                symbol.ordinal().to_string(),
            ]
        });
    }
    super::render_rows(
        &["kind".to_owned(), "name".to_owned(), "id".to_owned()],
        rows,
        at.0,
        "rows",
        robot,
        out,
    )
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
    let (statement, parameters) = lowered
        .as_ref()
        .map_or((statement, parameters), |(text, parameters)| {
            (text.text(), parameters)
        });
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

pub(super) enum CatalogDiscovery {
    Ready,
    Buffered(Box<fgdb::BufferedReadPublication>),
    Unavailable {
        failure: Failure,
        admission_refused: bool,
    },
}

impl CatalogDiscovery {
    /// Syntax errors survive a failed open. A name lookup cannot establish
    /// absence when the memory or root-size limit prevented the catalog read.
    pub(super) fn preflight_error(&self, error: Failure) -> Failure {
        match self {
            Self::Unavailable {
                failure,
                admission_refused: true,
            } if error.refusal == refusal::Code::NotFound => failure.clone(),
            _ => error,
        }
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
) -> Result<CatalogDiscovery, Failure> {
    let requested = requested_cuts(command, options, resolver, cx)?;
    // Bounded graph hosts must never recover through a resident graph while
    // discovering names. They retain the same current-checkpoint requirement
    // as their eventual buffered/sealed source open.
    let mut admission_refused = false;
    let opened = if options.buffered.enabled() {
        async {
            let (pool, limits) = options.buffered.admission(options.budget.policy())?;
            let publication = Database::open_buffered_publication_with_vfs(
                open_cx,
                asupersync::fs::UnixVfs::new(),
                &options.db,
                keys,
                pool,
                limits,
            )
            .await
            .map_err(|error| {
                admission_refused = match &error {
                    fgdb::BufferedOpenError::Memory(_)
                    | fgdb::BufferedOpenError::Read(fgdb::BufferedReadError::Limit { .. }) => true,
                    fgdb::BufferedOpenError::Read(fgdb::BufferedReadError::Store(source)) => {
                        matches!(
                            source.as_ref(),
                            fgdb_strata::store::StoreError::ObjectTooLarge { .. }
                        )
                    }
                    _ => false,
                };
                super::buffered::open_failure(error)
            })?;
            let view = publication
                .symbol_catalog(open_cx)
                .await
                .map_err(open_failure)?;
            Ok((view, CatalogDiscovery::Buffered(Box::new(publication))))
        }
        .await
    } else if options.fnx.is_sealed() {
        Database::open_current_symbol_catalog(open_cx, &options.db, keys)
            .await
            .map(|view| (view, CatalogDiscovery::Ready))
            .map_err(|error| match error {
                fgdb::SymbolCatalogOpenError::Open(source) => open_failure(*source),
                error => Failure::open(error),
            })
    } else {
        Database::open_symbol_catalog(open_cx, &options.db, keys)
            .await
            .map(|view| (view, CatalogDiscovery::Ready))
            .map_err(open_failure)
    };
    let (view, discovery) = match opened {
        Ok(ready) => ready,
        Err(failure) => {
            return Ok(CatalogDiscovery::Unavailable {
                failure,
                admission_refused,
            });
        }
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
    Ok(discovery)
}

/// A second open must not silently pair prepared names with another current
/// generation. Explicitly pinned sealed readers use their existing at-seq
/// argument; other hosts check this before executing or emitting any rows.
pub(super) fn check_frontier(options: &Options, actual: CommitSeq) -> Result<(), Failure> {
    if options
        .catalog_frontier
        .is_some_and(|expected| expected != actual)
    {
        return Err(Failure::coded(
            refusal::Code::Conflict,
            "the database changed while binding its symbol catalog; retry the invocation",
        ));
    }
    Ok(())
}
