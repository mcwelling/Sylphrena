# Syl ✨

**An honorspren for your codebase.**

Syl is a lightweight AI agent harness written in [Babashka](https://babashka.org/) (Clojure). She summons a containerized Ubuntu environment via [Podman](https://podman.io/), connects to Claude via the [Anthropic API](https://docs.anthropic.com/) or [AWS Bedrock](https://aws.amazon.com/bedrock/) with streaming responses, and provides an interactive REPL where an AI assistant can read, write, and execute code safely inside an isolated container.

> *"I'm not a pet. I'm a honorspren. There's a difference."*

## How It Works

```markdown
┌──────────────┐       ┌──────────────────┐  streaming  ┌──────────────────────────┐
│  You (REPL)  │ ───▶  │  syl.bb (Agent)  │ ◀═══════▶  │  Claude API              │
│              │ ◀───  │                  │  SSE/ES     │  (Anthropic or Bedrock)  │
└──────────────┘       └────────┬─────────┘             └──────────────────────────┘
                                │
                                │ tool calls
                                ▼
                       ┌──────────────────┐
                       │  Podman Container │
                       │  (Ubuntu, 2GB)   │
                       │  /workspace mount│
                       └──────────────────┘
```

1. **Container Management** — Syl spins up (or replaces) a Podman container with your project directory mounted at `/workspace`.
2. **SSH Forwarding** — On startup, Syl bind-mounts your host's `~/.ssh` directory read-only into the container, copies the keys to a clean location (`/tmp/.ssh`) with correct ownership and permissions, and installs an SSH wrapper script so that all SSH calls (including those from git) work seamlessly — even when the host UID differs from the container's root user.
3. **Git Identity** — Your host's `git config` identity (`user.name`, `user.email`) is read and applied inside the container, so commits are attributed correctly.
4. **Workspace Context & Memory** — At startup, Syl auto-injects a directory listing, `git status`, and any persistent memories from previous sessions into the system prompt, so she knows the project and your preferences without burning a tool call.
5. **Conversation Loop** — You chat with Syl in a terminal REPL. Responses stream in real-time — via Server-Sent Events (SSE) on the Anthropic backend, or binary EventStream on the Bedrock backend.
6. **Tool Execution** — When Claude decides to run commands, Syl executes them inside the sandboxed container and feeds results back into the conversation. Parallel tool calls are supported. Syl has five built-in tools: `run_bash`, `memory_write`, `memory_read`, `workflow_write`, and `workflow_delete`.
7. **Context Compaction** — When the conversation grows beyond `max-history` messages, Syl asks Claude (via a fast `claude-haiku-4-5` model) to summarize older messages into a compact digest. The most recent `compact-keep` messages are kept verbatim. A role-boundary correction ensures the kept messages always start on a plain `user` message. If summarization fails, a simple trim fallback ensures the conversation continues gracefully.
8. **Container Health Check** — Before every command execution, Syl checks that the container is still alive. If it has died (OOM, manual removal, etc.), she automatically resummons and reconfigures it before proceeding.
9. **Safety** — Your code runs in an isolated, memory-limited (2GB) container. Nothing touches your host system directly. SSH keys are mounted read-only for extra protection.

## Prerequisites

- [Babashka](https://babashka.org/) (bb)
- [Podman](https://podman.io/)
- **For the Anthropic backend (default):** An [Anthropic API key](https://console.anthropic.com/)
- **For the Bedrock backend:** AWS credentials with Bedrock access (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, and optionally `AWS_REGION`)

## Quick Start

```bash
# 1. Run the setup script to check dependencies and configure your API key
./setup.sh

# 2. Run Syl from your project directory
./syl.bb
```

The setup script will:
- Check for Babashka, Podman, and Git (with install instructions if missing)
- Verify Podman machine status (macOS)
- Help you set your `ANTHROPIC_API_KEY` (optionally adding it to your shell config)
- Pre-pull the container image for a faster first run
- Make `syl.bb` executable

You can also set up manually:

```bash
export ANTHROPIC_API_KEY="sk-ant-..."
./syl.bb
```

Syl will summon her container, forward your git and SSH configuration, and greet you with a styled banner. Two status bars are pinned to the top and bottom of the terminal — the banner and all conversation output scroll between them:

```markdown
Sylphrena ─ ↑ 0  ↓ 0  ·  0 calls ─── TPM ░░░░░░░░ 0% ─── 0 msgs ────────────────
≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋
✦ · * · ✦ · * · ✦ · * · ✦ · * · ✦ · * · ✦ · * · ✦ · * · ✦ · * · ✦ · * · ✦ · * ·

                           Life before death
                           Strength before weakness
                           Journey before destination

· * · ✦ · * · ✦ · * · ✦ · * · ✦ · * · ✦ · * · ✦ · * · ✦ · * · ✦ · * · ✦ · * · ✦
≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋

                                       ✦
                                       │
                                      /│\
                                     / │ \
                                    /  │  \
                                   /   │   \
                            ≋≋≋─────── ✦ ───────≋≋≋
                                   \   │   /
                                    \  │  /
                                     \ │ /
                                      \│/
                                       │
                                       ✦

≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋≋
✦ · * · ✦ · * · ✦ · * · ✦ · * · ✦ · * · ✦ · * · ✦ · * · ✦ · * · ✦ · * · ✦ · * ·

                                    S  Y  L
                              The Ancient Daughter
                         Windrunner Spren  ·  AI Agent

─ claude-sonnet-4-6 · 60s ───────────────────────── /help  /status  /save  /exit ─
You: _
```

## CLI Options

```markdown
Usage: ./syl.bb [options]

Options:
  --model <name>       Model name (overrides SYL_MODEL env var)
                       Default: claude-sonnet-4-6
  --backend <name>     API backend: 'anthropic' (default) or 'bedrock'
  --timeout <secs>     Command execution timeout in seconds (default: 60)
  --thinking <budget>  Enable extended thinking with given token budget
  --rate-limit <tpm>   Input tokens-per-minute budget (default: 30000)
                       Raise if your Anthropic tier allows more (e.g. 80000 for Tier 2)
                       Tip: use --backend bedrock to bypass Anthropic rate limits entirely
  --resume             Resume previous conversation session
  --no-banner          Skip the startup splash banner
  --light               Use dark-color palette for light terminal backgrounds
                         (also: SYL_THEME=light)
  --help               Show this help message

Environment:
  ANTHROPIC_API_KEY    (required for anthropic backend) Your Anthropic API key
  SYL_MODEL            Model override (--model takes precedence)
  SYL_BACKEND          Backend override: 'anthropic' or 'bedrock' (--backend takes precedence)
  SYL_RATE_LIMIT       Token-per-minute budget override (--rate-limit takes precedence)
  SYL_THEME             Set to 'light' to use dark-color palette (--light takes precedence)

  AWS (required for bedrock backend):
  AWS_ACCESS_KEY_ID      Your AWS access key
  AWS_SECRET_ACCESS_KEY  Your AWS secret key
  AWS_REGION             AWS region (default: us-east-1)
  AWS_SESSION_TOKEN      (optional) For temporary/session credentials
```

### Examples

```bash
# Standard session (Anthropic backend)
./syl.bb

# Use a different model
./syl.bb --model claude-opus-4-6

# Use AWS Bedrock instead of Anthropic
./syl.bb --backend bedrock --model claude-sonnet-4-6

# Enable extended thinking with a 4000-token budget
./syl.bb --thinking 4000

# Resume a previously saved session
./syl.bb --resume

# Quick session with no banner and a longer timeout
./syl.bb --no-banner --timeout 120

# Use light mode palette for light terminal backgrounds
./syl.bb --light
```

## Slash Commands

Type these at the `You:` prompt during a session:

| Command              | Description                                              |
|----------------------|----------------------------------------------------------|
| `/help`              | Show available slash commands                            |
| `/clear`             | Reset conversation history (container is preserved)      |
| `/status`            | Show session stats: model, backend, tokens used, message count |
| `/save`              | Save conversation to `.syl/session.json` for later `--resume` |
| `/workflow`          | List available workflow pipelines                        |
| `/workflow <name>`   | Run a named workflow pipeline                            |
| `/exit`              | Gracefully shut down Syl and remove the container        |
| `/quit`              | Alias for `/exit`                                        |

## Configuration

These top-level variables are defined at the top of `syl.bb` and can be adjusted:

| Variable           | Default              | Description                                                   |
|--------------------|----------------------|---------------------------------------------------------------|
| `container-name`   | `syl-agent`          | Name of the Podman container                                  |
| `model`            | `claude-sonnet-4-6`  | Claude model. Override with `--model` flag or `SYL_MODEL` env var |
| `compaction-model` | `claude-haiku-4-5`   | Lighter model used for context summarization                  |
| `max-tool-output`  | `8000`               | Max characters stored per tool result in history              |
| `max-history`      | `30`                 | Max messages kept in the conversation sliding window          |
| `compact-keep`     | `10`                 | Recent messages preserved verbatim during context compaction  |
| `max-tokens`       | `8096`               | Max tokens per Claude response                                |
| `max-retries`      | `5`                  | API retry attempts on rate-limit (429/529)                    |
| `rate-limit-budget` | `30000`             | Input tokens per minute budget for proactive throttling       |

The following values can be set at runtime via CLI flags or environment variables:

| Value          | How to change                                                              |
|----------------|---------------------------------------------------------------------------|
| Model          | `--model <name>` flag or `SYL_MODEL` env var                              |
| Backend        | `--backend <name>` flag or `SYL_BACKEND` env var (`anthropic` or `bedrock`) |
| Timeout        | `--timeout <secs>` flag                                                   |
| Thinking budget | `--thinking <tokens>` flag                                               |
| Rate limit      | `--rate-limit <tpm>` flag or `SYL_RATE_LIMIT` env var                   |
| Theme           | `--light` flag or `SYL_THEME=light` env var                               |

## Under the Hood

### Dual Backend Architecture

Syl supports two API backends, selectable at runtime:

| Backend      | Transport         | Auth                          | Notes                                    |
|--------------|-------------------|-------------------------------|------------------------------------------|
| `anthropic`  | HTTPS + SSE       | `ANTHROPIC_API_KEY`           | Default. Direct Anthropic API.           |
| `bedrock`    | HTTPS + EventStream (binary) | AWS Sig V4 credentials | Routes through AWS Bedrock. Useful for enterprise/VPC setups. |

Both backends support streaming, tool use, extended thinking, and context compaction. The Bedrock backend implements AWS Signature Version 4 request signing and parses the binary EventStream framing format natively — no AWS SDK required.

**Supported Bedrock model IDs:**

| Friendly name          | Bedrock model ID                                        |
|------------------------|---------------------------------------------------------|
| `claude-sonnet-4-6`    | `anthropic.claude-sonnet-4-6-20250514-v1:0`             |
| `claude-haiku-4-5`     | `anthropic.claude-haiku-4-5-20251001-v1:0`              |
| `claude-opus-4-6`      | `anthropic.claude-opus-4-6-20250514-v1:0`               |

### SSH Key Forwarding

```markdown
Host ~/.ssh/  (read-only mount)
       │
       ▼
  /tmp/.ssh/  (copied inside container)
       │      chown root:root
       │      chmod 700 (dir) / 600 (keys)
       │
       ▼
  /usr/local/bin/ssh wrapper
  (uses -F /tmp/.ssh/config
   -o UserKnownHostsFile=...)
```

SSH refuses to use key files it doesn't own. By mounting read-only and copying to a root-owned location, we get secure key access without modifying the host's files.

### Streaming Architecture

**Anthropic backend** uses Server-Sent Events (SSE):

```markdown
HTTP POST (stream: true)
         │
         ▼
  ┌─────────────────┐
  │  SSE Event Loop  │
  │                  │
  │  message_start ──▶ record usage metadata
  │  content_block_ ─▶ init text/tool_use/thinking block
  │  content_block_ ─▶ print text deltas / accumulate JSON / show thinking
  │  content_block_ ─▶ finalize block
  │  message_delta ──▶ capture stop_reason + final usage
  │  [DONE] ─────────▶ build result map
  └─────────────────┘
```

**Bedrock backend** uses binary EventStream framing (parsed natively, no SDK):

```markdown
HTTP POST (binary response body)
         │
         ▼
  ┌──────────────────────────┐
  │  EventStream Frame Loop  │
  │                          │
  │  Read 12-byte prelude ───▶ total_len + headers_len + prelude_crc
  │  Read headers ───────────▶ :event-type (e.g. "chunk")
  │  Read payload ───────────▶ JSON with "bytes" field (base64)
  │  Decode + dispatch ──────▶ same SSE event handler as Anthropic
  └──────────────────────────┘
```

Text deltas are printed immediately for a responsive feel. Tool use JSON is accumulated and parsed at block completion. Thinking deltas render in dim terminal text.

### Rate Limit Strategy

Three layers of defense against rate limits:

1. **Proactive budgeting** — Tracks input tokens per minute. When usage exceeds 80% of the budget (default 30k tokens/min), Syl automatically sleeps until the window resets.
2. **Retry with backoff** — On 429/529 responses, retries up to `max-retries` times with exponential backoff (2s, 4s, 8s, 16s, 32s), respecting `retry-after` headers.
3. **Prompt caching** — System prompt and tools are cached server-side via `cache_control` markers, reducing input tokens on repeat calls.

### Context Compaction Strategy

Long conversations are managed through intelligent summarization rather than naive truncation:

```markdown
[msg 1] [msg 2] ... [msg 20] [msg 21] ... [msg 30]
 \_________________________/  \____________________/
          │                          │
    Summarized by claude-haiku  Kept verbatim (compact-keep)
          │                          │
          ▼                          ▼
  [summary message]           Role-boundary correction:
  [ack from assistant]        scan forward past any tool_result
          \                   messages to start on a plain
           \                  user message
            \_________________/
                     │
              New compact history
```

When `max-history` is exceeded:

1. The message list is split: everything except the last `compact-keep` messages becomes the "old" batch.
2. **Role-boundary correction** — the keep batch is scanned forward, skipping messages until a plain `user` message (with string content, not a `tool_result`) is found.
3. The old batch is sent to `claude-haiku-4-5` with a summarization prompt.
4. The summary replaces those messages as a synthetic `[Context summary]` user message, followed by an assistant acknowledgment, then the keep batch.

If summarization fails, the fallback trims to the keep batch so the conversation continues uninterrupted.

### Token Usage Tracking & Status Bars

An atom accumulates token counts across all API calls (both main calls and compaction calls):

```markdown
session-tokens: {:input 0 :output 0 :calls 0}
```

`record-usage!` is called after every successful API response. The `/status` command displays the running totals.

Two status bars are pinned to the top and bottom of the terminal at all times, updating live after every API response:

```
Sylphrena ─ ↑ 1024  ↓ 512  ·  3 calls ─── TPM ████░░░░ 42% ─── 6 msgs ───────────
```

| Field | Description |
|---|---|
| `↑ N` | Cumulative input tokens sent to the API |
| `↓ N` | Cumulative output tokens received |
| `N calls` | Total API calls made this session |
| `TPM ████░░ N%` | Real-time token-per-minute utilisation vs. the `--rate-limit` budget |
| `N msgs` | Number of messages in the active conversation history |

The TPM bar is **colour-coded** by utilisation:
- **Green** — below 50% of budget
- **Yellow** — 50–80% of budget  
- **Red** — 80% or above (proactive throttling kicks in here)

A background daemon thread polls the container for **CPU and memory stats** every 5 seconds and injects them into the bottom status bar, giving live telemetry without blocking the conversation loop.

### Thinking Animation

When extended thinking is enabled (`--thinking <budget>`), a windspren animation runs in the bottom status bar while Claude's extended thinking block is being processed. An animated `≋` character drifts across a trail of `·` dots at 80 ms per frame, visually indicating that deep reasoning is in progress. The animation stops and the bar is cleared the moment the thinking block ends and the text response begins.

### Output Extraction & Truncation

Raw tool output is filtered through two layers before being stored in conversation history:

1. **`max-tool-output` truncation** — Any tool result longer than `max-tool-output` characters (default: `8000`) is hard-truncated. This keeps the context window from ballooning on verbose commands.

2. **Haiku-based extraction** — When a command returns output that is large or contains HTML (e.g. a `curl` returning a webpage), Syl calls `claude-haiku-4-5` with a focused extraction prompt: *"Given this raw output and the command that produced it, extract ONLY the relevant information. Return plain text or compact JSON — never HTML."* The extracted summary replaces the raw output in history. This uses the same retry/backoff logic as the main API loop and supports both Anthropic and Bedrock backends.

### Persistent Memory

Syl's long-term memory is stored as plain markdown files in `.syl/memory/`:

```markdown
.syl/
├── session.json              # Conversation history (ephemeral)
├── memory/
│   ├── project-stack.md      # e.g. "Babashka + Podman + Claude API"
│   ├── user-preferences.md   # e.g. "Prefers concise responses, uses zsh"
│   └── api-decisions.md      # e.g. "Chose SSE streaming over polling"
└── workflows/
    ├── ship.md               # Test → stage → commit → push
    └── check.md              # Test → status → log
```

- **`memory_write`** — Syl stores a note by topic key. Overwrites if the key exists.
- **`memory_read`** — Syl retrieves a specific memory or lists all memories.
- **Auto-injection** — At startup, all memory files are loaded and injected into the system prompt alongside workspace context, so Syl has full recall from turn one.
- **Human-readable** — Memory files are plain markdown, easy to inspect, edit, or delete manually.

### Workflows

Workflows are multi-step pipelines defined as markdown files in `.syl/workflows/`. Syl can create, list, run, and delete them through conversation using the `workflow_write` and `workflow_delete` tools, or you can author them manually.

**Format:**

```markdown
# Ship
Test and push to origin.

- Run tests: `bb test`
- Stage changes: `git add -A`
- Commit: `git commit -m "chore: release"`
- Push: `git push`
```

**Slash commands:**

```markdown
/workflow          — list all workflows
/workflow ship     — run the "ship" workflow
```

Each step runs sequentially inside the container. Output is printed as steps complete.

### Container Health Check

Before every `run-in-container` call, `ensure-container!` runs `podman inspect` to verify the container is in a running state. If it is not, `start-container` is called to re-summon and reconfigure the environment transparently.

## The Oaths

Syl operates under a strict code of honor:

1. **Action Over Words** — She uses her tools immediately rather than just describing what she *would* do.
2. **Verification** — After making changes, she confirms they exist and work.
3. **Honesty** — If something fails, she reports the error exactly. No hidden failures.
4. **Transparency** — Before modifying any file, she shows the intended changes using `diff` or `git diff`. No surprise edits.

## License

MIT License — Copyright (c) 2025 mcwellingtodman

---

*"Life before death. Strength before weakness. Journey before destination."*
