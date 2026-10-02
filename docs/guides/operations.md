# Operations

## Diagnose a process

```sh
kanata doctor --config config/config.toml
kanata doctor --config config/config.toml --plane private --probe-backends
kanata health --config config/config.toml
```

Doctor checks configuration and key-file permissions, then reads `/status` from the configured loopback admin listener. The report shows whether the process is ready and whether its latest key-file poll succeeded. An invalid replacement key file keeps the previous keys active; inspect reload health after changing keys. Inline keys have automatic keys-file polling disabled; they follow explicit configuration reload.

Run diagnostics in the process's network namespace. For Compose:

```sh
docker compose exec kanata /usr/local/bin/kanata doctor --config /etc/kanata/config.toml
```

`--probe-backends` makes one bounded TCP connection attempt per routed adapter. It sends no credentials or prompts. TCP reachability does not establish TLS validity, authentication or model availability. Doctor prints a diagnostic report even when the process is unavailable; use `health` for an exit-status readiness check.

Two additional opt-in checks use the configured upstream connection:

```sh
kanata doctor --config config/config.toml --plane private --probe-models
kanata doctor --config config/config.toml --plane private \
  --probe-inference local-embed --operation embeddings
```

`--probe-models` resolves configured credentials and reads each routed adapter's model catalog once, with a 10-second deadline and an 8 MiB response limit. It reports configured aliases as `listed`, `not_listed`, or a sanitized failure. Catalog membership does not verify capabilities. The experimental subscription adapter has no catalog probe.

`--probe-inference` requires an exact alias and operation in the selected plane. It sends a short fixed prompt, embedding string, or 100 ms silent WAV, with a 15-second deadline. Chat uses a 16-token output cap where supported and consumes streaming responses through completion. The probe can consume upstream credits or subscription allowance; it runs directly from the host and bypasses gateway key limits and usage accounting. It verifies that one synthetic request completed. Tools, images, recognition quality, other options and future availability remain untested. Providers without output-cap support are bounded by time and transport bytes only. Generated content and provider error bodies are omitted. Doctor still exits successfully when an individual probe fails; inspect its `verified`/`failed` result.

## Reload configuration

On Linux/macOS, save the config and send SIGHUP to the running process. New requests adopt a validated complete generation; active streams finish using their previous one. The loopback `/status.configuration_reload` object reports the outcome. Invalid replacements leave the current generation running. See [configuration reload](config-reload.md) for supported changes, state preservation, Compose usage and restart-only settings.

## Explain route access

```sh
kanata routes explain --config config/config.toml --model local-embed \
  --operation embeddings --key-id search --plane private
kanata routes explain --config config/config.toml --model local-embed \
  --operation embeddings --key-id search --plane public --json
```

This host command reads configuration and key metadata without resolving secrets or contacting providers. It reports whether the exact selector exists, survives process-plane narrowing, and is permitted by the key and listener. Reasons distinguish missing routes, removed/revoked keys, expired keys, missing scopes and unpublished routes. The report includes route/adapter IDs and declared capabilities, while omitting upstream model IDs, addresses and credentials.

Match `--plane` to the running process (`all` by default). In `all`, both listeners share the key set; in `public`, the process removes owner and private-provider keys. The private listener in a public-only process is container-loopback. This is an offline configuration snapshot: it does not establish that the running process has reloaded those settings, that credentials resolve, that capacity is available, or that a model can perform inference.

## Metrics

The loopback admin `/metrics` exposes these additional series:

| Metric | Meaning |
| --- | --- |
| `kanata_queue_wait_seconds` | Histogram of admission wait by endpoint |
| `kanata_first_content_seconds` | Time from request observation to first visible content; includes upload and queue time. Tool output and private reasoning count; the SSE role frame does not |
| `kanata_upstream_duration_seconds` | Time from adapter dispatch to completion/error/cancellation; streams include downstream backpressure |
| `kanata_requests_queued` | Requests waiting for route or adapter capacity |
| `kanata_buffered_requests` | Requests holding an input reservation, including queueing and dispatch |
| `kanata_reserved_request_bytes` | Reserved raw/decoded/outbound payload allowance |
| `kanata_key_reload_failures_total` | Failed key-file polls, including a missing file at startup |
| `kanata_key_reload_enabled` | Whether this process polls a keys file |
| `kanata_key_reload_healthy` | Whether the latest key-file poll succeeded; inspect `/status.key_reload.enabled` for inline keys |
| `kanata_key_reload_last_success_seconds` | Unix timestamp of the last successful key-file read |

Timing labels use a fixed endpoint set. Key/model counters retain current key IDs at scrape time and have a 16,384-series process cap; new combinations beyond the cap are omitted. Removal or revocation can therefore remove historical series from the process. Keep long-term history in your monitoring system.

A sustained queue indicates demand above inference capacity. Upload refusals indicate input reservations are exhausted. A ready process with unhealthy key reload may still be using old grants. Readiness describes the gateway's lifecycle; it does not probe every backend.

## Token accounting

`kanata key show <id> --config <path>` displays token totals. `key show --json` and `key list --json` expose a `tokens` object:

- `reported`: upstream attempts with final usage.
- `missing`: attempts without final usage, including interrupted streams and providers without usage reporting.
- `input_tokens`, `output_tokens`: sums of reported counts.
- `reasoning_tokens`, `reasoning_reported`: reported reasoning total and number of reports containing that count. Reasoning tokens are part of output tokens; do not add them again to a total.

Usage is recorded after response completion or cancellation even when a streaming client did not request usage chunks. Authenticated request counts also include local rejections and model listings; token report counts apply only to dispatched inference attempts. Usage state v2 remains local, flushes every 30 seconds and at shutdown, and can read v1 files. Old files contain no historical token measurements. Missing reports and delayed persistence make these observability totals unsuitable as billing or hard spend enforcement.

## Verification tools

```sh
scripts/check-templates.sh
scripts/container-smoke.sh
KANATA_FUZZ_CASES=20000 cargo test --locked --lib parser_mutation_smoke
cargo test --locked --release --lib connection_reuse_benchmark -- --ignored --nocapture
```

Template checks copy examples into a temporary directory with explicit empty fixture keys. The container smoke test builds `kanata:ci`, starts a disposable container without networking or credentials, checks readiness, then verifies graceful shutdown. Parser mutation tests use a reproducible seed and compare SSE framing across chunk boundaries; they are bounded mutation tests, not coverage-guided fuzzing.

The connection benchmark compares the current transport with a single reused HTTP/1 connection on loopback. It measures tiny JSON requests without TLS or inference. The prototype omits production transport validation, so its difference is an upper estimate of potential local savings, not a production pooling speedup. Connection pooling requires separate cancellation, credential-boundary and no-replay validation before adoption.

A local macOS release-mode run during development (200 requests per path) measured 144 µs median / 218 µs p95 for the current transport and 32 µs / 42 µs for the reuse prototype. That is approximately 0.11 ms median savings on this fixture. Production pooling remains unchanged pending representative remote TLS and inference measurements.
