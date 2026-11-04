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

# rustup 可能已安装，但新终端尚未加载 Cargo 的 PATH；只影响本脚本及其子进程。
if ! command -v cargo >/dev/null 2>&1; then
  cargo_home="${CARGO_HOME:-$HOME/.cargo}"
  if [[ -f "$cargo_home/env" ]]; then
    source "$cargo_home/env"
  fi
  if ! command -v cargo >/dev/null 2>&1 && [[ -x "$cargo_home/bin/cargo" ]]; then
    export PATH="$cargo_home/bin:$PATH"
  fi
fi

command -v cargo >/dev/null 2>&1 || {
  echo "error: cargo not found. Install Rust first: https://rustup.rs/" >&2
  exit 1
}

if ! cargo watch --version >/dev/null 2>&1; then
  echo "error: cargo-watch not installed." >&2
  echo "install: \"$(command -v cargo)\" install cargo-watch --locked" >&2
  exit 1
fi

# 支持在项目根目录或 scripts 目录中运行，避免监听到错误的 src 路径。
project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_root"

cargo watch \
  --clear \
  --watch src \
  --watch Cargo.toml \
  --exec run
