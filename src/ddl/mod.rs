//! Metadata-derived ALTER plans and globally preflighted execution.
mod app;
mod report;
mod sql;

use serde::Serialize;

use crate::{
    app::{Snapshot, compare_snapshot},
    catalog::{CatalogError, ddl::Metadata},
    compatibility::{alter_destination, compare_types_in_timezone, expected_definition},
    config::{Config, SourceTable, TablePair, TargetTable},
    diff,
    model::{ChColumn, ChTable, Status, TableDiff},
};

pub use app::run;
pub(crate) use sql::quote_identifier;

/// Requested command. Planning never constructs a write connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Plan,
    Apply,
}

/// Overall readiness or execution outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Ready,
    Blocked,
    Completed,
    Failed,
    Partial,
    Indeterminate,
}

/// Delivery state of the single ALTER for one table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionState {
    Unchanged,
    NotSent,
    Completed,
    Failed,
    Indeterminate,
}

/// Supported operations; arbitrary SQL is not an execution input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Operation {
    /// Add a nullable source column without reconstructing historical values.
    AddColumn { column: String, data_type: String },
    /// Change a column while retaining existing CH values.
    ModifyColumn {
        column: String,
        from: String,
        data_type: String,
    },
}

impl Operation {
    pub(crate) fn column(&self) -> &str {
        match self {
            Self::AddColumn { column, .. } | Self::ModifyColumn { column, .. } => column,
        }
    }
    pub(crate) fn data_type(&self) -> &str {
        match self {
            Self::AddColumn { data_type, .. } | Self::ModifyColumn { data_type, .. } => data_type,
        }
    }
}

/// A known prohibition or incomplete verification preventing the entire run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Blocker {
    /// Column, when the failure belongs to one.
    pub column: Option<String>,
    /// Stable diagnostic identifier.
    pub code: String,
    /// Mismatch is a known prohibition; unknown means incomplete verification.
    pub status: Status,
    /// Sanitized diagnostic.
    pub message: String,
}

/// One table's proposed operations and delivery outcome.
#[derive(Debug, Serialize)]
pub struct TablePlan {
    /// Unambiguously resolved PG relation.
    pub source: Option<SourceTable>,
    /// Discovered destination.
    pub target: TargetTable,
    /// Destination engine, when readable.
    pub engine: Option<String>,
    /// Destination database engine used to determine DDL scope.
    pub database_engine: Option<String>,
    /// Typed changes in deterministic order.
    pub operations: Vec<Operation>,
    /// Proposed statement, not an indication of permission to execute.
    pub sql: Option<String>,
    /// Reasons that prevent the whole plan from running.
    pub blockers: Vec<Blocker>,
    /// Confirmed delivery state.
    pub execution: ExecutionState,
    /// Identifier assigned before sending the ALTER.
    pub query_id: Option<String>,
    /// Actual comparison after execution, when available.
    pub after: Option<TableDiff>,
}

/// Version-one plan/apply result, including partial execution.
#[derive(Debug, Serialize)]
pub struct PlanReport {
    /// Machine-readable report version.
    pub report_version: u32,
    /// Command requested.
    pub mode: Mode,
    /// Whole-run status.
    pub status: PlanStatus,
    /// Discovered table plans.
    pub tables: Vec<TablePlan>,
    /// Tables excluded explicitly from the DDL scope.
    pub skipped_tables: Vec<crate::discovery::SkippedTable>,
    /// Operational diagnostics, without driver messages.
    pub errors: Vec<CatalogError>,
}

impl PlanReport {
    /// Construct an empty result before catalog access.
    pub fn new(mode: Mode) -> Self {
        Self {
            report_version: 1,
            mode,
            status: PlanStatus::Ready,
            tables: Vec::new(),
            skipped_tables: Vec::new(),
            errors: Vec::new(),
        }
    }

    pub(crate) fn blocked(&self) -> bool {
        !self.errors.is_empty() || self.tables.iter().any(|table| !table.blockers.is_empty())
    }

    fn ready(&self) -> Option<ReadyPlan<'_>> {
        if self.blocked() {
            None
        } else {
            Some(ReadyPlan(self))
        }
    }
}

/// Capability issued only for a plan with no blockers or catalog errors.
struct ReadyPlan<'a>(&'a PlanReport);

impl ReadyPlan<'_> {
    fn table(&self, index: usize) -> ReadyTable<'_> {
        ReadyTable(&self.0.tables[index])
    }
}

/// A write adapter can accept only tables from a globally checked plan.
pub(crate) struct ReadyTable<'a>(&'a TablePlan);
impl ReadyTable<'_> {
    pub(crate) fn sql(&self) -> String {
        sql::statement(&self.0.target, &self.0.operations)
    }
}

fn empty_table(target: &TargetTable) -> TablePlan {
    TablePlan {
        source: None,
        target: target.clone(),
        engine: None,
        database_engine: None,
        operations: Vec::new(),
        sql: None,
        blockers: Vec::new(),
        execution: ExecutionState::NotSent,
        query_id: None,
        after: None,
    }
}

