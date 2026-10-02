# Sign in with ChatGPT

Kanata has a separate `chatgpt` provider for the documented ChatGPT plan usage preview. Authentication runs on the host; chat routes stay private. The existing experimental `codex` provider remains separate and its credentials are not imported.

This implementation is checked with local protocol, OAuth and TLS fixtures. No live sign-in or account inference was performed for this change. Your account permissions, workspace policy and current model catalog determine what is available.

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

   Copy a returned `slug` into the route's `upstream_id`. Keep `model_alias = "chatgpt-chat"` or choose your own alias. Listing models does not create routes or grant permissions.

5. Create the config's `usage_dir`, then create a gateway key and start the private process:

   ```sh
   target/debug/kanata key new --config /absolute/path/to/chatgpt.toml \
     --id chat-client --chat chatgpt-chat --expires 30 \
     --key-out /absolute/private/path/chat-client.key
   target/debug/kanata serve --config /absolute/path/to/chatgpt.toml --plane private
   ```

   Use that gateway key with `/v1/chat/completions` or `/v1/responses` and the configured alias. Your client never needs the account's OAuth token. First try a short text request, then a function-call round trip if you need tools.

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

This first implementation rejects image/audio input, embeddings, transcription, speech, structured output, sampling controls, reasoning controls and output-token caps for this provider. It does not support hosted tools or persistent response chaining. Unsupported fields fail before inference. See the [preview contract](https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations).

## Security boundary

Authorization uses fresh state, OIDC nonce and PKCE values. The issued client ID is saved separately from the initial registration entrypoint. The ID token is signature-verified and checked for issuer, audience, expiry and nonce before selecting the profile; granted scopes determine whether plan usage is enabled. Files use owner-only permissions and atomic writes. Refreshes preserve rotating credentials under a cross-process lock.

Inference is pinned to `https://api.openai.com/v1/responses`. There is no inference retry or fallback. Success requires `response.completed`; failed, incomplete and interrupted streams are failures even after text has arrived. Public configuration rejects these routes, and the public process drops the authentication configuration and keys with private-provider scopes.

If inference reports an authentication failure, inspect local sign-in status and reauthorize the selected profile. Status reports saved metadata; it does not make a live account check. Kanata does not automatically replay a rejected inference request.
