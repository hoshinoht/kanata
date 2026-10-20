## Contract
- `Adapter` (`mod.rs`): `execute(RoutedRequest)` returns `AdapterOutput::Complete` or `Events` of `NormalizedEvent`s; failures are `GatewayError`s, never provider text.
- Validate inside `execute` before any network call and return `InvalidRequest`/`UnsupportedOperation` for local rejections; the circuit breaker treats these as neutral.
- Use the shared helpers in `mod.rs` (`status_error`, `upstream_failure`, `internal_error`, `invalid_request`, `unsupported_operation`) and `GatewayError::from(kind)`. Status mapping: 400/422 → `InvalidRequest`, 404 → `NotFound`, 401/403/408/503/504 → `UpstreamUnavailable`, 429 → `RateLimited`, else `UpstreamFailure`; Anthropic adds 529 → `UpstreamUnavailable`.
- No retries. The only exception is Codex's single token refresh after a 401 received before any output (`codex/provider.rs` `open_response`).

## Layout
- One directory per provider, split by concern (`mod.rs`, `request.rs`, `response.rs`, `stream*.rs`, `validation.rs`).
- `chat_completions/`: shared OpenAI-compatible chat code for `vllm/`, `ollama/` (incl. Apple FM) and `openrouter/`. Provider differences are explicit parameters (`StreamDialect`, `MessageRules`, strict vs lenient wire types); provider envelopes and their serde strictness stay in the provider modules.
- `auth_files.rs`: owner-only state directory, lock and atomic record writes shared by `codex/auth` and `chatgpt/auth`.
- `transport/`: the only HTTP client (hyper + rustls, no pool, bounded bodies, DNS lookups capped, credentials only over HTTPS or plain HTTP to a literal loopback IP). An adapter `ca_file` replaces platform roots for that adapter only.
- `diagnostics.rs`: sanitized provider errors; never log tokens, headers or bodies; provider messages DEBUG only.

## Provider rules that are easy to break
- vLLM: with `reasoning_control`, `reasoning_effort` is sent top-level and never with `chat_template_kwargs.enable_thinking`. CLIProxyAPI reuses this path, chat only, private-only, and still sends a route's pinned effort.
- Anthropic: private-only, pinned origin; `thinking` is never sent. Signed thinking blocks become `ReasoningItem`s prefixed `anthropic_thinking_`/`anthropic_redacted_thinking_` only when encrypted reasoning is requested; replay converts only those prefixes and skips foreign items.
- ChatGPT shares the Responses mapper in `codex/protocol` and `codex/stream`.

## Adding a provider kind
1. `src/config.rs`: `ProviderKind` variant, its methods and kind-specific validation (zone, URL, secrets).
2. Module here implementing `Adapter` on `transport::Transport` (reuse `chat_completions/` if OpenAI-compatible); declare it in `mod.rs`.
3. `src/cli.rs` `build_serve_adapters`: construct it.
4. Fixtures in `tests/fixtures/<provider>/`, `tests/adapter_<provider>.rs`, a template entry, `config/README.md` and the README backends table; add the name to `tests/architecture_boundaries.rs` if it must stay out of other layers.
