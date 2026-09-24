## Layout
- `mod.rs`: `routes()` (`/chat/completions`, `/audio/transcriptions`, mounted under `/v1` by `server/mod.rs`) and capability checks (`supported`, `check_supported`).
- `chat.rs`, `transcription.rs`: endpoint handlers. `multipart.rs`: transcription form parsing. `extensions.rs`: extension allowlist validation.
- `wire/`: inbound client wire structs (`chat.rs`, `messages.rs`, `options.rs`, `tools.rs`) and their conversion into `core` types. `serialization.rs`: core responses to outbound JSON.
- `sse.rs` + `sse/state.rs`, `sse/encoding.rs`: streaming responses. `lifecycle.rs`: stream body that owns the `AdmissionPermit` until the stream ends. `stream_timeout.rs`: first-byte/idle timeouts. `deadline.rs`: `RequestDeadline`, the overall per-request budget.
- `errors.rs`: every error response (see below).

## Handler conventions
- Handler order, as in `chat.rs`: drain check → `RequestDeadline` → body read (bounded) → wire parse → route resolve and `auth.authorize` → capability and extension checks → `RoutedRequest` → `state.admission().acquire(route, auth.key_identity())` → `adapter.execute` → `permit.record_outcome(..)` → respond.
- Wrap each await that can stall (body read, admission, execute) in `deadline.run(..)`.
- Build errors only through `errors.rs`: `gateway_error_observed` for `GatewayError`, `admission_rejected_observed` for admission refusals (adds `Retry-After`), `invalid`/`invalid_param`/`body_too_large` for 4xx, `server_draining_observed` while draining. `*_observed` variants record the outcome on the telemetry `Observer`; use them whenever an observer exists.
- Status and code for a `GatewayError` come from `ErrorKind::mapping()` in `core/contracts.rs`. New client-visible codes also belong in `docs/guides/public-api-quickstart.md`'s error table.
- Reject unsupported fields with `400 invalid_request` and a `param` rather than ignoring them.
- Name no concrete provider here, not even in comments (see the layering rule in `src/AGENTS.md`); use `route.provider_kind` methods (e.g. `provider_label`).
