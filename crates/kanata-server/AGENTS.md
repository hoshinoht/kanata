## Request flow
`server/runtime.rs` accept → auth (`server/mod.rs` `Authenticated`) → handler (`api/dispatch.rs`: parse, resolve route, authorize, capability checks) → `Admission::acquire` → `adapter.execute` → `permit.record_outcome` (breaker) → JSON (`api/serialization.rs`) or SSE (`api/lifecycle.rs` holds the permit for the stream's life).

## Where things live
- `server/`: listeners, client/public/admin routers, config and key reload; `server/guide*` is the only unauthenticated content route (`GET`/`HEAD /v1`, `/v1/`). Keep it narrow and free of secrets or deployment inventory.
- `api/`: client handlers (see `api/AGENTS.md`); `routing/`: registry, admission, breaker, probes, `kanata routes`; `auth/`: application-key auth (secret resolution lives in `kanata-config` `secret.rs`); `telemetry/`: access log, metrics, logging; `key_reload.rs`: keys-file hot reload.
- The server uses the `Adapter` trait from `kanata_core::adapter` only; concrete adapters are built by the root `src/cli.rs` and never enter this crate's dependency graph.

## Layering
- `tests/architecture_boundaries.rs` fails if `ollama`, `openrouter`, `openai`, `cliproxy`, `anthropic`, `chatgpt`, `vllm`, `apple_fm` or `codex` appear (case-insensitive, comments included) in any `.rs` file under this crate, `kanata-core`, `kanata-keys`, `kanata-contract` or the root `src/` (`main.rs` and `cli.rs` exempt), or if this crate's `[dependencies]` names `kanata-adapters`. Use `core` types and `ProviderKind` methods (e.g. `label()`).
- This crate never references `keys::store` or the key CLI (the same test enforces it).
- Metric labels are bounded enums, never adapter IDs or free text. The only keyed series uses configured key IDs and aliases.
- Log targets are explicit (`kanata::access`, `kanata::lifecycle`, `kanata::keys`, `kanata::upstream`, `kanata::telemetry`); keep `target:` on every event so filters do not depend on crate names.
