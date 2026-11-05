//! Versioned connection, discovery and comparison configuration.
use std::{collections::BTreeSet, path::Path, time::Duration};

use serde::{Deserialize, Serialize};

/// A configuration problem, without connection strings or environment values.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ConfigError(pub String);

/// An exact, case-sensitive PostgreSQL relation name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceTable {
    /// PostgreSQL namespace.
    pub schema: String,
    /// Relation name, without quoting or splitting on dots.
    pub table: String,
}

/// An exact, case-sensitive ClickHouse table name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetTable {
    /// ClickHouse database.
    pub database: String,
    /// Table name, without quoting or splitting on dots.
    pub table: String,
}

/// An internally resolved table pair and explicitly excluded target columns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TablePair {
    /// Source relation.
    pub source: SourceTable,
    /// Destination table.
    pub target: TargetTable,
    /// Target-only columns deliberately excluded from comparison.
    #[serde(default)]
    pub ignore_target_columns: Vec<String>,
}

/// PostgreSQL connection environment reference.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostgresConfig {
    /// Name of the environment variable containing a PostgreSQL URL.
    pub url_env: String,
    /// Namespace in which to resolve discovered ClickHouse table names.
    #[serde(default = "default_schema")]
    pub schema: String,
}

/// ClickHouse HTTP connection environment references.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClickHouseConfig {
    /// Environment variable containing an HTTP(S) endpoint.
    pub url_env: String,
    /// Database whose persistent tables define the comparison scope.
    pub database: String,
    /// Optional environment variable containing the user name.
    pub user_env: Option<String>,
    /// Optional environment variable containing the password.
    pub password_env: Option<String>,
    /// PEM certificate authority file, in addition to public trust roots.
    pub ca_cert: Option<std::path::PathBuf>,
}

/// Comparison policy and per-operation deadlines.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ComparisonConfig {
    /// The implemented mapping policy.
    pub policy: String,
    /// Connection deadline in seconds.
    pub connect_timeout_seconds: u64,
    /// Deadline for reading a table's metadata in seconds.
    pub query_timeout_seconds: u64,
    /// Target-only columns excluded from every discovered table.
    pub ignore_target_columns: Vec<String>,
}

impl Default for ComparisonConfig {
    fn default() -> Self {
        Self {
            policy: "pg-clickhouse-v1".into(),
            connect_timeout_seconds: 5,
            query_timeout_seconds: 15,
            ignore_target_columns: Vec::new(),
        }
    }
}

/// Configuration for a single metadata comparison run.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Configuration format version; currently 1.
    pub config_version: u32,
    /// PostgreSQL connection settings.
    pub postgres: PostgresConfig,
    /// ClickHouse connection settings.
    pub clickhouse: ClickHouseConfig,
    /// Comparison settings.
    #[serde(default)]
    pub comparison: ComparisonConfig,
}

