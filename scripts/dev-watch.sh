#!/usr/bin/env bash
set -euo pipefail

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
