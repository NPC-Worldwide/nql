use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::compiler::{Compiler, Target};
use crate::parser::{self, NqlModel};
use crate::sqlite_udf;

/// Errors from the runner.
#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Parse error for model '{model}': {message}")]
    Parse { model: String, message: String },

    #[error("Circular dependency detected involving model '{0}'")]
    CircularDependency(String),

    #[error("Missing dependency: model '{model}' references '{dependency}' which was not found")]
    MissingDependency { model: String, dependency: String },

    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("Compilation error: {0}")]
    Compilation(String),
}

/// Runner loads models from a directory, resolves dependencies, and executes them.
pub struct Runner {
    pub models_dir: PathBuf,
    pub target: Target,
    pub models: HashMap<String, NqlModel>,
    pub execution_order: Vec<String>,
}

impl Runner {
    /// Create a new runner for a given models directory and target.
    pub fn new(models_dir: &Path, target: Target) -> Self {
        Runner {
            models_dir: models_dir.to_path_buf(),
            target,
            models: HashMap::new(),
            execution_order: Vec::new(),
        }
    }

    /// Load all .sql model files from the models directory (recursively).
    pub fn load_models(&mut self) -> Result<(), RunnerError> {
        let sql_files = find_sql_files(&self.models_dir)?;

        for path in sql_files {
            let model = parser::parse_model_file(&path).map_err(|msg| RunnerError::Parse {
                model: path.display().to_string(),
                message: msg,
            })?;
            self.models.insert(model.name.clone(), model);
        }

        Ok(())
    }

    /// Resolve dependencies and compute execution order via topological sort.
    pub fn resolve_dependencies(&mut self) -> Result<(), RunnerError> {
        let model_names: HashSet<String> = self.models.keys().cloned().collect();
        let mut order: Vec<String> = Vec::new();
        let mut visited: HashSet<String> = HashSet::new();
        let mut in_progress: HashSet<String> = HashSet::new();

        for name in &model_names {
            if !visited.contains(name) {
                self.topo_sort(
                    name,
                    &model_names,
                    &mut visited,
                    &mut in_progress,
                    &mut order,
                )?;
            }
        }

        self.execution_order = order;
        Ok(())
    }

    fn topo_sort(
        &self,
        name: &str,
        all_models: &HashSet<String>,
        visited: &mut HashSet<String>,
        in_progress: &mut HashSet<String>,
        order: &mut Vec<String>,
    ) -> Result<(), RunnerError> {
        if in_progress.contains(name) {
            return Err(RunnerError::CircularDependency(name.to_string()));
        }
        if visited.contains(name) {
            return Ok(());
        }

        in_progress.insert(name.to_string());

        if let Some(model) = self.models.get(name) {
            for dep in &model.refs {
                if !all_models.contains(dep) {
                    // External table reference — skip dependency resolution
                    continue;
                }
                self.topo_sort(dep, all_models, visited, in_progress, order)?;
            }
        }

        in_progress.remove(name);
        visited.insert(name.to_string());
        order.push(name.to_string());
        Ok(())
    }

    /// Execute all models against a SQLite database.
    pub fn execute_sqlite(&self, db_path: &str) -> Result<(), RunnerError> {
        let conn = if db_path == ":memory:" {
            rusqlite::Connection::open_in_memory()?
        } else {
            rusqlite::Connection::open(db_path)?
        };

        // Register NQL UDFs
        sqlite_udf::register_nql_functions(&conn)?;

        let mut compiler = Compiler::new(Target::Sqlite);

        // Register all models as refs
        for model in self.models.values() {
            let schema = model.config.schema.as_deref().unwrap_or("");
            let table_name = if schema.is_empty() {
                model.name.clone()
            } else {
                format!("{}.{}", schema, model.name)
            };
            compiler.register_ref(&model.name, &table_name);
        }

        // Execute models in dependency order
        for model_name in &self.execution_order {
            if let Some(model) = self.models.get(model_name) {
                let compiled_sql = compiler.compile_model(model);
                eprintln!("[nql] Executing model: {}", model_name);
                eprintln!("[nql] SQL:\n{}\n", compiled_sql);
                conn.execute_batch(&compiled_sql).map_err(|e| {
                    RunnerError::Compilation(format!(
                        "Failed to execute model '{}': {}",
                        model_name, e
                    ))
                })?;
            }
        }

        Ok(())
    }

