# Syl ✨

**An honorspren for your codebase.**

Syl is a lightweight AI agent written in Rust. She spins up a sandboxed Ubuntu container via Podman, connects to Claude through the Anthropic API with real-time streaming, and drops you into an interactive terminal REPL where the assistant can read, write, and execute code safely.

> *"I'm not a pet. I'm a honorspren. There's a difference."*

```
┌──────────────┐       ┌──────────────────┐  streaming  ┌──────────────────────────┐
│  You (REPL)  │ ───▶  │  syl  (Rust)     │ ◀═══════▶  │  Claude API              │
│              │ ◀───  │                  │     SSE     │  (Anthropic)             │
└──────────────┘       └────────┬─────────┘             └──────────────────────────┘
                                │ tool calls
                                ▼
                       ┌──────────────────┐
                       │  Podman Container │
                       │  (Ubuntu, 2 GB)  │
                       │  /workspace      │
                       └──────────────────┘
```

## Prerequisites

| | macOS | Linux |
|---|---|---|
| **Rust + Cargo** | [rustup.rs](https://rustup.rs) | [rustup.rs](https://rustup.rs) |
| **Podman** | `brew install podman && podman machine init && podman machine start` | `sudo apt install podman` |
| **Git** | pre-installed | pre-installed |

Set `ANTHROPIC_API_KEY`.

## Quick Start

```bash
# 1. Check deps, build, and install the syl command
./setup.sh

# 2. Export your API key if setup.sh didn't add it
export ANTHROPIC_API_KEY="sk-ant-..."

# 3. Run from your project directory
syl
```

On first run Syl pulls an Ubuntu base image, installs dev tools, and bakes a cached image (`syl-agent-ready:latest`) so subsequent startups are fast. Available Claude models are fetched from the API on every startup — the picker is always current.

## CLI Options

| Flag | Default | Description |
|---|---|---|
| `--model <name>` | `claude-opus-5-5` | Starting model (switch live with `/model`) |
| `--rate-limit <n>` | `60000` | Token-per-minute budget |
| `--timeout <secs>` | `120` | Request timeout |
| `--thinking <n>` | — | Extended thinking token budget. On adaptive-thinking models (Opus 4.7+, all 5.x models, including the default) the budget is ignored and Syl streams summarized thinking instead |
| `--image <mode>` | `auto` | `kitty` / `iterm2` / `sixel` / `symbols` |
| `--continue`, `-c` | — | Continue the most recent session that has messages |
| `--resume [id]` | — | Resume a session by id, id prefix, number from `/sessions`, or name (the most recent without one) |
| `--fresh` | — | Nuke the container and start clean |
| `--no-banner` | — | Skip the startup portrait |
| `--light` | — | Light terminal colour theme |
| `--help` | — | Show this help |

**Environment overrides:** `SYL_MODEL`, `SYL_RATE_LIMIT`, `SYL_THEME`, `SYL_IMAGE`

**Per-project config** (lowest priority): `.syl/config.toml`
```toml
model      = "claude-sonnet-4-6"
rate_limit = 40000
image      = "kitty"

# Ask before some tool calls (optional; the last matching rule wins)
[[permission]]
tool = "run_bash"
pattern = "git push*"
action = "ask"          # allow | ask | deny

# Let a cheap model approve "ask" calls; if it doesn't, you're asked (optional)
[reviewer]
model = "claude-haiku-4-5"

# Container limits (optional)
[container]
memory = "4g"
cpus = "2"
# network = "none"

# MCP servers (optional; tools appear as <name>__<tool>). Stdio servers run in
# the container. `host = true` and remote `url` servers are only honoured in
# ~/.syl/config.toml, since a project's config comes with the project. A
# project's hooks and servers run only once you trust them (Syl asks).
[[mcp]]
name = "github"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_TOKEN = "..." }

# Hooks (optional; run in the container with the event as JSON on stdin;
# a failing pre_tool hook blocks the call)
[[hook]]
event = "pre_tool"
tool = "run_bash"
command = "./scripts/check-command.sh"

# Context compaction (optional)
context_window      = 1000000  # override the model's window, in tokens
compact_buffer      = 0.15     # compact when within 15% of the window
compact_keep_tokens = 40000    # recent history kept verbatim
```

Priority: **CLI flag > environment variable > `.syl/config.toml` > default**

## Slash Commands

| Command | Description |
|---|---|
| `/model [name]` | Interactive picker (↑↓ / jk + Enter), or direct: `/model sonnet` |
| `/status` | Model, timeout, rate limit, token counts, cost |
| `/tools` | The last turn's tool calls in full: output and diffs |
| `/undo` | Put files back as they were before the last turn that changed them (also Ctrl+Z) |
| `/checkpoints`, `/restore <n>` | List checkpoints, or go back to one |
| `/receipts [n]` | What the last turns edited, ran, fetched and were allowed |
| `/plan`, `/build` | Plan mode (Syl can't change files) or build mode; Shift+Tab toggles |
| `/sessions` | List saved sessions (`*` marks the current one) |
| `/resume <n\|id\|name>` | Switch to a saved session |
| `/history` | Prompts in this session, numbered for `/fork` |
| `/fork [n]` | Continue in a copy of this session, from just before prompt *n* |
| `/name <name>` | Name this session |
| `/memory [key]` | List persistent memory keys or show one |
| `/workflow [name]` | List pipelines or run one |
| `/log` | Last 20 lines of `.syl/syl.log` |
| `/clear` | Start a new session (container is preserved; the old one stays in `/sessions`) |
| `/help` | Show all commands |
| `/exit` / `/quit` | Shut down Syl and remove the container |

## Tools

Claude has these built-in tools, all running inside the container:

| Tool | Description |
|---|---|
| `run_bash` | Execute shell commands (long output is saved to a file the model can page) |
| `read_file` | Read numbered lines of a file, in windows |
| `edit_file` | Exact string replacement (must be unique unless `replace_all`); shows a diff |
| `write_file` | Create or replace a file; shows a diff |
| `grep` | Search contents with ripgrep (or grep in older images) |
| `glob` | Find files by pattern, e.g. `src/**/*.rs` |
| `ls` | List a directory |
| `memory_read` / `memory_write` | Persistent key-value notes in `.syl/memory/*.md` |
| `workflow_write` / `workflow_delete` | Create or remove named workflow pipelines |
| `dispatch_agent` | Fetch a URL and process it with a fast Haiku sub-agent |
| `show_image` | Render an image in the terminal |

Read-only calls (`read_file`, `grep`, `glob`, `ls`, `memory_read`) made together run in parallel.

In the terminal, your messages sit on the right and Syl's replies on the left, and the conversation scrolls back in your terminal's own scrollback. Tool calls show on a single line under the status bar that each call overwrites (`✓ edit_file › src/main.rs  +3 -1`), so they don't crowd the conversation. `/tools` shows the last turn's calls in full, with output and diffs.

**Project instructions:** `AGENTS.md` (or `CLAUDE.md` where there is no `AGENTS.md`) in the project and every directory above it is loaded into the system prompt when a conversation starts; the project's own file comes last.

**Images:** name an image in a prompt (`@screenshot.png`, or drag a file into the terminal) and it is sent with the prompt. PNG, JPEG, GIF and WebP, up to 3.75 MB each and 15 MB per prompt.

**Skills and commands:** `SKILL.md` skills in `.syl/skills/`, `.agents/skills/` or `~/.syl/skills/` are listed to Syl, who loads one when a task calls for it (`/skill:name` loads one yourself). Markdown files in `.syl/commands/` (or `~/.syl/commands/`) become slash commands: `/review error handling` sends `review.md` with `$ARGUMENTS` filled in.

**Recipes:** workflows can take parameters (`params: version` front matter, `{{version}}` in steps), retry a command (`(retry 2)`), and hand a step to Syl (`- Changelog: ask: update CHANGELOG.md for {{version}}`). Run one with `/workflow release 1.4.0` (quote a value with spaces: `message="fix the build"`). Ctrl+C stops a running step.

**Steering:** while Syl works, type on the bottom line and press Enter. The message reaches the model after the tool calls running now, together with their results. If the turn ends first, it becomes the next prompt. Ctrl+C cancels the turn; at the prompt it clears the line (Ctrl+D or `/exit` quits).

## Common Patterns

```bash
syl                          # standard session
syl --thinking 8000          # show Claude's thinking as it streams
syl -p "run the tests and fix what fails"   # one turn, no UI; the answer goes to stdout
syl -p "summarise the diff" --output json   # every event as a JSON line, for scripts
syl acp                      # an Agent Client Protocol agent, for Zed or Bondsmith
syl --continue               # continue the most recent session
syl --resume 20261122-1530   # resume a particular one
syl --fresh                  # clean slate (new container)
syl --no-banner --model claude-haiku-4-5   # quick, lightweight session
```

**Switch models mid-session:**
```
/model              → interactive picker with all API-available models
/model sonnet       → switch by substring (id or display name)
/model claude-opus-4-7-20260601   → use any raw model ID directly
```

**Workflows** — define multi-step pipelines in `.syl/workflows/`:
```markdown
# Ship
Test and push to origin.

- Run tests: `cargo test`
- Stage: `git add -A`
- Commit: `git commit -m "chore: release"`
- Push: `git push`
```
Run with `/workflow ship`.

**Persistent memory** — Syl stores key notes across sessions in `.syl/memory/*.md`. All entries are auto-injected into every new session's system prompt so she remembers your project context, preferences, and past decisions.

## The Oaths

1. **Action over words** — She uses her tools immediately rather than describing what she *would* do.
2. **Verification** — After changes, she confirms they work.
3. **Honesty** — If something fails, she reports the exact error.
4. **Transparency** — Before modifying files, she shows the diff.

---

→ **[Architecture & Internals](docs/architecture.md)** — streaming, compaction, rate limiting, SSH forwarding, container lifecycle, model discovery, status bars

MIT License · *"Life before death. Strength before weakness. Journey before destination."*
