# Kanata configuration

| File | Purpose |
| --- | --- |
| `config.toml` | **Your live config** (git-ignored). The repo `.env` points `KANATA_CONFIG_FILE` at it. |
| `container.example.toml` | Template for the private Docker deployment: private route via your reverse proxy, Ollama through `host.orb.internal`, Codex effort aliases, owner key |
| `container.public.example.toml` | The same plus the public listener for `kanata-api` (Cloudflare tunnel) and an empty public allowlist |
| `personal.example.toml` | Template for running `kanata serve` natively on a host, outside Docker. Shows vLLM text, audio-chat and native-ASR endpoints |
| `chatgpt.example.toml` | Private Sign in with ChatGPT, protected host credentials and explicit account model selection |
| `speech.example.toml` | Speech output through a Kokoro-FastAPI compatible server, with voice and format allowlists |
| `vision.example.toml` | Native Ollama image-input gateway with explicit capabilities and bounded payloads |
| `embeddings.example.toml` | Minimal native Ollama embedding gateway with separate embedding key scopes |

The offline schema fixture used by the tests lives in `tests/fixtures/config/example.toml`. Never serve it.

## Reloading route changes

On Linux/macOS, `kanata serve` accepts SIGHUP to validate and adopt route, adapter and publication changes while active requests finish. Listener/global/state-path changes require restart. See [configuration reload](../docs/guides/config-reload.md) for the supported scope, retained admission state and status checks. Ordinary keys-file changes continue to reload automatically.

## Concepts

- **Routes** map an exact `(model_alias, operation)` pair to one adapter and upstream model. One alias can carry several operations. For example, a multimodal alias such as `omni` can be both `chat` (text plus inline audio) and `transcription` (native ASR). Each pair maps to exactly one backend, so a second backend for the same operation needs its own alias (e.g. `omni-transcribe`).
- **Aliases** may not contain `:` except a Codex effort suffix (`gpt-6-sol:low`, `:high`). An unsuffixed Codex alias uses medium effort.
- **Reasoning:** on the private listener, chat replies carry the backend's reasoning text as `message.reasoning_content` (streamed as `delta.reasoning_content`, at most 256 KiB per reply) and `usage.completion_tokens_details.reasoning_tokens` when the backend reports it. The public listener never returns reasoning text. Codex returns only a summary of its reasoning; `codex_reasoning_summary = "auto" | "concise" | "detailed"` on a Codex chat route asks for one explicitly.
- **Keys** live in a separate file managed by the host `kanata key` CLI:
  ```toml
  [keys]
  file = "keys/keys.toml"   # relative to this config's directory
  usage_dir = "state"
  ```
  - Paths are relative to the config file's directory. A missing keys file means no keys are loaded (with a warning). The file must not be group- or world-writable (the CLI writes it 0600). It holds at most 1000 records, revoked ones included, and ids are never reused.
  - `kanata key new --config <cfg> --id <id> (--chat A|--transcription A)... --expires <1|3|7|13|30|60|unlimited> [--owner] [--max-in-flight N] [--rate-limit N/MS] [--key-out path]` prints the key once (or writes it to `--key-out`) and stores only its SHA-256 digest. With no scopes in a terminal it offers a route picker. `key list [--all] [--json]`, `key show <id>`, `key edit <id>` (`--add-chat`, `--remove-chat`, `--add-transcription`, `--remove-transcription`, `--expires`, `--max-in-flight`, `--rate-limit`, `--clear-limits`; same secret), `key rm <id>` (revoke; the owner needs `--force`), `key rotate (<id>|--owner) --expires ...`. `kanata routes --config <cfg>` shows whether each route is public (`yes`, `no`, `never`). Changes apply within about 2 s without a restart; an invalid file is rejected and the previous keys stay active.
  - Only one key may be the owner. Any key may hold Codex scopes (private listener only); the public plane never loads such keys.
  - **Usage:** each server plane writes `usage-<plane>.json` into its usage directory (Compose mounts `state/private` and `state/public`; key ids, request counts, last use and reported token totals; flushed every 30 s and at shutdown), shown by `key list`/`key show`. **Audit:** the CLI appends to `audit.jsonl` next to the keys file (time, local user and uid, action, key id, owner flag, edit changes; never secrets).
  - **Daily quotas:** file-backed keys may carry `daily_quota = { requests = 100, tokens = 200000, reservation_tokens = 8000 }`; `[keys] usage_dir` is required. `key new`/`key edit` accept `--daily-requests`, `--daily-tokens`, `--reservation-tokens`, and edits accept `--clear-quota`. Durable reservations precede dispatch; missing usage retains the token reservation. `kanata key usage` reads model/operation/day reports and keeps process planes separate. Token overruns are possible when a provider reports more than reserved; see [usage and quotas](../docs/guides/usage-quotas.md).
  - **Inline `[[application_keys]]` is deprecated.** It still loads, but has no expiry, revocation, reload or usage; combining it with `[keys]` is an error. `secret_ref` there is `file:/abs/path`, `env:NAME`, or `sha256:<hex>`. To migrate: `kanata key migrate --config <cfg>`, add the `[keys]` table, delete the inline blocks, then `scripts/kanata.sh check`.
