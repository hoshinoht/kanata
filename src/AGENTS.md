## Module map
- `main.rs`: parses args, starts a tokio runtime for `auth`/`serve`, prints CLI results.
- `cli.rs`: command parsing, `check`, `routes`, dispatch to `keys::cli`, Codex auth commands, and `build_serve_adapters` (the one place that matches `ProviderKind` to concrete adapters).
- `serve.rs`: `serve` lifecycle: load config, `for_plane`, logging, build server and adapters, bind, `serve_until`.
- `config.rs`: TOML config schema, validation and `ValidatedConfig` (see below).
- `core/contracts.rs`: provider-neutral IR: selectors, `RoutedRequest`, chat/transcription request and response types, `NormalizedEvent`, `ErrorKind` and its `mapping()` to HTTP status/code/type.
- `keys/`: key lifecycle. `time.rs` UTC timestamps; `file.rs` `keys.toml` schema, 0600/permission checks, 1000-record cap; `reload.rs` hot reload (~2 s poll, invalid file keeps the old set); `usage.rs` `usage-<plane>.json` recorder and reader; `store.rs` locked atomic writes and `audit.jsonl`; `cli.rs` host-only `kanata key` commands (no network path).
- `auth/`: secret resolution, bearer-key authentication, per-key route authorization.
- `routing/mod.rs`: `Registry` of exact `(model_alias, operation)` routes. `routing/admission.rs`: per-route queues, optional adapter and key limits, token bucket, `Retry-After`. `routing/breaker.rs`: per-adapter circuit breaker.
- `server/mod.rs`: `TwoPlaneServer` assembly, client/public/admin routers, auth extractor, `/v1/models`, admin `/live` `/ready` `/metrics`, test entry points `client_oneshot`/`public_oneshot`/`admin_oneshot`. `server/runtime.rs`: listeners, connection caps, header read timeout, drain. `server/shutdown.rs`: connection sets.
- `telemetry/`: request `Observer` and middleware, access log, metrics (`labels.rs` bounded enums, `metrics.rs`), `sanitize.rs`, `logging.rs`.
- `api/`: HTTP handlers and OpenAI wire format; see `api/AGENTS.md`. `adapter/`: providers and transport; see `adapter/AGENTS.md`.

## Request flow
`server/runtime.rs` accept → auth (`server/mod.rs` `Authenticated`) → `api/chat.rs` (parse, resolve route, authorize, capability checks) → `Admission::acquire` → `adapter.execute` → `permit.record_outcome` (breaker) → JSON (`api/serialization.rs`) or SSE (`api/sse.rs`, `api/lifecycle.rs` holds the permit for the stream's life).

## Layering
- `tests/architecture_boundaries.rs` fails if `ollama`, `openrouter`, `codex` or `openai` appear in any `.rs` file under `src/` outside `adapter/`, `main.rs`, `config.rs` and `cli.rs`. The match ignores case and includes comments. Keep provider knowledge in those places; other layers go through `core` types and `ProviderKind` methods (e.g. `label()`, `supports_chat_options()`).
- Metric labels are bounded enums; never label metrics with adapter IDs or free text. The only keyed series uses configured key IDs and aliases.

## `config.rs` conventions
- `Raw*` structs are `#[serde(deny_unknown_fields)]`; `validate()` turns them into `Validated*` types with private fields and getters.
- Errors are `ConfigError::new(path, class)`, rendered `config error at <path>: <class>`, with short snake_case classes (`zero`, `limit_too_large`, `duplicate`, `timeout_too_large`). Never include secret values in paths or classes.
- Optional settings use `#[serde(default)] Option<_>`, with defaults applied in validation.
- `ValidatedConfig::for_plane` narrows a config per process: `Private` drops the public listener; `Public` keeps only public routes, their adapters and keys without owner/Codex scopes.
