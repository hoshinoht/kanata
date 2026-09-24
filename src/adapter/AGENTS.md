## Contract
- `mod.rs`: `Adapter` trait (`id()`, `capabilities()`, `execute(RoutedRequest) -> AdapterFuture`). Output is `AdapterOutput::Complete(Response)` or `AdapterOutput::Events(EventStream)` of `NormalizedEvent`s; failures are `GatewayError` with an `ErrorKind`, never provider text.
- Adapters are built in `src/cli.rs` `build_serve_adapters` by matching `ProviderKind` (`new`/`from_config`), only for adapters that have a route.
- Validate the request inside `execute` before any network call, and return `InvalidRequest`/`UnsupportedOperation` for local rejections. The circuit breaker treats these as neutral, so keep that distinction.

## Layout
- One directory per provider, split by concern: `mod.rs` (adapter, `execute`, `status_error`), `request.rs` (IR → provider payload), `response.rs` (provider → IR), `stream.rs` + `stream_state.rs` + `stream_wire.rs` (streaming), `validation.rs` (capability and option checks).
  - `ollama/`: also `apple_fm.rs` (`ProviderKind::AppleFm` reuses the Ollama chat path) and `sse.rs` (re-export of the transport SSE framing).
  - `vllm/`: chat (streaming and tools when declared, also behind LiteLLM) and transcription (multipart or audio chat); construction rejects reasoning control, and a `secret_ref` without a resolved token.
  - `openrouter/`: tests in `tests.rs` and `tests/stream.rs`.
  - `codex/`: `provider.rs` (instead of `mod.rs` logic), `protocol/` (Responses wire protocol + tests), `auth/` (device login, token refresh with single-flight `RefreshCoordinator`, keyring/file credential store with a lock, pinned-TLS `net.rs`).
- `transport/`: the only HTTP client. HTTP/1 over hyper with rustls, per-request connection (no pool), `origin.rs` (URL and TLS rules, credentials only over HTTPS), `resolver.rs` (DNS lookups capped at 16), `time.rs` (connect/headers/first-byte/idle phases), `body.rs` (bounded bodies), `multipart.rs`, `sse.rs`. Tests live in `transport/tests/`.
- `diagnostics.rs`: `UpstreamLabel` and sanitized provider error parsing. It never logs tokens, headers or request bodies; provider messages are DEBUG only, with body reads capped at 8 KiB.

## Conventions
- Map upstream statuses with the per-provider `status_error` in the same way: 400/422 → `InvalidRequest`, 404 → `NotFound`, 401/403/408/503/504 → `UpstreamUnavailable`, 429 → `RateLimited`, anything else → `UpstreamFailure`.
- The only exception to the root no-retry rule is Codex's single refresh after a 401 received before any output (`codex/provider.rs` `open_response`).
- Unit tests stay next to the code (`#[cfg(test)]`, `tests.rs`, `provider_tests.rs`) and load fixtures from `tests/fixtures/<provider>/`.

## Adding a provider kind
1. `src/config.rs`: `ProviderKind` variant and its methods (`label`, `accepts_reasoning_effort`, `supports_chat_options`), plus any kind-specific validation (zone, URL, secrets).
2. New module here implementing `Adapter` on top of `transport::Transport`; declare it in `mod.rs`.
3. `src/cli.rs` `build_serve_adapters`: construct it.
4. Fixtures under `tests/fixtures/<provider>/`, an integration suite `tests/adapter_<provider>.rs`, and a template entry plus docs (`config/README.md`, README backends table).