    /// Compile all models and execute them against BigQuery via the `bq` CLI,
    /// in dependency order.
    pub fn execute_bigquery(&self) -> Result<(), RunnerError> {
        let results = self.compile_all()?;

        let mut bq_args: Vec<String> = vec![
            "query".to_string(),
            "--use_legacy_sql=false".to_string(),
            "--format=none".to_string(),
        ];
        if let Ok(project) = std::env::var("NQL_BQ_PROJECT") {
            bq_args.push(format!("--project_id={}", project));
        }
        if let Ok(location) = std::env::var("NQL_BQ_LOCATION") {
            bq_args.push(format!("--location={}", location));
        }

        for (name, sql) in results {
            eprintln!("[nql] Executing model on BigQuery: {}", name);
            let mut child = std::process::Command::new("bq")
                .args(&bq_args)
                .stdin(std::process::Stdio::piped())
                .spawn()
                .map_err(|e| RunnerError::Compilation(format!("Failed to spawn bq CLI: {}", e)))?;
            if let Some(mut stdin) = child.stdin.take() {
                use std::io::Write;
                stdin.write_all(sql.as_bytes()).map_err(|e| {
                    RunnerError::Compilation(format!("Failed to write SQL to bq stdin: {}", e))
                })?;
            }
            let status = child
                .wait_with_output()
                .map_err(|e| RunnerError::Compilation(format!("Failed waiting for bq: {}", e)))?;
            if !status.status.success() {
                return Err(RunnerError::Compilation(format!(
                    "bq query failed for model '{}': {}",
                    name,
                    String::from_utf8_lossy(&status.stderr)
                )));
            }
        }

        Ok(())
    }

    /// Compile all models and return the SQL strings (for non-SQLite targets).
    pub fn compile_all(&self) -> Result<Vec<(String, String)>, RunnerError> {
        let mut compiler = Compiler::new(self.target);

        // Register all models as refs
        for model in self.models.values() {
            let schema = model.config.schema.as_deref().unwrap_or("");
            let table_name = if schema.is_empty() {
                model.name.clone()
            } else {
                format!("{}.{}", schema, model.name)
            };
            compiler.register_ref(&model.name, &table_name);
        }

        let mut results = Vec::new();
        for model_name in &self.execution_order {
            if let Some(model) = self.models.get(model_name) {
                let compiled = compiler.compile_model(model);
                results.push((model_name.clone(), compiled));
            }
        }

        Ok(results)
    }

    /// Execute or compile a single named model.
    pub fn run_single(
        &self,
        model_name: &str,
        db_path: Option<&str>,
    ) -> Result<String, RunnerError> {
        let model = self
            .models
            .get(model_name)
            .ok_or_else(|| RunnerError::Parse {
                model: model_name.to_string(),
                message: "Model not found".to_string(),
            })?;

        let mut compiler = Compiler::new(self.target);

        // Register all models as refs
        for m in self.models.values() {
            let schema = m.config.schema.as_deref().unwrap_or("");
            let table_name = if schema.is_empty() {
                m.name.clone()
            } else {
                format!("{}.{}", schema, m.name)
            };
            compiler.register_ref(&m.name, &table_name);
        }

        let compiled = compiler.compile_model(model);

        if self.target == Target::Sqlite {
            if let Some(path) = db_path {
                let conn = if path == ":memory:" {
                    rusqlite::Connection::open_in_memory()?
                } else {
                    rusqlite::Connection::open(path)?
                };
                sqlite_udf::register_nql_functions(&conn)?;
                conn.execute_batch(&compiled).map_err(|e| {
                    RunnerError::Compilation(format!(
                        "Failed to execute model '{}': {}",
                        model_name, e
                    ))
                })?;
            }
        }

        Ok(compiled)
    }
}

/// Recursively find all .sql files in a directory.
fn find_sql_files(dir: &Path) -> Result<Vec<PathBuf>, std::io::Error> {
    let mut files = Vec::new();

    if !dir.exists() {
        return Ok(files);
    }

    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            files.extend(find_sql_files(&path)?);
        } else if path.extension().and_then(|e| e.to_str()) == Some("sql") {
            files.push(path);
        }
    }

    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_dependency_resolution() {
        let tmp = std::env::temp_dir().join("nql_test_models");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        // Model A depends on nothing
        let mut f = fs::File::create(tmp.join("a.sql")).unwrap();
        writeln!(f, "SELECT 1 as id").unwrap();

        // Model B depends on A
        let mut f = fs::File::create(tmp.join("b.sql")).unwrap();
        writeln!(f, "SELECT * FROM {{{{ ref('a') }}}}").unwrap();

        let mut runner = Runner::new(&tmp, Target::Sqlite);
        runner.load_models().unwrap();
        runner.resolve_dependencies().unwrap();

        // A should come before B
        let a_pos = runner
            .execution_order
            .iter()
            .position(|n| n == "a")
            .unwrap();
        let b_pos = runner
            .execution_order
            .iter()
            .position(|n| n == "b")
            .unwrap();
        assert!(a_pos < b_pos);

        let _ = fs::remove_dir_all(&tmp);
    }
}
