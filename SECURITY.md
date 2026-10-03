# Security policy

Kanata is **beta** software for single-host, self-hosted deployments. Only the latest release on the default branch receives fixes.

## Reporting a vulnerability

Please **do not open a public issue** for security problems. Report them privately with GitHub's [private vulnerability reporting](../../security/advisories/new) (Security tab → "Report a vulnerability").

Include:
- the affected version or commit;
- which listener was involved (private, public or admin) and the adapter;
- steps to reproduce, and the impact you observed.

Never include real API keys, tokens or credentials; redact them.

You should get an acknowledgement within a week. Fixes are released as soon as practical, with credit if you'd like it.

## In scope

- Authentication bypass or key-scope escalation, including reaching Codex through the public listener.
- Leaking keys, tokens, prompts or bodies through logs, metrics or error responses.
- Leaking configured model inventory through the public guide, broadening its exact unauthenticated `GET`/`HEAD /v1` and `/v1/` exception, or persisting bearer keys in its browser storage. The generic reference page is intentionally public; personalized model discovery remains authenticated.
- Request validation or bounds bypasses that cause unbounded resource use.
- Container or Compose settings that expose ports or host resources contrary to the documentation.
- Tampering with `keys.toml`, usage state or `audit.jsonl` through the container mounts (the keys directory is mounted read-only).
- Key management exposed through a gateway listener, a non-loopback portal bind, or a portal session/origin bypass. `kanata key` has no network path; the optional `kanata portal` is a separate host process with a one-use terminal login code, an in-memory browser bearer session, and exact Host/Origin checks. Login-code renewal requires input in the host terminal or a valid browser session; the locked page cannot issue a code.

## Out of scope

- Weaknesses of the host, Docker daemon, firewall, Tailscale or Cloudflare configuration you operate. See the host-isolation notes in the [README](README.md#security).
- The documented residual risks of the public profile: `kanata-public` shares the host, Docker daemon and backends with the private container, and `--plane all` runs both listeners in one process with the Codex credentials.
- Documented key-lifecycle residuals: the public container can read key digests and `audit.jsonl` (key ids, local usernames) through the read-only keys mount; with `--plane all` an expired owner key gets `401 key_expired` on the public listener (visible only to its holder); revoked records count toward the 1000-record cap; a revoked key's request already in progress (including a stream) runs to completion.
- Behaviour of upstream providers, including changes to ChatGPT's private Codex backend.

## Sign in with ChatGPT

The `chatgpt` provider is private-only. Its authentication commands run on the host with a loopback callback; no gateway or key-portal endpoint accepts account tokens. Protected account files must stay outside the repository. The public process drops this auth configuration and any key with its scopes. The provider pins OAuth and inference destinations, verifies ID-token signatures and identity claims, and does not retry inference. Live consent and account availability are separate from fixture verification.
