//! Append-only session logs.
//!
//! A session is a JSONL file, `.syl/sessions/<id>.jsonl`. Each line is an
//! [`Entry`]: an id, the id of the entry before it (`parentId`), a timestamp
//! and a typed payload. Lines are only ever appended, and each one is
//! fsynced before the call returns, so killing Syl loses at most the line
//! being written. A torn or unreadable line is skipped on load.
//!
//! The log is the source of truth. The message history sent to the model is
//! derived from it ([`History`]), so everything the model sees is in the
//! log: prompts, assistant turns, each tool result as it finishes, the frozen
//! system prompt, and compaction summaries. Compaction appends a summary
//! entry instead of rewriting history, so the full transcript survives.
//!
//! The history of a session is the chain of parents from its last entry back
//! to its root. Forking copies that chain up to an entry into a new file.
//!
//! Older `.syl/session.json` files are not read.
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::api::client::SystemBlock;
use crate::api::compaction::{build_compacted_messages, strip_thinking};
use crate::api::types::{ContentBlock, Message, MessageContent};

/// Written in every session header. Bump it when an entry changes shape.
pub const LOG_VERSION: u32 = 1;

/// One line of a session log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    #[serde(rename = "parentId", default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Milliseconds since the Unix epoch.
    pub ts: u64,
    #[serde(flatten)]
    pub kind: EntryKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EntryKind {
    /// The first entry of every session.
    Session {
        version: u32,
        cwd: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        forked_from: Option<ForkOrigin>,
    },
    /// The system prompt, frozen for the conversation from here on.
    SystemPrompt { blocks: Vec<PromptBlock> },
    /// A user prompt or an assistant turn, exactly as sent or received.
    Message { message: Message },
    /// One tool result, logged as soon as the tool finishes. Consecutive
    /// results form one user message.
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
    /// A message the user typed while a turn was running, delivered after
    /// the tool results it waited for (in the same user message).
    Steer { text: String },
    /// History before `first_kept` is replaced by `summary`. With no summary
    /// (the summarizer failed) it is dropped.
    Compaction {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        first_kept: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tokens_before: Option<u64>,
    },
    /// The model used from the next request on.
    ModelChange { model: String },
    Name { name: String },
    /// The session was loaded from disk by a new process. Thinking blocks
    /// from before this point are not sent again: they were signed in a
    /// conversation the new process can't vouch for (tools may differ).
    Resumed,
    /// Images are taken out of the history (each becomes a text marker),
    /// all of them or all but those of the newest message that has any:
    /// too many images make every request fail. Thinking blocks are removed
    /// too, since editing earlier messages invalidates them.
    ImagesDropped { all: bool },
    /// Reserved for the permission engine (Phase 3).
    Approval { tool_use_id: String, approved: bool },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForkOrigin {
    pub session: String,
    pub entry: String,
}

/// A system prompt block as logged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptBlock {
    pub text: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cached: bool,
}

impl PromptBlock {
    pub fn from_system(b: &SystemBlock) -> Self {
        PromptBlock { text: b.text.clone(), cached: b.cache_control.is_some() }
    }

    pub fn to_system(&self) -> SystemBlock {
        if self.cached {
            SystemBlock::cached(self.text.clone())
        } else {
            SystemBlock::text(self.text.clone())
        }
    }
}

// ---------------------------------------------------------------------------
// Deriving history
// ---------------------------------------------------------------------------

/// What a chain of entries adds up to. Built by applying entries in order.
#[derive(Debug, Clone, Default)]
pub struct History {
    /// The messages to send to the model.
    pub messages: Vec<Message>,
    /// The id of the entry that began each message, in step with `messages`.
    starts: Vec<String>,
    pub system: Option<Vec<PromptBlock>>,
    pub name: Option<String>,
    pub model: Option<String>,
    pub forked_from: Option<ForkOrigin>,
}

