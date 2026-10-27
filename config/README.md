# Kanata configuration

| File | Purpose |
| --- | --- |
| `config.toml` | **Your live config** (git-ignored). The repo `.env` points `KANATA_CONFIG_FILE` at it. |
| `container.example.toml` | Template for the private Docker deployment: private route via your reverse proxy, Ollama through `host.orb.internal`, owner key |
| `container.public.example.toml` | The same plus the public listener for `kanata-api` (Cloudflare tunnel) and an empty public allowlist |
| `personal.example.toml` | Template for running `kanata serve` natively on a host, outside Docker. Shows vLLM text, audio-chat and native-ASR endpoints |
| `chatgpt.example.toml` | Private Sign in with ChatGPT, protected host credentials and explicit account model selection |
| `anthropic.example.toml` | Private Claude chat through the Messages API with a Console API key, effort families and the Compose secret path |
| `compact.example.toml` | Private account catalog with named route profiles and explicit reasoning-family expansion |
| `speech.example.toml` | Speech output through a Kokoro-FastAPI compatible server, with voice and format allowlists |
| `vision.example.toml` | Native Ollama image-input gateway with explicit capabilities and bounded payloads |
| `embeddings.example.toml` | Minimal native Ollama embedding gateway with separate embedding key scopes |

The offline schema fixture used by the tests lives in `tests/fixtures/config/example.toml`. Never serve it.

Inline `[[application_keys]]` were removed; old configs fail `kanata check` with `inline_keys_removed`. Use `chatgpt.example.toml` for ChatGPT plan integrations.

