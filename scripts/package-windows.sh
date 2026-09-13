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

effective_target=${windows_target:-$host}
if [[ "$effective_target" == *windows-gnu && "${ALLOW_LIMITED_WINDOWS:-0}" != 1 ]]; then
    echo "Full Windows packages require native MSVC. Set ALLOW_LIMITED_WINDOWS=1 for a clearly marked GNU validation build." >&2
    exit 1
fi
export CARGO_PROFILE_RELEASE_DEBUG=2
build_args=(--release --locked -p ale-cli -p ale-gui -p ale-modeld --bins)
binary_dir="$repo_root/target/release"
if [[ -n "$windows_target" ]]; then
    build_args+=(--target "$windows_target")
    binary_dir="$repo_root/target/$windows_target/release"
    printf 'Cross-building Windows package for %s from %s\n' "$windows_target" "$host"
fi

export ALE_BUILD_ID=$(python3 scripts/write-build-manifest.py --source-id)
build_log="$repo_root/target/windows-package-build-$effective_target.jsonl"
mkdir -p "$repo_root/target"
cargo build "${build_args[@]}" --message-format=json-render-diagnostics > "$build_log"

package_name=ale-my-eyes-windows
if [[ "$effective_target" == *windows-gnu ]]; then package_name=ale-my-eyes-windows-gnu; fi
package_dir="$repo_root/$package_name"
archive="$repo_root/$package_name.zip"
rm -rf "$package_dir" "$archive"
mkdir -p "$package_dir"
cp "$binary_dir/ale-cli.exe" "$binary_dir/ale-gui.exe" "$binary_dir/ale-modeld.exe" LICENSE "$package_dir/"
cp ale-gui/DIAGNOSTICS.md "$package_dir/DIAGNOSTICS.md"
cp "$binary_dir/ale-modeld-acceptance.exe" "$package_dir/"
# Sherpa's native runtime can be emitted beside the binaries or in Cargo build output.
if [[ "$effective_target" == *windows-msvc ]]; then
    for symbol in ale_gui.pdb ale_modeld.pdb ale_cli.pdb ale_modeld_acceptance.pdb; do
        test -s "$binary_dir/$symbol"
        cp "$binary_dir/$symbol" "$package_dir/"
    done
fi
python3 scripts/write-build-manifest.py --package "$package_dir" --target "$effective_target" --expected-source "$ALE_BUILD_ID" --build-log "$build_log"

cat > "$package_dir/start-gui.bat" <<'EOF'
@echo off
cd /d "%~dp0"
start "" "%~dp0ale-gui.exe"
exit /b
EOF

cat > "$package_dir/README.txt" <<'EOF'
Ale, My Eyes! Desktop

Run ale-gui.exe, then configure the OpenAI-compatible endpoint in Settings.
Settings includes a native model downloader and an Open desktop log folder button.
Diagnostic snapshots are written at startup and every 60 seconds to Desktop/Ale-My-Eyes-Logs.
Windows defaults to software rendering. Set SLINT_BACKEND explicitly to override it.
Hangs and crashes automatically produce local diagnostic dumps; Settings can disable this.
See DIAGNOSTICS.md for model requirements and logging details. Model weights are not included.
The GNU validation build cannot run local SenseVoice ASR. Full support requires the MSVC build.
See build-manifest.json for the build target, binary hashes and native acceptance status.
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
symbols_dir="$repo_root/target/windows-symbols/$effective_target"
rm -rf "$symbols_dir" "$repo_root/$package_name-symbols.zip"
mkdir -p "$symbols_dir"
cp "$package_dir/build-manifest.json" "$symbols_dir/"
cp "$binary_dir/ale-gui.exe" "$binary_dir/ale-modeld.exe" "$binary_dir/ale-cli.exe" "$binary_dir/ale-modeld-acceptance.exe" "$symbols_dir/"
if [[ "$effective_target" == *windows-msvc ]]; then cp "$package_dir/"*.pdb "$symbols_dir/"; fi
# Exact unstripped EXEs are kept for GNU DWARF; MSVC also includes matching PDBs.
if command -v 7z >/dev/null 2>&1; then
    7z a "$repo_root/$package_name-symbols.zip" "$symbols_dir/*" >/dev/null
elif command -v powershell.exe >/dev/null 2>&1; then
    powershell.exe -NoProfile -Command "Compress-Archive -Force -Path '$symbols_dir/*' -DestinationPath '$repo_root/$package_name-symbols.zip'"
else
    (cd "$symbols_dir" && zip -qr "$repo_root/$package_name-symbols.zip" .)
fi
printf 'Windows package: %s\n' "$archive"