impl History {
    pub fn from_entries<'a>(entries: impl IntoIterator<Item = &'a Entry>) -> Self {
        let mut h = History::default();
        for e in entries {
            h.apply(e);
        }
        h
    }

    /// The id of the entry that began message `i`.
    pub fn message_start(&self, i: usize) -> Option<&str> {
        self.starts.get(i).map(String::as_str)
    }

    pub fn apply(&mut self, e: &Entry) {
        match &e.kind {
            EntryKind::Session { forked_from, .. } => {
                self.forked_from = forked_from.clone();
            }
            EntryKind::SystemPrompt { blocks } => self.system = Some(blocks.clone()),
            EntryKind::Message { message } => {
                self.messages.push(message.clone());
                self.starts.push(e.id.clone());
            }
            EntryKind::ToolResult { tool_use_id, content, is_error } => {
                let block = ContentBlock::ToolResult {
                    tool_use_id: tool_use_id.clone(),
                    content: content.clone(),
                    is_error: is_error.then_some(true),
                };
                match self.messages.last_mut() {
                    Some(m) if is_tool_results(m) => {
                        if let MessageContent::Blocks(blocks) = &mut m.content {
                            blocks.push(block);
                        }
                    }
                    _ => {
                        self.messages.push(Message {
                            role: "user".to_string(),
                            content: MessageContent::Blocks(vec![block]),
                        });
                        self.starts.push(e.id.clone());
                    }
                }
            }
            EntryKind::Steer { text } => {
                let block = ContentBlock::Text { text: text.clone(), cache_control: None };
                match self.messages.last_mut() {
                    Some(Message { role, content: MessageContent::Blocks(blocks) })
                        if role == "user"
                            && blocks.iter().any(|b| matches!(b, ContentBlock::ToolResult { .. })) =>
                    {
                        blocks.push(block);
                    }
                    _ => {
                        self.messages.push(Message {
                            role: "user".to_string(),
                            content: MessageContent::Blocks(vec![block]),
                        });
                        self.starts.push(e.id.clone());
                    }
                }
            }
            EntryKind::Compaction { summary, first_kept, .. } => {
                let Some(pos) = self.starts.iter().position(|s| s == first_kept) else {
                    tracing::warn!(first_kept, "compaction entry names an unknown message; ignored");
                    return;
                };
                let kept = strip_thinking(self.messages.split_off(pos));
                let mut starts = self.starts.split_off(pos);
                let user_first = kept.first().is_none_or(|m| m.role == "user");
                let framed = if user_first {
                    build_compacted_messages(summary.as_deref(), kept)
                } else {
                    let lead = match summary {
                        Some(s) => format!("[Context summary of earlier conversation]\n{s}"),
                        None => "[Earlier conversation was dropped to save context.]".to_string(),
                    };
                    let mut v = vec![Message::user_text(lead)];
                    v.extend(kept);
                    v
                };
                let lead = framed.len() - starts.len();
                let mut new_starts = vec![e.id.clone(); lead];
                new_starts.append(&mut starts);
                self.messages = framed;
                self.starts = new_starts;
            }
            EntryKind::ModelChange { model } => self.model = Some(model.clone()),
            EntryKind::Name { name } => self.name = Some(name.clone()),
            EntryKind::Resumed => {
                self.messages = strip_thinking(std::mem::take(&mut self.messages));
            }
            EntryKind::ImagesDropped { all } => {
                let keep = if *all { None } else { self.messages.iter().rposition(has_image) };
                for (i, m) in self.messages.iter_mut().enumerate() {
                    if Some(i) == keep {
                        continue;
                    }
                    if let MessageContent::Blocks(blocks) = &mut m.content {
                        for b in blocks.iter_mut() {
                            if matches!(b, ContentBlock::Image { .. }) {
                                *b = ContentBlock::Text {
                                    text: "[image removed from the context to save space]".to_string(),
                                    cache_control: None,
                                };
                            }
                        }
                    }
                }
                self.messages = strip_thinking(std::mem::take(&mut self.messages));
            }
            EntryKind::Approval { .. } => {}
        }
    }

    /// `tool_use` ids in the last assistant turn that have no `tool_result`
    /// yet: what a crash between a tool call and its result leaves behind.
    pub fn unanswered_tool_uses(&self) -> Vec<String> {
        let Some(ai) = self.messages.iter().rposition(|m| m.role == "assistant") else {
            return Vec::new();
        };
        let MessageContent::Blocks(blocks) = &self.messages[ai].content else {
            return Vec::new();
        };
        let answered: HashSet<&str> = self.messages[ai + 1..]
            .iter()
            .filter_map(|m| match &m.content {
                MessageContent::Blocks(b) => Some(b),
                _ => None,
            })
            .flatten()
            .filter_map(|b| match b {
                ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                _ => None,
            })
            .collect();
        blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, .. } if !answered.contains(id.as_str()) => Some(id.clone()),
                _ => None,
            })
            .collect()
    }
}

