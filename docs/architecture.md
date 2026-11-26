# Syl — Architecture & Internals

This document covers the implementation details behind Syl's main subsystems.

---

## Table of Contents

- [Crates and the Event Stream](#crates-and-the-event-stream)
- [Model Discovery](#model-discovery)
- [Streaming](#streaming)
- [Rate Limiting](#rate-limiting)
- [Session Log](#session-log)
- [Context Compaction](#context-compaction)
- [Container Lifecycle](#container-lifecycle)
- [SSH & Git Forwarding](#ssh--git-forwarding)
- [Persistent Memory](#persistent-memory)
- [Workflows](#workflows)
- [File Tools](#file-tools)
- [Steering and Images](#steering-and-images)
- [Control and Safety](#control-and-safety)
- [Headless and ACP](#headless-and-acp)
- [Extensibility](#extensibility)
- [Output Handling](#output-handling)
- [Status Bar](#status-bar)
- [Thinking Animation](#thinking-animation)
- [Source Layout](#source-layout)

---

## Crates and the Event Stream

The repository is a Cargo workspace with three crates:

| Crate | Owns | Depends on |
|---|---|---|
| `syl-container` (`crates/syl-container`) | Podman lifecycle (start, resume, remove, health check), command execution, stats | nothing in Syl |
| `syl-core` (`crates/syl-core`) | Agent loop, tools, sessions, compaction, the model client and the `ModelAdapter` seam | `syl-container` |
| `syl-rs` (root, binary `syl`) | The terminal client: status bars, animation, picker, slash commands, image rendering | both |

`syl-container` has no dependency on the agent, so Bondsmith can reuse it for agents that don't bring a sandbox. Its progress messages go to a callback instead of stdout.

The core runs the loop on its own task (`agent::spawn`) and has no terminal dependency. A client sends commands and reads events:

```
client ── AgentCommand ──▶ agent loop ── AgentEvent ──▶ client
          Prompt                        TurnStarted
          Cancel                        RequestStarted        (each model call)
          ApprovePermission             TextDelta / ThinkingDelta
          SetModel                      ToolCallStarted {id, name, input}
          Shutdown
                                        ToolCallFinished {id, name, output, is_error}
                                        PermissionRequested   (reserved for Phase 3)
                                        Usage {request, session}
                                        Compacted {before, after}
                                        Notice / Error
                                        TurnEnded {reason}
```

Every `Prompt` produces `TurnStarted`, then other events, then exactly one `TurnEnded` (`Completed(stop_reason)`, `Cancelled` or `Failed`). Prompts sent during a turn run after it, in order. `SetModel` takes effect from the next turn. `Shutdown` cancels any turn in progress and stops the loop.

Session commands (`NewSession`, `LoadSession(id)`, `Fork { at }`, `SetSessionName`) wait for a turn in progress to end, and answer with `SessionChanged { info }` or, on failure, an `Error` outside any turn. The loop also sends `SessionChanged` once when it starts.

The TUI is the first client. It renders events and never calls into the loop. Slash commands still read and edit `SharedState` directly, but only between turns. `show_image` is split in two: the core tool checks that the file exists, and the TUI draws it when it sees the successful `ToolCallFinished`.

Model calls go through the `ModelAdapter` trait (`crates/syl-core/src/model.rs`). `AnthropicAdapter` wraps `call_with_retry_shared` and is the only implementation. The loop, compaction and `dispatch_agent` all use the adapter.

`crates/syl-core/tests/agent_e2e.rs` drives the core with a wiremock model and a stand-in `podman` script (`Container::with_podman`), with no TTY, and asserts on the event stream.

---

## Model Discovery

At startup Syl fetches the available model list from the Anthropic API (5-second timeout), replacing the static fallback list in `SharedState::available_models`.

```
GET https://api.anthropic.com/v1/models
x-api-key: {ANTHROPIC_API_KEY}
anthropic-version: 2023-06-01
```
Returns `data[].{ id, display_name }`. Only `claude-*` entries are kept.

On failure (no credentials, network error, timeout) the compile-time `CLAUDE_MODELS` list in `crates/syl-core/src/config.rs` is used as a fallback. `/model <name>` accepts any string — unknown names are passed to the API as-is and validated there.

---

## Streaming

```
HTTP POST /v1/messages (stream: true)
   │
   ▼
SseParser (byte-chunk line buffer)
   │  message_start        → record usage metadata
   │  content_block_start  → init text / tool_use / thinking block
   │  content_block_delta  → emit BlockDeltaEvent (text / thinking / input_json)
   │  content_block_stop   → finalise block
   │  message_delta        → capture stop_reason + final usage
   └──────────────────────→ TurnResult
```

Text deltas are printed immediately via `terminal::write_streaming`. Tool-use JSON accumulates in `StreamHandler` and is parsed at block completion. Thinking deltas are printed in dim text.

---

## Rate Limiting

Syl follows the limits the API reports rather than guessing them (`api/rate_limit.rs`). Anthropic's limits depend on the organisation's usage tier and the model, and every response says what they are.

1. **The API's buckets.** Each response's `anthropic-ratelimit-{input,output}-tokens-{limit,remaining,reset}` headers are kept in `SharedState::model_limits`, by model, since the API limits each model separately: a `dispatch_agent` call on Haiku or a reviewer call doesn't pace the main model, and the status bar and `/status` show the current model's. A single `tokens` bucket is read as the input one. The buckets refill continuously at `limit` a minute. So before a request Syl works out what each bucket holds by now, and waits only if it can't hold about what the request will need, and 4 096 output tokens (or `max_tokens` if lower). The input estimate (`client::input_need`, at least 1 000) depends on the prompt cache, since cache reads aren't charged:
   - Within 4.5 minutes of the model's last request (the cache lives 5), only the messages added since then count.
   - Later, the cache has expired: the last request's whole input counts again, plus the new messages.
   - A first request, or a conversation shorter than last time (compaction, `/clear`), is estimated whole, system prompt included.

   The wait is just long enough for the shortfall to refill, never past the bucket's reset, and at most two minutes. Reset times are in the server's clock. When the response's `Date` header shows the local clock is more than 2 s off, they are moved onto the local one, so a fast clock doesn't see a bucket full too early.

2. **Retries.** A `429` or `529` (before or during the stream) is retried up to 6 times. The wait is the `retry-after` the API sent (at most two minutes). Without one, it is 1 s doubling to at most 30 s, or longer if the headers on the error itself say the bucket needs longer to refill. Waits of 2 s or more are shown as a notice.

3. **An optional cap of your own.** `rate_limit` (tokens a minute, default 0 = none) keeps Syl below what the API allows, for example to leave room for other tools sharing the key: a sliding 60-second window of uncached input tokens, and past 80% of the cap the next request waits for the window.

3. **Prompt caching** — `cache_control: {type: ephemeral}` breakpoints go on the system prompt and on the last text block of each of the two most recent assistant turns (at most three of the API's four), reducing input tokens on repeat calls via Anthropic's server-side prompt cache.

The status bar's TPM gauge shows how full the API's input bucket is (green < 50%, yellow 50–80%, red ≥ 80%), or the cap's window before any headers have been seen; with neither it is hidden. `/status` lists the API's limits and what is left of each.

---

## Session Log

Each session is an append-only JSONL file, `.syl/sessions/<id>.jsonl`, owned by the agent loop (`crates/syl-core/src/session.rs`). Ids look like `20261122-153012-a1b2c3` (UTC creation time, then random), so they sort by age. Every line is an entry:

```json
{"id":"3f2a9c01b7de","parentId":"a91c0e44f210","ts":1763820000000,"type":"message","message":{"role":"user","content":"hi"}}
```

| `type` | Written when |
|---|---|
| `session` | First entry: log version, working directory, and `forked_from` for a fork |
| `system_prompt` | The system prompt is built for a conversation (then frozen) |
| `message` | A user prompt is accepted, or an assistant turn finishes streaming |
| `tool_result` | Each tool call finishes (consecutive results form one user message) |
| `compaction` | Older history is replaced by a summary; `first_kept` names the first kept message |
| `model_change` | A turn starts on a different model |
| `name` | `/name` |
| `resumed` | A saved session is loaded by a new process |
| `approval` | Reserved for permissions (Phase 3) |

**Durability.** Each entry is written and `fsync`ed before the loop moves on, so killing Syl loses at most the entry being written. An unreadable line is skipped on load and costs only itself: where an entry's parent is missing, the chain carries on from the line before it. A torn tail, or a write that failed partway, is cut off before the next write. A new session's file appears with its first prompt; an unused session leaves nothing behind.

**One writer.** An open session's file is locked (`flock`), so a second Syl process can't resume it and interleave appends; it gets an error and a new session instead.

**History is derived, not stored.** `History::apply` folds the entries of the chain from the last entry back through `parentId` to the root into the messages sent to the model. Nothing is ever rewritten in place: compaction appends an entry, so the full transcript stays on disk. Everything the model sees is in the log, including the frozen system prompt, which a resumed session reuses.

**Resuming.** Loading a session answers any tool call a crash left without a result (an "interrupted" error result) and queues a `resumed` marker; both are written with the next entry. Thinking blocks from before the marker are not sent again, because the new process can't vouch that what they were signed against (tools included) is unchanged.

**Forking** copies the chain up to an entry into a new session, keeping entry ids, and hangs it off a new header. `/fork n` forks just before prompt *n*, so the prompt can be asked differently; `/fork` alone copies everything. The fork doesn't inherit the original's name.

**Large tool output.** `run_bash` output over 100 lines or 8,000 characters is saved to `.syl/spill/<session>/<tool_use_id>.txt`, which the container sees read-only under `/workspace/.syl/spill/`. The file holds the raw output (HTML too, so it keeps its lines), with lines over 2,000 characters wrapped so `read_file`, which pages by line, can reach everything. The model gets the first 100 lines (or the page as text) and that path.

Older `.syl/session.json` files are not migrated and are ignored.

---

## Context Compaction

Long conversations are summarised rather than naively truncated, and compaction is driven by tokens, not message counts.

**When.** Before **every** model call, including mid-turn inside a tool chain, the loop estimates the size of the next request: the token count the API reported for the last request (input, cache reads and writes, plus its output), plus an estimate (about three bytes of JSON per token) for messages added since. It adds room for the response (`max_tokens`) and compacts when the total is within `compact_buffer` (default 15%, at most 50%) of the context window. In the middle of a tool chain the buffer is halved, because compacting there strips thinking from the turn in progress. It doesn't compact again until something has been added since the last compaction. The window comes from a "prompt is too long" error seen this session for the same model, else from `context_window` in `.syl/config.toml`, else from the Models API's `max_input_tokens`, else a per-model default (1M for 4.6 and later and every 5.x model, 200K otherwise).

It also compacts when the API rejects a request as too long, then sends it again (at most twice per turn, and only if compaction shortened the history; otherwise the turn fails with the API's error), and before the next request after a response stops with `model_context_window_exceeded`.

**What.** The most recent `compact_keep_tokens` (default 40,000, estimated, and at most half the trigger) are kept verbatim. The cut moves forward to the first message a tail can start with: a plain user message, or an assistant message (framed as `[user: summary] + tail`, so every `tool_result` keeps its `tool_use`). Everything before the cut is summarised by `claude-haiku-5-5`, in a request with no tools; a summary that doesn't finish (`max_tokens`) is not used. Tool inputs and results are clipped to 2,000 characters in the text the summariser reads, and very long histories are summarised in chunks, each folding in the summary so far. Thinking blocks are removed from the kept tail, since they were signed with the full history in front of them. If summarisation fails, the older messages are dropped from the model's context (they stay in the log) and a notice says so.

---

## Container Lifecycle

```
syl starts
   │
   ├─ marker file (.syl/container-ready) present and matches live container ID?
   │     YES → podman start {name}; verify /workspace mount still live
   │     NO  → podman rm -f (plus any legacy `syl-agent` container for this project); cold start
   │
   └─ cold start
         ├─ syl-agent-ready:latest image exists? → run from baked image (fast)
         └─ base image (mcr.microsoft.com/devcontainers/base:ubuntu)
               → apt install dev tools + rustup
               → podman commit → syl-agent-ready:latest (baked for next time)
```

**Container name:** `syl-agent-<hash>`, where `<hash>` is the first 10 hex characters of SHA-256 of the canonical project path. Each project gets its own container, so several Syl sessions can run side by side. The baked image (`syl-agent-ready:latest`) is shared.

Before every batch of tool calls `Container::ensure` runs `podman inspect`. If the container has died (OOM, sleep/wake, manual removal), it is automatically resummons before the tool runs.

**Container constraints:** `--memory=2g`, `/workspace` bind-mounted read-write, `/workspace/.syl` re-mounted **read-only** (so code in the container can't rewrite Syl's memory, workflows, session or log), `~/.ssh` bind-mounted read-only. Host paths are shell-quoted, so project paths with spaces work.

**Host-side tools:** `memory_*` and `workflow_*` run on the host, not in the container. Their keys are validated (`[A-Za-z0-9._-]`, no `..`, no leading `.`, ≤128 chars) so a key can never resolve outside `.syl/memory/` or `.syl/workflows/`.

---

## SSH & Git Forwarding

SSH refuses to use key files it doesn't own. Syl works around this by copying the host's `~/.ssh` into `/tmp/.ssh` inside the container, setting correct ownership and permissions, and wrapping the `ssh` binary:

```
Host ~/.ssh/  (read-only bind mount)
       │
       ▼
/tmp/.ssh/   (copied inside container)
   chown root:root, chmod 700/600
   ssh-keyscan github.com >> known_hosts
       │
       ▼
/usr/local/bin/ssh wrapper
   exec /usr/bin/ssh -F /tmp/.ssh/config \
        -o UserKnownHostsFile=/tmp/.ssh/known_hosts "$@"
```

Git identity (`user.name`, `user.email`) is read from the host's global config and applied inside the container with `git config --global`.

---

## Persistent Memory

Memory entries are plain markdown files in `.syl/memory/`:

```
.syl/
├── config.toml          # per-project settings
├── sessions/            # one append-only JSONL log per session
├── spill/               # large tool output, per session
├── syl.log              # append-only event log
├── memory/
│   ├── project-stack.md
│   └── preferences.md
└── workflows/
    └── ship.md
```

`memory_write` stores a note by topic key (overwrites if the key exists). `memory_read` returns a specific entry or lists all keys. All memory files are injected into the system prompt when a conversation starts (startup, or after `/clear`), so Syl has full recall from turn one without burning a tool call.

---

## Workflows

Workflows are multi-step pipelines stored in `.syl/workflows/` as markdown files. Format:

```markdown
# Ship
Test and push to origin.

- Run tests: `cargo test`
- Stage: `git add -A`
- Commit: `git commit -m "chore: release"`
- Push: `git push`
```

Each step runs sequentially inside the container via `Container::exec_raw`. Output is printed as steps complete. Steps are parsed by matching list items with a backtick-enclosed command.

Run with `/workflow <name>`. List all with `/workflow`. Claude can create and delete workflows with `workflow_write` / `workflow_delete`.

---

## File Tools

`read_file`, `edit_file`, `write_file`, `grep`, `glob` and `ls` live in `crates/syl-core/src/tools/fs.rs` and run **inside the container** through `Container::exec_argv` (a program and its arguments, no shell string, with optional stdin). Paths resolve in the container's filesystem, so a symlink planted by code in the container can't make Syl write outside it. File contents travel over stdin and come back as bytes, so size and quoting don't matter.

- `edit_file` reads the file, requires `old_string` to occur exactly once (or `replace_all`), writes the result, and returns a unified diff (`similar`, computed off the async runtime with a deadline). On a CRLF file, `\n` line breaks in `old_string` match `\r\n`. `write_file` creates parent directories and returns a diff against the old contents. In the TUI the ticker shows each edit's size, and `/tools` shows the diffs in colour, with control characters made visible.
- Writes go to a temp file beside the target, are checked for length, take the old file's mode, and are renamed over it, so a cancel, timeout or full disk leaves the old file whole. A symlink is written through to its target.
- Every command in the container runs under `timeout -s KILL`, because killing the host-side `podman exec` doesn't stop the process inside.
- `grep` uses ripgrep (installed in new images) and falls back to `grep -rnE` in older ones, which skips `.git`, `.syl`, `target`, `node_modules` and similar by name and matches `glob` against base names. `glob` uses bash `globstar`, expands `{a,b}` itself, and lists only paths that exist. Paths in results are relative to `/workspace`.
- `tools::is_read_only` names the tools with no side effects. The agent loop runs a consecutive run of them with `join_all` and everything else one at a time, in the order the model asked; results always go back in call order, in one user message.

## Steering and Images

**Steering.** `AgentCommand::Steer(text)` during a turn queues the text. After the tool calls running now finish and their results are logged, the queue is logged as `steer` entries and becomes text blocks after those `tool_result` blocks, in the same user message, so the model reads it before its next step. Every `Steer` gets exactly one answer: `SteerDelivered`; `SteerQueued` when the turn ended first (or none was running) and it runs as the next prompt, whose `TurnStarted` follows; or `SteerDropped` when the turn was cancelled or failed. The TUI keeps rendering until every steering message it sent is answered, so a follow-on turn is never left unseen. It reads keys (with bracketed paste, so a paste is one message) on a blocking thread only while a turn renders, draws the line being typed on the terminal's last row with autowrap off, and when the turn ends, stops the reader and carries any unsent text into the next prompt.

**Images.** `attachments::attach_images` finds image paths in a prompt (`@file.png`, dragged paths with quotes or `\ ` escapes, or any word that is an existing image file), checks the file's magic bytes and size, and turns each into an image block placed before the prompt text (`AgentCommand::PromptWithImages`). Without `@`, only full paths count (what dragging a file in gives), and the text keeps the path. A prompt's images are capped at 15 MB in all. Images are logged like everything else the model sees; compaction counts each at about 1,600 tokens rather than by its base64 size.

The API takes about 100 images and 32 MB per request, and images stay in history. Before a request, if the history holds more than 80 images or 24 MB of them, an `images_dropped` entry replaces all but the newest prompt's images with a text marker. If the API rejects a request over its images (a 413, or a 400 about an image), they are all dropped the same way and the request is sent once more. Both also strip thinking blocks, since they edit earlier messages.

---

## Control and Safety

Nothing prompts by default: the container limits the damage, and checkpoints and receipts are the safety net. Rules, plan mode and the reviewer are opt-in.

**Checkpoints** (`checkpoint.rs`). Before the first `run_bash`, `edit_file` or `write_file` call in a turn, the workspace is committed to a shadow repository, `.syl/shadow.git`, whose work tree is the project. The user's own `.git` is never touched. The snapshot honours `.gitignore` and also skips `.git/`, `.syl/`, `node_modules/`, `target/`, `.venv/` and `__pycache__/`. Git runs on the host (`.syl` is read-only in the container) with every inherited `GIT_*` variable cleared, no global or system config, and no hooks. The shadow repository's own config and excludes are rewritten before every use, so a `.syl/shadow.git` shipped with a project can't define an fsmonitor or filter that git would run. A directory that is a git repository of its own is reported once: a snapshot holds only its commit, not its files. A `checkpoint` log entry and a `Checkpointed` event record the commit. `AgentCommand::Restore { commit }` snapshots the current state first, so a restore can itself be undone, then makes the work tree match the checkpoint: tracked files are rewritten, files added since are removed, and ignored files are left alone. That includes ignored files a turn exposed by editing `.gitignore`: those are put back from the safety snapshot. The safety snapshot is listed in `/checkpoints`, so a restore can be undone. It logs `restored` and a `note`, which reaches the model with the next prompt, so Syl knows its edits are gone. In the TUI, `/undo` (or Ctrl+Z at the prompt) shows what would change and asks before restoring the newest checkpoint; `/checkpoints` and `/restore <n>` go further back.

**Receipts.** `/receipts [n]` is built from the log (`session::receipts`): for each turn, the files given to `edit_file`/`write_file`, the commands run, the URLs fetched, the approvals, the failed calls, and the checkpoint.

**Permission rules** (`permissions.rs`). `[[permission]]` tables in `.syl/config.toml` are `{tool, pattern, action}` with `allow`, `ask` or `deny`. Then come plan mode's rules, and the last match wins; with no match a call is allowed. `.syl/approvals.toml` holds the "always" answers. They only turn an `ask` into an allow, so a `deny` added later still wins. A file that doesn't parse is reported and never overwritten. If `config.toml` can't be read, every tool call asks until it is fixed, rather than running with no rules. Unknown keys inside `[[permission]]`, `[reviewer]` or `[container]` are errors; unknown top-level keys are warnings (`[[permissions]]` is flagged). A rule matches a tool (`*` for any) and a glob over the call's subject: the command for `run_bash`, the path for file tools (normalised: workspace-relative, no `./` or `..`), the pattern and the path for searches, the URL for `dispatch_agent`. A `run_bash` command is split into its parts on `&&`, `||`, `;`, `|`, `&` and newlines, plus `$( )` and backticks, Groups, subshells and process substitution are split too. Each part is checked in several forms: as written; with assignments, reserved words (`then`, `do`, `!`) and wrappers (`env`, `sudo`, `timeout`, `nohup`, `xargs`, ...) dropped; with the program's path dropped; with git's global options dropped (`git -C repo push` is `git push`); and inside `sh -c '...'` and `eval`. The strictest answer applies. It's still a simple splitter, not a shell: rules are a convenience, and the container is the boundary. An `ask` emits `PermissionRequested { subject, note }`, where `subject` lists every part that needs asking (all approved together; the TUI shows the whole command) and the turn waits for `ApprovePermission { answer: Once | Always | Reject }`. `Always` saves an allow rule for exactly each of those parts in `.syl/approvals.toml`. In the TUI, keys pressed in the first 300 ms after the question appears are ignored, as typeahead. Every decision that isn't a plain allow is logged as an `approval` entry with who made it (`rule`, `user`, `user_always`, `reviewer`).

**Reviewer** (`control.rs`). With `[reviewer]` configured, an `ask` call first goes to that model (default `claude-haiku-4-5`) with the user's latest request. The request has no tools and asks for ALLOW or BLOCK. The reply's first word must be exactly ALLOW, and a reply naming both verdicts counts as neither. ALLOW runs the call. BLOCK, an error, a timeout (`timeout_secs`, default 20), an unreadable answer, or a call too long to show in full all fall back to asking the user, with the reviewer's reason shown: it fails closed.

**Modes.** `AgentCommand::SetMode(Plan | Build)` applies from the next turn and is told to the model as a `note`. Plan mode adds rules that deny `edit_file`, `write_file`, `workflow_write`, `workflow_delete`, and the git commands that change the user's repository: commit, push, pull, fetch, reset, checkout, switch, restore, stash, rebase, merge, clean, config, tag and branch deletion, among others. Snapshots don't cover `.git`. Each `run_bash` call then runs between two snapshots. Anything it changed in the workspace is put back and reported to the model as an error, and the state before the put-back is kept as a checkpoint. Ignored paths it removed can't be put back, so they are reported. If no snapshot can be taken (no git on the host, or checkpoints off), `run_bash` is refused in plan mode. Plan-mode turns don't take a turn checkpoint, so `/undo` finds the last build turn. What plan mode can't stop: a background process that outlives the command, and changes outside the workspace. The TUI shows the mode in the bar; Shift+Tab toggles it, as do `/plan` and `/build`.

**Container limits.** `[container]` in config.toml sets `memory` (default `2g`), `cpus` and `network` (`"none"` for no network; `dispatch_agent`, package installs and git over the network then fail). The options are recorded as a label on the container, and a container started with other options is recreated.

---

## Headless and ACP

**`syl -p "prompt"`** (`src/headless.rs`) runs one turn without the terminal UI, which suits scripts and CI.
- **Prompt:** taken from the argument (or any arguments that aren't flags), or from stdin when it is omitted or `-`. A terminal on stdin is an error rather than a wait.
- **Output:** the answer's text streams to stdout, and notices, errors and container progress go to stderr. With `--output json`, every `AgentEvent` is printed instead, one JSON object per line, tagged by `type` (`{"type":"text_delta","text":"…"}`, `{"type":"turn_ended","reason":"end_turn"}`).
- **Options:** `--plan` runs the turn in plan mode. `--continue` and `--resume` work as in the TUI.
- **Permissions:** nobody can answer a question, so a call an `ask` rule catches is refused, and stderr says so.
- **Exit status:**
  - 0 when the turn completes.
  - 2 when the answer was cut off (`max_tokens`) or refused.
  - 1 when the turn fails, or a `--resume` can't resume (it doesn't quietly start a new session).
  - 130 when the turn is cancelled, by Ctrl+C or by stdout closing early (`| head`).
- **Container:** left running for the next run. Podman's own output never reaches stdout.

**`syl acp`** (`crates/syl-core/src/acp.rs`) is an [Agent Client Protocol](https://agentclientprotocol.com) v1 agent on stdio, for Zed, Bondsmith or any ACP client.
- **Transport:** JSON-RPC 2.0, one message per line. Only protocol messages go to stdout; logs go to `~/.syl/acp.log`. A line that isn't UTF-8 or JSON gets a -32700 and the connection goes on.
- **Ordering:** requests are applied in the order they arrive, even from a client that doesn't wait for replies. `initialize`, `session/set_mode`, `session/prompt` and `session/cancel` are handled as they are read, so a mode switch applies to the prompt after it, prompts run in order, and a cancel reaches its prompt. Responses may arrive in any order, as JSON-RPC allows.
- **Sessions:**
  - Each ACP session is its own Syl agent loop and session log, and the ACP session id is the Syl session id.
  - Sessions on one project share one container, started once per process.
  - The process exits when the client closes stdin, stopping every session.

| ACP | Syl |
|---|---|
| `initialize` | protocol version 1. Capabilities: `loadSession`, image and embedded-resource prompts, `session/close`. No auth methods. |
| `session/new {cwd, mcpServers}` | a new session in `cwd` (which must be absolute) with `.syl/config.toml` from there. `mcpServers` is ignored until MCP lands (Phase 5). Returns the modes `build` and `plan`. |
| `session/load` | resumes the session from its log and replays it before answering. Prompts become `user_message_chunk`, replies `agent_message_chunk`, tool calls `tool_call` and `tool_call_update`. |
| `session/prompt` | a `Prompt` / `PromptWithImages`. Text and images pass through. A `resource_link` under the project becomes a note to `read_file` it; an embedded `resource` is inlined. Answered with `stopReason` (`end_turn`, `max_tokens`, `refusal`, `cancelled`), or a JSON-RPC error when the turn fails. |
| `session/cancel` | `Cancel`; prompts queued behind the turn are cancelled too, and every waiting prompt answers `cancelled` |
| `session/set_mode {modeId}` | `SetMode`; `current_mode_update` follows when it applies |
| `session/close` | `Shutdown`; prompts still waiting answer `cancelled`; answered once the session's log is released |
| `TextDelta` / `ThinkingDelta` | `agent_message_chunk` / `agent_thought_chunk` |
| `ToolCallStarted` / `ToolCallFinished` | `tool_call` (`in_progress`, with kind, title, raw input and locations) / `tool_call_update` (`completed` or `failed`, with the output) |
| `PermissionRequested` | a pending `tool_call`, then `session/request_permission` offering `allow_once`, `allow_always` (saved for the project) and `reject_once`. Anything but a selected allow is a reject. |
| `Notice` | `notice`, when the client advertises `session.notices`; otherwise only logged |

**Sessions side by side.** Sessions in one process share their project's container, whose start and recovery are serialised. A plan-mode command holds a per-project lock from its snapshot to its put-back, and file-changing calls in other sessions wait for it, so a put-back only undoes what that command did. Known limits:
- That lock doesn't reach across processes, such as a TUI and a `syl acp` on the same project.
- The TUI removes the container when it exits, even while another process is using it. The other process's next tool call recreates it.
- Each session keeps its own tokens-per-minute budget.

To use Syl in Zed, add it as an external agent in Zed's settings. Check Zed's own docs for the current shape:

```json
"agent_servers": {
  "Syl": { "command": "/path/to/syl", "args": ["acp"] }
}
```

---

## Extensibility

Everything here is plain files in `.syl/` (or `~/.syl/`), which other tools such as Bondsmith can read and write too. There is no plugin runtime.

**Where things run.** A project's `.syl/` comes with the project: clone a repository and you get its `config.toml`, commands and skills. So nothing from there runs on the host. Hooks and MCP servers run in the container. An MCP server can run on the host only when it is defined in `~/.syl/config.toml` with `host = true`, or passed in by an ACP client. The same goes for a `url` server, because Syl makes those requests from the host: a project could otherwise point it at your localhost or a cloud metadata address.

**Trust.** Running in the container doesn't make a project's hooks and MCP servers harmless: the container has the network and your SSH keys (`~/.ssh`, mounted read-only), and they run without a tool call or a permission prompt. So a project's `[[hook]]`s and `[[mcp]]` servers are held back until you trust them (`trust.rs`). The TUI lists them at startup and asks; yes is kept in `~/.syl/trusted.json` against a SHA-256 of exactly those entries, so any change to them asks again. Headless mode and ACP can't ask: they leave the entries out and say so (ACP as a message in the session). Run in your home directory, the project's config is `~/.syl/config.toml`, which is yours and needs no trust.

**What the container can write, and what a clone can ship.** Only `.syl/` is mounted read-only; the rest of the workspace, `.agents/` and `AGENTS.md` included, is writable by code in the container, and a cloned repository can ship symbolic links anywhere in it, `.syl/` too. Syl reads these files on the host, so it never follows a symbolic link to one: skills and their file lists, custom commands, memories, workflows and the project's own `AGENTS.md`/`CLAUDE.md` (`util::read_regular`). Memory and workflow writes don't follow one either (`util::write_regular`, with `O_NOFOLLOW`). A skill or command directory that resolves outside the project (or home) is skipped.

**MCP** (`mcp.rs`). `[[mcp]]` tables define servers: `command`/`args`/`env` for stdio, or `url`/`headers` for Streamable HTTP. Syl speaks MCP `2025-06-18`.
- **Startup:** stdio servers start inside the container (`podman exec -i`) with the first turn, all at once and within 60 seconds, and serve every session of the agent. Cancelling the turn stops the wait, and the next turn tries again. Values in `env` reach the container through podman's environment, not its command line, so they don't show in `ps`.
- **Schemas:** each tool's `inputSchema` is made acceptable to the Messages API: an object schema, with top-level `oneOf`/`anyOf`/`allOf`/`$ref` removed (and the schema loosened to allow any properties), since one rejected tool would fail every request of the conversation.
- **Transport:** stdio lines that aren't JSON are skipped; once a server exits, its calls fail at once. Streamable HTTP responses are read as they arrive, SSE events have their `data:` lines joined, and a `ping` from the server inside the stream is answered.
- **Tools:** offered to the model as `<server>__<tool>`, cut to the API's name rules. Server instructions go into the system prompt.
- **Frozen per conversation:** the tool list is fixed with the system prompt and logged as a `tool_set` entry, which keeps the prompt cache and resumed sessions intact. A resumed session keeps the tools it had even if a server is gone now; calling one of those then fails with an unknown-tool error.
- **Results:** text parts become the tool result; images and binary resources are named but not shown. Large results are spilled like `run_bash` output.
- **Permissions:** rules match MCP tools by name (`tool = "github__*"`), with the arguments as compact JSON for the subject. Plan mode refuses MCP tools that don't declare `readOnlyHint`, and runs the rest between two snapshots like `run_bash`, since a server in the container could write anyway.
- **Cancel and size:** a cancelled or timed-out stdio call is forgotten and the server gets `notifications/cancelled`. A message over 16 MB is skipped (stdio) or fails the call (HTTP).
- **Plan mode and host or HTTP servers:** plan mode goes by a tool's `readOnlyHint`, and its snapshot only covers the workspace. A tool on a host or HTTP server that says it only reads but writes anyway changes things no snapshot can put back.
- **Not parallel:** `skill` and MCP calls run one at a time, not alongside read-only built-in tools.
- **Limit:** stopping Syl stops the `podman exec` client and closes the server's stdin; a server that ignores EOF keeps running in the container until the container stops.
- **Not supported:** server-to-client requests (sampling, roots, elicitation) are answered "not supported", and there's no OAuth (use `headers`).

**Skills** (`commands.rs`). Directories with a `SKILL.md` (Agent Skills format: front matter `name`, `description`) in `.syl/skills/`, `.agents/skills/` and `~/.syl/skills/`.
- **In the prompt:** only names and descriptions go into the system prompt.
- **Loading:** the model loads a skill with the `skill` tool, which returns the instructions and lists the other files. Files under the project are readable from the container; files under `~/.syl/skills` are not. `/skill:<name> task` loads one directly.

**Custom commands.** `.syl/commands/<name>.md` (or `~/.syl/commands/`; the project's wins) is a prompt template with optional `description` front matter. `/name words` replaces `$ARGUMENTS` (or `{{args}}`) with the words, or appends them. The core expands commands (in prompts, prompts with images and steering), so they work in the TUI and over ACP; ACP clients get them, and the skills, in `available_commands_update`, sent right after the `session/new` or `session/load` reply. The expanded text is what the session log keeps, so `/history` shows it.

Front matter is `key: value` lines between two `---` lines; `key: >` and `key: |` take the indented lines after them (folded or kept as lines), as Agent Skills descriptions often do. `$ARGUMENTS` and `{{args}}` are replaced in one pass, so the arguments themselves are never substituted into.

**Hooks** (`hooks.rs`). `[[hook]]` tables: `event` (`session_start`, `pre_tool`, `post_tool`, `turn_end`), `command`, an optional `tool` glob and `timeout_secs`. The command runs with `sh -c` in the container, with the event as JSON on stdin. A `pre_tool` hook that exits non-zero blocks the call, and its stderr (else stdout) is the reason the model sees. Other events report a failure as a notice.
- **Cancel:** Ctrl+C (or ACP `session/cancel`) stops a running hook.
- **Session hooks and plan mode:** `session_start` and `turn_end` hooks share the project lock like any write, and in plan mode they too run in a snapshot, with a notice listing anything they changed and that was put back. `turn_end` hooks are part of the turn: `TurnEnded` comes after them, so headless mode waits for them.
- **Plan mode:** a call with tool hooks runs, hooks included, between two snapshots, and what the hooks wrote is put back with the rest.
- **Checkpoints:** a call with tool hooks counts as one that may change files, so the turn is checkpointed before it and `/undo` covers what the hooks wrote.

**Recipes** (`tools/workflow.rs`). Workflows keep their markdown format and gain three things:
- **Parameters:** `params: version, branch` front matter, used as `{{version}}`.
- **Retries:** `(retry N)` on a command step.
- **Prompt steps:** `- Label: ask: a prompt` runs a turn.

`/workflow name 1.2 branch=main` fills the parameters, in order or by name. The words are split like a shell's, so a value with spaces is quoted (`message="fix the build"`); after a named value, an unnamed one is an error rather than a guess. A word with `=` is a named value only when the name is a declared parameter. Steps run one by one, and the first failure stops the run. A command step can run for ten minutes; Ctrl+C stops it. A prompt step succeeds only if the turn ends normally (not cut off at the token limit). Values go into commands as typed, unquoted: write `'{{message}}'` in the recipe if a value may hold spaces or quotes, and remember that `;` in a value starts another command. Workflows that only run commands work as before. Not yet: choosing the model per recipe, a JSON schema for structured output, and running recipes over ACP.

---

## Output Handling

Tool output is processed before it reaches the model (`process_tool_output` in `crates/syl-core/src/util.rs`):

1. **HTML stripping** — Output that looks like HTML has `<script>`/`<style>` blocks and tags removed and whitespace collapsed.

2. **Paging** — Output longer than 100 lines is replaced by a numbered 100-line preview with a hint to narrow the command or use `read_file`.

3. **Hard truncation** — Anything still longer than `MAX_TOOL_OUTPUT` (8 000 bytes) is cut at the nearest UTF-8 character boundary with a `[truncated: N total chars]` trailer.

Separately, `/tools` in the TUI shows `run_bash` output capped at 40 lines with a `... (N more lines)` trailer; this is display-only.

For remote content, Claude can call `dispatch_agent`, which fetches a URL and has a `claude-haiku` sub-agent extract only what the task needs. That is an explicit tool call, not an automatic step applied to `run_bash` output.

---

## Status Bar

Everything fixed sits at the bottom of the terminal, under the conversation:

```
rows 1 … rows-3   the conversation (DECSTBM scroll region)
row  rows-2       the status bar
row  rows-1       input prompt; during a turn, the thinking animation or the tool ticker
row  rows         the steering line during a turn
```

The scroll region starts at row 1 on purpose. Terminals keep lines that scroll off the top of the screen in their scrollback, but discard lines that scroll out of a region starting lower down, so with a bar on row 1 the conversation could not be scrolled back through.

Your messages are printed into the conversation as you send them, right-aligned on a tinted background like a sent text message, wrapped to two thirds of the width (widths in terminal columns, via `unicode-width`). Syl's replies stay on the left.

**The bar** (`make_bar` in `src/tui/bars.rs`), redrawn at each prompt and after every model response:
```
 Syl · claude-opus-5-5 ── ↑12K ↓1K ── $0.1234 ── ███░░░ 50% ── cpu 12% 256M ── 7 msgs ───── /help ─
```

| Field | Description |
|---|---|
| model | The model for the next turn |
| `↑ N` / `↓ N` | Session input / output tokens |
| `$N` | Estimated session cost (USD) |
| `███░ N%` | How much of the API's per-minute input-token limit is in use (or of `rate_limit`, before the API has reported one) |
| `cpu N% NM` | Container CPU and memory (polled every 5 s) |
| `N msgs` | Messages in the conversation history |

On a narrow terminal the bar drops segments, least useful first (messages, container, TPM, tokens); the model and cost always stay. Cache tokens and the timeout are in `/status`.

**The tool ticker** uses the input row during a turn, which the thinking animation also uses; the two never run at once (the animation is for waiting on the model, the ticker for tools). Each tool call overwrites it: while calls run, a spinner with the call (`run_bash › cargo test`, and `+N more` for parallel calls); when they finish, `✓` or `✗` with a few words (`+3 -1` for an edit, a result count for a search). The next prompt clears it. Tool calls don't print into the conversation; `/tools` shows the last turn's calls in full, with output and diffs, read from the session log so it works after `--resume`. `show_image` still draws its image in the conversation.

---

## Thinking Animation

**`--thinking` and adaptive models.** Models with manual extended thinking get `thinking: {type: "enabled", budget_tokens: N}`. Adaptive-thinking models (Opus 4.7 and later, and every 5.x model, including the default `claude-opus-5-5`) reject that with a 400 and always think; by default their thinking arrives *omitted* (an empty block with a signature). For those models `--thinking` sends `thinking: {type: "adaptive", display: "summarized"}` so the summarized reasoning streams in, and the budget number is ignored. See `supports_manual_thinking` in `crates/syl-core/src/config.rs`.

**Thinking blocks and the prefix check.** Opus 5.5 and other models with preserved thinking sign each thinking block against everything before it, and reject (or drop) blocks if `system`, `tools` or an earlier message changed. Syl keeps conversations append-only:

- Assistant turns are stored and sent back exactly as streamed, thinking blocks and signatures included.
- The system prompt (persona, workspace snapshot, memories) is built when a conversation starts and then frozen in `SharedState::system_prompt`. It is rebuilt only while history has no assistant turn yet (new session, or after `/clear`), and logged; a resumed session reuses the logged prompt. Memories written mid-session reach the model through the tool result, and appear in the system prompt from the next conversation.
- Where history *is* cut (compaction keeps a tail after a summary) or replayed by a new process (`--resume`), the affected thinking blocks are removed with `compaction::strip_thinking`, so nothing stale is sent back. Removing thinking blocks is allowed by the check; editing what came before them is not.

When extended thinking is enabled (`--thinking <budget>`), a windspren animation plays in the bottom status bar while Claude's thinking block streams in — an animated `≋` character drifts across a trail of `·` dots at 80 ms per frame. The animation stops the moment the first text delta arrives.

---

## Source Layout

```
Cargo.toml                 # workspace + the syl-rs TUI package
src/                       # syl-rs: the terminal client (binary `syl`)
├── main.rs                # startup, REPL, renders AgentEvents
├── slash.rs               # slash command handlers
├── headless.rs            # syl -p and syl acp
└── tui/
    ├── state.rs           # TuiState (stdout, spinner, animation, stats)
    ├── terminal.rs        # raw mode, scroll region, DECSC/DECRC, SIGWINCH
    ├── bars.rs            # status bar rendering (pure)
    ├── picker.rs          # interactive arrow-key model picker
    ├── animation.rs       # windspren thinking animation
    ├── spinner.rs         # tool execution spinner
    ├── input.rs           # rustyline InputEditor
    ├── steer.rs           # typing during a turn: key reader, steering line
    ├── image_proto.rs     # terminal image protocol detection
    └── image_render.rs    # draws show_image output (timg / chafa)

crates/syl-container/src/
└── lib.rs                 # Container: lifecycle, exec, exec_argv (stdin), stats

crates/syl-core/src/
├── agent.rs               # AgentEvent, AgentCommand, spawn, the loop
├── acp.rs                 # Agent Client Protocol agent (syl acp)
├── model.rs               # ModelAdapter trait, AnthropicAdapter
├── config.rs              # AppConfig, CLI parser, CLAUDE_MODELS fallback
├── shared.rs              # SharedState (history, tokens, model, system prompt)
├── pricing.rs             # cost estimation from token usage
├── session.rs             # append-only session log, derived history, listing
├── system_prompt.rs       # system prompt assembly, AGENTS.md / CLAUDE.md loading
├── attachments.rs         # images named in prompts
├── checkpoint.rs          # shadow-git snapshots and restore
├── commands.rs            # custom slash commands and Agent Skills
├── hooks.rs               # [[hook]] commands
├── mcp.rs                 # MCP client (stdio in the container, Streamable HTTP)
├── permissions.rs         # allow/ask/deny rules, command splitter, modes
├── control.rs             # permission answers, the reviewer
├── workspace.rs           # workspace context (ls + git status)
├── logging.rs             # file logger to .syl/syl.log
├── util.rs                # output processing, HTML stripping, key checks
├── api/
│   ├── client.rs          # stream_turn, call_with_retry_shared, SseParser
│   ├── models.rs          # dynamic model discovery
│   ├── sse.rs             # StreamHandler (SSE event → TurnResult)
│   ├── compaction.rs      # context compaction via Haiku summarisation
│   ├── cache.rs           # cache_control breakpoint injection
│   ├── rate_limit.rs      # the API's rate-limit headers, retry-after, optional own cap
│   └── types.rs           # ContentBlock, Message, TurnResult, Usage
└── tools/
    ├── mod.rs             # ToolContext, execute, is_read_only
    ├── bash.rs            # run_bash
    ├── fs.rs              # read_file, edit_file, write_file, grep, glob, ls
    ├── image.rs           # show_image (existence check)
    ├── dispatch.rs        # dispatch_agent (URL fetch + Haiku sub-agent)
    ├── memory.rs          # memory_read / memory_write
    └── workflow.rs        # workflow_write / workflow_delete / parser
```
