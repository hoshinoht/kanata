# Sign in with ChatGPT

Kanata's `chatgpt` provider uses OpenAI's [documented OAuth integration](https://developers.openai.com/siwc/token-sharing-open-source) for eligible ChatGPT plan usage (preview). Authentication runs on the host; chat routes stay private. The legacy `codex` provider is deprecated and retained for explicit restoration; its credentials are not imported.

Your account permissions, workspace policy and current model catalog determine what is available. Verify a short inference request with your selected account after configuring a route.

## Try it on your host

1. Build the binary with `cargo build --locked`.
2. Copy `config/chatgpt.example.toml` to a private local config. Set `[chatgpt_auth].state_dir` to an absolute directory owned by your user, outside the checkout. Create that directory first with mode `0700` (for example, `mkdir -m 700 /absolute/private/path/kanata-chatgpt`). Use a canonical path without symlinks. It contains account credentials. The example binds the client to loopback.
3. Start sign-in:

   ```sh
   target/debug/kanata auth chatgpt login --config /absolute/path/to/chatgpt.toml
   ```

   Open the printed **Continue with ChatGPT** URL in a browser on the same host. Complete consent and keep the command running until the callback finishes. Nothing signs in automatically when Kanata is built or started.

4. Inspect status and fetch your account's model choices:

   ```sh
   target/debug/kanata auth chatgpt status --config /absolute/path/to/chatgpt.toml
   target/debug/kanata auth chatgpt models --config /absolute/path/to/chatgpt.toml
   ```

   Copy a returned `slug` into each family route's `upstream_id`. Keep `model_alias = "chatgpt-chat"` or choose your own alias. Select pins from `supported_reasoning_efforts` and use `default_reasoning_effort` for the unsuffixed route. Listing models does not create routes or grant permissions.

5. Create the config's `usage_dir`, then create a gateway key and start the private process:

   ```sh
   target/debug/kanata key new --config /absolute/path/to/chatgpt.toml \
     --id chat-client --chat chatgpt-chat,chatgpt-chat:low --expires 30 \
     --key-out /absolute/private/path/chat-client.key
   target/debug/kanata serve --config /absolute/path/to/chatgpt.toml --plane private
   ```

   Use that gateway key with `/v1/chat/completions` or `/v1/responses` and the configured alias. Your client never needs the account's OAuth token. First try a short text request, then a function-call round trip if you need tools.

## Docker deployment

For an existing Compose deployment, use the [Docker setup](../../deploy/docker/README.md#sign-in-with-chatgpt). It adds the opt-in `compose.kanata.chatgpt.yml` overlay and a dedicated `KANATA_CHATGPT_STATE_DIR` directory. Set `[chatgpt_auth].state_dir` to the same absolute path; host browser login and the private container use one protected state store. This preserves the existing client key file, provider API-key secrets and Codex credentials.

```sh
scripts/kanata.sh chatgpt login
scripts/kanata.sh chatgpt status
scripts/kanata.sh chatgpt models
# Named profile for login or logout:
scripts/kanata.sh chatgpt login --profile work
```

The callback stays on host loopback. Nothing is published through the reverse proxy or public gateway. After selecting an account model slug and adding a private route, grant its alias through `scripts/kanata.sh portal` or the key CLI. Starting the gateway does not start sign-in.

## Profiles and sign-out

`login --config <path> --profile <name>` creates or reauthorizes a named profile and selects it after validation. Returning sign-in shows the account selector; choose the profile’s original account and workspace. The default name is `default`. Names use letters, digits, underscores or hyphens, up to 64 characters. Status lists saved profile metadata and the active profile without tokens. Model discovery and inference use the active profile.

```sh
target/debug/kanata auth chatgpt logout --config /absolute/path/to/chatgpt.toml
# Or select the profile to sign out:
target/debug/kanata auth chatgpt logout --config /absolute/path/to/chatgpt.toml --profile work
```

Sign-out removes the selected local credentials and reports whether remote revocation was confirmed. A remote failure requires checking the connection in your ChatGPT account settings. The saved registration and stable host identity are retained for reauthorization. If the process exits during revocation, the profile stays inactive with credentials retained for recovery; run `logout --config <path> --profile <name>` to retry cleanup.

The callback always binds `127.0.0.1` on an available port at `/auth/callback`. For a headless deployment, follow the [official self-hosted instructions](https://developers.openai.com/siwc/token-sharing-open-source/self-hosted-vms); a browser on another device cannot reach the server's loopback callback directly.

## Supported requests

- Text chat, full conversation history and client-executed function tools.
- Streaming and complete gateway responses. Upstream HTTP always uses `store: false` and `stream: true`.
- Function tools are grouped in a private adapter namespace. Clients continue to use ordinary function names. A forced function choice restricts the upstream declarations to that function.
- Exact configured routes and existing key scopes, admission, cancellation, deadlines, usage reports and quotas.

With adapter `reasoning_control = true`, pin every route with `reasoning_effort` and use matching suffixes such as `chatgpt-chat:low` for variants. Clients discover one base alias and accessible `kanata.reasoning_efforts`, then send Chat `reasoning_effort` or Responses `reasoning.effort`. See [client integration and migration](service-handoff.md).

For debugging, set `reasoning_summary = "auto"`, `"concise"` or `"detailed"` on each chat route in a family. Summary requests are off by default, and every variant must use the same policy. Private Chat replies return summaries in `message.reasoning_content` or streamed `delta.reasoning_content`; [Responses](responses.md) returns reasoning output items and summary events. Summaries depend on account/model support and may be absent. They are not raw internal reasoning or encrypted reasoning, and their presence does not determine the reasoning-token count. Clients cannot override the route policy with `reasoning.summary`.

Legacy adapters with reasoning controls disabled keep their default request shape. Catalog levels outside the supported standard enum appear in `unsupported_reasoning_efforts`; `ultra` task delegation is not enabled. Configure only levels supported by the selected account model. The [official reasoning guide](https://developers.openai.com/api/docs/guides/reasoning) defines standard effort values; use the account catalog to select advertised levels and verify inference with the selected model.

This implementation rejects image/audio input, embeddings, transcription, speech, structured output, sampling controls and output-token caps for this provider. It does not support hosted tools or persistent response chaining. Unsupported fields fail before inference. See the [preview contract](https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations).

## Security boundary

Authorization uses fresh state, OIDC nonce and PKCE values. The issued client ID is saved separately from the initial registration entrypoint. The ID token is signature-verified and checked for issuer, audience, expiry and nonce before selecting the profile; granted scopes determine whether plan usage is enabled. Files use owner-only permissions and atomic writes. Refreshes preserve rotating credentials under a cross-process lock.

Inference is pinned to `https://api.openai.com/v1/responses`. There is no inference retry or fallback. Success requires `response.completed`; failed, incomplete and interrupted streams are failures even after text has arrived. Public configuration rejects these routes, and the public process drops the authentication configuration and keys with private-provider scopes.

If inference reports an authentication failure, inspect local sign-in status and reauthorize the selected profile. Status reports saved metadata; it does not make a live account check. Kanata does not automatically replay a rejected inference request.