/// Whether a message carries an image.
pub fn has_image(m: &Message) -> bool {
    matches!(&m.content, MessageContent::Blocks(b) if b.iter().any(|b| matches!(b, ContentBlock::Image { .. })))
}

/// The number of images in `messages`, and their base64 bytes.
pub fn image_load(messages: &[Message]) -> (usize, usize) {
    messages
        .iter()
        .filter_map(|m| match &m.content {
            MessageContent::Blocks(b) => Some(b),
            _ => None,
        })
        .flatten()
        .filter_map(|b| match b {
            ContentBlock::Image { source } => Some(source.data.len()),
            _ => None,
        })
        .fold((0, 0), |(n, bytes), len| (n + 1, bytes + len))
}

fn is_tool_results(m: &Message) -> bool {
    m.role == "user"
        && matches!(&m.content, MessageContent::Blocks(b)
            if !b.is_empty() && b.iter().all(|b| matches!(b, ContentBlock::ToolResult { .. })))
}

// ---------------------------------------------------------------------------
// The log
// ---------------------------------------------------------------------------

/// What a client is told about the current session.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionInfo {
    pub id: String,
    pub name: Option<String>,
    pub message_count: usize,
    /// The session this one was forked from.
    pub forked_from: Option<String>,
    /// Loaded from disk rather than started fresh.
    pub resumed: bool,
}

/// An open session: its entries and their derived history. Appends go to
/// the file (when it has one) and are fsynced. The file is locked while the
/// log is open, so two processes can't interleave appends to one session.
pub struct SessionLog {
    id: String,
    /// None for a session kept only in memory.
    path: Option<PathBuf>,
    file: Option<File>,
    /// Bytes of the file known to hold whole, written entries. Anything
    /// past it (a torn line, a failed write) is cut off before writing.
    good_len: u64,
    /// The current chain, root first.
    entries: Vec<Entry>,
    ids: HashSet<String>,
    /// Entries already applied to `history` but not yet written. A new
    /// session's header waits here so an unused session leaves no file.
    pending: usize,
    history: History,
    resumed: bool,
}

impl SessionLog {
    /// A new session. Its file is created with the first appended entry.
    pub fn create(dir: Option<&Path>, cwd: &Path) -> Self {
        let mut id = new_session_id();
        while dir.is_some_and(|d| d.join(format!("{id}.jsonl")).exists()) {
            id = new_session_id();
        }
        let mut log = SessionLog {
            path: dir.map(|d| d.join(format!("{id}.jsonl"))),
            id,
            file: None,
            good_len: 0,
            entries: Vec::new(),
            ids: HashSet::new(),
            pending: 0,
            history: History::default(),
            resumed: false,
        };
        log.queue(EntryKind::Session {
            version: LOG_VERSION,
            cwd: cwd.to_string_lossy().into_owned(),
            forked_from: None,
        });
        log
    }

