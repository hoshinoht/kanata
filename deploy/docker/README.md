# Running Kanata with Docker

One Kanata process runs in a hardened container: distroless, fixed non-root UID `10001`, read-only root filesystem, all capabilities dropped, `no-new-privileges`, and PID, CPU and memory limits. **No Compose profile publishes a host port.** Traffic reaches Kanata only over internal Docker networks:

| Listener | Address | Reached by |
| --- | --- | --- |
| Private client | `172.30.0.2:8080` on `kanata_private_ingress` (internal, fixed name) | Your reverse proxy (e.g. a tailnet-only Caddy) |
| Public client (optional) | `172.29.0.2:8081` on `kanata_public_origin` (internal) | `cloudflared` only; see [../cloudflared/README.md](../cloudflared/README.md) |
| Admin | `127.0.0.1:9090` inside the container | Nothing outside the container |

Kanata makes its outbound calls to model backends over the separate `backend_egress` network.

## Files

| File | Purpose |
| --- | --- |
| `compose.kanata.yml` | Base service: private listener, egress network, Codex volume, read-only config mount |
| `compose.kanata.host-ollama.yml` | Opt-in `host.orb.internal` mapping so Kanata can reach Ollama running on the Docker host (e.g. macOS for Metal) |
| `compose.kanata.public.yml` | Opt-in public plane: a separate `kanata-public` container (`--plane public`, no Codex volume) on the public network, the `cloudflared` sidecar, and `--plane private` for the main container |
| `compose.kanata.openrouter.yml` | Opt-in OpenRouter API key as a Compose secret at `/run/secrets/openrouter-api-key`, from `KANATA_OPENROUTER_KEY_FILE` |
| `compose.kanata.omnilion.yml` | Opt-in OmniLion API key as a Compose secret at `/run/secrets/omnilion-api-key` in both `kanata` and `kanata-public`, from `KANATA_OMNILION_KEY_FILE`; needs `compose.kanata.public.yml` |
| `compose.kanata.chatgpt.yml` | Opt-in writable ChatGPT credential directory in the private container; host browser sign-in uses the same protected directory |
| `compose.kanata.chatgpt-only.yml` | Apply after the ChatGPT overlay to replace private mounts and detach the retained Codex credential volume |
| `.env` (git-ignored; from `.env.example`) | `COMPOSE_FILE`, `COMPOSE_PROJECT_NAME`, and paths to the config, the host-side owner key and the tunnel token. Paths only, never secrets |

## Config and secrets

- `KANATA_CONFIG_FILE` is mounted read-only at `/etc/kanata/config.toml`. Start from `config/container.example.toml`.
- **Client keys** live in `keys.toml` as `sha256:` digests only; the owner key's plaintext stays on the host at `KANATA_OWNER_KEY_FILE`. Manage keys with the host CLI (`cargo install --locked --path .`, then `kanata key ...`); changes apply within about 2 s, no restart.
- **Keys and state mounts** (required): `KANATA_KEYS_DIR` must be `<config dir>/keys` and `KANATA_STATE_DIR` must be `<config dir>/state`, the same paths the host CLI derives from the config's `[keys]` table. The keys directory is mounted read-only at `/etc/kanata/keys` in both containers; `state/private` and `state/public` are mounted writable at `/etc/kanata/state` in `kanata` and `kanata-public`. `scripts/kanata.sh` checks these paths and creates missing directories (0700).
- **Why a directory:** the CLI replaces `keys.toml` by atomic rename. A single-file bind mount keeps pointing at the old inode, so the container would never see the change; mounting the directory does.
- **Ownership:** the container runs as uid 10001 and must read `keys.toml` (0600).
  - macOS: when the Docker runtime maps host ownership, run `kanata key ...` as your own user. Verify that the container can read the key file before starting services.
  - Linux: `sudo chown -R 10001:10001 <config dir>/keys <config dir>/state`, then run the CLI as the container uid: `sudo -u '#10001' kanata key ... --config <config>`. Do not change the container uid.
- **File permissions:** bind mounts and Compose file secrets keep their host ownership and mode. Make the config file, and the tunnel token if used, readable by the container user, e.g. `0444` inside a `0700` directory.
- **Codex credentials** live only in the dedicated `codex_state` volume. Log in with `scripts/kanata.sh codex login`. No host home directory, keychain or Docker socket is mounted.

## Host key portal

Install the host binary with `cargo install --locked --path .`, then run:

```sh
scripts/kanata.sh portal
# Optional: scripts/kanata.sh portal --port 9092
```

Keep this terminal open. It prints the loopback URL and one-use login code. The portal edits the deployed `keys/keys.toml` and reads usage from `state/private` and `state/public`; existing gateway keys and provider secrets stay in place. It runs on the Docker host, so it adds no Compose service or published port. See [private portal](../../docs/guides/private-portal.md) for login, rotation and expiry behavior.

## Sign in with ChatGPT

Use a dedicated directory outside the checkout, owned by the user running the host auth command with mode `0700`. Add `compose.kanata.chatgpt.yml` to `COMPOSE_FILE` in `.env`, and set:

