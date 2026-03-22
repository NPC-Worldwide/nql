use std::collections::HashMap;

use crate::parser::{NqlCall, NqlModel};
use regex::Regex;

/// Supported compilation targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Sqlite,
    Postgresql,
    Snowflake,
    Bigquery,
    Databricks,
}

impl Target {
    pub fn from_str(s: &str) -> Result<Self, String> {
        match s.to_lowercase().as_str() {
            "sqlite" => Ok(Target::Sqlite),
            "postgresql" | "postgres" | "pg" => Ok(Target::Postgresql),
            "snowflake" | "sf" => Ok(Target::Snowflake),
            "bigquery" | "bq" => Ok(Target::Bigquery),
            "databricks" | "dbx" | "spark" => Ok(Target::Databricks),
            _ => Err(format!("Unknown target: '{}'. Expected: sqlite, postgresql, snowflake, bigquery, databricks", s)),
        }
    }
}

/// Compiler transforms NQL model SQL into target-specific SQL.
pub struct Compiler {
    pub target: Target,
    /// Schema prefix for resolved refs (e.g. "insights" -> "insights.table_name").
    pub default_schema: Option<String>,
    /// Mapping from model name to fully qualified table name.
    pub ref_map: HashMap<String, String>,
}

impl Compiler {
    pub fn new(target: Target) -> Self {
        Compiler {
            target,
            default_schema: None,
            ref_map: HashMap::new(),
        }
    }

    /// Set the default schema for ref resolution.
    pub fn with_schema(mut self, schema: Option<String>) -> Self {
        self.default_schema = schema;
        self
    }

    /// Register a ref mapping: model_name -> fully qualified table name.
    pub fn register_ref(&mut self, model_name: &str, table_name: &str) {
        self.ref_map.insert(model_name.to_string(), table_name.to_string());
    }

    /// Compile a raw SQL string (with nql.* calls and {{ ref() }}) to target SQL.
    pub fn compile(&self, input_sql: &str, target: Target) -> String {
        let mut output = input_sql.to_string();

        // Replace nql.* function calls
        output = self.replace_nql_calls(&output, target);

        // Replace {{ ref('...') }} with resolved table names
        output = self.replace_refs(&output);

        output
    }

    /// Compile a full NqlModel, using its config for schema resolution.
    pub fn compile_model(&self, model: &NqlModel) -> String {
        let mut sql = self.compile(&model.raw_sql, self.target);

        // Wrap in CREATE TABLE/VIEW if configured
        let target_name = self.resolve_model_name(model);
        match model.config.materialized.as_str() {
            "table" => {
                sql = format!(
                    "CREATE TABLE IF NOT EXISTS {} AS\n{}",
                    target_name, sql
                );
            }
            "view" => {
                sql = format!(
                    "CREATE OR REPLACE VIEW {} AS\n{}",
                    target_name, sql
                );
            }
            "ephemeral" => {
                // No wrapping; used as CTE in downstream models
            }
            other => {
                sql = format!(
                    "-- Unknown materialization: {}\n{}",
                    other, sql
                );
            }
        }

        sql
    }

    /// Resolve the fully qualified name for a model.
    fn resolve_model_name(&self, model: &NqlModel) -> String {
        let schema = model
            .config
            .schema
            .as_deref()
            .or(self.default_schema.as_deref());

        match schema {
            Some(s) => format!("{}.{}", s, model.name),
            None => model.name.clone(),
        }
    }

    /// Replace all nql.* function calls with target-specific SQL.
    fn replace_nql_calls(&self, sql: &str, target: Target) -> String {
        use crate::parser::NQL_FUNCTIONS;
        let funcs = NQL_FUNCTIONS.join("|");

        let prefixed = format!(r"nql\.({})\(", funcs);
        let re_strip = Regex::new(&prefixed).expect("Invalid NQL prefix regex");
        let stripped = re_strip.replace_all(sql, |caps: &regex::Captures| {
            format!("{}(", &caps[1])
        }).to_string();

        let bare = format!(r"\b({})\(([^)]*)\)", funcs);
        let re_bare = Regex::new(&bare).expect("Invalid bare NQL regex");
        re_bare.replace_all(&stripped, |caps: &regex::Captures| {
            self.translate_function(&caps[1], &caps[2], target)
        }).to_string()
    }

