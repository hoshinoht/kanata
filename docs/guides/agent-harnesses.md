# Agent harnesses

Use an authenticated Kanata base URL and a model alias published by `GET /v1/models`. Keep the key in the harness's environment-backed secret setting, not in a shared config file. ChatGPT, CLIProxyAPI and Anthropic routes are private-only.

These setup shapes were exercised against a mock upstream on 2026-10-11; that does not establish live-provider availability.

## OMP 18.8.6

In `models.yml`, point a provider at Kanata and select the wire each model uses:

```yaml
providers:
  kanata:
    baseUrl: https://<kanata>/v1
    apiKey: KANATA_API_KEY
    api: openai-completions # or openai-responses
    models:
      - id: <chat-alias>
        name: Kanata
```

Set `KANATA_API_KEY` in OMP's environment. Choose `openai-completions` for Chat Completions or `openai-responses` for Responses.

## OpenCode 2.0.24

Add a custom provider to `opencode.json` and use the published chat alias as the model ID:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "provider": {
    "kanata": {
      "npm": "@ai-sdk/openai-compatible",
      "name": "Kanata",
      "options": {
        "baseURL": "https://<kanata>/v1",
        "apiKey": "{env:KANATA_API_KEY}"
      },
      "models": {
        "<chat-alias>": {
          "name": "Kanata chat",
          "tool_call": true
        }
      }
    }
  }
}
```

## Codex CLI 0.154

In the user `config.toml`, select the custom Responses provider. Disable tools that Kanata does not host:

```toml
model = "<chat-alias>"
model_provider = "kanata"
web_search = "disabled"

[features]
multi_agent = false

[model_providers.kanata]
name = "Kanata"
base_url = "https://<kanata>/v1"
env_key = "KANATA_API_KEY"
wire_api = "responses"
```

Set `KANATA_API_KEY` in the environment. `web_search = "disabled"` and `features.multi_agent = false` are required: Codex's `web_search` and `namespace` tools are non-function tools, which Kanata rejects with `param=tools[i]`.

## Claude Code

Claude Code uses the Messages endpoint. Set the base URL to the Kanata host **without** `/v1`, and use one of these credentials:

```sh
export ANTHROPIC_BASE_URL="https://<kanata>"
export ANTHROPIC_API_KEY="$KANATA_API_KEY"      # sends x-api-key
# Or, instead:
export ANTHROPIC_AUTH_TOKEN="$KANATA_API_KEY"   # sends Authorization: Bearer
```

Use a Kanata chat alias as the model ID. Claude Code may warn that an alias is unknown to its model catalog; set `CLAUDE_CODE_MAX_CONTEXT_TOKENS` to the route's declared context window. Check `kanata.messages.function_tools` and `kanata.messages.streaming` in `/v1/models` before relying on tools or streaming. Kanata accepts function tools and tool results but does not execute tools; hosted/server tools are rejected. Incoming `thinking` and `redacted_thinking` blocks are discarded, and Kanata does not emit thinking blocks. A completed reply with no text or tool calls returns `content: []` with `stop_reason: "end_turn"`; a stream sends no content blocks.

`/v1/messages` errors use `{ "type": "error", "error": { "type", "message", "code", "param" } }` for route-level errors. Types map by status: 400/other 4xx `invalid_request_error`, 401 `authentication_error`, 403 `permission_error`, 404 `not_found_error`, 408/504 `timeout_error`, 413 `request_too_large`, 429 `rate_limit_error`, 503/529 `overloaded_error`, and other 5xx `api_error`. Public-listener authentication failures before routing retain the OpenAI error envelope. `GET /v1/messages` returns 405 with `Allow: POST`; `count_tokens` is not implemented.

## Generic OpenAI SDK

Set the SDK base URL to the Kanata `/v1` URL and its API key to your Kanata key. For example, with the Python OpenAI SDK:

```python
import os
from openai import OpenAI

client = OpenAI(
    base_url="https://<kanata>/v1",
    api_key=os.environ["KANATA_API_KEY"],
)
```

Use `client.chat.completions` or `client.responses` with a published chat alias. See the [Responses subset](responses.md) for its stateless request and history rules. Kanata accepts only its documented wire fields; unknown fields fail with a JSON-path `param` rather than being silently ignored.

Chat assistant turns may replay the reasoning they returned as `reasoning_content` (a string of at most 256 KiB; empty is accepted). Other roles reject it with `param=messages[i].reasoning_content`. Kanata forwards it as the assistant message's `reasoning` to vLLM, Ollama and OpenRouter routes, which pass it to the model's chat template (gpt-oss, Qwen3 and similar); CLIProxyAPI, ChatGPT and Apple FM routes drop it. ChatGPT replays only encrypted reasoning through [Responses](responses.md).

## Prompt caching and limits

For ChatGPT routes, use a stable `prompt_cache_key` (1–64 printable ASCII bytes) and keep the system prompt byte-stable to help reuse upstream prefixes. The key is forwarded to those routes and dropped elsewhere. When the upstream reports cached input, private responses expose it as `usage.prompt_tokens_details.cached_tokens` (Chat), `usage.input_tokens_details.cached_tokens` (Responses), or `usage.cache_read_input_tokens` (Messages). Public listeners omit cached-token counts; missing values are not estimated.

Long agent transcripts can exceed the default `limits.max_body_bytes` of 1 MiB. Ask the operator to raise it for long sessions; the request-buffer reservation must still fit within `limits.max_buffered_bytes`. Messages requires `max_tokens` from 1 to 1,048,576 and clamps it to the route cap. On routes without `sampling_controls`, max-token fields are validated and dropped. Messages streams limit each argument delta to 16 KiB and each response to 64 tool calls.

The optional `timeouts.sse_keepalive_ms` defaults to 15000 ms, capped by `timeouts.overall_ms`. Chat and Responses send SSE comments; Messages sends `event: ping`, only after the first upstream event. Keepalives do not extend `idle_ms` or `overall_ms`.

## References

- [OMP model and provider configuration](https://github.com/can1357/oh-my-pi/blob/main/docs/models.md)
- [OpenCode custom providers](https://opencode.ai/docs/providers/)
- [Codex CLI configuration reference](https://developers.openai.com/codex/config-reference/)
- [Claude Code gateway setup](https://code.claude.com/docs/en/llm-gateway-connect)
- [OpenAI Python SDK](https://github.com/openai/openai-python)
