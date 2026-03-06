# Syl ✨

**An honorspren for your codebase.**

Syl is a lightweight AI agent harness written in [Babashka](https://babashka.org/) (Clojure). She summons a containerized Ubuntu environment via [Podman](https://podman.io/), connects to [Anthropic's Claude API](https://docs.anthropic.com/), and provides an interactive REPL where an AI assistant can read, write, and execute code safely inside an isolated container.

> *"I'm not a pet. I'm a honorspren. There's a difference."*

## How It Works

```
┌──────────────┐       ┌──────────────────┐       ┌──────────────────────┐
│  You (REPL)  │ ───▶  │  syl.bb (Agent)  │ ───▶  │  Claude API          │
│              │ ◀───  │                  │ ◀───  │  (claude-sonnet-4-6) │
└──────────────┘       └────────┬─────────┘       └──────────────────────┘
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
4. **Conversation Loop** — You chat with Syl in a terminal REPL. Your messages are sent to the Claude API.
5. **Tool Execution** — When Claude decides to run commands, Syl executes them inside the sandboxed container and feeds results back into the conversation. Parallel tool calls are supported — Claude can invoke multiple commands in a single response.
6. **Context Compaction** — When the conversation grows beyond `max-history` messages, Syl asks Claude to summarize older messages into a compact digest, preserving key decisions, file changes, and errors. The most recent `compact-keep` messages are kept verbatim alongside the summary. A role-boundary correction ensures the kept messages always start on a plain `user` message (not a `tool_result`), so the model never sees an invalid message sequence. If summarization fails, a simple trim fallback ensures the conversation continues gracefully.
7. **Safety** — Your code runs in an isolated, memory-limited (2GB) container. Nothing touches your host system directly. SSH keys are mounted read-only for extra protection.

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

Syl will summon her container, forward your git and SSH configuration, and greet you with a styled banner displaying the First Ideal:

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

You: _
```

Ask her anything — read files, write code, run tests, install packages, commit and push. She'll handle it with honor.

## Configuration

These top-level variables are defined at the top of `syl.bb` and can be adjusted:

| Variable          | Default          | Description                                                 |
| ----------------- | ---------------- | ----------------------------------------------------------- |
| `container-name`  | `syl-agent`      | Name of the Podman container                                |
| `model`           | `claude-sonnet-4-6` | Anthropic model to use. Override at runtime with the `SYL_MODEL` env var |
| `max-tool-output` | `8000`           | Max characters stored per tool result in history             |
| `max-history`     | `30`             | Max messages kept in the conversation sliding window         |
| `compact-keep`    | `10`             | Recent messages preserved verbatim during context compaction |

The following values are currently hardcoded inline and can be changed directly in the source:

| Value             | Location              | Default                                        | Description                   |
| ----------------- | --------------------- | ---------------------------------------------- | ----------------------------- |
| `max_tokens`      | `call-claude`         | `8096`                                         | Max tokens per Claude response|
| `memory`          | `start-container`     | `2g`                                           | Container memory limit        |
| `image`           | `start-container`     | `mcr.microsoft.com/devcontainers/base:ubuntu`  | Container base image          |

## Features

- **Anthropic Claude Integration** — Powered by `claude-sonnet-4-6` (default) via the Anthropic Messages API. The model can be overridden at runtime by setting the `SYL_MODEL` environment variable.
- **Splash Banner** — On startup, Syl displays a styled ASCII art banner featuring the First Ideal and a Shardblade glyph, rendered with ANSI color codes (cyan, gold, magenta).
- **Read-Only SSH Mount with Wrapper** — Your host `~/.ssh` is bind-mounted read-only into the container. On startup, keys are copied to `/tmp/.ssh` with corrected ownership (`root:root`) and permissions (`700`/`600`), and a lightweight wrapper script is installed at `/usr/local/bin/ssh` that forces all SSH invocations (including git-over-SSH) to use the clean copy. This solves the common UID mismatch problem on macOS where the host user's UID (e.g. 501) differs from the container's root.
- **Git Identity Forwarding** — Your host's `user.name` and `user.email` are applied inside the container via proper argument passing (no shell string interpolation).
- **Parallel Tool Execution** — Claude can issue multiple `run_bash` calls in a single response, and Syl executes them all, returning results together.
- **Graceful Timeouts** — Commands that exceed 60 seconds are terminated cleanly and reported as timed out.
- **Tool Output Truncation** — Long command outputs are automatically truncated to `max-tool-output` characters to keep the conversation context lean and prevent token bloat.
- **Context Compaction** — When the conversation exceeds `max-history` messages, older messages are summarized by Claude into a concise digest (preserving key decisions, file changes, and errors). The most recent `compact-keep` messages are kept verbatim, with a role-boundary correction that scans forward to ensure the kept window starts on a plain `user` message (not a `tool_result`). This prevents invalid message sequences in the API payload. If summarization fails, a simple trim fallback ensures the conversation continues gracefully.
- **Rich System Prompt** — Syl's personality, oaths, and operational rules are baked into the system instruction for consistent behavior.

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

SSH refuses to use key files it doesn't own. By mounting read-only and copying to a root-owned location, we get secure key access without modifying the host's files. The wrapper script ensures both direct `ssh` and `git push/pull` use the corrected keys transparently.

### Context Compaction Strategy

Long conversations are managed through intelligent summarization rather than naive truncation:

```
[msg 1] [msg 2] ... [msg 20] [msg 21] ... [msg 30]
 \_________________________/  \____________________/
          │                          │
    Summarized by Claude      Kept verbatim (compact-keep)
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

1. The message list is split: everything except the last `compact-keep` messages becomes the "old" batch, and the final `compact-keep` messages are the "keep" batch.
2. **Role-boundary correction** — The keep batch is scanned forward from its start, skipping any messages until a plain `user` message (with string content, not a `tool_result` array) is found. This ensures the kept window begins with a valid user turn, preventing malformed API payloads that would occur if a `tool_result` appeared without a preceding `assistant` tool-use message.
3. The old batch is sent to Claude with a summarization prompt that extracts key decisions, file modifications, command outcomes, and unresolved issues.
4. The summary replaces those messages as a synthetic `[Context summary]` user message, followed by an assistant acknowledgment, then the corrected keep batch — forming a coherent, role-alternating conversation for the API.

If the summarization API call fails, the fallback simply uses the trimmed keep batch so the conversation can continue without interruption.

## The Oaths

Syl operates under a strict code of honor:

1. **Action Over Words** — She uses her tools immediately rather than just describing what she *would* do.
2. **Verification** — After making changes, she confirms they exist and work.
3. **Honesty** — If something fails, she reports the error exactly. No hidden failures.

## License

This project is provided as-is. Use it with honor.

---

*"Life before death. Strength before weakness. Journey before destination."*
