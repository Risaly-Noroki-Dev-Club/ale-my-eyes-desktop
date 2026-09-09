# Model Calling

The desktop uses `ale-core/src/model_api.rs` for remote generation and
`ale-modeld/src/scheduler.rs` for routing. `provider` is a settings preset;
`wire_api` selects the actual HTTP protocol, including for custom endpoints.

| `wire_api` | Path appended to the base URL | Authentication |
| --- | --- | --- |
| `openai_chat_completions` (default) | `/chat/completions` | Bearer |
| `openai_responses` | `/responses` | Bearer |
| `anthropic_messages` | `/messages` | `x-api-key` |
| `google_generate_content` | `/models/{model}:generateContent` | `x-goog-api-key` |

Base URLs include the provider version prefix, such as OpenAI/Anthropic `/v1`
or Gemini `/v1beta`. HTTPS is required except for loopback testing. Credentials,
query parameters, and fragments do not belong in base URLs. Azure-specific
deployment URLs and native non-OpenAI TTS adapters are outside this change.

## Requests and Output

One request represents conversation messages, an optional image, and optional
function tools. Text-only planning retains tools. Image MIME types are detected
from the supplied bytes. Responses requests use typed input/output items and
`store: false`; Anthropic separates system instructions and tool-use blocks;
Gemini uses inline image data and function declarations with JSON schemas.

The adapters collect visible text and multiple tool calls, including tool-only
responses. Missing completion markers, truncation, refusals, unknown tools,
duplicate call IDs, malformed JSON arguments, and schema-invalid arguments fail
before an action plan reaches desktop confirmation. JSON Schema validation uses
the `jsonschema` crate with external HTTP/file schema resolution disabled.
Existing action validation and desktop-derived risk checks still apply.

## Capabilities and Transcription

Settings' **Test capabilities** uses the unsaved draft. It first lists destination
URLs, models, and request counts with a cost notice. Confirmation runs four probes
per configured planning endpoint: text, image, text with tools, and image with
tools. Enabled transcription adds one audio probe. Results include duration and
typed failures. Cancel drops the active request and prevents subsequent probes.
Startup and settings saves never trigger remote capability tests.

Probes send only built-in samples: a generated red PNG, fixed prompts, a
side-effect-free `probe_echo` definition, and `ale-core/assets/model-probe.wav`.
They neither capture the user's screen nor record microphone input. A probe
demonstrates handling of that sample, not general task quality or reliability.

`transcription.enabled` defaults to false for new configurations. Its `endpoint`
has an independent base URL, key, model (default `whisper-1`), and timeout, using
the OpenAI-compatible `/audio/transcriptions` multipart API. SenseVoice remains
the scheduler's first attempt; authorized cloud transcription is its fallback.
The CLI/legacy inference path also uses the independent transcription endpoint.

For existing files without a `transcription` section, loading copies the legacy
primary URL/key once, sets `whisper-1`, and enables transcription if a key exists.
The migration preserves legacy routing rather than asserting provider support.
Subsequent changes to the primary provider do not alter transcription credentials.

## Deadlines and Failover

- A desktop processing request has one 85-second deadline shared across capture,
  ASR, planning, IPC, and retries, leaving room inside the existing phone wait.
  Confirmation and execution retain their separate timing and cancellation rules.
- Each remote endpoint defaults to 30 seconds; settings accept 1-80 seconds.
  Connection establishment is capped at five seconds. Local model stages are
  capped at 30 seconds; cloud stages may consume the remaining processing budget.
- Without backup, transient network, timeout, rate-limit, and server errors may
  retry once. Jitter is 200-400 ms; `Retry-After` takes precedence when supplied.
  No attempt starts if the shared deadline has elapsed or the delay cannot fit.
- Backup defaults off and requires explicit authorization in advanced settings.
  An authorized primary attempt receives at most half the remaining stage budget;
  one backup attempt may use the remainder. This path does not stack a same-endpoint
  retry. Authentication, permission, invalid output, and invalid request errors
  do not trigger failover. The primary circuit defaults to three failures/60 seconds.
- Requests pin their remote configuration. Hot updates apply to new requests
  without restarting local model processes; old failures cannot change the new
  configuration's circuit. IPC stages use distinct IDs and cancellation targets
  the original process. Late replies cannot satisfy a later stage. An interrupted
  partial IPC write invalidates that connection.

Retries only cover inference. They never replay desktop automation. Cancelling a
blocking native SenseVoice call stops waiting for its result but cannot forcibly
interrupt the native call already executing; its result is discarded.

## Persistence and Verification

Configuration writes use a sibling temporary file and atomic replacement. Primary,
backup, and transcription keys have separate credential-store entries and are
omitted from JSON. A failed save restores the prior credentials and leaves the
in-memory configuration unchanged. A failed scheduler update rolls back the saved
configuration and does not display success. Rollback failures are reported explicitly.

Run the workspace fmt/check/test/clippy commands in `AGENTS.md`. Tests use local
HTTP/IPC peers, including protocol headers/bodies, tool-only output, cancellation,
configuration migration/rollback, pinned requests, cross-protocol failover, and a
31-second response that exceeds the old cloud stage cap. No paid endpoint calls
are part of automated verification. Native settings screenshots can be regenerated:

```sh
cargo build -p ale-gui --example ui_preview --locked
SLINT_BACKEND=winit-software target/debug/examples/ui_preview --model-settings
```

Screenshots are written under `target/ui-preview/`. These checks do not establish
real-provider compatibility, Windows/Linux native acceptance, or real-task quality.
The phone protocol is unchanged; the user's completed phone testing is preserved.
