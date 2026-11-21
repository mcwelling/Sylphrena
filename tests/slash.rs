use std::fs;
use tempfile::TempDir;

use syl_rs::config::AppConfig;
use syl_rs::slash::{handle_slash, SlashAction, SlashContext};
use syl_rs::api::types::{Message, MessageContent};

// ============================================================
// Helpers
// ============================================================

/// Returns (session_path, workflow_dir, memory_dir, log_path) — all owned.
fn make_paths(tmp: &TempDir) -> (
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let session_path = tmp.path().join("session.json");
    let workflow_dir = tmp.path().join("workflows");
    let memory_dir   = tmp.path().join("memories");
    let log_path     = tmp.path().join("syl.log");
    fs::create_dir_all(&workflow_dir).unwrap();
    fs::create_dir_all(&memory_dir).unwrap();
    (session_path, workflow_dir, memory_dir, log_path)
}

fn build_ctx<'a>(
    session_path:  &'a std::path::PathBuf,
    workflow_dir:  &'a std::path::PathBuf,
    memory_dir:    &'a std::path::PathBuf,
    log_path:      &'a std::path::PathBuf,
    model:         &'a str,
    backend:       &'a str,
    timeout_ms:    u64,
    rate_limit:    u64,
    input_tokens:  u64,
    output_tokens: u64,
    message_count: usize,
    config:        &'a AppConfig,
) -> SlashContext<'a> {
    SlashContext {
        session_path,
        workflow_dir,
        memory_dir,
        log_path,
        model,
        backend,
        timeout_ms,
        rate_limit,
        input_tokens,
        output_tokens,
        message_count,
        config,
    }
}

fn empty_messages() -> Vec<Message> {
    vec![]
}

fn some_messages() -> Vec<Message> {
    vec![Message { role: "user".to_string(), content: MessageContent::Text("hi".to_string()) }]
}

// ============================================================
// Tests
// ============================================================

#[tokio::test]
async fn slash_exit_returns_exit_action() {
    let tmp = TempDir::new().unwrap();
    let (sp, wd, md, lp) = make_paths(&tmp);
    let cfg = AppConfig::default();
    let ctx = build_ctx(&sp, &wd, &md, &lp, "claude-opus-4-6", "anthropic", 120_000, 0, 0, 0, 0, &cfg);
    let result = handle_slash("/exit", empty_messages(), &ctx).await.unwrap();
    assert_eq!(result.action, SlashAction::Exit);
}

#[tokio::test]
async fn slash_quit_also_exits() {
    let tmp = TempDir::new().unwrap();
    let (sp, wd, md, lp) = make_paths(&tmp);
    let cfg = AppConfig::default();
    let ctx = build_ctx(&sp, &wd, &md, &lp, "claude-opus-4-6", "anthropic", 120_000, 0, 0, 0, 0, &cfg);
    let result = handle_slash("/quit", empty_messages(), &ctx).await.unwrap();
    assert_eq!(result.action, SlashAction::Exit);
}

#[tokio::test]
async fn slash_clear_empties_messages() {
    let tmp = TempDir::new().unwrap();
    let (sp, wd, md, lp) = make_paths(&tmp);
    let cfg = AppConfig::default();
    let ctx = build_ctx(&sp, &wd, &md, &lp, "claude-opus-4-6", "anthropic", 120_000, 0, 0, 0, 1, &cfg);
    let result = handle_slash("/clear", some_messages(), &ctx).await.unwrap();
    assert!(result.messages.is_empty());
    assert_eq!(result.action, SlashAction::Continue);
}

#[tokio::test]
async fn slash_help_has_output() {
    let tmp = TempDir::new().unwrap();
    let (sp, wd, md, lp) = make_paths(&tmp);
    let cfg = AppConfig::default();
    let ctx = build_ctx(&sp, &wd, &md, &lp, "claude-opus-4-6", "anthropic", 120_000, 0, 0, 0, 0, &cfg);
    let result = handle_slash("/help", empty_messages(), &ctx).await.unwrap();
    assert!(result.output.is_some());
    let text = result.output.unwrap();
    assert!(text.contains("/status"), "help should list /status");
    assert!(text.contains("/exit"),   "help should list /exit");
}

