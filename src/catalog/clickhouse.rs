//! ClickHouse system catalog access with verified HTTP TLS and query parameters.
use std::{io::BufReader, time::Duration};

use clickhouse::{Client, Row};
use hyper_util::{
    client::legacy::{Client as HttpClient, connect::HttpConnector},
    rt::TokioExecutor,
};
use serde::Deserialize;

use super::CatalogError;
use crate::{
    config::{ClickHouseConfig, TargetTable, env_value},
    model::{ChColumn, ChTable},
};

/// A ClickHouse client restricted to metadata queries by this adapter.
pub struct ClickHouseCatalog {
    pub(super) client: Client,
}

#[derive(Row, Deserialize)]
struct ColumnRow {
    name: String,
    r#type: String,
    default_kind: String,
}

impl ClickHouseCatalog {
    /// Construct an HTTP client with a connection deadline and optional private CA.
    pub fn new(config: &ClickHouseConfig, deadline: Duration) -> Result<Self, CatalogError> {
        let endpoint = env_value(&config.url_env)
            .map_err(|error| CatalogError::new("configuration_error", error.to_string()))?;
        let url = url::Url::parse(&endpoint).map_err(|_| {
            CatalogError::new("invalid_clickhouse_url", "invalid ClickHouse HTTP endpoint")
        })?;
        if !["http", "https"].contains(&url.scheme())
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(CatalogError::new(
                "invalid_clickhouse_url",
                "ClickHouse URL must be an HTTP(S) endpoint without credentials, query or fragment; use user_env/password_env",
            ));
        }
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        if let Some(path) = &config.ca_cert {
            let file = std::fs::File::open(path).map_err(|_| {
                CatalogError::new(
                    "invalid_clickhouse_ca",
                    "cannot read ClickHouse CA certificate",
                )
            })?;
            let mut reader = BufReader::new(file);
            let mut count = 0;
            for certificate in rustls_pemfile::certs(&mut reader) {
                let certificate = certificate.map_err(|_| {
                    CatalogError::new("invalid_clickhouse_ca", "invalid PEM CA certificate")
                })?;
                roots.add(certificate).map_err(|_| {
                    CatalogError::new("invalid_clickhouse_ca", "invalid CA certificate")
                })?;
                count += 1;
            }
            if count == 0 {
                return Err(CatalogError::new(
                    "invalid_clickhouse_ca",
                    "CA file contains no certificates",
                ));
            }
        }
        let tls = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|_| {
            CatalogError::new(
                "tls_configuration_error",
                "cannot initialize TLS protocol versions",
            )
        })?
        .with_root_certificates(roots)
        .with_no_client_auth();
        let mut connector = HttpConnector::new();
        connector.enforce_http(false);
        connector.set_connect_timeout(Some(deadline));
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(tls)
            .https_or_http()
            .enable_http1()
            .wrap_connector(connector);
        let http = HttpClient::builder(TokioExecutor::new())
            .pool_idle_timeout(Duration::from_secs(2))
            .build(connector);
        let mut client = Client::with_http_client(http)
            .with_url(endpoint)
            .with_compression(clickhouse::Compression::None)
            .with_validation(false)
            .with_setting("readonly", "2");
        if let Some(name) = &config.user_env {
            client = client
                .with_user(env_value(name).map_err(|error| {
                    CatalogError::new("configuration_error", error.to_string())
                })?);
        }
        if let Some(name) = &config.password_env {
            client = client
                .with_password(env_value(name).map_err(|error| {
                    CatalogError::new("configuration_error", error.to_string())
                })?);
        }
        Ok(Self { client })
    }

    /// Check authentication and require complete table/column metadata visibility.
    pub async fn version_and_visibility(&self) -> Result<String, CatalogError> {
        let version = self
            .client
            .query("SELECT version()")
            .fetch_one::<String>()
            .await
            .map_err(CatalogError::ch)?;
        for query in [
            "CHECK GRANT SHOW TABLES ON *.*",
            "CHECK GRANT SHOW COLUMNS ON *.*",
        ] {
            let visible = self
                .client
                .query(query)
                .fetch_one::<u8>()
                .await
                .map_err(CatalogError::ch)?;
            if visible == 0 {
                return Err(CatalogError::new(
                    "clickhouse_metadata_visibility",
                    "ClickHouse requires SHOW TABLES and SHOW COLUMNS ON *.* to distinguish missing tables from hidden metadata",
                ));
            }
        }
        Ok(version)
    }

    /// Discover all persistent objects in one database without filtering by engine.
    ///
    /// Unsupported objects remain visible so the comparator can report them as unknown.
    pub async fn tables(&self, database: &str) -> Result<Vec<TargetTable>, CatalogError> {
        let exists = self
            .client
            .query("SELECT name FROM system.databases WHERE name = ?")
            .bind(database)
            .fetch_optional::<String>()
            .await
            .map_err(CatalogError::ch)?;
        if exists.is_none() {
            return Err(CatalogError::new(
                "clickhouse_database_missing",
                "configured ClickHouse database does not exist",
            ));
        }
        let names = self.client.query("SELECT name FROM system.tables WHERE database = ? AND is_temporary = 0 ORDER BY name")
            .bind(database).fetch_all::<String>().await.map_err(CatalogError::ch)?;
        let mut tables = Vec::with_capacity(names.len());
        for name in names {
            tables.push(TargetTable {
                database: database.to_owned(),
                table: name,
            });
        }
        Ok(tables)
    }

    /// Read the engine and writable-column metadata of a discovered table.
    pub async fn table(&self, table: &TargetTable) -> Result<Option<ChTable>, CatalogError> {
        let engine = self
            .client
            .query("SELECT engine FROM system.tables WHERE database = ? AND name = ?")
            .bind(&table.database)
            .bind(&table.table)
            .fetch_optional::<String>()
            .await
            .map_err(CatalogError::ch)?;
        let Some(engine) = engine else {
            return Ok(None);
        };
        let rows = self.client.query("SELECT name, type, default_kind FROM system.columns WHERE database = ? AND table = ? ORDER BY name")
            .bind(&table.database).bind(&table.table).fetch_all::<ColumnRow>().await.map_err(CatalogError::ch)?;
        let mut columns = Vec::with_capacity(rows.len());
        for ColumnRow {
            name,
            r#type,
            default_kind,
        } in rows
        {
            columns.push(ChColumn {
                name,
                data_type: r#type,
                default_kind,
            });
        }
        Ok(Some(ChTable { engine, columns }))
    }
}
