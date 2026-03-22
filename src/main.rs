use std::path::PathBuf;
use std::process;

use clap::{Parser, Subcommand};

use nql::compiler::Target;
use nql::runner::Runner;

#[derive(Parser)]
#[command(name = "nql", about = "NQL — SQL compiler with AI function calls")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run model(s) against a database
    Run {
        /// Name of a specific model to run (omit to run all)
        model_name: Option<String>,

        /// Database connection string / path (e.g. "./my.db" for SQLite)
        #[arg(long, short = 'd')]
        db: Option<String>,

        /// Directory containing .sql model files
        #[arg(long, short = 'm', default_value = "models")]
        models_dir: PathBuf,

        /// Target database: sqlite, postgresql, snowflake, bigquery
        #[arg(long, short = 't', default_value = "sqlite")]
        target: String,
    },

    /// Compile model(s) and print the target SQL (dry-run)
    Compile {
        /// Name of a specific model to compile (omit to compile all)
        model_name: Option<String>,

        /// Directory containing .sql model files
        #[arg(long, short = 'm', default_value = "models")]
        models_dir: PathBuf,

        /// Target database: sqlite, postgresql, snowflake, bigquery
        #[arg(long, short = 't', default_value = "sqlite")]
        target: String,
    },
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Run {
            model_name,
            db,
            models_dir,
            target,
        } => {
            let target = match Target::from_str(&target) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("Error: {}", e);
                    process::exit(1);
                }
            };

            let mut runner = Runner::new(&models_dir, target);

            if let Err(e) = runner.load_models() {
                eprintln!("Error loading models: {}", e);
                process::exit(1);
            }

            if let Err(e) = runner.resolve_dependencies() {
                eprintln!("Error resolving dependencies: {}", e);
                process::exit(1);
            }

            eprintln!(
                "[nql] Loaded {} model(s), execution order: {:?}",
                runner.models.len(),
                runner.execution_order
            );

            match model_name {
                Some(name) => {
                    match runner.run_single(&name, db.as_deref()) {
                        Ok(sql) => {
                            println!("{}", sql);
                        }
                        Err(e) => {
                            eprintln!("Error running model '{}': {}", name, e);
                            process::exit(1);
                        }
                    }
                }
                None => {
                    if target == Target::Sqlite {
                        let db_path = db.as_deref().unwrap_or(":memory:");
                        if let Err(e) = runner.execute_sqlite(db_path) {
                            eprintln!("Error executing models: {}", e);
                            process::exit(1);
                        }
                        eprintln!("[nql] All models executed successfully.");
                    } else {
                        match runner.compile_all() {
                            Ok(results) => {
                                for (name, sql) in results {
                                    println!("-- ═══ Model: {} ═══", name);
                                    println!("{}", sql);
                                    println!();
                                }
                            }
                            Err(e) => {
                                eprintln!("Error compiling models: {}", e);
                                process::exit(1);
                            }
                        }
                    }
                }
            }
        }

        Commands::Compile {
            model_name,
            models_dir,
            target,
        } => {
            let target = match Target::from_str(&target) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("Error: {}", e);
                    process::exit(1);
                }
            };

            let mut runner = Runner::new(&models_dir, target);

            if let Err(e) = runner.load_models() {
                eprintln!("Error loading models: {}", e);
                process::exit(1);
            }

            if let Err(e) = runner.resolve_dependencies() {
                eprintln!("Error resolving dependencies: {}", e);
                process::exit(1);
            }

            match model_name {
                Some(name) => {
                    match runner.run_single(&name, None) {
                        Ok(sql) => println!("{}", sql),
                        Err(e) => {
                            eprintln!("Error compiling model '{}': {}", name, e);
                            process::exit(1);
                        }
                    }
                }
                None => {
                    match runner.compile_all() {
                        Ok(results) => {
                            for (name, sql) in results {
                                println!("-- ═══ Model: {} ═══", name);
                                println!("{}", sql);
                                println!();
                            }
                        }
                        Err(e) => {
                            eprintln!("Error compiling models: {}", e);
                            process::exit(1);
                        }
                    }
                }
            }
        }
    }
}
