# Service handoff: model discovery and ChatGPT

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

Services with a model picker should display one model and a separate effort selector. A label such as `GPT-6-Luna` is for display; send the exact advertised ID `gpt-6-luna`. Populate the selector from `kanata.reasoning_efforts` when `reasoning_control` is true. Some ordinary providers advertise `null` for their effort list; that means there is no enumerated list, rather than a list to invent from model names.

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

Send full conversation history. Stored responses and reasoning summaries remain unsupported. See [Responses](responses.md) for the supported subset and terminal streaming events.

### Compatibility and errors

- Existing requests such as `model: "gpt-6-luna:low"` remain accepted with their exact key permission. An explicit effort must match the suffix.
- Grouped replies and streamed model fields use the canonical base ID, including replies to legacy suffixed requests. Update response-model comparisons and cache keys that expect the suffix; store the selected effort separately.
- Omitting an effort requires permission for the unsuffixed default route. A key with only `:low` access must explicitly send `low` with the base ID.
- A configured level outside the key's access returns `403 permission_denied`. An unsupported or unconfigured level, conflicting suffix, or unavailable default returns `400 invalid_request`; inspect `param` (`reasoning_effort` for Chat, `reasoning.effort` for Responses).
- Exact internal scopes remain in the key file, admission limits, usage records and logs. API grouping does not grant additional levels. Refresh discovery after key scope changes; key reload normally takes about two seconds.
- Existing ungrouped models retain their request shape. Pinned routes whose adapter disables reasoning control retain their exact advertised IDs.

## ChatGPT integration in this deployment

The private deployment has a separate ChatGPT account provider and these aliases:

| Alias | Configured upstream | Verified here |
| --- | --- | --- |
| `chatgpt-6-sol` | `gpt-6-sol` | Authenticated complete Chat at `low` returned HTTP 200 and `OK`; account catalog omitted this model |
| `chatgpt-6.1-sol` | `gpt-6.1-sol` | Authenticated complete Chat at `low` returned HTTP 200 and `OK`; account catalog omitted this model |
| `chatgpt-luna` | `gpt-6-luna` | Authenticated complete and streaming Chat requests at `low` returned HTTP 200 and `OK`; stream finished with `[DONE]` |
| `chatgpt-chat` | `gpt-6-astra` | Account catalog/configuration only; inference not tested |
| `chatgpt-5.6-sol` | `gpt-5.6-sol` | Account catalog and authenticated private discovery; inference not tested |
| `chatgpt-5.6-terra` | `gpt-5.6-terra` | Account catalog and authenticated private discovery; inference not tested |
| `chatgpt-5.6-luna` | `gpt-5.6-luna` | Account catalog and authenticated private discovery; inference not tested |
| `chatgpt-5.5` | `gpt-5.5` | Account catalog and authenticated private discovery; inference not tested |

The sign-in completed and the private container read the shared protected account state. These checks establish availability for this account at the time of testing, rather than every account or a future guarantee.

All five models returned by this account's current catalog have configured private routes, alongside the previously tested `gpt-6-luna` model. The eight aliases are exposed through the private API to the existing `chatgpt-client` key, including all 41 configured effort variants. GPT-6-Sol and GPT-6.1-Sol were added explicitly and succeeded with live inference despite their absence from this account catalog. They appear under **ChatGPT** in the portal's provider sections, labelled with the actual configured model name and a separate API alias. For example, **GPT-6-Luna** shows `API alias: chatgpt-luna`; services keep sending that alias. The key's secret and expiry are unchanged. To give another service access, use the portal to grant that service's existing key the exact ChatGPT aliases, then refresh its authenticated model list. Selecting a never-public scope makes the entire key private-only. Keep a separate key for services that also need public access. Future catalog changes require explicit route and scope updates.

Authenticated discovery verified these arrays for `chatgpt-client` after the local update:

| Private alias | Accessible reasoning levels |
| --- | --- |
| `chatgpt-6-sol`, `chatgpt-luna` | `none`, `low`, `medium`, `high`, `xhigh`, `max` |
| `chatgpt-6.1-sol`, `chatgpt-chat`, `chatgpt-5.6-sol`, `chatgpt-5.6-terra`, `chatgpt-5.6-luna` | `low`, `medium`, `high`, `xhigh`, `max` |
| `chatgpt-5.5` | `low`, `medium`, `high`, `xhigh` |

