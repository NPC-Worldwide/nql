use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

/// Represents a parsed NQL model file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NqlModel {
    /// Name of the model (derived from filename).
    pub name: String,
    /// Configuration from YAML frontmatter.
    pub config: ModelConfig,
    /// Raw SQL body (with nql.* calls and {{ ref() }} intact).
    pub raw_sql: String,
    /// All nql.* function calls found in the SQL.
    pub nql_calls: Vec<NqlCall>,
    /// All {{ ref('...') }} references found in the SQL.
    pub refs: Vec<String>,
}

/// Model configuration extracted from YAML frontmatter comments.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelConfig {
    #[serde(default = "default_materialized")]
    pub materialized: String,
    #[serde(default)]
    pub schema: Option<String>,
    /// Extra key-value pairs.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_yaml::Value>,
}

fn default_materialized() -> String {
    "view".to_string()
}

/// A single nql.* function call extracted from SQL.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NqlCall {
    /// The full matched text, e.g. `nql.analyze_sentiment(feedback_text)`
    pub full_match: String,
    /// Function name without the `nql.` prefix, e.g. `analyze_sentiment`
    pub function_name: String,
    /// The arguments string (everything inside the parentheses).
    pub args: String,
}

/// Known NQL AI functions.
pub const NQL_FUNCTIONS: &[&str] = &[
    "generate_text",
    "summarize",
    "analyze_sentiment",
    "translate",
    "extract_entities",
    "generate_embedding",
    "sentiment",
    "get_facts",
    "identify_groups",
    "classify",
    "classify_into",
    "extract_json",
    "detect_language",
    "answer_question",
    "generate_code",
    "criticize",
    "synthesize",
    "breathe",
    "zoom_in",
    "abstract",
    "generate_groups",
    "remove_redundant_groups",
    "assign_groups_to_fact",
    "get_related_concepts_multi",
    "get_related_facts_llm",
    "find_best_link_concept",
    "consolidate_facts",
    "prune_fact_subset",
];

/// Parse a .sql model file from a path.
pub fn parse_model_file(path: &Path) -> Result<NqlModel, String> {
    let content = fs::read_to_string(path)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;

    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();

    parse_model(&name, &content)
}

/// Parse a model from its name and SQL content string.
pub fn parse_model(name: &str, content: &str) -> Result<NqlModel, String> {
    let config = extract_frontmatter(content)?;
    let raw_sql = strip_frontmatter(content);
    let nql_calls = extract_nql_calls(&raw_sql);
    let refs = extract_refs(&raw_sql);

    Ok(NqlModel {
        name: name.to_string(),
        config,
        raw_sql,
        nql_calls,
        refs,
    })
}

/// Extract YAML frontmatter from SQL comment lines.
///
/// Frontmatter is encoded as consecutive `-- ` comment lines at the top of
/// the file. A line starting with `-- config:` begins the YAML block, and
/// subsequent indented `--   key: value` lines continue it.
fn extract_frontmatter(content: &str) -> Result<ModelConfig, String> {
    let mut yaml_lines: Vec<String> = Vec::new();
    let mut in_config = false;

    for line in content.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with("-- config:") || trimmed.starts_with("--config:") {
            in_config = true;
            // Push "config:" portion
            let after = trimmed.trim_start_matches("--").trim();
            yaml_lines.push(after.to_string());
            continue;
        }

        if in_config {
            if let Some(after_dashes) = trimmed.strip_prefix("--") {
                // Check if this is still an indented config line
                if after_dashes.starts_with("  ") || after_dashes.starts_with('\t') {
                    yaml_lines.push(after_dashes.to_string());
                    continue;
                }
            }
            // End of config block
            break;
        }

        // Skip non-comment lines at top, or comment lines that aren't config
        if !trimmed.starts_with("--") && !trimmed.is_empty() {
            break;
        }
    }

    if yaml_lines.is_empty() {
        return Ok(ModelConfig::default());
    }

    let yaml_str = yaml_lines.join("\n");

    // The YAML is nested under "config:", so we parse the whole thing and
    // extract the "config" key.
    let top: HashMap<String, serde_yaml::Value> = serde_yaml::from_str(&yaml_str)
        .map_err(|e| format!("Failed to parse YAML frontmatter: {}", e))?;

    if let Some(config_val) = top.get("config") {
        let config: ModelConfig = serde_yaml::from_value(config_val.clone())
            .map_err(|e| format!("Failed to deserialize model config: {}", e))?;
        Ok(config)
    } else {
        Ok(ModelConfig::default())
    }
}

