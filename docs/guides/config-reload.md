# Reload routes without restarting

On Linux and macOS, send **SIGHUP** to a running `kanata serve` process after saving a valid configuration:

```sh
kanata check --config config/config.toml --plane private
kill -HUP "$KANATA_PID"
```

Set `KANATA_PID` to the process ID from the `kanata started` log or the loopback admin `/status` response. Signal each process that should adopt the change. There is no HTTP mutation endpoint for configuration reload.

For a Compose deployment, use the same Compose file list as the running deployment and signal its services:

```sh
docker compose kill -s HUP kanata
# When the separate public service is enabled:
docker compose kill -s HUP kanata-public
```

The process re-reads the config at its original path, validates it with the current keys file, applies its original `--plane` narrowing, builds the configured adapters and prepares a complete replacement. Only then does it publish the new generation. Invalid config, invalid keys or adapter initialization failure leaves the previous generation active.

A config bind-mounted as a single file must expose the updated bytes inside the container. Editors that replace the file's inode can leave an existing bind mount showing the old file; recreate that container when needed. Config reload does not discover a replacement mount.

## What can change

- Add, remove or rename exact routes and aliases.
- Change upstream model IDs, backend URLs, route capabilities, output caps and route options.
- Add or remove adapters and change their supported operations or credentials.
- Change the public publication allowlist.
- Change inline key grants, or apply a matching keys-file update with the new routes.

Removing a route also requires removing its active key grants. Save the updated config and keys before sending the signal. A keys-file poll during that transition can reject the intermediate file and retain old grants until the complete config reload succeeds.

These settings require a process restart:

- Listener addresses or ports, process plane, global limits, timeouts and logging settings.
- Key source type, keys-file path and usage directory.
- Authentication store settings and state directories.
- Concurrency limits or circuit-breaker policies for an existing adapter ID, including an ID removed and later restored.

The retained admission pool allows up to 4,096 distinct route and adapter IDs between restarts. Old IDs stay reserved so removing and re-adding an ID cannot create fresh capacity while older requests are still running. Exceeding the pool limit rejects the reload with `restart_required`.

## Requests and accounting

Each new HTTP request selects one complete generation, including requests on an existing keep-alive connection. Route selection, adapter bindings, key grants and public policy come from that generation. An already-running request or stream can finish using its previous generation.

Reloading preserves upload reservations, active and queued request counts, route and adapter semaphores, circuit-breaker state, telemetry, persisted usage and daily quota accounting. Unchanged keys retain their rate buckets and concurrency limiters. A changed key limit follows the existing key-reload behavior. New adapter IDs start with new admission state.

The public process applies public-plane narrowing on every reload. It still excludes owner keys, private-provider grants, private-provider credentials and unpublished routes. Reloading never adds a fallback route or retries an inference request.

Key-file polling continues after a successful reload and validates future grants against the newly loaded routes. Ordinary key changes still apply automatically within about two seconds; they do not need SIGHUP.

## Check the result

The loopback admin `/status` includes `pid` and a `configuration_reload` object:

- `enabled`: whether this process supports signal reload.
- `generation`: starts at 1 and increases after each successful reload.
- `healthy`: whether the most recent requested reload succeeded. Failed reloads can leave the gateway ready while it serves its prior generation.
- `attempts`, `failures`, `last_attempt`, `last_success`: attempt counts and Unix timestamps.
- `last_error`: a sanitized validation or initialization error, or `null` after success.

`key_reload` remains separate and describes automatic keys-file polling. Restart the process for settings reported as `restart_required`.

Regression coverage uses synthetic adapters and a subprocess loopback gateway. It checks old streams, reused keep-alive connections, admission and key-rate preservation, failed reload retention, key polling against new routes, and persisted usage. These are fixture checks, not evidence of deployment or live provider availability.
