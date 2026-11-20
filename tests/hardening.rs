//! Regression tests for the path-traversal, UTF-8 truncation, container-name,
//! shell-quoting and mid-turn compaction fixes.
use std::path::Path;

use serde_json::json;
use syl_rs::api::compaction::{choose_keep_boundary, KeepBoundary, COMPACT_KEEP};
use syl_rs::api::types::{ContentBlock, Message, MessageContent};
use syl_rs::config::container_name_for;
use syl_rs::tools::memory::{memory_read, memory_write};
use syl_rs::tools::workflow::{load_workflow, workflow_delete, workflow_write};
use syl_rs::util::{is_safe_key, shell_quote, truncate_output, MAX_TOOL_OUTPUT};

// ---------------------------------------------------------------------------
// Key validation / path traversal
// ---------------------------------------------------------------------------

#[test]
fn safe_keys_accept_normal_names() {
    for k in ["ship", "deploy-staging", "project_stack", "v1.2", "ABC123"] {
        assert!(is_safe_key(k), "{k} should be allowed");
    }
}

#[test]
fn safe_keys_reject_traversal_and_junk() {
    for k in ["", "..", "../x", "a/b", "/etc/passwd", ".hidden", "a..b", "a b", "a\\b", "x\0y"] {
        assert!(!is_safe_key(k), "{k:?} should be rejected");
    }
    assert!(!is_safe_key(&"a".repeat(129)));
}

#[test]
fn memory_write_cannot_escape_memory_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let mem = tmp.path().join("memory");
    let r = memory_write(&mem, "../escaped", "pwned");
    assert!(r.starts_with("Error: invalid memory key"), "{r}");
    assert!(!tmp.path().join("escaped.md").exists());

    let r = memory_read(&mem, "../escaped");
    assert!(r.starts_with("Error: invalid memory key"), "{r}");
}

#[test]
fn workflow_tools_cannot_escape_workflow_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let wf = tmp.path().join("workflows");
    let content = "# X\n- Step: `echo hi`\n";

    let r = workflow_write(&wf, "../escaped", content);
    assert!(r.starts_with("Error: invalid workflow name"), "{r}");
    assert!(!tmp.path().join("escaped.md").exists());

    // A file outside the dir must not be deletable or loadable via traversal.
    std::fs::write(tmp.path().join("victim.md"), content).unwrap();
    let r = workflow_delete(&wf, "../victim");
    assert!(r.starts_with("Error: invalid workflow name"), "{r}");
    assert!(tmp.path().join("victim.md").exists());
    assert!(load_workflow(&wf, "../victim").is_none());
}

// ---------------------------------------------------------------------------
// UTF-8-safe truncation
// ---------------------------------------------------------------------------

#[test]
fn truncate_output_does_not_panic_on_multibyte_boundary() {
    // 'é' is 2 bytes; prefix with one ASCII byte so MAX_TOOL_OUTPUT lands mid-char.
    let s = format!("a{}", "é".repeat(MAX_TOOL_OUTPUT));
    assert!(!s.is_char_boundary(MAX_TOOL_OUTPUT));
    let out = truncate_output(&s);
    assert!(out.contains("[truncated:"));
    let body = out.split("\n... [truncated").next().unwrap();
    assert!(body.len() <= MAX_TOOL_OUTPUT);
}

#[test]
fn truncate_output_handles_emoji_and_box_drawing() {
    for unit in ["🙂", "─", "漢"] {
        let s = unit.repeat(MAX_TOOL_OUTPUT);
        let _ = truncate_output(&s); // must not panic
    }
}

// ---------------------------------------------------------------------------
// Shell quoting
// ---------------------------------------------------------------------------

#[test]
fn shell_quote_survives_spaces_and_quotes() {
    for raw in ["/Users/me/My Projects/syl", "it's", "a'b'c", "$(rm -rf /)", ""] {
        let out = std::process::Command::new("sh")
            .args(["-c", &format!("printf %s {}", shell_quote(raw))])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), raw);
    }
}

// ---------------------------------------------------------------------------
// Per-project container names
// ---------------------------------------------------------------------------

#[test]
fn container_names_are_stable_and_per_project() {
    let a1 = container_name_for(Path::new("/Users/me/code/a"));
    let a2 = container_name_for(Path::new("/Users/me/code/a"));
    let b = container_name_for(Path::new("/Users/me/code/b"));
    assert_eq!(a1, a2);
    assert_ne!(a1, b);
    assert!(a1.starts_with("syl-agent-"));
    assert_eq!(a1.len(), "syl-agent-".len() + 10);
    assert!(a1.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
}

// ---------------------------------------------------------------------------
// Mid-turn compaction boundary
// ---------------------------------------------------------------------------

fn tool_use(id: &str) -> Message {
    Message::assistant_blocks(vec![ContentBlock::ToolUse {
        id: id.to_string(),
        name: "run_bash".to_string(),
        input: json!({"command": "true"}),
    }])
}

fn tool_result(id: &str) -> Message {
    Message {
        role: "user".to_string(),
        content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
            tool_use_id: id.to_string(),
            content: "ok".to_string(),
            is_error: None,
        }]),
    }
}

#[test]
fn boundary_prefers_plain_user_message() {
    let mut msgs = vec![Message::user_text("start")];
    for i in 0..15 {
        msgs.push(tool_use(&format!("t{i}")));
        msgs.push(tool_result(&format!("t{i}")));
    }
    msgs.push(Message::user_text("next question"));
    msgs.push(tool_use("z"));
    msgs.push(tool_result("z"));
    let b = choose_keep_boundary(&msgs).unwrap();
    assert_eq!(b, KeepBoundary::UserText(msgs.len() - 3));
}

#[test]
fn boundary_inside_long_tool_chain_starts_at_assistant() {
    // One user prompt followed by a long tool chain: no plain user message in
    // the recent window. Old code kept nothing here and lost the whole chain.
    let mut msgs = vec![Message::user_text("do a big task")];
    for i in 0..20 {
        msgs.push(tool_use(&format!("t{i}")));
        msgs.push(tool_result(&format!("t{i}")));
    }
    let b = choose_keep_boundary(&msgs).unwrap();
    let KeepBoundary::Assistant(i) = b else { panic!("expected Assistant, got {b:?}") };
    assert!(i >= msgs.len() - COMPACT_KEEP);
    assert_eq!(msgs[i].role, "assistant");
    // Every tool_result in the tail is preceded by its tool_use.
    assert_eq!(msgs[i + 1].role, "user");
}
