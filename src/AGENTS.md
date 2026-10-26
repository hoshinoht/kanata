## Request flow
`server/runtime.rs` accept → auth (`server/mod.rs` `Authenticated`) → handler (`api/dispatch.rs`: parse, resolve route, authorize, capability checks) → `Admission::acquire` → `adapter.execute` → `permit.record_outcome` (breaker) → JSON (`api/serialization.rs`) or SSE (`api/lifecycle.rs` holds the permit for the stream's life).

## Where things live
- `cli.rs` `build_serve_adapters`: the one place that maps `ProviderKind` to concrete adapters.
- `core/contracts.rs`: provider-neutral IR, `ErrorKind` and its `mapping()` to HTTP status/code/type.
- `config.rs` + `config/`: raw schema (`raw.rs`), validation, catalogs, plane narrowing (`ValidatedConfig::for_plane`: `Private` drops the public listener; `Public` keeps only public routes, their adapters and keys without owner/private-provider scopes). `config/reload.rs` lists restart-only settings.
- `keys/`: host key store (`kanata key`, no network path); `portal/`: host-only loopback dashboard, never mounted on gateway listeners.
- `server/guide*`: the only unauthenticated content route (`GET`/`HEAD /v1`, `/v1/`). Keep it narrow and free of secrets or deployment inventory.
- `macros.rs`: `str_enum!` (enum ↔ string tables) and `getters!` (trivial field getters); use them instead of hand-writing either.

## Layering
- `tests/architecture_boundaries.rs` fails if `ollama`, `openrouter`, `openai` or `cliproxy` appear (case-insensitive, comments included) in any `.rs` file under `src/` outside `adapter/`, `config/`, `main.rs`, `config.rs` and `cli.rs`. Other layers use `core` types and `ProviderKind` methods (e.g. `label()`).
- Metric labels are bounded enums, never adapter IDs or free text. The only keyed series uses configured key IDs and aliases.

## Config conventions
- `Raw*` structs are `#[serde(deny_unknown_fields)]`; `validate()` produces `Validated*` types with private fields and getters.
- Errors: `ConfigError::new(path, class)` with short snake_case classes (`zero`, `duplicate`, `timeout_too_large`). Never put secret values in paths or classes.
- Optional settings: `#[serde(default)] Option<_>`, defaults applied in validation.
