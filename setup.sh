#!/usr/bin/env bash
set -euo pipefail

# Syl Setup Script
# Checks for required dependencies and helps configure the environment.

BOLD="\033[1m"
GREEN="\033[32m"
YELLOW="\033[33m"
RED="\033[31m"
CYAN="\033[96m"
RESET="\033[0m"

pass() { echo -e "  ${GREEN}✓${RESET} $1"; }
fail() { echo -e "  ${RED}✗${RESET} $1"; }
warn() { echo -e "  ${YELLOW}!${RESET} $1"; }
info() { echo -e "  ${CYAN}→${RESET} $1"; }

echo
echo -e "${BOLD}Syl Setup${RESET}"
echo -e "Checking prerequisites...\n"

errors=0

# --- Detect OS ---
OS="unknown"
case "$(uname -s)" in
  Darwin*) OS="macos" ;;
  Linux*)  OS="linux" ;;
esac

# --- Check Babashka ---
if command -v bb &>/dev/null; then
  bb_version=$(bb --version 2>&1 | head -1)
  pass "Babashka installed ($bb_version)"
else
  fail "Babashka not found"
  if [ "$OS" = "macos" ]; then
    info "Install with: brew install borkdude/brew/babashka"
  else
    info "Install with: bash < <(curl -s https://raw.githubusercontent.com/babashka/babashka/master/install)"
  fi
  errors=$((errors + 1))
fi

# --- Check Podman ---
if command -v podman &>/dev/null; then
  podman_version=$(podman --version 2>&1)
  pass "Podman installed ($podman_version)"

  # Check Podman machine (macOS only)
  if [ "$OS" = "macos" ]; then
    if podman machine inspect &>/dev/null; then
      machine_state=$(podman machine inspect --format '{{.State}}' 2>/dev/null || echo "unknown")
      if [ "$machine_state" = "running" ]; then
        pass "Podman machine is running"
      else
        warn "Podman machine exists but is not running"
        info "Start with: podman machine start"
        errors=$((errors + 1))
      fi
    else
      fail "No Podman machine found (required on macOS)"
      info "Initialize with: podman machine init && podman machine start"
      errors=$((errors + 1))
    fi
  fi
else
  fail "Podman not found"
  if [ "$OS" = "macos" ]; then
    info "Install with: brew install podman"
    info "Then run: podman machine init && podman machine start"
  else
    info "Install with: sudo apt-get install -y podman"
  fi
  errors=$((errors + 1))
fi

# --- Check Git ---
if command -v git &>/dev/null; then
  git_version=$(git --version 2>&1)
  pass "Git installed ($git_version)"

  # Check git identity
  git_name=$(git config --global user.name 2>/dev/null || echo "")
  git_email=$(git config --global user.email 2>/dev/null || echo "")
  if [ -n "$git_name" ] && [ -n "$git_email" ]; then
    pass "Git identity configured ($git_name <$git_email>)"
  else
    warn "Git identity not fully configured"
    info "Set with: git config --global user.name \"Your Name\""
    info "          git config --global user.email \"you@example.com\""
  fi
else
  fail "Git not found"
  errors=$((errors + 1))
fi

# --- Check ANTHROPIC_API_KEY ---
echo
echo -e "${BOLD}API Configuration${RESET}\n"

if [ -n "${ANTHROPIC_API_KEY:-}" ]; then
  # Show first 10 and last 4 chars
  key_preview="${ANTHROPIC_API_KEY:0:10}...${ANTHROPIC_API_KEY: -4}"
  pass "ANTHROPIC_API_KEY is set ($key_preview)"
else
  fail "ANTHROPIC_API_KEY is not set"

  # Only prompt interactively if stdin is a terminal
  api_key=""
  if [ -t 0 ]; then
    echo
    read -rp "  Enter your Anthropic API key (or press Enter to skip): " api_key
  fi

  if [ -n "$api_key" ]; then
    # Detect shell config file
    shell_rc=""
    if [ -n "${ZSH_VERSION:-}" ] || [ "$(basename "$SHELL")" = "zsh" ]; then
      shell_rc="$HOME/.zshrc"
    elif [ -n "${BASH_VERSION:-}" ] || [ "$(basename "$SHELL")" = "bash" ]; then
      shell_rc="$HOME/.bashrc"
    fi

    if [ -n "$shell_rc" ]; then
      read -rp "  Add to $shell_rc? [Y/n]: " confirm
      confirm=${confirm:-Y}
      if [[ "$confirm" =~ ^[Yy]$ ]]; then
        echo "" >> "$shell_rc"
        echo "# Anthropic API key for Syl" >> "$shell_rc"
        echo "export ANTHROPIC_API_KEY=\"$api_key\"" >> "$shell_rc"
        pass "Added ANTHROPIC_API_KEY to $shell_rc"
        info "Run 'source $shell_rc' or open a new terminal to apply"
        export ANTHROPIC_API_KEY="$api_key"
      else
        warn "Skipped. Set it manually:"
        info "export ANTHROPIC_API_KEY=\"$api_key\""
      fi
    else
      warn "Could not detect shell config file. Set it manually:"
      info "export ANTHROPIC_API_KEY=\"$api_key\""
    fi
  else
    warn "You'll need to set ANTHROPIC_API_KEY before running Syl."
    info "Get a key at: https://console.anthropic.com/"
    info "Then run: export ANTHROPIC_API_KEY=\"sk-ant-...\""
    errors=$((errors + 1))
  fi
fi

# --- Check AWS Credentials (for Bedrock backend) ---
echo
echo -e "${BOLD}AWS Configuration (optional — for Bedrock backend)${RESET}\n"

if [ -n "${AWS_ACCESS_KEY_ID:-}" ] && [ -n "${AWS_SECRET_ACCESS_KEY:-}" ]; then
  key_preview="${AWS_ACCESS_KEY_ID:0:6}...${AWS_ACCESS_KEY_ID: -4}"
  region="${AWS_REGION:-${AWS_DEFAULT_REGION:-us-east-1}}"
  pass "AWS credentials are set ($key_preview)"
  pass "AWS region: $region"
  if [ -n "${AWS_SESSION_TOKEN:-}" ]; then
    pass "AWS session token is set (temporary credentials)"
  fi
else
  info "AWS credentials not set (only needed for --backend bedrock)"
  info "To use Bedrock, set: AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY, AWS_REGION"
fi

# --- Pull container image ---
echo
echo -e "${BOLD}Container Image${RESET}\n"

image="mcr.microsoft.com/devcontainers/base:ubuntu"
if command -v podman &>/dev/null; then
  if podman image exists "$image" 2>/dev/null; then
    pass "Container image already pulled ($image)"
  else
    info "Pulling container image (this may take a minute)..."
    if podman pull "$image" &>/dev/null; then
      pass "Container image pulled successfully"
    else
      fail "Failed to pull container image"
      info "You can try manually: podman pull $image"
      errors=$((errors + 1))
    fi
  fi
else
  warn "Skipping image pull (Podman not installed)"
fi

# --- Make syl.bb executable ---
if [ -f "./syl.bb" ]; then
  chmod +x ./syl.bb
  pass "syl.bb is executable"
fi

# --- Summary ---
echo
if [ $errors -eq 0 ]; then
  echo -e "${GREEN}${BOLD}Ready!${RESET} Run ${CYAN}./syl.bb${RESET} to start Syl."
else
  echo -e "${YELLOW}${BOLD}$errors issue(s) found.${RESET} Fix them above, then run ${CYAN}./syl.bb${RESET}"
fi
echo