#[tokio::test]
async fn slash_status_output_contains_model() {
    let tmp = TempDir::new().unwrap();
    let (sp, wd, md, lp) = make_paths(&tmp);
    let cfg = AppConfig::default();
    let ctx = build_ctx(&sp, &wd, &md, &lp, "claude-opus-4-6", "anthropic", 120_000, 60_000, 100, 50, 4, &cfg);
    let result = handle_slash("/status", some_messages(), &ctx).await.unwrap();
    let text = result.output.unwrap();
    assert!(text.contains("claude-opus-4-6"), "status should show model");
    assert!(text.contains("anthropic"),       "status should show backend");
    assert!(text.contains("120"),             "status should show timeout");
    assert!(text.contains("60000 tpm"),       "status should show rate limit");
    assert!(text.contains("in:100"),          "status should show input tokens");
    assert!(text.contains("out:50"),          "status should show output tokens");
}

#[tokio::test]
async fn slash_status_unlimited_rate() {
    let tmp = TempDir::new().unwrap();
    let (sp, wd, md, lp) = make_paths(&tmp);
    let cfg = AppConfig::default();
    let ctx = build_ctx(&sp, &wd, &md, &lp, "claude-opus-4-6", "anthropic", 60_000, 0, 0, 0, 0, &cfg);
    let result = handle_slash("/status", empty_messages(), &ctx).await.unwrap();
    let text = result.output.unwrap();
    assert!(text.contains("unlimited"));
}

#[tokio::test]
async fn slash_save_writes_file() {
    let tmp = TempDir::new().unwrap();
    let (session_path, wd, md, lp) = make_paths(&tmp);
    let cfg = AppConfig::default();
    let ctx = build_ctx(&session_path, &wd, &md, &lp, "claude-opus-4-6", "anthropic", 120_000, 0, 0, 0, 1, &cfg);
    let result = handle_slash("/save", some_messages(), &ctx).await.unwrap();
    assert!(result.output.is_some());
    let text = result.output.unwrap().to_lowercase();
    assert!(text.contains("saved") || text.contains("session"), "save should confirm success");
    // File must exist with non-trivial content
    let data = fs::read(&session_path).expect("session file should exist after /save");
    assert!(data.len() > 2, "session file should have content");
}

#[tokio::test]
async fn slash_save_output_confirms() {
    let tmp = TempDir::new().unwrap();
    let (sp, wd, md, lp) = make_paths(&tmp);
    let cfg = AppConfig::default();
    let ctx = build_ctx(&sp, &wd, &md, &lp, "claude-opus-4-6", "anthropic", 120_000, 0, 0, 0, 0, &cfg);
    let result = handle_slash("/save", empty_messages(), &ctx).await.unwrap();
    let text = result.output.unwrap().to_lowercase();
    assert!(text.contains("saved") || text.contains("session"));
}

#[tokio::test]
async fn slash_memory_empty_dir() {
    let tmp = TempDir::new().unwrap();
    let (sp, wd, md, lp) = make_paths(&tmp);
    let cfg = AppConfig::default();
    let ctx = build_ctx(&sp, &wd, &md, &lp, "claude-opus-4-6", "anthropic", 120_000, 0, 0, 0, 0, &cfg);
    let result = handle_slash("/memory", empty_messages(), &ctx).await.unwrap();
    assert_eq!(result.output.as_deref(), Some("No memories."));
}

#[tokio::test]
async fn slash_memory_lists_keys() {
    let tmp = TempDir::new().unwrap();
    let (sp, wd, memory_dir, lp) = make_paths(&tmp);
    let cfg = AppConfig::default();
    let ctx = build_ctx(&sp, &wd, &memory_dir, &lp, "claude-opus-4-6", "anthropic", 120_000, 0, 0, 0, 0, &cfg);
    fs::write(memory_dir.join("alpha.md"), "content a").unwrap();
    fs::write(memory_dir.join("beta.md"),  "content b").unwrap();
    let result = handle_slash("/memory", empty_messages(), &ctx).await.unwrap();
    let text = result.output.unwrap();
    assert!(text.contains("alpha"), "should list alpha");
    assert!(text.contains("beta"),  "should list beta");
}