impl Config {
    /// Read and validate TOML. Errors never include the configuration contents.
    pub fn read(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path)
            .map_err(|_| ConfigError("cannot read configuration file".into()))?;
        Self::parse(&text)
    }

    /// Parse and validate TOML without resolving environment variables.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let mut config: Self = toml::from_str(text).map_err(|error| {
            let location = match error.span() {
                Some(span) => format!(" at byte {}", span.start),
                None => String::new(),
            };
            ConfigError(format!(
                "invalid configuration{location}; check fields and TOML syntax"
            ))
        })?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&mut self) -> Result<(), ConfigError> {
        if self.config_version != 1 {
            return Err(ConfigError(
                "unsupported config_version (expected 1)".into(),
            ));
        }
        if self.comparison.policy != "pg-clickhouse-v1" {
            return Err(ConfigError("unsupported comparison policy".into()));
        }
        for timeout in [
            self.comparison.connect_timeout_seconds,
            self.comparison.query_timeout_seconds,
        ] {
            if !(1..=3600).contains(&timeout) {
                return Err(ConfigError(
                    "timeouts must be between 1 and 3600 seconds".into(),
                ));
            }
        }
        for name in [&self.postgres.url_env, &self.clickhouse.url_env] {
            validate_env_name(name)?;
        }
        for name in [&self.clickhouse.user_env, &self.clickhouse.password_env]
            .into_iter()
            .flatten()
        {
            validate_env_name(name)?;
        }
        validate_identifier(&self.postgres.schema)?;
        validate_identifier(&self.clickhouse.database)?;
        if ["system", "information_schema", "INFORMATION_SCHEMA"]
            .contains(&self.clickhouse.database.as_str())
        {
            return Err(ConfigError(
                "select a user ClickHouse database for discovery".into(),
            ));
        }
        let mut ignored = BTreeSet::new();
        for column in &self.comparison.ignore_target_columns {
            validate_identifier(column)?;
            if !ignored.insert(column) {
                return Err(ConfigError("duplicate ignored target column".into()));
            }
        }
        self.comparison.ignore_target_columns.sort();
        Ok(())
    }

    /// Deadline for establishing the connections.
    pub fn connect_timeout(&self) -> Duration {
        Duration::from_secs(self.comparison.connect_timeout_seconds)
    }

    /// Deadline for each catalog operation.
    pub fn query_timeout(&self) -> Duration {
        Duration::from_secs(self.comparison.query_timeout_seconds)
    }
}

fn default_schema() -> String {
    "public".into()
}

fn validate_env_name(name: &str) -> Result<(), ConfigError> {
    if name.is_empty() || name.contains(['=', '\0']) {
        return Err(ConfigError("invalid environment variable reference".into()));
    }
    Ok(())
}

fn validate_identifier(name: &str) -> Result<(), ConfigError> {
    if name.is_empty() || name.contains('\0') {
        return Err(ConfigError(
            "identifiers must be nonempty and contain no NUL".into(),
        ));
    }
    Ok(())
}

pub(crate) fn env_value(name: &str) -> Result<String, ConfigError> {
    let value = std::env::var(name).map_err(|_| {
        ConfigError("a referenced environment variable is missing or not UTF-8".into())
    })?;
    if value.is_empty() {
        return Err(ConfigError(
            "a referenced environment variable is empty".into(),
        ));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"
config_version = 1
[postgres]
url_env = "PG_URL"
[clickhouse]
url_env = "CH_URL"
database = "analytics.with.dot"
"#;

    #[test]
    fn names_are_not_split_and_defaults_are_explicit() {
        let config = Config::parse(CONFIG).unwrap();
        assert_eq!(config.postgres.schema, "public");
        assert_eq!(config.clickhouse.database, "analytics.with.dot");
        assert_eq!(config.query_timeout(), Duration::from_secs(15));
    }

    #[test]
    fn rejects_invalid_and_ambiguous_configuration() {
        for text in [
            CONFIG.replace("config_version = 1", "config_version = 2"),
            CONFIG.replace("url_env = \"PG_URL\"", "url = \"secret\""),
            CONFIG.replace("database = \"analytics.with.dot\"", "database = \"\""),
            CONFIG.replace("database = \"analytics.with.dot\"", "database = \"system\""),
            CONFIG.replace("database = \"analytics.with.dot\"", ""),
            CONFIG.replace(
                "url_env = \"PG_URL\"",
                "url_env = \"PG_URL\"\nschema = \"\"",
            ),
            format!(
                "{CONFIG}\n[[tables]]\nsource = {{schema=\"public\",table=\"a\"}}\ntarget = {{database=\"default\",table=\"a\"}}"
            ),
            format!("{CONFIG}\n[comparison]\nquery_timeout_seconds = 0"),
            format!("{CONFIG}\n[comparison]\nignore_target_columns = ['v', 'v']"),
        ] {
            assert!(Config::parse(&text).is_err());
        }
    }

    #[test]
    fn parse_error_never_echoes_contents() {
        let error = Config::parse("password = 'super-secret'").unwrap_err();
        assert!(!error.to_string().contains("super-secret"));
    }
}
