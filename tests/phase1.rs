/// Phase 1 test suite — ported from test_syl.bb
///
/// Covers: CLI args, truncate_output, find_valid_start, build_compacted_messages,
/// memory I/O, session persistence, SSE parsing, workflow CRUD, slash commands,
/// rate limit tracking, system prompt, Bedrock signing/parsing, stream handler,
/// cache breakpoints, fmt_num, workspace context.
use syl_rs::api::cache::attach_cache_breakpoints;
use syl_rs::api::compaction::{build_compacted_messages, find_valid_start};
use syl_rs::api::rate_limit::{prune_old_calls, record_rate_call, RateCall, SessionTokens};
use syl_rs::api::sse::StreamHandler;
use syl_rs::api::types::{ContentBlock, Message, MessageContent, StopReason};
use syl_rs::bedrock::{
    bedrock_model_id, hmac_sha256, parse_bedrock_stream, parse_eventstream_headers, sha256_hex,
    sign_bedrock_request,
};
use syl_rs::config::{parse_cli_args, AppConfig};
use syl_rs::session::{load_session, save_session};
use syl_rs::slash::{handle_slash, SlashAction, SlashContext};
use syl_rs::system_prompt::build_system_blocks;
use syl_rs::tools::memory::{load_all_memories, memory_read, memory_write};
use syl_rs::tools::workflow::{
    list_workflows, load_workflow, parse_workflow, workflow_delete, workflow_write,
};
use syl_rs::util::{fmt_num, truncate_output, MAX_TOOL_OUTPUT};
use syl_rs::workspace::gather_workspace_context;

// ============================================================
// 1. CLI Argument Parsing
// ============================================================

#[test]
fn test_parse_cli_args_empty() {
    let a = parse_cli_args(&[]);
    assert_eq!(a.model, None);
    assert!(!a.resume);
}

#[test]
fn test_parse_cli_args_model() {
    let a = parse_cli_args(&["--model", "claude-opus-4-6"]);
    assert_eq!(a.model.as_deref(), Some("claude-opus-4-6"));
}

#[test]
fn test_parse_cli_args_timeout() {
    let a = parse_cli_args(&["--timeout", "120"]);
    assert_eq!(a.timeout, Some(120));
}

#[test]
fn test_parse_cli_args_thinking() {
    let a = parse_cli_args(&["--thinking", "4000"]);
    assert_eq!(a.thinking, Some(4000));
}

#[test]
fn test_parse_cli_args_boolean_flags() {
    let a = parse_cli_args(&["--resume", "--no-banner"]);
    assert!(a.resume);
    assert!(a.no_banner);
}

#[test]
fn test_parse_cli_args_help() {
    let a = parse_cli_args(&["--help"]);
    assert!(a.help);
}

#[test]
fn test_parse_cli_args_combined() {
    let a = parse_cli_args(&["--model", "claude-haiku-4-5", "--timeout", "30", "--resume"]);
    assert_eq!(a.model.as_deref(), Some("claude-haiku-4-5"));
    assert_eq!(a.timeout, Some(30));
    assert!(a.resume);
}

#[test]
fn test_parse_cli_args_unknown_flags_ignored() {
    let a = parse_cli_args(&["--unknown", "--model", "test", "--bogus"]);
    assert_eq!(a.model.as_deref(), Some("test"));
}

#[test]
fn test_parse_cli_args_backend() {
    let a = parse_cli_args(&["--backend", "bedrock"]);
    assert_eq!(a.backend.as_deref(), Some("bedrock"));

    let a = parse_cli_args(&["--backend", "bedrock", "--model", "claude-opus-4-6"]);
    assert_eq!(a.backend.as_deref(), Some("bedrock"));
    assert_eq!(a.model.as_deref(), Some("claude-opus-4-6"));
}

#[test]
fn test_parse_cli_args_rate_limit() {
    let a = parse_cli_args(&["--rate-limit", "80000"]);
    assert_eq!(a.rate_limit, Some(80000));

    let a = parse_cli_args(&["--model", "claude-sonnet-4-6", "--rate-limit", "50000"]);
    assert_eq!(a.model.as_deref(), Some("claude-sonnet-4-6"));
    assert_eq!(a.rate_limit, Some(50000));
}

// ============================================================
// 2. truncate_output
// ============================================================

#[test]
fn test_truncate_output_short_string() {
    assert_eq!(truncate_output("hello"), "hello");
}

#[test]
fn test_truncate_output_at_limit() {
    let s: String = "x".repeat(MAX_TOOL_OUTPUT);
    assert_eq!(truncate_output(&s), s);
}

