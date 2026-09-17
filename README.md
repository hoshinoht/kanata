# Kanata

Kanata is a lightweight Rust inference gateway for routing and adapting requests across local and remote AI runtimes.

> **Kanata serves inference. Kanata does not perform inference.**

Model execution, batching, tokenization, GPU scheduling, and model loading belong to runtimes such as Ollama, vLLM, SGLang, and hosted providers. Kanata owns the application-facing boundary: authentication, routing, transformation, admission, cancellation, timeout policy, trust zones, and observability.

## Project status

Kanata is in early development. Version `0.1.0` currently provides the Rust package and reviewed provider-neutral core contracts. It does **not** yet run an HTTP gateway or contact inference providers.

Implemented:

- Provider-neutral chat and transcription requests and responses
- Exact `(model alias, operation)` routed-request validation
- Object-safe adapter interface for complete and streaming output
- Tool declarations, choices, calls, and results without tool execution
- Trust zones, capabilities, timeout phases, and stable error mappings
- Validated transcription files and bounded namespaced extensions
- Sanitized fixtures and provider-boundary architecture tests

## Planned MVP

| Public contract | Route | Adapter |
| --- | --- | --- |
| `GET /v1/models` | Generated locally from authorized routes | None |
| `POST /v1/chat/completions` | Exact configured model alias | Ollama or Codex |
| `POST /v1/audio/transcriptions` | Exact configured model alias | OpenRouter |

The Codex adapter is gated on native ChatGPT OAuth and Responses-protocol feasibility. Provider-specific behavior remains inside in-tree adapters and must not leak into the core contract.

## Architecture

```text
OpenAI-compatible client
          │
          ▼
       Kanata
  auth · routing · limits
  transforms · telemetry
          │
          ▼
  provider-neutral adapter boundary
     ├── Ollama
     ├── OpenRouter
     └── Codex
```

Routes are selected by exact model alias and operation. Model listing is generated from the active routing registry rather than dispatched to a provider. Adapter execution accepts only a validated `RoutedRequest`.

See [the MVP contract](docs/architecture/kanata-mvp.md) for the current type and boundary decisions.

## Security direction

The MVP design requires:

- Separate public and loopback-only admin listeners
- Bearer-key scopes and permission-filtered model discovery
- No automatic retry or fallback across trust zones
- Client disconnect propagation through the complete stream lifetime
- Bounded admission, uploads, extensions, timeouts, and metric labels
- STRIDE analysis for every process, store, data flow, and trust boundary
- A non-root, shell-less, read-only Kanata container
- Cloudflared as a separately operated edge deployment with no admin/backend access

These are design requirements unless listed as implemented above; they are not deployment claims.

## Development

Requirements:

- Rust 1.98 or newer

Run the current checks:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```

The current placeholder binary reports its package version:

```sh
cargo run --quiet
# kanata 0.1.0
```

## Non-goals

Kanata is not an inference engine, GPU scheduler, model manager, agent framework, tool executor, training platform, or LiteLLM clone. Dynamic Rust plugin ABIs and a distributed control plane are outside the initial scope.

## License

Kanata is licensed under the [GNU General Public License v3.0](LICENSE).
