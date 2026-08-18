#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

release_dir="$repo_root/release"
source_name="ale-my-eyes-source"
source_dir="$release_dir/$source_name"

rm -rf "$release_dir"
mkdir -p "$source_dir"

cp Cargo.toml Cargo.lock flake.nix flake.lock README.md LICENSE AGENTS.md "$source_dir/"
cp -R ale-core ale-cli ale-gui ale-modeld scripts vendor assets "$source_dir/"

cat > "$source_dir/BUILD.md" <<'EOF'
# Build

The desktop routes local and remote inference through the bundled ale-modeld process.

```bash
cargo check --workspace --locked
cargo build --release --locked -p ale-cli -p ale-gui -p ale-modeld
```

The offline source check uses `--no-default-features` because the optional sherpa runtime obtains platform-native archives during its build. A networked default build enables SenseVoice on supported targets.

Runtime configuration is created in the operating system user configuration directory under `ale-my-eyes/config.json`. API keys are stored through the system credential store; no package-local configuration file is used.

Local inference is experimental and must be enabled explicitly with the `local-inference` feature.
EOF

tar -C "$release_dir" -czf "$release_dir/$source_name.tar.gz" "$source_name"

mkdir -p "$release_dir/ale-my-eyes-quickstart"
cp README.md LICENSE "$release_dir/ale-my-eyes-quickstart/"
cat > "$release_dir/ale-my-eyes-quickstart/QUICKSTART.md" <<'EOF'
# Quick start

1. Install the package for your operating system.
2. Start `ale-gui`.
3. Open Settings and enter an API key for an OpenAI-compatible HTTPS endpoint.

The application creates its configuration in the current user's standard configuration directory. Do not create or edit a `config/config.json` beside the executable.
EOF
tar -C "$release_dir" -czf "$release_dir/ale-my-eyes-quickstart.tar.gz" ale-my-eyes-quickstart

printf 'Release archives created in %s\n' "$release_dir"
