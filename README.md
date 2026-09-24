# Kanata

Kanata is a lightweight Rust inference gateway. It presents one OpenAI-compatible API in front of local and remote AI runtimes, and handles authentication, routing and policy for them.

> **Kanata serves inference. Kanata does not perform inference.**

Model execution, batching, tokenization, GPU scheduling and model loading belong to runtimes such as Ollama, vLLM and hosted providers. Kanata owns the application-facing boundary: static bearer keys with exact per-model scopes, routing, request validation, admission limits, cancellation, timeouts, and a separate public listener.

**Status: beta (`1.0.0-beta.1`).** Kanata runs as a single-host personal deployment. Interfaces may still change before 1.0.

## Features

- **OpenAI-compatible API:** `GET /v1/models`, `POST /v1/chat/completions` (JSON and SSE), and `POST /v1/audio/transcriptions` (multipart).
- **Exact routing:** each `(model alias, operation)` pair maps to one adapter and upstream model. One alias can serve several operations (e.g. chat plus transcription), and `/v1/models` lists only what the caller's key may use.
- **Adapters:**

  | Adapter | Scope |
  | --- | --- |
  | Ollama | Chat and tools |
  | vLLM | Text chat, audio chat, native ASR, and transcription through audio chat |
  | OpenRouter | Chat |
  | Codex | Via a ChatGPT sign-in; owner key only, private listener only |
- **Generation options:** `response_format` (JSON object / JSON schema), `temperature`, `top_p`, `seed`, `max_tokens` / `max_completion_tokens` and `reasoning_effort` are typed and bounded. Each adapter declares which options it supports, and unsupported options return a 400 naming the parameter instead of being dropped.
- **Tools:** tool declarations, calls and results pass through. Kanata never executes tools.
- **Keys:** static bearer keys with exact scopes. `kanata key new` issues `kanata_sk_…` keys stored in config only as SHA-256 digests.
- **Two client listeners in one process:** a private listener (typically behind a tailnet-only reverse proxy) and an optional public listener behind a Cloudflare tunnel. The public listener's model allowlist is empty by default, and it never serves Codex. A loopback-only admin listener serves `/ready` and `/metrics`.
- **Operational limits:** bounded admission, request bodies and uploads, phased timeouts, cancellation on client disconnect, and graceful drain.
- **Container:** a hardened image (distroless, non-root, read-only) with Compose profiles that publish **no** host ports.

## Architecture

```text
OpenAI-compatible client
     │ private: tailnet → your reverse proxy          public: Cloudflare tunnel
     ▼                                                  ▼
  Kanata private listener                            Kanata public listener
     └────────── auth · routing · limits · telemetry ──────────┘
                              │
                 provider-neutral adapter boundary
                 ├── Ollama   ├── vLLM   ├── OpenRouter   └── Codex
```

See [the contract notes](docs/architecture/kanata-mvp.md) for the core types and boundaries.

## Quick start (Docker)

Requirements: Docker with Compose v2, and a reverse proxy for the private route. Any proxy that can join a Docker network works, such as Caddy.

1. **Build the image:** `docker build -t kanata:test .`
2. **Create your config:** copy [`config/container.example.toml`](config/container.example.toml) to `config/config.toml` (git-ignored) and edit the tailnet address, adapters and routes. [`config/README.md`](config/README.md) explains the fields.
3. **Create `.env`** from [`.env.example`](.env.example). It holds only file paths and the Compose file list.
4. **Create the owner key, then start:**
   ```sh
   mkdir -p ~/.config/kanata && chmod 700 ~/.config/kanata
   scripts/rotate-owner-key.sh --no-restart   # key → ~/.config/kanata/owner-client-key; digest → config
   docker compose up -d
   ```
   The container never receives a plaintext client key: the config holds only `sha256:` digests.
5. **Wire up the private route:** attach your reverse proxy to the `kanata_private_ingress` network and forward to `172.30.0.2:8080`. See [deploy/docker/README.md](deploy/docker/README.md).
6. **Optional:** set up the public route with Cloudflare ([deploy/cloudflared/README.md](deploy/cloudflared/README.md)), log in to Codex (`scripts/codex-login.sh`), and issue keys for others (`scripts/new-client-key.sh`).

Hand testers [the API quickstart](docs/guides/public-api-quickstart.md).

## Development

Requires Rust 1.98 or newer.

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo run -q -- check --config config/container.example.toml
```

The offline schema fixture lives in `tests/fixtures/config/example.toml`; never serve it.

## Security notes

- **Keys:** keys are bearer credentials. Anyone holding one gets its scopes. Only one key may be `owner = true`, and only it may hold Codex scopes.
- **Public listener:** missing or invalid keys get 403 on every path, and so do models that aren't allowlisted. Cloudflare Access is optional defence in depth, never a replacement for keys.
- **Same process:** the public and private listeners share one process, which also holds the Codex credentials. A compromise through the public listener could expose those credentials, and the listener split does not prevent that. Expose publicly only what you would accept that risk for.
- **Codex:** Kanata calls ChatGPT's Codex backend with your own sign-in. That is an unofficial, private interface and may change. OpenAI's [Terms of Use](https://openai.com/policies/terms-of-use/) forbid sharing account access, so never expose Codex to other people.
- **Host isolation:** Docker bridges alone do not isolate containers from each other or from the host. Use a host firewall appropriate to your platform.
- **Suspected compromise:** run `scripts/rotate-owner-key.sh`. It writes a fresh owner key and recreates Kanata so the old key stops working, then prints what else to rotate: other client keys, the Codex sign-in, the tunnel token, and your proxy's DNS API token.

## Non-goals

Kanata is not an inference engine, GPU scheduler, model manager, agent framework, tool executor, training platform, or LiteLLM clone. Dynamic plugin ABIs and a distributed control plane are out of scope.

## License

Kanata is licensed under the [GNU General Public License v3.0](LICENSE).
