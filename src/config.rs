//! Versioned configuration and explicit source/target table mappings.
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

/// A selected pair of tables and explicitly excluded target columns.
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
}

/// ClickHouse HTTP connection environment references.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClickHouseConfig {
    /// Environment variable containing an HTTP(S) endpoint.
    pub url_env: String,
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
}

impl Default for ComparisonConfig {
    fn default() -> Self {
        Self {
            policy: "pg-clickhouse-v1".into(),
            connect_timeout_seconds: 5,
            query_timeout_seconds: 15,
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
    /// Explicit table pairs, sorted during validation.
    pub tables: Vec<TablePair>,
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
        if self.tables.is_empty() {
            return Err(ConfigError("at least one table pair is required".into()));
        }
        let mut targets = BTreeSet::new();
        for pair in &mut self.tables {
            for name in [
                &pair.source.schema,
                &pair.source.table,
                &pair.target.database,
                &pair.target.table,
            ] {
                validate_identifier(name)?;
            }
            if !targets.insert(pair.target.clone()) {
                return Err(ConfigError("duplicate target table mapping".into()));
            }
            let mut ignored = BTreeSet::new();
            for column in &pair.ignore_target_columns {
                validate_identifier(column)?;
                if !ignored.insert(column) {
                    return Err(ConfigError("duplicate ignored target column".into()));
                }
            }
            pair.ignore_target_columns.sort();
        }
        self.tables
            .sort_by(|a, b| (&a.source, &a.target).cmp(&(&b.source, &b.target)));
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
[[tables]]
source = { schema = "public", table = "a.b" }
target = { database = "default", table = "a.b" }
"#;

    #[test]
    fn names_are_not_split_and_defaults_are_explicit() {
        let config = Config::parse(CONFIG).unwrap();
        assert_eq!(config.tables[0].source.table, "a.b");
        assert_eq!(config.query_timeout(), Duration::from_secs(15));
    }

    #[test]
    fn rejects_invalid_and_ambiguous_configuration() {
        for text in [
            CONFIG.replace("config_version = 1", "config_version = 2"),
            CONFIG.replace("url_env = \"PG_URL\"", "url = \"secret\""),
            CONFIG.replace("schema = \"public\"", "schema = \"\""),
            format!(
                "{CONFIG}\n[[tables]]\nsource = {{schema=\"other\",table=\"x\"}}\ntarget = {{database=\"default\",table=\"a.b\"}}"
            ),
            format!("{CONFIG}\n[comparison]\nquery_timeout_seconds = 0"),
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