#[test]
fn test_truncate_output_over_limit() {
    let s: String = "x".repeat(MAX_TOOL_OUTPUT + 100);
    let result = truncate_output(&s);
    assert!(result.starts_with(&s[..MAX_TOOL_OUTPUT]));
    assert!(result.contains(&format!("{} total chars", s.len())));
    assert!(result.len() < s.len());
}

#[test]
fn test_truncate_output_empty() {
    assert_eq!(truncate_output(""), "");
}

// ============================================================
// 3. find_valid_start
// ============================================================

fn user_text(text: &str) -> Message {
    Message::user_text(text)
}

fn asst_text(text: &str) -> Message {
    Message::assistant_blocks(vec![ContentBlock::Text {
        text: text.to_string(),
        cache_control: None,
    }])
}

fn tool_result_msg(tool_use_id: &str) -> Message {
    Message {
        role: "user".to_string(),
        content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
            tool_use_id: tool_use_id.to_string(),
            content: "ok".to_string(),
            is_error: None,
        }]),
    }
}

fn asst_tool_use(id: &str) -> Message {
    Message {
        role: "assistant".to_string(),
        content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
            id: id.to_string(),
            name: "run_bash".to_string(),
            input: serde_json::json!({"command": "ls"}),
        }]),
    }
}

#[test]
fn test_find_valid_start_already_valid() {
    let msgs = vec![user_text("hello"), asst_text("hi")];
    assert_eq!(find_valid_start(msgs.clone()), msgs);
}

#[test]
fn test_find_valid_start_skips_tool_result() {
    let tool = tool_result_msg("t1");
    let user = user_text("hello");
    let asst = asst_text("hi");
    let result = find_valid_start(vec![tool, user.clone(), asst.clone()]);
    assert_eq!(result, vec![user, asst]);
}

#[test]
fn test_find_valid_start_skips_assistant_then_tool_result() {
    let asst = asst_tool_use("t1");
    let tool = tool_result_msg("t1");
    let user = user_text("now what?");
    let result = find_valid_start(vec![asst, tool, user.clone()]);
    assert_eq!(result, vec![user]);
}

#[test]
fn test_find_valid_start_all_tool_exchanges() {
    let asst = asst_tool_use("t1");
    let tool = tool_result_msg("t1");
    assert_eq!(
        find_valid_start(vec![asst.clone(), tool.clone(), asst, tool]),
        vec![]
    );
}

#[test]
fn test_find_valid_start_empty() {
    assert_eq!(find_valid_start(vec![]), vec![]);
}

// ============================================================
// 4. build_compacted_messages
// ============================================================

#[test]
fn test_build_compacted_messages_with_summary() {
    let keep = vec![user_text("what next?")];
    let result = build_compacted_messages(Some("User asked about X. Syl did Y."), keep);

    assert_eq!(result.len(), 3);
    assert_eq!(result[0].role, "user");
    // first message content includes "Context summary"
    if let MessageContent::Text(ref t) = result[0].content {
        assert!(t.contains("Context summary"));
    } else {
        panic!("expected Text content in first message");
    }
    assert_eq!(result[1].role, "assistant");
    if let MessageContent::Text(ref t) = result[2].content {
        assert_eq!(t, "what next?");
    } else {
        panic!("expected Text content in last message");
    }
}

#[test]
fn test_build_compacted_messages_nil_summary() {
    let keep = vec![user_text("still here")];
    let result = build_compacted_messages(None, keep.clone());
    assert_eq!(result, keep);
}

// ============================================================
// 5. Memory (file I/O with temp directory)
// ============================================================

#[test]
fn test_memory_write_and_read() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    let r = memory_write(dir, "test-topic", "Some notes here");
    assert!(r.contains("Remembered"));
    assert!(r.contains("test-topic"));

    assert_eq!(memory_read(dir, "test-topic"), "Some notes here");
    assert!(memory_read(dir, "nope").contains("No memory found"));

    memory_write(dir, "another", "More notes");
    let all = memory_read(dir, "");
    assert!(all.contains("test-topic"));
    assert!(all.contains("another"));
    assert!(all.contains("Some notes here"));
    assert!(all.contains("More notes"));

    let all2 = load_all_memories(dir).unwrap();
    assert!(all2.contains("test-topic"));
    assert!(all2.contains("another"));
}

#[test]
fn test_memory_read_empty_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    assert_eq!(memory_read(dir, ""), "No memories stored yet.");
    assert!(load_all_memories(dir).is_none());
}