    /// Load session `id` from `dir`. Tool calls a crash left without a
    /// result get an "interrupted" result, and a `Resumed` marker is queued;
    /// both are written with the next append. Fails if another process has
    /// the session open.
    pub fn open(dir: &Path, id: &str) -> anyhow::Result<Self> {
        let path = session_path(dir, id)?;
        let file = OpenOptions::new().append(true).open(&path)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                anyhow::bail!("session {id} is open in another Syl process")
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
        }
        let bytes = fs::read(&path)?;
        // Whole lines end at the last newline; a torn tail is cut off at the
        // next write.
        let good_len = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i as u64 + 1);
        let entries = chain_to_last(parse_entries(&path, &bytes));
        if entries.is_empty() {
            anyhow::bail!("session {id} is empty");
        }
        let mut log = SessionLog {
            id: id.to_string(),
            path: Some(path),
            file: Some(file),
            good_len,
            ids: entries.iter().map(|e| e.id.clone()).collect(),
            history: History::from_entries(&entries),
            entries,
            pending: 0,
            resumed: true,
        };
        log.answer_interrupted_tools();
        log.queue(EntryKind::Resumed);
        Ok(log)
    }

    /// A new session holding this one's chain up to and including entry
    /// `at` (the last entry when None). Written at once.
    pub fn fork(&self, dir: Option<&Path>, cwd: &Path, at: Option<&str>) -> anyhow::Result<Self> {
        let end = match at {
            None => self.entries.len(),
            Some(at) => {
                self.entries
                    .iter()
                    .position(|e| e.id == at)
                    .ok_or_else(|| anyhow::anyhow!("entry {at} is not in this session"))?
                    + 1
            }
        };
        let origin = ForkOrigin {
            session: self.id.clone(),
            entry: self.entries[end - 1].id.clone(),
        };
        let mut log = SessionLog::create(dir, cwd);
        if let EntryKind::Session { forked_from, .. } = &mut log.entries[0].kind {
            *forked_from = Some(origin.clone());
        }
        log.history.forked_from = Some(origin);
        // Copy everything after the old header, keeping ids so compaction
        // entries still find their messages, and hang it off the new header.
        let mut parent = log.entries[0].id.clone();
        for e in &self.entries[..end] {
            // The header is replaced, and a name belongs to the original.
            if matches!(e.kind, EntryKind::Session { .. } | EntryKind::Name { .. }) {
                continue;
            }
            let mut e = e.clone();
            e.parent_id = Some(parent);
            parent = e.id.clone();
            log.ids.insert(e.id.clone());
            log.history.apply(&e);
            log.entries.push(e);
            log.pending += 1;
        }
        log.resumed = false;
        log.answer_interrupted_tools();
        log.flush()?;
        Ok(log)
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn history(&self) -> &History {
        &self.history
    }

    pub fn messages(&self) -> &[Message] {
        &self.history.messages
    }

    pub fn info(&self) -> SessionInfo {
        SessionInfo {
            id: self.id.clone(),
            name: self.history.name.clone(),
            message_count: self.history.messages.len(),
            forked_from: self.history.forked_from.as_ref().map(|f| f.session.clone()),
            resumed: self.resumed,
        }
    }

    /// Append an entry after the current last one. It is applied to the
    /// history even if writing fails; the error says the log is behind.
    pub fn append(&mut self, kind: EntryKind) -> anyhow::Result<&Entry> {
        self.queue(kind);
        let written = self.flush();
        written.map(|()| self.entries.last().expect("just queued"))
    }

    /// Write every pending entry, then fsync. On failure nothing counts as
    /// written: the next flush cuts the file back and writes them again.
    pub fn flush(&mut self) -> anyhow::Result<()> {
        if self.pending == 0 {
            return Ok(());
        }
        let Some(path) = &self.path else {
            self.pending = 0;
            return Ok(());
        };
        if self.file.is_none() {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            // create_new: never append to a file this log didn't start.
            let file = OpenOptions::new().append(true).create_new(true).open(path)?;
            file.try_lock().map_err(|e| anyhow::anyhow!("could not lock {}: {e}", path.display()))?;
            // Make the new directory entry durable too.
            if let Some(parent) = path.parent() {
                if let Ok(d) = File::open(parent) {
                    let _ = d.sync_all();
                }
            }
            self.file = Some(file);
            self.good_len = 0;
        }
        let file = self.file.as_mut().expect("opened above");
        if file.metadata()?.len() != self.good_len {
            file.set_len(self.good_len)?;
        }
        let mut buf = String::new();
        for e in &self.entries[self.entries.len() - self.pending..] {
            buf.push_str(&serde_json::to_string(e)?);
            buf.push('\n');
        }
        file.write_all(buf.as_bytes())?;
        file.sync_data()?;
        self.good_len += buf.len() as u64;
        self.pending = 0;
        Ok(())
    }

    fn queue(&mut self, kind: EntryKind) {
        let entry = Entry {
            id: self.fresh_entry_id(),
            parent_id: self.entries.last().map(|e| e.id.clone()),
            ts: now_ms(),
            kind,
        };
        self.history.apply(&entry);
        self.entries.push(entry);
        self.pending += 1;
    }

    fn answer_interrupted_tools(&mut self) {
        for id in self.history.unanswered_tool_uses() {
            self.queue(EntryKind::ToolResult {
                tool_use_id: id,
                content: "[Tool error: interrupted — Syl stopped before this tool finished]".to_string(),
                is_error: true,
            });
        }
    }

    fn fresh_entry_id(&mut self) -> String {
        loop {
            let id = format!("{:012x}", random_u64() & 0xffff_ffff_ffff);
            if self.ids.insert(id.clone()) {
                return id;
            }
        }
    }
}

