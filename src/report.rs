//! Stable JSON reports and escaped, human-readable output.
use std::io::{self, Write};

use serde::Serialize;

use crate::{
    catalog::{CatalogError, Extension},
    model::{Status, TableDiff},
};

/// Server versions recorded during this run.
#[derive(Debug, Default, Serialize)]
pub struct Servers {
    /// PostgreSQL server version, if readable.
    pub postgres: Option<String>,
    /// ClickHouse server version, if readable.
    pub clickhouse: Option<String>,
    /// Installed PostgreSQL extensions.
    pub extensions: Vec<Extension>,
}

/// Counts of table-level comparison results.
#[derive(Debug, Default, Serialize)]
pub struct Counts {
    /// Exact matches after normalization.
    pub r#match: usize,
    /// Allowed destination widening.
    pub compatible: usize,
    /// Known incompatibilities.
    pub mismatch: usize,
    /// Unsupported or uninspectable tables.
    pub unknown: usize,
}

/// Complete result of a comparison invocation, including partial failures.
#[derive(Debug, Serialize)]
pub struct Report {
    /// Machine-readable format version.
    pub report_version: u32,
    /// Directional type mapping policy identifier.
    pub policy: &'static str,
    /// Limits of what schema comparison can prove about value serialization.
    pub policy_limitations: Vec<&'static str>,
    /// Whether every selected table was inspected and evaluated by supported rules.
    pub complete: bool,
    /// Worst table status; operational errors force unknown.
    pub status: Status,
    /// Counts of table statuses.
    pub counts: Counts,
    /// Server and extension versions.
    pub servers: Servers,
    /// Discovered table results in deterministic identifier order.
    pub tables: Vec<TableDiff>,
    /// Operational diagnostics without driver messages or secrets.
    pub errors: Vec<CatalogError>,
}

impl Default for Report {
    fn default() -> Self {
        Self {
            report_version: 1,
            policy: "pg-clickhouse-v1",
            policy_limitations: vec![
                "Only schema metadata is checked; row values and CDC serialization are not validated.",
                "Column order, constraints, indexes, defaults, sorting keys and TTL are not compared.",
                "Date/time ranges and special values may differ between PostgreSQL and ClickHouse.",
                "JSON, bytea and geometry use explicit String representation conventions.",
                "Geo subtype, SRID and dimensions are not_represented_in_target_schema for String; arbitrary CHECK constraints and historical SRID changes are not inferred.",
                "Two metadata reads detect observed changes, not an atomic snapshot across servers or future DDL.",
            ],
            complete: true,
            status: Status::Match,
            counts: Counts::default(),
            servers: Servers::default(),
            tables: Vec::new(),
            errors: Vec::new(),
        }
    }
}

impl Report {
    /// Recompute the aggregate status after collecting findings and errors.
    pub fn finish(&mut self) {
        self.tables.sort_by(|a, b| a.target.cmp(&b.target));
        for table in &mut self.tables {
            table
                .findings
                .sort_by(|a, b| (&a.column, &a.code).cmp(&(&b.column, &b.code)));
        }
        self.status = Status::Match;
        self.counts = Counts::default();
        for table in &self.tables {
            self.status = self.status.max(table.status);
            match table.status {
                Status::Match => self.counts.r#match += 1,
                Status::Compatible => self.counts.compatible += 1,
                Status::Mismatch => self.counts.mismatch += 1,
                Status::Unknown => self.counts.unknown += 1,
            }
        }
        if !self.errors.is_empty() || !self.complete || self.status == Status::Unknown {
            self.complete = false;
            self.status = Status::Unknown;
        }
    }

    /// Process exit status: 0 compatible, 1 incompatible, 2 unknown/error.
    pub fn exit_code(&self) -> u8 {
        if !self.complete {
            return 2;
        }
        self.status.exit_code()
    }

    /// Write a JSON document followed by a newline.
    pub fn write_json(&self, writer: &mut impl Write) -> io::Result<()> {
        serde_json::to_writer_pretty(&mut *writer, self).map_err(io::Error::other)?;
        writeln!(writer)
    }

    /// Write table summaries and non-matching columns, escaping identifiers.
    pub fn write_text(&self, writer: &mut impl Write) -> io::Result<()> {
        writeln!(
            writer,
            "{} (complete={}, policy={})",
            status_name(self.status),
            self.complete,
            self.policy
        )?;
        for table in &self.tables {
            write!(writer, "{} ", status_name(table.status))?;
            match &table.source {
                Some(source) => write!(writer, "{:?}.{:?}", source.schema, source.table)?,
                None => write!(writer, "(unresolved source)")?,
            }
            writeln!(
                writer,
                " -> {:?}.{:?}",
                table.target.database, table.target.table
            )?;
            if table.source.is_none() && !table.source_candidates.is_empty() {
                for source in &table.source_candidates {
                    writeln!(
                        writer,
                        "  source candidate: {:?}.{:?}",
                        source.schema, source.table
                    )?;
                }
            }
            for finding in &table.findings {
                if finding.status == Status::Match {
                    continue;
                }
                write!(writer, "  {} {}", status_name(finding.status), finding.code)?;
                if let Some(column) = &finding.column {
                    write!(writer, " {column:?}")?;
                }
                if let Some(expected) = &finding.expected {
                    write!(writer, " expected={expected:?}")?;
                }
                if let Some(target) = &finding.target {
                    write!(writer, " actual={:?}", target.data_type)?;
                }
                writeln!(writer, ": {}", finding.message.escape_debug())?;
            }
            if !table.ignored_target_columns.is_empty() {
                writeln!(
                    writer,
                    "  ignored target columns: {:?}",
                    table.ignored_target_columns
                )?;
            }
        }
        for error in &self.errors {
            writeln!(
                writer,
                "error {}: {}",
                error.code,
                error.message.escape_debug()
            )?;
        }
        writeln!(
            writer,
            "tables: match={}, compatible={}, mismatch={}, unknown={}",
            self.counts.r#match, self.counts.compatible, self.counts.mismatch, self.counts.unknown
        )?;
        writeln!(
            writer,
            "Metadata only; value serialization and future DDL are outside this check."
        )
    }
}

fn status_name(status: Status) -> &'static str {
    match status {
        Status::Match => "match",
        Status::Compatible => "compatible",
        Status::Mismatch => "mismatch",
        Status::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operational_failure_overrides_matching_results() {
        let mut report = Report::default();
        report
            .errors
            .push(CatalogError::new("timeout", "query timed out"));
        report.finish();
        assert_eq!(report.exit_code(), 2);
        assert!(!report.complete);
        let mut output = Vec::new();
        report.write_json(&mut output).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(json["report_version"], 1);
        assert_eq!(json["status"], "unknown");
    }
}
