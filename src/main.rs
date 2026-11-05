use std::{
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
};

use clap::{Parser, Subcommand, ValueEnum};
use sync_pg2ch::{
    app,
    catalog::CatalogError,
    config::Config,
    ddl::{self, Mode, PlanReport, PlanStatus},
    report::Report,
};

#[derive(Parser)]
#[command(
    version,
    about = "PostgreSQL to ClickHouse schema diff and preflighted DDL"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Discover ClickHouse tables and compare them with PostgreSQL; never run migrations.
    Diff {
        /// TOML configuration with connections, a ClickHouse database and a PostgreSQL schema.
        #[arg(short, long)]
        config: PathBuf,
        /// Output format. JSON includes matching columns and source geo metadata.
        #[arg(long, value_enum, default_value_t = Format::Text)]
        format: Format,
    },
    /// Build an ALTER plan without modifying either database.
    Plan {
        #[arg(short, long)]
        config: PathBuf,
        #[arg(long, value_enum, default_value_t = PlanFormat::Text)]
        format: PlanFormat,
    },
    /// Apply a complete compatible ALTER plan; any known blocker prevents all DDL.
    Apply {
        #[arg(short, long)]
        config: PathBuf,
        #[arg(long, value_enum, default_value_t = Format::Text)]
        format: Format,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Format {
    Text,
    Json,
}

#[derive(Clone, Copy, ValueEnum)]
enum PlanFormat {
    Text,
    Json,
    Sql,
}

impl From<Format> for PlanFormat {
    fn from(format: Format) -> Self {
        match format {
            Format::Text => Self::Text,
            Format::Json => Self::Json,
        }
    }
}

enum Output {
    Diff(Report),
    Ddl(PlanReport),
}

#[tokio::main]
async fn main() -> ExitCode {
    let (output, format) = match Cli::parse().command {
        Command::Diff { config, format } => {
            (Output::Diff(diff_report(config).await), format.into())
        }
        Command::Plan { config, format } => {
            (Output::Ddl(ddl_report(config, Mode::Plan).await), format)
        }
        Command::Apply { config, format } => (
            Output::Ddl(ddl_report(config, Mode::Apply).await),
            format.into(),
        ),
    };
    let stdout = io::stdout();
    let mut stdout = io::BufWriter::new(stdout.lock());
    let result = match (&output, format) {
        (Output::Diff(report), PlanFormat::Text) => report.write_text(&mut stdout),
        (Output::Diff(report), PlanFormat::Json) => report.write_json(&mut stdout),
        (Output::Diff(_), PlanFormat::Sql) => {
            Err(io::Error::other("SQL output is only supported for plan"))
        }
        (Output::Ddl(report), PlanFormat::Text) => report.write_text(&mut stdout),
        (Output::Ddl(report), PlanFormat::Json) => report.write_json(&mut stdout),
        (Output::Ddl(report), PlanFormat::Sql) => report.write_sql(&mut stdout),
    }
    .and_then(|()| stdout.flush());
    if result.is_err() {
        eprintln!("cannot write report to stdout");
        return ExitCode::from(2);
    }
    let (errors, code) = match &output {
        Output::Diff(report) => (&report.errors, report.exit_code()),
        Output::Ddl(report) => {
            if let PlanFormat::Sql = format {
                for table in &report.tables {
                    for blocker in &table.blockers {
                        eprintln!(
                            "{} {:?}.{:?} {:?}: {}",
                            blocker.code,
                            table.target.database,
                            table.target.table,
                            blocker.column,
                            blocker.message.escape_debug()
                        );
                    }
                }
            }
            (&report.errors, report.exit_code())
        }
    };
    for error in errors {
        eprintln!("{}: {}", error.code, error.message.escape_debug());
    }
    ExitCode::from(code)
}

async fn diff_report(path: PathBuf) -> Report {
    match Config::read(&path) {
        Ok(config) => app::run(&config).await,
        Err(error) => {
            let mut report = Report::default();
            report.errors.push(CatalogError {
                code: "configuration_error",
                message: error.to_string(),
            });
            report.finish();
            report
        }
    }
}

async fn ddl_report(path: PathBuf, mode: Mode) -> PlanReport {
    match Config::read(&path) {
        Ok(config) => ddl::run(&config, mode).await,
        Err(error) => {
            let mut report = PlanReport::new(mode);
            report.errors.push(CatalogError {
                code: "configuration_error",
                message: error.to_string(),
            });
            report.status = PlanStatus::Blocked;
            report
        }
    }
}