/// The file for session `id`, refusing ids that could leave `dir`.
fn session_path(dir: &Path, id: &str) -> anyhow::Result<PathBuf> {
    if !crate::util::is_safe_key(id) {
        anyhow::bail!("invalid session id {id:?}");
    }
    Ok(dir.join(format!("{id}.jsonl")))
}

/// Every readable entry of a log file, in file order.
fn parse_entries(path: &Path, bytes: &[u8]) -> Vec<Entry> {
    let mut out = Vec::new();
    for (n, line) in bytes.split(|&b| b == b'\n').enumerate() {
        let Ok(line) = std::str::from_utf8(line) else {
            tracing::warn!(path = %path.display(), line = n + 1, "skipping session line: not UTF-8");
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Entry>(line) {
            Ok(e) => out.push(e),
            Err(e) => tracing::warn!(path = %path.display(), line = n + 1, "skipping session line: {e}"),
        }
    }
    out
}

/// The chain from the last entry back to the root, root first.
fn read_chain(path: &Path) -> anyhow::Result<Vec<Entry>> {
    let bytes = fs::read(path)?;
    Ok(chain_to_last(parse_entries(path, &bytes)))
}

/// Follow parents back from the last entry. Where a parent is missing (its
/// line was unreadable) the walk carries on from the entry just before in
/// the file, so one bad line costs that line, not everything before it.
fn chain_to_last(all: Vec<Entry>) -> Vec<Entry> {
    if all.is_empty() {
        return all;
    }
    let index: std::collections::HashMap<&str, usize> =
        all.iter().enumerate().map(|(i, e)| (e.id.as_str(), i)).collect();
    let mut keep = vec![false; all.len()];
    let mut cur = all.len() - 1;
    loop {
        keep[cur] = true;
        let next = match all[cur].parent_id.as_deref() {
            None => break,
            Some(p) => match index.get(p) {
                Some(&i) if i < cur && !keep[i] => i,
                _ => {
                    tracing::warn!(entry = %all[cur].id, "session entry's parent is missing; continuing from the previous line");
                    match cur.checked_sub(1) {
                        Some(i) if !keep[i] => i,
                        _ => break,
                    }
                }
            },
        };
        cur = next;
    }
    all.into_iter().zip(keep).filter_map(|(e, k)| k.then_some(e)).collect()
}

/// The chain of session `id`, for reading without opening it.
pub fn load_entries(dir: &Path, id: &str) -> anyhow::Result<Vec<Entry>> {
    read_chain(&session_path(dir, id)?)
}

// ---------------------------------------------------------------------------
// Listing and lookup
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct SessionSummary {
    pub id: String,
    pub name: Option<String>,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub message_count: usize,
    pub prompt_count: usize,
    /// The first prompt, cut to one short line.
    pub title: String,
    pub forked_from: Option<String>,
}

/// Every session in `dir`, most recently updated first.
pub fn list_sessions(dir: &Path) -> Vec<SessionSummary> {
    let Ok(read) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<SessionSummary> = read
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|p| {
            let id = p.file_stem()?.to_str()?.to_string();
            let entries = read_chain(&p).ok()?;
            let first = entries.first()?;
            let history = History::from_entries(&entries);
            let prompts = user_prompts(&entries);
            Some(SessionSummary {
                title: prompts.first().map(|p| one_line(&p.text, 60)).unwrap_or_default(),
                prompt_count: prompts.len(),
                created_ms: first.ts,
                // A fork's copied entries keep their old times; its new
                // header is what makes it recent.
                updated_ms: entries.iter().map(|e| e.ts).max().unwrap_or(first.ts),
                message_count: history.messages.len(),
                name: history.name,
                forked_from: history.forked_from.map(|f| f.session),
                id,
            })
        })
        .collect();
    out.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms).then_with(|| b.id.cmp(&a.id)));
    out
}

