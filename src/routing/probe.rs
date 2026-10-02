use std::time::Duration;

use futures_util::StreamExt;

use crate::{
    adapter::{Adapter, AdapterOutput},
    core::{
        ChatContent, ChatMessage, ChatOptions, ChatRequest, ChatRole, EmbeddingRequest, Extensions,
        FinishReason, NormalizedEvent, Operation, Request, RequestContext, Response, RoutedRequest,
        ToolChoice, TranscriptionRequest, ValidatedFile,
    },
};

use super::RouteEntry;

pub(crate) async fn inference(
    route: &RouteEntry,
    adapter: &dyn Adapter,
) -> Result<&'static str, &'static str> {
    let model = route.identity.selector.model_alias.clone();
    let request = match route.identity.selector.operation {
        Operation::Speech => return Err("not_supported"),
        Operation::Chat => Request::Chat(ChatRequest {
            model,
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: vec![ChatContent::Text {
                    text: "Reply with OK.".into(),
                }],
            }],
            tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
            stream: route.capabilities.streaming_chat,
            options: ChatOptions {
                max_output_tokens: route
                    .capabilities
                    .sampling_controls
                    .then_some(route.max_output_tokens.unwrap_or(16).min(16)),
                ..ChatOptions::default()
            },
            extensions: Extensions::default(),
        }),
        Operation::Embeddings => Request::Embeddings(EmbeddingRequest {
            model,
            input: vec!["Kanata diagnostic".into()],
            dimensions: None,
        }),
        Operation::Transcription => Request::Transcription(TranscriptionRequest {
            model,
            file: ValidatedFile::new("diagnostic.wav", "audio/wav", silence())
                .map_err(|_| "invalid_probe")?,
            language: Some("en".into()),
            prompt: None,
            extensions: Extensions::default(),
        }),
    };
    let request = RoutedRequest::new(
        RequestContext {
            request_id: "host-diagnostic".into(),
            route: route.identity.clone(),
            trust_zone: route.trust_zone,
            extensions: Extensions::default(),
        },
        request,
    )
    .map_err(|_| "invalid_probe")?;
    tokio::time::timeout(Duration::from_secs(15), async {
        let output = adapter
            .execute(request)
            .await
            .map_err(|error| error.kind.mapping().code)?;
        match output {
            AdapterOutput::Complete(response) => match response {
                Response::Chat(chat)
                    if route.identity.selector.operation == Operation::Chat
                        && matches!(
                            chat.finish_reason,
                            FinishReason::Stop | FinishReason::Length
                        ) =>
                {
                    Ok("chat_completed")
                }
                Response::Embeddings(_)
                    if route.identity.selector.operation == Operation::Embeddings =>
                {
                    Ok("embeddings_completed")
                }
                Response::Transcription(_)
                    if route.identity.selector.operation == Operation::Transcription =>
                {
                    Ok("transcription_completed")
                }
                _ => Err("unexpected_response"),
            },
            AdapterOutput::Events(mut events) => {
                let mut completed = false;
                while let Some(event) = events.next().await {
                    if completed {
                        return Err("unexpected_response");
                    }
                    match event.map_err(|error| error.kind.mapping().code)? {
                        NormalizedEvent::ChatCompleted { finish_reason, .. } => {
                            if !matches!(finish_reason, FinishReason::Stop | FinishReason::Length) {
                                return Err("unexpected_finish");
                            }
                            completed = true;
                        }
                        NormalizedEvent::ChatToolCallDelta { .. } => {
                            return Err("unexpected_tool_call");
                        }
                        _ => {}
                    }
                }
                if completed {
                    Ok("chat_stream_completed")
                } else {
                    Err("incomplete_stream")
                }
            }
        }
    })
    .await
    .unwrap_or(Err("timeout"))
}

fn silence() -> Vec<u8> {
    let length = 3_200_u32;
    let mut wav = b"RIFF".to_vec();
    wav.extend_from_slice(&(36 + length).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&16_000_u32.to_le_bytes());
    wav.extend_from_slice(&32_000_u32.to_le_bytes());
    wav.extend_from_slice(&2_u16.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&length.to_le_bytes());
    wav.resize(44 + length as usize, 0);
    wav
}
