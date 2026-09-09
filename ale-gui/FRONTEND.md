# Desktop Frontend

The Slint desktop interface implements the three supplied Figma pages:
pairing (`0:1`, frame `1:2`), work (`12:32`, content `12:53`), and settings
(`12:238`, content `12:259`) in file `cxaBufUY2jTxJt1E50MUUj`.
The default window is 1032 by 800 logical pixels, with a 440 by 640 minimum.

## Runtime Behavior

- Pairing displays a real QR code, code, expiry, and connection state. Refresh
  rotates the credentials for new connections without restarting authenticated
  sessions. Only a valid encrypted protocol-v3 ClientHello unlocks Work.
- The phone supplies voice input. The desktop has no conversation input and
  does not automatically start its microphone.
- Work shows authenticated devices, measured WebSocket round-trip latency,
  current task, full confirmation text, action risk, model health, and bounded
  activity history. Unavailable latency and model state remain explicitly unknown.
- Pause cancels processing, uploads, pending decisions, and cooperative execution
  while preserving the connection. Resume accepts new requests; it does not replay
  cancelled work. A native action already in progress can finish before its next
  cancellation checkpoint.
- Desktop and phone confirmations use the same server validation. Expired or
  consumed requests cannot be executed again. Phone cancellation clears the
  corresponding desktop confirmation.
- Settings support provider selection, an explicit wire protocol, a custom endpoint/model, capability testing,
  request limits, Chinese/English, speech output, and high contrast. Testing uses
  the draft without saving it. Leaving an edited settings page prompts to save,
  discard, or continue editing. API keys use the existing system credential store.
- Advanced settings configure an explicitly authorized backup. Cloud transcription
  has its own endpoint/key/model. Capability tests require cost confirmation, use
  only built-in samples, and can be cancelled. See [Model Calling](../MODEL-CALLING.md)
  for protocol mapping, deadlines, migration, and persistence behavior.
- Pairing and settings suspend screen capture to protect credentials. API key
  reveal automatically ends after ten seconds or navigation.

`src/desktop_ui.rs` owns UI callbacks and rendering, `src/ui_state.rs` receives
remote session events, and `src/ui_text.rs` localizes runtime status labels.
`ui/state.slint` is the UI contract. Model health adds an optional
`sensevoice_state` field to the local scheduler IPC response; phone protocol
messages are unchanged. Builds without `ALE_BUILD_DATE` display an unknown build
date instead of a fixture date.

## Local Verification

```sh
cargo fmt --all -- --check
cargo check --workspace --locked
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo build -p ale-gui -p ale-modeld --locked
cargo run -p ale-gui
```

For deterministic visual inspection without microphone access, user settings,
credentials, or network services:

```sh
cargo build -p ale-gui --example ui_preview --locked
SLINT_BACKEND=winit-software target/debug/examples/ui_preview
SLINT_BACKEND=winit-software target/debug/examples/ui_preview --interactive
SLINT_BACKEND=winit-software target/debug/examples/ui_preview --model-settings
```

The fixture runner writes first-screen and scrolled screenshots for all three
pages under `target/ui-preview/`, covering 1032x800, 1280x900, and 440x640,
plus English with a dark high-contrast palette. It exits after taking the
screenshots. Fixture values, including QR codes, are not production state.
The interactive fixture supports navigation, pause, and confirmation only;
settings persistence and connection testing belong to the real application.

Phone testing was reported complete by the user. This desktop change was checked
locally on macOS; these checks do not establish Windows/Linux native model or
screen-reader acceptance. The real-display capture test remains opt-in because
it requires screen-recording permission. Slint's current Parley/ICU dependency
prints a missing Chinese/Japanese segmentation-model diagnostic in debug visual
runs; text and wrapping were visually inspected, without changing vendored code
or suppressing the diagnostic.
