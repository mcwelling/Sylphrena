# Syl ✨

**An honorspren for your codebase.**

Syl is a lightweight AI agent harness written in [Babashka](https://babashka.org/) (Clojure). She summons a containerized Ubuntu environment via [Podman](https://podman.io/), connects to [Anthropic's Claude API](https://docs.anthropic.com/), and provides an interactive REPL where an AI assistant can read, write, and execute code safely inside an isolated container.

> *"I'm not a pet. I'm a honorspren. There's a difference."*

## How It Works

```
┌──────────────┐       ┌──────────────────┐       ┌────────────────────┐
│  You (REPL)  │ ───▶  │  syl.bb (Agent)  │ ───▶  │  Claude API        │
│              │ ◀───  │                  │ ◀───  │  (claude-opus-4-6) │
└──────────────┘       └────────┬─────────┘       └────────────────────┘
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
6. **Safety** — Your code runs in an isolated, memory-limited (2GB) container. Nothing touches your host system directly. SSH keys are mounted read-only for extra protection.

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

Syl will summon her container, forward your git and SSH configuration, and greet you with a prompt:

```
Summoning a more powerful Syl in: /your/project
[Syl Setup] Configuring SSH for git operations...
[Syl Setup] Configuring git identity as: Your Name your@email.com
--- Syl Harness Ready (Powered by Claude) ---

You: _
```

Ask her anything — read files, write code, run tests, install packages, commit and push. She'll handle it with honor.

## Configuration

These defaults live at the top of `syl.bb` and can be adjusted:

| Variable          | Default                                        | Description                                             |
| ----------------- | ---------------------------------------------- | ------------------------------------------------------- |
| `container-name`  | `syl-agent`                                    | Name of the Podman container                            |
| `model`           | `claude-opus-4-6`                              | Anthropic model to use                                  |
| `memory`          | `2g`                                           | Container memory limit                                  |
| `image`           | `mcr.microsoft.com/devcontainers/base:ubuntu`  | Container base image                                    |
| `max-tokens`      | `8096`                                         | Max tokens per Claude response                          |
| `max-tool-output` | `8000`                                         | Max characters stored per tool result in history         |
| `max-history`     | `30`                                           | Max messages kept in the conversation sliding window     |

## Features

- **Anthropic Claude Integration** — Powered by `claude-opus-4-6` via the Anthropic Messages API.
- **Read-Only SSH Mount with Wrapper** — Your host `~/.ssh` is bind-mounted read-only into the container. On startup, keys are copied to `/tmp/.ssh` with corrected ownership (`root:root`) and permissions (`700`/`600`), and a lightweight wrapper script is installed at `/usr/local/bin/ssh` that forces all SSH invocations (including git-over-SSH) to use the clean copy. This solves the common UID mismatch problem on macOS where the host user's UID (e.g. 501) differs from the container's root.
- **Git Identity Forwarding** — Your host's `user.name` and `user.email` are applied inside the container via proper argument passing (no shell string interpolation).
- **Parallel Tool Execution** — Claude can issue multiple `run_bash` calls in a single response, and Syl executes them all, returning results together.
- **Graceful Timeouts** — Commands that exceed 60 seconds are terminated cleanly and reported as timed out.
- **Tool Output Truncation** — Long command outputs are automatically truncated to `max-tool-output` characters to keep the conversation context lean and prevent token bloat.
- **Conversation History Trimming** — A sliding window (`max-history`) keeps only the most recent messages, always trimming to start on a clean user message so the conversation remains coherent for the model.
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

## The Oaths

Syl operates under a strict code of honor:

1. **Action Over Words** — She uses her tools immediately rather than just describing what she *would* do.
2. **Verification** — After making changes, she confirms they exist and work.
3. **Honesty** — If something fails, she reports the error exactly. No hidden failures.

## License

This project is provided as-is. Use it with honor.

---

*"Life before death. Strength before weakness. Journey before destination."*
