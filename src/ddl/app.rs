use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::{
    ExecutionState, Mode, PlanReport, PlanStatus, TablePlan, block, empty_table, plan_table,
    projected,
};
use crate::{
    app::{Snapshot, bounded, read_snapshot},
    catalog::{
        CatalogError,
        clickhouse::ClickHouseCatalog,
        ddl::{ExecuteError, Executor, Metadata},
        postgres::PostgresCatalog,
    },
    compatibility::same_target_type,
    config::{Config, TargetTable, env_value},
    model::{ChTable, Status},
};

#[derive(Debug, Clone, PartialEq, Eq)]
struct DdlSnapshot {
    schema: Snapshot,
    metadata: Metadata,
}

async fn read(
    postgres: &mut PostgresCatalog,
    clickhouse: &ClickHouseCatalog,
    target: &TargetTable,
    config: &Config,
) -> Result<DdlSnapshot, CatalogError> {
    let schema = read_snapshot(postgres, clickhouse, target, config).await?;
    let metadata = bounded(config.query_timeout(), clickhouse.ddl_metadata(target))
        .await?
        .ok_or_else(|| {
            CatalogError::new(
                "schema_changed_during_check",
                "ClickHouse table disappeared while reading DDL metadata",
            )
        })?;
    let Some(table) = &schema.target else {
        return Err(CatalogError::new(
            "schema_changed_during_check",
            "ClickHouse table appeared while reading DDL metadata",
        ));
    };
    if table.engine != metadata.table.engine || table.columns.len() != metadata.columns.len() {
        return Err(CatalogError::new(
            "schema_changed_during_check",
            "ClickHouse metadata changed while reading the table",
        ));
    }
    for (column, attributes) in table.columns.iter().zip(&metadata.columns) {
        if column.name != attributes.name
            || column.data_type != attributes.data_type
            || column.default_kind != attributes.default_kind
        {
            return Err(CatalogError::new(
                "schema_changed_during_check",
                "column metadata changed while reading the table",
            ));
        }
    }
    Ok(DdlSnapshot { schema, metadata })
}

fn failed_table(target: &TargetTable, error: &CatalogError) -> TablePlan {
    let mut table = empty_table(target);
    block(
        &mut table,
        None,
        error.code,
        Status::Unknown,
        &error.message,
    );
    table
}

