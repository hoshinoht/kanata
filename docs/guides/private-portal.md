# Private key portal

Run the portal **on the host device** that owns the Kanata config and keys:

```sh
kanata portal --config config/config.toml
```

Open the printed address, normally **http://127.0.0.1:9091**, in a browser on that same device. Enter the one-use login code printed in the terminal. Keep the terminal private. To choose a different local port:

```sh
kanata portal --config config/config.toml --port 9092
```

The config must use `[keys] file = "..."`. For an older inline-key config, migrate it first with `kanata key migrate --config <path>`. The portal does not read or reveal provider credentials.

## Manage a Docker deployment

Install the current host binary, then run the helper from the deployment checkout:

```sh
cargo install --locked --path .
scripts/kanata.sh portal
# Choose another host port if 9091 is occupied:
scripts/kanata.sh portal --port 9092
```

The helper reads the resolved Compose mounts, including `.env` and `COMPOSE_FILE`, and opens the deployed config on the host. It verifies that `KANATA_KEYS_DIR` is `<config dir>/keys`, `KANATA_STATE_DIR` is `<config dir>/state`, and `[keys]` uses `file = "keys/keys.toml"` and `usage_dir = "state"`. This makes the dashboard write the keys file mounted into the gateway and read persisted usage from its private and public state subdirectories. The gateway's read-only key mount remains in place; the host writes and the gateway reloads the file.

Keep the helper running in a private terminal, open its printed loopback URL on that host, and use the login code from the terminal. The helper starts a separate foreground host process; it adds no Compose service or published port. Ctrl-C stops that process. Starting or stopping it does not restart the gateway. Check `scripts/kanata.sh status` and gateway logs to confirm deployment health and key reloads after an edit.

The portal manages gateway keys and quotas. Set up account authentication separately with the [Sign in with ChatGPT guide](sign-in-with-chatgpt.md).

## Manage keys

Selectable reasoning families appear as one model with separate effort controls. The portal retains exact route scopes underneath, including duplicate default/medium grants on unrelated edits. Toggling an effort explicitly selects or clears its configured scopes. Clients discover one base ID and accessible levels; see the [service handoff](service-handoff.md).

- **Create:** choose a unique ID, exact model/operation scopes and expiry. Copy the generated secret from the one-time dialog. Only its SHA-256 digest is stored.
- **Inspect:** select a key to see its scopes, expiry, owner status, request and reported token usage, existing daily allowances, and missing route references.
- **Edit:** change scopes, expiry, maximum concurrent requests, request rate or daily request/token quotas. The quota selector preserves existing limits by default; choose **Set a daily quota** to replace them or **No daily quota** to remove them. Daily quotas require `[keys] usage_dir`.
- **Rotate:** type the key ID to confirm, choose a new expiry and copy the replacement secret. Update clients that used the old secret.
- **Revoke:** type the key ID to confirm. Revocation is permanent and IDs are never reused, including owner IDs. Already-running requests may finish.

An owner key is private-only. Adding a never-public route requires a separate acknowledgement because the entire key becomes unavailable on the public listener. Other public access still depends on the gateway's publication allowlist.

The gateway normally reloads the keys file within about two seconds. The portal writes the same locked, atomic, permission-checked file and audit log as the host CLI. It does not verify that a gateway is running or that its reload succeeded. Check gateway status and warnings when validating access. A stale page cannot overwrite another writer's changes: refresh and repeat the edit.

Usage is read from configured persisted state and can lag by about 30 seconds. Token totals include only reported upstream usage. An unavailable count or missing-usage count is not evidence of zero consumption. The portal makes no inference requests. Daily quotas reserve tokens before dispatch, retain reservations when usage is missing, and reset at UTC midnight. Actual reported usage can exceed a reservation, so token quotas are not a hard billing cap. See [usage and quotas](usage-quotas.md) for reservation policy and separate plane allowances.

## Session and network boundary

The command binds strictly to `127.0.0.1`; it has no `--bind`, public-listener or remote-host option. Run it on the host, outside the gateway containers. Do not publish its port through a tunnel, reverse proxy, container port mapping or SSH forwarding. Those would change the intended same-device boundary.

The portal uses:

- A random login code that expires after ten minutes, is consumed after one successful login, and stops accepting attempts after ten failures.
- A separate random session that expires after one hour. It stays in the page's JavaScript memory, with no cookies, local storage or session storage.
- Exact Host and Origin validation, a required custom request header, JSON POST requests and no CORS permission for the management API.
- Bounded request bodies and connections, request deadlines, no-store responses, a restrictive content policy and frame blocking.

Use the dashboard's **Refresh** button to update keys without leaving the session. Reloading the browser page, navigating away or choosing **Lock portal** clears the browser session. Restart the terminal command for another login code. Ctrl-C stops the portal and invalidates every session. If a one-time key secret is lost, rotate the key again.

The static login page contains no configured inventory, keys or provider credentials. No management routes are mounted on the private gateway, public gateway or admin listener. The terminal code is delivered separately from the URL, and the portal does not write HTTP access logs containing credentials.

This protects the browser boundary. A compromised host account with access to the keys file is outside that boundary.

## Validation

The implementation was checked with isolated fixture keys, router security and lifecycle tests, a subprocess loopback HTTP test, and browser creation, expiry editing, rotation, revocation and locking. These checks did not alter live keys or deploy the portal.
