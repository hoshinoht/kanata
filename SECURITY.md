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
- Request validation or bounds bypasses that cause unbounded resource use.
- Container or Compose settings that expose ports or host resources contrary to the documentation.

## Out of scope

- Weaknesses of the host, Docker daemon, firewall, Tailscale or Cloudflare configuration you operate. See the host-isolation notes in the [README](README.md#security).
- The documented residual risks of the public profile: `kanata-public` shares the host, Docker daemon and backends with the private container, and `--plane all` runs both listeners in one process with the Codex credentials.
- Behaviour of upstream providers, including changes to ChatGPT's private Codex backend.
