# ferro — task runner (stage 0.1 deliverable)
# Targets: fmt, lint, test, cov (plan §Phase 0.1)

default: help

help:
    @just --list

# Format all crates in check+apply mode
fmt:
    cargo fmt --all

# Clippy with -D warnings
lint:
    cargo clippy --all-targets --all-features -- -D warnings

# Full test suite (nextest if available, cargo test fallback)
test:
    #!/usr/bin/env bash
    set -euo pipefail
    if command -v cargo-nextest >/dev/null 2>&1 || cargo nextest --version >/dev/null 2>&1; then
      cargo nextest run --workspace --all-features
    else
      cargo test --workspace --all-features
    fi

# Coverage report (requires cargo-llvm-cov)
cov:
    cargo llvm-cov --workspace --all-features --html

# Run everything CI runs
ci: fmt lint test
