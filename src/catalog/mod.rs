//! Database catalog adapters and a separate ClickHouse DDL executor.
pub mod clickhouse;
pub(crate) mod ddl;
pub mod postgres;

use serde::Serialize;

/// Sanitized database failure. Driver messages and connection URLs are not retained.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, thiserror::Error)]
#[error("{message}")]
pub struct CatalogError {
    /// Stable machine-readable error category.
    pub code: &'static str,
    /// Safe diagnostic explaining the failed operation.
    pub message: String,
}

impl CatalogError {
    pub(crate) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub(crate) fn pg(error: sqlx::Error) -> Self {
        let state = match error.as_database_error().and_then(|e| e.code()) {
            Some(code) if code.len() == 5 && code.bytes().all(|b| b.is_ascii_alphanumeric()) => {
                format!(" (SQLSTATE {code})")
            }
            Some(_) | None => String::new(),
        };
        Self::new(
            "postgres_catalog_error",
            format!(
                "PostgreSQL metadata query failed{state}; check connection, credentials and catalog privileges"
            ),
        )
    }

    pub(crate) fn ch(_error: ::clickhouse::error::Error) -> Self {
        Self::new(
            "clickhouse_catalog_error",
            "ClickHouse metadata query failed; check connection, credentials and SHOW privileges",
        )
    }
}

/// Version of an installed PostgreSQL extension and its namespace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Extension {
    /// Extension name.
    pub name: String,
    /// Installed version.
    pub version: String,
    /// Namespace containing extension objects.
    pub schema: String,
}