/// Plan or apply ALTERs, checking the complete discovered scope before any write.
pub async fn run(config: &Config, mode: Mode) -> PlanReport {
    let mut report = PlanReport::new(mode);
    let connection = async {
        let url = env_value(&config.postgres.url_env)
            .map_err(|error| CatalogError::new("configuration_error", error.to_string()))?;
        let postgres = PostgresCatalog::connect(&url, config.connect_timeout()).await?;
        let clickhouse = ClickHouseCatalog::new(&config.clickhouse, config.connect_timeout())?;
        Ok::<_, CatalogError>((postgres, clickhouse))
    }
    .await;
    let (mut postgres, clickhouse) = match connection {
        Ok(connection) => connection,
        Err(error) => {
            report.errors.push(error);
            report.status = PlanStatus::Blocked;
            return report;
        }
    };
    let (pg, ch) = tokio::join!(
        bounded(config.query_timeout(), postgres.versions()),
        bounded(config.query_timeout(), clickhouse.version_and_visibility())
    );
    if let Err(error) = pg {
        report.errors.push(error);
    }
    match ch {
        Ok(version) if version.starts_with("25.8.") || version.starts_with("26.8.") => {}
        Ok(_) => report.errors.push(CatalogError::new(
            "unsupported_ddl_version",
            "DDL execution is validated for ClickHouse 25.8 and 26.8 LTS",
        )),
        Err(error) => report.errors.push(error),
    }
    if !report.errors.is_empty() {
        report.status = PlanStatus::Blocked;
        return report;
    }
    if let Err(error) = bounded(config.query_timeout(), clickhouse.ddl_visibility()).await {
        report.errors.push(error);
        report.status = PlanStatus::Blocked;
        return report;
    }
    let targets = match bounded(
        config.query_timeout(),
        clickhouse.tables(&config.clickhouse.database),
    )
    .await
    {
        Ok(targets) if !targets.is_empty() => targets,
        Ok(_) => {
            report.errors.push(CatalogError::new(
                "no_tables_discovered",
                "configured ClickHouse database contains no persistent tables",
            ));
            report.status = PlanStatus::Blocked;
            return report;
        }
        Err(error) => {
            report.errors.push(error);
            report.status = PlanStatus::Blocked;
            return report;
        }
    };
    let (targets, skipped) = crate::discovery::select(targets, &config.discovery);
    report.skipped_tables = skipped;
    if targets.is_empty() {
        report.errors.push(CatalogError::new(
            "no_tables_selected",
            "all discovered ClickHouse tables were excluded",
        ));
        report.status = PlanStatus::Blocked;
        return report;
    }
    let mut snapshots = Vec::with_capacity(targets.len());
    for target in &targets {
        match read(&mut postgres, &clickhouse, target, config).await {
            Ok(snapshot) => {
                report.tables.push(plan_table(
                    target,
                    &snapshot.schema,
                    &snapshot.metadata,
                    config,
                ));
                snapshots.push(Some(snapshot));
            }
            Err(error) => {
                report.tables.push(failed_table(target, &error));
                report.errors.push(error);
                snapshots.push(None);
            }
        }
    }
    for (index, target) in targets.iter().enumerate() {
        if mode == Mode::Apply && !report.tables[index].operations.is_empty() {
            match bounded(
                config.query_timeout(),
                clickhouse.ddl_grants(target, &report.tables[index].operations),
            )
            .await
            {
                Ok(true) => {}
                Ok(false) => block(
                    &mut report.tables[index],
                    None,
                    "ddl_permission_denied",
                    Status::Mismatch,
                    "required ALTER privileges are missing",
                ),
                Err(error) => {
                    block(
                        &mut report.tables[index],
                        None,
                        error.code,
                        Status::Unknown,
                        &error.message,
                    );
                    report.errors.push(error);
                }
            }
        }
        let Some(first) = &snapshots[index] else {
            continue;
        };
        match read(&mut postgres, &clickhouse, target, config).await {
            Ok(second) if &second == first => {}
            Ok(_) => block(
                &mut report.tables[index],
                None,
                "schema_changed_during_check",
                Status::Unknown,
                "source identity, candidates or DDL metadata changed before execution",
            ),
            Err(error) => {
                block(
                    &mut report.tables[index],
                    None,
                    error.code,
                    Status::Unknown,
                    &error.message,
                );
                report.errors.push(error);
            }
        }
    }
    match bounded(
        config.query_timeout(),
        clickhouse.tables(&config.clickhouse.database),
    )
    .await
    .map(|tables| crate::discovery::select(tables, &config.discovery).0)
    {
        Ok(second) if second == targets => {}
        Ok(_) => report.errors.push(CatalogError::new(
            "table_set_changed_during_check",
            "discovered table set changed before execution",
        )),
        Err(error) => report.errors.push(error),
    }
    if report.blocked() {
        report.status = PlanStatus::Blocked;
        return report;
    }
    if mode == Mode::Plan {
        return report;
    }
    if report
        .tables
        .iter()
        .all(|table| table.operations.is_empty())
    {
        report.status = PlanStatus::Completed;
        return report;
    }
    let executor = match Executor::new(&config.clickhouse, config.connect_timeout()) {
        Ok(executor) => executor,
        Err(error) => {
            report.errors.push(error);
            report.status = PlanStatus::Blocked;
            return report;
        }
    };
    let run_id = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(time) => format!("sync-pg2ch-{}-{}", std::process::id(), time.as_nanos()),
        Err(_) => {
            report.errors.push(CatalogError::new(
                "invalid_system_clock",
                "cannot assign DDL query identifiers",
            ));
            report.status = PlanStatus::Blocked;
            return report;
        }
    };
    for (index, target) in targets.iter().enumerate() {
        if report.tables[index].operations.is_empty() {
            continue;
        }
        let Some(first) = &snapshots[index] else {
            report.errors.push(CatalogError::new(
                "missing_ddl_snapshot",
                "a preflight snapshot is unavailable",
            ));
            finish_failure(&mut report);
            return report;
        };
        match read(&mut postgres, &clickhouse, target, config).await {
            Ok(current) if &current == first => {}
            Ok(_) => {
                report.errors.push(CatalogError::new(
                    "schema_changed_before_ddl",
                    "metadata changed before the next ALTER; remaining commands were not sent",
                ));
                finish_failure(&mut report);
                return report;
            }
            Err(error) => {
                report.errors.push(error);
                finish_failure(&mut report);
                return report;
            }
        }
        let query_id = format!("{run_id}-{index}");
        report.tables[index].query_id = Some(query_id.clone());
        let Some(ready) = report.ready() else {
            report.status = PlanStatus::Blocked;
            return report;
        };
        let result = executor
            .execute(
                &ready.table(index),
                &query_id,
                Duration::from_secs(config.apply.ddl_timeout_seconds),
            )
            .await;
        match result {
            Ok(()) => {}
            Err(ExecuteError::Rejected(error)) => {
                report.tables[index].execution = ExecutionState::Failed;
                report.errors.push(error);
                finish_failure(&mut report);
                return report;
            }
            Err(ExecuteError::Indeterminate(error)) => {
                report.tables[index].execution = ExecutionState::Indeterminate;
                report.errors.push(error);
                report.status = PlanStatus::Indeterminate;
                return report;
            }
        }
        match read(&mut postgres, &clickhouse, target, config).await {
            Ok(current) if verified(first, &current, &report.tables[index]) => {
                report.tables[index].execution = ExecutionState::Completed;
                report.tables[index].after = Some(crate::app::compare_snapshot(
                    target,
                    &current.schema,
                    config,
                ));
            }
            Ok(_) => {
                report.tables[index].execution = ExecutionState::Indeterminate;
                report.errors.push(CatalogError::new("ddl_verification_failed", "ALTER was acknowledged but its completion or preserved metadata could not be confirmed"));
                report.status = PlanStatus::Indeterminate;
                return report;
            }
            Err(error) => {
                report.tables[index].execution = ExecutionState::Indeterminate;
                report.errors.push(error);
                report.status = PlanStatus::Indeterminate;
                return report;
            }
        }
    }
    let verification = crate::app::run(config).await;
    if verification.status > Status::Compatible || !verification.complete {
        report.errors.push(CatalogError::new(
            "post_apply_diff_failed",
            "final schema diff did not confirm compatibility",
        ));
        report.errors.extend(verification.errors);
        finish_failure(&mut report);
    } else {
        report.status = PlanStatus::Completed;
    }
    for after in verification.tables {
        for table in &mut report.tables {
            if table.target == after.target {
                table.after = Some(after.clone());
            }
        }
    }
    report
}