    /// Translate a single nql.* function call to target SQL.
    fn translate_function(&self, func_name: &str, args: &str, target: Target) -> String {
        match target {
            Target::Sqlite => self.translate_sqlite(func_name, args),
            Target::Postgresql => self.translate_postgresql(func_name, args),
            Target::Snowflake => self.translate_snowflake(func_name, args),
            Target::Bigquery => self.translate_bigquery(func_name, args),
            Target::Databricks => self.translate_databricks(func_name, args),
        }
    }

    // ── SQLite: UDF-based ──────────────────────────────────────────────

    fn translate_sqlite(&self, func_name: &str, args: &str) -> String {
        format!("{}({})", func_name, args)
    }

    // ── PostgreSQL: pgai extension ─────────────────────────────────────

    fn translate_postgresql(&self, func_name: &str, args: &str) -> String {
        let arg_parts: Vec<&str> = args.split(',').map(|a| a.trim()).collect();

        match func_name {
            "generate_text" => {
                let col = arg_parts.first().copied().unwrap_or("''");
                let prompt = if arg_parts.len() > 1 { arg_parts[1] } else { "''" };
                format!(
                    "pgai.generate_text(prompt => {}, text => {})",
                    prompt, col
                )
            }
            "summarize" => {
                let col = arg_parts.first().copied().unwrap_or("''");
                format!("pgai.summarize(text => {})", col)
            }
            "analyze_sentiment" => {
                let col = arg_parts.first().copied().unwrap_or("''");
                format!("pgai.analyze_sentiment(text => {})", col)
            }
            "translate" => {
                let col = arg_parts.first().copied().unwrap_or("''");
                let lang = if arg_parts.len() > 1 { arg_parts[1] } else { "'en'" };
                format!(
                    "pgai.translate(text => {}, target_language => {})",
                    col, lang
                )
            }
            "extract_entities" => {
                let col = arg_parts.first().copied().unwrap_or("''");
                format!("pgai.extract_entities(text => {})", col)
            }
            "generate_embedding" => {
                let col = arg_parts.first().copied().unwrap_or("''");
                format!("pgai.generate_embedding(text => {})", col)
            }
            _ => format!("pgai.{}(text => {})", func_name, args),
        }
    }

    // ── Snowflake: ML functions ────────────────────────────────────────

    fn translate_snowflake(&self, func_name: &str, args: &str) -> String {
        match func_name {
            "generate_text" => format!("SNOWFLAKE.CORTEX.COMPLETE('llama3.1-8b', {})", args),
            "summarize" => format!("SNOWFLAKE.CORTEX.SUMMARIZE({})", args),
            "analyze_sentiment" => format!("SNOWFLAKE.CORTEX.SENTIMENT({})", args),
            "translate" => {
                let arg_parts: Vec<&str> = args.split(',').map(|a| a.trim()).collect();
                let col = arg_parts.first().copied().unwrap_or("''");
                let lang = if arg_parts.len() > 1 { arg_parts[1] } else { "'en'" };
                format!("SNOWFLAKE.CORTEX.TRANSLATE({}, {}, 'en')", col, lang)
            }
            "extract_entities" => format!("SNOWFLAKE.CORTEX.EXTRACT_ANSWER({}, 'Extract all entities')", args),
            "generate_embedding" => format!("SNOWFLAKE.CORTEX.EMBED_TEXT_768('snowflake-arctic-embed-m-v1.5', {})", args),
            _ => format!("SNOWFLAKE.CORTEX.COMPLETE('llama3.1-8b', {})", args),
        }
    }

    // ── BigQuery: ML functions ─────────────────────────────────────────

