#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

host=$(rustc -vV | sed -n 's/^host: //p')
windows_target=${WINDOWS_TARGET:-}
if [[ "$host" != *windows* && -z "$windows_target" ]]; then
    echo "Run on Windows, or set WINDOWS_TARGET to an installed Windows Rust target" >&2
    exit 1
fi
if [[ -n "$windows_target" && "$windows_target" != *windows* ]]; then
    echo "WINDOWS_TARGET must name a Windows Rust target" >&2
    exit 1
fi

build_args=(--release --locked -p ale-cli -p ale-gui -p ale-modeld)
binary_dir="$repo_root/target/release"
if [[ -n "$windows_target" ]]; then
    build_args+=(--target "$windows_target")
    binary_dir="$repo_root/target/$windows_target/release"
    printf 'Cross-building Windows package for %s from %s\n' "$windows_target" "$host"
fi

cargo build "${build_args[@]}"

package_dir="$repo_root/ale-my-eyes-windows"
archive="$repo_root/ale-my-eyes-windows.zip"
rm -rf "$package_dir" "$archive"
mkdir -p "$package_dir"
cp "$binary_dir/ale-cli.exe" "$binary_dir/ale-gui.exe" "$binary_dir/ale-modeld.exe" LICENSE "$package_dir/"

cat > "$package_dir/start-gui.bat" <<'EOF'
@echo off
cd /d "%~dp0"
ale-gui.exe
EOF

cat > "$package_dir/README.txt" <<'EOF'
Ale, My Eyes! Desktop

Run start-gui.bat, then configure the OpenAI-compatible endpoint in Settings.
The app stores configuration in the current Windows user configuration directory under ale-my-eyes. It does not use a config directory beside the executable.

Run `ale-cli.exe status` from Command Prompt to inspect the active configuration.
EOF

if command -v 7z >/dev/null 2>&1; then
    7z a "$archive" "$package_dir" >/dev/null
elif command -v powershell.exe >/dev/null 2>&1; then
    powershell.exe -NoProfile -Command \
        "Compress-Archive -Force -Path '$package_dir' -DestinationPath '$archive'"
elif command -v zip >/dev/null 2>&1; then
    zip -qr "$archive" "$(basename "$package_dir")"
else
    echo "7z, PowerShell, or zip is required to create the Windows archive" >&2
    exit 1
fi
printf 'Windows package: %s\n' "$archive"
