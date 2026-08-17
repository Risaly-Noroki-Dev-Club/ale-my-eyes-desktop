#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"
./scripts/create-release.sh

extract_dir=$(mktemp -d)
trap 'rm -rf "$extract_dir"' EXIT
tar -C "$extract_dir" -xzf release/ale-my-eyes-source.tar.gz
source_dir="$extract_dir/ale-my-eyes-source"
test -f "$source_dir/Cargo.lock"
test -f "$source_dir/vendor/i-slint-backend-winit-1.16.1/Cargo.toml"
cargo metadata --manifest-path "$source_dir/Cargo.toml" --locked --offline --no-deps >/dev/null
cargo check --manifest-path "$source_dir/Cargo.toml" --workspace --locked --offline
