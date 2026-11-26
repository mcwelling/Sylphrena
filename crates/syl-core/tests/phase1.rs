/// Phase 1 test suite — ported from test_syl.bb
///
/// Covers: truncate_output, build_compacted_messages, memory I/O,
/// workflow CRUD, system prompt, stream handler, cache
/// breakpoints, fmt_num, workspace context. CLI/config, slash commands, rate
/// limiting have their own test files.
use syl_core::api::cache::messages_with_cache_points;
use syl_core::api::compaction::build_compacted_messages;
use syl_core::api::sse::StreamHandler;
use syl_core::api::types::{ContentBlock, Message, MessageContent, StopReason};
use syl_core::system_prompt::build_system_blocks;
use syl_core::tools::memory::{load_all_memories, memory_read, memory_write};
use syl_core::tools::workflow::{
    list_workflows, load_workflow, parse_workflow, workflow_delete, workflow_write,
};
use syl_core::util::{fmt_num, truncate_output, MAX_TOOL_OUTPUT};
use syl_core::workspace::gather_workspace_context;

// ============================================================
// 1. truncate_output
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
// 2. Message helpers
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

// ============================================================
// 3. build_compacted_messages
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
// 4. Memory (file I/O with temp directory)
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

// 5. Session persistence: see session_log.rs.

// ============================================================
// 6. Workflow Parsing
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
// 7. System Prompt Construction
// ============================================================

