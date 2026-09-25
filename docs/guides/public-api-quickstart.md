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
  | `reasoning_effort` | `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max` (the model decides which it honours) |
  | `chat_template_kwargs` | only `{"enable_thinking": true\|false}`, on vLLM-served models (e.g. `omnilion`) |
- **Strictness:** Kanata returns `400 invalid_request` for fields it doesn't support or a model can't honour, rather than silently ignoring them. The error's `param` names the field.
- **Not available:** embeddings, images, the Responses/Assistants APIs, fine-tuning and files.

## Errors

| HTTP | `error.code` | Meaning / what to do |
| --- | --- | --- |
| 400 | `invalid_request` | Malformed JSON or an unsupported field. Check `param` |
| 401 | `key_expired` | Your key has expired. Ask the owner for a new one |
| 403 | `permission_denied` | Missing, invalid or revoked key, or a model your key may not use publicly |
| 404 | `not_found` | Wrong path. Use `/v1/models`, `/v1/chat/completions` or `/v1/audio/transcriptions` |
| 408 | `request_cancelled` | The request was cancelled, for example because the client disconnected |
| 413 | `invalid_request` | Request body larger than 1 MiB |
| 429 | `gateway_queue_full` | The gateway queue for this model is full. Retry after the `Retry-After` seconds |
| 429 | `gateway_key_busy` / `gateway_key_rate_limited` | Your key has too many requests running, or sent too many recently. Retry after the `Retry-After` seconds |
| 429 | `rate_limit_exceeded` | The model backend is rate-limiting. Wait and retry with backoff |
| 502 / 503 | `upstream_failure` / `upstream_unavailable` | The model backend is down or restarting. Try again later (after `Retry-After` seconds if present) |
| 503 | `gateway_busy` | No slot freed up in time. Retry after the `Retry-After` seconds |
| 504 | `upstream_timeout` | The model took too long |
| 5xx page from Cloudflare | — | The gateway itself is offline, for example because the host is asleep or restarting |

## Good to know

- This runs on a personal machine. There is no uptime guarantee, it may be offline at times, and capacity is small (a handful of concurrent requests), so please don't load-test it without asking.
- Traffic passes through Cloudflare, which terminates TLS, to the owner's machine. Kanata keeps only aggregate counters (endpoint, outcome, timing), never message content. Cloudflare and the model server may keep their own operational logs, so don't send secrets or sensitive personal data.
- When reporting a problem, include the time (with timezone), the model, the HTTP status and the `error.code`. **Never include your key.**
