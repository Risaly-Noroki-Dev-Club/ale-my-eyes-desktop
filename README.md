# Ale, My Eyes! Desktop

Desktop application for Ale, My Eyes!, providing continuous voice interaction, screen understanding, and guarded desktop automation on Windows and Linux/NixOS.

This repository is intentionally independent from the mobile application. It owns its GUI, desktop runtime, CLI, and a private copy of `ale-core`; it has no source or Cargo dependency on `ale-my-eyes-mobile`.

## Supported inference

The production desktop build uses the OpenAI-compatible cloud transport. Custom endpoints must implement that protocol; provider labels such as Anthropic, Google, or Azure do not enable their native APIs.

Local ASR and ONNX image description are experimental and available only when explicitly built with `ale-core/local-inference`. Local text generation, visual question answering, automation planning, and automatic cloud/local fallback are not production-supported and are not exposed in the standard desktop UI.

## Development

```bash
cargo run -p ale-gui
cargo run -p ale-cli -- status
cargo test -p ale-core
```

## Nix and NixOS

The Flake provides packages for `x86_64-linux` and `aarch64-linux`, GUI and CLI apps, a development shell, and `nixosModules.default`.

```bash
nix run .
nix run .#cli -- status
nix develop
```

The NixOS module only installs the selected package:

```nix
programs.ale-my-eyes.enable = true;
```

It does not create a service, enable autostart, or place API credentials in the Nix store. macOS packaging and support have been removed.

The desktop remote server implements protocol v2 for the Android arm64 client. Protocol v1 peers are rejected without fallback.

See the mobile client at https://github.com/Risaly-Noroki-Dev-Club/ale-my-eyes-mobile.