- **Context length:** a chat route may declare `context_tokens` (256 to 16,777,216). Kanata shows it on `/v1/models` as `kanata.context_tokens` so clients can size prompts; leaving it unset (`null`) means the provider's own window. Kanata doesn't set the window itself:
  - Cloud models (Ollama cloud, OpenRouter, Codex) already run at their full context; leave it unset.
  - Local Ollama can't take a context size per request on its OpenAI-compatible API. Create a sized variant with `scripts/kanata.sh ollama-context <model> <tokens>` (e.g. `gpt-oss:20b` at 16384 becomes `gpt-oss:20b-16k`, sharing the same weights), point the route's `upstream_id` at it, and set `context_tokens` to match. Unsized local models use Ollama's server-wide `OLLAMA_CONTEXT_LENGTH`.
  - vLLM's window is fixed at server start (`--max-model-len`); set `context_tokens` to match.
- **Output cap:** a chat route may declare `max_output_tokens` (1 to 1,048,576, and no more than `context_tokens` when both are set). Kanata shows it on `/v1/models` as `kanata.max_output_tokens` and rejects a larger `max_tokens`/`max_completion_tokens` with `400 invalid_request` naming that field. When omitted by the client, the route cap is sent upstream. The adapter must declare `sampling_controls`; Codex output caps are rejected because that adapter cannot enforce them.
- **Apple Foundation Models** (`kind = "apple_fm"`, macOS 27+): run `scripts/kanata.sh fm-serve install` to keep Apple's `fm serve` on `127.0.0.1:1976`, then point an adapter at `http://host.orb.internal:1976/v1` with `trust_zone = "local"`, `upstream_id = "system"` and `context_tokens = 8192`. It supports chat, streaming, `json_schema` output and sampling; tools, audio, `json_object` and `reasoning_effort` are rejected. Output formatting is loose (e.g. a reply cut short by `max_tokens` still reports `stop`). A guardrail refusal becomes an empty reply with `finish_reason: "content_filter"`, and a context overflow is a 400. `fm serve` has no authentication and accepts only loopback `Host` headers, so Kanata sends `Host: localhost:<port>` and you should keep `fm serve` on loopback. Check Apple's terms (`fm license`) before exposing it to other people.
- **Images:** set `capabilities.input_images = true` on an Ollama, vLLM or OpenRouter adapter and `allows_input_images = true` on its chat route. Both default to false. Only inline PNG/JPEG data URLs are accepted; no remote fetch. The complete image-only/text-image JSON request also fits `limits.max_body_bytes`. See [formats, fixed limits and validation](../docs/guides/image-input.md).
- **Speech output:** `kind = "speech"` uses an operator-run Kokoro-FastAPI compatible endpoint. Each `speech` route declares `speech_voices` and `speech_formats`; the operation needs its own key scope. See [speech setup and limits](../docs/guides/speech-output.md).
- **Audio:** vLLM and OpenRouter adapters can take inline audio in chat (`capabilities.input_audio = true` plus `allows_input_audio = true` on the route; wav or mp3, up to `limits.max_audio_bytes`). Transcription is either vLLM's `transcription_mode` (`native_asr` or `audio_chat`) or OpenRouter's speech-to-text endpoint (declare the `transcription` operation and route it to an STT model such as `mistralai/voxtral-small-24b-2507-stt`). Tool calls with audio need `audio_function_tools` on the adapter and `allows_audio_function_tools` on the route (vLLM and OpenRouter).
- **Remote vLLM** (e.g. a GPU VM): set `trust_zone = "external"`, an `https` `base_url` and `secret_ref` for the key vLLM was started with (`--api-key`). For Qwen3-style models served with a reasoning parser, `enable_thinking = false` on a route turns thinking off, so it doesn't use up `max_tokens` (useful for transcription through audio chat). A client can override it per request with `chat_template_kwargs: {"enable_thinking": false}`. Declare `streaming_chat` / `function_tools` (and `audio_streaming_chat` / `audio_function_tools` with the route's `allows_audio_*`) when the server runs a tool-call parser (`--enable-auto-tool-choice`).
- **Capacity:** `[limits]` `max_in_flight`/`max_queue` and `timeouts.queue_ms` apply per route. Optional extras:
  - `adapters[].max_in_flight` caps all routes on one backend together (e.g. `1` for a local model server that runs one call at a time). A request needs both a route slot and an adapter slot within `queue_ms`.
  - `adapters[].circuit_breaker` is on by default: after `failures` (5) connection failures or unavailable responses in a row, requests to that adapter get `503 upstream_unavailable` with `Retry-After` for `cooldown_ms` (30000), then one probe request decides whether it closes. Set `circuit_breaker = { enabled = false }` to turn it off. It never retries. Upstream 401/403 responses count as unavailable too, so bad backend credentials also open it.
  - A key's `max_in_flight` (`--max-in-flight`) caps one key's concurrent requests across all routes (`429 gateway_key_busy`), and `--rate-limit N/M` allows at most N requests per M ms (`429 gateway_key_rate_limited`); requests refused by the key or route limits don't use up the rate limit. Limits count per process, so the public and private containers count separately.
  - Refusals carry a slightly randomized `Retry-After` so refused clients don't all retry together. Clients should still add their own jitter.
