# Daily usage and quotas

Kanata can record inference attempts by key, model alias, operation and UTC date. Enable `[keys] usage_dir` and keep that directory writable and persistent. Daily reports contain counts only: prompts, completions, audio, images, keys and provider credentials are not stored.

```toml
[keys]
file = "keys/keys.toml"
usage_dir = "state"
```

The existing `usage-<plane>.json` totals count authenticated API requests, including discovery, and flush periodically. The `daily-<plane>.json` records admitted inference attempts durably before dispatch. Validation failures, permission failures, discovery and admission rejections do not consume daily quotas. Cancellation while the durable write is completing can consume an allowance before the backend receives the request.

## Set a daily allowance

The [private key portal](private-portal.md) can also create, edit and clear daily quotas. Its default edit action preserves the existing quota.

```sh
kanata key edit client --config config/config.toml --daily-requests 100
kanata key edit client --config config/config.toml \
  --daily-tokens 200000 --reservation-tokens 8000
kanata key edit client --config config/config.toml --clear-quota
```

`key new` accepts the same quota flags. Changes reload with the keys file. Editing or rotating a key preserves its existing usage. `--clear-limits` clears concurrency and rate limits; `--clear-quota` clears the daily allowance. Quota flags update the named fields and preserve other quota fields.

The equivalent optional field in a key record is:

```toml
daily_quota = { requests = 100, tokens = 200000, reservation_tokens = 8000 }
```

All values are positive integers. `tokens` requires `reservation_tokens`, which must not exceed the daily token allowance. A request allowance can be used on its own. Keys without a daily quota retain unlimited daily admission. Quotas require file-backed keys and `usage_dir`; invalid settings are rejected during startup and key reload. The owner key is subject to its configured quotas too.

### Request limits

Each accepted reservation consumes one request across all of that key's models and operations. Concurrent requests and multiple handles using the same plane ledger serialize through a file lock. Kanata writes and syncs the replacement and its directory before dispatch. Restarting the server does not reset counters. Exhaustion returns `429 daily_quota_exceeded` with `Retry-After` pointing to the next UTC midnight.

### Token reservations

Before dispatch, Kanata reserves the configured `reservation_tokens`. A later request must fit its entire reservation within the remaining daily allowance. On a valid final usage report, Kanata replaces the reservation with the reported total. Missing usage, cancellation, an incomplete stream, process termination or a failed reconciliation retains the full reservation. Requests stay charged even when the backend fails. Reconciliation runs asynchronously; until it finishes, admission sees the reservation.

**A token allowance is an admission budget, not a guaranteed provider billing ceiling.** Kanata cannot know the exact input token count before dispatch, and some routes do not support an output limit. Choose a reservation that covers the expected maximum input and output; image and audio costs are model-dependent. Apply route output limits where supported. A provider report above the reservation is fully charged, appears as `overrun_tokens`, and can take the daily total above the allowance. Subsequent requests are blocked whenever their reservation no longer fits. Kanata does not truncate requests or invent token usage to hide that overrun.

Quota storage errors or invalid state return `503 quota_unavailable` before dispatch for keys with quotas. The ledger is never silently reset after corruption. Requests without quotas may continue if daily reporting fails; Kanata logs that the report is unavailable. Keep a durable volume and backups. Removing or restoring old ledger files can reset usage and is an operator action outside quota enforcement.

## UTC boundaries and process planes

Requests are charged to the UTC day on which the reservation is created. A response that finishes after midnight reconciles against that original day. The ledger remembers the latest admitted day and refuses reservations if the clock moves to an earlier day. Correct the host clock before retrying; Kanata never grants an old day's allowance twice through a clock rollback.

Each process plane has its own ledger: `all`, `private` and `public`. Separate private and public processes receive separate daily allowances, even if they read the same key file. The `all` process shares its one ledger across its two inference listeners. Reports keep plane labels; there is no cross-plane global quota. Changing process plane or state directory changes the ledger, so keep both stable in deployment.

Daily rows retain the most recent 90 UTC days as new requests arrive. Ledger size is bounded to 100,000 key/model/operation/day rows and 32 MiB. Hitting a bound fails quota admission closed. Long requests that finish after their day has aged out cannot change current counters.

## Read reports

```sh
kanata key usage --config config/config.toml
kanata key usage --config config/config.toml --key-id client --model local-chat \
  --from 2026-10-01 --to 2026-10-31 --json
kanata key usage --usage-dir config/state --plane private --json
```

Without `--plane`, the report reads all three process-plane ledgers in the selected directory and its immediate subdirectories (including Compose’s `state/private` and `state/public`). Each row includes its relative source file, so separate state directories stay distinguishable. Dates are inclusive UTC dates. A missing ledger has no recorded history; an invalid ledger makes the report fail instead of returning incomplete totals as if they were complete.

Rows distinguish:

- `requests`: durably reserved attempts.
- `tokens.reported` and `tokens.missing`: attempts with final token usage versus pending or unreported attempts.
- `tokens.input_tokens`, `output_tokens` and optional reasoning counts: provider-reported totals only.
- `charged_tokens`: reported totals plus retained quota reservations.
- `retained_reservation_tokens`: reservations awaiting usable final usage, including cancelled attempts.
- `overrun_tokens`: reported tokens above the per-request reservation.

Optional prices are supplied for each report:

```sh
kanata key usage --config config/config.toml --model local-chat \
  --input-cost-per-million 2 --output-cost-per-million 4 --cost-unit credits --json
```

These are estimates from your supplied rates and reported input/output counts. They do not fetch provider prices or model-specific image, audio, cached-token, tiered or other billing rules. Missing usage sets `cost_incomplete`; retained reservations are not presented as known billing. Use a model filter when aliases have different rates. No price estimate changes quota admission.