// ============================================================
// 6. Session Persistence
// ============================================================

#[test]
fn test_session_save_and_load() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("session.json");

    let msgs = vec![user_text("hello"), asst_text("hi there")];

    save_session(&path, &msgs).unwrap();
    assert!(path.exists());

    let loaded = load_session(&path).unwrap();
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0].role, "user");
    if let MessageContent::Text(ref t) = loaded[0].content {
        assert_eq!(t, "hello");
    } else {
        panic!("expected text");
    }
}

#[test]
fn test_load_session_missing_file() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("nonexistent.json");
    assert!(load_session(&path).is_none());
}

// ============================================================
// 8. Workflow Parsing
// ============================================================

#[test]
fn test_parse_workflow_basic() {
    let content = "# Ship\nTest and push.\n\n- Run tests: `bb test`\n- Push: `git push`\n";
    let wf = parse_workflow(content).unwrap();
    assert_eq!(wf.name, "Ship");
    assert_eq!(wf.description.as_deref(), Some("Test and push."));
    assert_eq!(wf.steps.len(), 2);
    assert_eq!(wf.steps[0].label, "Run tests");
    assert_eq!(wf.steps[0].command, "bb test");
    assert_eq!(wf.steps[1].label, "Push");
    assert_eq!(wf.steps[1].command, "git push");
}

#[test]
fn test_parse_workflow_no_heading() {
    let content = "- Step one: `echo hello`\n- Step two: `echo world`\n";
    let wf = parse_workflow(content).unwrap();
    assert_eq!(wf.name, "unnamed");
    assert_eq!(wf.steps.len(), 2);
}

#[test]
fn test_parse_workflow_no_steps() {
    let content = "# Empty\nJust a heading, no steps.\n";
    assert!(parse_workflow(content).is_none());
}

#[test]
fn test_parse_workflow_asterisk_bullets() {
    let content = "# Deploy\n* Build: `npm run build`\n* Deploy: `npm run deploy`\n";
    let wf = parse_workflow(content).unwrap();
    assert_eq!(wf.steps.len(), 2);
    assert_eq!(wf.steps[0].command, "npm run build");
}

#[test]
fn test_list_workflows_empty() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(list_workflows(tmp.path()).is_empty());
}

#[test]
fn test_list_and_load_workflows() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    std::fs::write(
        dir.join("ship.md"),
        "# Ship\nPush code.\n\n- Test: `bb test`\n- Push: `git push`\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("check.md"),
        "# Check\nRun checks.\n\n- Lint: `bb lint`\n",
    )
    .unwrap();

    let wfs = list_workflows(dir);
    assert_eq!(wfs.len(), 2);
    assert_eq!(wfs[0].key, "check");
    assert_eq!(wfs[1].key, "ship");

    let wf = load_workflow(dir, "ship").unwrap();
    assert_eq!(wf.name, "Ship");
    assert_eq!(wf.steps.len(), 2);

    assert!(load_workflow(dir, "nope").is_none());
}

#[test]
fn test_workflow_write_and_delete() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let content =
        "# Deploy\nDeploy to prod.\n\n- Build: `npm run build`\n- Deploy: `npm run deploy`\n";

    let r = workflow_write(dir, "deploy", content);
    assert!(r.contains("created"), "expected 'created' in: {}", r);
    assert!(r.contains("2 steps"));
    assert!(dir.join("deploy.md").exists());

    let r = workflow_write(dir, "deploy", content);
    assert!(r.starts_with("Error"));
    assert!(r.contains("already exists"));

    let r = workflow_write(dir, "bad", "# Bad\nNo steps here.\n");
    assert!(r.starts_with("Error"));
    assert!(r.contains("Invalid workflow format"));

    let r = workflow_delete(dir, "deploy");
    assert!(r.contains("deleted"));
    assert!(!dir.join("deploy.md").exists());

    let r = workflow_delete(dir, "deploy");
    assert!(r.starts_with("Error"));
    assert!(r.contains("not found"));

    let r = workflow_write(dir, "deploy", content);
    assert!(r.contains("created"));
}

// ============================================================
// 9. Slash Commands
// ============================================================

