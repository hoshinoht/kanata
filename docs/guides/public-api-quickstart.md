# Kanata API: tester quickstart

You have been given access to a small, personally run, OpenAI-compatible API. It works with the official OpenAI SDKs and any client that lets you set a base URL.

| | |
| --- | --- |
| Base URL | `https://api.example.com/v1` |
| Auth | `Authorization: Bearer <your key>` (keys start with `kanata_sk_`) |
| Endpoints | `GET /v1/models`, `POST /v1/chat/completions`, `POST /v1/audio/transcriptions` (models with speech input) |
| Models | Whatever `GET /v1/models` lists for your key (for example `qwen3-0.6b`, a tiny test model, so expect short and sometimes silly answers) |

## Your key

- The key was sent to you privately. Treat it like a password: don't paste it into chats, screenshots, public repos or shared notebooks.
- It is personal to you and can be revoked at any time. If you think it leaked, tell the owner and they'll issue a new one.
- Keep it in an environment variable instead of in code:
  ```sh
  export KANATA_API_KEY='kanata_sk_...'
  ```

## Browser reference

Open the API base URL (for example, `https://api.example.com/v1`) in a browser for a public reference with placeholder examples. `/v1/` also works. Chat and transcription examples include cURL, JavaScript (Node.js), Python, Go and Rust, with setup instructions and copy buttons. Use the arrow keys, Home or End to change language tabs. The Material 3 Expressive layout places request details beside examples on wide screens and stacks them on mobile. Code uses a bundled Maple Mono font and local syntax highlighting; the guide loads no CDN assets. Both client listeners serve this same generic page; the admin listener does not.

Choose **View my access** after entering a bearer key to load its permitted models and capabilities from `GET /v1/models`. The public listener also applies its route allowlist. A listed model means a configured, bound route; it does not confirm that its backend is healthy. No inference request is sent by the page.

The page stores the key only in memory, clears the input after submission, and clears the key and model details on **Disconnect**, reload or navigation away. **Refresh access** rechecks permissions; failed refreshes clear the session. Examples always use `$KANATA_API_KEY`, never the entered secret. Keys are not accepted from query strings or fragments. Use HTTPS; browser key entry is disabled over plain HTTP except on localhost/loopback for development. Browser extensions and scripts that compromise the page can still read an in-memory key.

Only `GET`/`HEAD` at the exact `/v1` and `/v1/` paths are unauthenticated. All discovery and inference endpoints retain bearer authentication. Guide and model-list responses carry `Cache-Control: no-store`; reverse proxies must honor this and must not inject third-party scripts or weaken the page's Content Security Policy. Cloudflare Access, when configured, still applies before the guide is reached.

## 1. Check access

```sh
curl -s https://api.example.com/v1/models \
  -H "Authorization: Bearer $KANATA_API_KEY"
```

Expected: `{"object":"list","data":[{"id":"qwen3-0.6b",...}]}`. A `403` means the key is missing, mistyped or revoked.

## 2. Chat completion

```sh
curl -s https://api.example.com/v1/chat/completions \
  -H "Authorization: Bearer $KANATA_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model":"qwen3-0.6b","messages":[{"role":"user","content":"Say hello in one sentence."}]}'
```

The answer is in `choices[0].message.content`.

## 3. Streaming

```sh
curl -sN https://api.example.com/v1/chat/completions \
  -H "Authorization: Bearer $KANATA_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model":"qwen3-0.6b","stream":true,"messages":[{"role":"user","content":"Count from 1 to 5."}]}'
```

You receive `data: {...}` server-sent events, ending with `data: [DONE]`. Add `"stream_options":{"include_usage":true}` to also get token usage in the last chunk.

## 4. Speech (models that list `transcription` or `input_audio`)

`GET /v1/models` shows each model's `kanata.operations` and `kanata.input_audio` (and, where declared, `kanata.context_tokens` and `kanata.max_output_tokens`). Audio is WAV or MP3, up to 25 MiB; 16 kHz mono 16-bit WAV works best.

```sh
curl -s https://api.example.com/v1/audio/transcriptions \
  -H "Authorization: Bearer $KANATA_API_KEY" \
  -F model=<model> \
  -F "file=@clip.wav;type=audio/wav"
```

Set the file's type explicitly (`type=audio/wav` or `type=audio/mpeg`); an untyped upload is rejected. Returns `{"text": "..."}`. Models with `input_audio` also take audio inside a chat message, as a `{"type":"input_audio","input_audio":{"data":"<base64>","format":"wav"}}` content part.

## Python (openai ≥ 1.0)

```python
import os
from openai import OpenAI

client = OpenAI(base_url="https://api.example.com/v1", api_key=os.environ["KANATA_API_KEY"])

print([m.id for m in client.models.list().data])

reply = client.chat.completions.create(
    model="qwen3-0.6b",
    messages=[{"role": "user", "content": "Give me one fun fact about otters."}],
)
print(reply.choices[0].message.content)

for chunk in client.chat.completions.create(
    model="qwen3-0.6b", stream=True,
    messages=[{"role": "user", "content": "Count from 1 to 5."}],
):
    if chunk.choices and chunk.choices[0].delta.content:
        print(chunk.choices[0].delta.content, end="", flush=True)
```

## JavaScript / TypeScript (openai ≥ 4)

```js
import OpenAI from "openai";

const client = new OpenAI({ baseURL: "https://api.example.com/v1", apiKey: process.env.KANATA_API_KEY });
const reply = await client.chat.completions.create({
  model: "qwen3-0.6b",
  messages: [{ role: "user", content: "Say hello in one sentence." }],
});
console.log(reply.choices[0].message.content);
```

## Text embeddings

An alias with the `embeddings` operation accepts `POST /v1/embeddings`:

