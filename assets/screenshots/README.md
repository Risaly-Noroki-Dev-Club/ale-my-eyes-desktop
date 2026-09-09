# README Screenshots

These PNGs are native Slint snapshots captured on 2026-09-09. All device addresses,
pairing credentials, QR codes, task descriptions, timings, model states, and probe
results are deterministic fixtures from `ale-gui/examples/ui_preview.rs`.
They contain no real API keys or user sessions and do not certify provider support.
Compact screenshots depict a narrow desktop window, not the Android client.

| Published asset | Source under `target/ui-preview/` |
| --- | --- |
| `work.png` | `main-1032-800-false.png` |
| `pairing.png` | `pairing-1032-800-false.png` |
| `settings.png` | `models-1032-false-1.png` |
| `backup-and-capabilities.png` | `models-1032-false-3.png` |
| `transcription-high-contrast.png` | `models-1032-true-5.png` |
| `capability-confirmation-compact.png` | `models-440-true-7.png` |
| `work-high-contrast.png` | `main-1032-800-true-bottom.png` |

Regenerate snapshots from the repository root using a native graphical session:

```sh
cargo build -p ale-gui --example ui_preview --locked
SLINT_BACKEND=winit-software target/debug/examples/ui_preview
SLINT_BACKEND=winit-software target/debug/examples/ui_preview --model-settings
```

The preview exits after capture. Review the images and copy the selected outputs
to this directory with the published names. Commit these selected documentation
assets; keep the remaining build and preview outputs under ignored `target/`.
The README uses relative links so the images work both on GitHub and in a clone.