// Helper: builds temp paths and returns them (the caller owns them)
fn tmp_ctx_paths(
    tmp: &tempfile::TempDir,
) -> (
    std::path::PathBuf, // session_path
    std::path::PathBuf, // workflow_dir
    std::path::PathBuf, // memory_dir
    std::path::PathBuf, // log_path
) {
    let session_path = tmp.path().join("session.json");
    let workflow_dir = tmp.path().join("workflows");
    let memory_dir = tmp.path().join("memory");
    let log_path = tmp.path().join("syl.log");
    std::fs::create_dir_all(&workflow_dir).ok();
    std::fs::create_dir_all(&memory_dir).ok();
    (session_path, workflow_dir, memory_dir, log_path)
}

fn make_slash_ctx<'a>(
    sp: &'a std::path::PathBuf,
    wd: &'a std::path::PathBuf,
    md: &'a std::path::PathBuf,
    lp: &'a std::path::PathBuf,
    cfg: &'a AppConfig,
) -> SlashContext<'a> {
    SlashContext {
        session_path: sp,
        workflow_dir: wd,
        memory_dir: md,
        log_path: lp,
        model: "claude-opus-4-6",
        backend: "anthropic",
        timeout_ms: 120_000,
        rate_limit: 0,
        input_tokens: 0,
        output_tokens: 0,
        message_count: 0,
        config: cfg,
    }
}

#[tokio::test]
async fn test_slash_help() {
    let tmp = tempfile::tempdir().unwrap();
    let (sp, wd, md, lp) = tmp_ctx_paths(&tmp);
    let cfg = AppConfig::default();
    let ctx = make_slash_ctx(&sp, &wd, &md, &lp, &cfg);
    let msgs = vec![user_text("hi")];
    let r = handle_slash("/help", msgs.clone(), &ctx).await.unwrap();
    assert_eq!(r.action, SlashAction::Continue);
    assert_eq!(r.messages, msgs);
}

#[tokio::test]
async fn test_slash_clear() {
    let tmp = tempfile::tempdir().unwrap();
    let (sp, wd, md, lp) = tmp_ctx_paths(&tmp);
    let cfg = AppConfig::default();
    let ctx = make_slash_ctx(&sp, &wd, &md, &lp, &cfg);
    let msgs = vec![user_text("hi")];
    let r = handle_slash("/clear", msgs, &ctx).await.unwrap();
    assert_eq!(r.action, SlashAction::Continue);
    assert!(r.messages.is_empty());
}

#[tokio::test]
async fn test_slash_status() {
    let tmp = tempfile::tempdir().unwrap();
    let (sp, wd, md, lp) = tmp_ctx_paths(&tmp);
    let cfg = AppConfig::default();
    let ctx = make_slash_ctx(&sp, &wd, &md, &lp, &cfg);
    let msgs = vec![user_text("hi")];
    let r = handle_slash("/status", msgs.clone(), &ctx).await.unwrap();
    assert_eq!(r.action, SlashAction::Continue);
    assert_eq!(r.messages, msgs);
}

#[tokio::test]
async fn test_slash_save() {
    let tmp = tempfile::tempdir().unwrap();
    let (sp, wd, md, lp) = tmp_ctx_paths(&tmp);
    let cfg = AppConfig::default();
    let ctx = make_slash_ctx(&sp, &wd, &md, &lp, &cfg);
    let msgs = vec![user_text("test")];
    let r = handle_slash("/save", msgs.clone(), &ctx).await.unwrap();
    assert_eq!(r.action, SlashAction::Continue);
    assert_eq!(r.messages, msgs);
    assert!(sp.exists());
}

#[tokio::test]
async fn test_slash_exit() {
    let tmp = tempfile::tempdir().unwrap();
    let (sp, wd, md, lp) = tmp_ctx_paths(&tmp);
    let cfg = AppConfig::default();
    let ctx = make_slash_ctx(&sp, &wd, &md, &lp, &cfg);
    let r = handle_slash("/exit", vec![], &ctx).await.unwrap();
    assert_eq!(r.action, SlashAction::Exit);
}

#[tokio::test]
async fn test_slash_quit() {
    let tmp = tempfile::tempdir().unwrap();
    let (sp, wd, md, lp) = tmp_ctx_paths(&tmp);
    let cfg = AppConfig::default();
    let ctx = make_slash_ctx(&sp, &wd, &md, &lp, &cfg);
    let r = handle_slash("/quit", vec![], &ctx).await.unwrap();
    assert_eq!(r.action, SlashAction::Exit);
}

#[tokio::test]
async fn test_slash_unknown_returns_none() {
    let tmp = tempfile::tempdir().unwrap();
    let (sp, wd, md, lp) = tmp_ctx_paths(&tmp);
    let cfg = AppConfig::default();
    let ctx = make_slash_ctx(&sp, &wd, &md, &lp, &cfg);
    assert!(handle_slash("hello world", vec![], &ctx).await.is_none());
    assert!(handle_slash("/unknown", vec![], &ctx).await.is_none());
}

