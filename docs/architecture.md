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
- [Output Handling](#output-handling)
- [Status Bars & Token Tracking](#status-bars--token-tracking)
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

Three layers protect against rate limit errors:

1. **Proactive budgeting** — A sliding 60-second window tracks input tokens across all API calls. When usage exceeds 80% of `rate_limit` (default 60 000 TPM), the next call sleeps until the window resets.

2. **Retry with exponential backoff** — On `429` / `529` responses, retries up to 4 times with base delay 500 ms, doubling each attempt (capped at 32 s). `retry-after` headers are respected.

3. **Prompt caching** — `cache_control: {type: ephemeral}` breakpoints go on the system prompt and on the last text block of each of the two most recent assistant turns (at most three of the API's four), reducing input tokens on repeat calls via Anthropic's server-side prompt cache.

The TPM bar in the top status bar is colour-coded: green < 50%, yellow 50–80%, red ≥ 80% (when proactive throttling is active).

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

## Output Handling

Tool output is processed before it reaches the model (`process_tool_output` in `crates/syl-core/src/util.rs`):

1. **HTML stripping** — Output that looks like HTML has `<script>`/`<style>` blocks and tags removed and whitespace collapsed.

2. **Paging** — Output longer than 100 lines is replaced by a numbered 100-line preview with a hint to narrow the command or use `read_file`.

3. **Hard truncation** — Anything still longer than `MAX_TOOL_OUTPUT` (8 000 bytes) is cut at the nearest UTF-8 character boundary with a `[truncated: N total chars]` trailer.

Separately, the TUI shows `run_bash` output capped at 40 lines with a `... (N more lines)` trailer; this is display-only.

For remote content, Claude can call `dispatch_agent`, which fetches a URL and has a `claude-haiku` sub-agent extract only what the task needs. That is an explicit tool call, not an automatic step applied to `run_bash` output.

---

## Status Bars

Two status bars are pinned to the terminal at all times, updated after every API response.

**Top bar:**
```
Syl ── ↑ 1024  ↓ 512  · $0.0124 ── TPM ████░░ 42% ── 80% cpu 123M ── 6 msgs ─────
```

| Field | Description |
|---|---|
| `↑ N` / `↓ N` | Cumulative session input / output tokens |
| `· $N` | Estimated session cost (USD) |
| `TPM ███░ N%` | Real-time TPM vs. rate-limit budget |
| `cpu% / MB` | Container CPU and memory (polled every 5 s) |
| `N msgs` | Messages in the active conversation history |

**Bottom bar:**
```
─ claude-opus-5-5 · 120s ────────────────────────── /help  /save  /exit ─
```

Shows the active model and request timeout. While a tool is running, it switches to a spinner showing the active tool name.

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
└── tui/
    ├── state.rs           # TuiState (stdout, spinner, animation, stats)
    ├── terminal.rs        # raw mode, scroll region, DECSC/DECRC, SIGWINCH
    ├── bars.rs            # top/bottom status bar rendering (pure)
    ├── picker.rs          # interactive arrow-key model picker
    ├── animation.rs       # windspren thinking animation
    ├── spinner.rs         # tool execution spinner
    ├── input.rs           # rustyline InputEditor
    ├── image_proto.rs     # terminal image protocol detection
    └── image_render.rs    # draws show_image output (timg / chafa)

crates/syl-container/src/
└── lib.rs                 # Container: lifecycle, exec, stats, shell_quote

crates/syl-core/src/
├── agent.rs               # AgentEvent, AgentCommand, spawn, the loop
├── model.rs               # ModelAdapter trait, AnthropicAdapter
├── config.rs              # AppConfig, CLI parser, CLAUDE_MODELS fallback
├── shared.rs              # SharedState (history, tokens, model, system prompt)
├── pricing.rs             # cost estimation from token usage
├── session.rs             # append-only session log, derived history, listing
├── system_prompt.rs       # system prompt assembly
├── workspace.rs           # workspace context (ls + git status)
├── logging.rs             # file logger to .syl/syl.log
├── util.rs                # output processing, HTML stripping, key checks
├── api/
│   ├── client.rs          # stream_turn, call_with_retry_shared, SseParser
│   ├── models.rs          # dynamic model discovery
│   ├── sse.rs             # StreamHandler (SSE event → TurnResult)
│   ├── compaction.rs      # context compaction via Haiku summarisation
│   ├── cache.rs           # cache_control breakpoint injection
│   ├── rate_limit.rs      # sliding-window TPM tracking
│   └── types.rs           # ContentBlock, Message, TurnResult, Usage
└── tools/
    ├── mod.rs             # ToolContext, execute (tool dispatch)
    ├── bash.rs            # run_bash
    ├── files.rs           # read_file (paginated)
    ├── image.rs           # show_image (existence check)
    ├── dispatch.rs        # dispatch_agent (URL fetch + Haiku sub-agent)
    ├── memory.rs          # memory_read / memory_write
    └── workflow.rs        # workflow_write / workflow_delete / parser
```