For Docker sign-in with ChatGPT, add `deploy/compose/compose.kanata.chatgpt.yml` and set `KANATA_CHATGPT_STATE_DIR` to the same absolute path as `[chatgpt_auth].state_dir`. It is a dedicated owner-only writable directory outside the checkout, shared by host sign-in and the private container. See [deployment setup](../deploy/docker/README.md#sign-in-with-chatgpt). Manage the deployed client key file with `deploy/scripts/kanata.sh portal`.

## Reloading route changes

On Linux/macOS, `kanata serve` accepts SIGHUP to validate and adopt route, adapter and publication changes while active requests finish. Listener/global/state-path changes require restart. See [configuration reload](../docs/guides/config-reload.md) for the supported scope, retained admission state and status checks. Ordinary keys-file changes continue to reload automatically.

## Concepts

- **Routes** map an exact `(model_alias, operation)` pair to one adapter and upstream model. One alias can carry several operations. For example, a multimodal alias such as `omni` can be both `chat` (text plus inline audio) and `transcription` (native ASR). Each pair maps to exactly one backend, so a second backend for the same operation needs its own alias (e.g. `omni-transcribe`).
- **Aliases** may include a validated effort suffix, such as `gpt-6-sol:low`, on ChatGPT routes and on chat routes pinned with `reasoning_effort` on any `reasoning_control` adapter. Grouped discovery exposes the base alias and accessible efforts. Pin the ChatGPT base route explicitly.
- **Reasoning:** on the private listener, chat replies carry the backend's reasoning text as `message.reasoning_content` (streamed as `delta.reasoning_content`, at most 256 KiB per reply) and `usage.completion_tokens_details.reasoning_tokens` when reported. ChatGPT chat routes can request a summary with `reasoning_summary = "auto" | "concise" | "detailed"`; omitting it leaves summary requests off. All variants of a reasoning family must use the same summary policy. Responses exposes private summaries as reasoning output items and summary SSE events. Account/model support determines whether a summary is returned; requesting one does not guarantee text or a nonzero reasoning-token count. Raw internal reasoning and encrypted reasoning are not exposed for ChatGPT. The public listener never returns reasoning text.
- **Reasoning replay:** chat clients may send that text back on assistant turns as `reasoning_content` (at most 256 KiB; other roles are rejected). vLLM, Ollama and OpenRouter adapters forward it as the assistant message's `reasoning`; CLIProxyAPI, ChatGPT, Anthropic and Apple FM adapters drop it.
- **Reasoning model families:** pinned effort routes with adapter `reasoning_control = true` advertise one base model and the key's accessible `kanata.reasoning_efforts`. Variants must share their adapter, upstream and route policies/caps; incompatible mappings fail with `inconsistent_reasoning_family`. Keep exact key scopes unchanged. Disabled reasoning controls preserve legacy exact IDs. See the [client migration notes](../docs/guides/service-handoff.md).
- **Keys** live in a separate file managed by the host `kanata key` CLI. The `[keys]` table is required (`keys: required`):
  ```toml
  [keys]
  file = "keys/keys.toml"   # relative to this config's directory
  usage_dir = "state"
  ```
  - Paths are relative to the config file's directory. A missing keys file means no keys are loaded (with a warning). The file must not be group- or world-writable (the CLI writes it 0600). It holds at most 1000 records, revoked ones included, and ids are never reused.
  - `kanata key new --config <cfg> --id <id> (--chat A|--transcription A)... --expires <1|3|7|13|30|60|unlimited> [--owner] [--max-in-flight N] [--rate-limit N/MS] [--key-out path]` prints the key once (or writes it to `--key-out`) and stores only its SHA-256 digest. With no scopes in a terminal it offers a route picker. `key list [--all] [--json]`, `key show <id>`, `key edit <id>` (`--add-chat`, `--remove-chat`, `--add-transcription`, `--remove-transcription`, `--expires`, `--max-in-flight`, `--rate-limit`, `--clear-limits`; same secret), `key rm <id>` (revoke; the owner needs `--force`), `key rotate (<id>|--owner) --expires ...`. `kanata routes --config <cfg>` shows whether each route is public (`yes`, `no`, `never`). Changes apply within about 2 s without a restart; an invalid file is rejected and the previous keys stay active.
  - Only one key may be the owner. Any key may hold private-only scopes (ChatGPT, CLIProxyAPI, Anthropic; private listener only); the public plane never loads such keys.
  - **Usage:** each server plane writes `usage-<plane>.json` into its usage directory (Compose mounts `state/private` and `state/public`; key ids, request counts, last use and reported token totals; flushed every 30 s and at shutdown), shown by `key list`/`key show`. **Audit:** the CLI appends to `audit.jsonl` next to the keys file (time, local user and uid, action, key id, owner flag, edit changes; never secrets).
  - **Daily quotas:** file-backed keys may carry `daily_quota = { requests = 100, tokens = 200000, reservation_tokens = 8000 }`; `[keys] usage_dir` is required. `key new`/`key edit` accept `--daily-requests`, `--daily-tokens`, `--reservation-tokens`, and edits accept `--clear-quota`. Durable reservations precede dispatch; missing usage retains the token reservation. `kanata key usage` reads model/operation/day reports and keeps process planes separate. Token overruns are possible when a provider reports more than reserved; see [usage and quotas](../docs/guides/usage-quotas.md).
- **Context length:** a chat route may declare `context_tokens` (256 to 16,777,216). Kanata shows it on `/v1/models` as `kanata.context_tokens` so clients can size prompts; leaving it unset (`null`) means the provider's own window. Kanata doesn't set the window itself:
  - Cloud models (Ollama cloud, OpenRouter, ChatGPT) already run at their full context; leave it unset.
  - Local Ollama can't take a context size per request on its OpenAI-compatible API. Create a sized variant with `deploy/scripts/kanata.sh ollama-context <model> <tokens>` (e.g. `gpt-oss:20b` at 16384 becomes `gpt-oss:20b-16k`, sharing the same weights), point the route's `upstream_id` at it, and set `context_tokens` to match. Unsized local models use Ollama's server-wide `OLLAMA_CONTEXT_LENGTH`.
  - vLLM's window is fixed at server start (`--max-model-len`); set `context_tokens` to match.
- **Output cap:** a chat route may declare `max_output_tokens` (1 to 1,048,576, and no more than `context_tokens` when both are set). Kanata shows it on `/v1/models` as `kanata.max_output_tokens`; on routes with `sampling_controls`, a larger request `max_tokens`/`max_completion_tokens` (Chat) or `max_output_tokens` (Responses) returns `400 invalid_request` naming that field, and an omitted value inherits the route cap upstream. These max-token fields are still validated but dropped on routes without sampling controls.
- **Apple Foundation Models** (`kind = "apple_fm"`, macOS 27+): run `deploy/scripts/kanata.sh fm-serve install` to keep Apple's `fm serve` on `127.0.0.1:1976`, then point an adapter at `http://host.orb.internal:1976/v1` with `trust_zone = "local"`, `upstream_id = "system"` and `context_tokens = 8192`. It supports chat, streaming, `json_schema` output and sampling; tools, audio, `json_object` and `reasoning_effort` are rejected. Output formatting is loose (e.g. a reply cut short by `max_tokens` still reports `stop`). A guardrail refusal becomes an empty reply with `finish_reason: "content_filter"`, and a context overflow is a 400. `fm serve` has no authentication and accepts only loopback `Host` headers, so Kanata sends `Host: localhost:<port>` and you should keep `fm serve` on loopback. Check Apple's terms (`fm license`) before exposing it to other people.
- **Images:** set `capabilities.input_images = true` on an Ollama, vLLM, OpenRouter, CLIProxyAPI or Anthropic adapter and `allows_input_images = true` on its chat route. Both default to false. Only inline PNG/JPEG data URLs are accepted; no remote fetch. The complete image-only/text-image JSON request also fits `limits.max_body_bytes`. See [formats, fixed limits and validation](../docs/guides/image-input.md).
- **Speech output:** `kind = "speech"` uses an operator-run Kokoro-FastAPI compatible endpoint. Each `speech` route declares `speech_voices` and `speech_formats`; the operation needs its own key scope. See [speech setup and limits](../docs/guides/speech-output.md).
- **Audio:** vLLM and OpenRouter adapters can take inline audio in chat (`capabilities.input_audio = true` plus `allows_input_audio = true` on the route; wav or mp3, up to `limits.max_audio_bytes`). Transcription is either vLLM's `transcription_mode` (`native_asr` or `audio_chat`) or OpenRouter's speech-to-text endpoint (declare the `transcription` operation and route it to an STT model such as `mistralai/voxtral-small-24b-2507-stt`). Tool calls with audio need `audio_function_tools` on the adapter and `allows_audio_function_tools` on the route (vLLM and OpenRouter).
- **Remote vLLM** (e.g. a GPU VM): set `trust_zone = "external"`, an `https` `base_url` and `secret_ref` for the key vLLM was started with (`--api-key`). For Qwen3-style models served with a reasoning parser, `enable_thinking = false` on a route turns thinking off, so it doesn't use up `max_tokens` (useful for transcription through audio chat). A client can override it per request with `chat_template_kwargs: {"enable_thinking": false}`. Declare `streaming_chat` / `function_tools` (and `audio_streaming_chat` / `audio_function_tools` with the route's `allows_audio_*`) when the server runs a tool-call parser (`--enable-auto-tool-choice`). Declare `reasoning_control = true` for a backend that takes OpenAI's top-level `reasoning_effort` (gpt-oss-class models on vLLM): a request's `reasoning_effort` is then forwarded as-is, and without it the field is rejected with `param: reasoning_effort`. The effort and the thinking switch are alternatives: on a `reasoning_control` adapter a chat route may not set `enable_thinking` (`conflicts_with_reasoning_control`), and a request carrying both `reasoning_effort` and `chat_template_kwargs` is rejected with `param: chat_template_kwargs`.
  - **Effort families on vLLM/Ollama/OpenRouter/CLIProxyAPI:** on a `reasoning_control` adapter, a chat route may pin `reasoning_effort` (optional, unlike ChatGPT), and a catalog model may declare `default_effort`/`efforts` to generate a family (see [compact model catalogs](#compact-model-catalogs)). A request routed to a pinned route sends the pin upstream as its `reasoning_effort`; a request for the base alias with `reasoning_effort` selects the matching `alias:effort` route, an unconfigured level returns `400` with `param: reasoning_effort`, and omitting it uses the base route's pin (the key needs the base scope). An explicit effort sent to a suffixed alias must match its suffix. Unpinned routes keep forwarding any per-request effort.
- **CLIProxyAPI** (`kind = "cliproxy"`): a local [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI) OpenAI-compatible endpoint (for example Claude through account sign-in). It replaces the earlier `kind = "vllm"` setup for CLIProxyAPI; change the adapter's `kind` and drop it from `publication.public_routes`. It uses the vLLM chat path (streaming, tools, inline images, `reasoning_control` with effort families and pins) and requires `trust_zone = "external"` and `secret_ref` (the CLIProxyAPI client key), with `base_url` either `http://127.0.0.1:<port>/v1` on the same host or `https` with `ca_file` (see [the TLS sidecar](../deploy/docker/README.md#cliproxyapi-upstream)). Only the `chat` operation is accepted (`cliproxy_chat_only`); `input_audio` is rejected, and `transcription_mode` and route `enable_thinking` are vLLM-only. Like ChatGPT and Anthropic it is **private-only**: naming one of its routes in `publication.public_routes` is a config error (`cliproxy_not_public`), the public plane loads neither its adapter nor any key holding one of its scopes, and `kanata routes` shows `never` under PUBLIC.
- **Anthropic** (`kind = "anthropic"`, template `anthropic.example.toml`): Claude through the Messages API with a Console API key. It requires `trust_zone = "external"`, the exact `base_url = "https://api.anthropic.com/v1"` (`anthropic_pinned_origin_required`) and `secret_ref` (with Compose, `file:/run/secrets/anthropic-api-key` from `deploy/compose/compose.kanata.anthropic.yml` and `KANATA_ANTHROPIC_KEY_FILE`). Kanata sends `Authorization: Bearer <key>` and `anthropic-version: 2023-06-01`. Only `chat` is accepted (`anthropic_chat_only`); streaming, tools, inline images, `structured_output` (`json_schema` only; `json_object` returns `400`) and `reasoning_control` may be declared, while `input_audio` and `sampling_controls` are rejected because current Claude models refuse non-default sampling. `reasoning_effort` `low` through `max` is sent as `output_config.effort` (`none`/`minimal` are rejected, also as pins or family levels with `unsupported_effort`); pins and effort families work as on CLIProxyAPI. `thinking` is never sent, so models use their default adaptive thinking. Optional `[adapters.anthropic]`: `workspace_id = "wrkspc_…"` (sent as `anthropic-workspace-id`, required for multi-workspace keys) and `default_max_output_tokens` (default 16000), which is the `max_tokens` of every request: without sampling controls a client's `max_tokens` is dropped and a route `max_output_tokens` is invalid. Every request asks for automatic prompt caching; `prompt_cache_key` is not forwarded; cached reads are reported as cached input tokens and cache writes count as input. System and developer messages are only accepted before the conversation starts. With encrypted reasoning requested (Responses `include: ["reasoning.encrypted_content"]`, private listener), signed thinking blocks come back as reasoning items with `anthropic_thinking_`/`anthropic_redacted_thinking_` ids and are replayed on later turns; other providers' items are skipped. Upstream `529` overload maps to `503 upstream_unavailable`. Like CLIProxyAPI it is **private-only** (`anthropic_not_public`, never loaded by `--plane public`, `never` in `kanata routes`). `doctor --probe-models` reports `not_supported`; use `--probe-inference`.
- **Adapter `base_url` and credentials:** an adapter with `secret_ref` (vLLM, CLIProxyAPI, speech) or `trust_zone = "external"` needs an `https` `base_url`. The one exception is plain `http` to a literal loopback IP (`127.0.0.0/8` or `[::1]`), e.g. a local CLIProxyAPI at `http://127.0.0.1:18317/v1` with `kind = "cliproxy"`, `trust_zone = "external"` and `secret_ref`. Kanata connects to that address directly, without DNS. Hostnames, including `localhost`, and LAN or other IPs still require `https`. Inside a container, `127.0.0.1` is the container itself, not the host.
  - **Private CA:** an `https` Ollama, vLLM, OpenRouter, CLIProxyAPI or speech adapter may set `ca_file = "/abs/path/ca.pem"`, a PEM bundle of one or more `CERTIFICATE` blocks (at most 64 KiB, no keys). Those certificates replace the system trust store for that adapter only, so it trusts nothing else; the server certificate must still name the `base_url` host (an IP-literal host needs a matching IP SAN). `kanata check --plane P`, `serve --plane P` and every SIGHUP reload read and parse the file only for adapters that plane keeps, so a public container need not mount the CA of a private-only adapter such as CLIProxyAPI. For a kept adapter a missing, oversized or invalid bundle is a config error (`adapters[N].ca_file`) and a reload picks up a replaced bundle; the host key CLI, portal and `health` skip it. `ca_file` with an `http` `base_url`, or on ChatGPT and Apple Foundation Models adapters, is rejected.
- **Capacity:** `[limits]` `max_in_flight`/`max_queue` and `timeouts.queue_ms` apply per route. Optional extras:
  - `adapters[].max_in_flight` caps all routes on one backend together (e.g. `1` for a local model server that runs one call at a time). A request needs both a route slot and an adapter slot within `queue_ms`.
  - `adapters[].circuit_breaker` is on by default: after `failures` (5) connection failures or unavailable responses in a row, requests to that adapter get `503 upstream_unavailable` with `Retry-After` for `cooldown_ms` (30000), then one probe request decides whether it closes. Set `circuit_breaker = { enabled = false }` to turn it off. It never retries. Upstream 401/403 responses count as unavailable too, so bad backend credentials also open it.
  - A key's `max_in_flight` (`--max-in-flight`) caps one key's concurrent requests across all routes (`429 gateway_key_busy`), and `--rate-limit N/M` allows at most N requests per M ms (`429 gateway_key_rate_limited`); requests refused by the key or route limits don't use up the rate limit. Limits count per process, so the public and private containers count separately.
  - Refusals carry a slightly randomized `Retry-After` so refused clients don't all retry together. Clients should still add their own jitter.
- **Public exposure** is opt-in: `[listeners.public]` plus exact entries in `publication.public_routes`. ChatGPT, CLIProxyAPI and Anthropic are never allowed there. Clients only ever see the alias, so use an opaque alias for public routes if the upstream model should stay private.

After any edit, validate offline: `cargo run -q -- check --config config/config.toml`.

## Compact model catalogs

Named route profiles reuse policies across a keyed catalog. Existing `[[routes]]` remain supported and can coexist with catalog entries. Both forms expand into the same validated exact routes; duplicate selectors or IDs fail instead of overriding earlier definitions.

```toml
[route_profiles.account]
adapter_id = "chatgpt-private"
requires_streaming_chat = true
requires_function_tools = true
default_effort = "medium"
efforts = ["low", "medium", "high"]

[models.chat]
assistant = { profile = "account", upstream_id = "your-account-model" }

[models.chat.writer]
profile = "account"
upstream_id = "another-account-model"
efforts = ["low", "high"]
reasoning_summary = "auto"
```

`assistant` creates the base alias pinned to medium plus exactly `assistant:low`, `assistant:medium` and `assistant:high`. `writer` creates the base alias pinned to medium plus its two listed variants. Account access is not queried during loading: choose model slugs, defaults and supported efforts from your account's catalog. A base effort does not have to appear in the suffix list. Omit `efforts` to create just the pinned base; an explicitly empty effort list is invalid.

Catalog tables are `[models.chat]`, `[models.transcription]`, `[models.embeddings]` and `[models.speech]`. One alias can occur under several operations, each with its own adapter and upstream. Profiles are optional; a standalone model can declare `adapter_id` directly. Always specify `upstream_id` on the model. Compact routes default `requires_streaming_chat` and `requires_function_tools` to false; existing explicit routes still require both fields.

Settings resolve as **built-in defaults → one selected profile → model overrides**. Profiles can contain adapter selection and all existing route policy fields, plus `default_effort` and `efforts`. Profiles cannot refer to another profile or define IDs, upstreams or `unset`.

- Explicit `false` overrides an inherited `true`.
- Lists replace inherited lists; `[]` clears an inherited extension, voice or format allowlist, subject to operation validation.
- `unset = ["context_tokens", "reasoning_summary"]` removes inherited optional values. Clearable fields are `reasoning_effort`, `reasoning_summary`, `context_tokens`, `max_output_tokens`, `enable_thinking`, `default_effort`, `efforts` and `adapter_id`. Clearing and overriding the same field is invalid.
- `default_effort`/`efforts` generate chat families on ChatGPT accounts and on any adapter declaring `reasoning_control = true` (vLLM, Ollama, OpenRouter, CLIProxyAPI, Anthropic); other models fail with `reasoning_chat_family_required`. They cannot be combined with the explicit pin field `reasoning_effort`. Variants are pinned with `reasoning_effort`, which non-account adapters (including CLIProxyAPI and Anthropic) send upstream. Other routes use the existing policy fields directly.
- Every variant shares the resolved policy, including summaries and caps. Adapter capability validation still applies independently.

Chat IDs default to the alias with `:` replaced by `-`; other operations append `-<operation>`. Effort IDs append `-<effort>` to the base ID. Set `id` to preserve a base ID, and `route_ids = { base = "old-base", low = "old-low" }` for individual generated IDs. ID collisions fail validation; reordering entries never changes IDs. IDs must remain stable when migrating a running server because admission state is retained by ID.

Profiles and families do not grant key scopes or publish routes. Keys continue to grant exact `(alias, operation)` pairs. Publication remains a separate exact `publication.public_routes` allowlist: each generated route of a family on a publishable adapter is published only when listed. Private-only routes (ChatGPT, CLIProxyAPI, Anthropic) and their scoped keys remain excluded from public processes.

### Configuration fragments

Place `include` at the root of the config, before any table:

```toml
include = ["adapters.toml", "profiles.toml", "models/account.toml"]
```

Fragments may contain only `[[adapters]]`, `[[routes]]`, `[route_profiles]` and `[models]`. Listeners, authentication, keys, publication, limits, timeouts and logging stay in the root. Definitions combine independently of file order; duplicate profiles, models, adapters or route IDs fail. Profiles may reference adapters from any listed file.

Paths are relative to the root config directory, including `[keys]` paths. Fragment paths must stay inside that directory, including symlink targets; absolute paths, parent traversal, duplicate files and nested includes are rejected. At most 64 fragments and 8 MiB of combined configuration are accepted. Missing, malformed or changing fragments reject the complete load. For Compose, mount the directory or each required fragment so the same relative layout is available inside the container.

SIGHUP reads and validates the root and all fragments before publishing a complete generation. Save all related edits before signaling; a failed load preserves the previous generation. Merely editing a fragment does not trigger automatic reload. See [configuration reload](../docs/guides/config-reload.md).

### Compact, expand and preview

```sh
kanata config compact --config config/config.toml --output config/compact.toml
kanata config expand --config config/compact.toml --output config/expanded.toml
kanata config plan --config config/compact.toml --against config/config.toml
```

`compact` collects common policies into profiles and compatible reasoning routes into families. `expand` writes a standalone config with explicit routes. Both preserve existing IDs and secret references, validate the result and compare effective routes, adapter policies, listeners, publication, keys and global settings before publishing a new file. Outputs are created with mode 0600 on Unix; an existing path is never overwritten. When the output directory changes, relative key and usage paths become absolute references to their original locations. Comments and original formatting are not preserved.

`plan` compares two on-disk configurations and their current key files. It shows route additions/removals, changed route fields and their source declarations, adapter changes, public exposure, key changes, dangling grants and all statically detectable restart requirements. It prints field names rather than credential references or backend URLs. A plan can report a removed route with dangling file-backed grants so those grants can be repaired; `check` and `serve` continue to reject that configuration. Run `check` before adopting a candidate. A missing keys file means no keys, as with ordinary loading.

These commands do not resolve provider credentials, contact providers, inspect a running server or apply changes. Plans cannot account for historical admission IDs retained by a running process. The normal signal/restart workflow still applies after review. Compact examples and fixture checks are not claims of live model availability.

## Request buffers and diagnostics

### Embedding routes

Declare `operations = ["embeddings"]` on an Ollama, vLLM or OpenRouter adapter and `operation = "embeddings"` on its route; other kinds fail with `embeddings_unsupported_by_adapter_kind`. OpenRouter embeddings never fall back to another provider (`allow_fallbacks = false`). Mixed chat/embedding adapters are also supported. Grant a key with `kanata key new --config <cfg> --id search --embeddings <alias> --expires 30`; edit grants with `--add-embeddings` and `--remove-embeddings`. Chat grants do not authorize embeddings. Public access needs the exact embedding selector in `publication.public_routes`.

The endpoint accepts text or batches of 1–128 non-empty strings. `dimensions` is optional (1–16,384, subject to the model's support). A response may contain at most 262,144 vector values across its batch; malformed or larger upstream responses fail. `encoding_format` accepts `float` (default) or `base64` (little-endian float32). Token arrays, streaming, `user` and unknown fields are rejected. Input uses `limits.max_body_bytes`, the shared upload reservations, admission and deadlines. Reported input tokens enter usage accounting; omitted upstream usage remains explicitly missing. These limits do not replace the upstream model's token limit.

### Shared reservations

`limits.max_uploads` (default 8) and `limits.max_buffered_bytes` (default 268435456, 256 MiB) apply per process, across chat and transcription. They are checked after authentication and before reading the body. A reservation holds three times the allowed body size for raw, decoded and outbound payloads, through parsing, queueing and adapter dispatch. `Content-Length` narrows the reservation and becomes an enforced body limit; a request without it reserves the full endpoint limit. Streams release this input reservation when dispatch returns the stream; inference permits still cover the stream's lifetime.

The byte setting bounds reserved payload capacity, not total process RSS. Leave memory for JSON objects, response buffers, TLS and the runtime. It must accommodate at least one maximum-size request reservation. Capacity exhaustion returns `503 gateway_upload_busy` with `Retry-After: 1`.

Long agent transcripts may exceed the default `limits.max_body_bytes` (1 MiB); raise it for those sessions. `limits.max_buffered_bytes` must still accommodate at least one maximum-size reservation, which uses three times the allowed body size.

`timeouts.upload_ms` defaults to the smaller of 30000 ms and `overall_ms`. It bounds body reception and returns `408 request_upload_timeout`; the overall deadline still takes precedence when it expires first.

`timeouts.sse_keepalive_ms` defaults to 15000 ms, capped by `timeouts.overall_ms`; zero is a config error and a value above `overall_ms` is rejected. Chat and Responses send SSE comments, while Messages sends `event: ping`, only after the first upstream event. Keepalives do not extend `idle_ms` or `overall_ms`. Changing this setting requires a restart.

- `kanata doctor --config <path> [--plane all|private|public]` validates config and key-file permissions and reads loopback `/status` for readiness and key-reload health. A host command cannot see a container's loopback; run it with `docker compose exec kanata /usr/local/bin/kanata doctor --config /etc/kanata/config.toml` for that process.
- Add `--probe-backends` to request TCP connectivity checks. These send no credentials or inference requests and do not verify TLS, authentication or model availability.
- `kanata health --config <path>` checks loopback `/ready`, returning a nonzero exit code on failure. Compose uses this native command in its health checks.
- See [operations](../docs/guides/operations.md) for metrics and token usage semantics.

## Sign in with ChatGPT

`kind = "chatgpt"` requires `[chatgpt_auth].state_dir` (an absolute private host path), `trust_zone = "external"`, and the exact `base_url = "https://api.openai.com/v1"`. No `secret_ref` is accepted. Chat text and function tools are supported. For selectable reasoning, declare `reasoning_control = true` and pin every route with `reasoning_effort = "low" | "medium" | ...` (required on ChatGPT, optional on other `reasoning_control` adapters); suffix variants must match their pin. Select pins from the account catalog's `supported_reasoning_efforts` and use its `default_reasoning_effort` for the base alias. Other optional modality/chat-option capabilities stay false. Routes cannot be public and their scoped keys are excluded from a public process. Select an `upstream_id` from `kanata auth chatgpt models --config <path>` after signing in. Authentication paths require a restart to change. See [setup and trial](../docs/guides/sign-in-with-chatgpt.md).

## Field reference

Every setting the config schema accepts; unknown fields fail with `unknown_field`. Fields marked optional may be omitted.

- **Root:** `listeners`, `publication`, `limits` and `timeouts` are required; `adapters` and `routes` must end up non-empty (directly, through `include` fragments or through `models` catalogs); `keys` is required; `chatgpt_auth` and `logging` are optional.
- **`[logging]`:** `level` (`trace`, `debug`, `info` (default), `warn`, `error`) and `format` (`text` (default) or `json`).
- **`[listeners]`:** `client` (the private API), `admin` (loopback `/live`, `/ready`, `/status` and `/metrics`) and optional `public`, each with `bind` (an IP literal) and `port` (non-zero). The client bind must be loopback, unspecified, private/unique-local or one of `publication.tailnet_addresses`; the public bind must be a concrete private or unique-local address outside the tailnet prefix and must not collide with the client listener.
- **`[publication]`:** `tailnet_addresses` (required, at least one): the host's own Tailscale addresses (`100.64.0.0/10` or `fd7a:115c:a1e0::/48`), used to accept a client listener bound to one of them and to keep the public listener off the tailnet. `public_routes` (optional): exact `{ model_alias, operation }` selectors served on the public listener.
- **`[chatgpt_auth]`:** `state_dir`, see [Sign in with ChatGPT](#sign-in-with-chatgpt).
- **`[[adapters]]`:** `id`, `kind` (`ollama`, `vllm`, `openrouter`, `chatgpt`, `apple_fm`, `speech`, `cliproxy`, `anthropic`), `base_url`, `trust_zone` (`local`, `private_network` or `external`), optional `secret_ref` (`env:NAME` or `file:/abs/path`), `transcription_mode`, `extension_allowlist`, `max_in_flight`, `circuit_breaker` (`enabled`, `failures`, `cooldown_ms`), `ca_file` and `anthropic` (`workspace_id`, `default_max_output_tokens`), and the required `capabilities` table:
  - `operations` (required: `chat`, `transcription`, `embeddings`, `speech`), `streaming_chat` and `function_tools` (both required booleans).
  - Optional booleans, default false: `input_audio`, `input_images`, `audio_streaming_chat` (streamed chat with inline audio; needs `input_audio` and `streaming_chat`), `audio_function_tools` (tool calls with inline audio; needs `input_audio` and `function_tools`), `structured_output`, `sampling_controls` and `reasoning_control`.
- **`[[routes]]`:** `id`, `model_alias`, `operation`, `adapter_id`, `upstream_id`, `requires_streaming_chat` and `requires_function_tools` are required. Optional: `reasoning_effort`, `reasoning_summary`, `extension_allowlist`, `allows_input_audio`, `allows_input_images`, `allows_audio_streaming_chat` (accepts streamed chat with inline audio; needs `allows_input_audio` and the adapter's `audio_streaming_chat`), `allows_audio_function_tools` (accepts tool calls with inline audio; needs `allows_input_audio` and the adapter's `audio_function_tools`), `speech_voices`, `speech_formats` (`mp3`, `wav`), `context_tokens`, `max_output_tokens` and `enable_thinking`.
- **Request extensions:** `extension_allowlist` on an adapter and on a route lists namespaced keys (two or more dot-separated lowercase segments such as `io.example.trace`, at most 128 bytes) that a request may carry in its top-level `extensions` object (or the multipart `extensions` field of a transcription). A key must be on both lists, and the serialized object must fit `limits.max_extension_bytes`; otherwise the request fails with `400` before dispatch. Both lists default to empty, which refuses every extension. ChatGPT and Anthropic adapters reject a non-empty list, and the bundled adapters currently refuse requests that carry extensions, so leave the lists empty unless an adapter documents support.
- **`[keys]`:** `file` (required) and `usage_dir` (optional). Keys-file records use the same `{ model_alias, operation }` permission shape and `rate_limit = { requests, per_ms }`.
- **`[limits]`:** `max_queue` and `max_in_flight` (per route), `max_body_bytes` (JSON request bodies), `max_audio_bytes` (decoded inline or uploaded audio), `max_extension_bytes` (serialized request `extensions`), all required and non-zero; `max_uploads` and `max_buffered_bytes` are optional, see [shared reservations](#shared-reservations).
- **`[timeouts]`** (milliseconds, each non-zero, at most seven days and at most `overall_ms`): `queue_ms` (waiting for a route and adapter slot), `connect_ms` (opening the upstream connection), `headers_ms` (upstream response headers), `first_byte_ms` (first upstream body byte or stream event), `idle_ms` (gap between upstream stream chunks) and `overall_ms` (the whole request) are required; `upload_ms` and `sse_keepalive_ms` are optional, see [request buffers](#request-buffers-and-diagnostics).
- **Catalogs:** `include`, `route_profiles` and `models` (`chat`, `transcription`, `embeddings`, `speech`), see [compact model catalogs](#compact-model-catalogs). A model or profile takes the route fields above plus `profile`, `id`, `route_ids`, `default_effort`, `efforts` and `unset`.

## Validation errors

`kanata check`, `serve`, SIGHUP reloads and the key CLI report a rejected config as `config error at <path>: <class>`. The path names the field (for example `adapters[0].base_url` or `models.chat.assistant.efforts`). Neither part contains a secret value, and for files other than the config the path never echoes an unknown key.

| Class | Meaning |
| --- | --- |
| **Parsing and reading** | |
| `parse_error` | The file is not valid UTF-8 TOML. |
| `schema_error` | The TOML does not match the schema (for example a missing required field). |
| `type_error` | A field has the wrong TOML type. |
| `unknown_field` | A field or table the schema does not define. |
| `inline_keys_removed` | The config still has inline `[[application_keys]]`; move keys to the `[keys] file`. |
| `read_error` | A config, fragment, keys or CA file could not be read. |
| `serialization_error` | A config table could not be re-encoded while expanding catalogs or writing `config compact`/`expand` output. |
| **Shared value rules** | |
| `required` | A setting required in this context is missing (for example `[keys]`, `secret_ref` or a ChatGPT route's `reasoning_effort`). |
| `empty` | A list or value that must not be empty is empty. |
| `zero` | A count, port or duration is zero. |
| `duplicate` | A list entry, profile or permission appears twice. |
| `out_of_range` | A number is outside its accepted range. |
| `too_large` | A keys or CA file exceeds its size limit. |
| `too_many` | The keys file holds more than 1000 records. |
| `limit_too_large` | A `max_in_flight` or rate-limit count exceeds 1,000,000. |
| `timeout_too_large` | A duration exceeds seven days, or `sse_keepalive_ms` exceeds `overall_ms`. |
| `invalid_or_duplicate_id` | An adapter, route or key id is empty, has characters outside `A-Z a-z 0-9 - _ .`, or is reused. |
| `invalid_or_empty` | A route `upstream_id` is blank. |
| `invalid_exact_alias` | A model alias is not an exact identifier, optionally with one valid `:effort` suffix. |
| `invalid_path` | A path contains control characters or cannot be represented. |
| `invalid_absolute_path` | `[chatgpt_auth].state_dir` is not a clean absolute path. |
| `absolute_path_required` | An adapter `ca_file` is relative. |
| `invalid_extension_key` | An `extension_allowlist` entry is not a valid extension key. |
| `invalid_allowlist` | A speech route's `speech_voices` or `speech_formats` is empty, duplicated, invalid or too long. |
| `invalid_ip` | A bind or tailnet address is not an IP literal. |
| **Listeners and publication** | |
| `not_loopback` | `listeners.admin.bind` is not a loopback address. |
| `not_internal_address` | A client or public bind is not an accepted internal address (loopback, private, unique-local or a configured tailnet address for the client). |
| `not_concrete` | `listeners.public.bind` is loopback or unspecified. |
| `tailnet_address_forbidden` | `listeners.public.bind` shares a configured tailnet prefix. |
| `client_admin_port_collision` | The client and admin listeners use the same port. |
| `client_port_collision` | The public listener's bind and port overlap the client listener's. |
| `client_bind_collision` | The public listener binds the client listener's address. |
| `not_tailnet_address` | A `publication.tailnet_addresses` entry is outside the Tailscale ranges. |
| `public_listener_required` | `publication.public_routes` is set without `[listeners.public]`. |
| `unknown_route_selector` | A public route or key permission names no configured `(model_alias, operation)`. |
| `chatgpt_not_public` | A ChatGPT route is listed in `publication.public_routes`. |
| `cliproxy_not_public` | A CLIProxyAPI route is listed in `publication.public_routes`. |
| `anthropic_not_public` | An Anthropic route is listed in `publication.public_routes`. |
| `required_for_public_plane` | `--plane public` was requested without `[listeners.public]`. |
| **Limits and timeouts** | |
| `audio_limit_too_large` | `limits.max_audio_bytes` exceeds the built-in audio maximum. |
| `audio_envelope_overflow` | The base64 audio envelope added to `max_body_bytes` overflows. |
| `exceeds_overall` | A per-phase timeout exceeds `timeouts.overall_ms`. |
| `below_request_reservation` | `limits.max_buffered_bytes` cannot hold one maximum-size request reservation. |
| **Adapters** | |
| `invalid_url` | `base_url` does not parse. |
| `invalid_http_url` | `base_url` is not `http`/`https` with a host. |
| `userinfo_forbidden` | `base_url` contains a user name or password. |
| `fragment_forbidden` | `base_url` contains a `#fragment`. |
| `query_forbidden` | `base_url` contains a `?query`. |
| `https_required` | An external adapter, or one with `secret_ref`, uses plain `http` to anything but a literal loopback IP. |
| `https_base_url_required` | `ca_file` is set on an `http` adapter. |
| `invalid_secret_reference` | `secret_ref` is neither `env:NAME` nor `file:/absolute/path`. |
| `digest_reference_unsupported` | `secret_ref` is a `sha256:` digest. |
| `provider_zone_mismatch` | `trust_zone` is not allowed for the adapter kind. |
| `unsupported_by_adapter_kind` | A capability, `extension_allowlist` or `ca_file` the adapter kind cannot use. |
| `chat_operation_required` | A chat-only capability is declared without the `chat` operation. |
| `speech_adapter_required` | The `speech` operation and `kind = "speech"` must go together. |
| `speech_only` | A speech adapter declares other operations or chat capabilities, or a non-speech route sets `speech_voices`/`speech_formats`. |
| `embeddings_unsupported_by_adapter_kind` | `embeddings` on a kind other than Ollama, vLLM or OpenRouter. |
| `chatgpt_text_chat_only` | A ChatGPT adapter declares operations other than `chat`, or `input_audio`. |
| `chatgpt_pinned_origin_required` | A ChatGPT adapter's `base_url` is not `https://api.openai.com/v1`. |
| `chatgpt_secret_ref_unsupported` | A ChatGPT adapter sets `secret_ref`. |
| `without_chatgpt_adapter` | `[chatgpt_auth]` is set but no ChatGPT adapter exists. |
| `apple_fm_chat_only` | An Apple FM adapter declares `transcription`. |
| `cliproxy_chat_only` | A CLIProxyAPI adapter declares operations other than `chat`. |
| `anthropic_chat_only` | An Anthropic adapter declares operations other than `chat`. |
| `anthropic_only` | `[adapters.anthropic]` on another kind. |
| `anthropic_pinned_origin_required` | An Anthropic adapter's `base_url` is not `https://api.anthropic.com/v1`. |
| `invalid_workspace_id` | `anthropic.workspace_id` is not `wrkspc_` followed by letters and digits (at most 128 bytes). |
| `without_transcription_operation` | `transcription_mode` is set without the `transcription` operation. |
| `requires_input_audio_capability` | `transcription_mode = "audio_chat"` without `capabilities.input_audio`. |
| `required_for_transcription` | A vLLM adapter declares `transcription` without `transcription_mode`. |
| `vllm_only` | `transcription_mode` or a route's `enable_thinking` on a non-vLLM adapter. |
| `requires_audio_and_streaming_chat` | `audio_streaming_chat` without `input_audio` and `streaming_chat`. |
| `requires_audio_and_function_tools` | `audio_function_tools` without `input_audio` and `function_tools`. |
| `invalid_pem` | A `ca_file` is not valid PEM. |
| `certificates_only` | A `ca_file` contains a non-certificate PEM block. |
| `invalid_certificate` | A `ca_file` certificate cannot be used as a trust anchor. |
| `no_certificates` | A `ca_file` contains no certificates. |
| **Routes** | |
| `missing_adapter` | A route, model or public route names an adapter that does not exist. |
| `duplicate_selector` | Two routes or catalog models share a `(model_alias, operation)`. |
| `effort_alias_requires_reasoning_effort` | An `alias:effort` route on a non-ChatGPT adapter has no `reasoning_effort` pin. |
| `unsupported_by_adapter` | The route's operation or a route policy (`requires_*`, `allows_*`, `reasoning_effort`, `reasoning_summary`) is not supported by its adapter. |
| `unsupported_effort` | A pinned or family effort the adapter kind does not accept. |
| `does_not_match_alias` | `reasoning_effort` differs from the alias's `:effort` suffix. |
| `chat_only` | `context_tokens` or `max_output_tokens` on a non-chat route. |
| `unsupported_output_cap` | `max_output_tokens` on an adapter without `sampling_controls`. |
| `exceeds_context_tokens` | `max_output_tokens` is larger than `context_tokens`. |
| `requires_chat_template` | `enable_thinking` on a route that does not use a chat template. |
| `conflicts_with_reasoning_control` | `enable_thinking` on a chat route of a `reasoning_control` adapter. |
| `inconsistent_reasoning_family` | Variants of one reasoning family differ in adapter, upstream or route policy. |
| **Catalogs and fragments** | |
| `too_many_fragments` | `include` lists more than 64 fragments. |
| `relative_fragment_required` | An `include` entry is empty, absolute or uses `..`. |
| `outside_config_directory` | An `include` entry resolves outside the config directory. |
| `duplicate_fragment` | A fragment (or the root config) is included twice. |
| `document_too_large` | The config and its fragments exceed 8 MiB. |
| `changed_during_read` | A file changed while the config was being loaded. |
| `invalid_profile` | A `route_profiles` name is not a valid identifier. |
| `model_only_field` | A profile sets `profile`, `id`, `route_ids`, `upstream_id` or `unset`. |
| `missing_profile` | A model names an undefined profile. |
| `invalid_clear_field` | An `unset` entry is not a clearable field. |
| `conflicting_override` | A model both clears and sets the same field. |
| `reasoning_chat_family_required` | `default_effort`/`efforts` on a model that cannot form a chat reasoning family. |
| `conflicting_effort_options` | `default_effort`/`efforts` combined with `reasoning_effort`. |
| `unknown_variant` | A `route_ids` key is not `base` or one of the model's efforts. |
| **Keys** | |
| `required_for_daily_quota` | A key has `daily_quota` but `[keys] usage_dir` is unset. |
| `not_found` | The keys file is missing at reload. |
| `insecure_permissions` | The keys file is group- or world-writable. |
| `unsupported_version` | The keys file `version` is not supported. |
| `invalid_timestamp` | A key timestamp is malformed. |
| `invalid_digest` | A key digest is not `sha256:` plus 64 lowercase hex digits. |
| `duplicate_secret` | Two keys share a digest. |
| `multiple_owners` | More than one key is marked owner. |
| `invalid_token_reservation` | A daily quota's `reservation_tokens` is set without `tokens` or exceeds it. |
| `build_error` | A valid keys file could not be applied to the running config. |
| **Reload and config commands** | |
| `restart_required` | A SIGHUP reload changes a restart-only setting (`reload.<field>`). |
| `already_exists` | The `--output` of `config compact`/`expand` already exists. |
| `write_error` | The output file could not be written. |
| `random_error` | No randomness was available for the temporary output name. |
| `not_equivalent` | The rewritten config does not validate to the same effective config. |
