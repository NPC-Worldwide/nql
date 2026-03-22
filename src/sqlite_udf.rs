use rusqlite::functions::FunctionFlags;
use rusqlite::types::{ToSqlOutput, Value};
use rusqlite::Connection;

const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434/api/generate";
const DEFAULT_MODEL: &str = "llama3.2";

#[derive(Debug, Clone)]
pub struct LlmConfig {
    pub backend: String,
    pub api_url: String,
    pub model: String,
    pub api_key: Option<String>,
}

impl Default for LlmConfig {
    fn default() -> Self {
        LlmConfig {
            backend: "ollama".to_string(),
            api_url: DEFAULT_OLLAMA_URL.to_string(),
            model: DEFAULT_MODEL.to_string(),
            api_key: None,
        }
    }
}

impl LlmConfig {
    pub fn from_env() -> Self {
        let backend = std::env::var("NQL_LLM_BACKEND").unwrap_or_else(|_| "ollama".to_string());
        let api_key = std::env::var("OPENAI_API_KEY").ok()
            .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
            .or_else(|| std::env::var("GEMINI_API_KEY").ok());

        let (api_url, model) = match backend.as_str() {
            "openai" => (
                std::env::var("NQL_API_URL").unwrap_or_else(|_| "https://api.openai.com/v1/chat/completions".to_string()),
                std::env::var("NQL_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_string()),
            ),
            _ => (
                std::env::var("NQL_API_URL").unwrap_or_else(|_| DEFAULT_OLLAMA_URL.to_string()),
                std::env::var("NQL_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string()),
            ),
        };

        LlmConfig { backend, api_url, model, api_key }
    }
}

fn call_llm(config: &LlmConfig, prompt: &str) -> Result<String, String> {
    let client = reqwest::blocking::Client::new();
    match config.backend.as_str() {
        "openai" => call_openai(&client, config, prompt),
        _ => call_ollama(&client, config, prompt),
    }
}

fn call_ollama(client: &reqwest::blocking::Client, config: &LlmConfig, prompt: &str) -> Result<String, String> {
    let body = serde_json::json!({"model": config.model, "prompt": prompt, "stream": false});
    let resp = client.post(&config.api_url).json(&body).send().map_err(|e| format!("Ollama: {}", e))?;
    let json: serde_json::Value = resp.json().map_err(|e| format!("Parse: {}", e))?;
    json["response"].as_str().map(|s| s.trim().to_string()).ok_or_else(|| "No response".to_string())
}

fn call_openai(client: &reqwest::blocking::Client, config: &LlmConfig, prompt: &str) -> Result<String, String> {
    let api_key = config.api_key.as_deref().ok_or("API key not set")?;
    let body = serde_json::json!({"model": config.model, "messages": [{"role": "user", "content": prompt}], "max_tokens": 512});
    let resp = client.post(&config.api_url).header("Authorization", format!("Bearer {}", api_key)).json(&body).send().map_err(|e| format!("OpenAI: {}", e))?;
    let json: serde_json::Value = resp.json().map_err(|e| format!("Parse: {}", e))?;
    json["choices"][0]["message"]["content"].as_str().map(|s| s.trim().to_string()).ok_or_else(|| "No content".to_string())
}

fn register_udf<F>(conn: &Connection, name: &str, config: &LlmConfig, prompt_builder: F) -> rusqlite::Result<()>
where F: Fn(&str) -> String + Send + 'static {
    let cfg = config.clone();
    conn.create_scalar_function(name, 1, FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC, move |ctx| {
        let input: String = ctx.get(0)?;
        let prompt = prompt_builder(&input);
        match call_llm(&cfg, &prompt) {
            Ok(result) => Ok(ToSqlOutput::Owned(Value::Text(result))),
            Err(e) => Ok(ToSqlOutput::Owned(Value::Text(format!("ERROR: {}", e)))),
        }
    })
}

fn register_udf2<F>(conn: &Connection, name: &str, config: &LlmConfig, prompt_builder: F) -> rusqlite::Result<()>
where F: Fn(&str, &str) -> String + Send + 'static {
    let cfg = config.clone();
    conn.create_scalar_function(name, 2, FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC, move |ctx| {
        let input1: String = ctx.get(0)?;
        let input2: String = ctx.get(1)?;
        let prompt = prompt_builder(&input1, &input2);
        match call_llm(&cfg, &prompt) {
            Ok(result) => Ok(ToSqlOutput::Owned(Value::Text(result))),
            Err(e) => Ok(ToSqlOutput::Owned(Value::Text(format!("ERROR: {}", e)))),
        }
    })
}