/// Resolve what a user typed to a session id: empty for the most recent,
/// a list position (`1` is the most recent), an id, a unique id prefix, or
/// a name (the most recent session with it).
pub fn find_session(dir: &Path, query: &str) -> anyhow::Result<String> {
    let all = list_sessions(dir);
    let query = query.trim();
    if all.is_empty() {
        anyhow::bail!("no saved sessions in {}", dir.display());
    }
    if query.is_empty() {
        // The most recent session that has a conversation in it.
        return all
            .iter()
            .find(|s| s.message_count > 0)
            .map(|s| s.id.clone())
            .ok_or_else(|| anyhow::anyhow!("no saved session has any messages"));
    }
    if let Some(s) = all.iter().find(|s| s.id == query) {
        return Ok(s.id.clone());
    }
    if let Some(s) = all.iter().find(|s| s.name.as_deref() == Some(query)) {
        return Ok(s.id.clone());
    }
    if let Ok(n) = query.parse::<usize>() {
        if (1..=all.len()).contains(&n) {
            return Ok(all[n - 1].id.clone());
        }
    }
    let prefixed: Vec<&SessionSummary> = all.iter().filter(|s| s.id.starts_with(query)).collect();
    match prefixed.len() {
        1 => Ok(prefixed[0].id.clone()),
        0 => anyhow::bail!("no session matches {query:?}"),
        n => anyhow::bail!("{n} sessions start with {query:?}; give more of the id"),
    }
}

/// A user prompt in a chain, for choosing a fork point.
#[derive(Debug, Clone, PartialEq)]
pub struct Prompt {
    pub entry: String,
    /// Forking at this entry gives the history just before the prompt.
    pub parent: Option<String>,
    pub text: String,
}

/// The typed prompts of a chain, oldest first (tool results and steering
/// messages excluded; a prompt with images counts, by its text).
pub fn user_prompts(entries: &[Entry]) -> Vec<Prompt> {
    entries
        .iter()
        .filter_map(|e| {
            let EntryKind::Message { message } = &e.kind else { return None };
            if message.role != "user" {
                return None;
            }
            let text = match &message.content {
                MessageContent::Text(t) => t.clone(),
                MessageContent::Blocks(blocks) => {
                    if blocks.iter().any(|b| matches!(b, ContentBlock::ToolResult { .. })) {
                        return None;
                    }
                    blocks
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                }
            };
            Some(Prompt { entry: e.id.clone(), parent: e.parent_id.clone(), text })
        })
        .collect()
}