#[test]
fn test_build_system_blocks_structure() {
    let tmp = tempfile::tempdir().unwrap();
    let blocks = build_system_blocks("### WORKSPACE\nFiles: syl.bb", tmp.path(), None);
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
    let blocks = build_system_blocks("workspace ctx", tmp.path(), None);
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
// 8. Stream Handler
// ============================================================

#[test]
fn stream_handler_parses_cache_tokens() {
    let mut h = StreamHandler::new();
    h.on_event(&serde_json::json!({
        "type": "message_start",
        "message": {"usage": {
            "input_tokens": 100,
            "output_tokens": 0,
            "cache_read_input_tokens": 500,
            "cache_creation_input_tokens": 250
        }}
    }));
    let result = h.build_result();
    assert_eq!(result.usage.cache_read_tokens, 500);
    assert_eq!(result.usage.cache_write_tokens, 250);

    let mut h = StreamHandler::new();
    h.on_event(&serde_json::json!({
        "type": "message_start",
        "message": {"usage": {"input_tokens": 10, "output_tokens": 0}}
    }));
    let result = h.build_result();
    assert_eq!(result.usage.cache_read_tokens, 0);
    assert_eq!(result.usage.cache_write_tokens, 0);
}

#[test]
fn stream_handler_keeps_redacted_thinking() {
    let mut h = StreamHandler::new();
    h.on_event(&serde_json::json!({"type": "content_block_start", "index": 0,
        "content_block": {"type": "redacted_thinking", "data": "opaque"}}));
    h.on_event(&serde_json::json!({"type": "content_block_start", "index": 1,
        "content_block": {"type": "text", "text": ""}}));
    h.on_event(&serde_json::json!({"type": "content_block_delta", "index": 1,
        "delta": {"type": "text_delta", "text": "hi"}}));
    let result = h.build_result();
    assert_eq!(result.content[0], ContentBlock::RedactedThinking { data: "opaque".into() });
    // Round-trips with the API's field names.
    assert_eq!(
        serde_json::to_value(&result.content[0]).unwrap(),
        serde_json::json!({"type": "redacted_thinking", "data": "opaque"})
    );
}

#[test]
fn stream_handler_truncated_tool_input_becomes_empty_object() {
    let mut h = StreamHandler::new();
    h.on_event(&serde_json::json!({"type": "content_block_start", "index": 0,
        "content_block": {"type": "tool_use", "id": "t1", "name": "run_bash", "input": {}}}));
    h.on_event(&serde_json::json!({"type": "content_block_delta", "index": 0,
        "delta": {"type": "input_json_delta", "partial_json": "{\"command\": \"ls"}}));
    h.on_event(&serde_json::json!({"type": "message_delta",
        "delta": {"stop_reason": "max_tokens"}, "usage": {"output_tokens": 5}}));
    let result = h.build_result();
    assert_eq!(result.stop_reason, StopReason::MaxTokens);
    match &result.content[0] {
        ContentBlock::ToolUse { input, .. } => assert_eq!(input, &serde_json::json!({})),
        other => panic!("unexpected block: {:?}", other),
    }
}

#[test]
fn stream_handler_records_error_event() {
    let mut h = StreamHandler::new();
    assert!(h.stream_error().is_none());
    h.on_event(&serde_json::json!({"type": "error",
        "error": {"type": "overloaded_error", "message": "Overloaded"}}));
    let e = h.stream_error().unwrap();
    assert_eq!(e.kind, "overloaded_error");
    assert_eq!(e.message, "Overloaded");
}

// ============================================================
// 9. Cache Breakpoints
// ============================================================

/// The indexes of `(message, block)` that carry a breakpoint.
fn tagged(messages: &[Message]) -> Vec<(usize, usize)> {
    let v = messages_with_cache_points(messages);
    let mut out = Vec::new();
    for (i, m) in v.as_array().unwrap().iter().enumerate() {
        for (j, b) in m["content"].as_array().unwrap().iter().enumerate() {
            if b.get("cache_control").is_some() {
                assert_eq!(b["cache_control"]["type"], "ephemeral");
                out.push((i, j));
            }
        }
    }
    out
}

fn tool_result(id: &str, content: &str) -> ContentBlock {
    ContentBlock::ToolResult { tool_use_id: id.to_string(), content: content.to_string(), is_error: None }
}

fn user_blocks(blocks: Vec<ContentBlock>) -> Message {
    Message { role: "user".to_string(), content: MessageContent::Blocks(blocks) }
}

fn tool_use(id: &str) -> ContentBlock {
    ContentBlock::ToolUse { id: id.to_string(), name: "run_bash".to_string(), input: serde_json::json!({}) }
}

#[test]
fn cache_points_go_on_the_last_two_user_messages() {
    let messages = vec![user_text("a"), asst_text("1"), user_text("b"), asst_text("2"), user_text("c")];
    assert_eq!(tagged(&messages), [(2, 0), (4, 0)]);
}

#[test]
fn the_final_tool_results_are_cached() {
    // A run of tool calls with no text between them: each request still
    // caches up to its last tool result.
    let messages = vec![
        user_text("fix it"),
        Message::assistant_blocks(vec![tool_use("a"), tool_use("b")]),
        user_blocks(vec![tool_result("a", "out a"), tool_result("b", "out b")]),
        Message::assistant_blocks(vec![tool_use("c")]),
        user_blocks(vec![tool_result("c", "out c")]),
    ];
    assert_eq!(tagged(&messages), [(2, 1), (4, 0)]);
}

#[test]
fn thinking_and_empty_text_are_skipped() {
    let messages = vec![
        user_blocks(vec![
            tool_result("a", "out"),
            ContentBlock::Text { text: String::new(), cache_control: None },
        ]),
        Message::assistant_blocks(vec![ContentBlock::Thinking { thinking: "hm".into(), signature: None }, tool_use("b")]),
    ];
    // The only user message: its tool result, not the empty text after it;
    // the assistant's thinking never.
    assert_eq!(tagged(&messages), [(0, 0)]);
}

#[test]
fn plain_string_content_is_always_sent_as_a_text_block() {
    let messages = vec![user_text("a"), asst_text("1"), user_text("b"), asst_text("2"), user_text("c")];
    let v = messages_with_cache_points(&messages);
    // Tagged or not, the same shape: the prefix is byte-identical next time.
    assert_eq!(v[0]["content"], serde_json::json!([{ "type": "text", "text": "a" }]));
    assert_eq!(v[2]["content"][0]["text"], "b");
    // At most three breakpoints with the system prompt's.
    assert!(tagged(&messages).len() <= 2);
}

#[test]
fn an_earlier_request_is_a_prefix_of_the_next_once_markers_are_removed() {
    let mut messages = vec![user_text("a"), asst_text("1"), user_text("b")];
    let strip = |mut v: serde_json::Value| {
        for m in v.as_array_mut().unwrap() {
            for b in m["content"].as_array_mut().unwrap() {
                b.as_object_mut().unwrap().remove("cache_control");
            }
        }
        v
    };
    let first = strip(messages_with_cache_points(&messages));
    messages.push(asst_text("2"));
    messages.push(user_text("c"));
    let second = strip(messages_with_cache_points(&messages));
    assert_eq!(first.as_array().unwrap()[..], second.as_array().unwrap()[..3]);
}

// ============================================================
// 10. fmt_num
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
// 11. Workspace Context
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