#[tokio::test]
async fn test_slash_with_whitespace() {
    let tmp = tempfile::tempdir().unwrap();
    let (sp, wd, md, lp) = tmp_ctx_paths(&tmp);
    let cfg = AppConfig::default();
    let ctx = make_slash_ctx(&sp, &wd, &md, &lp, &cfg);
    let r = handle_slash("  /help  ", vec![], &ctx).await.unwrap();
    assert_eq!(r.action, SlashAction::Continue);
}

#[tokio::test]
async fn test_slash_workflow_list() {
    let tmp = tempfile::tempdir().unwrap();
    let (sp, wd, md, lp) = tmp_ctx_paths(&tmp);
    let cfg = AppConfig::default();
    let ctx = make_slash_ctx(&sp, &wd, &md, &lp, &cfg);
    let msgs = vec![user_text("hi")];
    let r = handle_slash("/workflow", msgs.clone(), &ctx).await.unwrap();
    assert_eq!(r.action, SlashAction::Continue);
}

#[tokio::test]
async fn test_slash_workflow_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let (sp, wd, md, lp) = tmp_ctx_paths(&tmp);
    let cfg = AppConfig::default();
    let ctx = make_slash_ctx(&sp, &wd, &md, &lp, &cfg);
    let r = handle_slash("/workflow nonexistent", vec![], &ctx)
        .await
        .unwrap();
    assert_eq!(r.action, SlashAction::Continue);
}

// ============================================================
// 10. Rate Limit Budgeting
// ============================================================

#[test]
fn test_record_usage() {
    let mut tokens = SessionTokens::default();
    tokens.record_usage(100, 50);
    assert_eq!(tokens.input, 100);
    assert_eq!(tokens.output, 50);
    assert_eq!(tokens.calls, 1);

    tokens.record_usage(200, 30);
    assert_eq!(tokens.input, 300);
    assert_eq!(tokens.output, 80);
    assert_eq!(tokens.calls, 2);
}

#[test]
fn test_rate_call_tracking() {
    let mut calls: Vec<RateCall> = vec![];

    record_rate_call(&mut calls, 1000);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].tokens, 1000);

    record_rate_call(&mut calls, 2000);
    assert_eq!(calls.len(), 2);

    // Inject an old call (ts=0) and prune
    calls.push(RateCall { ts: 0, tokens: 500 });
    let _recent_tokens = 2000u64;
    prune_old_calls(&mut calls);
    // The ts=0 entry should be removed; the two recent ones should remain
    assert!(calls.len() >= 2);
    assert!(calls.iter().all(|c| c.ts > 0));
}

// ============================================================
// 11. System Prompt Construction
// ============================================================

#[test]
fn test_build_system_blocks_structure() {
    let tmp = tempfile::tempdir().unwrap();
    let blocks = build_system_blocks("### WORKSPACE\nFiles: syl.bb", tmp.path());
    assert_eq!(blocks.len(), 1);
    if let ContentBlock::Text {
        text,
        cache_control,
    } = &blocks[0]
    {
        assert!(
            text.contains("Syl") || text.contains("WORKSPACE"),
            "text: {}",
            &text[..text.len().min(200)]
        );
        assert!(text.contains("WORKSPACE"));
        assert_eq!(cache_control.as_ref().unwrap().kind, "ephemeral");
    } else {
        panic!("expected Text block");
    }
}

#[test]
fn test_build_system_blocks_includes_memories() {
    let tmp = tempfile::tempdir().unwrap();
    memory_write(tmp.path(), "test-key", "Test memory content");
    let blocks = build_system_blocks("workspace ctx", tmp.path());
    if let ContentBlock::Text { text, .. } = &blocks[0] {
        assert!(
            text.contains("PERSISTENT MEMORIES"),
            "text missing PERSISTENT MEMORIES: {}",
            &text[..200.min(text.len())]
        );
        assert!(text.contains("Test memory content"));
    } else {
        panic!("expected Text block");
    }
}

// ============================================================
// 12. Bedrock Backend
// ============================================================

#[test]
fn test_bedrock_model_id_known() {
    // Use `contains` rather than exact equality so these tests survive
    // routine model-ID updates (new date suffixes, version bumps) without
    // needing a code change.  The exact IDs live in CLAUDE_MODELS in config.rs.
    assert!(bedrock_model_id("claude-sonnet-4-6").contains("anthropic.claude-sonnet"));
    assert!(bedrock_model_id("claude-haiku-4-5").contains("anthropic.claude-haiku"));
    assert!(bedrock_model_id("claude-opus-4-6").contains("anthropic.claude-opus"));
}

