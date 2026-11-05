use std::io::{self, Write};

use super::{ExecutionState, Mode, PlanReport, PlanStatus};
use crate::model::Status;

impl PlanReport {
    /// Exit 0 for a ready/completed plan, 1 for known blockers, 2 for incomplete or failed execution.
    pub fn exit_code(&self) -> u8 {
        if !self.errors.is_empty()
            || self.tables.iter().any(|table| {
                table
                    .blockers
                    .iter()
                    .any(|blocker| blocker.status == Status::Unknown)
            })
        {
            return 2;
        }
        match self.status {
            PlanStatus::Ready | PlanStatus::Completed => 0,
            PlanStatus::Blocked => 1,
            PlanStatus::Failed | PlanStatus::Partial | PlanStatus::Indeterminate => 2,
        }
    }

    /// Serialize a version-one report.
    pub fn write_json(&self, writer: &mut impl Write) -> io::Result<()> {
        serde_json::to_writer_pretty(&mut *writer, self).map_err(io::Error::other)?;
        writeln!(writer)
    }

    /// Emit SQL only when every table is executable.
    pub fn write_sql(&self, writer: &mut impl Write) -> io::Result<()> {
        if self.blocked() {
            return Ok(());
        }
        for table in &self.tables {
            if let Some(sql) = &table.sql {
                writeln!(writer, "{sql}")?;
            }
        }
        Ok(())
    }

    /// Write an overview, proposed statements and all blockers.
    pub fn write_text(&self, writer: &mut impl Write) -> io::Result<()> {
        let mode = match self.mode {
            Mode::Plan => "plan",
            Mode::Apply => "apply",
        };
        let status = match self.status {
            PlanStatus::Ready => "ready",
            PlanStatus::Blocked => "blocked",
            PlanStatus::Completed => "completed",
            PlanStatus::Failed => "failed",
            PlanStatus::Partial => "partial",
            PlanStatus::Indeterminate => "indeterminate",
        };
        writeln!(writer, "{mode}: {status}")?;
        let mut operations = 0;
        let mut blockers = 0;
        for table in &self.tables {
            let state = match table.execution {
                ExecutionState::Unchanged => "unchanged",
                ExecutionState::NotSent => "not_sent",
                ExecutionState::Completed => "completed",
                ExecutionState::Failed => "failed",
                ExecutionState::Indeterminate => "indeterminate",
            };
            writeln!(
                writer,
                "{:?}.{:?}: {state}",
                table.target.database, table.target.table
            )?;
            if let (Some(database), Some(engine)) = (&table.database_engine, &table.engine) {
                writeln!(writer, "  database_engine={database}, engine={engine}")?;
            }
            if let Some(sql) = &table.sql {
                writeln!(writer, "{sql}")?;
            }
            for blocker in &table.blockers {
                write!(writer, "  blocked {}", blocker.code)?;
                if let Some(column) = &blocker.column {
                    write!(writer, " {column:?}")?;
                }
                writeln!(writer, ": {}", blocker.message.escape_debug())?;
            }
            if let Some(query_id) = &table.query_id {
                writeln!(writer, "  query_id={query_id}")?;
            }
            operations += table.operations.len();
            blockers += table.blockers.len();
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
            "tables={}, operations={operations}, blockers={blockers}",
            self.tables.len()
        )?;
        if self.status == PlanStatus::Blocked {
            writeln!(writer, "No DDL was sent.")?;
        }
        Ok(())
    }
}
