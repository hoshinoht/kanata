## Feature rules
`Kanata` is currently in early stage development, so when bumping features in `CHANGELOG`, start from 0.0.1 and onwards, where

- 0.1.0: major feature
- 0.0.1: minor feature
- 1.0.0: release
- 1.0.0b: beta release, a pre-release, etc.

- Update `CHANGELOG` alongside user-facing features or security-relevant behavior. Keep work under `Unreleased` until a version bump is authorized; distinguish fixture evidence from live availability and do not imply a release or deployment.

## Commenting Rules
- Do not use overly verbose comments, keep the what, and not the why or how. 
- The comment should be clear and concise, explaining the purpose of the code.

## Tests
- When designing tests, be reasonable and avoid overtesting, thins that are obvious or trivial should be skipped.

## Repository map
Kanata is a single Rust crate (edition 2024, `rust-version = "1.98"`): an OpenAI-compatible inference gateway with a private listener, an optional public listener and a loopback admin listener.

- `src/`: the crate; see `src/AGENTS.md` (module map, layering), `src/api/AGENTS.md` (HTTP boundary), `src/adapter/AGENTS.md` (providers, transport).
- `tests/`: integration suites and fixtures; see `tests/AGENTS.md`.
- `config/`: `*.example.toml` templates plus `config/README.md` (template index, config concepts). `config/config.toml` is the live, git-ignored config.
- `deploy/docker/`, `deploy/cloudflared/`: Compose and public-tunnel runbooks (READMEs).
- `Dockerfile`, `compose.kanata.yml` (private base), `compose.kanata.public.yml` (separate `kanata-public` container, `--plane public`), `compose.kanata.host-ollama.yml`, `compose.kanata.openrouter.yml` (opt-in overlays).
- `scripts/kanata.sh`: operator helper (`build`, `check`, `up`, `down`, `restart`, `status`, `logs`, `codex`, `key new`, `owner-key rotate`, `ollama-context`, `fm-serve`); run `scripts/kanata.sh help`.
- `docs/architecture/kanata-mvp.md` (contracts, plane boundaries), `docs/guides/public-api-quickstart.md` (client guide, error table).
- Ignored, not part of the repo: `research/` (local security/threat docs), `luna-sonata/` (separate project with its own AGENTS.md), `.opencode/`.

## Commands
- CI (`.github/workflows/ci.yml`, runs on `master`/`dev` pushes and PRs):
  - `cargo fmt --all -- --check`
  - `cargo clippy --locked --all-targets --all-features -- -D warnings`
  - `cargo test --locked --all-targets`
  - `for f in config/*.example.toml; do cargo run --locked -q -- check --config "$f"; done`
- One suite: `cargo test --test <file-stem>` (e.g. `--test admission`); unit tests in a module: `cargo test --lib <filter>`.
- Validate a config: `cargo run -q -- check --config <path> [--plane all|private|public]`, or `scripts/kanata.sh check` for the Compose config.
- CLI surface (`src/cli.rs` `USAGE`): `check`, `serve`, `auth codex {login,status,logout}`, `key new`.

## Conventions
- User-facing or security-relevant changes also update, as applicable: `CHANGELOG` (`Unreleased`), `README.md`, `config/README.md`, `docs/guides/public-api-quickstart.md` (error table), `config/*.example.toml`.
- Security invariants (see `SECURITY.md`, README "Security"): exact `(model_alias, operation)` routing with no fallback, wildcard or general retries; Codex is never public and the public plane never loads the owner key or Codex-scoped keys; secrets never enter the repo (`*.key`, `.env`, live config are ignored; keys are stored as `sha256:` digests).
- Commits: Conventional Commits with a scope and a one-sentence body (`feat(admission): ...`, `fix(server): ...`, `docs(readme): ...`). Releases are `chore(release): X.Y.Z-beta.N`, bumping `Cargo.toml`, `Cargo.lock`, the README beta badge and moving `CHANGELOG` `Unreleased` into a dated section. Work happens on `dev`; `master` receives PRs from `dev`.
