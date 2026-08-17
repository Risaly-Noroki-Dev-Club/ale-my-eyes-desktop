#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

archive=${1:-$repo_root/ale-my-eyes-windows.zip}
package_dir=${2:-$repo_root/ale-my-eyes-windows}
objdump=${WINDOWS_OBJDUMP:-x86_64-w64-mingw32-objdump}
strings_tool=${WINDOWS_STRINGS:-x86_64-w64-mingw32-strings}

for path in "$archive" "$package_dir/ale-gui.exe" "$package_dir/ale-cli.exe" \
    "$package_dir/ale-modeld.exe" \
    "$package_dir/start-gui.bat" "$package_dir/README.txt" "$package_dir/LICENSE"; do
    test -s "$path"
done
test ! -e "$package_dir/config"
test ! -e "$package_dir/config.json"

unzip -t "$archive" >/dev/null
if unzip -Z1 "$archive" | rg -q '(^|/)config(/|\.json$)'; then
    echo "Windows archive must not contain package-local configuration" >&2
    exit 1
fi

file "$package_dir/ale-gui.exe" | rg -q 'PE32\+ executable \(GUI\) x86-64'
file "$package_dir/ale-cli.exe" | rg -q 'PE32\+ executable \(console\) x86-64'
command -v "$objdump" >/dev/null
command -v "$strings_tool" >/dev/null
gui_headers=$("$objdump" -p "$package_dir/ale-gui.exe")
cli_headers=$("$objdump" -p "$package_dir/ale-cli.exe")
gui_sections=$("$objdump" -h "$package_dir/ale-gui.exe")
resource_tree=$("$objdump" -x "$package_dir/ale-gui.exe" \
    | sed -n '/The \.rsrc Resource Directory section:/,/Sections:/p')
resource_strings=$("$strings_tool" -el "$package_dir/ale-gui.exe")
rg -q 'Subsystem.*Windows GUI' <<<"$gui_headers"
rg -q 'Subsystem.*Windows CUI' <<<"$cli_headers"
rg -q '\.rsrc' <<<"$gui_sections"
for resource_id in 0x000003 0x00000e 0x000010; do
    rg -q "Entry: ID: $resource_id" <<<"$resource_tree"
done
rg -q 'Ale, My Eyes! Desktop Assistant' <<<"$resource_strings"
rg -q 'ProductName' <<<"$resource_strings"

if rg -qi 'DLL Name: (libgcc|libstdc\+\+|libwinpthread)' \
    <<<"$gui_headers"$'\n'"$cli_headers"; then
    echo "Windows package depends on an unbundled MinGW runtime DLL" >&2
    exit 1
fi

rg -q '^cd /d "%~dp0"$' "$package_dir/start-gui.bat"
rg -q '^ale-gui\.exe$' "$package_dir/start-gui.bat"
if rg -qi 'start-server|config/config\.json' "$package_dir"; then
    echo "Windows package contains stale startup or configuration guidance" >&2
    exit 1
fi

printf 'Windows package verification passed: %s\n' "$archive"
