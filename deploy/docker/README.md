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
| `compose.kanata.public.yml` | Opt-in public listener network plus the `cloudflared` sidecar |
| `.env` (git-ignored; from `.env.example`) | `COMPOSE_FILE`, `COMPOSE_PROJECT_NAME`, and paths to the config, the host-side owner key and the tunnel token. Paths only, never secrets |

## Config and secrets

- `KANATA_CONFIG_FILE` is mounted read-only at `/etc/kanata/config.toml`. Start from `config/container.example.toml`.
- **Client keys** are stored in the config only as `sha256:` digests, so no key file is mounted. The owner key's plaintext stays on the host at `KANATA_OWNER_KEY_FILE`. `scripts/rotate-owner-key.sh` creates or rotates it and updates its digest.
- **File permissions:** bind mounts and Compose file secrets keep their host ownership and mode. Make the config file, and the tunnel token if used, readable by the container user, e.g. `0444` inside a `0700` directory.
- **Codex credentials** live only in the dedicated `codex_state` volume. Log in with `scripts/codex-login.sh`. No host home directory, keychain or Docker socket is mounted.

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
docker compose up -d
docker compose logs -f kanata
```

Before `docker compose down`, stop or detach the proxy: a network can't be removed while another container is attached to it.

## Host boundary

- **Docker bridges are not a firewall.** On Docker Desktop and OrbStack, containers can reach each other across networks and can reach host services. On Linux, pair these profiles with a deny-by-default host firewall. It should allow only the proxy's published port on the intended interface and Kanata's required backend egress. Prefer rootless Docker or user-namespace remapping.
- **What the image doesn't prove:** the non-root, read-only image limits what a compromised Kanata process can do inside the container. It does not establish network isolation, DNS integrity, or provider availability.