    fn translate_bigquery(&self, func_name: &str, args: &str) -> String {
        match func_name {
            "generate_text" => {
                format!(
                    "ML.GENERATE_TEXT(MODEL `nql_model`, (SELECT {} AS prompt), STRUCT(256 AS max_output_tokens))",
                    args
                )
            }
            "summarize" => {
                format!(
                    "ML.GENERATE_TEXT(MODEL `nql_model`, (SELECT CONCAT('Summarize: ', {}) AS prompt), STRUCT(512 AS max_output_tokens))",
                    args
                )
            }
            "analyze_sentiment" => {
                format!(
                    "ML.GENERATE_TEXT(MODEL `nql_model`, (SELECT CONCAT('Analyze sentiment: ', {}) AS prompt), STRUCT(64 AS max_output_tokens))",
                    args
                )
            }
            "translate" => {
                let arg_parts: Vec<&str> = args.split(',').map(|a| a.trim()).collect();
                let col = arg_parts.first().copied().unwrap_or("''");
                let lang = if arg_parts.len() > 1 { arg_parts[1] } else { "'en'" };
                format!(
                    "ML.GENERATE_TEXT(MODEL `nql_model`, (SELECT CONCAT('Translate to ', {}, ': ', {}) AS prompt), STRUCT(512 AS max_output_tokens))",
                    lang, col
                )
            }
            "extract_entities" => {
                format!(
                    "ML.GENERATE_TEXT(MODEL `nql_model`, (SELECT CONCAT('Extract entities from: ', {}) AS prompt), STRUCT(256 AS max_output_tokens))",
                    args
                )
            }
            "generate_embedding" => {
                format!(
                    "ML.GENERATE_TEXT_EMBEDDING(MODEL `nql_embedding_model`, (SELECT {} AS content), STRUCT(TRUE AS flatten_json_output))",
                    args
                )
            }
            _ => {
                format!(
                    "ML.GENERATE_TEXT(MODEL `nql_model`, (SELECT {} AS prompt), STRUCT(256 AS max_output_tokens))",
                    args
                )
            }
        }
    }

    /// Replace {{ ref('model_name') }} with the resolved table name.
    fn translate_databricks(&self, func_name: &str, args: &str) -> String {
        let arg_parts: Vec<&str> = args.split(',').map(|a| a.trim()).collect();
        match func_name {
            "generate_text" => {
                let col = arg_parts.first().copied().unwrap_or("''");
                let prompt = if arg_parts.len() > 1 { arg_parts[1] } else { "''" };
                format!("ai_query('databricks-meta-llama-3-1-70b-instruct', CONCAT({}, ' ', {}))", prompt, col)
            }
            "summarize" => {
                let col = arg_parts.first().copied().unwrap_or("''");
                format!("ai_query('databricks-meta-llama-3-1-70b-instruct', CONCAT('Summarize: ', {}))", col)
            }
            "analyze_sentiment" => {
                let col = arg_parts.first().copied().unwrap_or("''");
                format!("ai_query('databricks-meta-llama-3-1-70b-instruct', CONCAT('Analyze sentiment of: ', {}))", col)
            }
            "translate" => {
                let col = arg_parts.first().copied().unwrap_or("''");
                let lang = if arg_parts.len() > 1 { arg_parts[1] } else { "'en'" };
                format!("ai_query('databricks-meta-llama-3-1-70b-instruct', CONCAT('Translate to ', {}, ': ', {}))", lang, col)
            }
            "extract_entities" => {
                let col = arg_parts.first().copied().unwrap_or("''");
                format!("ai_query('databricks-meta-llama-3-1-70b-instruct', CONCAT('Extract entities from: ', {}))", col)
            }
            "generate_embedding" => {
                let col = arg_parts.first().copied().unwrap_or("''");
                format!("ai_query('databricks-bge-large-en', {})", col)
            }
            _ => format!("ai_query('databricks-meta-llama-3-1-70b-instruct', {})", args),
        }
    }

