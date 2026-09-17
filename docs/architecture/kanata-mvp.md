# Kanata MVP contracts

## Support matrix

The MVP exposes models, chat, and transcription endpoints. Its initial routes are Ollama/local chat, OpenRouter transcription, and Codex chat. `Operation` is limited to adapter-routable `chat` and `transcription`; models is a registry-local API listing, not an adapter request or response. A route is an exact `(model alias, operation)` match with explicit `route_id` and `upstream_id`. Aliases are never wildcarded, suffixed, or rewritten.

## Core boundary

`src/core` defines provider-neutral requests, responses, normalized chat events, capabilities, routes, usage, finish reasons, errors, and request context. Core context includes a request ID, route identity, trust zone, and extensions, but never credentials. Concrete adapter types and wire payloads remain in adapter modules, configuration decoding, or the composition root.

Chat supports text, declared function tools, relayed tool calls/results in history, and complete or streaming output. `ToolChoice` is canonical `none`, `auto`, `required`, or named `function`; required needs declared tools and named function needs an exact declaration. Unknown internal IR fields reject rather than being dropped. Kanata never executes tools. Transcription receives a `ValidatedFile` that owns validated name, media type, and bytes after ingress validation. Remote media is not supported. `RoutedRequest` validates tool semantics plus exact route model alias and operation before adapter dispatch.

## Trust, extensions, and errors

Trust zones are `local`, `private_network`, and `external`; routing must not silently cross zones or fall back. Extensions are JSON values keyed only by bounded, lowercase, dot-namespaced keys such as `io.kanata.trace`. The container permits at most 16 entries, 8 KiB of canonical JSON, and nesting depth 4; invalid keys or bounds reject during decoding and mutation. Extensions are not blind pass-through: adapter and route allowlists are a later policy layer.

`ErrorKind` carries no message or provider details. Its `mapping()` supplies stable status, code, and type for later OpenAI wire rendering. Timeouts retain one of `queue`, `connect`, `headers`, `first_byte`, `idle`, or `overall`.

## Adapter lifetime

`Adapter` is object-safe. It accepts an owned, validated `RoutedRequest` and returns a boxed, `Send + 'static` future. `RoutedRequest` exposes read-only request/context access and consuming parts for adapter ownership. Its result is either a complete response or a boxed normalized event stream. Future or stream drop is the later transport's cancellation mechanism; this contract supplies no runtime or cancellation implementation.

## Non-goals

No provider code, network access, retry/fallback, tool execution, media fetching, dynamic plugins, model lifecycle, multimedia chat, or unknown-field forwarding is introduced by these contracts.
