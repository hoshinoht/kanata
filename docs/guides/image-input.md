# Inline images in chat

Chat routes can accept image parts when the adapter declares `input_images = true` and the route declares `allows_input_images = true`. Both settings default to false. Configure these only after checking that the selected upstream model supports vision. The implemented transports are Ollama, vLLM and OpenRouter; Apple FM, Codex and ChatGPT sign-in backends reject this capability.

Use the [`vision.example.toml`](../../config/vision.example.toml) template or add the two flags to an existing chat adapter and route. Image requests use the same exact `chat` key scope, public-route allowlist, admission limits and deadlines as text chat.

## Request format

Send `POST /v1/chat/completions` with user-message content parts:

```json
{
  "model": "local-vision",
  "messages": [{
    "role": "user",
    "content": [
      {"type": "text", "text": "Describe this image."},
      {"type": "image_url", "image_url": {"url": "data:image/png;base64,BASE64_IMAGE"}}
    ]
  }]
}
```

An image-only user message is supported. Text and image order is preserved. Existing streaming and tool capability checks still apply. Images in system, developer, assistant or tool messages are rejected. The `image_url` object accepts only `url`; `detail` and other options are rejected.

Only exact `data:image/png;base64,` and `data:image/jpeg;base64,` prefixes are accepted, followed by canonical standard base64. Remote HTTP URLs, file paths, SVG, GIF, WebP, animated PNG and other media are rejected. The gateway makes no image-download request.

## Limits and validation

| Limit | Value |
| --- | --- |
| Images across all messages | 4 |
| Decoded file bytes per image | 4 MiB |
| Decoded file bytes across all images | 8 MiB |
| Base64 characters per image | 5,592,408 |
| Width or height | 4,096 pixels |
| Pixels across all images | 16,777,216 |

`limits.max_body_bytes` also bounds the complete JSON body for requests without audio; its default may be smaller than these image limits. The vision template uses 16 MiB. Existing audio-envelope limits apply when a request also contains permitted audio. Shared upload reservations cover incoming, parsed and outgoing payload capacity and remain held through queueing and dispatch.

Validation checks the declared MIME type, file signature, PNG chunk boundaries and IHDR dimensions, or JPEG segment boundaries and baseline/progressive 8-bit frame dimensions. It requires a complete end marker and rejects additional data after it. It rejects animated PNG and JPEG frames whose dimensions can change during decoding. It does **not** decompress pixels, verify PNG CRCs or validate compressed pixel streams. The upstream decoder remains responsible for complete image validation. Pixel bounds therefore constrain declared image dimensions; they are not a guarantee about upstream decoder memory usage.

Malformed or unsupported content returns `400 invalid_request`; image count, byte, dimension or pixel limits return `413 invalid_request`. Images sent to an undeclared route return `400 invalid_request` before dispatch.

Authenticated `/v1/models` returns `kanata.input_images` and, when enabled for an authorized chat route, `kanata.images` with formats and limits, including `max_json_body_bytes_without_audio`. These declarations describe gateway configuration, not a successful live vision probe.

## Provider references

[Ollama compatibility](https://docs.ollama.com/api/openai-compatibility), [vLLM multimodal inputs](https://docs.vllm.ai/en/latest/features/multimodal_inputs/), and [OpenRouter image inputs](https://openrouter.ai/blog/tutorials/send-image-to-llm/).
