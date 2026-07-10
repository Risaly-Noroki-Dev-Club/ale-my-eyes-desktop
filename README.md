# Ale, My Eyes! Desktop

Desktop application for Ale, My Eyes!, providing continuous voice interaction, screen understanding, and guarded desktop automation on macOS, Windows, and Linux.

This repository is intentionally independent from the mobile application. It owns its GUI, desktop runtime, CLI, and a private copy of `ale-core`; it has no source or Cargo dependency on `ale-my-eyes-mobile`.

## Development

```bash
cargo run -p ale-gui
cargo run -p ale-cli -- status
cargo test -p ale-core
```

See the mobile client at https://github.com/Risaly-Noroki-Dev-Club/ale-my-eyes-mobile.