- **Public exposure** is opt-in: `[listeners.public]` plus exact entries in `publication.public_routes`. Codex is never allowed there. Clients only ever see the alias, so use an opaque alias for public routes if the upstream model should stay private.

After any edit, validate offline: `cargo run -q -- check --config config/config.toml`.

## Request buffers and diagnostics

### Embedding routes

Declare `operations = ["embeddings"]` on an Ollama adapter and `operation = "embeddings"` on its route. Mixed chat/embedding adapters are also supported. Grant a key with `kanata key new --config <cfg> --id search --embeddings <alias> --expires 30`; edit grants with `--add-embeddings` and `--remove-embeddings`. Chat grants do not authorize embeddings. Public access needs the exact embedding selector in `publication.public_routes`.

The endpoint accepts text or batches of 1–128 non-empty strings. `dimensions` is optional (1–16,384, subject to the model's support). A response may contain at most 262,144 vector values across its batch; malformed or larger upstream responses fail. `encoding_format` accepts `float` (default) or `base64` (little-endian float32). Token arrays, streaming, `user` and unknown fields are rejected. Input uses `limits.max_body_bytes`, the shared upload reservations, admission and deadlines. Reported input tokens enter usage accounting; omitted upstream usage remains explicitly missing. These limits do not replace the upstream model's token limit.

### Shared reservations

`limits.max_uploads` (default 8) and `limits.max_buffered_bytes` (default 268435456, 256 MiB) apply per process, across chat and transcription. They are checked after authentication and before reading the body. A reservation holds three times the allowed body size for raw, decoded and outbound payloads, through parsing, queueing and adapter dispatch. `Content-Length` narrows the reservation and becomes an enforced body limit; a request without it reserves the full endpoint limit. Streams release this input reservation when dispatch returns the stream; inference permits still cover the stream's lifetime.

The byte setting bounds reserved payload capacity, not total process RSS. Leave memory for JSON objects, response buffers, TLS and the runtime. It must accommodate at least one maximum-size request reservation. Capacity exhaustion returns `503 gateway_upload_busy` with `Retry-After: 1`.

`timeouts.upload_ms` defaults to the smaller of 30000 ms and `overall_ms`. It bounds body reception and returns `408 request_upload_timeout`; the overall deadline still takes precedence when it expires first.

- `kanata doctor --config <path> [--plane all|private|public]` validates config and key-file permissions and reads loopback `/status` for readiness and key-reload health. A host command cannot see a container's loopback; run it with `docker compose exec kanata /usr/local/bin/kanata doctor --config /etc/kanata/config.toml` for that process.
- Add `--probe-backends` to request TCP connectivity checks. These send no credentials or inference requests and do not verify TLS, authentication or model availability.
- `kanata health --config <path>` checks loopback `/ready`, returning a nonzero exit code on failure. Compose uses this native command in its health checks.
- See [operations](../docs/guides/operations.md) for metrics and token usage semantics.

## Sign in with ChatGPT

`kind = "chatgpt"` requires `[chatgpt_auth].state_dir` (an absolute private host path), `trust_zone = "external"`, and the exact `base_url = "https://api.openai.com/v1"`. No `secret_ref` is accepted. Chat text and function tools are supported; all optional modality/chat-option capabilities stay false. Routes cannot be public and their scoped keys are excluded from a public process. Select an `upstream_id` from `kanata auth chatgpt models --config <path>` after signing in. Authentication paths require a restart to change. See [setup and trial](../docs/guides/sign-in-with-chatgpt.md).
