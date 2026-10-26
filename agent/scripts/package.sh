#!/usr/bin/env bash
# Release packaging (v0.1): build the stripped release binary, run it
# through the standalone smoke checks, then produce
#   dist/iris-<version>-<target>.tar.gz   (binary + README + LICENSE)
#   dist/iris-<version>-<target>.tar.gz.sha256
# One source of truth for the release workflow's matrix builds and for
# local dry runs:  scripts/package.sh 0.1.0
set -euo pipefail

cd "$(dirname "$0")/.."

version="${1:?usage: package.sh <version> [target]}"
target="${2:-$(rustc -vV | sed -n 's/^host: //p')}"

cargo build --release -p iris

# The exact binary being shipped must pass the standalone smoke test
# (clean env, no key, no sidecar files) before it is packaged.
bash scripts/smoke.sh

name="iris-$version-$target"
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT

mkdir -p "$stage/$name"
cp target/release/iris "$stage/$name/iris"
cp ../README.md ../LICENSE "$stage/$name/"

mkdir -p dist
rm -f "dist/$name.tar.gz" "dist/$name.tar.gz.sha256"
tar -C "$stage" -czf "dist/$name.tar.gz" "$name"

# macOS runners ship shasum, not sha256sum — emit the same format.
if command -v sha256sum >/dev/null 2>&1; then
  (cd dist && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
else
  (cd dist && shasum -a 256 "$name.tar.gz" > "$name.tar.gz.sha256")
fi

echo "packaged: agent/dist/$name.tar.gz ($(wc -c < "dist/$name.tar.gz") bytes)"
cat "dist/$name.tar.gz.sha256"
