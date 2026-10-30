#!/usr/bin/env bash
# Single-binary smoke test (stage 7.4). Proves the stripped release
# binary is self-contained: copied ALONE into a temp dir and run with a
# completely clean environment (env -i) — no sidecar files, no ambient
# IRIS_* overrides, no API key, no network. Used by `just smoke` and by
# the GitHub Actions `smoke` job, so the check has one source of truth.
set -euo pipefail

cd "$(dirname "$0")/.."

cargo build --release -p iris

# Windows builds emit iris.exe — copy and invoke by the real file name.
bin="target/release/iris"
if [ -f "$bin.exe" ]; then bin="$bin.exe"; fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
cp "$bin" "$tmp/"

# env -i: the empty environment — only PATH/HOME (a fresh temp HOME, so
# even a developer's real config can't leak in) and the config pointer.
run() {
  env -i PATH="$PATH" HOME="$tmp" IRIS_CONFIG=/nonexistent/smoke "$tmp/$(basename "$bin")" "$@"
}

version="$(run --version)"
run --help >/dev/null
out="$(run doctor)"

echo "$out" | grep -q "telemetry"      # privacy report present
echo "$out" | grep -q "not set"        # works with no key at all

size="$(wc -c < "$bin")"
echo "smoke ok — $version, single standalone binary, $size bytes"
