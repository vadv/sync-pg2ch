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

    pub(crate) fn during(mut self, operation: &str) -> Self {
        self.message = format!("{operation}: {}", self.message);
        self
    }

    pub(crate) fn pg(error: sqlx::Error) -> Self {
        if let Some(state) = error.as_database_error().and_then(|error| error.code())
            && state.len() == 5
            && state.bytes().all(|byte| byte.is_ascii_alphanumeric())
        {
            let (code, reason) = match state.as_ref() {
                "28P01" | "28000" => (
                    "postgres_authentication_error",
                    "PostgreSQL authentication failed; check user, password and pg_hba.conf",
                ),
                "42501" => (
                    "postgres_access_denied",
                    "PostgreSQL permission denied; check catalog and extension privileges",
                ),
                "3D000" => (
                    "postgres_database_missing",
                    "PostgreSQL database does not exist",
                ),
                _ => ("postgres_catalog_error", "PostgreSQL query failed"),
            };
            return Self::new(code, format!("{reason} (SQLSTATE {state})"));
        }
        match &error {
            sqlx::Error::Tls(_) => Self::new(
                "postgres_tls_error",
                "PostgreSQL TLS validation or handshake failed; check CA and server hostname",
            ),
            sqlx::Error::Io(error) => {
                let reason = transport_reason(error);
                let code = if reason == "TLS validation or handshake failed" {
                    "postgres_tls_error"
                } else {
                    "postgres_connection_error"
                };
                Self::new(code, format!("PostgreSQL connection failed: {reason}"))
            }
            _ => Self::new(
                "postgres_catalog_error",
                "PostgreSQL catalog response could not be read or decoded",
            ),
        }
    }

    pub(crate) fn ch(error: ::clickhouse::error::Error) -> Self {
        match &error {
            ::clickhouse::error::Error::BadResponse(message) => {
                let server_code = clickhouse_code(message);
                let (code, reason) = match server_code {
                    Some(516 | 192 | 193 | 194) => (
                        "clickhouse_authentication_error",
                        "ClickHouse authentication failed; check user and password",
                    ),
                    Some(497) => (
                        "clickhouse_access_denied",
                        "ClickHouse permission denied; check grants for this operation",
                    ),
                    Some(60 | 81) => (
                        "clickhouse_catalog_error",
                        "ClickHouse table or database does not exist",
                    ),
                    Some(47 | 115) => (
                        "clickhouse_catalog_error",
                        "ClickHouse does not recognize a requested column or setting; check server compatibility",
                    ),
                    Some(62) => (
                        "clickhouse_catalog_error",
                        "ClickHouse rejected the query syntax",
                    ),
                    Some(_) | None => (
                        "clickhouse_catalog_error",
                        "ClickHouse returned an unsuccessful response",
                    ),
                };
                let message = match server_code {
                    Some(number) => format!("{reason} (code {number})"),
                    None => reason.to_owned(),
                };
                Self::new(code, message)
            }
            ::clickhouse::error::Error::Network(error) => {
                let reason = transport_reason(error.as_ref());
                let code = if reason == "TLS validation or handshake failed" {
                    "clickhouse_tls_error"
                } else {
                    "clickhouse_connection_error"
                };
                Self::new(
                    code,
                    format!(
                        "ClickHouse connection failed: {reason}; check endpoint, network and TLS configuration"
                    ),
                )
            }
            ::clickhouse::error::Error::TimedOut => {
                Self::new("clickhouse_timeout", "ClickHouse request timed out")
            }
            _ => Self::new(
                "clickhouse_catalog_error",
                "ClickHouse catalog response could not be read or decoded",
            ),
        }
    }
}

pub(crate) fn clickhouse_code(message: &str) -> Option<u32> {
    message
        .split("Code: ")
        .nth(1)?
        .split('.')
        .next()?
        .parse()
        .ok()
}

fn transport_reason(mut error: &(dyn std::error::Error + 'static)) -> &'static str {
    for _ in 0..16 {
        if error.is::<rustls::Error>() {
            return "TLS validation or handshake failed";
        }
        if let Some(io) = error.downcast_ref::<std::io::Error>() {
            match io.kind() {
                std::io::ErrorKind::ConnectionRefused => return "connection refused",
                std::io::ErrorKind::TimedOut => return "network timeout",
                std::io::ErrorKind::ConnectionReset => return "connection reset",
                std::io::ErrorKind::NotFound => return "socket or connection path not found",
                _ => {}
            }
            if let Some(inner) = io.get_ref() {
                error = inner;
                continue;
            }
        }
        match error.source() {
            Some(source) => error = source,
            None => break,
        }
    }
    "network connection or protocol error"
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_keep_the_cause_and_discard_server_secrets() {
        for (number, expected) in [
            (516, "clickhouse_authentication_error"),
            (194, "clickhouse_authentication_error"),
            (497, "clickhouse_access_denied"),
            (47, "clickhouse_catalog_error"),
        ] {
            let error = CatalogError::ch(::clickhouse::error::Error::BadResponse(format!(
                "Code: {number}. DB::Exception: secret-password postgres://secret@host"
            )))
            .during("reading test metadata");
            assert_eq!(error.code, expected);
            assert!(error.message.contains(&format!("code {number}")));
            assert!(error.message.contains("reading test metadata"));
            assert!(!error.message.contains("secret"));
        }
        let tls = std::io::Error::other(rustls::Error::General("secret-certificate".into()));
        let error = CatalogError::ch(::clickhouse::error::Error::Network(Box::new(tls)));
        assert_eq!(error.code, "clickhouse_tls_error");
        assert!(!error.message.contains("secret"));
        let tls = std::io::Error::other(rustls::Error::General("secret-certificate".into()));
        let error = CatalogError::pg(sqlx::Error::Io(tls));
        assert_eq!(error.code, "postgres_tls_error");
        assert!(!error.message.contains("secret"));
        let error = CatalogError::ch(::clickhouse::error::Error::Network(Box::new(
            std::io::Error::from(std::io::ErrorKind::ConnectionRefused),
        )));
        assert_eq!(error.code, "clickhouse_connection_error");
        assert!(error.message.contains("connection refused"));
    }
}