fn block(
    table: &mut TablePlan,
    column: Option<&str>,
    code: &str,
    status: Status,
    message: impl Into<String>,
) {
    let blocker = Blocker {
        column: column.map(str::to_owned),
        code: code.into(),
        status,
        message: message.into(),
    };
    if !table.blockers.contains(&blocker) {
        table.blockers.push(blocker);
    }
}

fn plan_table(
    target: &TargetTable,
    snapshot: &Snapshot,
    metadata: &Metadata,
    config: &Config,
) -> TablePlan {
    let mut table = empty_table(target);
    table.engine = Some(metadata.table.engine.clone());
    table.database_engine = Some(metadata.table.database_engine.clone());
    let comparison = compare_snapshot(target, snapshot, config);
    table.source = comparison.source.clone();
    for finding in &comparison.findings {
        if finding.status == Status::Unknown
            || (finding.status == Status::Mismatch
                && (finding.column.is_none()
                    || ![
                        "type_mismatch",
                        "nullability_mismatch",
                        "source_column_missing_in_target",
                        "timezone_mismatch",
                        "geo_representation_mismatch",
                    ]
                    .contains(&finding.code.as_str())))
        {
            block(
                &mut table,
                finding.column.as_deref(),
                &finding.code,
                finding.status,
                &finding.message,
            );
        }
    }
    let (Some(source), Some(current), Some(source_name)) =
        (&snapshot.source, &snapshot.target, table.source.clone())
    else {
        return table;
    };
    for column in &source.columns {
        if table
            .blockers
            .iter()
            .any(|blocker| blocker.column.as_deref() == Some(&column.name))
        {
            continue;
        }
        let mut existing = None;
        for target_column in &current.columns {
            if target_column.name == column.name {
                existing = Some(target_column);
                break;
            }
        }
        let operation = match existing {
            None => expected_definition(&column.data_type).map(|data_type| Operation::AddColumn {
                column: column.name.clone(),
                data_type: format!("Nullable({data_type})"),
            }),
            Some(existing) => {
                let comparison = compare_types_in_timezone(
                    &column.data_type,
                    &existing.data_type,
                    current.default_timezone.as_deref(),
                );
                if comparison.status <= Status::Compatible
                    && (!column.nullable || comparison.target_nullable == Some(true))
                {
                    continue;
                }
                alter_destination(
                    &column.data_type,
                    column.nullable,
                    &existing.data_type,
                    current.default_timezone.as_deref(),
                )
                .map(|data_type| Operation::ModifyColumn {
                    column: column.name.clone(),
                    from: existing.data_type.clone(),
                    data_type,
                })
            }
        };
        match operation {
            Ok(operation) => table.operations.push(operation),
            Err(message) => block(
                &mut table,
                Some(&column.name),
                "unsupported_alter_conversion",
                Status::Mismatch,
                message,
            ),
        }
    }
    if !table.operations.is_empty() {
        check_metadata(&mut table, metadata);
        let expected = projected(current, &table.operations);
        let pair = TablePair {
            source: source_name,
            target: target.clone(),
            ignore_target_columns: config.comparison.ignore_target_columns.clone(),
        };
        for finding in diff::compare(&pair, Some(source), Some(&expected)).findings {
            if finding.status > Status::Compatible {
                block(
                    &mut table,
                    finding.column.as_deref(),
                    "unresolved_difference",
                    finding.status,
                    finding.message,
                );
            }
        }
        table.sql = Some(sql::statement(target, &table.operations));
    } else if table.blockers.is_empty() {
        table.execution = ExecutionState::Unchanged;
    }
    table
}

fn projected(current: &ChTable, operations: &[Operation]) -> ChTable {
    let mut expected = current.clone();
    for operation in operations {
        match operation {
            Operation::AddColumn { column, data_type } => expected.columns.push(ChColumn {
                name: column.clone(),
                data_type: data_type.clone(),
                default_kind: String::new(),
            }),
            Operation::ModifyColumn {
                column, data_type, ..
            } => {
                for existing in &mut expected.columns {
                    if existing.name == *column {
                        existing.data_type = data_type.clone();
                    }
                }
            }
        }
    }
    expected.columns.sort_by(|a, b| a.name.cmp(&b.name));
    expected
}