#[tokio::test]
async fn slash_memory_named_key() {
    let tmp = TempDir::new().unwrap();
    let (sp, wd, memory_dir, lp) = make_paths(&tmp);
    let cfg = AppConfig::default();
    let ctx = build_ctx(&sp, &wd, &memory_dir, &lp, "claude-opus-4-6", "anthropic", 120_000, 0, 0, 0, 0, &cfg);
    fs::write(memory_dir.join("mykey.md"), "the memory contents").unwrap();
    let result = handle_slash("/memory mykey", empty_messages(), &ctx).await.unwrap();
    let text = result.output.unwrap();
    assert!(text.contains("the memory contents"));
}

#[tokio::test]
async fn slash_memory_missing_key() {
    let tmp = TempDir::new().unwrap();
    let (sp, wd, md, lp) = make_paths(&tmp);
    let cfg = AppConfig::default();
    let ctx = build_ctx(&sp, &wd, &md, &lp, "claude-opus-4-6", "anthropic", 120_000, 0, 0, 0, 0, &cfg);
    let result = handle_slash("/memory nonexistent", empty_messages(), &ctx).await.unwrap();
    let text = result.output.unwrap().to_lowercase();
    assert!(text.contains("not found"));
}

#[tokio::test]
async fn slash_workflow_empty_dir() {
    let tmp = TempDir::new().unwrap();
    let (sp, wd, md, lp) = make_paths(&tmp);
    let cfg = AppConfig::default();
    let ctx = build_ctx(&sp, &wd, &md, &lp, "claude-opus-4-6", "anthropic", 120_000, 0, 0, 0, 0, &cfg);
    let result = handle_slash("/workflow", empty_messages(), &ctx).await.unwrap();
    assert_eq!(result.output.as_deref(), Some("No workflows."));
}

#[tokio::test]
async fn slash_log_missing_file() {
    let tmp = TempDir::new().unwrap();
    let (sp, wd, md, lp) = make_paths(&tmp);
    let cfg = AppConfig::default();
    let ctx = build_ctx(&sp, &wd, &md, &lp, "claude-opus-4-6", "anthropic", 120_000, 0, 0, 0, 0, &cfg);
    let result = handle_slash("/log", empty_messages(), &ctx).await.unwrap();
    assert_eq!(result.output.as_deref(), Some("No log entries yet."));
}

#[tokio::test]
async fn slash_log_reads_last_lines() {
    let tmp = TempDir::new().unwrap();
    let (sp, wd, md, log_path) = make_paths(&tmp);
    let cfg = AppConfig::default();
    let ctx = build_ctx(&sp, &wd, &md, &log_path, "claude-opus-4-6", "anthropic", 120_000, 0, 0, 0, 0, &cfg);
    let lines: Vec<String> = (1..=25).map(|i| format!("line {}", i)).collect();
    fs::write(&log_path, lines.join("\n")).unwrap();
    let result = handle_slash("/log", empty_messages(), &ctx).await.unwrap();
    let text = result.output.unwrap();
    // Should contain line 25 (last) but not line 1 (before last-20 window)
    assert!(text.contains("line 25"), "should include last line");
    assert!(!text.contains("line 1\r") && !text.contains("line 1\n") && !text.ends_with("line 1"),
            "should not include line 1");
}

#[tokio::test]
async fn slash_unknown_returns_none() {
    let tmp = TempDir::new().unwrap();
    let (sp, wd, md, lp) = make_paths(&tmp);
    let cfg = AppConfig::default();
    let ctx = build_ctx(&sp, &wd, &md, &lp, "claude-opus-4-6", "anthropic", 120_000, 0, 0, 0, 0, &cfg);
    let result = handle_slash("/notacommand", empty_messages(), &ctx).await;
    assert!(result.is_none());
}

#[tokio::test]
async fn slash_command_tolerates_surrounding_whitespace() {
    let tmp = TempDir::new().unwrap();
    let (sp, wd, md, lp) = make_paths(&tmp);
    let cfg = AppConfig::default();
    let ctx = build_ctx(&sp, &wd, &md, &lp, "claude-opus-4-6", "anthropic", 120_000, 0, 0, 0, 0, &cfg);
    let result = handle_slash("  /exit  ", empty_messages(), &ctx).await.unwrap();
    assert_eq!(result.action, SlashAction::Exit);
}
