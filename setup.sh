#!/usr/bin/env bash
set -euo pipefail

# Syl Setup Script
# Checks prerequisites, builds the binary, and installs the `syl` command.

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

# --- Check Rust toolchain ---
if command -v cargo &>/dev/null; then
  rust_version=$(rustc --version 2>&1)
  pass "Rust toolchain installed ($rust_version)"
else
  fail "Rust toolchain not found"
  info "Install with: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
  info "Then: source \"\$HOME/.cargo/env\""
  errors=$((errors + 1))
fi

# --- Check Podman ---
if command -v podman &>/dev/null; then
  podman_version=$(podman --version 2>&1)
  pass "Podman installed ($podman_version)"

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
  key_preview="${ANTHROPIC_API_KEY:0:10}...${ANTHROPIC_API_KEY: -4}"
  pass "ANTHROPIC_API_KEY is set ($key_preview)"
else
  fail "ANTHROPIC_API_KEY is not set"

  api_key=""
  if [ -t 0 ]; then
    echo
    read -rp "  Enter your Anthropic API key (or press Enter to skip): " api_key
  fi

  if [ -n "$api_key" ]; then
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

# --- Build ---
echo
echo -e "${BOLD}Build${RESET}\n"

if command -v cargo &>/dev/null; then
  info "Building syl (release)..."
  if cargo build --release 2>&1; then
    pass "Build succeeded → target/release/syl"
  else
    fail "Build failed"
    errors=$((errors + 1))
  fi
else
  warn "Skipping build (cargo not found)"
fi

# --- Install `syl` command ---
echo
echo -e "${BOLD}Installing syl command${RESET}\n"

INSTALL_DIR="$HOME/.local/bin"
BINARY="$(pwd)/target/release/syl"

if [ ! -f "$BINARY" ]; then
  warn "Binary not found at $BINARY — skipping install (fix build errors first)"
else
  mkdir -p "$INSTALL_DIR"
  ln -sf "$BINARY" "$INSTALL_DIR/syl"
  pass "Symlink created: $INSTALL_DIR/syl → $BINARY"

  # Check if INSTALL_DIR is on PATH
  if echo "$PATH" | tr ':' '\n' | grep -qx "$INSTALL_DIR"; then
    pass "$INSTALL_DIR is on your PATH"
  else
    warn "$INSTALL_DIR is not on your PATH"

    shell_rc=""
    if [ -n "${ZSH_VERSION:-}" ] || [ "$(basename "$SHELL")" = "zsh" ]; then
      shell_rc="$HOME/.zshrc"
    elif [ -n "${BASH_VERSION:-}" ] || [ "$(basename "$SHELL")" = "bash" ]; then
      shell_rc="$HOME/.bashrc"
    fi

    if [ -n "$shell_rc" ] && [ -t 0 ]; then
      read -rp "  Add $INSTALL_DIR to PATH in $shell_rc? [Y/n]: " confirm
      confirm=${confirm:-Y}
      if [[ "$confirm" =~ ^[Yy]$ ]]; then
        echo "" >> "$shell_rc"
        echo "# Added by Syl setup" >> "$shell_rc"
        echo "export PATH=\"$INSTALL_DIR:\$PATH\"" >> "$shell_rc"
        pass "Added $INSTALL_DIR to PATH in $shell_rc"
        info "Run 'source $shell_rc' or open a new terminal, then type: syl"
      else
        info "Add it manually: export PATH=\"$INSTALL_DIR:\$PATH\""
      fi
    else
      info "Add to your shell config: export PATH=\"$INSTALL_DIR:\$PATH\""
    fi
  fi
fi

# --- Summary ---
echo
if [ $errors -eq 0 ]; then
  echo -e "${GREEN}${BOLD}Ready!${RESET} Run ${CYAN}syl${RESET} from this directory to start."
else
  echo -e "${YELLOW}${BOLD}$errors issue(s) found.${RESET} Fix them above, then re-run ${CYAN}./setup.sh${RESET}"
fi
echo