#[test]
fn test_bedrock_model_id_passthrough() {
    // Raw Bedrock IDs and unknown names pass through unchanged.
    let raw = "anthropic.claude-sonnet-4-6-20250514-v1:0";
    assert_eq!(bedrock_model_id(raw), raw);
    assert_eq!(bedrock_model_id("some-custom-model"), "some-custom-model");
    // Future model not yet in catalog — should pass through as-is.
    assert_eq!(bedrock_model_id("claude-opus-4-7"), "claude-opus-4-7");
}

#[test]
fn test_sha256_hex() {
    assert_eq!(
        sha256_hex(""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

#[test]
fn test_hmac_sha256() {
    let result = hmac_sha256(b"key", "data");
    assert_eq!(result.len(), 32);
}

#[test]
fn test_sign_bedrock_request() {
    let headers = sign_bedrock_request(
        "POST",
        "/model/test/invoke",
        "{\"test\":true}",
        "us-east-1",
        "AKIAIOSFODNN7EXAMPLE",
        "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        None,
    );
    let auth = headers.get("authorization").unwrap();
    assert!(auth.starts_with("AWS4-HMAC-SHA256"));
    assert!(headers.contains_key("x-amz-date"));
    assert!(headers.contains_key("x-amz-content-sha256"));
    let host = headers.get("host").unwrap();
    assert!(host.contains("bedrock-runtime"));
}

#[test]
fn test_sign_bedrock_request_with_session_token() {
    let headers = sign_bedrock_request(
        "POST",
        "/model/test/invoke",
        "{}",
        "us-west-2",
        "AKID",
        "SECRET",
        Some("SESSION123"),
    );
    assert_eq!(
        headers.get("x-amz-security-token").map(|s| s.as_str()),
        Some("SESSION123")
    );
    assert!(headers
        .get("authorization")
        .unwrap()
        .contains("x-amz-security-token"));
}

#[test]
fn test_parse_eventstream_headers_string() {
    // Build: name=":event-type" (type 7), value="chunk"
    let name_bytes = b":event-type";
    let val_bytes = b"chunk";
    let mut buf = vec![];
    buf.push(name_bytes.len() as u8);
    buf.extend_from_slice(name_bytes);
    buf.push(7u8); // string type
    let val_len = val_bytes.len() as u16;
    buf.push((val_len >> 8) as u8);
    buf.push((val_len & 0xFF) as u8);
    buf.extend_from_slice(val_bytes);

    let headers = parse_eventstream_headers(&buf);
    assert_eq!(
        headers.get(":event-type").map(|s| s.as_str()),
        Some("chunk")
    );
}

#[test]
fn test_parse_eventstream_headers_empty() {
    let headers = parse_eventstream_headers(&[]);
    assert!(headers.is_empty());
}

#[test]
fn test_parse_bedrock_stream() {
    let anthropic_event = serde_json::json!({
        "type": "message_start",
        "message": {"usage": {"input_tokens": 100}}
    });
    let event_json = anthropic_event.to_string();
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(event_json.as_bytes());
    let payload_json = serde_json::json!({"bytes": b64}).to_string();
    let payload_bytes = payload_json.as_bytes();

    // Build EventStream headers
    let mut hdr = vec![];
    // :message-type = "event"
    let n1 = b":message-type";
    let v1 = b"event";
    hdr.push(n1.len() as u8);
    hdr.extend_from_slice(n1);
    hdr.push(7u8);
    hdr.push(0u8);
    hdr.push(v1.len() as u8);
    hdr.extend_from_slice(v1);
    // :event-type = "chunk"
    let n2 = b":event-type";
    let v2 = b"chunk";
    hdr.push(n2.len() as u8);
    hdr.extend_from_slice(n2);
    hdr.push(7u8);
    hdr.push(0u8);
    hdr.push(v2.len() as u8);
    hdr.extend_from_slice(v2);

    let headers_len = hdr.len() as u32;
    let total_len = 16 + headers_len + payload_bytes.len() as u32;

    let mut frame = vec![];
    frame.extend_from_slice(&total_len.to_be_bytes());
    frame.extend_from_slice(&headers_len.to_be_bytes());
    frame.extend_from_slice(&0u32.to_be_bytes()); // prelude CRC
    frame.extend_from_slice(&hdr);
    frame.extend_from_slice(payload_bytes);
    frame.extend_from_slice(&0u32.to_be_bytes()); // message CRC

    let mut events = vec![];
    parse_bedrock_stream(std::io::Cursor::new(frame), |ev| events.push(ev));

    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["type"], "message_start");
    assert_eq!(events[0]["message"]["usage"]["input_tokens"], 100);
}

// ============================================================
// 12b. Stream Handler (make_stream_handler equivalent)
// ============================================================

#[test]
fn test_stream_handler_text() {
    let mut h = StreamHandler::new();

    h.on_event(&serde_json::json!({"type": "message_start", "message": {"usage": {"input_tokens": 50, "output_tokens": 0}}}));
    h.on_event(&serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}));
    h.on_event(&serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hello"}}));
    h.on_event(&serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": " world"}}));
    h.on_event(&serde_json::json!({"type": "content_block_stop", "index": 0}));
    h.on_event(&serde_json::json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 10}}));

    let result = h.build_result();
    assert_eq!(result.stop_reason, StopReason::EndTurn);
    assert_eq!(result.content.len(), 1);
    match &result.content[0] {
        ContentBlock::Text { text, .. } => assert_eq!(text, "Hello world"),
        other => panic!("unexpected block: {:?}", other),
    }
}

