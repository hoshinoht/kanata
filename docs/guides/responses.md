# Stateless Responses API

`POST /v1/responses` accepts text and function calls through existing **chat** routes and key scopes. A model's authenticated `/v1/models` entry advertises this subset under `kanata.responses`. Publication, admission, quotas, cancellation and deadlines use the same chat path.

```sh
curl "$KANATA_BASE_URL/responses" \
  -H "Authorization: Bearer $KANATA_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model":"your-chat-alias","input":"Hello","store":false}'
```

With the Python client:

```python
from openai import OpenAI
import os

client = OpenAI(base_url=os.environ["KANATA_BASE_URL"], api_key=os.environ["KANATA_API_KEY"])
response = client.responses.create(model="your-chat-alias", input="Hello", store=False)
print(response.output_text)
```

## Accepted input

| Field | Supported value |
| --- | --- |
| `model` | Advertised chat model ID, or a legacy exact effort alias |
| `input` | Nonempty text or an array of message/function-call/function-result items |
| `instructions` | Optional nonempty system text |
| `store`, `background` | Omitted or `false` |
| `stream` | Boolean; route must support streaming |
| `truncation` | Omitted or `"disabled"` |
| `tools` | Flat function declarations: `type`, `name`, `description`, `parameters`; `strict` omitted or `false` |
| `tool_choice` | `auto`, `none`, `required`, or `{ "type": "function", "name": "..." }` |
| `temperature`, `top_p`, `max_output_tokens` | Only when the route declares sampling controls; configured output caps still apply |
| `reasoning` | `{ "effort": "low" }` when `kanata.reasoning_control` is true; choose from the model's advertised `kanata.reasoning_efforts` |

Messages use `role` (`system`, `developer`, `user`, `assistant`) and text `content`, or an array of `input_text` parts. Replayed assistant output may use `output_text` with empty annotations and logprobs. Function calls use `type: "function_call"`, `call_id`, `name`, and string `arguments`; their results use `type: "function_call_output"`, the same `call_id`, and nonempty string `output`. Include a result for every outstanding call before continuing the conversation. Kanata does not execute functions.

To continue, send the original input, the previous response's `output` items and the function results or next user message. Generated summary-only reasoning items are accepted during replay and discarded; their text is not forwarded as conversation context. Preserve the actual message and tool history. Encrypted reasoning input remains unsupported. Kanata keeps no conversation state. Generated response/item IDs are identifiers for that result, not retrieval handles.

For grouped reasoning models, send the base model ID and an accessible effort. Omitting the effort requires permission for the unsuffixed default route. See [client integration and migration](service-handoff.md) for migration examples. Responses return the canonical model ID and the explicit requested effort under `reasoning.effort`.

`previous_response_id`, `conversation`, stored responses, background work, hosted tools, strict function-schema enforcement, images/audio, structured-output options, client-requested `reasoning.summary`, encrypted reasoning and all other fields are unsupported. Unsupported options return `400 invalid_request`; recognized unsupported fields include a `param`. Use `/v1/chat/completions` for its additional supported modalities and options. GET, DELETE and cancellation endpoints for stored responses are absent.

## Private reasoning summaries

A private ChatGPT or Codex route with `reasoning_summary = "auto"`, `"concise"` or `"detailed"` can return a `type: "reasoning"` output item. Its `summary` contains `type: "summary_text"` parts. Summary requests are off by default and controlled by the operator; the client request still accepts only `reasoning.effort`. Account/model support determines whether text is returned. Raw internal reasoning and encrypted reasoning are not exposed for these providers.

Summary output is optional. A missing summary does not mean that the model used zero reasoning tokens; usage reports remain independent, and a reported zero is distinct from a missing count.

## Streaming and limits

`stream: true` emits typed SSE events with increasing `sequence_number`: `response.created`, `response.in_progress`, item/content additions, text or function-argument deltas, corresponding done events, and a terminal response. Private reasoning output also emits `response.reasoning_summary_part.added`, `response.reasoning_summary_text.delta`, `response.reasoning_summary_text.done` and `response.reasoning_summary_part.done`, alongside output item events. There is no chat-style `[DONE]` marker.

- `response.completed` means the normalized upstream response completed.
- `response.incomplete` carries `max_output_tokens` or `content_filter` when the provider stopped early.
- `response.failed` carries a sanitized error for failure or timeout after streaming began. Partial text is not a successful response. Failures before streaming begins use the normal HTTP error envelope.

The final response includes accumulated output and available usage. Missing token counts remain unknown; no tokenizer estimates are returned as measured usage. Text, function arguments and reasoning summaries together are limited to 8 MiB; reasoning text is capped at 256 KiB per response, with at most 64 function calls and 16 KiB per streamed argument delta. Request body limits also apply. The full output is retained in memory to produce typed done events and the terminal response; dropping the client stream cancels upstream work and releases capacity.

The wire shapes follow the documented [Responses events](https://developers.openai.com/api/reference/resources/responses/streaming-events) and [function-call flow](https://developers.openai.com/api/docs/guides/function-calling). Kanata's supported subset is defined above.