```dotenv
KANATA_CHATGPT_STATE_DIR=/absolute/path/outside/repo/kanata-chatgpt
```

Set `[chatgpt_auth].state_dir` in the deployed config to **that same absolute path**. The overlay mounts the directory at the identical path in the private container. Host sign-in and container inference share one installation's saved host identity and refresh lock. No credential directory is mounted in `kanata-public`; existing API-key Compose secrets and the Codex volume remain separate.

Add the `chatgpt` adapter from [`config/chatgpt.example.toml`](../../config/chatgpt.example.toml), then:

```sh
scripts/kanata.sh chatgpt login
scripts/kanata.sh chatgpt status
scripts/kanata.sh chatgpt models
```

Complete sign-in in a browser **on the Docker host**. Select a returned account model `slug` for an explicit private chat route, then grant its alias to a gateway key using the portal or host CLI. Run `scripts/kanata.sh build` and `scripts/kanata.sh restart` to install the image and credential mount. Later route changes can use the [configuration reload](../../docs/guides/config-reload.md) procedure.

On macOS, verify that the Docker runtime maps host ownership and the private container can access the protected state directory. On Linux, keep container UID `10001`: create/chown the dedicated directory to `10001:10001` and run host auth commands as that UID, as with key management. Keep credentials `0600`; do not broaden permissions to make them readable. For a remote Docker host, use the [official self-hosted procedure](https://developers.openai.com/siwc/token-sharing-open-source/self-hosted-vms); a browser on another device cannot reach its loopback callback directly.

The directory must stay writable because refresh rotates saved credentials. A static Compose secret cannot replace this state store. Verify sign-in consent, the account catalog and a short inference request with your account before relying on the integration.

### Retire the experimental Codex provider

Keep the adapter implementation and `config/codex.example.toml` for restoration. Back up the operator-managed config, then replace its Codex routes with explicit ChatGPT routes using the same aliases and upstream IDs. Replace `codex_reasoning_effort` with `reasoning_effort`; explicitly pin formerly unpinned Codex defaults to `medium`. Remove `codex_reasoning_summary`, the unused Codex adapter and `[codex_auth]`. Preserve existing key scopes and publication rules. Verify each service's requested models through ChatGPT before switching.

Append `compose.kanata.chatgpt-only.yml` after `compose.kanata.chatgpt.yml` in `COMPOSE_FILE`, then run `scripts/kanata.sh check` and `scripts/kanata.sh restart`. This overlay requires [Compose support for `!override`](https://docs.docker.com/reference/compose-file/merge/#replace-value) (2.24.4 or newer). It replaces the private service's volume mounts with config, keys, usage state and ChatGPT state; include any other custom mounts explicitly. API-key secrets and public service mounts are preserved. The existing `codex_state` volume stays on disk and is unmounted; do not delete it when retaining a restoration path.

To restore Codex, remove the ChatGPT-only overlay, restore the saved Codex adapter/auth configuration and selected exact routes, reconcile key grants against those routes, validate, and recreate the gateway. The base Compose file mounts the retained volume again. Restoration is an explicit operator action; failed ChatGPT requests never fall back to Codex.

## Private route (reverse proxy)

Kanata owns the `kanata_private_ingress` network. Attach your proxy to it as an external network, and publish **only the proxy**, on the one host address you intend, e.g. the host's tailnet IP. A minimal Caddy example:

```yaml
# In your proxy's compose file
services:
  caddy:
    networks:
      kanata_private_ingress:
        ipv4_address: 172.30.0.3
networks:
  kanata_private_ingress:
    external: true
```

```caddy
kanata.example.com {
	reverse_proxy 172.30.0.2:8080
}
```

- **Proxy placement:** give the proxy its own gateway network for publishing and ACME. Docker publishes ports only through the network that provides a container's default gateway, and an `internal` network never provides one. That is why Kanata itself can't be published from `kanata_private_ingress`.
- **Certificates:** for a tailnet-only hostname, use an ACME DNS challenge, for example Caddy with a DNS provider module.
- **Shared proxies:** if one proxy serves several services, bind its listener to its own egress address (Caddy `default_bind`). Otherwise services on the attached networks can reach each other through it.

## Start

Start Kanata first; it creates the network the proxy joins.

```sh
scripts/kanata.sh up     # validates the config first
scripts/kanata.sh logs
```

Before `docker compose down`, stop or detach the proxy: a network can't be removed while another container is attached to it.

## Host boundary

- **Docker bridges are not a firewall.** On Docker Desktop and OrbStack, containers can reach each other across networks and can reach host services. On Linux, pair these profiles with a deny-by-default host firewall. It should allow only the proxy's published port on the intended interface and Kanata's required backend egress. Prefer rootless Docker or user-namespace remapping.
- **What the image doesn't prove:** the non-root, read-only image limits what a compromised Kanata process can do inside the container. It does not establish network isolation, DNS integrity, or provider availability.