    fn replace_refs(&self, sql: &str) -> String {
        let re = Regex::new(r#"\{\{\s*ref\(\s*['"]([^'"]+)['"]\s*\)\s*\}\}"#)
            .expect("Invalid ref regex");

        re.replace_all(sql, |caps: &regex::Captures| {
            let model_name = &caps[1];
            self.resolve_ref(model_name)
        })
        .to_string()
    }

    /// Resolve a ref to its table name.
    fn resolve_ref(&self, model_name: &str) -> String {
        if let Some(table) = self.ref_map.get(model_name) {
            return table.clone();
        }

        // Default: use schema prefix if available
        match &self.default_schema {
            Some(schema) => format!("{}.{}", schema, model_name),
            None => model_name.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sqlite_compilation() {
        let compiler = Compiler::new(Target::Sqlite);
        let input = "SELECT nql.analyze_sentiment(feedback_text) as sentiment FROM feedback";
        let output = compiler.compile(input, Target::Sqlite);
        assert!(output.contains("analyze_sentiment(feedback_text)"));
        assert!(!output.contains("nql."));
    }

    #[test]
    fn test_postgresql_compilation() {
        let compiler = Compiler::new(Target::Postgresql);
        let input = "SELECT nql.summarize(feedback_text) as summary FROM feedback";
        let output = compiler.compile(input, Target::Postgresql);
        assert!(output.contains("pgai.summarize(text => feedback_text)"));
    }

    #[test]
    fn test_snowflake_compilation() {
        let compiler = Compiler::new(Target::Snowflake);
        let input = "SELECT nql.analyze_sentiment(feedback_text) FROM feedback";
        let output = compiler.compile(input, Target::Snowflake);
        assert!(output.contains("SNOWFLAKE.CORTEX.SENTIMENT(feedback_text)"));
    }

    #[test]
    fn test_databricks_compilation() {
        let compiler = Compiler::new(Target::Databricks);
        let input = "SELECT summarize(feedback_text) FROM feedback";
        let output = compiler.compile(input, Target::Databricks);
        assert!(output.contains("ai_query("));
    }

    #[test]
    fn test_ref_resolution() {
        let mut compiler = Compiler::new(Target::Sqlite);
        compiler.register_ref("customer_feedback", "raw.customer_feedback");
        let input = "SELECT * FROM {{ ref('customer_feedback') }}";
        let output = compiler.compile(input, Target::Sqlite);
        assert!(output.contains("raw.customer_feedback"));
    }

    #[test]
    fn test_bare_function_sqlite() {
        let compiler = Compiler::new(Target::Sqlite);
        let input = "SELECT analyze_sentiment(feedback_text) as sentiment FROM feedback";
        let output = compiler.compile(input, Target::Sqlite);
        assert!(output.contains("analyze_sentiment(feedback_text)"));
    }

    #[test]
    fn test_bare_function_snowflake() {
        let compiler = Compiler::new(Target::Snowflake);
        let input = "SELECT summarize(notes) as summary FROM meetings";
        let output = compiler.compile(input, Target::Snowflake);
        assert!(output.contains("SNOWFLAKE.CORTEX.SUMMARIZE(notes)"));
    }

    #[test]
    fn test_bare_and_prefixed_mixed() {
        let compiler = Compiler::new(Target::Sqlite);
        let input = "SELECT analyze_sentiment(a), nql.summarize(b) FROM t";
        let output = compiler.compile(input, Target::Sqlite);
        assert!(output.contains("analyze_sentiment(a)"));
        assert!(output.contains("summarize(b)"));
        assert!(!output.contains("nql."));
    }

    #[test]
    fn test_ref_default_schema() {
        let compiler = Compiler::new(Target::Sqlite).with_schema(Some("public".to_string()));
        let input = "SELECT * FROM {{ ref('orders') }}";
        let output = compiler.compile(input, Target::Sqlite);
        assert!(output.contains("public.orders"));
    }
}