/// Strip frontmatter comment lines, returning only the SQL body.
fn strip_frontmatter(content: &str) -> String {
    let mut lines: Vec<&str> = Vec::new();
    let mut past_frontmatter = false;
    let mut in_config = false;

    for line in content.lines() {
        let trimmed = line.trim();

        if !past_frontmatter {
            if trimmed.starts_with("-- config:") || trimmed.starts_with("--config:") {
                in_config = true;
                continue;
            }

            if in_config {
                if let Some(after) = trimmed.strip_prefix("--") {
                    if after.starts_with("  ") || after.starts_with('\t') {
                        continue;
                    }
                }
                in_config = false;
                past_frontmatter = true;
            }

            // Skip leading blank lines and comment-only lines before SQL body
            if trimmed.is_empty() || (trimmed.starts_with("--") && !in_config) {
                // Could be a regular comment before the config or between config and SQL
                if !in_config {
                    // Keep regular comments that come after config block
                    if past_frontmatter {
                        lines.push(line);
                    }
                    continue;
                }
            }

            past_frontmatter = true;
        }

        lines.push(line);
    }

    lines.join("\n").trim().to_string()
}

/// Extract all AI function calls from SQL text.
/// Supports both `nql.func_name(args)` (legacy) and bare `func_name(args)` syntax.
pub fn extract_nql_calls(sql: &str) -> Vec<NqlCall> {
    let funcs = NQL_FUNCTIONS.join("|");
    let prefixed = format!(r"nql\.({})\(([^)]*)\)", funcs);
    let bare = format!(r"\b({})\(([^)]*)\)", funcs);

    let re_prefixed = Regex::new(&prefixed).expect("Invalid NQL regex");
    let re_bare = Regex::new(&bare).expect("Invalid bare NQL regex");

    let mut calls = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for cap in re_prefixed.captures_iter(sql) {
        let full = cap[0].to_string();
        if seen.insert(full.clone()) {
            calls.push(NqlCall {
                full_match: full,
                function_name: cap[1].to_string(),
                args: cap[2].to_string(),
            });
        }
    }

    for cap in re_bare.captures_iter(sql) {
        let full = cap[0].to_string();
        let prefixed_form = format!("nql.{}", full);
        if !seen.contains(&prefixed_form) && seen.insert(full.clone()) {
            calls.push(NqlCall {
                full_match: full,
                function_name: cap[1].to_string(),
                args: cap[2].to_string(),
            });
        }
    }

    calls
}

/// Extract all {{ ref('...') }} references from SQL text.
pub fn extract_refs(sql: &str) -> Vec<String> {
    let re =
        Regex::new(r#"\{\{\s*ref\(\s*['"]([^'"]+)['"]\s*\)\s*\}\}"#).expect("Invalid ref regex");

    re.captures_iter(sql)
        .map(|cap| cap[1].to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_nql_calls() {
        let sql = r#"
SELECT
    customer_id,
    feedback_text,
    nql.analyze_sentiment(feedback_text) as sentiment,
    nql.summarize(feedback_text) as summary
FROM customer_feedback
"#;
        let calls = extract_nql_calls(sql);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].function_name, "analyze_sentiment");
        assert_eq!(calls[0].args, "feedback_text");
        assert_eq!(calls[1].function_name, "summarize");
        assert_eq!(calls[1].args, "feedback_text");
    }

    #[test]
    fn test_extract_refs() {
        let sql = "FROM {{ ref('customer_feedback') }} JOIN {{ ref('orders') }}";
        let refs = extract_refs(sql);
        assert_eq!(refs, vec!["customer_feedback", "orders"]);
    }

    #[test]
    fn test_parse_frontmatter() {
        let content = r#"-- config:
--   materialized: table
--   schema: insights

SELECT 1
"#;
        let model = parse_model("test", content).unwrap();
        assert_eq!(model.config.materialized, "table");
        assert_eq!(model.config.schema, Some("insights".to_string()));
    }
}
