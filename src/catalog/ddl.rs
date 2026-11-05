//! DDL-specific metadata and the separate ClickHouse write capability.
use std::time::Duration;

use clickhouse::Row;
use serde::{Deserialize, Serialize};

use super::{CatalogError, clickhouse::ClickHouseCatalog};
use crate::{
    config::{ClickHouseConfig, TargetTable},
    ddl::{Operation, ReadyTable, quote_identifier},
};

/// Complete column attributes used to protect existing definitions during ALTER.
#[derive(Debug, Clone, PartialEq, Eq, Row, Deserialize, Serialize)]
pub(crate) struct ColumnMetadata {
    pub name: String,
    pub data_type: String,
    pub default_kind: String,
    pub default_expression: String,
    pub codec: String,
    pub comment: String,
    pub in_key: u8,
}

/// Stable identity and definition, including dependencies outside the diff policy.
#[derive(Debug, Clone, PartialEq, Eq, Row, Deserialize, Serialize)]
pub(crate) struct TableMetadata {
    pub uuid: String,
    pub database_engine: String,
    pub engine: String,
    pub engine_full: String,
    pub create_query: String,
    pub dependencies: u64,
    pub indexes: u64,
    pub projections: u64,
}

/// DDL snapshot; unfinished mutations are a blocker rather than a successful schema read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Metadata {
    pub table: TableMetadata,
    pub columns: Vec<ColumnMetadata>,
    pub pending_mutations: u64,
    pub active_alters: u64,
}

impl ClickHouseCatalog {
    pub(crate) async fn ddl_visibility(&self) -> Result<(), CatalogError> {
        for table in [
            "data_skipping_indices",
            "projections",
            "mutations",
            "processes",
        ] {
            let query = format!("CHECK GRANT SELECT ON system.{table}");
            if self
                .client
                .query(&query)
                .fetch_one::<u8>()
                .await
                .map_err(CatalogError::ch)?
                == 0
            {
                return Err(CatalogError::new(
                    "ddl_metadata_access_denied",
                    format!(
                        "plan/apply require SELECT ON system.{table} to check DDL dependencies and execution"
                    ),
                ));
            }
        }
        Ok(())
    }

    pub(crate) async fn ddl_metadata(
        &self,
        target: &TargetTable,
    ) -> Result<Option<Metadata>, CatalogError> {
        let table = self.client.query(
            "SELECT toString(t.uuid) AS uuid, d.engine AS database_engine, t.engine, t.engine_full, t.create_table_query AS create_query, toUInt64(length(t.dependencies_table)) AS dependencies, (SELECT count() FROM system.data_skipping_indices WHERE database = ? AND table = ?) AS indexes, (SELECT count() FROM system.projections WHERE database = ? AND table = ?) AS projections FROM system.tables t INNER JOIN system.databases d ON t.database = d.name WHERE t.database = ? AND t.name = ?"
        ).bind(&target.database).bind(&target.table).bind(&target.database).bind(&target.table)
            .bind(&target.database).bind(&target.table)
            .fetch_optional::<TableMetadata>().await.map_err(CatalogError::ch)?;
        let Some(table) = table else {
            return Ok(None);
        };
        let columns = self.client.query(
            "SELECT name, type AS data_type, default_kind, default_expression, compression_codec AS codec, comment, toUInt8(is_in_partition_key OR is_in_sorting_key OR is_in_primary_key OR is_in_sampling_key) AS in_key FROM system.columns WHERE database = ? AND table = ? ORDER BY name"
        ).bind(&target.database).bind(&target.table).fetch_all::<ColumnMetadata>().await.map_err(CatalogError::ch)?;
        let pending_mutations = self.client.query("SELECT count() FROM system.mutations WHERE database = ? AND table = ? AND NOT is_done")
            .bind(&target.database).bind(&target.table).fetch_one::<u64>().await.map_err(CatalogError::ch)?;
        let active_alters = self
            .client
            .query("SELECT count() FROM system.processes WHERE query_kind = 'Alter'")
            .fetch_one::<u64>()
            .await
            .map_err(CatalogError::ch)?;
        Ok(Some(Metadata {
            table,
            columns,
            pending_mutations,
            active_alters,
        }))
    }

    pub(crate) async fn ddl_grants(
        &self,
        target: &TargetTable,
        operations: &[Operation],
    ) -> Result<bool, CatalogError> {
        for operation in operations {
            let privilege = match operation {
                Operation::AddColumn { .. } => "ALTER ADD COLUMN",
                Operation::ModifyColumn { .. } => "ALTER MODIFY COLUMN",
            };
            let query = format!(
                "CHECK GRANT {privilege}({}) ON {}.{}",
                quote_identifier(operation.column()),
                quote_identifier(&target.database),
                quote_identifier(&target.table)
            );
            if self
                .client
                .query(&query)
                .with_setting("readonly", "0")
                .fetch_one::<u8>()
                .await
                .map_err(CatalogError::ch)?
                == 0
            {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// Write connection, only constructed after a global preflight has succeeded.
pub(crate) struct Executor {
    client: clickhouse::Client,
}

/// Server rejection is distinct from a lost or ambiguous acknowledgement.
pub(crate) enum ExecuteError {
    Rejected(CatalogError),
    Indeterminate(CatalogError),
}

impl Executor {
    pub(crate) fn new(config: &ClickHouseConfig, timeout: Duration) -> Result<Self, CatalogError> {
        let catalog = ClickHouseCatalog::new(config, timeout)?;
        Ok(Self {
            client: catalog.client.with_setting("readonly", "0"),
        })
    }

    pub(crate) async fn execute(
        &self,
        table: &ReadyTable<'_>,
        query_id: &str,
        timeout: Duration,
    ) -> Result<(), ExecuteError> {
        let result = tokio::time::timeout(
            timeout,
            self.client
                .query(&table.sql())
                .with_setting("query_id", query_id)
                .with_setting("alter_sync", "1")
                .with_setting("wait_end_of_query", "1")
                .execute(),
        )
        .await;
        match result {
            Ok(Ok(())) => Ok(()),
            Err(_) => Err(ExecuteError::Indeterminate(CatalogError::new(
                "ddl_timeout",
                "DDL acknowledgement timed out; the server may still be executing it; inspect query_id before retrying",
            ))),
            Ok(Err(error)) => {
                let code = match &error {
                    clickhouse::error::Error::BadResponse(message) => message
                        .split("Code: ")
                        .nth(1)
                        .and_then(|value| value.split('.').next())
                        .and_then(|value| value.parse::<u32>().ok()),
                    _ => None,
                };
                let error = CatalogError::new("ddl_execution_error", match code {
                    Some(code) => format!("ClickHouse rejected or interrupted DDL (code {code}); inspect query_id on the server"),
                    None => "DDL acknowledgement failed; the outcome must be checked on the server using query_id".into(),
                });
                match code {
                    Some(497 | 516 | 524 | 62) => Err(ExecuteError::Rejected(error)),
                    _ => Err(ExecuteError::Indeterminate(error)),
                }
            }
        }
    }
}
