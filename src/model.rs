//! Database metadata and comparison results shared by adapters and reports.

use serde::Serialize;

/// Spatial constraints declared by a PostgreSQL type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GeoMetadata {
    /// Source family, such as `pg_catalog`, `geometry`, or `geography`.
    pub family: String,
    /// Declared geometric subtype, if constrained.
    pub subtype: Option<String>,
    /// Declared spatial reference identifier, if constrained.
    pub srid: Option<i32>,
    /// Declared coordinate dimension count, if constrained.
    pub dimensions: Option<u8>,
    /// Whether the declared type includes a Z coordinate.
    pub has_z: Option<bool>,
    /// Whether the declared type includes a measure coordinate.
    pub has_m: Option<bool>,
}

/// PostgreSQL type identity and its original catalog representation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PgType {
    /// Namespace containing the type.
    pub schema: String,
    /// Catalog type name rather than a display alias.
    pub name: String,
    /// Definition returned by PostgreSQL `format_type`.
    pub definition: String,
    /// PostgreSQL `pg_type.typtype` value.
    pub kind: String,
    /// PostgreSQL attribute type modifier, or -1 when unconstrained.
    pub typmod: i32,
    /// Extension that owns this exact type, if any.
    pub extension: Option<String>,
    /// Declared spatial properties, when available.
    pub geo: Option<GeoMetadata>,
}

/// PostgreSQL column metadata required by the compatibility policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PgColumn {
    /// Exact column identifier.
    pub name: String,
    /// Original source type metadata.
    pub data_type: PgType,
    /// Whether the source permits SQL NULL.
    pub nullable: bool,
    /// Whether PostgreSQL computes this column as a generated expression.
    pub generated: bool,
}

/// ClickHouse column metadata required by the compatibility policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChColumn {
    /// Exact column identifier.
    pub name: String,
    /// Type expression reported by ClickHouse.
    pub data_type: String,
    /// ClickHouse default kind: empty, DEFAULT, MATERIALIZED, or ALIAS.
    pub default_kind: String,
}

/// Metadata snapshot for a PostgreSQL relation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PgTable {
    /// PostgreSQL `pg_class.relkind` value.
    pub kind: String,
    /// PostgreSQL `pg_class.relpersistence` value.
    pub persistence: String,
    /// Visible, non-dropped user columns.
    pub columns: Vec<PgColumn>,
}

/// Metadata snapshot for a ClickHouse table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChTable {
    /// ClickHouse table engine name.
    pub engine: String,
    /// User column definitions.
    pub columns: Vec<ChColumn>,
}

/// Comparison severity, ordered from success to incomplete evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// The normalized definitions match.
    Match,
    /// A difference is permitted by a supported widening rule.
    Compatible,
    /// A known rule or structural requirement is violated.
    Mismatch,
    /// Evaluation is incomplete because an input is unsupported or unavailable.
    Unknown,
}

impl Status {
    /// Returns the CLI exit code for this status.
    #[must_use]
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::Match | Self::Compatible => 0,
            Self::Mismatch => 1,
            Self::Unknown => 2,
        }
    }
}

/// One structural, type, or informational comparison result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    /// Exact column name, absent for table-level findings.
    pub column: Option<String>,
    /// Stable machine-readable reason code.
    pub code: String,
    /// Severity of this finding.
    pub status: Status,
    /// Original source column, when available.
    pub source: Option<PgColumn>,
    /// Canonical expected ClickHouse type, when known.
    pub expected: Option<String>,
    /// Original target column, when available.
    pub target: Option<ChColumn>,
    /// Human-readable explanation of the rule and its limits.
    pub message: String,
}

/// Deterministic comparison result for one discovered pair of tables.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TableDiff {
    /// Resolved PostgreSQL source identity, absent if resolution failed or was ambiguous.
    pub source: Option<crate::config::SourceTable>,
    /// Matching PostgreSQL relations observed during source resolution.
    pub source_candidates: Vec<crate::config::SourceTable>,
    /// Discovered ClickHouse target identity.
    pub target: crate::config::TargetTable,
    /// Highest severity among findings for this pair.
    pub status: Status,
    /// Findings sorted by column identifier and reason code.
    pub findings: Vec<Finding>,
    /// Configured target-column exclusions, including currently absent names.
    /// Conflicts with source columns are reported and never suppress comparison.
    pub ignored_target_columns: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::Status;

    #[test]
    fn incomplete_evaluation_takes_priority_over_known_differences() {
        assert!(Status::Unknown > Status::Mismatch);
        assert!(Status::Mismatch > Status::Compatible);
        assert!(Status::Compatible > Status::Match);
        assert_eq!(Status::Match.exit_code(), 0);
        assert_eq!(Status::Compatible.exit_code(), 0);
        assert_eq!(Status::Mismatch.exit_code(), 1);
        assert_eq!(Status::Unknown.exit_code(), 2);
    }
}
