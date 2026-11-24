//! The session log on its own: file format, history derivation, recovery
//! from torn writes, listing and lookup.
use std::io::Write;

use serde_json::{json, Value};
use syl_core::api::types::{ContentBlock, Message, MessageContent};
use syl_core::session::{
    find_session, list_sessions, load_entries, user_prompts, Entry, EntryKind, History, SessionLog,
};

fn assistant_with_thinking(text: &str) -> Message {
    Message::assistant_blocks(vec![
        ContentBlock::Thinking { thinking: "hmm".into(), signature: Some("sig".into()) },
        ContentBlock::Text { text: text.into(), cache_control: None },
    ])
}

fn tool_use(id: &str) -> Message {
    Message::assistant_blocks(vec![ContentBlock::ToolUse {
        id: id.into(),
        name: "run_bash".into(),
        input: json!({"command": "true"}),
    }])
}

fn result(id: &str) -> EntryKind {
    EntryKind::ToolResult { tool_use_id: id.into(), content: format!("out {id}"), is_error: false }
}

fn msg(m: Message) -> EntryKind {
    EntryKind::Message { message: m }
}

fn lines(path: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn entries_are_jsonl_with_ids_parents_and_types() {
    let tmp = tempfile::tempdir().unwrap();
    let mut log = SessionLog::create(Some(tmp.path()), tmp.path());
    let path = log.path().unwrap().to_path_buf();
    assert!(!path.exists(), "an unused session leaves no file");

    log.append(msg(Message::user_text("hi"))).unwrap();
    log.append(EntryKind::Name { name: "greeting".into() }).unwrap();
    let l = lines(&path);
    assert_eq!(l.len(), 3);
    assert_eq!(l[0]["type"], "session");
    assert_eq!(l[0]["version"], 1);
    assert!(l[0].get("parentId").is_none());
    assert_eq!(l[1]["type"], "message");
    assert_eq!(l[1]["parentId"], l[0]["id"]);
    assert_eq!(l[1]["message"]["content"], "hi");
    assert_eq!(l[2]["parentId"], l[1]["id"]);
    assert!(l[2]["ts"].as_u64().unwrap() > 1_700_000_000_000);
}

#[test]
fn tool_results_group_into_one_user_message() {
    let tmp = tempfile::tempdir().unwrap();
    let mut log = SessionLog::create(None, tmp.path());
    log.append(msg(Message::user_text("go"))).unwrap();
    log.append(msg(Message::assistant_blocks(vec![
        ContentBlock::ToolUse { id: "a".into(), name: "x".into(), input: json!({}) },
        ContentBlock::ToolUse { id: "b".into(), name: "x".into(), input: json!({}) },
    ])))
    .unwrap();
    log.append(result("a")).unwrap();
    assert_eq!(log.history().unanswered_tool_uses(), ["b"]);
    log.append(result("b")).unwrap();
    assert!(log.history().unanswered_tool_uses().is_empty());
    let m = log.messages();
    assert_eq!(m.len(), 3);
    let MessageContent::Blocks(b) = &m[2].content else { panic!() };
    assert_eq!(b.len(), 2);
}

#[test]
fn a_torn_last_line_is_skipped_and_the_session_resumes() {
    let tmp = tempfile::tempdir().unwrap();
    let mut log = SessionLog::create(Some(tmp.path()), tmp.path());
    log.append(msg(Message::user_text("one"))).unwrap();
    log.append(msg(tool_use("t1"))).unwrap();
    let id = log.id().to_string();
    let path = log.path().unwrap().to_path_buf();
    drop(log);
    // A crash mid-write leaves half a line.
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(br#"{"id":"zz","parentId":"#)
        .unwrap();

    let mut log = SessionLog::open(tmp.path(), &id).unwrap();
    assert!(log.info().resumed);
    // The unanswered call got a result, ready to be written.
    let m = log.messages();
    assert_eq!(m.len(), 3);
    assert!(matches!(&m[2].content, MessageContent::Blocks(b)
        if matches!(&b[0], ContentBlock::ToolResult { is_error: Some(true), .. })));
    log.append(msg(Message::user_text("two"))).unwrap();

    // Reloading sees the repair, the marker and the new prompt, chained
    // past the torn line.
    let entries = load_entries(tmp.path(), &id).unwrap();
    let kinds: Vec<&str> = entries
        .iter()
        .map(|e| match e.kind {
            EntryKind::Session { .. } => "session",
            EntryKind::Message { .. } => "message",
            EntryKind::ToolResult { .. } => "tool_result",
            EntryKind::Resumed => "resumed",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds, ["session", "message", "message", "tool_result", "resumed", "message"]);
}

#[test]
fn resuming_strips_thinking_from_earlier_turns_only() {
    let tmp = tempfile::tempdir().unwrap();
    let mut log = SessionLog::create(Some(tmp.path()), tmp.path());
    log.append(msg(Message::user_text("q"))).unwrap();
    log.append(msg(assistant_with_thinking("a"))).unwrap();
    let id = log.id().to_string();
    drop(log);

    let mut log = SessionLog::open(tmp.path(), &id).unwrap();
    let has_thinking = |m: &Message| matches!(&m.content, MessageContent::Blocks(b) if b.iter().any(|b| b.is_thinking()));
    assert!(!has_thinking(&log.messages()[1]));
    log.append(msg(Message::user_text("q2"))).unwrap();
    log.append(msg(assistant_with_thinking("a2"))).unwrap();
    assert!(has_thinking(&log.messages()[3]), "thinking after the resume is kept");
}

#[test]
fn compaction_frames_by_the_first_kept_role() {
    let tmp = tempfile::tempdir().unwrap();
    let mut log = SessionLog::create(None, tmp.path());
    log.append(msg(Message::user_text("old"))).unwrap();
    log.append(msg(tool_use("t"))).unwrap();
    log.append(result("t")).unwrap();
    log.append(msg(tool_use("u"))).unwrap();
    log.append(result("u")).unwrap();

    // Keep from the second tool call (an assistant message).
    let first_kept = log.history().message_start(3).unwrap().to_string();
    log.append(EntryKind::Compaction { summary: Some("S".into()), first_kept, tokens_before: None })
        .unwrap();
    let m = log.messages();
    assert_eq!(m.len(), 3);
    assert_eq!(m[0].role, "user");
    assert!(matches!(&m[0].content, MessageContent::Text(t) if t.contains('S')));
    assert_eq!(m[1].role, "assistant");

    // A second compaction can keep from a message the first one kept.
    log.append(msg(Message::user_text("new"))).unwrap();
    let first_kept = log.history().message_start(3).unwrap().to_string();
    log.append(EntryKind::Compaction { summary: Some("T".into()), first_kept, tokens_before: None })
        .unwrap();
    let m = log.messages();
    assert_eq!(m.len(), 3, "summary, ack, new");
    assert_eq!(m[1].role, "assistant");
    assert_eq!(m[2], Message::user_text("new"));
}

#[test]
fn fork_reparents_onto_a_new_header_and_is_written() {
    let tmp = tempfile::tempdir().unwrap();
    let mut log = SessionLog::create(Some(tmp.path()), tmp.path());
    log.append(msg(Message::user_text("one"))).unwrap();
    log.append(msg(Message::assistant_blocks(vec![]))).unwrap();
    log.append(msg(Message::user_text("two"))).unwrap();
    let prompts = user_prompts(log.entries());

    let fork = log.fork(Some(tmp.path()), tmp.path(), prompts[1].parent.as_deref()).unwrap();
    assert_eq!(fork.messages().len(), 2);
    assert_eq!(fork.info().forked_from.as_deref(), Some(log.id()));
    let on_disk: Vec<Entry> = load_entries(tmp.path(), fork.id()).unwrap();
    assert_eq!(on_disk.len(), 3);
    assert_eq!(on_disk[1].parent_id.as_deref(), Some(on_disk[0].id.as_str()));
    assert_eq!(History::from_entries(&on_disk).messages, fork.messages());

    assert!(log.fork(None, tmp.path(), Some("not-an-entry")).is_err());
}

#[test]
fn find_session_by_position_prefix_id_and_name() {
    let tmp = tempfile::tempdir().unwrap();
    let mut ids = Vec::new();
    for (i, name) in ["alpha", "beta"].iter().enumerate() {
        let mut log = SessionLog::create(Some(tmp.path()), tmp.path());
        log.append(msg(Message::user_text(format!("prompt {i}")))).unwrap();
        log.append(EntryKind::Name { name: name.to_string() }).unwrap();
        ids.push(log.id().to_string());
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let dir = tmp.path();
    let listed = list_sessions(dir);
    assert_eq!(listed.iter().map(|s| s.id.clone()).collect::<Vec<_>>(), [ids[1].clone(), ids[0].clone()]);
    assert_eq!(listed[1].title, "prompt 0");
    assert_eq!(find_session(dir, "").unwrap(), ids[1]);
    assert_eq!(find_session(dir, "2").unwrap(), ids[0]);
    // A short id prefix is not mistaken for a list position.
    assert!(find_session(dir, "2026").unwrap_err().to_string().contains("2 sessions"));
    assert_eq!(find_session(dir, "alpha").unwrap(), ids[0]);
    assert_eq!(find_session(dir, &ids[0]).unwrap(), ids[0]);
    assert!(find_session(dir, "9").is_err());
    assert!(find_session(dir, "nope").is_err());
    // Both ids start with the date, so a short prefix is ambiguous.
    assert!(find_session(dir, &ids[0][..4]).unwrap_err().to_string().contains("2 sessions"));
    // Ids that could escape the directory are refused.
    assert!(SessionLog::open(dir, "../etc").is_err());
}

#[test]
fn old_session_json_is_ignored() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("session.json"), "[]").unwrap();
    assert!(list_sessions(tmp.path()).is_empty());
}

#[test]
fn a_bad_line_in_the_middle_costs_only_that_line() {
    let tmp = tempfile::tempdir().unwrap();
    let mut log = SessionLog::create(Some(tmp.path()), tmp.path());
    for t in ["p1", "p2", "p3"] {
        log.append(msg(Message::user_text(t))).unwrap();
    }
    let (id, path) = (log.id().to_string(), log.path().unwrap().to_path_buf());
    drop(log);
    let text = std::fs::read_to_string(&path).unwrap();
    let mangled: Vec<String> = text
        .lines()
        .map(|l| if l.contains("\"p2\"") { "{garbage".to_string() } else { l.to_string() })
        .collect();
    std::fs::write(&path, mangled.join("\n") + "\n").unwrap();
    let h = History::from_entries(&load_entries(tmp.path(), &id).unwrap());
    assert_eq!(h.messages, vec![Message::user_text("p1"), Message::user_text("p3")]);
}

#[test]
fn a_session_open_in_one_log_cannot_be_opened_by_another() {
    let tmp = tempfile::tempdir().unwrap();
    let mut log = SessionLog::create(Some(tmp.path()), tmp.path());
    log.append(msg(Message::user_text("mine"))).unwrap();
    let id = log.id().to_string();
    let err = SessionLog::open(tmp.path(), &id).err().expect("locked");
    assert!(err.to_string().contains("another Syl process"), "{err}");
    drop(log);
    let mut reopened = SessionLog::open(tmp.path(), &id).unwrap();
    assert!(SessionLog::open(tmp.path(), &id).is_err());
    reopened.append(msg(Message::user_text("again"))).unwrap();
}

#[test]
fn continue_skips_sessions_with_no_messages() {
    let tmp = tempfile::tempdir().unwrap();
    let mut real = SessionLog::create(Some(tmp.path()), tmp.path());
    real.append(msg(Message::user_text("real work"))).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let mut empty = SessionLog::create(Some(tmp.path()), tmp.path());
    empty.append(EntryKind::Name { name: "just a name".into() }).unwrap();
    assert_eq!(find_session(tmp.path(), "").unwrap(), real.id());
    assert_eq!(find_session(tmp.path(), "1").unwrap(), empty.id(), "positions still count it");
}

#[test]
fn names_lose_control_characters_when_shown() {
    assert_eq!(syl_core::session::one_line("evil\x1b]0;pwn\x07name", 60), "evil ]0;pwn name");
}

#[test]
fn spilled_html_and_long_lines_stay_pageable() {
    use syl_core::util::{process_tool_output_spilled, SpillDir, SPILL_LINE_CHARS};
    let tmp = tempfile::tempdir().unwrap();
    let spill = SpillDir { host: tmp.path().to_path_buf(), container: "/c".into() };
    let html = format!("<html><body>\n{}</body></html>\n", "<p>some row of text</p>\n".repeat(1000));
    let out = process_tool_output_spilled(&html, Some(&spill), "h");
    assert!(out.contains("/c/h.txt"), "{out}");
    assert!(std::fs::read_to_string(tmp.path().join("h.txt")).unwrap().lines().count() > 1000);

    let long = "x".repeat(SPILL_LINE_CHARS * 5 + 7);
    process_tool_output_spilled(&long, Some(&spill), "l");
    let saved = std::fs::read_to_string(tmp.path().join("l.txt")).unwrap();
    assert_eq!(saved.lines().count(), 6);
    assert!(saved.lines().all(|l| l.len() <= SPILL_LINE_CHARS));
}

#[test]
fn an_empty_agents_md_falls_back_to_claude_md_and_the_project_file_gets_the_budget() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("a/b");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("AGENTS.md"), "  \n").unwrap();
    std::fs::write(project.join("CLAUDE.md"), "project rule").unwrap();
    // Outer files big enough to use the whole budget on their own.
    std::fs::write(tmp.path().join("AGENTS.md"), "x".repeat(50_000)).unwrap();
    std::fs::write(tmp.path().join("a/AGENTS.md"), "y".repeat(50_000)).unwrap();
    let text = syl_core::system_prompt::load_project_instructions(&project).unwrap();
    assert!(text.contains("project rule"));
    assert!(text.ends_with("project rule"), "the project's file comes last");
}