/// A tool call and its result, as the log recorded them.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallRecord {
    pub name: String,
    pub input: serde_json::Value,
    /// None if the call has no result in the log (yet).
    pub output: Option<String>,
    pub is_error: bool,
}

/// The tool calls of the last turn in a chain (everything after the last
/// typed prompt), in the order they were made.
pub fn last_turn_tool_calls(entries: &[Entry]) -> Vec<ToolCallRecord> {
    let start = user_prompts(entries)
        .last()
        .and_then(|p| entries.iter().position(|e| e.id == p.entry))
        .map_or(0, |i| i + 1);
    let mut calls: Vec<(String, ToolCallRecord)> = Vec::new();
    for e in &entries[start..] {
        match &e.kind {
            EntryKind::Message { message } if message.role == "assistant" => {
                if let MessageContent::Blocks(blocks) = &message.content {
                    for b in blocks {
                        if let ContentBlock::ToolUse { id, name, input } = b {
                            calls.push((
                                id.clone(),
                                ToolCallRecord { name: name.clone(), input: input.clone(), output: None, is_error: false },
                            ));
                        }
                    }
                }
            }
            EntryKind::ToolResult { tool_use_id, content, is_error } => {
                if let Some((_, c)) = calls.iter_mut().find(|(id, _)| id == tool_use_id) {
                    c.output = Some(content.clone());
                    c.is_error = *is_error;
                }
            }
            _ => {}
        }
    }
    calls.into_iter().map(|(_, c)| c).collect()
}

/// `text` on one line of at most `max` characters, with control
/// characters (terminal escapes included) replaced by spaces.
pub fn one_line(text: &str, max: usize) -> String {
    let clean: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let flat: String = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        let cut: String = flat.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

// ---------------------------------------------------------------------------
// Ids and time
// ---------------------------------------------------------------------------

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn random_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    // RandomState is seeded from the OS once per process and varied per
    // instance, which is plenty for ids that only need to be unique.
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
    h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    h.write_u32(std::process::id());
    h.finish()
}

/// `YYYYMMDD-HHMMSS-xxxxxx` in UTC: sorts by creation time.
pub fn new_session_id() -> String {
    let secs = (now_ms() / 1000) as i64;
    format!("{}-{:06x}", utc_stamp(secs, "", "-", ""), random_u64() & 0xff_ffff)
}

/// `YYYY-MM-DD HH:MM` in UTC.
pub fn format_utc(ms: u64) -> String {
    utc_stamp((ms / 1000) as i64, "-", " ", ":")
}

fn utc_stamp(secs: i64, date_sep: &str, mid: &str, time_sep: &str) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss) = (rem / 3600, rem % 3600 / 60, rem % 60);
    if time_sep.is_empty() {
        format!("{y:04}{date_sep}{m:02}{date_sep}{d:02}{mid}{hh:02}{mm:02}{ss:02}")
    } else {
        format!("{y:04}{date_sep}{m:02}{date_sep}{d:02}{mid}{hh:02}{time_sep}{mm:02}")
    }
}

/// Days since 1970-01-01 to (year, month, day). Howard Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_414), (2025, 11, 22));
        assert_eq!(format_utc(1_763_820_000_000), "2025-11-22 14:00");
    }

    #[test]
    fn chain_follows_parents_not_file_order_of_branches() {
        let e = |id: &str, parent: Option<&str>| Entry {
            id: id.into(),
            parent_id: parent.map(Into::into),
            ts: 0,
            kind: EntryKind::Name { name: id.into() },
        };
        // a <- b <- c, and a branch a <- d written last.
        let all = vec![e("a", None), e("b", Some("a")), e("c", Some("b")), e("d", Some("a"))];
        let ids: Vec<String> = chain_to_last(all).into_iter().map(|e| e.id).collect();
        assert_eq!(ids, ["a", "d"]);
    }
}
