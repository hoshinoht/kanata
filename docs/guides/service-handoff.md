# Client integration and migration

## Changes for client services

Keep your existing Kanata base URL and bearer key. Refresh authenticated `GET /v1/models` and use each entry's `id` as the request model. Grouped reasoning routes now appear once, for example:

```json
{
  "id": "gpt-6-luna",
  "object": "model",
  "kanata": {
    "operations": ["chat"],
    "reasoning_control": true,
    "reasoning_efforts": ["low", "medium"]
  }
}
```

This is an illustrative partial entry. The array contains configured, bound reasoning levels accessible to **the requesting key**. Different keys can see different arrays. Discovery describes route access; it does not establish backend health or live model availability.

Services with a model picker should display one model and a separate effort selector. A label such as `GPT-6-Luna` is for display; send the exact advertised ID `gpt-6-luna`. Populate the selector from `kanata.reasoning_efforts` when `reasoning_control` is true. This applies equally to account providers and to operator-configured effort families on vLLM, Ollama, OpenRouter or CLIProxyAPI routes (for example Claude through CLIProxyAPI). Ordinary routes without a family advertise `null` (or the provider's fixed list); `null` means there is no enumerated list, rather than a list to invent from model names.

### Chat completions

Replace a hardcoded suffixed model with the base model and a separate field:

```json
{
  "model": "gpt-6-luna",
  "reasoning_effort": "low",
  "messages": [{"role": "user", "content": "Hello"}]
}
```

Python SDK example, using a key that advertises `low`:

```python
response = client.chat.completions.create(
    model="gpt-6-luna",
    reasoning_effort="low",
    messages=[{"role": "user", "content": "Hello"}],
)
```

### Stateless Responses

Use the nested effort field on `POST /v1/responses`:

```json
{
  "model": "gpt-6-luna",
  "reasoning": {"effort": "low"},
  "input": "Hello",
  "store": false
}
```

Send full conversation history. Stored responses remain unsupported. Private routes can return operator-enabled reasoning summaries; see [Responses](responses.md) for output items, replay rules and streaming events.

### Reasoning summaries for debugging

Operators can enable summaries on private ChatGPT routes with `reasoning_summary = "auto"`, `"concise"` or `"detailed"`. Clients keep their request shape; `reasoning.summary` is not accepted. Chat completions return optional `message.reasoning_content`, and streams use `delta.reasoning_content`. Responses returns `type: "reasoning"` output items with `summary_text` parts and typed summary events. Replayed summary-only items are accepted and discarded; preserve the actual message and tool history because summaries do not become conversation context.

Treat missing summary text as “No reasoning summary was returned for this request.” Keep missing token counts distinct from a reported `0`: requesting a summary or selecting an effort does not guarantee reasoning tokens. Raw internal reasoning and encrypted reasoning are not exposed for ChatGPT.

### Compatibility and errors

- Existing requests such as `model: "gpt-6-luna:low"` remain accepted with their exact key permission. An explicit effort must match the suffix.
- Grouped replies and streamed model fields use the canonical base ID, including replies to legacy suffixed requests. Update response-model comparisons and cache keys that expect the suffix; store the selected effort separately.
- Omitting an effort requires permission for the unsuffixed default route. A key with only `:low` access must explicitly send `low` with the base ID.
- A configured level outside the key's access returns `403 permission_denied`. An unsupported or unconfigured level, conflicting suffix, or unavailable default returns `400 invalid_request`; inspect `param` (`reasoning_effort` for Chat, `reasoning.effort` for Responses).
- Exact internal scopes remain in the key file, admission limits, usage records and logs. API grouping does not grant additional levels. Refresh discovery after key scope changes; key reload normally takes about two seconds.
- Existing ungrouped models retain their request shape. Pinned routes whose adapter disables reasoning control retain their exact advertised IDs.

## Use ChatGPT routes

Configure the account provider using [Sign in with ChatGPT](sign-in-with-chatgpt.md), then add explicit private routes and grant their exact aliases to service keys. The account catalog does not create routes or permissions. Confirm inference with the selected account and model before relying on a route.

Services continue to use gateway bearer keys. Account OAuth credentials stay on the Kanata host. ChatGPT, CLIProxyAPI and Anthropic routes are private-only; adding one of their scopes makes the entire key unavailable on the public listener. Use a separate key for public access.

The portal displays the configured upstream name and its API alias. For example, a route may display **GPT-6-Luna** with `API alias: chatgpt-luna`. Send the alias returned by authenticated discovery. Model and effort availability depend on the configured routes, key grants and upstream account.

## Operate client access

- Use the [private portal](private-portal.md) to create, rotate or revoke gateway keys and grant exact models and efforts. Provider and global selection controls stage changes until **Save changes**.
- Run `deploy/scripts/kanata.sh chatgpt status` to inspect saved sign-in metadata and `deploy/scripts/kanata.sh chatgpt models` to fetch the account catalog. Reauthenticate with `deploy/scripts/kanata.sh chatgpt login` when needed.
- Configure ChatGPT pins from `supported_reasoning_efforts` and select the desired default for the unsuffixed route. Unsupported catalog levels cannot be configured.
- Keep the protected ChatGPT state directory writable and mounted only in the private container. See [Docker setup](../../deploy/docker/README.md#sign-in-with-chatgpt).
- Key edits use automatic reload. Route edits follow [configuration reload](config-reload.md); binary and mount changes require rebuilding or restarting as appropriate.
