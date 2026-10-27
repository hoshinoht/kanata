# kanata-contract

The client-facing wire contract of the kanata gateway, as plain serde types and constants:

- `errors`: the `{"error": {code, message, param, type}}` envelope and every `code` and `type` string the gateway sends.
- `models`: `GET /v1/models` bodies, including the `kanata` metadata member (operations, capability flags, image, embeddings and speech limits, the Responses and Messages endpoints, and the private-listener `admission` snapshot).
- `Operation`, `ReasoningEffort`, `TrustZone`, `SpeechFormat`: wire enums.
- `limits`: request and response bounds the gateway enforces (sampling ranges, `max_tokens` cap, stop sequences, reasoning, image, embeddings and speech limits).
- `events`: `event:` names of the typed Responses and Messages streams.

The gateway's own crates use these definitions, so they cannot drift from what it serves.

Clients such as kanade may depend on this crate by git tag:

```toml
kanata-contract = { git = "https://github.com/hoshinoht/kanata", tag = "v1.0.0" }
```

It depends only on `serde` and `serde_json` and has no transport: no HTTP client, server or async runtime.