fn check_metadata(table: &mut TablePlan, metadata: &Metadata) {
    let info = &metadata.table;
    if !["Atomic", "Ordinary"].contains(&info.database_engine.as_str())
        || info.engine.starts_with("Replicated")
        || info.engine.starts_with("Shared")
    {
        block(
            table,
            None,
            "unsupported_ddl_topology",
            Status::Mismatch,
            "DDL supports a local table in Atomic/Ordinary databases; replicated execution is not enabled",
        );
    }
    if !["MergeTree", "ReplacingMergeTree"].contains(&info.engine.as_str()) {
        block(
            table,
            None,
            "unsupported_ddl_engine",
            Status::Mismatch,
            "DDL supports MergeTree and ReplacingMergeTree",
        );
    }
    if metadata.pending_mutations > 0 || metadata.active_alters > 0 {
        block(
            table,
            None,
            "ddl_in_progress",
            Status::Mismatch,
            "unfinished mutations or ALTER queries must finish before this plan can run",
        );
    }
    if info.dependencies > 0 {
        block(
            table,
            None,
            "dependent_views",
            Status::Mismatch,
            "dependent views require a separate DDL compatibility rule",
        );
    }
    let complex = sql::complex_definition(&info.create_query);
    if complex {
        block(
            table,
            None,
            "dependent_table_definition",
            Status::Mismatch,
            "TTL or constraints require a separate DDL compatibility rule",
        );
    }
    let mut column_names = Vec::with_capacity(metadata.columns.len());
    for column in &metadata.columns {
        column_names.push(column.name.as_str());
    }
    let operations = table.operations.clone();
    for operation in operations {
        let Operation::ModifyColumn { column, .. } = operation else {
            continue;
        };
        for attributes in &metadata.columns {
            if attributes.name == column && attributes.in_key != 0 {
                block(
                    table,
                    Some(&column),
                    "key_column_change",
                    Status::Mismatch,
                    "changing a sorting, primary, partition or sampling key column is not supported",
                );
            }
            if !attributes.default_expression.is_empty() {
                match sql::expression_references(&attributes.default_expression, &column_names) {
                    Some(references) if references.contains(&column) => block(
                        table,
                        Some(&column),
                        "dependent_column_expression",
                        Status::Mismatch,
                        format!(
                            "computed column {:?} depends on the column being modified",
                            attributes.name
                        ),
                    ),
                    Some(_) => {}
                    None => block(
                        table,
                        Some(&column),
                        "unresolved_column_expression",
                        Status::Mismatch,
                        format!(
                            "dependencies of computed column {:?} could not be resolved",
                            attributes.name
                        ),
                    ),
                }
            }
            if attributes.name == column && !sql::generic_codec(&attributes.codec) {
                block(
                    table,
                    Some(&column),
                    "dependent_codec",
                    Status::Mismatch,
                    "this codec requires a separate type-conversion rule",
                );
            }
        }
        if info.indexes > 0 || info.projections > 0 {
            block(
                table,
                Some(&column),
                "dependent_index_or_projection",
                Status::Mismatch,
                "indexes and projections require a separate type-conversion rule",
            );
        }
        match sql::engine_columns(&info.engine_full) {
            Some(columns) if columns.contains(&column) => block(
                table,
                Some(&column),
                "engine_column_change",
                Status::Mismatch,
                "engine argument columns cannot be altered automatically",
            ),
            None => block(
                table,
                Some(&column),
                "unsupported_engine_expression",
                Status::Mismatch,
                "engine arguments could not be resolved",
            ),
            Some(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ddl::TableMetadata;

    #[test]
    fn unsupported_topology_and_pending_work_prevent_the_whole_plan() {
        for (database_engine, engine, pending_mutations, active_alters, code) in [
            (
                "Atomic",
                "ReplicatedMergeTree",
                0,
                0,
                "unsupported_ddl_topology",
            ),
            (
                "Atomic",
                "SharedMergeTree",
                0,
                0,
                "unsupported_ddl_topology",
            ),
            ("Replicated", "MergeTree", 0, 0, "unsupported_ddl_topology"),
            ("Atomic", "SummingMergeTree", 0, 0, "unsupported_ddl_engine"),
            ("Atomic", "MergeTree", 1, 0, "ddl_in_progress"),
            ("Atomic", "MergeTree", 0, 1, "ddl_in_progress"),
        ] {
            let target = TargetTable {
                database: "test".into(),
                table: "last".into(),
            };
            let mut table = empty_table(&target);
            table.operations.push(Operation::AddColumn {
                column: "added".into(),
                data_type: "Nullable(String)".into(),
            });
            let metadata = Metadata {
                table: TableMetadata {
                    uuid: "test-uuid".into(),
                    database_engine: database_engine.into(),
                    engine: engine.into(),
                    engine_full: engine.into(),
                    create_query: String::new(),
                    dependencies: 0,
                    indexes: 0,
                    projections: 0,
                },
                columns: Vec::new(),
                pending_mutations,
                active_alters,
            };
            check_metadata(&mut table, &metadata);
            assert!(table.blockers.iter().any(|blocker| blocker.code == code));
            let mut report = PlanReport::new(Mode::Apply);
            report.tables.push(empty_table(&TargetTable {
                database: "test".into(),
                table: "first".into(),
            }));
            assert!(report.ready().is_some());
            report.tables.push(table);
            assert!(report.ready().is_none(), "{database_engine}/{engine}");
            let mut sql = Vec::new();
            report.write_sql(&mut sql).unwrap();
            assert!(sql.is_empty());
        }
    }
}
