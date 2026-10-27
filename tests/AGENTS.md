## Layout
- Each `tests/<suite>.rs` is one test binary (`cargo test --test <suite>`). Larger suites keep a thin entry file plus a same-named directory (`#[path = "<suite>/cases.rs"] mod cases;`, suite-local `support.rs`).
- In-crate test helpers live next to the crate that uses them (`crates/kanata-adapters/src/test_support.rs`; `SseFramer::feed` behind `kanata-core`'s `test-support` feature); unit tests stay next to the code.
- Unit tests in `crates/*` run with the crate directory as the working directory: reach repo files with `concat!(env!("CARGO_MANIFEST_DIR"), "/../../...")` or a relative `include_str!`.

## Shared helpers
- `support/scratch.rs`: self-cleaning temp paths/dirs/files and config loading from text. Declare it once per binary (`#[path = "support/scratch.rs"] mod scratch;`) and reach it from support modules via `crate::scratch`; loading it twice fails clippy (`duplicate_mod`).
- `support/gateway.rs`: fixture `config()`, `server_with` (a `TwoPlaneServer` with recording adapters from `AdapterSpec`), request/response helpers. Its resolver maps every secret to `test-key`. Prefer `AdapterSpec` over a new hand-written fake adapter.
- `support/keys.rs` (key-file fixtures), `support/http.rs` (mock upstream request reader), `support/template.rs` (isolated example templates), `support/mod.rs` (`run_kanata()`).
- Drive servers in-process with `client_oneshot`/`public_oneshot`/`admin_oneshot`; use real sockets only for connection-level behaviour.

## Fixtures
- `fixtures/config/example.toml` is the base config. Make variants by string replacement and assert the replacement changed the text.
- `tests/config.rs` validates `config/*.example.toml`; template edits must keep them valid.

## Timing
- Without real sockets, prefer `#[tokio::test(start_paused = true)]` with `tokio::time::advance`. With real sockets, paused time races; use short real timeouts.

## Live and manual tests
- `#[ignore]` tests (`ollama_live.rs`, `runtime_contention.rs`, one `serve_smoke.rs` case, the transport `connection_reuse_benchmark`) need real services or are manual; run them only when asked, with `-- --ignored`.
