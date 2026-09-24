# Kanata configuration

| File | Purpose |
| --- | --- |
| `config.toml` | **Your live config** (git-ignored). The repo `.env` points `KANATA_CONFIG_FILE` at it. |
| `container.example.toml` | Template for the private Docker deployment: private route via your reverse proxy, Ollama through `host.orb.internal`, Codex effort aliases, owner key |
| `container.public.example.toml` | The same plus the public listener for `kanata-api` (Cloudflare tunnel) and an empty public allowlist |
| `personal.example.toml` | Template for running `kanata serve` natively on a host, outside Docker. Shows vLLM text, audio-chat and native-ASR endpoints |

The offline schema fixture used by the tests lives in `tests/fixtures/config/example.toml`. Never serve it.

## Concepts

- **Routes** map an exact `(model_alias, operation)` pair to one adapter and upstream model. One alias can carry several operations. For example, a multimodal alias such as `omni` can be both `chat` (text plus inline audio) and `transcription` (native ASR). Each pair maps to exactly one backend, so a second backend for the same operation needs its own alias (e.g. `omni-transcribe`).
- **Aliases** may not contain `:` except a Codex effort suffix (`gpt-6-sol:low`, `:high`). An unsuffixed Codex alias uses medium effort.
- **Keys:** `secret_ref` is `file:/abs/path`, `env:NAME`, or `sha256:<hex>`. The `sha256:` form is for application keys only, and the server never holds that key in plaintext. Create keys with `kanata key new`; see `deploy/cloudflared/README.md`. Only one key may be `owner = true`, and only it may hold Codex scopes. Prefer `sha256:` for every key, including the owner; `scripts/rotate-owner-key.sh` manages the owner's digest.
- **Public exposure** is opt-in: `[listeners.public]` plus exact entries in `publication.public_routes`. Codex is never allowed there.

After any edit, validate offline: `cargo run -q -- check --config config/config.toml`.
