## Versioning and CHANGELOG
- Releases follow semantic versioning from `1.0.0`: patch for fixes, minor for compatible features, major for breaking changes to the client API, configuration schema or key file format. Choose the bump from the `Unreleased` section unless told otherwise.
- Record user-facing or security-relevant changes under `Unreleased` until a bump is authorized. Distinguish fixture evidence from live availability; never imply a release or deployment.

## Style
- Comments: short, state what the code does, not why or how.
- Tests: no tests for obvious or trivial behaviour.

## Repository map
Cargo workspace (edition 2024, `rust-version = "1.98"`, shared `[workspace.package]` and `[workspace.dependencies]` in the root `Cargo.toml`): an OpenAI-compatible inference gateway with a private listener, an optional public listener and a loopback admin listener. The root package `kanata` is the binary.
- `src/`: root crate: `main.rs`, `cli.rs` (`build_serve_adapters` is the one place that maps `ProviderKind` to concrete adapters), `serve.rs` + `serve/`, `portal/` (host-only loopback dashboard, never mounted on gateway listeners), `diagnostics.rs` (`doctor`/`health`), and a `lib.rs` facade (`kanata::{core, config, keys, adapter, server, routing, auth}`) that `tests/` import.
- `crates/kanata-core`: provider-neutral IR, `ErrorKind` and its `mapping()`, the `Adapter` trait (`adapter.rs`), the SSE framer, `str_enum!`/`getters!` (`macros.rs`; use them instead of hand-writing enum string tables or trivial getters). No tokio, hyper or axum.
- `crates/kanata-contract`: client-facing wire contract (error envelope and code/type strings, `/v1/models` `kanata` metadata, wire enums, limits, SSE event names); serde only, no transport.
- `crates/kanata-config` (see its `AGENTS.md`): config schema and validation, the `keys.toml` schema, secret resolution.
- `crates/kanata-keys`: `kanata key` CLI, locked key store, quota and usage ledgers; its manifest has no network crate.
- `crates/kanata-adapters` (see its `AGENTS.md`): concrete adapters and the outbound transport.
- `crates/kanata-server` (see its `AGENTS.md` and `src/api/AGENTS.md`): listeners, auth, routing, admission, handlers, telemetry; never depends on `kanata-adapters`.
- `tests/` (see `tests/AGENTS.md`): integration tests of the root package.
- `config/`: `*.example.toml` templates and `config/README.md`. `config/config.toml` is the live, git-ignored config.
- `deploy/`: `compose/` (base `compose.kanata.yml`, `compose.kanata.public.yml`, opt-in overlays; run Compose from the repo root, `.env` lists them in `COMPOSE_FILE`), `docker/` (`Dockerfile`, build context is the repo root), `scripts/kanata.sh` (operator helper; `help`), runbooks in `docker/` and `cloudflared/`.
- `docs/guides/`: operator and client guides; `public-api-quickstart.md` holds the client error table.
- Not part of the repo: `research/`, `luna-sonata/`, `.opencode/`.

## Commands
- CI (`.github/workflows/ci.yml`): `cargo fmt --all -- --check`; `cargo clippy --locked --workspace --all-targets --all-features -- -D warnings`; `cargo test --locked --workspace --all-targets`; `node --test tests/guide/*.test.mjs`; `python3 tests/deploy/helper_test.py`; `scripts/check-templates.sh`.
- One suite: `cargo test --test <stem>`; module unit tests: `cargo test -p <crate> --lib <filter>` (e.g. `-p kanata-server --lib parser_mutation_smoke`).
- Validate a config: `cargo run -q -- check --config <path> [--plane all|private|public]`, or `deploy/scripts/kanata.sh check` for the Compose config.

## Conventions
- User-facing or security-relevant changes also update, as applicable: `CHANGELOG`, `README.md`, `config/README.md`, `config/*.example.toml`, the quickstart error table.
- Security invariants (`SECURITY.md`): exact `(model_alias, operation)` routing with no fallback, wildcard or retries; private-only providers (ChatGPT, CLIProxyAPI, Anthropic) are never public, and the public plane never loads the owner key or keys scoped to them; secrets never enter the repo (keys stored as `sha256:` digests; `*.key`, `.env`, live config are ignored).
- Commits: Conventional Commits with a scope and a one-sentence body (`feat(admission): ...`). Releases: `chore(release): X.Y.Z`, bumping `version` under `[workspace.package]` in the root `Cargo.toml`, `Cargo.lock`, `info.version` in `docs/openapi.json` (a test compares it to the crate version), the README version badge and dating the `CHANGELOG` section. Work on `dev`; `master` takes PRs from `dev`.
