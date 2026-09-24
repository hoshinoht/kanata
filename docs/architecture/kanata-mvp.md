# Kanata MVP contracts

## Deployment goal

The MVP targets one Kanata process on a personal homelab node. The private client listener sits behind a tailnet-only reverse proxy; an optional second client listener is reached through a Cloudflare tunnel. Both require bearer keys. `kanata serve --plane all` runs both in one process with shared adapter and admission state; the Compose public profile instead runs `--plane private` and a separate `--plane public` process that loads only the public routes, their adapters and public keys. The private listener can serve every key-authorized route, including owner-key Codex; the public listener starts with zero routes and can serve only explicitly allowlisted non-Codex chat or transcription/ASR selectors. Admin remains container-loopback. These are intended operator-owned hostnames, not verified DNS, tunnel, or deployment state. macOS is the current development environment; Windows 11 Docker is KIV and Linux host deployment is later.

The deployment routes local models through Ollama and vLLM, and remote models through OpenRouter and OpenAI using OAuth. Local runtimes remain responsible for model loading and inference. Kanata is responsible for the client-facing API, authentication, exact route selection, policy, and provider adaptation.

## Support matrix

The MVP exposes models, chat, and transcription endpoints. Its initial routes cover local chat through Ollama and vLLM, and remote model access through OpenRouter and private OpenAI/Codex OAuth. Transcription is available only where the configured upstream declares that capability. `Operation` is limited to adapter-routable `chat` and `transcription`; models is a registry-local API listing, not an adapter request or response. A route is an exact `(model alias, operation)` match with explicit `route_id` and `upstream_id`. An explicitly configured Codex alias may include a literal `:low`, `:medium`, or `:high` effort suffix validated in provider configuration; core never parses or strips it. No wildcarding or upstream-model rewriting occurs.

The planned public-route policy intersects an authenticated key's exact permissions with a separately configured listener allowlist and bound adapter capabilities; the allowlist defaults empty. Selecting a multimodal chat alias does not implicitly select its transcription/ASR operation. Codex is never a public-listener route, even for the owner key, and a non-owner key cannot be configured to authorize Codex. In one process (`--plane all`) this is API routing isolation only; the Compose public profile runs the public listener as `--plane public`, a separate process without Codex credentials or the owner key.

## Core boundary

`src/core` defines provider-neutral requests, responses, normalized chat events, capabilities, routes, usage, finish reasons, errors, and request context. Core context includes a request ID, route identity, trust zone, and extensions, but never credentials. Concrete adapter types and wire payloads remain in adapter modules, configuration decoding, or the composition root.

Chat supports text, declared function tools, relayed tool calls/results in history, and complete or streaming output. `ToolChoice` is canonical `none`, `auto`, `required`, or named `function`; required needs declared tools and named function needs an exact declaration. Unknown internal IR fields reject rather than being dropped. Kanata never executes tools. Transcription receives a `ValidatedFile` that owns validated name, media type, and bytes after ingress validation. Remote media is not supported. `RoutedRequest` validates tool semantics plus exact route model alias and operation before adapter dispatch.

## Trust, extensions, and errors

Trust zones are `local`, `private_network`, and `external`; routing must not silently cross zones or fall back. Extensions are JSON values keyed only by bounded, lowercase, dot-namespaced keys such as `io.kanata.trace`. The container permits at most 16 entries, 8 KiB of canonical JSON, and nesting depth 4; invalid keys or bounds reject during decoding and mutation. Extensions are not blind pass-through: adapter and route allowlists are a later policy layer.

`ErrorKind` carries no message or provider details. Its `mapping()` supplies stable status, code, and type for later OpenAI wire rendering. Timeouts retain one of `queue`, `connect`, `headers`, `first_byte`, `idle`, or `overall`.

## Adapter lifetime

`Adapter` is object-safe. It accepts an owned, validated `RoutedRequest` and returns a boxed, `Send + 'static` future. `RoutedRequest` exposes read-only request/context access and consuming parts for adapter ownership. Its result is either a complete response or a boxed normalized event stream. Future or stream drop is the later transport's cancellation mechanism; this contract supplies no runtime or cancellation implementation.

## Non-goals

No provider code, network access, deployment automation, Tailscale/Cloudflare/DNS management, retry/fallback, tool execution, media fetching, dynamic plugins, model lifecycle, multimedia chat, or unknown-field forwarding is introduced by these contracts. The optional public-ingress profile requires separate STRIDE review, authenticated client-only routing and operator attestation; this contract document does not expose it.
