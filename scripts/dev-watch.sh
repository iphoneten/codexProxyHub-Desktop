#!/usr/bin/env bash
set -euo pipefail

# 仅为本次开发进程选择工具链，不修改系统 xcode-select 设置。
if [[ "$(uname -s)" == "Darwin" ]] && ! xcrun --find clang >/dev/null 2>&1; then
  if [[ -z "${DEVELOPER_DIR:-}" ]] && \
    DEVELOPER_DIR=/Library/Developer/CommandLineTools xcrun --find clang >/dev/null 2>&1; then
    export DEVELOPER_DIR=/Library/Developer/CommandLineTools
    echo "Using Command Line Tools: $DEVELOPER_DIR"
  else
    echo "error: Apple developer tools are unavailable. Check DEVELOPER_DIR or run xcode-select --install." >&2
    exit 1
  fi
fi

command -v cargo >/dev/null 2>&1 || {
  echo "error: cargo not found. Install Rust first: https://rustup.rs/" >&2
  exit 1
}

if ! cargo watch --version >/dev/null 2>&1; then
  echo "error: cargo-watch not installed." >&2
  echo "install: cargo install cargo-watch" >&2
  exit 1
fi

cargo watch \
  --clear \
  --watch src \
  --watch Cargo.toml \
  --exec run