#[test]
fn images_dropped_replaces_older_images_and_strips_thinking() {
    use syl_core::api::types::ImageSource;
    let img = |d: &str| ContentBlock::Image { source: ImageSource::base64("image/png", d) };
    let tmp = tempfile::tempdir().unwrap();
    let mut log = SessionLog::create(None, tmp.path());
    log.append(msg(Message { role: "user".into(), content: MessageContent::Blocks(vec![img("old")]) })).unwrap();
    log.append(msg(assistant_with_thinking("seen"))).unwrap();
    log.append(msg(Message { role: "user".into(), content: MessageContent::Blocks(vec![img("new")]) })).unwrap();
    assert_eq!(syl_core::session::image_load(log.messages()), (2, 6));
    log.append(EntryKind::ImagesDropped { all: false }).unwrap();
    assert_eq!(syl_core::session::image_load(log.messages()), (1, 3));
    assert!(matches!(&log.messages()[1].content, MessageContent::Blocks(b) if !b.iter().any(|b| b.is_thinking())));
    log.append(EntryKind::ImagesDropped { all: true }).unwrap();
    assert_eq!(syl_core::session::image_load(log.messages()), (0, 0));
}

#[test]
fn last_turn_tool_calls_come_from_the_log() {
    let tmp = tempfile::tempdir().unwrap();
    let mut log = SessionLog::create(None, tmp.path());
    log.append(msg(Message::user_text("first"))).unwrap();
    log.append(msg(tool_use("old"))).unwrap();
    log.append(result("old")).unwrap();
    log.append(msg(Message::user_text("second"))).unwrap();
    log.append(msg(Message::assistant_blocks(vec![
        ContentBlock::ToolUse { id: "a".into(), name: "ls".into(), input: json!({}) },
        ContentBlock::ToolUse { id: "b".into(), name: "grep".into(), input: json!({"pattern": "x"}) },
    ])))
    .unwrap();
    log.append(EntryKind::ToolResult { tool_use_id: "b".into(), content: "boom".into(), is_error: true }).unwrap();
    log.append(result("a")).unwrap();
    let calls = syl_core::session::last_turn_tool_calls(log.entries());
    let names: Vec<&str> = calls.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["ls", "grep"]);
    assert_eq!(calls[0].output.as_deref(), Some("out a"));
    assert!(calls[1].is_error);
}
