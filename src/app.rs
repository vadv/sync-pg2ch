//! Orchestration: collect catalogs, compare, then verify metadata did not change.
use std::time::Duration;

use crate::{
    catalog::{CatalogError, clickhouse::ClickHouseCatalog, postgres::PostgresCatalog},
    config::{Config, SourceTable, TablePair, TargetTable, env_value},
    diff,
    model::{ChTable, Finding, PgTable, Status, TableDiff},
    report::Report,
};

/// Run a configured check without issuing table DDL, DML or user-row queries.
pub async fn run(config: &Config) -> Report {
    let mut report = Report::default();
    let postgres_url = match env_value(&config.postgres.url_env) {
        Ok(url) => url,
        Err(error) => {
            report
                .errors
                .push(CatalogError::new("configuration_error", error.to_string()));
            report.finish();
            return report;
        }
    };
    let clickhouse = ClickHouseCatalog::new(&config.clickhouse, config.connect_timeout());
    let postgres = PostgresCatalog::connect(&postgres_url, config.connect_timeout()).await;
    let (mut postgres, clickhouse) = match (postgres, clickhouse) {
        (Ok(postgres), Ok(clickhouse)) => (postgres, clickhouse),
        (Err(pg), Err(ch)) => {
            report.errors.extend([pg, ch]);
            report.finish();
            return report;
        }
        (Err(error), Ok(_)) | (Ok(_), Err(error)) => {
            report.errors.push(error);
            report.finish();
            return report;
        }
    };
    let (pg, ch) = tokio::join!(
        bounded(config.query_timeout(), postgres.versions()),
        bounded(config.query_timeout(), clickhouse.version_and_visibility())
    );
    match pg {
        Ok((version, extensions)) => {
            report.servers.postgres = Some(version);
            report.servers.extensions = extensions;
        }
        Err(error) => report.errors.push(error),
    }
    match ch {
        Ok(version) => report.servers.clickhouse = Some(version),
        Err(error) => report.errors.push(error),
    }
    if !report.errors.is_empty() {
        report.finish();
        return report;
    }
    let targets = match bounded(
        config.query_timeout(),
        clickhouse.tables(&config.clickhouse.database),
    )
    .await
    {
        Ok(tables) if !tables.is_empty() => tables,
        Ok(_) => {
            report.errors.push(CatalogError::new(
                "no_tables_discovered",
                "configured ClickHouse database contains no persistent tables to compare",
            ));
            report.finish();
            return report;
        }
        Err(error) => {
            report.errors.push(error);
            report.finish();
            return report;
        }
    };
    let mut snapshots = Vec::with_capacity(targets.len());
    for target in &targets {
        snapshots.push(read_snapshot(&mut postgres, &clickhouse, target, config).await);
    }
    for (target, snapshot) in targets.iter().zip(snapshots) {
        let snapshot = match snapshot {
            Ok(snapshot) => snapshot,
            Err(error) => {
                report.tables.push(failed_table(target, config, &error));
                report.errors.push(error);
                continue;
            }
        };
        let mut result = compare_snapshot(target, &snapshot, config);
        let second = read_snapshot(&mut postgres, &clickhouse, target, config).await;
        let error = match second {
            Ok(ref second) if second.candidates != snapshot.candidates => Some(CatalogError::new(
                "source_mapping_changed_during_check",
                "PostgreSQL source candidates changed between reads; rerun after DDL completes",
            )),
            Ok(ref second) if second == &snapshot => None,
            Ok(_) => Some(CatalogError::new(
                "schema_changed_during_check",
                "selected metadata changed between reads; rerun after DDL completes",
            )),
            Err(error) => Some(error),
        };
        if let Some(error) = error {
            result.status = Status::Unknown;
            result.findings.push(error_finding(&error));
            report.errors.push(error);
        }
        report.tables.push(result);
    }
    match bounded(config.query_timeout(), clickhouse.tables(&config.clickhouse.database)).await {
        Ok(tables) if tables == targets => {}
        Ok(_) => report.errors.push(CatalogError::new("table_set_changed_during_check", "the set of ClickHouse tables changed during discovery and comparison; rerun after DDL completes")),
        Err(error) => report.errors.push(error),
    }
    report.finish();
    report
}

async fn bounded<T>(
    deadline: Duration,
    future: impl std::future::Future<Output = Result<T, CatalogError>>,
) -> Result<T, CatalogError> {
    tokio::time::timeout(deadline, future)
        .await
        .map_err(|_| CatalogError::new("catalog_timeout", "catalog operation timed out"))?
}

#[derive(PartialEq, Eq)]
struct Snapshot {
    candidates: Vec<SourceTable>,
    source: Option<PgTable>,
    target: Option<ChTable>,
}

async fn read_snapshot(
    pg: &mut PostgresCatalog,
    ch: &ClickHouseCatalog,
    target: &TargetTable,
    config: &Config,
) -> Result<Snapshot, CatalogError> {
    let (source, target) = tokio::join!(
        bounded(config.query_timeout(), async {
            let candidates = pg
                .source_candidates(&config.postgres.schema, &target.table)
                .await?;
            let source = match candidates.as_slice() {
                [source] => pg.table(source).await?,
                [] | [_, _, ..] => None,
            };
            Ok((candidates, source))
        }),
        bounded(config.query_timeout(), ch.table(target))
    );
    let (candidates, source) = source?;
    Ok(Snapshot {
        candidates,
        source,
        target: target?,
    })
}

fn compare_snapshot(target: &TargetTable, snapshot: &Snapshot, config: &Config) -> TableDiff {
    let source = match snapshot.candidates.as_slice() {
        [source] => source.clone(),
        [] => SourceTable {
            schema: config.postgres.schema.clone(),
            table: target.table.clone(),
        },
        [_, _, ..] => {
            let error = CatalogError::new(
                "ambiguous_source_table",
                "both the literal table name and the PeerDB schema_table name match PostgreSQL relations; source selection is ambiguous",
            );
            let mut result = failed_table(target, config, &error);
            result.source_candidates = snapshot.candidates.clone();
            return result;
        }
    };
    let pair = TablePair {
        source,
        target: target.clone(),
        ignore_target_columns: config.comparison.ignore_target_columns.clone(),
    };
    let mut result = diff::compare(&pair, snapshot.source.as_ref(), snapshot.target.as_ref());
    result.source_candidates = snapshot.candidates.clone();
    if snapshot.candidates.is_empty() {
        let names = crate::catalog::postgres::source_names(&config.postgres.schema, &target.table);
        for finding in &mut result.findings {
            if finding.code == "source_table_missing" {
                finding.message = format!(
                    "no source relation in PostgreSQL schema {:?}; tried table names {:?}",
                    config.postgres.schema, names,
                );
            }
        }
    }
    result
}

fn error_finding(error: &CatalogError) -> Finding {
    Finding {
        column: None,
        code: error.code.into(),
        status: Status::Unknown,
        source: None,
        expected: None,
        target: None,
        message: error.message.clone(),
    }
}

fn failed_table(target: &TargetTable, config: &Config, error: &CatalogError) -> TableDiff {
    TableDiff {
        source: None,
        source_candidates: Vec::new(),
        target: target.clone(),
        status: Status::Unknown,
        findings: vec![error_finding(error)],
        ignored_target_columns: config.comparison.ignore_target_columns.clone(),
    }
}
