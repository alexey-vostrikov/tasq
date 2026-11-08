#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use tasq::db;

#[derive(Parser)]
#[command(
    name = "tasq",
    version,
    about = "Local-first task tracker for AI agents",
    long_about = None
)]
struct Cli {
    /// Path to the database; overrides the upward search for .tasq/tasq.db
    #[arg(long, global = true, value_name = "PATH")]
    db: Option<PathBuf>,

    /// Machine-readable output
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create .tasq/tasq.db in the current directory and register a project
    Init {
        /// Project key used in dot-notation IDs ([A-Z0-9]{2,8})
        #[arg(long, default_value = "TASQ")]
        key: String,
        /// Project name
        #[arg(long)]
        name: Option<String>,
        /// Keep the database in the current directory instead of the project root
        #[arg(long, hide = true)]
        here: bool,
    },
    /// Check database integrity, schema version and lease hygiene
    Doctor {
        /// Only check, never modify (currently the default behaviour)
        #[arg(long)]
        check: bool,
        /// Close expired attempts and release their reservations
        #[arg(long)]
        gc: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(code) => code,
        Err(err) => {
            report_error(&err, cli.json);
            ExitCode::from(1)
        }
    }
}

fn run(cli: &Cli) -> tasq::Result<ExitCode> {
    match &cli.command {
        Command::Init { key, name, here: _ } => {
            let cwd = std::env::current_dir()?;
            let name = name.clone().unwrap_or_else(|| key.clone());
            let outcome = db::init(&cwd, key, &name)?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&outcome).expect("serialisable")
                );
            } else {
                println!("database: {}", outcome.db_path);
                println!("schema:   v{}", outcome.schema_version);
                println!(
                    "project:  {} ({})",
                    outcome.project_key,
                    if outcome.project_created {
                        "created"
                    } else {
                        "already present"
                    }
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Doctor { check: _, gc } => {
            let cwd = std::env::current_dir()?;
            let path = db::discover(&cwd, cli.db.as_deref())?;
            let report = db::doctor(&path)?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report).expect("serialisable")
                );
            } else {
                println!("database:  {}", report.db_path);
                println!(
                    "schema:    v{} (supported v{})",
                    report.schema_version, report.supported_version
                );
                println!("integrity: {}", report.integrity);
                println!(
                    "foreign keys: {} violation(s)",
                    report.foreign_key_violations
                );
                println!(
                    "data:      {} project(s), {} node(s), {} live lease(s)",
                    report.projects, report.nodes, report.live_attempts
                );
                if report.expired_attempts > 0 {
                    println!(
                        "stale:     {} expired attempt(s), {} orphan reservation(s)",
                        report.expired_attempts, report.orphan_reservations
                    );
                }
                println!("status:    {}", if report.ok { "ok" } else { "FAILED" });
            }
            if *gc {
                println!("--gc is not implemented yet (planned for M2)");
            }
            Ok(if report.ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
    }
}

fn report_error(err: &tasq::Error, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&err.to_json()).expect("serialisable")
        );
    } else {
        eprintln!("error[{}]: {}", err.code(), err);
        eprintln!("hint: {}", err.hint());
    }
}
