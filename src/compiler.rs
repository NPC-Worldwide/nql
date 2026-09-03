use std::collections::HashMap;

use crate::parser::NqlModel;
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

impl std::str::FromStr for Target {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
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

    /// Split a comma-separated argument string while respecting single-quoted
    /// strings and square brackets. This prevents internal commas inside SQL
    /// literals or JSON arrays from being treated as delimiters.
    fn split_sql_args(args: &str) -> Vec<&str> {
        let mut parts = Vec::new();
        let mut start = 0;
        let mut in_single_quote = false;
        let mut bracket_depth: usize = 0;
        let chars: Vec<char> = args.chars().collect();

        for (i, &ch) in chars.iter().enumerate() {
            if ch == '\'' {
                in_single_quote = !in_single_quote;
            } else if !in_single_quote {
                match ch {
                    '[' => bracket_depth += 1,
                    ']' => bracket_depth = bracket_depth.saturating_sub(1),
                    ',' if bracket_depth == 0 => {
                        parts.push(args[start..i].trim());
                        start = i + 1;
                    }
                    _ => {}
                }
            }
        }
        let tail = args[start..].trim();
        if !tail.is_empty() {
            parts.push(tail);
        }
        parts
    }

    /// Set the default schema for ref resolution.
    pub fn with_schema(mut self, schema: Option<String>) -> Self {
        self.default_schema = schema;
        self
    }

    /// Register a ref mapping: model_name -> fully qualified table name.
    pub fn register_ref(&mut self, model_name: &str, table_name: &str) {
        self.ref_map
            .insert(model_name.to_string(), table_name.to_string());
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
                sql = format!("CREATE TABLE IF NOT EXISTS {} AS\n{}", target_name, sql);
            }
            "view" => {
                sql = format!("CREATE OR REPLACE VIEW {} AS\n{}", target_name, sql);
            }
            "incremental" => {
                match model
                    .config
                    .extra
                    .get("unique_key")
                    .and_then(|v| v.as_str())
                {
                    Some(key) => {
                        sql = format!(
                            "CREATE TABLE IF NOT EXISTS {} AS\n(SELECT * FROM (\n{}\n) WHERE FALSE);\nINSERT INTO {}\nSELECT s.* FROM (\n{}\n) s\nWHERE NOT EXISTS (SELECT 1 FROM {} t WHERE t.{} = s.{});",
                            target_name, sql, target_name, sql, target_name, key, key
                        );
                    }
                    None => {
                        sql = format!(
                            "-- incremental materialization requires unique_key in config\nCREATE TABLE IF NOT EXISTS {} AS\n{}",
                            target_name, sql
                        );
                    }
                }
            }
            "ephemeral" => {
                // No wrapping; used as CTE in downstream models
            }
            other => {
                sql = format!("-- Unknown materialization: {}\n{}", other, sql);
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
        let stripped = re_strip
            .replace_all(sql, |caps: &regex::Captures| format!("{}(", &caps[1]))
            .to_string();

        let bare = format!(r"\b({})\(([^)]*)\)", funcs);
        let re_bare = Regex::new(&bare).expect("Invalid bare NQL regex");
        re_bare
            .replace_all(&stripped, |caps: &regex::Captures| {
                self.translate_function(&caps[1], &caps[2], target)
            })
            .to_string()
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
        let arg_parts = Self::split_sql_args(args);

        match func_name {
            "generate_text" => {
                let col = arg_parts.first().copied().unwrap_or("''");
                let prompt = if arg_parts.len() > 1 {
                    arg_parts[1]
                } else {
                    "''"
                };
                format!("pgai.generate_text(prompt => {}, text => {})", prompt, col)
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
                let lang = if arg_parts.len() > 1 {
                    arg_parts[1]
                } else {
                    "'en'"
                };
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
                let arg_parts = Self::split_sql_args(args);
                let col = arg_parts.first().copied().unwrap_or("''");
                let lang = if arg_parts.len() > 1 {
                    arg_parts[1]
                } else {
                    "'en'"
                };
                format!("SNOWFLAKE.CORTEX.TRANSLATE({}, {}, 'en')", col, lang)
            }
            "extract_entities" => format!(
                "SNOWFLAKE.CORTEX.EXTRACT_ANSWER({}, 'Extract all entities')",
                args
            ),
            "generate_embedding" => format!(
                "SNOWFLAKE.CORTEX.EMBED_TEXT_768('snowflake-arctic-embed-m-v1.5', {})",
                args
            ),
            _ => format!("SNOWFLAKE.CORTEX.COMPLETE('llama3.1-8b', {})", args),
        }
    }

    // ── BigQuery: built-in AI functions ────────────────────────────────

