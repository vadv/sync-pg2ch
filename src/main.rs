use std::{
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
};

use clap::{Parser, Subcommand, ValueEnum};
use sync_pg2ch::{app, catalog::CatalogError, config::Config, report::Report};

#[derive(Parser)]
#[command(
    version,
    about = "Read-only PostgreSQL to ClickHouse schema compatibility diff"
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
}

#[derive(Clone, Copy, ValueEnum)]
enum Format {
    Text,
    Json,
}

#[tokio::main]
async fn main() -> ExitCode {
    let Cli {
        command: Command::Diff { config, format },
    } = Cli::parse();
    let report = match Config::read(&config) {
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
    };
    let stdout = io::stdout();
    let mut stdout = io::BufWriter::new(stdout.lock());
    let result = match format {
        Format::Text => report.write_text(&mut stdout),
        Format::Json => report.write_json(&mut stdout),
    }
    .and_then(|()| stdout.flush());
    if result.is_err() {
        eprintln!("cannot write diff report to stdout");
        return ExitCode::from(2);
    }
    for error in &report.errors {
        eprintln!("{}: {}", error.code, error.message);
    }
    ExitCode::from(report.exit_code())
}