#[test]
fn stream_handler_parses_cache_read_tokens() {
    let mut h = StreamHandler::new();
    h.on_event(&serde_json::json!({
        "type": "message_start",
        "message": {"usage": {
            "input_tokens": 100,
            "output_tokens": 0,
            "cache_read_input_tokens": 500
        }}
    }));
    let result = h.build_result();
    assert_eq!(result.usage.cache_read_tokens, 500);
}

#[test]
fn stream_handler_parses_cache_write_tokens() {
    let mut h = StreamHandler::new();
    h.on_event(&serde_json::json!({
        "type": "message_start",
        "message": {"usage": {
            "input_tokens": 100,
            "output_tokens": 0,
            "cache_creation_input_tokens": 250
        }}
    }));
    let result = h.build_result();
    assert_eq!(result.usage.cache_write_tokens, 250);
}

#[test]
fn stream_handler_cache_tokens_default_zero() {
    let mut h = StreamHandler::new();
    h.on_event(&serde_json::json!({
        "type": "message_start",
        "message": {"usage": {"input_tokens": 10, "output_tokens": 0}}
    }));
    let result = h.build_result();
    assert_eq!(result.usage.cache_read_tokens, 0);
    assert_eq!(result.usage.cache_write_tokens, 0);
}

// ============================================================
// 13. Cache Breakpoints
// ============================================================

#[test]
fn test_attach_cache_breakpoints_no_eligible() {
    let messages = vec![
        user_text("hello"),
        Message {
            role: "assistant".to_string(),
            content: MessageContent::Text("world".to_string()),
        },
    ];
    let result = attach_cache_breakpoints(messages.clone());
    assert_eq!(result, messages);
}

#[test]
fn test_attach_cache_breakpoints_one_turn() {
    let messages = vec![user_text("hi"), asst_text("hello")];
    let result = attach_cache_breakpoints(messages);
    if let MessageContent::Blocks(ref blocks) = result[1].content {
        if let ContentBlock::Text { cache_control, .. } = &blocks[0] {
            assert_eq!(cache_control.as_ref().unwrap().kind, "ephemeral");
        } else {
            panic!("expected Text block");
        }
    } else {
        panic!("expected Blocks");
    }
}

#[test]
fn test_attach_cache_breakpoints_two_turns() {
    let messages = vec![
        user_text("first"),
        asst_text("reply 1"),
        user_text("second"),
        asst_text("reply 2"),
    ];
    let result = attach_cache_breakpoints(messages);
    for idx in [1usize, 3] {
        if let MessageContent::Blocks(ref blocks) = result[idx].content {
            if let ContentBlock::Text { cache_control, .. } = &blocks[0] {
                assert_eq!(cache_control.as_ref().unwrap().kind, "ephemeral");
            } else {
                panic!("expected Text block at idx {}", idx);
            }
        } else {
            panic!("expected Blocks at idx {}", idx);
        }
    }
}

