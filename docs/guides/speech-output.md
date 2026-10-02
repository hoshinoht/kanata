# Speech output

`POST /v1/audio/speech` converts text into an MP3 or WAV file. It uses an exact `speech` route and a separate key scope. The first backend implementation is `kind = "speech"`, for an operator-run server that accepts the [Kokoro-FastAPI speech request dialect](https://github.com/remsky/Kokoro-FastAPI/blob/master/api/src/structures/schemas.py). This is separate from the chat and transcription adapters.

## Configure an upstream

Start with [`speech.example.toml`](../../config/speech.example.toml). Set the base URL, installed model ID, allowed voices and response formats:

```toml
[[adapters]]
id = "speech-local"
kind = "speech"
base_url = "http://127.0.0.1:8880/v1"
trust_zone = "local"
[adapters.capabilities]
operations = ["speech"]
streaming_chat = false
function_tools = false

[[routes]]
id = "local-speech-route"
model_alias = "local-speech"
operation = "speech"
adapter_id = "speech-local"
upstream_id = "kokoro"
requires_streaming_chat = false
requires_function_tools = false
speech_voices = ["af_heart"]
speech_formats = ["mp3", "wav"]
```

Each speech route requires nonempty `speech_voices` and `speech_formats` allowlists. Voice IDs use up to 64 ASCII letters, digits, underscores or hyphens; at most 128 voices can be listed. Voice blends, user-supplied voice files and per-request aliases are unsupported. These fields are rejected on other operations.

Local and private-network upstreams may use unauthenticated HTTP. An external upstream requires HTTPS and `secret_ref`; any upstream with credentials requires HTTPS. Credentials are resolved through the existing secret resolver. Requests use the pinned shared transport, without redirects, retries or fallback models.

Create a scoped key:

```sh
kanata key new --config config/speech.example.toml --id reader \
  --speech local-speech --expires 30
```

Use `--add-speech` and `--remove-speech` to edit scopes. The host portal also lists speech scopes. Public access additionally requires the exact `(local-speech, speech)` publication allowlist entry.

## Client request

```sh
curl http://127.0.0.1:8080/v1/audio/speech \
  -H "Authorization: Bearer $KANATA_API_KEY" \
  -H 'Content-Type: application/json' \
  --data '{"model":"local-speech","input":"Hello from Kanata.","voice":"af_heart","response_format":"wav","speed":1}' \
  --output speech.wav
```

| Field | Accepted values |
| --- | --- |
| `model` | Required speech alias |
| `input` | Required nonblank text; at most 4,096 Unicode scalar values and 16,384 UTF-8 bytes |
| `voice` | Required voice from the route allowlist |
| `response_format` | `mp3` (default) or `wav`, when allowed by the route |
| `speed` | Optional finite number from 0.25 to 4, default 1 |

The configured JSON body limit also applies. Unknown fields and null option values are rejected. The endpoint accepts no streaming flag, instructions, SSML mode, download links or timestamps. Inline `[pause:...]`, `[voice:...]` and `[rate:...]` controls are rejected case-insensitively. The upstream request sets `stream: false` and `allow_voice_tags: false`.

## Response and limits

Success returns binary `audio/mpeg` or `audio/wav` with `Cache-Control: no-store`. Kanata collects at most 8 MiB before returning the audio. It checks the response MIME type and MP3 header or WAV container boundaries; it does not decode audio frames or assess the spoken content. It forwards neither upstream download paths nor provider headers.

Upload reservations, admission, queue limits, circuit breakers and request deadlines apply. Dropping an in-progress request closes its upstream connection. The inference permit stays with the completed response body until delivery or cancellation. Oversized, truncated or mismatched upstream audio fails with `502 upstream_failure`.

No token usage accompanies this binary dialect. Usage accounting records the upstream attempt with missing token usage; it never reports zero tokens as measured usage. Daily request quotas apply. For token quotas, the configured token reservation remains charged when upstream usage is absent; it is an operator estimate, not an audio billing measurement.

Authenticated `/v1/models` returns the permitted speech operation and `kanata.speech` voice/format allowlists, text limits, output byte cap and speed range. `doctor --probe-models` can query an upstream catalog if it implements that endpoint. Synthetic speech inference probes currently report `not_supported`.

## Validation status

Tests cover local mock audio responses, private/public scopes, strict inputs, voice/format allowlists, key commands, malformed and oversized responses, cancellation and credential configuration. These are fixture checks. No live Kokoro service, model, voice availability or deployment has been verified.