These discovery results establish configured access. GPT-6-Sol, GPT-6.1-Sol and Luna were tested with live inference at low; other levels and models have configuration/catalog evidence. Sol effort sets follow the official [GPT-6-Sol](https://developers.openai.com/api/docs/models/gpt-6-sol) and [GPT-6.1-Sol](https://developers.openai.com/api/docs/models/gpt-6.1-sol) references; GPT-6.1-Sol does not expose `none`. The same key returned `403 permission_denied` from the public model list.

Use `chatgpt-luna` to call the tested ChatGPT integration. It is a separate alias from the preserved `gpt-6-luna` service routes, which now also use ChatGPT. ChatGPT now uses the same effort selection contract. Refresh discovery with your service key and populate the picker from the returned array. Complete request selecting low:

```json
{"model":"chatgpt-luna","reasoning_effort":"low","messages":[{"role":"user","content":"Hello"}]}
```

Set `stream: true` for Chat SSE. Supply the user-created `chatgpt-client` gateway key privately to the service; account OAuth tokens stay on the Kanata host. Services continue to authenticate with gateway bearer keys. Account-provider routes remain private-only and unavailable through the public listener.

## Retiring Codex in this deployment

The active configuration now uses ChatGPT for every former Codex route. The Codex implementation, tests and example config remain intact. Its credential volume `kanata_codex_state` is retained and detached from both running gateways. No automatic fallback is configured.

Existing services can keep their URL, bearer key, model alias and supported reasoning selection. The migrated families are `gpt-6-astra`, `gpt-6-sol`, `gpt-6-luna`, `gpt-5.6-sol`, `gpt-5.6-terra` and `gpt-5.6-luna`. Existing exact grants are preserved, including legacy suffixes; their base routes remain pinned to `medium`. The new `gpt-6.1-sol` family has base, low and high grants on the owner key. Compatibility families advertise `low`, `medium`, `high` when all variants are authorized. Other service keys retain their current grants; use the portal to grant additional models or efforts explicitly.

After recreation, complete Chat requests at `low` returned HTTP 200 and `OK` through the deployed aliases `gpt-6-sol`, `gpt-6.1-sol`, `gpt-6-luna`, `chatgpt-6-sol` and `chatgpt-6.1-sol`. Both gateways were healthy. Authenticated discovery exposed the expected key-filtered arrays; owner and ChatGPT client keys were rejected by the public model list. Other migrated models, tool replay, streaming through migrated aliases and higher efforts were not tested live in this switch.

Use **Refresh** in the running portal to load the new provider inventory. Preserved aliases appear under **ChatGPT** alongside its namespaced aliases; their different key grants remain explicit. Inference now uses ChatGPT plan usage and authentication. Codex summary-specific route options have been removed; clients should accept the reasoning content the ChatGPT provider returns.

Protected restoration copies are in the ignored `config/state/operator-backups/20261003-codex-retired/` directory (`config.toml` and `deployment.env`). Follow the [restoration procedure](../../deploy/docker/README.md#retire-the-experimental-codex-provider), reconciling any newly added key scopes with the restored routes before validation. Keep the saved account volume. No release or remote push was performed.

## Operator handoff

- Open the host-only dashboard with `scripts/kanata.sh portal`, then enter its fresh terminal code at `http://127.0.0.1:9091/`. The portal shares the API guide's styles and Relay logo. Press Enter in that terminal for a new code without restarting, or choose **New login code** while unlocked. Expand a provider section to choose models or reasoning levels; provider and global bulk controls stage exact scopes until **Save changes**. Collapsing preserves selections. Its shape and containment choices follow the supplied M3 Expressive evaluation and verification references.
- Inspect sign-in with `scripts/kanata.sh chatgpt status`; use `scripts/kanata.sh chatgpt models` for the account catalog. Reauthenticate with `scripts/kanata.sh chatgpt login` when needed. A catalog entry and an inference result are separate evidence.
- Preserve the opt-in `compose.kanata.chatgpt.yml` overlay and dedicated `KANATA_CHATGPT_STATE_DIR` bind. It must match `[chatgpt_auth].state_dir`, remain writable and private, and stay outside the repository. The public container must not mount it. See [deployment setup](../../deploy/docker/README.md#sign-in-with-chatgpt).
- Selectable pinned families require adapter `reasoning_control = true`. All routes in a family must share the adapter, upstream and route policies/caps; only the pinned effort differs. Validation reports `inconsistent_reasoning_family` for incompatible mappings. Keep independently configured models under distinct aliases.
- ChatGPT pins use `routes[].reasoning_effort`; Codex retains `codex_reasoning_effort`. For new namespaced ChatGPT families, pin the unsuffixed route to the account default: `low` for this account's `gpt-5.6-sol`, `medium` for the other configured models. The account catalog exposes supported/default levels; unrecognized levels such as `ultra` are reported separately and cannot be configured. Luna levels follow the [official model reference](https://developers.openai.com/api/docs/models/gpt-6-luna) and require live inference checks independently of catalog evidence.
- Existing secrets, URLs and publication allowlists need no migration for this change. Deploy code changes by rebuilding/restarting the gateway and updating the host binary; key edits use the existing hot reload.

Changes remain under `Unreleased`; no release or remote push is part of this handoff. Grouped dispatch and authorization have fixture coverage; the live model checks and their limits are listed above.