    fn translate_bigquery(&self, func_name: &str, args: &str) -> String {
        match func_name {
            "generate_text" => {
                format!("(AI.GENERATE({})).result", args)
            }
            "summarize" => {
                format!("(AI.GENERATE(CONCAT('Summarize: ', {}))).result", args)
            }
            "analyze_sentiment" => {
                format!(
                    "(AI.GENERATE(CONCAT('Analyze sentiment: ', {}))).result",
                    args
                )
            }
            "translate" => {
                let arg_parts = Self::split_sql_args(args);
                let col = arg_parts.first().copied().unwrap_or("''");
                let lang = if arg_parts.len() > 1 {
                    arg_parts[1]
                } else {
                    "'en'"
                };
                format!(
                    "(AI.GENERATE(CONCAT('Translate to ', {}, ': ', {}))).result",
                    lang, col
                )
            }
            "extract_entities" => {
                format!(
                    "(AI.GENERATE(CONCAT('Extract entities from: ', {}))).result",
                    args
                )
            }
            "generate_embedding" => {
                format!("(AI.GENERATE_EMBEDDING({})).result", args)
            }
            "sentiment" => {
                format!("(AI.GENERATE(CONCAT('Analyze the sentiment of the following text. Respond with exactly one word: positive, negative, or neutral.\\n\\n', {}))).result", args)
            }
            "get_facts" => {
                format!("(AI.GENERATE(CONCAT('Extract facts from this text. A fact is a specific statement that can be sourced from the text. Return as JSON array of objects with \"statement\", \"source_text\", and \"type\" (explicit or inferred) fields.\\n\\nText: ', {}))).result", args)
            }
            "identify_groups" => {
                format!("(AI.GENERATE(CONCAT('What are the main groups these items could be organized into? Return as JSON array of group names.\\n\\nItems: ', {}))).result", args)
            }
            "classify" => {
                format!("(AI.GENERATE(CONCAT('Classify the following text into a category. Return only the category name.\\n\\n', {}))).result", args)
            }
            "classify_into" => {
                let arg_parts = Self::split_sql_args(args);
                let col = arg_parts.first().copied().unwrap_or("''");
                let categories = arg_parts.get(1).copied().unwrap_or("''");
                format!("(AI.GENERATE(CONCAT('Classify the following text into one of these categories: ', {}, '. Return only the category name.\\n\\n', {}))).result", categories, col)
            }
            "extract_json" => {
                format!("(AI.GENERATE(CONCAT('Extract structured data from this text and return as valid JSON:\\n\\n', {}))).result", args)
            }
            "detect_language" => {
                format!("(AI.GENERATE(CONCAT('Detect the language of this text. Return only the ISO 639-1 language code.\\n\\n', {}))).result", args)
            }
            "answer_question" => {
                let arg_parts = Self::split_sql_args(args);
                let context = arg_parts.first().copied().unwrap_or("''");
                let question = arg_parts.get(1).copied().unwrap_or("''");
                format!("(AI.GENERATE(CONCAT('Based on the following context, answer the question.\\n\\nContext: ', {}, '\\n\\nQuestion: ', {}))).result", context, question)
            }
            "generate_code" => {
                format!("(AI.GENERATE(CONCAT('Generate code for the following task. Return only the code, no explanation.\\n\\n', {}))).result", args)
            }
            "criticize" => {
                format!("(AI.GENERATE(CONCAT('Provide a critical analysis and constructive criticism of the following, focused on weaknesses, improvements, and alternatives:\\n\\n', {}))).result", args)
            }
            "synthesize" => {
                format!("(AI.GENERATE(CONCAT('Synthesize this content into a clear, concise summary that captures the essence:\\n\\n', {}))).result", args)
            }
            "breathe" => {
                format!("(AI.GENERATE(CONCAT('Read the following and identify the high level objective, most recent task, accomplishments, and failures. Return as JSON with keys: high_level_objective, most_recent_task, accomplishments, failures.\\n\\n', {}))).result", args)
            }
            "zoom_in" => {
                format!("(AI.GENERATE(CONCAT('Look at these facts and infer new implied facts. Return as JSON array of objects with \"statement\" and \"inferred_from\" fields.\\n\\n', {}))).result", args)
            }
            "abstract" => {
                format!("(AI.GENERATE(CONCAT('Create more abstract categories from this list of groups. Group names should never be more than two words, should not contain gerunds, and should never contain conjunctions like AND or OR. Generate no more than 5 new concepts and no fewer than 2. Return as JSON: {{\\\"groups\\\": [{{\\\"name\\\": \\\"abstract category name\\\"}}]}}.\\n\\nGroups: ', {}))).result", args)
            }
            "generate_groups" => {
                format!("(AI.GENERATE(CONCAT('Generate conceptual groups for these facts. Group names should never be more than two words, should not contain gerunds, and should never contain conjunctions like AND or OR. Return as JSON: {{\\\"groups\\\": [{{\\\"name\\\": \\\"group name\\\"}}]}}.\\n\\nFacts: ', {}))).result", args)
            }
            "remove_redundant_groups" => {
                format!("(AI.GENERATE(CONCAT('Remove redundant groups from this list. Merge similar groups and keep only distinct concepts. Group names should never be more than two words, should not contain gerunds, and should never contain conjunctions like AND or OR. Return as JSON: {{\\\"groups\\\": [{{\\\"name\\\": \\\"final group name\\\"}}]}}.\\n\\nGroups: ', {}))).result", args)
            }
            "assign_groups_to_fact" => {
                let arg_parts = Self::split_sql_args(args);
                let fact = arg_parts.first().copied().unwrap_or("''");
                let groups = arg_parts.get(1).copied().unwrap_or("''");
                format!("(AI.GENERATE(CONCAT('Given this fact, assign it to any relevant groups. Return as JSON: {{\\\"groups\\\": [\\\"list of group names\\\"]}}.\\n\\nFact: ', {}, '\\n\\nGroups: ', {}))).result", fact, groups)
            }
            "get_related_concepts_multi" => {
                let arg_parts = Self::split_sql_args(args);
                let node_name = arg_parts.first().copied().unwrap_or("''");
                let node_type = arg_parts.get(1).copied().unwrap_or("'fact'");
                let concepts = arg_parts.get(2).copied().unwrap_or("''");
                format!("(AI.GENERATE(CONCAT('Which of the following concepts relate to the given ', {}, '? Select all that apply from most specific to most abstract. ', INITCAP({}), ': \"', {}, '\"\\n\\nAvailable Concepts: ', {}, '\\n\\nReturn as JSON: {{\\\"related_concepts\\\": [\\\"Concept A\\\", \\\"Concept B\\\"]}}'))).result", node_type, node_type, node_name, concepts)
            }
            "get_related_facts_llm" => {
                let arg_parts = Self::split_sql_args(args);
                let new_fact = arg_parts.first().copied().unwrap_or("''");
                let existing_facts = arg_parts.get(1).copied().unwrap_or("''");
                format!("(AI.GENERATE(CONCAT('A new fact has been learned: \"', {}, '\". Which of the following existing facts are directly related to it (causally, sequentially, or thematically)? Select only the most direct and meaningful connections.\\n\\nExisting Facts: ', {}, '\\n\\nReturn as JSON: {{\\\"related_facts\\\": [\\\"statement of a related fact\\\"]}}'))).result", new_fact, existing_facts)
            }
            "find_best_link_concept" => {
                let arg_parts = Self::split_sql_args(args);
                let candidate = arg_parts.first().copied().unwrap_or("''");
                let existing = arg_parts.get(1).copied().unwrap_or("''");
                format!("(AI.GENERATE(CONCAT('Here is a new candidate concept: \"', {}, '\". Which of the following existing concepts is it most closely related to? The relationship could be as a sub-category, a similar idea, or a related domain. Respond with the single best-fit concept from the list, or \"none\" if it is genuinely new.\\n\\nExisting Concepts: ', {}, '\\n\\nReturn as JSON: {{\\\"best_link_concept\\\": \\\"The single best concept name OR none\\\"}}'))).result", candidate, existing)
            }
            "consolidate_facts" => {
                let arg_parts = Self::split_sql_args(args);
                let new_fact = arg_parts.first().copied().unwrap_or("''");
                let existing_facts = arg_parts.get(1).copied().unwrap_or("''");
                format!("(AI.GENERATE(CONCAT('A new fact has been learned: \"', {}, '\". Determine whether it duplicates or contradicts any of these existing facts. Return as JSON: {{\\\"action\\\": \\\"add|merge|replace|skip\\\", \\\"target_fact\\\": \\\"statement of existing fact or null\\\", \\\"final_statement\\\": \\\"merged statement or null\\\"}}.\\n\\nExisting Facts: ', {}))).result", new_fact, existing_facts)
            }
            "prune_fact_subset" => {
                format!("(AI.GENERATE(CONCAT('From the following list of facts, select the most informative subset that preserves the core meaning without redundancy. Return as JSON: {{\\\"kept_facts\\\": [\\\"fact statement\\\"]}}.\\n\\nFacts: ', {}))).result", args)
            }
            _ => {
                format!("(AI.GENERATE({})).result", args)
            }
        }
    }

