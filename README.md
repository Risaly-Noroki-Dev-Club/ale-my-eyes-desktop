# Ale, My Eyes! Desktop  

Ale, My Eyes! help Users understand your computer screen and carry out tasks through voice. Pair it with the companion app to ask questions, request actions, and review an operation plan before confirming execution.

## Quick start

With Rust and the required platform dependencies installed, run these commands from the desktop repository:

```bash
cargo build -p ale-gui -p ale-modeld --locked
cargo run -p ale-gui
```

Configure your models in Settings, pair your phone, and start a voice request. Local models require separate downloads; cloud use requires configuration and authorization.

## Development

Built with **Rust** and **Slint**:

- `ale-core` — engine and encrypted sessions
- `ale-gui` — desktop interface and automation
- `ale-cli` — command-line tools
- `ale-modeld` — local model scheduling

```bash
cargo test -p ale-core
```

The app is under active development and may misunderstand speech or screen content. Review proposed actions carefully.

[Android companion app](https://github.com/Risaly-Noroki-Dev-Club/ale-my-eyes-mobile)
