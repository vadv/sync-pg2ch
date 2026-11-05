//! Orchestration: collect catalogs, compare, then verify metadata did not change.
use std::time::Duration;

use crate::{
    catalog::{CatalogError, clickhouse::ClickHouseCatalog, postgres::PostgresCatalog},
    config::{Config, SourceTable, TablePair, env_value},
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
    let mut pairs = Vec::with_capacity(targets.len());
    for target in &targets {
        pairs.push(TablePair {
            source: SourceTable {
                schema: config.postgres.schema.clone(),
                table: target.table.clone(),
            },
            target: target.clone(),
            ignore_target_columns: config.comparison.ignore_target_columns.clone(),
        });
    }
    let mut snapshots = Vec::with_capacity(pairs.len());
    for pair in &pairs {
        snapshots.push(read_pair(&mut postgres, &clickhouse, pair, config.query_timeout()).await);
    }
    for (pair, snapshot) in pairs.iter().zip(snapshots) {
        let (source, target) = match snapshot {
            Ok(snapshot) => snapshot,
            Err(error) => {
                report.tables.push(failed_table(pair, &error));
                report.errors.push(error);
                continue;
            }
        };
        let mut result = diff::compare(pair, source.as_ref(), target.as_ref());
        let second = read_pair(&mut postgres, &clickhouse, pair, config.query_timeout()).await;
        let error = match second {
            Ok((ref pg, ref ch)) if pg == &source && ch == &target => None,
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

async fn read_pair(
    pg: &mut PostgresCatalog,
    ch: &ClickHouseCatalog,
    pair: &TablePair,
    deadline: Duration,
) -> Result<(Option<PgTable>, Option<ChTable>), CatalogError> {
    let (source, target) = tokio::join!(
        bounded(deadline, pg.table(&pair.source)),
        bounded(deadline, ch.table(&pair.target))
    );
    Ok((source?, target?))
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

fn failed_table(pair: &TablePair, error: &CatalogError) -> TableDiff {
    TableDiff {
        source: pair.source.clone(),
        target: pair.target.clone(),
        status: Status::Unknown,
        findings: vec![error_finding(error)],
        ignored_target_columns: pair.ignore_target_columns.clone(),
    }
}
