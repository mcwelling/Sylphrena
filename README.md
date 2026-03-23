# Syl ✨

**An honorspren for your codebase.**

Syl is a lightweight AI agent harness written in Rust. She summons a containerized Ubuntu environment via [Podman](https://podman.io/), connects to Claude via the [Anthropic API](https://docs.anthropic.com/) or [AWS Bedrock](https://aws.amazon.com/bedrock/) with streaming responses, and provides an interactive terminal REPL where an AI assistant can read, write, and execute code safely inside an isolated container.

> *"I'm not a pet. I'm a honorspren. There's a difference."*

## How It Works

```
┌──────────────┐       ┌──────────────────┐  streaming  ┌──────────────────────────┐
│  You (REPL)  │ ───▶  │  syl  (Rust)     │ ◀═══════▶  │  Claude API              │
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
6. **Tool Execution** — When Claude decides to run commands, Syl executes them inside the sandboxed container and feeds results back into the conversation. Parallel tool calls are supported. Syl has eight built-in tools: `run_bash`, `read_file`, `show_image`, `memory_write`, `memory_read`, `workflow_write`, `workflow_delete`, and `dispatch_agent`.
7. **Context Compaction** — When the conversation grows beyond the history limit, Syl asks Claude (via a fast `claude-haiku-4-5` model) to summarize older messages into a compact digest. The most recent messages are kept verbatim. A role-boundary correction ensures the kept messages always start on a plain `user` message. If summarization fails, a simple trim fallback ensures the conversation continues gracefully.
8. **Container Health Check** — Before every command execution, Syl checks that the container is still alive. If it has died (OOM, manual removal, etc.), she automatically resummons and reconfigures it before proceeding.
9. **Safety** — Your code runs in an isolated, memory-limited (2GB) container. Nothing touches your host system directly. SSH keys are mounted read-only for extra protection.

## Prerequisites

- [Rust toolchain](https://rustup.rs/) (`cargo`)
- [Podman](https://podman.io/)
- **For the Anthropic backend (default):** An [Anthropic API key](https://console.anthropic.com/)
- **For the Bedrock backend:** AWS credentials with Bedrock access (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, and optionally `AWS_REGION`)

## Quick Start

```bash
# 1. Run the setup script — checks dependencies, builds the binary, installs the syl command
./setup.sh

# 2. Run Syl from your project directory
syl
```

The setup script will:
- Check for the Rust toolchain, Podman, and Git (with install instructions if missing)
- Verify Podman machine status (macOS)
- Help you set your `ANTHROPIC_API_KEY` (optionally adding it to your shell config)
- Pre-pull the container image for a faster first run
- Build the release binary and symlink it to `~/.local/bin/syl`

You can also build and run manually:

```bash
export ANTHROPIC_API_KEY="sk-ant-..."
cargo build --release
./target/release/syl
```

On startup, Syl renders a truecolor portrait banner, summons her container, forwards your git and SSH configuration, and drops you into the REPL. Two status bars are pinned to the top and bottom of the terminal — all conversation output scrolls between them:

```
Sylphrena ─ ↑ 0  ↓ 0  ·  0 calls ─── TPM ░░░░░░░░ 0% ─── 0 msgs ────────────────

  [truecolor portrait of Sylphrena]

  Life before death  ·  Strength before weakness  ·  Journey before destination

─ claude-opus-4-6 · 120s ────────────────────────── /help  /status  /save  /exit ─
You: _
```

## CLI Options

```
syl — AI coding assistant

Usage: syl [options]

Options:
  --model <name>      Override model (default: claude-opus-4-6)
  --backend <name>    anthropic | bedrock  (default: anthropic)
  --rate-limit <n>    TPM budget (default: 60000)
  --timeout <secs>    Request timeout in seconds (default: 120)
  --thinking <n>      Thinking budget in tokens
  --image <mode>      auto | kitty | iterm2 | sixel | symbols
  --resume            Resume previous conversation session
  --fresh             Nuke container and start a new session
  --no-banner         Suppress startup banner
  --light             Light colour theme
  --help              Show this help

Environment:
  ANTHROPIC_API_KEY    (required for anthropic backend) Your Anthropic API key
  SYL_MODEL            Model override (--model takes precedence)
  SYL_BACKEND          Backend override: 'anthropic' or 'bedrock' (--backend takes precedence)
  SYL_RATE_LIMIT       TPM budget override (--rate-limit takes precedence)
  SYL_THEME            Set to 'light' for light terminal palette (--light takes precedence)
  SYL_IMAGE            Image protocol override (--image takes precedence)

Config file (lowest priority, overridden by env vars and CLI args):
  .syl/config.toml     Per-project settings. Example:
                         model = "claude-opus-4-6"
                         rate_limit = 60000
                         image = "kitty"
                       Supported keys: model, backend, rate_limit, theme, image

  AWS (required for bedrock backend):
  AWS_ACCESS_KEY_ID      Your AWS access key
  AWS_SECRET_ACCESS_KEY  Your AWS secret key
  AWS_REGION             AWS region (default: us-east-1)
  AWS_SESSION_TOKEN      (optional) For temporary credentials
```

### Examples

```bash
# Standard session (Anthropic backend)
syl

# Use a different model
syl --model claude-opus-4-6

# Use AWS Bedrock instead of Anthropic
syl --backend bedrock

# Enable extended thinking with a 4000-token budget
syl --thinking 4000

# Resume a previously saved session
syl --resume

# Force a fresh container and new session
syl --fresh

# Quick session with no banner and a longer timeout
syl --no-banner --timeout 180
```

## Slash Commands

Type these at the `You:` prompt during a session:

| Command              | Description                                              |
|----------------------|----------------------------------------------------------|
| `/help`              | Show available slash commands                            |
| `/clear`             | Reset conversation history (container is preserved)      |
| `/status`            | Show session stats: model, backend, timeout, rate limit, message count, token usage |
| `/save`              | Save conversation to `.syl/session.json` for later `--resume` |
| `/log`               | Show the last 20 lines of `.syl/syl.log` (errors, retries, events) |
| `/memory`            | List all persistent memory keys and their contents       |
| `/memory <key>`      | Show the contents of a specific memory key               |
| `/workflow`          | List available workflow pipelines                        |
| `/workflow <name>`   | Run a named workflow pipeline                            |
| `/exit`              | Gracefully shut down Syl and remove the container        |
| `/quit`              | Alias for `/exit`                                        |

## Configuration

Config priority: **CLI flag > environment variable > `.syl/config.toml` > default**

| Value           | CLI flag                  | Env var           | config.toml key  |
|-----------------|---------------------------|-------------------|------------------|
| Model           | `--model <name>`          | `SYL_MODEL`       | `model`          |
| Backend         | `--backend <name>`        | `SYL_BACKEND`     | `backend`        |
| Timeout         | `--timeout <secs>`        | —                 | —                |
| Thinking budget | `--thinking <tokens>`     | —                 | —                |
| Rate limit      | `--rate-limit <tpm>`      | `SYL_RATE_LIMIT`  | `rate_limit`     |
| Theme           | `--light`                 | `SYL_THEME=light` | `theme = "light"`|
| Image protocol  | `--image <mode>`          | `SYL_IMAGE`       | `image`          |

> **Migrating from the Babashka version?** If `.syl/config.edn` exists and `.syl/config.toml` does not, Syl will automatically migrate your settings on first run.

## Under the Hood

### Dual Backend Architecture

| Backend      | Transport                        | Auth                       | Notes                                        |
|--------------|----------------------------------|----------------------------|----------------------------------------------|
| `anthropic`  | HTTPS + SSE                      | `ANTHROPIC_API_KEY`        | Default. Direct Anthropic API.               |
| `bedrock`    | HTTPS + EventStream (binary)     | AWS Sig V4 credentials     | Routes through AWS Bedrock. No AWS SDK needed. |

Both backends support streaming, tool use, extended thinking, and context compaction. The Bedrock backend implements AWS Signature Version 4 request signing and parses the binary EventStream framing format natively.

**Supported Bedrock model IDs:**

| Friendly name          | Bedrock model ID                                        |
|------------------------|---------------------------------------------------------|
| `claude-opus-4-6`      | `anthropic.claude-opus-4-6-20250514-v1:0`               |
| `claude-sonnet-4-6`    | `anthropic.claude-sonnet-4-6-20250514-v1:0`             |
| `claude-haiku-4-5`     | `anthropic.claude-haiku-4-5-20251001-v1:0`              |

### SSH Key Forwarding

```
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

```
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

```
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

Text deltas are printed immediately for a responsive feel. Tool use JSON is accumulated and parsed at block completion. Thinking blocks render in dim terminal text.

### Rate Limit Strategy

Three layers of defense against rate limits:

1. **Proactive budgeting** — Tracks input tokens per minute. When usage exceeds 80% of the budget (default 60k TPM), Syl automatically sleeps until the window resets.
2. **Retry with backoff** — On 429/529 responses, retries up to 4 times with exponential backoff (base 500 ms), respecting `retry-after` headers.
3. **Prompt caching** — System prompt and tools are cached server-side via `cache_control` markers, reducing input tokens on repeat calls.

### Context Compaction

Long conversations are managed through intelligent summarization rather than naive truncation:

```
[msg 1] [msg 2] ... [msg N-K] [msg N-K+1] ... [msg N]
 \_________________________/   \____________________/
          │                            │
    Summarized by claude-haiku   Kept verbatim
          │                            │
          ▼                            ▼
  [summary message]            Role-boundary correction:
  [ack from assistant]         scan forward past any tool_result
          \                    messages to start on a plain
           \                   user message
            \__________________/
                     │
              New compact history
```

If summarization fails, the fallback trims to the keep batch so the conversation continues uninterrupted.

### Token Usage & Status Bars

Two status bars are pinned to the terminal at all times, updating live after every API response:

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

The TPM bar is colour-coded by utilisation:
- **Green** — below 50% of budget
- **Yellow** — 50–80% of budget
- **Red** — 80% or above (proactive throttling kicks in here)

A background task polls the container for **CPU and memory stats** every 5 seconds and injects them into the bottom status bar.

### Thinking Animation

When extended thinking is enabled (`--thinking <budget>`), a windspren animation runs in the bottom status bar while Claude's extended thinking block is being processed. An animated `≋` character drifts across a trail of `·` dots at 80 ms per frame, visually indicating that deep reasoning is in progress.

### Output Extraction & Truncation

Tool output is processed through three layers:

1. **Display cap** — `run_bash` output is capped at 40 lines in the terminal. If the result is longer, a `... [N more lines — full output sent to model]` trailer is shown. The full output is still forwarded to the model.

2. **History truncation** — Any tool result longer than the `max-tool-output` limit is hard-truncated before being stored in conversation history, keeping the context window from ballooning on verbose commands.

3. **Haiku-based extraction** — When a command returns large or HTML output (e.g. `curl` returning a webpage), Syl calls `claude-haiku-4-5` with a focused extraction prompt to distil only the relevant information. The extracted summary replaces the raw output in history.

### Persistent Memory

Syl's long-term memory is stored as plain markdown files in `.syl/memory/`:

```
.syl/
├── session.json              # Conversation history (loaded with --resume)
├── syl.log                   # Append-only event log (errors, retries, stream failures)
├── config.toml               # Per-project settings
├── memory/
│   ├── project-stack.md      # e.g. "Rust + Podman + Claude API"
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

- Run tests: `cargo test`
- Stage changes: `git add -A`
- Commit: `git commit -m "chore: release"`
- Push: `git push`
```

**Slash commands:**

```
/workflow          — list all workflows
/workflow ship     — run the "ship" workflow
```

Each step runs sequentially inside the container. Output is printed as steps complete.

### Container Health Check

Before every tool execution, Syl verifies the container is in a running state via `podman inspect`. If it is not, the container is automatically resummons and reconfigured before proceeding.

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
