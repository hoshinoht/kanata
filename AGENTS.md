## Versioning and CHANGELOG
- Releases are pre-releases `X.Y.Z-beta.N` (currently `1.0.0-beta.N`); bump `N` per release unless told otherwise.
- Record user-facing or security-relevant changes under `Unreleased` until a bump is authorized. Distinguish fixture evidence from live availability; never imply a release or deployment.

## Style
- Comments: short, state what the code does, not why or how.
- Tests: no tests for obvious or trivial behaviour.

## Repository map
Single Rust crate (edition 2024, `rust-version = "1.98"`): an OpenAI-compatible inference gateway with a private listener, an optional public listener and a loopback admin listener.
- `src/` (see `src/AGENTS.md`, `src/api/AGENTS.md`, `src/adapter/AGENTS.md`), `tests/` (see `tests/AGENTS.md`).
- `config/`: `*.example.toml` templates and `config/README.md`. `config/config.toml` is the live, git-ignored config.
- `deploy/`: `compose/` (base `compose.kanata.yml`, `compose.kanata.public.yml`, opt-in overlays; run Compose from the repo root, `.env` lists them in `COMPOSE_FILE`), `docker/` (`Dockerfile`, build context is the repo root), `scripts/kanata.sh` (operator helper; `help`), runbooks in `docker/` and `cloudflared/`.
- `docs/guides/`: operator and client guides; `public-api-quickstart.md` holds the client error table.
- Not part of the repo: `research/`, `luna-sonata/`, `.opencode/`.

## Commands
- CI (`.github/workflows/ci.yml`): `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --all-features -- -D warnings`; `cargo test --locked --all-targets`; `node --test tests/guide/*.test.mjs`; `python3 tests/deploy/helper_test.py`; `scripts/check-templates.sh`.
- One suite: `cargo test --test <stem>`; module unit tests: `cargo test --lib <filter>`.
- Validate a config: `cargo run -q -- check --config <path> [--plane all|private|public]`, or `deploy/scripts/kanata.sh check` for the Compose config.

## Conventions
- User-facing or security-relevant changes also update, as applicable: `CHANGELOG`, `README.md`, `config/README.md`, `config/*.example.toml`, the quickstart error table.
- Security invariants (`SECURITY.md`): exact `(model_alias, operation)` routing with no fallback, wildcard or retries; private-only providers (ChatGPT, CLIProxyAPI, Anthropic) are never public, and the public plane never loads the owner key or keys scoped to them; secrets never enter the repo (keys stored as `sha256:` digests; `*.key`, `.env`, live config are ignored).
- Commits: Conventional Commits with a scope and a one-sentence body (`feat(admission): ...`). Releases: `chore(release): X.Y.Z-beta.N`, bumping `Cargo.toml`, `Cargo.lock`, `info.version` in `docs/openapi.json` (a test compares it to the crate version), the README version badge and dating the `CHANGELOG` section. Work on `dev`; `master` takes PRs from `dev`.
