use nql::compiler::{Compiler, Target};
use nql::parser::parse_model_file;
use std::io::Write;
use tempfile::NamedTempFile;

#[test]
fn bigquery_get_facts_emits_ai_generate_result() {
    let sql = "SELECT nql.get_facts(bio) AS facts FROM users";
    let compiler = Compiler::new(Target::Bigquery);
    let out = compiler.compile(sql, Target::Bigquery);
    assert!(out.contains("AI.GENERATE"));
    assert!(out.contains(").result"));
    assert!(out.contains("Extract facts"));
}

#[test]
fn bigquery_summarize_emits_ai_generate() {
    let sql = "SELECT nql.summarize(bio) AS s FROM users";
    let compiler = Compiler::new(Target::Bigquery);
    let out = compiler.compile(sql, Target::Bigquery);
    assert!(out.contains("AI.GENERATE(CONCAT('Summarize: ', bio))"));
}

#[test]
fn bigquery_generate_embedding_emits_ai_generate_embedding() {
    let sql = "SELECT nql.generate_embedding(bio) AS e FROM users";
    let compiler = Compiler::new(Target::Bigquery);
    let out = compiler.compile(sql, Target::Bigquery);
    assert!(out.contains("AI.GENERATE_EMBEDDING(bio)).result"));
}

#[test]
fn incremental_materialization_emits_insert_not_exists() {
    let mut f = NamedTempFile::new().unwrap();
    write!(
        f,
        "-- config:\n--   materialized: incremental\n--   unique_key: user_id\n\nSELECT user_id, name FROM users\n"
    )
    .unwrap();
    let model = parse_model_file(f.path()).unwrap();
    let compiler = Compiler::new(Target::Bigquery);
    let out = compiler.compile_model(&model);
    assert!(out.contains("CREATE TABLE IF NOT EXISTS"));
    assert!(out.contains("INSERT INTO"));
    assert!(out.contains("NOT EXISTS"));
    assert!(out.contains("t.user_id = s.user_id"));
}

#[test]
fn ref_replaced_with_table_name() {
    let sql = "SELECT * FROM {{ ref('users') }}";
    let mut compiler = Compiler::new(Target::Bigquery);
    compiler.register_ref("users", "analytics.users");
    let out = compiler.compile(sql, Target::Bigquery);
    assert!(out.contains("FROM analytics.users"), "got: {}", out);
}
