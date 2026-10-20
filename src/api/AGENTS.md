## Handler conventions
- Use the shared steps in `dispatch.rs` (`inbound`, `Inbound::read_json`, `Inbound::resolve`, `routed`, `Inbound::execute`); the order is drain check → deadline → upload reservation → bounded body read → parse → resolve and authorize → capability/extension checks → admission → execute → `record_outcome` → respond.
- Wrap every await that can stall in `deadline.run(..)`.
- Build error responses only through `errors.rs`; use the `*_observed` variants whenever an observer exists. Status and code come from `ErrorKind::mapping()`; new client-visible codes go in `docs/guides/public-api-quickstart.md`.
- Reject unknown fields with `400 invalid_request` and a `param` (the member's JSON path); only the documented hint-field allowlist in `wire/` and `responses/wire.rs` is accepted without a backend option.
- Event-typed streams (Messages, Responses) go through `sse/typed.rs` (`EventOutput`); chat streams through `sse.rs`.
- Name no concrete provider here, not even in comments; use `route.provider_kind` methods.