#[test]
fn test_attach_cache_breakpoints_only_last_two() {
    let messages = vec![
        user_text("a"),
        asst_text("1"),
        user_text("b"),
        asst_text("2"),
        user_text("c"),
        asst_text("3"),
    ];
    let result = attach_cache_breakpoints(messages);
    // First assistant turn (idx 1) should NOT have cache_control
    if let MessageContent::Blocks(ref blocks) = result[1].content {
        if let ContentBlock::Text { cache_control, .. } = &blocks[0] {
            assert!(
                cache_control.is_none(),
                "idx 1 should not have cache_control"
            );
        }
    }
    // Last two (idx 3 and 5) should have it
    for idx in [3usize, 5] {
        if let MessageContent::Blocks(ref blocks) = result[idx].content {
            if let ContentBlock::Text { cache_control, .. } = &blocks[0] {
                assert_eq!(
                    cache_control.as_ref().unwrap().kind,
                    "ephemeral",
                    "idx {} missing cache_control",
                    idx
                );
            }
        }
    }
}

#[test]
fn test_attach_cache_breakpoints_skips_tool_use_blocks() {
    let messages = vec![
        user_text("run it"),
        Message {
            role: "assistant".to_string(),
            content: MessageContent::Blocks(vec![
                ContentBlock::ToolUse {
                    id: "x".to_string(),
                    name: "run_bash".to_string(),
                    input: serde_json::json!({}),
                },
                ContentBlock::Text {
                    text: "done".to_string(),
                    cache_control: None,
                },
            ]),
        },
    ];
    let result = attach_cache_breakpoints(messages);
    if let MessageContent::Blocks(ref blocks) = result[1].content {
        // tool_use block (idx 0) should NOT have cache_control
        if let ContentBlock::ToolUse { .. } = &blocks[0] { /* ok */
        } else {
            panic!("expected ToolUse");
        }
        // text block (idx 1) SHOULD have cache_control
        if let ContentBlock::Text { cache_control, .. } = &blocks[1] {
            assert_eq!(cache_control.as_ref().unwrap().kind, "ephemeral");
        } else {
            panic!("expected Text at idx 1");
        }
    }
}

// ============================================================
// 14. fmt_num
// ============================================================

#[test]
fn test_fmt_num_zero() {
    assert_eq!(fmt_num(0), "0");
}

#[test]
fn test_fmt_num_small() {
    assert_eq!(fmt_num(42), "42");
    assert_eq!(fmt_num(999), "999");
}

#[test]
fn test_fmt_num_thousands() {
    assert_eq!(fmt_num(1000), "1k");
    assert_eq!(fmt_num(1999), "1k");
    assert_eq!(fmt_num(12345), "12k");
    assert_eq!(fmt_num(999999), "999k");
}

#[test]
fn test_fmt_num_millions() {
    assert_eq!(fmt_num(1_000_000), "1M");
    assert_eq!(fmt_num(5_500_000), "5M");
}

// ============================================================
// 15. Workspace Context
// ============================================================

#[test]
fn test_gather_workspace_context_structure() {
    // Use the syl-rs directory itself — it's a git repo on the host.
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let ctx = gather_workspace_context(dir);

    assert!(
        ctx.contains("WORKSPACE STATE"),
        "missing header: {}",
        &ctx[..ctx.len().min(300)]
    );
    assert!(ctx.contains("Project:"), "missing Project: line");
    assert!(ctx.contains("Files:"), "missing Files: section");
    assert!(ctx.contains("```"), "missing code fence");
}

#[test]
fn test_gather_workspace_context_contains_files() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let ctx = gather_workspace_context(dir);
    // The manifest dir contains Cargo.toml — it should appear in the find output
    assert!(
        ctx.contains("Cargo.toml"),
        "Cargo.toml not in file listing: {}",
        &ctx[..ctx.len().min(500)]
    );
}

#[test]
fn test_gather_workspace_context_git_branch() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let ctx = gather_workspace_context(dir);
    // In a git repo, the Branch: line should be present
    assert!(
        ctx.contains("Branch:"),
        "Branch: line missing — is this a git repo? ctx: {}",
        &ctx[..ctx.len().min(300)]
    );
}

#[test]
fn test_gather_workspace_context_non_git_dir() {
    let tmp = tempfile::tempdir().unwrap();
    // Write a dummy file so find has something to list
    std::fs::write(tmp.path().join("hello.txt"), "hi").unwrap();
    let ctx = gather_workspace_context(tmp.path());

    // Should still produce the header and Files: block even without git
    assert!(ctx.contains("WORKSPACE STATE"));
    assert!(ctx.contains("Files:"));
    // No Branch: line without git
    assert!(
        !ctx.contains("Branch:"),
        "unexpected Branch: in non-git dir"
    );
}
