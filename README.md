# Syl ✨

**An honorspren for your codebase.**

Syl is a lightweight AI agent harness written in [Babashka](https://babashka.org/) (Clojure). She summons a containerized Ubuntu environment via [Podman](https://podman.io/), connects to [Anthropic's Claude API](https://docs.anthropic.com/) with streaming responses, and provides an interactive REPL where an AI assistant can read, write, and execute code safely inside an isolated container.

> *"I'm not a pet. I'm a honorspren. There's a difference."*

## How It Works

```
┌──────────────┐       ┌──────────────────┐  streaming  ┌──────────────────────┐
│  You (REPL)  │ ───▶  │  syl.bb (Agent)  │ ◀═══════▶  │  Claude API          │
│              │ ◀───  │                  │    SSE      │  (claude-sonnet-4-6) │
└──────────────┘       └────────┬─────────┘             └──────────────────────┘
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
4. **Workspace Context** — At startup, Syl auto-injects a directory listing and `git status` into the system prompt, so she knows the project structure without burning a tool call.
5. **Conversation Loop** — You chat with Syl in a terminal REPL. Responses stream in real-time via Server-Sent Events (SSE).
6. **Tool Execution** — When Claude decides to run commands, Syl executes them inside the sandboxed container and feeds results back into the conversation. Parallel tool calls are supported.
7. **Context Compaction** — When the conversation grows beyond `max-history` messages, Syl asks Claude (via a fast `claude-haiku-4-5` model) to summarize older messages into a compact digest. The most recent `compact-keep` messages are kept verbatim. A role-boundary correction ensures the kept messages always start on a plain `user` message. If summarization fails, a simple trim fallback ensures the conversation continues gracefully.
8. **Container Health Check** — Before every command execution, Syl checks that the container is still alive. If it has died (OOM, manual removal, etc.), she automatically resummmons and reconfigures it before proceeding.
9. **Safety** — Your code runs in an isolated, memory-limited (2GB) container. Nothing touches your host system directly. SSH keys are mounted read-only for extra protection.

## Prerequisites

- [Babashka](https://babashka.org/) (bb)
- [Podman](https://podman.io/)
- An [Anthropic API key](https://console.anthropic.com/)

## Quick Start

```bash
# 1. Set your API key
export ANTHROPIC_API_KEY="sk-ant-..."

# 2. Run Syl from your project directory
./syl.bb
```

Syl will summon her container, forward your git and SSH configuration, and greet you with a styled banner:

```
  ✦ · * · ✦ · * · ✦ · * · ✦ · * · ✦ · * · ✦
  ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋

      Life before death           ≋≋
      Strength before weakness    ≋≋
      Journey before destination  ≋≋

                 ✦
                /|\
               / | \
  ≋≋≋─────────── ✦ ───────────≋≋≋
               \ | /
                \|/
                 ✦

              S  Y  L
        The Ancient Daughter

  Model: claude-sonnet-4-6   Timeout: 60s   Type /help for commands

You: _
```

## CLI Options

```
Usage: ./syl.bb [options]

Options:
  --model <name>       Anthropic model (overrides SYL_MODEL env var)
                       Default: claude-sonnet-4-6
  --timeout <secs>     Command execution timeout in seconds (default: 60)
  --thinking <budget>  Enable extended thinking with given token budget
  --resume             Resume previous conversation session
  --no-banner          Skip the startup splash banner
  --help               Show this help message

Environment:
  ANTHROPIC_API_KEY    (required) Your Anthropic API key
  SYL_MODEL            Anthropic model override (--model takes precedence)
```

### Examples

```bash
# Standard session
./syl.bb

# Use a different model
./syl.bb --model claude-opus-4-6

# Enable extended thinking with a 4000-token budget
./syl.bb --thinking 4000

# Resume a previously saved session
./syl.bb --resume

# Quick session with no banner and a longer timeout
./syl.bb --no-banner --timeout 120
```

## Slash Commands

Type these at the `You:` prompt during a session:

| Command   | Description                                              |
|-----------|----------------------------------------------------------|
| `/help`   | Show available slash commands                            |
| `/clear`  | Reset conversation history (container is preserved)      |
| `/status` | Show session stats: model, tokens used, message count    |
| `/save`   | Save conversation to `.syl/session.json` for later `--resume` |
| `/exit`   | Gracefully shut down Syl and remove the container        |
| `/quit`   | Alias for `/exit`                                        |

## Configuration

These top-level variables are defined at the top of `syl.bb` and can be adjusted:

| Variable           | Default              | Description                                                   |
|--------------------|----------------------|---------------------------------------------------------------|
| `container-name`   | `syl-agent`          | Name of the Podman container                                  |
| `model`            | `claude-sonnet-4-6`  | Anthropic model. Override with `--model` flag or `SYL_MODEL` env var |
| `compaction-model` | `claude-haiku-4-5`   | Lighter model used for context summarization                  |
| `max-tool-output`  | `8000`               | Max characters stored per tool result in history              |
| `max-history`      | `30`                 | Max messages kept in the conversation sliding window          |
| `compact-keep`     | `10`                 | Recent messages preserved verbatim during context compaction  |
| `max-tokens`       | `8096`               | Max tokens per Claude response                                |
| `max-retries`      | `5`                  | API retry attempts on rate-limit (429/529)                    |
| `rate-limit-budget` | `30000`             | Input tokens per minute budget for proactive throttling       |

The following values can be set at runtime via CLI flags or are hardcoded inline:

| Value          | How to change              | Default | Description                   |
|----------------|----------------------------|---------|-------------------------------|
| `cmd-timeout-ms` | `--timeout <secs>` flag  | `60s`   | Command execution timeout     |
| `thinking-budget` | `--thinking <budget>` flag | `nil` (disabled) | Extended thinking token budget |
| `memory`       | Edit `start-container`     | `2g`    | Container memory limit        |
| `image`        | Edit `start-container`     | `mcr.microsoft.com/devcontainers/base:ubuntu` | Container base image |

## Features

- **Streaming Responses** — Text streams word-by-word via SSE as Claude generates it, instead of waiting for the full response. Tool use and text are parsed incrementally from the event stream.
- **Prompt Caching** — The system prompt and tools include `cache_control` markers, so Anthropic caches them across turns. After the first call, input tokens drop significantly, reducing cost and rate limit pressure.
- **Proactive Rate Limit Budgeting** — Tracks input tokens per minute and automatically throttles before hitting the API rate limit, instead of waiting for a 429 error.
- **Extended Thinking** — Enable Claude's extended thinking with `--thinking <budget>`. Thinking text streams in dim terminal text so you can follow the reasoning process in real-time.
- **Conversation Persistence** — `/save` writes the conversation to `.syl/session.json`. Use `--resume` to pick up where you left off after a crash or restart.
- **Workspace Context Injection** — At startup, Syl auto-injects a directory listing and `git status` into the system prompt so she knows the project layout without a tool call.
- **Graceful Shutdown** — `/exit`, `/quit`, and Ctrl+D trigger clean teardown. A JVM shutdown hook also cleans up the container on Ctrl+C / SIGINT.
- **Anthropic Claude Integration** — Powered by `claude-sonnet-4-6` (default) via the Anthropic Messages API. Override at runtime with `--model` or the `SYL_MODEL` environment variable.
- **Slash Commands** — `/help`, `/clear`, `/status`, `/save`, `/exit`, `/quit` for session control without leaving the REPL.
- **Token Usage Tracking** — Every API call's input/output tokens are accumulated. View totals at any time with `/status`.
- **Container Health Check** — Before each command, Syl verifies the container is running. If it has died, she automatically resummmons and reconfigures it.
- **Configurable Timeout** — Command execution timeout defaults to 60s but can be set with `--timeout <secs>`.
- **Splash Banner** — On startup, Syl displays a styled ASCII art banner featuring the First Ideal and a Shardblade glyph, with the active model, timeout, and thinking budget shown. Suppress with `--no-banner`.
- **Read-Only SSH Mount with Wrapper** — Your host `~/.ssh` is bind-mounted read-only. Keys are copied to `/tmp/.ssh` with corrected ownership and permissions, and a wrapper script ensures all SSH invocations (including `git push/pull`) use the corrected keys.
- **Git Identity Forwarding** — Your host's `user.name` and `user.email` are applied inside the container.
- **Parallel Tool Execution** — Claude can issue multiple `run_bash` calls in a single response; Syl executes them all and returns results together.
- **Rate-Limit Retry with Backoff** — Both the main API call and compaction calls retry on 429/529 responses with exponential backoff and `retry-after` header support.
- **Tool Output Truncation** — Long command outputs are automatically truncated to `max-tool-output` characters.
- **Context Compaction** — When the conversation exceeds `max-history` messages, older messages are summarized by `claude-haiku-4-5` into a concise digest. The most recent `compact-keep` messages are kept verbatim with role-boundary correction.

## Architecture Notes

### SSH Forwarding Strategy

The SSH setup solves a cross-platform ownership problem:

```
Host (~/.ssh, UID 501) ──read-only mount──▶ Container (/root/.ssh, UID 0)
                                                  │
                                           copy to /tmp/.ssh
                                           chown root:root
                                           chmod 700 (dir) / 600 (keys)
                                                  │
                                           /usr/local/bin/ssh wrapper
                                           (uses -F /tmp/.ssh/config
                                            -o UserKnownHostsFile=...)
```

SSH refuses to use key files it doesn't own. By mounting read-only and copying to a root-owned location, we get secure key access without modifying the host's files.

### Streaming Architecture

Syl uses the Anthropic streaming API with Server-Sent Events (SSE):

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

Text deltas are printed immediately for a responsive feel. Tool use JSON is accumulated and parsed at block completion. Thinking deltas render in dim terminal text.

### Rate Limit Strategy

Three layers of defense against rate limits:

1. **Proactive budgeting** — Tracks input tokens per minute. When usage exceeds 80% of the budget (default 30k tokens/min), Syl automatically sleeps until the window resets.
2. **Retry with backoff** — On 429/529 responses, retries up to `max-retries` times with exponential backoff (2s, 4s, 8s, 16s, 32s), respecting `retry-after` headers.
3. **Prompt caching** — System prompt and tools are cached server-side via `cache_control` markers, reducing input tokens on repeat calls.

### Context Compaction Strategy

Long conversations are managed through intelligent summarization rather than naive truncation:

```
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

### Token Usage Tracking

An atom accumulates token counts across all API calls (both main calls and compaction calls):

```
session-tokens: {:input 0 :output 0 :calls 0}
```

`record-usage!` is called after every successful API response. The `/status` command displays the running totals.

### Container Health Check

Before every `run-in-container` call, `ensure-container!` runs `podman inspect` to verify the container is in a running state. If it is not, `start-container` is called to re-summon and reconfigure the environment transparently.

## The Oaths

Syl operates under a strict code of honor:

1. **Action Over Words** — She uses her tools immediately rather than just describing what she *would* do.
2. **Verification** — After making changes, she confirms they exist and work.
3. **Honesty** — If something fails, she reports the error exactly. No hidden failures.

## License

This project is provided as-is. Use it with honor.

---

*"Life before death. Strength before weakness. Journey before destination."*
