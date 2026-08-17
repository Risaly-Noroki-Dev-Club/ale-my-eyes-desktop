#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

if [[ "$(uname -s)" != "Linux" ]]; then
    echo "package-linux.sh must run on Linux" >&2
    exit 1
fi

cargo build --release --locked -p ale-cli -p ale-gui

package_dir="$repo_root/ale-my-eyes-linux"
archive="$repo_root/ale-my-eyes-linux.tar.gz"
rm -rf "$package_dir" "$archive"
mkdir -p "$package_dir/bin" "$package_dir/share/applications" \
    "$package_dir/share/icons/hicolor/scalable/apps"
cp target/release/ale-cli target/release/ale-gui "$package_dir/bin/"
cp assets/icon.svg "$package_dir/share/icons/hicolor/scalable/apps/ale-my-eyes.svg"

cat > "$package_dir/share/applications/ale-my-eyes.desktop" <<'EOF'
[Desktop Entry]
Name=Ale, My Eyes!
Comment=Accessible voice and visual desktop assistant
Exec=ale-gui
Icon=ale-my-eyes
Terminal=false
Type=Application
Categories=Utility;Accessibility;
Keywords=accessibility;assistant;vision;speech;
EOF

cat > "$package_dir/run-gui.sh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
exec "$script_dir/bin/ale-gui" "$@"
EOF
chmod +x "$package_dir/run-gui.sh" "$package_dir/bin/ale-cli" "$package_dir/bin/ale-gui"

cat > "$package_dir/README.md" <<'EOF'
# Ale, My Eyes! Linux

Run `./run-gui.sh`, then configure the OpenAI-compatible endpoint in Settings. The app stores configuration in the current user's standard config directory under `ale-my-eyes`; it does not read a package-local config file.

For system installation, copy `bin/*` into a directory on `PATH`, the desktop file into `share/applications`, and the icon into the matching `share/icons` path.
EOF

tar -C "$repo_root" -czf "$archive" "$(basename "$package_dir")"
printf 'Linux package: %s\n' "$archive"