```sh
curl https://kanata.example.com/v1/embeddings \
  -H "Authorization: Bearer $KANATA_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model":"your-embedding-model","input":["A document","A search query"],"encoding_format":"float"}'
```

`input` is a non-empty string or 1–128 non-empty strings. The configured JSON body limit applies. `encoding_format` is `float` (default) or `base64` (little-endian float32). Optional `dimensions` must be 1–16,384 and supported by the model; the batch has a maximum of 262,144 output values. Token arrays and streaming are unsupported. The response contains `object: "list"`, your model alias and ordered `data` entries with `object: "embedding"`, `index` and `embedding`. `usage.prompt_tokens` and `usage.total_tokens` appear when reported upstream.

Embedding permissions are separate from chat permissions. `/v1/models` lists `embeddings` only for aliases your key can use on that listener. Its `kanata.embeddings` object describes request and output bounds. Model token limits are enforced by the upstream runtime.

## What is supported

- **Request fields:** `model`, `messages` (`system` / `user` / `assistant` / `tool` roles with text content, plus assistant `tool_calls` and user `input_audio` parts), `stream`, `stream_options.include_usage`, and `tools` / `tool_choice` on models that support tools.
- **Generation options,** where the model's route supports them (all do on `qwen3-0.6b`):

  | Option | Accepted values |
  | --- | --- |
  | `temperature` | 0–2 |
  | `top_p` | above 0, up to 1 |
  | `seed` | integer |
  | `max_tokens` or `max_completion_tokens` | 1 – 1,048,576, and no more than the model's `kanata.max_output_tokens` when set |
  | `response_format` | `{"type":"json_object"}` or `{"type":"json_schema","json_schema":{"name":…,"schema":{…},"strict":true}}` (schema ≤ 64 KiB, nesting ≤ 32 levels) |
  | `reasoning_effort` | On reasoning-enabled models, choose from `kanata.reasoning_efforts` when enumerated. Grouped models use one base ID with a separate effort; see the [service handoff](service-handoff.md) |
  | `chat_template_kwargs` | only `{"enable_thinking": true\|false}`, on vLLM-served models (e.g. `omnilion`) |
- **Strictness:** Kanata returns `400 invalid_request` for fields it doesn't support or a model can't honour, rather than silently ignoring them. The error's `param` names the field.
- **Reasoning:** this listener never returns a model's reasoning text. `usage.completion_tokens_details.reasoning_tokens` is included when the backend reports it.
- **Responses:** `/v1/responses` supports stateless text, function calls/results and typed SSE through chat routes and scopes. Send full history with `store: false`. See the [supported subset](responses.md).
- **Not available:** stored Responses/conversations, Assistants, fine-tuning and files.

## Speech output

Use `POST /v1/audio/speech` with a separately permitted speech model, text and a declared voice. It returns MP3/WAV bytes, up to 8 MiB. Authenticated model discovery lists `kanata.speech` formats, voices and limits. See [speech request examples](speech-output.md).

## Image input

When `/v1/models` declares `kanata.input_images`, user messages can include inline PNG/JPEG `image_url` data URLs. `kanata.images` lists the limits. Remote image URLs and `detail` options are rejected. See [image request examples and limits](image-input.md).

## Errors

| HTTP | `error.code` | Meaning / what to do |
| --- | --- | --- |
| 400 | `invalid_request` | Malformed JSON, unsupported/unconfigured effort, conflicting effort suffix, or omitted inaccessible default. Check `param` |
| 401 | `key_expired` | Your key has expired. Ask the owner for a new one |
| 403 | `permission_denied` | Missing, invalid or revoked key, or a model/configured effort your key may not use on this listener |
| 404 | `not_found` | Wrong path. Use `/v1/models`, `/v1/chat/completions` or `/v1/audio/transcriptions` |
| 408 | `request_cancelled` | The request was cancelled, for example because the client disconnected |
| 413 | `invalid_request` | Request body or image count, file bytes, dimensions or total pixels exceed gateway limits |
| 429 | `gateway_queue_full` | The gateway queue for this model is full. Retry after the `Retry-After` seconds |
| 429 | `gateway_key_busy` / `gateway_key_rate_limited` | Your key has too many requests running, or sent too many recently. Retry after the `Retry-After` seconds |
| 429 | `daily_quota_exceeded` | Your key has exhausted its daily request allowance, or another full token reservation does not fit. `Retry-After` points to the next UTC midnight |
| 503 | `quota_unavailable` | The gateway cannot durably reserve your daily allowance, or its clock moved to an earlier UTC day. Contact the operator |
| 429 | `rate_limit_exceeded` | The model backend is rate-limiting. Wait and retry with backoff |
| 502 / 503 | `upstream_failure` / `upstream_unavailable` | The model backend is down or restarting. Try again later (after `Retry-After` seconds if present) |
| 503 | `gateway_busy` | No slot freed up in time. Retry after the `Retry-After` seconds |
| 503 | `gateway_upload_busy` | Request-buffer capacity is full. Retry after `Retry-After` seconds |
| 408 | `request_upload_timeout` | The request body did not arrive before the upload deadline |
| 504 | `upstream_timeout` | The model took too long |
| 5xx page from Cloudflare | — | The gateway itself is offline, for example because the host is asleep or restarting |

## Good to know

- This runs on a personal machine. There is no uptime guarantee, it may be offline at times, and capacity is small (a handful of concurrent requests), so please don't load-test it without asking.
- Traffic passes through Cloudflare, which terminates TLS, to the owner's machine. Kanata keeps only aggregate counters (endpoint, outcome, timing), never message content. Cloudflare and the model server may keep their own operational logs, so don't send secrets or sensitive personal data.
- When reporting a problem, include the time (with timezone), the model, the HTTP status and the `error.code`. **Never include your key.**
