## Layout
- Each `tests/<suite>.rs` is one integration test binary; run it with `cargo test --test <suite>`.
- Larger suites keep a thin entry file that pulls in a same-named directory with `#[path = "<suite>/cases.rs"] mod cases;` plus suite-local `support.rs`. Examples: `admission`, `chat_contract`, `models_contract`, `sse_contract`, `error_contract`, `cancellation`, `shutdown`, `timeouts`, `metrics`, `redaction`, `adapter_vllm`, `adapter_openrouter`, `adapter_ollama`. Some split further (`transcription_contract` → `transcription/`, `extensions_contract` → `extensions/`).
- Small suites are single files (`config`, `routing`, `auth`, `listeners`, `smoke`, `serve_smoke`, `codex_*`, `adapter_codex`, `access_log`).
- Key lifecycle: `key_reload` (hot reload, expiry, revocation, public-plane narrowing), `key_usage` (usage state files), `key_cli` (host `kanata key` binary, concurrent writers, file permissions).
- `architecture_boundaries.rs` enforces the provider-name ban described in `src/AGENTS.md`.

## Shared helpers
- `support/gateway.rs`: `config()` (loads the fixture), `config_with_public_routes`, `server_with`/`try_server_with` (a `TwoPlaneServer` with recording test adapters), `adapter_spec`, `capabilities`, `models_request`, `response_json`. Its `Resolver` resolves every secret to `test-key`, so requests use `Authorization: Bearer test-key`.
- `support/mod.rs`: `run_kanata()` runs the built binary.
- Drive a server in-process with `client_oneshot`, `public_oneshot` and `admin_oneshot` (defined on `TwoPlaneServer` in `src/server/mod.rs`). Use real sockets only for connection-level behaviour (`metrics/socket.rs`, `serve_smoke.rs`).
- `admission/support.rs`: `pending_adapter` (a gated adapter with a dispatch probe), `failing_adapter`, `stream_adapter`, `poll_once`, and `config_with(.., edits)`.

## Fixtures and config variants
- `fixtures/config/example.toml` is the schema fixture every suite starts from. Make variants by string `replace` on it, write a unique temp file, `kanata::config::load` it, then delete it (`support/gateway.rs`, `tests/config.rs` `check`). Assert that the replacement actually changed the text (`assert_ne!` or `contains`).
- Provider payloads live in `fixtures/{openai,ollama,openrouter,vllm,codex}/`, contract JSON in `fixtures/contracts/`, and Codex auth flows in `fixtures/codex_login/`, `fixtures/codex_token/`.
- `tests/config.rs` also validates the `config/*.example.toml` templates, so template edits must keep them valid.

## Timing
- Prefer `#[tokio::test(start_paused = true)]` with `tokio::time::advance` and `poll_once` for queue and timeout logic when no real sockets are involved. With real sockets, paused time races auto-advance; use short real timeouts instead (see `metrics/socket.rs`, which uses `BoundTwoPlaneServer::with_header_read_timeout`).

## Live tests
- `ollama_live.rs` and one `serve_smoke.rs` case are `#[ignore]` and need real services (local Ollama `qwen3:0.6b`; `KANATA_TEST_PUBLIC_BIND`). Run them only when asked, with `-- --ignored`.