    /// Replace {{ ref('model_name') }} with the resolved table name.
    fn translate_databricks(&self, func_name: &str, args: &str) -> String {
        let arg_parts = Self::split_sql_args(args);
        match func_name {
            "generate_text" => {
                let col = arg_parts.first().copied().unwrap_or("''");
                let prompt = if arg_parts.len() > 1 {
                    arg_parts[1]
                } else {
                    "''"
                };
                format!(
                    "ai_query('databricks-meta-llama-3-1-70b-instruct', CONCAT({}, ' ', {}))",
                    prompt, col
                )
            }
            "summarize" => {
                let col = arg_parts.first().copied().unwrap_or("''");
                format!(
                    "ai_query('databricks-meta-llama-3-1-70b-instruct', CONCAT('Summarize: ', {}))",
                    col
                )
            }
            "analyze_sentiment" => {
                let col = arg_parts.first().copied().unwrap_or("''");
                format!("ai_query('databricks-meta-llama-3-1-70b-instruct', CONCAT('Analyze sentiment of: ', {}))", col)
            }
            "translate" => {
                let col = arg_parts.first().copied().unwrap_or("''");
                let lang = if arg_parts.len() > 1 {
                    arg_parts[1]
                } else {
                    "'en'"
                };
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
            _ => format!(
                "ai_query('databricks-meta-llama-3-1-70b-instruct', {})",
                args
            ),
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
