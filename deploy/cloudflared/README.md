# Public ingress through a Cloudflare tunnel

Public clients reach Kanata's **public listener** through a remotely managed Cloudflare tunnel. The public listener runs in its own `kanata-public` container (`kanata serve --plane public`), started from the same image and config. That process loads only the routes in `public_routes`, the adapters they use, and non-owner keys trimmed to their public permissions. It never loads the owner key, so use a separate non-owner key for public routes (`scripts/kanata.sh key new`). It has no Codex adapter, credentials or volume, and no private listener. The private `kanata` container runs `--plane private` and is not on the public network.

Nothing is host-published: `cloudflared` makes outbound connections to Cloudflare, and the tunnel forwards to `172.29.0.2:8081` on the internal `kanata_public_origin` network, which only `kanata-public` and `cloudflared` join.

The examples use `api.example.com`; replace it with your own hostname. Pick a **first-level** name such as `kanata-api.example.com`. Cloudflare's free Universal SSL does not cover deeper names like `api.kanata.example.com`.

## Public listener rules

- Every forwarded request without a valid key gets **403**, on any path, before routing.
- A valid key sees and calls only models that are both in `publication.public_routes` **and** in that key's permissions. Everything else gets 403. Unknown paths get 404.
- Codex can never be public, even with the owner key. `kanata check` rejects Codex entries in `public_routes`.
- Cloudflare Access is optional extra protection; it never replaces bearer keys.
- Responses and `/v1/models` show only the alias, never the upstream model id. To keep the model private, give public routes an opaque alias (e.g. `kanata-mini`) with its own `[[routes]]` entry. The model may still name itself when asked.

## One-time setup (Cloudflare dashboard)

1. **Create the tunnel:** Cloudflare One (Zero Trust) → **Networks → Tunnels → Create a tunnel** → **Cloudflared**. Give it a name.
2. **Save the token:** on the connector page, copy only the **token**, the long string after `--token`. Don't run the command shown there. Save it to a file outside the repository without printing it:
   ```sh
   pbpaste > ~/.config/kanata/cloudflared-tunnel-token   # or paste with an editor
   chmod 444 ~/.config/kanata/cloudflared-tunnel-token
   ```
3. **Add the public hostname:** on the tunnel's **Public Hostname** (published application routes) tab, add your hostname with service **HTTP** `172.29.0.2:8081` and an empty path. Map nothing else to the tunnel. Cloudflare creates the proxied DNS record.

   Changing a hostname later means editing it on this tab. Renaming only the DNS record leaves the tunnel without a matching rule, and requests get 404.

## Enable

1. Add the public listener and allowlist to `config/config.toml`, as shown in [`config/container.public.example.toml`](../../config/container.public.example.toml):
   ```toml
   [listeners.public]
   bind = "172.29.0.2"
   port = 8081

   # under [publication]
   public_routes = [{ model_alias = "qwen3-0.6b", operation = "chat" }]
   ```
2. In `.env`, append `:compose.kanata.public.yml` to `COMPOSE_FILE`, and set `KANATA_CLOUDFLARED_TOKEN_FILE` to the token file's absolute path.
3. Start the stack and watch the connector:
   ```sh
   docker compose up -d
   docker compose logs -f cloudflared   # expect "Registered tunnel connection"
   ```

The `cloudflared` sidecar is digest-pinned, runs as non-root with a read-only filesystem and all capabilities dropped, and has auto-update disabled. It reads the token from a Compose secret file, and its metrics stay on container loopback.

## Keys for other people

Issue one key per person. The config stores only the key's SHA-256 digest:

```sh
scripts/kanata.sh key new alice ~/.config/kanata/keys/alice.key --chat qwen3-0.6b
# or, with a Rust toolchain:
cargo run -q -- key new --id alice --chat qwen3-0.6b --key-out ~/.config/kanata/keys/alice.key
```

1. Paste the printed `[[application_keys]]` block into `config/config.toml`, then run `docker compose restart kanata`.
2. Send the key file's contents over a private channel, together with [the API quickstart](../../docs/guides/public-api-quickstart.md).
3. **Revoke:** delete the block and restart. **Rotate:** issue a new key under a new `id`, then revoke the old one.

Scope these keys only to public-allowed, non-Codex aliases. A key also works on the private route for anyone who can reach it.

## Verify

```sh
H='Authorization: Bearer '"$(cat ~/.config/kanata/keys/alice.key)"
curl -s -o /dev/null -w '%{http_code}\n' https://api.example.com/v1/models                   # 403 (no key)
curl -s -H "$H" https://api.example.com/v1/models                                            # 200, allowlisted models only
curl -s -o /dev/null -w '%{http_code}\n' -H "$H" https://api.example.com/nope                 # 404
```

## Disable or rotate

- **Stop public traffic:** remove `:compose.kanata.public.yml` from `COMPOSE_FILE`, remove `[listeners.public]` and `public_routes` from the config, then run `docker compose up -d --remove-orphans`.
- **Remove the route:** delete the public hostname, or the whole tunnel, in the dashboard.
- **Rotate the tunnel token:** refresh it in the dashboard, replace the file, then run `docker compose up -d`. Old connectors stay connected until they restart.

## Residual risk

The public process holds no Codex credentials and never accepts the owner key. It still mounts the whole read-only config file, so a compromised container could read key ids and digests, private route names and adapter URLs (but no plaintext keys or Codex credentials). It also shares the host, the Docker daemon and backends such as Ollama with the private one. Docker networks alone don't isolate containers from each other or from the host (on macOS/OrbStack, bridges can reach each other), so a compromised public container could still reach the private listener's address, where it would need a valid key. A public route on an adapter that needs a secret (e.g. OpenRouter) needs that secret mounted into `kanata-public` too; without it, `kanata-public` fails to start. Expose publicly only models and keys whose compromise you'd accept.