fn finish_failure(report: &mut PlanReport) {
    report.status = if report
        .tables
        .iter()
        .any(|table| table.execution == ExecutionState::Completed)
    {
        PlanStatus::Partial
    } else {
        PlanStatus::Failed
    };
}

fn verified(first: &DdlSnapshot, current: &DdlSnapshot, plan: &TablePlan) -> bool {
    if first.schema.candidates != current.schema.candidates
        || first.schema.source != current.schema.source
        || current.metadata.pending_mutations != 0
        || current.metadata.active_alters != 0
    {
        return false;
    }
    let before = &first.metadata.table;
    let after = &current.metadata.table;
    if before.uuid != after.uuid
        || before.database_engine != after.database_engine
        || before.engine_full != after.engine_full
        || before.dependencies != after.dependencies
        || before.indexes != after.indexes
        || before.projections != after.projections
        || super::sql::complex_definition(&after.create_query)
    {
        return false;
    }
    let (Some(original), Some(actual)) = (&first.schema.target, &current.schema.target) else {
        return false;
    };
    if !same_schema(&projected(original, &plan.operations), actual) {
        return false;
    }
    for old in &first.metadata.columns {
        let mut found = false;
        for new in &current.metadata.columns {
            if old.name == new.name {
                if old.default_kind != new.default_kind
                    || old.default_expression != new.default_expression
                    || old.codec != new.codec
                    || old.comment != new.comment
                    || old.in_key != new.in_key
                {
                    return false;
                }
                found = true;
            }
        }
        if !found {
            return false;
        }
    }
    true
}

fn same_schema(expected: &ChTable, actual: &ChTable) -> bool {
    if expected.engine != actual.engine
        || expected.default_timezone != actual.default_timezone
        || expected.columns.len() != actual.columns.len()
    {
        return false;
    }
    for (expected, actual) in expected.columns.iter().zip(&actual.columns) {
        if expected.name != actual.name
            || expected.default_kind != actual.default_kind
            || !same_target_type(&expected.data_type, &actual.data_type)
        {
            return false;
        }
    }
    true
}