pub fn register_all_functions(conn: &Connection) -> rusqlite::Result<()> {
    let config = LlmConfig::from_env();

    register_udf(conn, "sentiment", &config, |text| {
        format!("Analyze the sentiment of the following text. Respond with exactly one word: positive, negative, or neutral.\n\n{}", text)
    })?;

    register_udf(conn, "summarize", &config, |text| {
        format!("Summarize the following text concisely:\n\n{}", text)
    })?;

    register_udf2(conn, "translate", &config, |text, lang| {
        format!("Translate the following text to {}:\n\n{}", lang, text)
    })?;

    register_udf(conn, "extract_entities", &config, |text| {
        format!("Extract all named entities (people, places, organizations) from the following text. Return as JSON array.\n\n{}", text)
    })?;

    register_udf(conn, "generate_text", &config, |prompt| {
        prompt.to_string()
    })?;

    register_udf(conn, "generate_embedding", &config, |text| {
        format!("Generate a semantic embedding description for: {}", text)
    })?;

    register_udf(conn, "get_facts", &config, |text| {
        format!(
            "Extract facts from this text. A fact is a specific statement that can be sourced from the text.\n\n\
            Text: \"{}\"\n\n\
            Return as JSON array of objects with \"statement\", \"source_text\", and \"type\" (explicit or inferred) fields.",
            text
        )
    })?;

    register_udf(conn, "identify_groups", &config, |text| {
        format!("What are the main groups these items could be organized into? Express in plain language.\n\nItems: {}\n\nReturn as JSON array of group names.", text)
    })?;

    register_udf(conn, "classify", &config, |text| {
        format!("Classify the following text into a category. Return only the category name.\n\n{}", text)
    })?;

    register_udf2(conn, "classify_into", &config, |text, categories| {
        format!("Classify the following text into one of these categories: {}. Return only the category name.\n\n{}", categories, text)
    })?;

    register_udf(conn, "extract_json", &config, |text| {
        format!("Extract structured data from this text and return as valid JSON:\n\n{}", text)
    })?;

    register_udf(conn, "detect_language", &config, |text| {
        format!("Detect the language of this text. Return only the ISO 639-1 language code.\n\n{}", text)
    })?;

    register_udf2(conn, "answer_question", &config, |context, question| {
        format!("Based on the following context, answer the question.\n\nContext: {}\n\nQuestion: {}", context, question)
    })?;

    register_udf(conn, "generate_code", &config, |prompt| {
        format!("Generate code for the following task. Return only the code, no explanation.\n\n{}", prompt)
    })?;

    register_udf(conn, "criticize", &config, |text| {
        format!("Provide a critical analysis and constructive criticism of the following:\n{}\n\nFocus on identifying weaknesses, potential improvements, and alternative approaches.", text)
    })?;

    register_udf(conn, "synthesize", &config, |text| {
        format!("Synthesize this content into a clear, concise summary that captures the essence:\n\n{}", text)
    })?;

    register_udf(conn, "breathe", &config, |text| {
        format!(
            "Read the following conversation and identify:\n\
            1. The high level objective\n\
            2. The most recent task\n\
            3. The accomplishments thus far\n\
            4. The failures thus far\n\n\
            Return as JSON with keys: high_level_objective, most_recent_task, accomplishments, failures.\n\n{}",
            text
        )
    })?;

    register_udf(conn, "zoom_in", &config, |text| {
        format!("Look at these facts and infer new implied facts:\n\n{}\n\nReturn as JSON array of objects with \"statement\" and \"inferred_from\" fields.", text)
    })?;

    Ok(())
}

pub fn register_nql_functions(conn: &Connection) -> rusqlite::Result<()> {
    register_all_functions(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = LlmConfig::default();
        assert_eq!(config.backend, "ollama");
        assert_eq!(config.model, DEFAULT_MODEL);
    }

    #[test]
    fn test_register_functions() {
        let conn = Connection::open_in_memory().unwrap();
        register_all_functions(&conn).unwrap();
    }
}
